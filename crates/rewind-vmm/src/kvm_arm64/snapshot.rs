//! Keyframes on Linux ARM64 KVM: capturing and restoring complete vCPU state
//! via `KVM_GET_REG_LIST` / `KVM_GET_ONE_REG` / `KVM_SET_ONE_REG`, device
//! state, and content-addressed guest memory.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_REG_ARM_COPROC_MASK, KVM_REG_ARM_DEMUX, KVM_REG_ARM_FW, KVM_REG_ARM_FW_FEAT_BMAP,
    KVM_REG_SIZE_MASK, KVM_REG_SIZE_SHIFT, RegList,
};
use kvm_ioctls::VcpuExit;
use serde::{Deserialize, Serialize};

use crate::arm64::layout::{PAGE_SIZE, RAM_BASE};
use crate::kvm_arm64::machine::ARM_CPU_IRQ;
use crate::kvm_arm64::state::{is_host_timer_reg, is_id_sysreg, reg_name};
use crate::pv::Clock;
use crate::{Config, DirtyLog, Machine};

/// Where keyframe pages go: a content-addressed page store.
pub trait Pages {
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>>;
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()>;
}

/// The hash the all-zero page goes by; see rewind-store.
pub const ZERO_PAGE: [u8; 32] = [0; 32];

/// A machine captured at a step.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Keyframe {
    pub step: u64,
    pub parent: Option<u64>,
    pub(crate) cpu: CpuState,
    pub(crate) irqchips: Vec<Vec<u8>>,
    pub(crate) devices: DeviceState,
    pub pages: Vec<(u32, [u8; 32])>,
}

/// Keyframe metadata preceding `pages`.
#[derive(Debug, Deserialize)]
pub struct KeyframeHead {
    step: u64,
    parent: Option<u64>,
    cpu: CpuState,
    irqchips: Vec<Vec<u8>>,
    devices: DeviceState,
}

impl Keyframe {
    pub fn from_head(head: KeyframeHead, pages: Vec<(u32, [u8; 32])>) -> Keyframe {
        Keyframe {
            step: head.step,
            parent: head.parent,
            cpu: head.cpu,
            irqchips: head.irqchips,
            devices: head.devices,
            pages,
        }
    }
}

/// Complete architectural ARM64 vCPU state captured from Linux KVM.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CpuState {
    /// Every register returned by `KVM_GET_REG_LIST` as `(reg_id, bytes)`.
    pub(crate) regs: Vec<(u64, Vec<u8>)>,
    /// Whether the monitor's virtual IRQ line (`KVM_ARM_IRQ_TYPE_CPU`) is asserted.
    pub(crate) irq_pending: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct DeviceState {
    pub(crate) now: u64,
    pub(crate) deadline: Option<u64>,
    pub(crate) shared: Option<u64>,
    pub(crate) epoch: u64,
    pub(crate) step: u64,
    pub(crate) branches: u64,
}

#[cfg(not(test))]
const PUT_AT_ONCE: usize = 16_384;
#[cfg(test)]
const PUT_AT_ONCE: usize = 16;

fn reg_byte_len(id: u64) -> Option<usize> {
    let shift = (id & KVM_REG_SIZE_MASK) >> KVM_REG_SIZE_SHIFT;
    if shift > 10 {
        return None;
    }
    Some(1usize << shift)
}

fn is_readonly_or_fw_reg(id: u64) -> bool {
    if is_id_sysreg(id) {
        return true;
    }
    let coproc = (id as u32) & KVM_REG_ARM_COPROC_MASK;
    matches!(
        coproc,
        KVM_REG_ARM_DEMUX | KVM_REG_ARM_FW | KVM_REG_ARM_FW_FEAT_BMAP
    )
}

