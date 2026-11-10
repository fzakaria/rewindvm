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
use crate::{Config, DirtyLog, Machine, SLOT_RAM};

/// Where keyframe pages go: a content-addressed page store.
pub trait Pages {
    /// Stores pages and returns their hashes, in order: [`ZERO_PAGE`] for
    /// a page of zeros, which needs storing nowhere. A keyframe puts many
    /// pages at once, so a store can spread them over threads.
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>>;
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
    pub(crate) cpu: CpuState,
    pub(crate) irqchips: Vec<Vec<u8>>,
    pub(crate) devices: DeviceState,
    /// Guest page numbers and the hashes of their contents.
    pub pages: Vec<(u32, [u8; 32])>,
}

/// A keyframe without its pages: the fields of [`Keyframe`] before
/// `pages`, in its order, so a reader can decode them with serde and
/// read the pages, most of a keyframe's bytes, straight from the bytes.
#[derive(Debug, Deserialize)]
pub struct KeyframeHead {
    step: u64,
    parent: Option<u64>,
    cpu: CpuState,
    irqchips: Vec<Vec<u8>>,
    devices: DeviceState,
}

impl Keyframe {
    /// The keyframe `head` begins, with `pages`.
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CpuState {
    pub(crate) regs: Vec<u8>,
    pub(crate) sregs: Vec<u8>,
    pub(crate) xsave: Vec<u8>,
    pub(crate) xcrs: Vec<u8>,
    pub(crate) lapic: Vec<u8>,
    pub(crate) msrs: Vec<(u32, u64)>,
    pub(crate) events: Vec<u8>,
    pub(crate) debugregs: Vec<u8>,
    pub(crate) mp_state: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct DeviceState {
    pub(crate) now: u64,
    pub(crate) deadline: Option<u64>,
    pub(crate) shared: Option<u64>,
    pub(crate) epoch: u64,
    pub(crate) step: u64,
    /// Guest branches counted so far, when virtual time follows them.
    pub(crate) branches: u64,
}

/// XSTATE_BV, the XSAVE header's bitmap of the parts the area holds: the
/// index of its low word in `kvm_xsave::region`, at byte 512.
const XSTATE_BV_WORD: usize = 512 / size_of::<u32>();

/// The XSTATE_BV bits of x87 and SSE.
const X87_AND_SSE: u32 = 0b11;

/// `xsave` with x87 and SSE marked in use in its header.
///
/// A CPU may clear a part's header bit while the part holds its init
/// values, and kinds of core differ in when they do (see the guest's
/// rewind-xstate.patch): the same machine read on an Intel performance
/// core and on an efficient core would differ in XSTATE_BV alone. KVM
/// writes the init values into the area for a part whose bit is clear, so
/// setting the bits changes no register. Restoring the keyframe then marks
/// both parts in use on every Intel core, as the guest kernel's own saves
/// do.
fn x87_and_sse_in_use(mut xsave: kvm_xsave) -> kvm_xsave {
    xsave.region[XSTATE_BV_WORD] |= X87_AND_SSE;
    xsave
}

/// The bytes of a plain KVM structure.
pub(crate) fn pod<T>(value: &T) -> Vec<u8> {
    // SAFETY: the KVM structures saved here are plain C structs with no
    // pointers, so their bytes are their value.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
}

/// A plain KVM structure back from its bytes.
pub(crate) fn unpod<T: Default>(bytes: &[u8]) -> Result<T> {
    if bytes.len() != size_of::<T>() {
        bail!(
            "keyframe holds {} bytes for a {}-byte {}",
            bytes.len(),
            size_of::<T>(),
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

/// How many pages a keyframe hands the page store at once: enough for the
/// store to share among its threads, and few enough that the list of them
/// and their hashes stays small in a full keyframe, which puts every page
/// of the machine. Put all at once, a 1 GiB machine's 262,144 pages took
/// 12 MiB more at the peak of a run of echo.
#[cfg(not(test))]
const PUT_AT_ONCE: usize = 16_384;
#[cfg(test)]
const PUT_AT_ONCE: usize = 16;

/// Puts the pages of `ram` at `indices` into `pages`, PUT_AT_ONCE at a
/// time, and returns the ones a keyframe over `parent` lists, with their
/// hashes: all of them for a delta, and for a full keyframe, without a
/// parent, the ones that are not zero.
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

        // A full keyframe need not list pages that are zero anyway.
        for (&i, hash) in batch.iter().zip(hashes) {
            if parent.is_some() || hash != ZERO_PAGE {
                stored.push((i as u32, hash));
            }
        }
    }
    Ok(stored)
}

impl Machine {
    /// Captures the machine at its current step. With a parent, only the
    /// pages written since the parent was captured are stored; without
    /// one, every page written since the machine was made, by boot, the
    /// guest, the monitor or a restore, which holds every page that is not
    /// zero. A full keyframe used to read all of memory to find them, a
    /// cost that grew with the machine's memory rather than with what was
    /// written, and faulted in every page the guest had not touched: a
    /// 1 GiB machine has 262,144 pages, of which the first keyframe of a
    /// run, at step 256, lists about 16,200. That keyframe took about
    /// 120 ms either way on a loaded host, hashing those pages and
    /// writing the file; what is saved is the memory walk, which a machine
    /// with more memory paid more for. A page a full keyframe does not
    /// list restores as zero.
    pub fn keyframe(&mut self, pages: &mut dyn Pages, parent: Option<u64>) -> Result<Keyframe> {
        self.complete_pending_exit()?;

        // The dirty log is read even for a full keyframe, to clear it, so
        // the next keyframe's delta starts here; reading it also marks the
        // pages it names among the pages written since the machine was
        // made.
        let dirty = self.dirty_pages()?;
        let mut indices: Vec<usize> = match parent {
            None => self.dev.ram.written_pages(),
            Some(_) => dirty,
        };
        // The monitor writes the shared page itself, and the dirty log only
        // sees the guest's writes.
        if let Some(shared) = self.dev.shared {
            indices.push(shared as usize / PAGE_SIZE);
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

    /// Completes the port or MMIO instruction of the last exit. KVM
    /// finishes one only on the next KVM_RUN, so right after an exit the
    /// vCPU still points at it. A run with immediate_exit set completes it
    /// without entering the guest, so the machine's state is after the
    /// instruction, as a keyframe or a checkpoint keeps it. Does nothing
    /// when no exit is pending.
    pub fn complete_pending_exit(&mut self) -> Result<()> {
        // immediate_exit is set for this one run only, however the run
        // ends: left set, every later run would return at once without
        // entering the guest. The exit borrows the vCPU, so the run's
        // result is kept in words until the flag is cleared.
        self.interrupt.set_immediate_exit(1);
        let ran = self.vcpu.run().map(|exit| format!("{exit:?}"));
        self.interrupt.set_immediate_exit(0);

        match ran {
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            Err(e) => Err(e).context("completing the pending exit"),
            Ok(exit) => bail!("the vCPU ran while completing an exit: {exit}"),
        }
    }

    /// The vCPU's state, the interrupt controllers' and the monitor's.
    pub(crate) fn state(&self) -> Result<(CpuState, Vec<Vec<u8>>, DeviceState)> {
        let mut irqchips = Vec::new();
        for id in IRQCHIPS {
            let mut chip = kvm_irqchip {
                chip_id: id,
                ..Default::default()
            };
            self.vm.get_irqchip(&mut chip)?;
            irqchips.push(pod(&chip));
        }
        let devices = DeviceState {
            now: self.dev.clock.now,
            deadline: self.dev.clock.deadline,
            shared: self.dev.shared,
            epoch: self.dev.epoch,
            step: self.dev.step,
            branches: self.branches(),
        };
        Ok((self.cpu_state()?, irqchips, devices))
    }

    fn cpu_state(&self) -> Result<CpuState> {
        let v = &self.vcpu;
        Ok(CpuState {
            regs: pod(&v.get_regs()?),
            sregs: pod(&v.get_sregs()?),
            xsave: pod(&x87_and_sse_in_use(v.get_xsave()?)),
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
    /// the one to restore last. `trace` is the run's trace, record by
    /// record, which the machine's read and write records and switches go
    /// on from at the last keyframe's step. A machine that takes neither
    /// keyframes nor checkpoints restores with `DirtyLog::Off`; see
    /// [`DirtyLog`].
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

        // Memory: each page once, with the contents the last keyframe that
        // lists it gives it, which a rewind to a checkpoint reads again.
        let restored = final_pages(chain, m.dev.ram.len() / PAGE_SIZE)?;
        for (page, _) in &restored {
            m.dev.ram.mark_page(*page as usize);
        }
        read_pages(m.dev.ram.as_mut_slice(), &restored, pages)?;
        m.origin = crate::Origin::Restored(restored.into_iter().map(|(i, h)| (i, *h)).collect());
        // Start the dirty log, when there is one, afresh from the restored
        // memory.
        if m.dirty_log == DirtyLog::On {
            m.vm.get_dirty_log(SLOT_RAM, m.dev.ram.len())?;
        }

        m.apply_state(&last.cpu, &last.irqchips, &last.devices)?;
        m.resume_records(trace, last.devices.step)?;
        Ok(m)
    }

    /// Sets the record encoders for a machine just put at a keyframe at
    /// `step`: a kernel that buffers read and write records has them
    /// encoded against the ones `trace`, the run's whole trace, holds up
    /// to the step, and the switches go on from the batches written by
    /// then and the switches up to the step that the run wrote in a batch
    /// after it.
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

    /// Sets the vCPU, the interrupt controllers and the monitor's state to
    /// what a keyframe or checkpoint kept, on a machine that has not run
    /// since or has its pending exit completed. The guest's branch counter
    /// opens again from the kept count the next time the machine runs.
    pub(crate) fn apply_state(
        &mut self,
        cpu: &CpuState,
        irqchips: &[Vec<u8>],
        devices: &DeviceState,
    ) -> Result<()> {
        // The vCPU, in the order Firecracker restores it: the MSRs after
        // the local APIC, the pending events last. Of the MSRs, the ones
        // this host's KVM can hold (see [`msrs_to_restore`]).
        let c = cpu;
        let v = &self.vcpu;
        v.set_mp_state(unpod::<kvm_mp_state>(&c.mp_state)?)?;
        v.set_regs(&unpod::<kvm_regs>(&c.regs)?)?;
        v.set_sregs(&unpod::<kvm_sregs>(&c.sregs)?)?;
        // SAFETY: a kvm_xsave read back from this CPU model's KVM.
        unsafe { v.set_xsave(&unpod::<kvm_xsave>(&c.xsave)?)? };
        v.set_xcrs(&unpod::<kvm_xcrs>(&c.xcrs)?)?;
        v.set_debug_regs(&unpod::<kvm_debugregs>(&c.debugregs)?)?;
        v.set_lapic(&unpod::<kvm_lapic_state>(&c.lapic)?)?;
        let listed = self.kvm.get_msr_index_list()?;
        let entries: Vec<kvm_msr_entry> = msrs_to_restore(&c.msrs, listed.as_slice())
            .into_iter()
            .map(|(index, data)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let msrs = Msrs::from_entries(&entries)?;
        let n = v.set_msrs(&msrs)?;
        if n != entries.len() {
            bail!(
                "KVM took {n} of {} MSRs; the next was {}, recorded as {:#x}",
                entries.len(),
                crate::state::msr_name(entries[n].index),
                entries[n].data
            );
        }
        v.set_vcpu_events(&unpod::<kvm_vcpu_events>(&c.events)?)?;

        for chip in irqchips {
            self.vm.set_irqchip(&unpod::<kvm_irqchip>(chip)?)?;
        }

        let d = devices;
        self.dev.clock = Clock {
            now: d.now,
            deadline: d.deadline,
            quantum: self.config.quantum,
        };
        self.dev.shared = d.shared;
        // A keyframe taken after setup restores the interface the guest
        // kernel named then, which must be one this monitor speaks.
        if let Some(shared) = self.dev.shared {
            crate::pv::check_interface(self.dev.guest_interface(shared)?)?;
        }
        self.dev.epoch = d.epoch;
        self.dev.step = d.step;
        self.dev.stop = None;
        self.dev.inspect = false;
        self.dev.input_sent = false;
        self.dev.timer_armed_at = None;
        self.work = None;
        self.work_base = d.branches;
        self.branch_stop = None;
        self.crossing = None;
        self.exception_trap = None;
        Ok(())
    }
}

/// The most threads that read a restored machine's pages, and the fewest
/// pages worth more than one. Each page costs a read, a decompression
/// and a hash check, about 11 µs on one thread: a late keyframe of a
/// long build restores some 700,000 pages, 8 s of it.
const MAX_PAGE_READERS: usize = 16;
const PAGES_PER_READER: usize = 4096;

/// Reads each of `restored`, a page number and its hash, from `pages`
/// into its page of `ram`, on as many threads as the host has, up to
/// MAX_PAGE_READERS. Each page is written by one thread only, so the
/// memory comes out the same however the reads are shared.
pub(crate) fn read_pages(
    ram: &mut [u8],
    restored: &[(u32, &[u8; 32])],
    pages: &(dyn Pages + Sync),
) -> Result<()> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_PAGE_READERS)
        .min(restored.len().div_ceil(PAGES_PER_READER))
        .max(1);

    // Each restored page's own slice of the memory, paired with its hash.
    let mut slots: Vec<Option<&mut [u8]>> = ram.chunks_mut(PAGE_SIZE).map(Some).collect();
    let mut work = Vec::with_capacity(restored.len());
    for &(index, hash) in restored {
        let slot = slots
            .get_mut(index as usize)
            .and_then(Option::take)
            .with_context(|| format!("page {index} is restored twice or past the memory"))?;
        work.push((slot, hash));
    }
    let share = work.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let readers: Vec<_> = work
            .chunks_mut(share)
            .map(|part| {
                scope.spawn(move || -> Result<()> {
                    for (out, hash) in part.iter_mut() {
                        pages.get(hash, out)?;
                    }
                    Ok(())
                })
            })
            .collect();
        for reader in readers {
            reader.join().expect("a page reader panicked")?;
        }
        Ok(())
    })
}

/// The pages restoring `chain` leaves non-zero, in page order, each once
/// with the hash of the contents the last keyframe that lists it gives it:
/// each keyframe's pages are written over the ones before it. A page whose
/// last contents are zero is left out, since a new machine's memory is
/// zero. A chain of keyframes taken while a run executes lists the pages a
/// build keeps rewriting many times over, so this reads each from the
/// store once instead of once per keyframe. Fails when a page is past the
/// machine's `ram_pages`.
pub(crate) fn final_pages<'k>(
    chain: impl IntoIterator<Item = &'k Keyframe>,
    ram_pages: usize,
) -> Result<Vec<(u32, &'k [u8; 32])>> {
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

/// The MSRs of `recorded`, a keyframe's, that a restore sets on a host
/// whose KVM lists `listed`. A keyframe holds every MSR the recording
/// host's KVM listed, and which those are depends on the host's CPU. One
/// this host's KVM does not list is left out when the keyframe holds it as
/// 0, the value a vCPU that never had it stands for, and when the guest
/// cannot read it at all ([`crate::state::host_only_msr`]). Any other
/// stays, for KVM to take or to refuse by name: the guest may have read
/// its value.
fn msrs_to_restore(recorded: &[(u32, u64)], listed: &[u32]) -> Vec<(u32, u64)> {
    recorded
        .iter()
        .copied()
        .filter(|(index, data)| {
            listed.contains(index) || !(*data == 0 || crate::state::host_only_msr(*index))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    // Keyframe chains as restoring reads them, with no VM.
    use super::*;

    #[test]
    fn a_restore_leaves_out_msrs_only_the_recording_host_had() {
        // A keyframe from a Zen 4, whose KVM lists the PerfMonV2 controls,
        // restored where KVM lists neither them nor an MSR the keyframe
        // holds as 0: both are left out, as is an unlisted counter with a
        // value, which the VM never counts with, and unlisted host time.
        // What this host lists is set as recorded, zero or not. An
        // unlisted MSR with a value the guest could read stays, for KVM
        // to refuse by name.
        const PERFMON_V2_STATUS: u32 = 0xc000_0300;
        const PERF_CTL0: u32 = 0xc001_0000;
        const HYPERV_TSC_FREQUENCY: u32 = 0x4000_0022;
        const LSTAR: u32 = 0xc000_0082;
        const TSC_AUX: u32 = 0xc000_0103;
        const UNKNOWN_ZERO: u32 = 0x1234;
        const UNKNOWN_SET: u32 = 0x1235;
        let recorded = [
            (LSTAR, 0xffff_ffff_8100_0000),
            (TSC_AUX, 0),
            (PERFMON_V2_STATUS, 0),
            (PERF_CTL0, 0x43_00c2),
            (HYPERV_TSC_FREQUENCY, 3_294_861_576),
            (UNKNOWN_ZERO, 0),
            (UNKNOWN_SET, 7),
        ];
        let zen3 = [LSTAR, TSC_AUX];
        assert_eq!(
            msrs_to_restore(&recorded, &zen3),
            [
                (LSTAR, 0xffff_ffff_8100_0000),
                (TSC_AUX, 0),
                (UNKNOWN_SET, 7)
            ]
        );

        // On the recording host every MSR is listed, and all are set.
        let zen4: Vec<u32> = recorded.iter().map(|(index, _)| *index).collect();
        assert_eq!(msrs_to_restore(&recorded, &zen4), recorded);
    }

    /// A keyframe whose vCPU has `regs`, `msrs` and every other structure
    /// at KVM's zero value, for tests of what reads its state by name.
    pub(crate) fn with_cpu(regs: &kvm_regs, msrs: &[(u32, u64)]) -> Keyframe {
        Keyframe {
            cpu: CpuState {
                regs: pod(regs),
                sregs: pod(&kvm_sregs::default()),
                xsave: pod(&kvm_xsave::default()),
                xcrs: pod(&kvm_xcrs::default()),
                lapic: pod(&kvm_lapic_state::default()),
                msrs: msrs.to_vec(),
                events: pod(&kvm_vcpu_events::default()),
                debugregs: pod(&kvm_debugregs::default()),
                mp_state: pod(&kvm_mp_state::default()),
            },
            ..Keyframe::default()
        }
    }

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
    fn a_captured_xsave_area_marks_x87_and_sse_in_use() {
        // XSTATE_BV as each kind of CPU may report it with x87 and SSE at
        // their init values: a capture sets both bits whatever the header
        // said, and leaves AVX's bit and the registers as they were.
        for header in [0b000, 0b001, 0b010, 0b011, 0b100, 0b111] {
            let mut xsave = kvm_xsave::default();
            xsave.region[0] = 0x037f;
            xsave.region[XSTATE_BV_WORD] = header;
            let captured = x87_and_sse_in_use(xsave);
            assert_eq!(captured.region[XSTATE_BV_WORD], header | X87_AND_SSE);
            assert_eq!(captured.region[0], 0x037f);
        }
    }

    #[test]
    fn a_page_a_full_keyframe_leaves_out_restores_as_zero() {
        // A full keyframe of a four-page machine lists pages 0 and 2, as
        // one taken from the pages written since boot does when pages 1
        // and 3 were never touched. Restoring reads only the two listed
        // pages into zeroed memory, so the unlisted pages are zero, the
        // same as when the keyframe listed them as the zero page.
        let listed = [kf(10, None, &[(0, 1), (2, 3)])];
        let with_zeros = [kf(10, None, &[(0, 1), (1, 0), (2, 3), (3, 0)])];
        let pages: Vec<(u32, u8)> = final_pages(&listed, 4)
            .unwrap()
            .into_iter()
            .map(|(i, h)| (i, h[0]))
            .collect();
        assert_eq!(pages, [(0, 1), (2, 3)]);
        assert_eq!(
            final_pages(&with_zeros, 4).unwrap(),
            final_pages(&listed, 4).unwrap()
        );

        let mut ram = vec![0xffu8; 4 * PAGE_SIZE];
        let restored = final_pages(&listed, 4).unwrap();
        read_pages(&mut ram, &restored, &ByFirstByte).unwrap();
        for (page, fill) in [(0, 1), (2, 3)] {
            assert!(ram[page * PAGE_SIZE..(page + 1) * PAGE_SIZE] == vec![fill; PAGE_SIZE]);
        }
        // A new machine's memory is zero; here the unlisted pages keep
        // what was there, which restoring never reads.
        for page in [1, 3] {
            assert!(ram[page * PAGE_SIZE..(page + 1) * PAGE_SIZE] == vec![0xffu8; PAGE_SIZE]);
        }
    }

    /// A page store whose every page is its hash's first byte repeated.
    struct ByFirstByte;

    impl Pages for ByFirstByte {
        fn put_all(&mut self, _pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
            bail!("only gets")
        }
        fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
            out.fill(hash[0]);
            Ok(())
        }
    }

    #[test]
    fn a_page_past_the_machines_memory_is_refused() {
        // A keyframe naming page 8 of an eight-page machine does not restore.
        let chain = [kf(10, None, &[(8, 1)])];
        assert!(final_pages(&chain, 8).is_err());
    }

    /// A page store that names each page by its first byte and notes how
    /// many pages each put held.
    #[derive(Default)]
    struct Batches(Vec<usize>);

    impl Pages for Batches {
        fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
            self.0.push(pages.len());
            Ok(pages
                .iter()
                .map(|page| match page[0] {
                    0 => ZERO_PAGE,
                    b => [b; 32],
                })
                .collect())
        }
        fn get(&self, _hash: &[u8; 32], _out: &mut [u8]) -> Result<()> {
            bail!("only puts")
        }
    }

    #[test]
    fn a_keyframe_puts_its_pages_a_bounded_batch_at_a_time() {
        // A full keyframe of a machine more than two batches large, every
        // third page not zero: the store is handed at most PUT_AT_ONCE
        // pages a time, so what a keyframe holds besides the pages stays
        // small however large the machine, and the keyframe lists every
        // page that is not zero, in order. A delta lists its zero pages
        // too, since each overwrote a page that was not zero.
        let count = 2 * PUT_AT_ONCE + 5;
        let mut ram = vec![0u8; count * PAGE_SIZE];
        for i in (0..count).step_by(3) {
            ram[i * PAGE_SIZE] = 1 + (i % 250) as u8;
        }
        let indices: Vec<usize> = (0..count).collect();

        let mut store = Batches::default();
        let stored = put_pages(&mut store, &ram, &indices, None).unwrap();
        assert_eq!(store.0, vec![PUT_AT_ONCE, PUT_AT_ONCE, 5]);
        let expected: Vec<(u32, [u8; 32])> = (0..count)
            .step_by(3)
            .map(|i| (i as u32, [1 + (i % 250) as u8; 32]))
            .collect();
        assert_eq!(stored, expected);

        let delta = put_pages(&mut Batches::default(), &ram, &[1, 3], Some(256)).unwrap();
        assert_eq!(delta, vec![(1, ZERO_PAGE), (3, [4; 32])]);
    }
}
