//! Keyframes: capturing a machine at a step, and restoring it.
//!
//! A keyframe holds everything that decides what the machine does next:
//! the vCPU's registers, extended state, MSRs, local APIC and pending
//! events, the in-kernel interrupt controllers, the monitor's own device
//! state, and guest memory. Memory is stored as page hashes in a page
//! store, and only the pages written since the previous keyframe, which
//! KVM's dirty log names, so a chain of keyframes costs about what the
//! guest wrote between them.
//!
//! Restoring a keyframe and running on reaches the same steps with the
//! same events as the original run did; `rewind replay --from` checks it.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, Msrs, kvm_debugregs,
    kvm_irqchip, kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_regs, kvm_sregs,
    kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use serde::{Deserialize, Serialize};

use crate::layout::PAGE_SIZE;
use crate::pv::Clock;
use crate::{Config, Machine, SLOT_RAM};

/// Where keyframe pages go: a content-addressed page store.
pub trait Pages {
    /// Stores a page and returns its hash.
    fn put(&mut self, page: &[u8]) -> Result<[u8; 32]>;
    /// Reads the page with this hash into `out`.
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()>;
}

/// The hash the all-zero page goes by; see rewind-store.
pub const ZERO_PAGE: [u8; 32] = [0; 32];

/// A machine at a step. The default is an empty one with no pages, for
/// tests of what reads only a keyframe's pages.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Keyframe {
    pub step: u64,
    /// The step of the keyframe whose memory this one's pages are written
    /// over, or None when this one holds every non-zero page.
    pub parent: Option<u64>,
    cpu: CpuState,
    irqchips: Vec<Vec<u8>>,
    devices: DeviceState,
    /// Guest page numbers and the hashes of their contents.
    pub pages: Vec<(u32, [u8; 32])>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CpuState {
    regs: Vec<u8>,
    sregs: Vec<u8>,
    xsave: Vec<u8>,
    xcrs: Vec<u8>,
    lapic: Vec<u8>,
    msrs: Vec<(u32, u64)>,
    events: Vec<u8>,
    debugregs: Vec<u8>,
    mp_state: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DeviceState {
    now: u64,
    deadline: Option<u64>,
    shared: Option<u64>,
    epoch: u64,
    step: u64,
    /// Guest branches counted so far, when virtual time follows them.
    branches: u64,
}

/// The bytes of a plain KVM structure.
fn pod<T>(value: &T) -> Vec<u8> {
    // SAFETY: the KVM structures saved here are plain C structs with no
    // pointers, so their bytes are their value.
    unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
    }
    .to_vec()
}

/// A plain KVM structure back from its bytes.
fn unpod<T: Default>(bytes: &[u8]) -> Result<T> {
    if bytes.len() != std::mem::size_of::<T>() {
        bail!(
            "keyframe holds {} bytes for a {}-byte {}",
            bytes.len(),
            std::mem::size_of::<T>(),
            std::any::type_name::<T>()
        );
    }
    let mut value = T::default();
    // SAFETY: as in pod, and the length matches.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&mut value as *mut T).cast::<u8>(),
            bytes.len(),
        );
    }
    Ok(value)
}

const IRQCHIPS: [u32; 3] = [
    KVM_IRQCHIP_PIC_MASTER,
    KVM_IRQCHIP_PIC_SLAVE,
    KVM_IRQCHIP_IOAPIC,
];