fn put_pages(
    pages: &mut dyn Pages,
    ram: &[u8],
    indices: &[usize],
    parent: Option<u64>,
) -> Result<Vec<(u32, [u8; 32])>> {
    let mut stored = Vec::new();
    for batch in indices.chunks(PUT_AT_ONCE) {
        let contents: Vec<&[u8]> = batch
            .iter()
            .map(|&i| &ram[i * PAGE_SIZE..(i + 1) * PAGE_SIZE])
            .collect();
        let hashes = pages.put_all(&contents)?;
        if hashes.len() != batch.len() {
            bail!(
                "{} pages were put and {} hashes came back",
                batch.len(),
                hashes.len()
            );
        }
        for (&i, hash) in batch.iter().zip(hashes) {
            if parent.is_some() || hash != ZERO_PAGE {
                stored.push((i as u32, hash));
            }
        }
    }
    Ok(stored)
}

pub(crate) fn final_pages<'a>(
    chain: impl IntoIterator<Item = &'a Keyframe>,
    ram_pages: usize,
) -> Result<Vec<(u32, &'a [u8; 32])>> {
    let mut map: std::collections::BTreeMap<u32, &'a [u8; 32]> = std::collections::BTreeMap::new();
    for kf in chain {
        for (page, hash) in &kf.pages {
            if *page as usize >= ram_pages {
                bail!(
                    "keyframe at step {} names page {page}, past the machine's {ram_pages}",
                    kf.step
                );
            }
            if *hash == ZERO_PAGE {
                map.remove(page);
            } else {
                map.insert(*page, hash);
            }
        }
    }
    Ok(map.into_iter().collect())
}

pub(crate) fn read_pages(
    ram: &mut [u8],
    restored: &[(u32, &[u8; 32])],
    pages: &(dyn Pages + Sync),
) -> Result<()> {
    for &(page, hash) in restored {
        let start = page as usize * PAGE_SIZE;
        pages.get(hash, &mut ram[start..start + PAGE_SIZE])?;
    }
    Ok(())
}

impl Machine {
    /// Captures the ARM64 KVM VM at its current step.
    pub fn keyframe(&mut self, pages: &mut dyn Pages, parent: Option<u64>) -> Result<Keyframe> {
        self.complete_pending_exit()?;

        let dirty = self.dirty_pages()?;
        let mut indices: Vec<usize> = match parent {
            None => self.dev.ram.written_pages(),
            Some(_) => dirty,
        };
        if let Some(shared) = self.dev.shared
            && let Some(off) = shared.checked_sub(RAM_BASE)
        {
            indices.push(off as usize / PAGE_SIZE);
            indices.sort_unstable();
            indices.dedup();
        }

        let stored = put_pages(pages, self.dev.ram.as_slice(), &indices, parent)?;
        let (cpu, irqchips, devices) = self.state()?;
        Ok(Keyframe {
            step: self.dev.step,
            parent,
            cpu,
            irqchips,
            devices,
            pages: stored,
        })
    }

    /// Completes a pending `KVM_EXIT_MMIO` instruction (`kvm_handle_mmio_return`
    /// writes the loaded register and increments `PC` on the next `KVM_RUN`)
    /// without executing any further guest instruction.
    pub fn complete_pending_exit(&mut self) -> Result<()> {
        if !self.pending_exit {
            return Ok(());
        }
        self.vcpu.set_kvm_immediate_exit(1);
        let outcome = match self.vcpu.run() {
            Ok(VcpuExit::Intr) => Ok(()),
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            other => Err(anyhow::anyhow!(
                "completing pending ARM64 KVM MMIO exit: {other:?}"
            )),
        };
        self.vcpu.set_kvm_immediate_exit(0);
        outcome?;
        self.pending_exit = false;
        Ok(())
    }

    pub(crate) fn state(&self) -> Result<(CpuState, Vec<Vec<u8>>, DeviceState)> {
        let devices = DeviceState {
            now: self.dev.clock.now,
            deadline: self.dev.clock.deadline,
            shared: self.dev.shared,
            epoch: self.dev.epoch,
            step: self.dev.step,
            branches: self.branches(),
        };
        Ok((self.cpu_state()?, Vec::new(), devices))
    }

