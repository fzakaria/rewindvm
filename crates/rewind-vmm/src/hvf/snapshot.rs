//! Keyframes on Apple Silicon Hypervisor.framework: capturing and restoring
//! full ARM64 vCPU state, device state, and content-addressed guest memory.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hvf::layout::PAGE_SIZE;
use crate::hvf::state::SAVED_SYS_REGS;
use crate::hvf::sys;
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

/// Complete architectural ARM64 vCPU state captured from HVF.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CpuState {
    /// `X0..=X30` (31), `PC` (31), `FPCR` (32), `FPSR` (33), `CPSR` (34).
    pub(crate) regs: Vec<u64>,
    /// `Q0..=Q31` (32 × 16-byte SIMD/FP registers).
    pub(crate) simd: Vec<[u8; 16]>,
    /// Architectural EL0/EL1 system registers as `(hv_sys_reg_t, value)`.
    pub(crate) sys_regs: Vec<(u16, u64)>,
    /// Whether the monitor's virtual IRQ line is currently asserted.
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

/// CPU, device, and copied dirty guest memory pages captured from a live
/// [`Machine`] on its pinned vCPU thread/process, ready to be committed into a
/// [`Pages`] store via [`finish_keyframe`].
#[derive(Debug, Serialize, Deserialize)]
pub struct PreparedKeyframe {
    pub step: u64,
    pub parent: Option<u64>,
    pub(crate) cpu: CpuState,
    pub(crate) irqchips: Vec<Vec<u8>>,
    pub(crate) devices: DeviceState,
    pub(crate) indices: Vec<usize>,
    pub(crate) data: Vec<u8>,
}

/// Commits a [`PreparedKeyframe`] into `pages`, returning the finalized
/// [`Keyframe`].
pub fn finish_keyframe(prepared: PreparedKeyframe, pages: &mut dyn Pages) -> Result<Keyframe> {
    let mut stored = Vec::new();
    for (batch_idx, batch) in prepared.indices.chunks(PUT_AT_ONCE).enumerate() {
        let base = batch_idx * PUT_AT_ONCE;
        let contents: Vec<&[u8]> = (0..batch.len())
            .map(|k| {
                let off = (base + k) * PAGE_SIZE;
                &prepared.data[off..off + PAGE_SIZE]
            })
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
            if prepared.parent.is_some() || hash != ZERO_PAGE {
                stored.push((i as u32, hash));
            }
        }
    }
    Ok(Keyframe {
        step: prepared.step,
        parent: prepared.parent,
        cpu: prepared.cpu,
        irqchips: prepared.irqchips,
        devices: prepared.devices,
        pages: stored,
    })
}

impl Machine {
    /// Captures dirty pages, CPU state, and device state without touching the
    /// [`Pages`] store, so cluster workers can prepare keyframes concurrently.
    pub fn prepare_keyframe(&mut self, parent: Option<u64>) -> Result<PreparedKeyframe> {
        self.complete_pending_exit()?;

        let dirty = self.dirty_pages()?;
        let mut indices: Vec<usize> = match parent {
            None => self.dev.ram.written_pages(),
            Some(_) => dirty,
        };
        if let Some(shared) = self.dev.shared
            && let Some(off) = shared.checked_sub(crate::hvf::layout::RAM_BASE)
        {
            indices.push(off as usize / PAGE_SIZE);
        }
        indices.append(&mut self.dev.monitor_dirty_pages);
        if !indices.is_empty() {
            indices.sort_unstable();
            indices.dedup();
        }

        let ram = self.dev.ram.as_slice();
        let mut data = Vec::with_capacity(indices.len() * PAGE_SIZE);
        for &i in &indices {
            data.extend_from_slice(&ram[i * PAGE_SIZE..(i + 1) * PAGE_SIZE]);
        }

        let (cpu, irqchips, devices) = self.state()?;
        Ok(PreparedKeyframe {
            step: self.dev.step,
            parent,
            cpu,
            irqchips,
            devices,
            indices,
            data,
        })
    }