impl Machine {
    /// Captures the machine at its current step. With a parent, only the
    /// pages written since the parent was captured are stored; without
    /// one, every non-zero page is.
    pub fn keyframe(&mut self, pages: &mut dyn Pages, parent: Option<u64>) -> Result<Keyframe> {
        // KVM finishes a port or MMIO instruction only on the next
        // KVM_RUN, so right after an exit the vCPU still points at it. A
        // run with immediate_exit set completes it without entering the
        // guest, and the state saved below is after the instruction.
        self.vcpu.set_kvm_immediate_exit(1);
        match self.vcpu.run() {
            Err(e) if e.errno() == libc::EINTR => {}
            Err(e) => return Err(e).context("completing the pending exit"),
            Ok(exit) => bail!("the vCPU ran while completing an exit: {exit:?}"),
        }
        self.vcpu.set_kvm_immediate_exit(0);

        let ram_pages = self.dev.ram.len() / PAGE_SIZE;

        // The dirty log is read even for a full keyframe, to clear it, so
        // the next keyframe's delta starts here.
        let dirty = self
            .vm
            .get_dirty_log(SLOT_RAM, self.dev.ram.len())
            .context("reading the dirty log")?;
        let mut indices: Vec<usize> = match parent {
            None => (0..ram_pages).collect(),
            Some(_) => (0..ram_pages)
                .filter(|&i| dirty[i / 64] & (1 << (i % 64)) != 0)
                .collect(),
        };
        // The monitor writes the shared page itself, and the dirty log only
        // sees the guest's writes.
        if let Some(shared) = self.dev.shared {
            indices.push(shared as usize / PAGE_SIZE);
            indices.sort_unstable();
            indices.dedup();
        }

        let ram = self.dev.ram.as_slice();
        let mut stored = Vec::with_capacity(indices.len());
        for i in indices {
            let page = &ram[i * PAGE_SIZE..(i + 1) * PAGE_SIZE];
            let hash = if page.iter().all(|b| *b == 0) {
                ZERO_PAGE
            } else {
                pages.put(page)?
            };
            // A full keyframe need not list pages that are zero anyway.
            if parent.is_some() || hash != ZERO_PAGE {
                stored.push((i as u32, hash));
            }
        }

        let mut irqchips = Vec::new();
        for id in IRQCHIPS {
            let mut chip = kvm_irqchip {
                chip_id: id,
                ..Default::default()
            };
            self.vm.get_irqchip(&mut chip)?;
            irqchips.push(pod(&chip));
        }

        Ok(Keyframe {
            step: self.dev.step,
            parent,
            cpu: self.cpu_state()?,
            irqchips,
            devices: DeviceState {
                now: self.dev.clock.now,
                deadline: self.dev.clock.deadline,
                shared: self.dev.shared,
                epoch: self.dev.epoch,
                step: self.dev.step,
                branches: self.branches(),
            },
            pages: stored,
        })
    }

    fn cpu_state(&self) -> Result<CpuState> {
        let v = &self.vcpu;
        Ok(CpuState {
            regs: pod(&v.get_regs()?),
            sregs: pod(&v.get_sregs()?),
            xsave: pod(&v.get_xsave()?),
            xcrs: pod(&v.get_xcrs()?),
            lapic: pod(&v.get_lapic()?),
            msrs: self.read_msrs()?,
            events: pod(&v.get_vcpu_events()?),
            debugregs: pod(&v.get_debug_regs()?),
            mp_state: pod(&v.get_mp_state()?),
        })
    }

    /// Every MSR KVM lists that this vCPU can read. KVM stops a batch read
    /// at the first MSR it cannot read, so the batch resumes after it.
    fn read_msrs(&self) -> Result<Vec<(u32, u64)>> {
        let list = self.kvm.get_msr_index_list()?;
        let indices: Vec<u32> = list.as_slice().to_vec();
        let mut out = Vec::with_capacity(indices.len());
        let mut at = 0;
        while at < indices.len() {
            let entries: Vec<kvm_msr_entry> = indices[at..]
                .iter()
                .map(|&index| kvm_msr_entry {
                    index,
                    ..Default::default()
                })
                .collect();
            let mut msrs = Msrs::from_entries(&entries)?;
            let n = self.vcpu.get_msrs(&mut msrs)?;
            out.extend(msrs.as_slice()[..n].iter().map(|e| (e.index, e.data)));
            at += n + 1;
        }
        Ok(out)
    }