    fn cpu_state(&self) -> Result<CpuState> {
        let mut reg_list = RegList::new(1).context("allocating initial KVM reg list")?;
        if let Err(e) = self.vcpu.get_reg_list(&mut reg_list) {
            if e.errno() != libc::E2BIG {
                return Err(e).context("KVM_GET_REG_LIST sizing");
            }
            let count = reg_list.as_fam_struct_ref().n as usize;
            reg_list = RegList::new(count).context("allocating full KVM reg list")?;
            self.vcpu
                .get_reg_list(&mut reg_list)
                .context("KVM_GET_REG_LIST")?;
        }

        let mut ids: Vec<u64> = reg_list.as_slice().to_vec();
        ids.sort_unstable();

        let mut regs = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(len) = reg_byte_len(id) else {
                continue;
            };
            let mut buf = vec![0u8; len];
            match self.vcpu.get_one_reg(id, &mut buf) {
                Ok(_) => regs.push((id, buf)),
                Err(e) if matches!(e.errno(), libc::ENOENT | libc::EINVAL) => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", reg_name(id))),
            }
        }
        Ok(CpuState {
            regs,
            irq_pending: self.irq_pending,
        })
    }

    /// Restores a machine from a chain of keyframes (full keyframe first).
    pub fn restore(
        config: &Config,
        chain: &[Keyframe],
        pages: &(dyn Pages + Sync),
        trace: &[(u64, Vec<u8>)],
        dirty_log: DirtyLog,
    ) -> Result<Machine> {
        let last = chain.last().context("restoring from no keyframes")?;
        if chain[0].parent.is_some() {
            bail!("a keyframe chain must start with a full keyframe");
        }
        let mut m = Self::create(config, dirty_log)?;

        let restored = final_pages(chain, m.dev.ram.len() / PAGE_SIZE)?;
        for (page, _) in &restored {
            m.dev.ram.mark_page(*page as usize);
        }
        read_pages(m.dev.ram.as_mut_slice(), &restored, pages)?;
        m.origin = crate::Origin::Restored(restored.into_iter().map(|(i, h)| (i, *h)).collect());
        if m.dirty_log == DirtyLog::On {
            let _ = m.drain_dirty_log()?;
        }

        m.apply_state(&last.cpu, &last.irqchips, &last.devices)?;
        m.resume_records(trace, last.devices.step)?;
        Ok(m)
    }

    pub(crate) fn resume_records(&mut self, trace: &[(u64, Vec<u8>)], step: u64) -> Result<()> {
        self.dev.io = None;
        self.dev.switches = None;
        let Some(shared) = self.dev.shared else {
            return Ok(());
        };
        crate::pv::check_interface(self.dev.guest_interface(shared)?)?;
        let through = trace.partition_point(|(s, _)| *s <= step);
        let encoder = rewind_trace::io_records::Encoder::resume(&trace[..through])
            .context("reading the read and write records up to the keyframe")?;
        self.dev.io = Some(encoder);
        let encoder = rewind_trace::switches::Encoder::resume(trace, step)
            .context("reading the context switches up to the keyframe")?;
        self.dev.switches = Some(encoder);
        Ok(())
    }

    pub(crate) fn apply_state(
        &mut self,
        cpu: &CpuState,
        _irqchips: &[Vec<u8>],
        devices: &DeviceState,
    ) -> Result<()> {
        for (id, bytes) in &cpu.regs {
            if is_host_timer_reg(*id) {
                continue;
            }
            match self.vcpu.set_one_reg(*id, bytes) {
                Ok(_) => {}
                Err(e)
                    if is_readonly_or_fw_reg(*id)
                        && matches!(e.errno(), libc::EINVAL | libc::EPERM | libc::ENOENT) => {}
                Err(e) => return Err(e).with_context(|| format!("restoring {}", reg_name(*id))),
            }
        }
        self.irq_pending = cpu.irq_pending;
        self.vm
            .set_irq_line(ARM_CPU_IRQ, self.irq_pending)
            .context("restoring ARM64 virtual IRQ line")?;
        self.pending_exit = false;

        let d = devices;
        self.dev.clock = Clock {
            now: d.now,
            deadline: d.deadline,
            quantum: self.config.quantum,
        };
        self.dev.shared = d.shared;
        self.dev.epoch = d.epoch;
        self.dev.step = d.step;
        self.dev.stop = None;
        self.dev.idle = false;
        self.dev.timer_armed_at = None;
        self.work_base = d.branches;
        Ok(())
    }
}