    /// Captures the ARM64 VM at its current step.
    pub fn keyframe(&mut self, pages: &mut dyn Pages, parent: Option<u64>) -> Result<Keyframe> {
        self.complete_pending_exit()?;

        let dirty = self.dirty_pages()?;
        let mut indices: Vec<usize> = match parent {
            None => self.dev.ram.written_pages(),
            Some(_) => dirty,
        };
        if let Some(shared) = self.dev.shared
            && let Some(off) = shared.checked_sub(crate::hvf::layout::RAM_BASE)
        {
            indices.push(off as usize / PAGE_SIZE);
        }
        indices.append(&mut self.dev.monitor_dirty_pages);
        if !indices.is_empty() {
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

    /// Captures a full standalone keyframe without draining or clearing the
    /// dirty tracker (for zero-perturbation live inspection).
    pub fn snapshot_non_destructive(&mut self, pages: &mut dyn Pages) -> Result<Keyframe> {
        self.complete_pending_exit()?;

        let ram = self.dev.ram.as_slice();
        let page_count = ram.len() / PAGE_SIZE;
        let mut indices = Vec::new();
        for i in 0..page_count {
            let chunk = &ram[i * PAGE_SIZE..(i + 1) * PAGE_SIZE];
            if chunk.iter().any(|&b| b != 0) {
                indices.push(i);
            }
        }
        let stored = put_pages(pages, ram, &indices, None)?;
        let (cpu, irqchips, devices) = self.state()?;
        Ok(Keyframe {
            step: self.dev.step,
            parent: None,
            cpu,
            irqchips,
            devices,
            pages: stored,
        })
    }

    /// On HVF, synchronous exits (`HVC`, `MMIO`) complete immediately during the
    /// exit handler (there is no deferred instruction completion like KVM's I/O
    /// port completion), so this is a no-op.
    pub fn complete_pending_exit(&mut self) -> Result<()> {
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
        let mut regs = Vec::with_capacity(35);
        for reg in sys::HV_REG_X0..=sys::HV_REG_CPSR {
            regs.push(self.vcpu.get_reg(reg)?);
        }
        let mut simd = Vec::with_capacity(32);
        for q in 0..32 {
            simd.push(self.vcpu.get_simd(q)?);
        }
        let mut sys_regs = Vec::with_capacity(SAVED_SYS_REGS.len());
        for &(id, _) in SAVED_SYS_REGS {
            if let Ok(val) = self.vcpu.get_sys_reg(id) {
                sys_regs.push((id, val));
            }
        }
        Ok(CpuState {
            regs,
            simd,
            sys_regs,
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
            m.dirty.arm_all(&m.vm)?;
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
        if cpu.regs.len() != 35 {
            bail!(
                "ARM64 keyframe holds {} core registers; expected 35",
                cpu.regs.len()
            );
        }
        if cpu.simd.len() != 32 {
            bail!(
                "ARM64 keyframe holds {} SIMD registers; expected 32",
                cpu.simd.len()
            );
        }
        for (i, &val) in cpu.regs.iter().enumerate() {
            self.vcpu.set_reg(i as u32, val)?;
        }
        for (q, &bytes) in cpu.simd.iter().enumerate() {
            self.vcpu.set_simd(q as u32, bytes)?;
        }
        for &(id, val) in &cpu.sys_regs {
            self.vcpu
                .set_sys_reg(id, val)
                .with_context(|| format!("restoring {}", crate::hvf::state::sys_reg_name(id)))?;
        }
        self.irq_pending = cpu.irq_pending;
        self.vcpu.set_vtimer_mask(true)?;

        let d = devices;
        self.dev.clock = Clock {
            now: d.now,
            deadline: d.deadline,
            quantum: self.config.quantum,
        };
        self.dev.shared = d.shared;
        self.dev.net_rx_ring = None;
        self.dev.net_rx_slots = 0;
        if let Some(shared) = self.dev.shared {
            crate::pv::check_interface(self.dev.guest_interface(shared)?)?;
            if self.config.net.is_some() {
                let mut ring_b = [0u8; 8];
                let mut slots_b = [0u8; 4];
                self.dev
                    .ram_read(shared + crate::pv::SHARED_NET_RX_RING, &mut ring_b)?;
                self.dev
                    .ram_read(shared + crate::pv::SHARED_NET_RX_SLOTS, &mut slots_b)?;
                let ring = u64::from_le_bytes(ring_b);
                let slots = u32::from_le_bytes(slots_b);
                if ring != 0 && (1..=crate::pv::NET_RX_SLOTS_MAX).contains(&slots) {
                    self.dev.net_rx_ring = Some(ring);
                    self.dev.net_rx_slots = slots;
                }
            }
        }
        self.dev.epoch = d.epoch;
        self.dev.step = d.step;
        self.dev.stop = None;
        self.dev.idle = false;
        self.dev.net_rx_pending = false;
        self.dev.tx_queue.clear();
        self.dev.tx_pending = None;
        self.dev.monitor_dirty_pages.clear();
        self.dev.timer_armed_at = None;
        self.passing_breakpoint = false;
        self.watching_step = None;
        self.work_base = d.branches;
        Ok(())
    }
}