    /// A machine restored from a chain of keyframes, the full one first and
    /// the one to restore last.
    pub fn restore(config: &Config, chain: &[Keyframe], pages: &dyn Pages) -> Result<Machine> {
        let last = chain.last().context("restoring from no keyframes")?;
        if chain[0].parent.is_some() {
            bail!("a keyframe chain must start with a full keyframe");
        }
        let mut m = Self::create(config)?;

        // Memory: each page once, with the contents the last keyframe that
        // lists it gives it.
        let ram = m.dev.ram.as_mut_slice();
        for (index, hash) in final_pages(chain, ram.len() / PAGE_SIZE)? {
            let at = index as usize * PAGE_SIZE;
            pages.get(hash, &mut ram[at..at + PAGE_SIZE])?;
        }
        // Start the dirty log afresh from the restored memory.
        m.vm.get_dirty_log(SLOT_RAM, m.dev.ram.len())?;

        // The vCPU, in the order Firecracker restores it: the MSRs after
        // the local APIC, the pending events last.
        let c = &last.cpu;
        let v = &m.vcpu;
        v.set_mp_state(unpod::<kvm_mp_state>(&c.mp_state)?)?;
        v.set_regs(&unpod::<kvm_regs>(&c.regs)?)?;
        v.set_sregs(&unpod::<kvm_sregs>(&c.sregs)?)?;
        // SAFETY: a kvm_xsave read back from this CPU model's KVM.
        unsafe { v.set_xsave(&unpod::<kvm_xsave>(&c.xsave)?)? };
        v.set_xcrs(&unpod::<kvm_xcrs>(&c.xcrs)?)?;
        v.set_debug_regs(&unpod::<kvm_debugregs>(&c.debugregs)?)?;
        v.set_lapic(&unpod::<kvm_lapic_state>(&c.lapic)?)?;
        let entries: Vec<kvm_msr_entry> = c
            .msrs
            .iter()
            .map(|&(index, data)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let msrs = Msrs::from_entries(&entries)?;
        let n = v.set_msrs(&msrs)?;
        if n != entries.len() {
            bail!(
                "KVM took {n} of {} MSRs; the next was {:#x}",
                entries.len(),
                entries[n].index
            );
        }
        v.set_vcpu_events(&unpod::<kvm_vcpu_events>(&c.events)?)?;

        for chip in &last.irqchips {
            m.vm.set_irqchip(&unpod::<kvm_irqchip>(chip)?)?;
        }

        let d = &last.devices;
        m.dev.clock = Clock {
            now: d.now,
            deadline: d.deadline,
            quantum: config.quantum,
        };
        m.dev.shared = d.shared;
        // A keyframe taken after setup restores the interface the guest
        // kernel named then, which must be this monitor's too.
        if let Some(shared) = m.dev.shared {
            crate::pv::check_interface(m.dev.guest_interface(shared)?)?;
        }
        m.dev.epoch = d.epoch;
        m.dev.step = d.step;
        m.work_base = d.branches;
        Ok(m)
    }
}

/// The pages restoring `chain` leaves non-zero, in page order, each once
/// with the hash of the contents the last keyframe that lists it gives it:
/// each keyframe's pages are written over the ones before it. A page whose
/// last contents are zero is left out, since a new machine's memory is
/// zero. A chain of keyframes taken while a run executes lists the pages a
/// build keeps rewriting many times over, so this reads each from the
/// store once instead of once per keyframe. Fails when a page is past the
/// machine's `ram_pages`.
fn final_pages(chain: &[Keyframe], ram_pages: usize) -> Result<Vec<(u32, &[u8; 32])>> {
    let mut last: Vec<Option<&[u8; 32]>> = vec![None; ram_pages];
    for kf in chain {
        for (index, hash) in &kf.pages {
            let slot = last
                .get_mut(*index as usize)
                .context("keyframe page is outside the VM's memory")?;
            *slot = Some(hash);
        }
    }
    Ok((0u32..)
        .zip(last)
        .filter_map(|(index, hash)| Some((index, hash?)))
        .filter(|(_, hash)| **hash != ZERO_PAGE)
        .collect())
}

#[cfg(test)]
mod tests {
    // Keyframe chains as restoring reads them, with no VM.
    use super::*;

    /// A keyframe at `step` over the one at `parent`, listing `pages`.
    fn kf(step: u64, parent: Option<u64>, pages: &[(u32, u8)]) -> Keyframe {
        Keyframe {
            step,
            parent,
            pages: pages.iter().map(|&(i, h)| (i, [h; 32])).collect(),
            ..Keyframe::default()
        }
    }

    #[test]
    fn a_chain_restores_each_page_once_with_its_last_contents() {
        // A full keyframe and two over it that rewrite page 1 twice and page
        // 2 back to zero: page 1 is restored once, from the last keyframe,
        // page 2 not at all, since a new machine's memory is zero, and the
        // pages come in page order.
        let chain = [
            kf(10, None, &[(0, 1), (1, 2), (2, 3)]),
            kf(20, Some(10), &[(1, 4), (3, 5)]),
            kf(30, Some(20), &[(1, 6), (2, 0)]),
        ];
        let pages: Vec<(u32, u8)> = final_pages(&chain, 8)
            .unwrap()
            .into_iter()
            .map(|(i, h)| (i, h[0]))
            .collect();
        assert_eq!(pages, [(0, 1), (1, 6), (3, 5)]);
    }

    #[test]
    fn a_page_past_the_machines_memory_is_refused() {
        // A keyframe naming page 8 of an eight-page machine does not restore.
        let chain = [kf(10, None, &[(8, 1)])];
        assert!(final_pages(&chain, 8).is_err());
    }
}
