//! A deterministic virtual machine monitor on KVM.
//!
//! One vCPU runs a Linux guest built with the Rewind platform patch. The
//! monitor never interrupts the guest: the vCPU runs until the guest itself
//! exits with a port or MMIO access, and only then does the monitor move
//! virtual time or inject an interrupt. Every such exit is a guest
//! instruction, so the sequence of exits is a function of the guest's
//! inputs, and so is everything the guest does between them.
//!
//! A step is one guest exit. [`Machine::run`] runs to a given step or until
//! the guest stops, reporting each record the guest emits to an
//! [`Observer`] along with the step it arrived on.

pub mod boot;
pub mod checkpoint;
pub mod cpu;
pub mod debug;
pub mod interrupt;
pub mod kvm;
pub mod layout;
pub mod memory;
pub mod pmu;
pub mod pv;
pub mod snapshot;
pub mod state;
mod watchdog;

#[cfg(test)]
mod test_alloc;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_MEM_LOG_DIRTY_PAGES, KVM_MEM_READONLY, kvm_msi, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

pub use interrupt::Interrupter;
use layout::*;
use memory::Mapping;
use pv::{Clock, GuestExit, Schedule};
pub use rewind_trace::machine::{ClockSource, CounterEvent, CpuModel, Extras, Preemption};

/// The memory slot of guest RAM, and of the input image.
pub(crate) const SLOT_RAM: u32 = 0;
const SLOT_PMEM: u32 = 1;
const SLOT_EXTRAS: u32 = 2;

/// Where KVM places the TSS the vCPU needs for real-mode emulation. Any
/// three pages the guest never uses; this is the address other monitors use.
const TSS_ADDRESS: usize = 0xfffb_d000;

/// The persistent memory region is sized in 2 MiB steps, which is what the
/// kernel's memory hotplug code expects of it.
const PMEM_ALIGN: usize = 2 << 20;

/// Whether a run on this host reserves the extras slot. The slot ends past
/// what a CPU with 36 physical address bits reaches, such as an Ivy Bridge
/// or a VM shown 36 bits, and KVM refuses a memory slot there.
pub fn extras_for(cpu: CpuModel) -> Result<Extras> {
    let kvm = kvm::open()?;
    extras_for_bits(cpu, cpu::address_bits(&kvm)?)
}

/// The extras slot for `cpu` on a host whose CPU reaches `bits` physical
/// address bits. The host model goes without the slot where the host
/// cannot reach the slot. The fixed models, v3 and v3-compat, promise the
/// guest the same machine on every host, slot included, so a narrower host
/// refuses them.
fn extras_for_bits(cpu: CpuModel, bits: u32) -> Result<Extras> {
    if bits >= EXTRAS_ADDRESS_BITS {
        return Ok(Extras::Reserved);
    }
    match cpu {
        CpuModel::Host => Ok(Extras::Absent),
        CpuModel::V3 | CpuModel::V3Compat => bail!(
            "this CPU reaches {bits} physical address bits, where the x86-64-v3 model needs \
             {EXTRAS_ADDRESS_BITS}; run with --cpu host"
        ),
    }
}

/// Fails, saying where the run replays, when a recorded run reserves the
/// extras slot and this host's CPU reaches only `bits` physical address
/// bits, short of the slot.
fn recorded_slot_fits(bits: u32) -> Result<()> {
    if bits >= EXTRAS_ADDRESS_BITS {
        return Ok(());
    }
    bail!(
        "this run reserves the extras slot, which needs {EXTRAS_ADDRESS_BITS} physical address \
         bits, and this CPU reaches {bits}; replay it on a host whose CPU reaches \
         {EXTRAS_ADDRESS_BITS}. Recording it again here with --cpu host makes a different run, \
         without the slot"
    )
}

/// Everything that determines a run. Two machines built from equal configs
/// run identically.
#[derive(Clone, Debug)]
pub struct Config {
    pub kernel: PathBuf,
    pub initrd: Vec<u8>,
    pub image: Option<PathBuf>,
    pub mem_bytes: u64,
    pub cmdline: String,
    pub seed: [u8; 32],
    /// The guest's wall clock at boot, in seconds since the Unix epoch.
    pub epoch: u64,
    /// Nanoseconds of virtual time per exit.
    pub quantum: u64,
    /// Where to ask the guest to reschedule; see [`pv::Schedule`].
    pub schedule: Schedule,
    /// The CPU the guest is shown.
    pub cpu: CpuModel,
    /// What moves virtual time besides exits and idling.
    pub clock: ClockSource,
    /// Where a guest that computes without exits can be interrupted.
    pub preemption: Preemption,
    /// Whether the machine reserves the extras slot (layout::EXTRAS_START).
    pub extras: Extras,
}

/// Whether KVM logs the pages the guest writes, which keyframes and
/// checkpoints read to list the pages that changed. Logging makes KVM map
/// the guest's RAM in 4 KiB pages, so a run that takes neither keyframes
/// nor checkpoints runs faster without the log, in 2 MiB pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyLog {
    On,
    Off,
}

impl DirtyLog {
    /// The RAM slot's KVM flags.
    fn slot_flags(self) -> u32 {
        match self {
            DirtyLog::On => KVM_MEM_LOG_DIRTY_PAGES,
            DirtyLog::Off => 0,
        }
    }
}

/// Where boot's writes go: the machine's RAM, or memory of their own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootInto {
    Ram,
    Scratch,
}

/// Whether a step adds the per-exit quantum: an exit does, a step the
/// monitor forced at the timer's branch count does not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quantum {
    Add,
    Skip,
}

/// Virtual time per retired conditional branch: about one nanosecond, what
/// current cores average.
pub const PS_PER_BRANCH: u64 = 1000;

/// The guest's work so far, when virtual time follows it.
pub(crate) struct Work {
    counter: pmu::Counter,
    overflow: pmu::Overflow,
    /// Branches counted before the counter was opened, from a keyframe.
    base: u64,
    /// The branch count at the last step.
    pub(crate) total: u64,
    /// The branch count at which the armed timer is due, while the guest
    /// runs toward it.
    target: Option<u64>,
    /// Whether the vCPU is being single-stepped onto the target.
    stepping: bool,
}

/// How far before the target the overflow is armed. The overflow interrupt
/// lands some way past the point it was armed for (the counter's skid);
/// the rest of the way is single-stepped. On a Zen 4 laptop the skid was 9
/// to 73 branches over a hundred preemptions, with one outlier of 1269. A
/// count that ever lands past the target stops the run rather than let it
/// go on unrepeatably.
const PREEMPT_MARGIN: u64 = 256;

impl Work {
    fn open(event: CounterEvent, base: u64) -> Result<Work> {
        Ok(Work {
            counter: pmu::Counter::open(event, pmu::Modes::UserOnly)?,
            overflow: pmu::Overflow::open(event, pmu::Modes::UserOnly)?,
            base,
            total: base,
            target: None,
            stepping: false,
        })
    }

    fn count(&self) -> Result<u64> {
        Ok(self.base + self.counter.read()?)
    }

    /// Branches since the last step.
    fn advance(&mut self) -> Result<u64> {
        let now = self.count()?;
        let delta = now - self.total;
        self.total = now;
        Ok(delta)
    }
}

/// Why a run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The guest stopped the machine itself.
    Guest(GuestExit),
    /// The vCPU triple faulted.
    TripleFault,
    /// The guest went idle with no timer armed: nothing will ever wake it.
    Stalled,
    /// The wall-clock deadline passed first, and where the guest was then.
    /// Unlike the others this depends on the host, not the guest: a faster
    /// one may have finished.
    TimedOut(Stall),
}

/// Where a guest was when its run reached its time limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stall {
    /// The instruction the vCPU was at.
    pub rip: u64,
    /// Whether that was user code or the kernel's.
    pub mode: CpuMode,
    /// How long the guest had gone without an exit: long for a guest
    /// computing without system calls, short for one still making them.
    pub since_exit: std::time::Duration,
}

/// The privilege the vCPU was running at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuMode {
    User,
    Kernel,
}

/// The privilege level of user code, in the code segment's DPL.
const USER_DPL: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The requested step was reached; the machine can run on from here.
    Paused,
    /// An [`Interrupter`] stopped the machine between two instructions,
    /// short of any exit: no step was taken and nothing recorded, and the
    /// guest runs on from exactly there.
    Interrupted,
    Stopped(Stop),
    /// A debugged machine hit a breakpoint or finished a single step
    /// (debug.rs); it can run on from here.
    Debug(debug::DebugStop),
}

/// Receives what the guest reports, with the step it arrived on.
pub trait Observer {
    /// One record written to the event port, header included.
    fn record(&mut self, _step: u64, _record: &[u8]) {}
    /// One byte written to the early serial console.
    fn serial(&mut self, _step: u64, _byte: u8) {}
    /// Whether to stop at the step just taken, short of the step the run
    /// was asked to reach: the observer has seen enough, as a replay that
    /// went another way has. The machine pauses there and can run on.
    fn stop(&self) -> bool {
        false
    }
}

/// An observer that ignores everything.
#[derive(Debug)]
pub struct Ignore;
impl Observer for Ignore {}

/// Where a forked machine's console input comes from: a person at a
/// terminal, typing into a shell inside the VM. A machine with an input
/// waits for it in real time whenever the VM goes idle, up to the next
/// timer, so the VM's clock keeps pace with the person rather than racing
/// ahead to the next timer. Recordings have no input.
pub trait Input: Send {
    /// Bytes to type into the VM, waiting at most `timeout` for them, or
    /// for as long as it takes when None. Empty when none came, and, with
    /// no timeout, when none ever will.
    fn wait(&mut self, timeout: Option<std::time::Duration>) -> Vec<u8>;
}

/// How often, in steps, a busy VM's input is checked without waiting, so
/// that typing reaches a shell that is running something.
const INPUT_POLL_STEPS: u64 = 4096;

/// Device state: everything the monitor keeps besides KVM's own.
pub(crate) struct Devices {
    pub ram: Mapping,
    pub clock: Clock,
    pub schedule: Schedule,
    pub shared: Option<u64>,
    pub epoch: u64,
    pub step: u64,
    pub stop: Option<Stop>,
    /// An inspection request waiting for the VM's APIC to take it.
    pub inspect: bool,
    /// The guest said nothing is runnable with no timer armed, so nothing
    /// wakes it unless an interrupt is sent before it runs again. Decided
    /// on the way back into the guest, since a fork paused at the idle
    /// exit may still get a request.
    pub idle: bool,
    /// The console's input, when a person is typing into the VM.
    pub input: Option<Box<dyn Input>>,
    /// Typed bytes not yet given to the VM.
    pub typed: std::collections::VecDeque<u8>,
    /// Input placed in the shared page whose interrupt the VM's APIC has
    /// not yet taken.
    pub input_sent: bool,
    /// The step of the exit that last armed the timer, since this machine
    /// was booted or restored: a schedule's late timer at a step changes
    /// the run only if that step armed one.
    pub timer_armed_at: Option<u64>,
    /// What turns the guest's read and write records into trace records,
    /// once a guest kernel that buffers them has set up.
    pub io: Option<rewind_trace::io_records::Encoder>,
    /// What turns the guest's switch records into batches in the trace,
    /// once a guest kernel that makes them has set up.
    pub switches: Option<rewind_trace::switches::Encoder>,
    /// Scratch space reused for guest records emitted through the port.
    record_buffer: Vec<u8>,
}

pub struct Machine {
    pub(crate) kvm: Kvm,
    pub(crate) vm: VmFd,
    pub(crate) vcpu: VcpuFd,
    pub(crate) dev: Devices,
    pub(crate) pmem: Option<Mapping>,
    /// The extras slot's memory, when the machine reserves one.
    pub(crate) extras: Option<Mapping>,
    pub(crate) config: Config,
    /// Whether KVM logs the guest's writes; see [`DirtyLog`].
    pub(crate) dirty_log: DirtyLog,
    /// Exits counted by port and guest instruction pointer, when profiling.
    pub profile: Option<std::collections::HashMap<(u16, u64), u64>>,
    /// An experiment: a guest-mode performance counter read at every exit,
    /// and a hash of every value read.
    pub pmu: Option<(pmu::Counter, u64)>,
    /// The guest's work, when virtual time follows it. Opened lazily on the
    /// thread that runs the vCPU, which is the thread a counter counts.
    pub(crate) work: Option<Work>,
    /// The branch count to resume from, set by a restore.
    pub(crate) work_base: u64,
    /// What a debugger has asked for, while one is attached (debug.rs).
    pub(crate) debugging: Option<debug::Debugging>,
    /// int3 the debugger had written into memory (debug.rs).
    pub(crate) int3s: debug::Int3s,
    /// When to stop the machine, however far it has got.
    pub(crate) deadline: Option<std::time::Instant>,
    /// When the guest last exited, while a deadline is watched: how long it
    /// has gone without one says whether it is stuck computing.
    pub(crate) last_exit: std::time::Instant,
    /// Where the machine's memory started: boot, or the keyframes it was
    /// restored from, for rewinding to a checkpoint (checkpoint.rs).
    pub(crate) origin: Origin,
    /// The checkpoints a debugger keeps to rewind to (checkpoint.rs).
    pub(crate) checkpoints: checkpoint::Checkpoints,
    /// The branch count a debugger asked the machine to stop at, and how
    /// close the machine is to it (debug.rs).
    pub(crate) branch_stop: Option<debug::BranchStop>,
    /// Where a single step across the kernel's boundary ends, into it at a
    /// `syscall` or out of it at its return to user space, trapped in a
    /// debug register (debug.rs).
    pub(crate) crossing: Option<debug::Crossing>,
    /// The kernel's exception entry, trapped in a free debug register for
    /// a single step whose instruction may fault or meet an interrupt
    /// (debug.rs).
    pub(crate) exception_trap: Option<debug::Crossing>,
    /// The pc the last single step began at (debug.rs).
    pub(crate) stepped_from: u64,
    /// The stub instructions the last single step ran unstepped
    /// (debug.rs).
    pub(crate) skipped: Vec<u64>,
    /// What an [`Interrupter`] of this machine shares with it
    /// (interrupt.rs).
    pub(crate) interrupt: std::sync::Arc<interrupt::Shared>,
}

impl Drop for Machine {
    /// Forgets the vCPU's kvm_run page before the vCPU, and the page with
    /// it, goes, so an interrupter that outlives the machine writes
    /// nothing there, and the machine among the process's.
    fn drop(&mut self) {
        self.interrupt.forget();
    }
}

/// Where a machine's memory started.
pub(crate) enum Origin {
    /// Boot wrote the kernel, its command line and page tables into it.
    Boot,
    /// Keyframes held its pages: each non-zero page's number and the hash
    /// of its contents, in page order.
    Restored(Vec<(u32, [u8; 32])>),
}

/// The machine's configuration and how far it has run; its devices and
/// memory are the guest's whole state.
impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("config", &self.config)
            .field("step", &self.dev.step)
            .finish_non_exhaustive()
    }
}

impl Machine {
    /// Creates the VM with its memory and devices, but no guest state:
    /// either [`Machine::boot`] or a snapshot restore supplies that.
    fn create(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        if config.mem_bytes > RAM_MAX || config.mem_bytes < (64 << 20) {
            bail!("VM memory must be between 64 MiB and {} MiB", RAM_MAX >> 20);
        }

        let kvm = kvm::open()?;
        let vm = kvm.create_vm().context("creating the VM")?;
        vm.set_tss_address(TSS_ADDRESS)?;
        vm.create_irq_chip()
            .context("creating the in-kernel interrupt controller")?;
        kvm::turn_off_counters(&vm)?;

        let ram = Mapping::anonymous(config.mem_bytes as usize)?;
        // Huge pages make restores faster where the host has them; RAM
        // works the same without, so a refusal is no error.
        let _advice = ram.prefer_huge_pages();
        // SAFETY: the mapping outlives the VM (both live in Machine, and the
        // VM is dropped first), and nothing else maps this range.
        unsafe {
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: SLOT_RAM,
                flags: dirty_log.slot_flags(),
                guest_phys_addr: 0,
                memory_size: ram.len() as u64,
                userspace_addr: ram.host_addr(),
            })?;
        }

        let pmem = match &config.image {
            Some(path) => {
                let image = Mapping::file(path, PMEM_ALIGN)?;
                // SAFETY: as above; the slot is read-only, so guest writes
                // arrive as MMIO exits instead of reaching the file.
                unsafe {
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        slot: SLOT_PMEM,
                        flags: KVM_MEM_READONLY,
                        guest_phys_addr: PMEM_START,
                        memory_size: image.len() as u64,
                        userspace_addr: image.host_addr(),
                    })?;
                }
                Some(image)
            }
            None => None,
        };

        // The extras slot, empty: anonymous memory reads as zeros and costs
        // nothing until a fork maps an image over it.
        let extras = match config.extras {
            Extras::Absent => None,
            Extras::Reserved => {
                // A run recorded where the CPU reaches the slot, replayed
                // where the CPU does not: name the address bits, where KVM
                // would fail with a bare EINVAL.
                recorded_slot_fits(cpu::address_bits(&kvm)?)?;
                let slot = Mapping::anonymous(EXTRAS_LEN as usize)?;
                // SAFETY: the mapping outlives the VM; the slot is read-only.
                unsafe {
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        slot: SLOT_EXTRAS,
                        flags: KVM_MEM_READONLY,
                        guest_phys_addr: EXTRAS_START,
                        memory_size: slot.len() as u64,
                        userspace_addr: slot.host_addr(),
                    })?;
                }
                Some(slot)
            }
        };

        let mut vcpu = vm.create_vcpu(0).context("creating the vCPU")?;
        vcpu.set_cpuid2(&cpu::cpuid(&kvm, config.cpu)?)?;
        let immediate_exit = std::ptr::addr_of_mut!(vcpu.get_kvm_run().immediate_exit);

        Ok(Machine {
            kvm,
            vm,
            vcpu,
            dev: Devices {
                ram,
                clock: Clock::new(config.quantum),
                schedule: config.schedule.clone(),
                shared: None,
                epoch: config.epoch,
                step: 0,
                stop: None,
                inspect: false,
                idle: false,
                input: None,
                typed: std::collections::VecDeque::new(),
                input_sent: false,
                timer_armed_at: None,
                io: None,
                switches: None,
                record_buffer: Vec::new(),
            },
            pmem,
            extras,
            config: config.clone(),
            dirty_log,
            profile: None,
            pmu: None,
            work: None,
            work_base: 0,
            debugging: None,
            int3s: debug::Int3s::default(),
            deadline: None,
            last_exit: std::time::Instant::now(),
            origin: Origin::Boot,
            checkpoints: checkpoint::Checkpoints::default(),
            branch_stop: None,
            crossing: None,
            exception_trap: None,
            stepped_from: 0,
            skipped: Vec::new(),
            interrupt: interrupt::Shared::new(immediate_exit),
        })
    }

    /// A machine at step 0: the kernel loaded, the vCPU at its entry point.
    /// A machine that takes neither keyframes nor checkpoints boots with
    /// `DirtyLog::Off`; see [`DirtyLog`].
    pub fn boot(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        let mut m = Self::create(config, dirty_log)?;
        let entry = m.write_boot(BootInto::Ram)?;

        cpu::setup_msrs(&m.vcpu)?;
        cpu::setup_sregs(&m.vcpu)?;
        cpu::setup_regs(&m.vcpu, entry)?;
        cpu::setup_fpu(&m.vcpu)?;
        cpu::setup_lapic(&m.vcpu)?;
        Ok(m)
    }

    /// Writes what boot puts in memory, the kernel, its command line and
    /// page tables, into the machine's RAM, or into memory of its own that
    /// a rewind reads the boot's pages from, which it returns. Returns the
    /// kernel's entry point.
    pub(crate) fn write_boot(&mut self, into: BootInto) -> Result<u64> {
        let kernel = std::fs::read(&self.config.kernel)
            .with_context(|| format!("reading {}", self.config.kernel.display()))?;
        let spec = boot::BootSpec {
            kernel: &kernel,
            initrd: &self.config.initrd,
            cmdline: &self.config.cmdline,
            seed: self.config.seed,
            pmem_len: self.pmem.as_ref().map_or(0, |p| p.len() as u64),
            extras_len: self.extras.as_ref().map_or(0, |e| e.len() as u64),
        };
        let write = |ram: &mut Mapping| -> Result<u64> {
            let entry = boot::load(ram, &spec)?;
            boot::write_tables(ram)?;
            Ok(entry)
        };
        match into {
            BootInto::Ram => write(&mut self.dev.ram),
            BootInto::Scratch => {
                let mut ram = Mapping::anonymous(self.dev.ram.len())?;
                let entry = write(&mut ram)?;
                self.checkpoints.booted = Some(ram);
                Ok(entry)
            }
        }
    }

    /// The number of exits so far.
    pub fn step(&self) -> u64 {
        self.dev.step
    }

    /// The step of the exit that last armed the timer since this machine
    /// was booted or restored, which only the monitor sees.
    pub fn timer_armed_at(&self) -> Option<u64> {
        self.dev.timer_armed_at
    }

    /// Guest branches counted so far, when virtual time follows them.
    pub fn branches(&self) -> u64 {
        self.work.as_ref().map_or(self.work_base, |w| w.total)
    }

    /// Lets go of the counters the thread that ran the machine opened,
    /// which count that thread's guest alone, so the machine can run on
    /// another thread, where its next run opens counters of its own with
    /// the branches counted so far as their base. Between runs only, at
    /// a step's start, where the count is settled.
    pub fn release_counters(&mut self) {
        self.work_base = self.branches();
        self.work = None;
    }

    /// Virtual nanoseconds since boot.
    pub fn now(&self) -> u64 {
        self.dev.clock.now
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The number of pages in the VM's allocated RAM.
    pub fn ram_pages(&self) -> usize {
        self.dev.ram.len() / PAGE_SIZE
    }

    /// Stops the machine at `deadline` with [`Stop::TimedOut`], wherever
    /// the guest is then, even computing without exits. The thread that
    /// calls [`Machine::run`] is the one stopped.
    pub fn set_deadline(&mut self, deadline: Option<std::time::Instant>) {
        self.deadline = deadline;
    }

    /// Writes the switches still pending as a batch at the machine's
    /// step, as a machine that stops there writes them. For a replay
    /// paused at the step where the host's time limit stopped its
    /// recording: the replay's guest has not stopped, and the recording's
    /// trace ends with that batch. Nothing when no switch is pending.
    pub fn finish_switches(&mut self, obs: &mut dyn Observer) {
        self.dev.finish_switches(obs);
    }

    /// Where the vCPU is now, and how long since the guest last exited.
    fn stall(&self) -> Result<Stall> {
        let rip = self.vcpu.get_regs()?.rip;
        let mode = if self.vcpu.get_sregs()?.cs.dpl == USER_DPL {
            CpuMode::User
        } else {
            CpuMode::Kernel
        };
        Ok(Stall {
            rip,
            mode,
            since_exit: self.last_exit.elapsed(),
        })
    }

    /// Runs until the guest stops, or until step `until` if given.
    ///
    /// The machine's counters count only in here. A counter counts its
    /// thread's guest, whichever machine's that is, and a thread can run
    /// another machine between two calls, as `rewind gdb` runs the forks
    /// it inspects the process with before the fork it debugs goes on.
    pub fn run(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
        self.resume_counting()?;
        self.interrupt.entered();
        let outcome = self.run_counted(until, obs);
        self.interrupt.left();
        let paused = self.pause_counting();
        let outcome = outcome?;
        paused?;
        Ok(outcome)
    }

    /// A way for another thread to stop this machine's run between two
    /// instructions: `run` then returns [`Outcome::Interrupted`].
    pub fn interrupter(&self) -> Interrupter {
        Interrupter(self.interrupt.clone())
    }

    /// Counts again, and aims again at the timer the guest was running
    /// toward when the machine last stopped: the overflow a margin short
    /// of its branch count, or single steps when it is closer than that.
    fn resume_counting(&mut self) -> Result<()> {
        if let Some((counter, _)) = &self.pmu {
            counter.resume()?;
        }
        let Some(work) = &mut self.work else {
            return Ok(());
        };
        work.counter.resume()?;
        let (Some(target), false) = (work.target, work.stepping) else {
            return Ok(());
        };
        let left = target.saturating_sub(work.count()?);
        if left > PREEMPT_MARGIN {
            work.overflow.arm(left - PREEMPT_MARGIN)?;
            return Ok(());
        }
        self.start_stepping()
    }

    /// Stops the counters until the machine runs again, the overflow
    /// with them, so neither counts another machine's guest.
    fn pause_counting(&mut self) -> Result<()> {
        if let Some((counter, _)) = &self.pmu {
            counter.pause()?;
        }
        if let Some(work) = &mut self.work {
            work.counter.pause()?;
            work.overflow.disarm()?;
        }
        self.branch_interrupt_disarmed();
        Ok(())
    }

    fn run_counted(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
        self.skipped.clear();
        if let Some(stop) = self.dev.stop {
            return Ok(Outcome::Stopped(stop));
        }
        if until.is_some_and(|u| u <= self.dev.step) {
            return Ok(Outcome::Paused);
        }

        // A deadline is watched only while this thread is in here, so no
        // signal reaches it once it has left.
        let watchdog = self.deadline.map(watchdog::Watchdog::start);
        let expired = watchdog.as_ref().map(|w| w.expired());
        self.last_exit = std::time::Instant::now();

        if let (ClockSource::Branches(event), None) = (self.config.clock, &self.work) {
            self.work = Some(Work::open(event, self.work_base)?);
        }

        loop {
            if expired
                .as_ref()
                .is_some_and(|e| e.load(std::sync::atomic::Ordering::Relaxed))
            {
                let stop = Stop::TimedOut(self.stall()?);
                self.dev.stop = Some(stop);
                self.dev.end_trace(stop, obs)?;
                return Ok(Outcome::Stopped(stop));
            }
            if let Some(stop) = self.dev.stall_if_idle(obs)? {
                return Ok(Outcome::Stopped(stop));
            }
            if self.work.is_some() && self.config.preemption == Preemption::AtBranchCounts {
                self.aim_preemption()?;
            }
            if let Some(stop) = self.close_in_on_branches()? {
                return Ok(Outcome::Debug(stop));
            }
            // An interruption asked for before the guest runs, or while
            // the machine was between runs.
            if self.interrupt.take() {
                return Ok(Outcome::Interrupted);
            }
            self.step_across_the_boundary()?;

            // The port of a counted exit; MMIO exits count under MMIO_PORT.
            const MMIO_PORT: u16 = 0xffff;
            let mut debug_stop = None;
            let counted = match self.vcpu.run() {
                // Each step takes the read and write records the guest
                // buffered before it, ahead of what the step itself
                // records.
                Ok(exit) => match exit {
                    VcpuExit::IoOut(port, data) => {
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        self.dev.io_out(port, data, obs)?;
                        Some(port)
                    }
                    VcpuExit::IoIn(port, data) => {
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        self.dev.io_in(port, data)?;
                        Some(port)
                    }
                    VcpuExit::MmioRead(_, data) => {
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        data.fill(0xff);
                        Some(MMIO_PORT)
                    }
                    VcpuExit::MmioWrite(..) => {
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        Some(MMIO_PORT)
                    }
                    VcpuExit::Shutdown => {
                        self.dev.stop = Some(Stop::TripleFault);
                        None
                    }
                    // A signal to the monitor thread, a counter overflow
                    // among them, or a debug exit: not the guest's doing,
                    // so not a step of its own. A debugger's trap ends the
                    // run for the debugger once the timer has had its say.
                    VcpuExit::Debug(arch) => {
                        debug_stop = self.debug_exit(arch.exception, arch.pc, arch.dr6)?;
                        None
                    }
                    VcpuExit::Intr => None,
                    VcpuExit::Hlt => bail!("the VM executed HLT; is its kernel built for Rewind?"),
                    other => bail!("unexpected exit at step {}: {other:?}", self.dev.step),
                },
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => None,
                Err(e) => return Err(e).context("KVM_RUN"),
            };

            // A machine that stopped ends its trace here, whether a step
            // stopped it, as an exit or a stall does, or no step did, as a
            // triple fault.
            if let Some(stop) = self.dev.stop {
                self.dev.end_trace(stop, obs)?;
                return Ok(Outcome::Stopped(stop));
            }
            if counted.is_some() && self.deadline.is_some() {
                self.last_exit = std::time::Instant::now();
            }
            let Some(port) = counted else {
                // Close in on the timer's branch count; reaching it is a
                // step, the one where the timer fires. A debugger's stop
                // waits for that, so the count is not passed by while the
                // debugger has the machine.
                let mut preempted = false;
                if self.work.is_some()
                    && self.config.preemption == Preemption::AtBranchCounts
                    && self.reached_preemption()?
                {
                    self.dev.step += 1;
                    self.dev.drain_io(obs)?;
                    self.after_step(Quantum::Skip)?;
                    preempted = true;
                }
                if let Some(stop) = debug_stop {
                    return Ok(Outcome::Debug(stop));
                }
                if preempted && until.is_some_and(|u| self.dev.step >= u) {
                    return Ok(Outcome::Paused);
                }
                // A signal pulled the vCPU out of the guest: an
                // interrupter's, which stops the run here, between two
                // instructions, or the watchdog's or the counter's, which
                // the next round of the loop looks at.
                if self.interrupt.take() {
                    return Ok(Outcome::Interrupted);
                }
                continue;
            };
            if let Some(profile) = &mut self.profile {
                // Keyed by the caller, read off the top of the guest stack,
                // for exits made from a small leaf like the clock read.
                let regs = self.vcpu.get_regs()?;
                let mut at = regs.rip;
                if let Ok(t) = self.vcpu.translate_gva(regs.rsp) {
                    let mut ret = [0u8; 8];
                    if t.valid != 0 && self.dev.ram.read(t.physical_address, &mut ret).is_ok() {
                        at = u64::from_le_bytes(ret);
                    }
                }
                *profile.entry((port, at)).or_default() += 1;
            }
            if let Some((counter, hash)) = &mut self.pmu {
                let count = counter.read()?;
                *hash = (*hash ^ count).wrapping_mul(0x100_0000_01b3);
            }
            self.after_step(Quantum::Add)?;

            if until.is_some_and(|u| self.dev.step >= u) || obs.stop() {
                return Ok(Outcome::Paused);
            }
        }
    }

    /// Moves time for the step just taken, and sends the interrupt if the
    /// timer is due or the schedule asks for a reschedule here.
    fn after_step(&mut self, quantum: Quantum) -> Result<()> {
        if quantum == Quantum::Add {
            self.dev.clock.tick();
        }
        if let Some(work) = &mut self.work {
            self.dev.clock.now += work.advance()? * PS_PER_BRANCH / 1000;
            // Whatever the guest was stepping toward, this step settles it,
            // and the debugger's traps stay as they were.
            let stepped = std::mem::take(&mut work.stepping);
            work.target = None;
            work.overflow.disarm()?;
            self.branch_interrupt_disarmed();
            if stepped {
                self.apply_guest_debug()?;
            }
        }
        // The guest's scheduler clock reads this without an exit, so it is
        // refreshed at every step.
        if let Some(shared) = self.dev.shared {
            let now = self.dev.clock.now.to_le_bytes();
            self.dev.ram.write(shared + pv::SHARED_NOW, &now)?;
        }
        let mut reasons = 0;
        if self.dev.clock.due() {
            reasons |= pv::PENDING_TIMER;
        }
        if self.dev.schedule.preempt_at(self.dev.step) {
            reasons |= pv::PENDING_PREEMPT;
        }
        if let (Some(ns), Some(shared)) =
            (self.dev.schedule.stall_at(self.dev.step), self.dev.shared)
        {
            self.dev
                .ram
                .write(shared + pv::SHARED_STALL_NS, &(ns as u32).to_le_bytes())?;
            reasons |= pv::PENDING_STALL;
        }
        if let (Some(seed), Some(shared)) =
            (self.dev.schedule.reseed_at(self.dev.step), self.dev.shared)
        {
            let bytes = Schedule::reseed_bytes(seed, self.dev.step);
            self.dev.ram.write(shared + pv::SHARED_RESEED, &bytes)?;
            reasons |= pv::PENDING_RESEED;
        }
        if self.dev.inspect {
            reasons |= pv::PENDING_INSPECT;
        }
        if self.dev.input.is_some() && self.deliver_input()? {
            reasons |= pv::PENDING_INPUT;
        }
        if reasons != 0 && self.inject(reasons)? {
            // A timer or a request counts as delivered once the APIC takes
            // it; until then it stays pending and goes again at the next
            // exit.
            if reasons & pv::PENDING_TIMER != 0 {
                self.dev.clock.deadline = None;
            }
            self.dev.inspect = false;
            self.dev.input_sent = false;
        }
        Ok(())
    }

    /// Maps `image` into the extras slot, for a fork: the VM sees it as the
    /// slot's persistent memory from the next access on. The host mapping
    /// is replaced in place, and KVM follows host mappings, so the slot
    /// itself does not change.
    pub fn attach_extras(&mut self, image: &std::path::Path) -> Result<()> {
        let slot = self.extras.as_ref().context(
            "this run was recorded without the extras slot; record it again to use --with",
        )?;
        let file =
            std::fs::File::open(image).with_context(|| format!("opening {}", image.display()))?;
        let len = file.metadata()?.len() as usize;
        if len == 0 || len > slot.len() {
            bail!(
                "{} is {len} bytes; the extras slot holds 1 to {} bytes",
                image.display(),
                slot.len()
            );
        }
        // SAFETY: the target range lies inside the slot's own mapping, which
        // this machine owns; MAP_FIXED replaces those pages with the file's.
        let mapped = unsafe {
            libc::mmap(
                slot.host_addr() as *mut libc::c_void,
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                std::os::fd::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            bail!(
                "mapping {}: {}",
                image.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// Gives the VM console input from now on, typed through `input`.
    pub fn set_input(&mut self, input: Box<dyn Input>) {
        self.dev.input = Some(input);
    }

    /// Moves typed bytes into the shared page when the VM has taken the
    /// last ones, checking for new typing every so often while the VM is
    /// busy. True when there is input whose interrupt is still to send.
    fn deliver_input(&mut self) -> Result<bool> {
        if self.dev.step.is_multiple_of(INPUT_POLL_STEPS)
            && let Some(input) = &mut self.dev.input
        {
            let bytes = input.wait(Some(std::time::Duration::ZERO));
            self.dev.typed.extend(bytes);
        }
        if self.dev.input_sent {
            return Ok(true);
        }
        let Some(shared) = self.dev.shared else {
            return Ok(false);
        };
        if self.dev.typed.is_empty() {
            return Ok(false);
        }

        // The kernel zeroes the length when it has taken the last bytes.
        let mut len = [0u8; 4];
        self.dev.ram.read(shared + pv::SHARED_INPUT_LEN, &mut len)?;
        if u32::from_le_bytes(len) != 0 {
            return Ok(false);
        }
        let n = self.dev.typed.len().min(pv::INPUT_MAX);
        let chunk: Vec<u8> = self.dev.typed.drain(..n).collect();
        self.dev.ram.write(shared + pv::SHARED_INPUT, &chunk)?;
        self.dev
            .ram
            .write(shared + pv::SHARED_INPUT_LEN, &(n as u32).to_le_bytes())?;
        self.dev.input_sent = true;
        Ok(true)
    }

    /// Asks the VM to start an inspection: `/init --inspect` with `args`,
    /// whose answer arrives as records like any process's output. The
    /// interrupt goes out now, so the VM takes it on resuming from this
    /// step, in the task that made this step's exit: the kernel names that
    /// task as the one running, where the next exit may be another task's
    /// or the idle task's. An interrupt the APIC does not accept goes again
    /// at the next exit. Only for machines forked from a run, since the
    /// request is not part of any recording.
    pub fn request_inspection(&mut self, args: &[String]) -> Result<()> {
        let shared = self
            .dev
            .shared
            .context("the VM has not finished booting at this step")?;
        let request = encode_request(args)?;
        self.dev.ram.write(
            shared + pv::SHARED_REQUEST_LEN,
            &(request.len() as u32).to_le_bytes(),
        )?;
        self.dev.ram.write(shared + pv::SHARED_REQUEST, &request)?;
        self.dev.inspect = !self.inject(pv::PENDING_INSPECT)?;
        Ok(())
    }

    /// Before entering the guest: the branch count where the armed timer
    /// falls due, and the overflow set a margin short of it, so a guest
    /// computing without exits is still interrupted there.
    fn aim_preemption(&mut self) -> Result<()> {
        let Some(work) = self.work.as_mut() else {
            return Ok(());
        };
        if work.target.is_some() {
            return Ok(());
        }
        let Some(deadline) = self.dev.clock.deadline else {
            return Ok(());
        };
        if deadline <= self.dev.clock.now {
            return Ok(());
        }
        let branches = (deadline - self.dev.clock.now) * 1000 / PS_PER_BRANCH;
        let target = work.total + branches.max(1);
        work.target = Some(target);
        if branches > PREEMPT_MARGIN {
            work.overflow.arm(branches - PREEMPT_MARGIN)?;
        } else {
            self.start_stepping()?;
        }
        Ok(())
    }

    /// After an interruption or a single step: true once the count is
    /// exactly on the target; single-stepping once it is within the margin,
    /// and the overflow armed again short of it before that, as an
    /// interruption may come early: on a hybrid host, when the thread moved
    /// to another kind of core.
    fn reached_preemption(&mut self) -> Result<bool> {
        let Some(work) = self.work.as_mut() else {
            return Ok(false);
        };
        let Some(target) = work.target else {
            return Ok(false);
        };
        let count = work.count()?;
        if count > target {
            bail!(
                "the branch counter ran {} past a preemption point at step {}; \
                 its skid exceeds the margin of {PREEMPT_MARGIN}",
                count - target,
                self.dev.step
            );
        }
        if count == target {
            return Ok(true);
        }
        if !work.stepping {
            let left = target - count;
            if left > PREEMPT_MARGIN {
                work.overflow.arm(left - PREEMPT_MARGIN)?;
            } else {
                self.start_stepping()?;
            }
        }
        Ok(false)
    }

    /// Single-steps the guest, interrupts held, alongside whatever a
    /// debugger traps.
    fn start_stepping(&mut self) -> Result<()> {
        if let Some(work) = self.work.as_mut() {
            work.stepping = true;
        }
        self.apply_guest_debug()
    }

    /// Records why in the shared page and sends the monitor's interrupt as
    /// an MSI to the one local APIC. False when the guest has no shared page
    /// yet or its APIC is not accepting interrupts.
    fn inject(&mut self, reasons: u32) -> Result<bool> {
        let Some(shared) = self.dev.shared else {
            return Ok(false);
        };
        let at = shared + pv::SHARED_PENDING;
        let mut pending = [0u8; 4];
        self.dev.ram.read(at, &mut pending)?;
        let pending = u32::from_le_bytes(pending) | reasons;
        self.dev.ram.write(at, &pending.to_le_bytes())?;

        let msi = kvm_msi {
            address_lo: APIC_BASE as u32,
            address_hi: 0,
            data: CALLBACK_VECTOR,
            ..Default::default()
        };
        let taken = self.vm.signal_msi(msi)? > 0;
        if taken {
            // An interrupt the APIC holds wakes a guest that said it was
            // idle: it is taken right after the idle exit's port write.
            self.dev.idle = false;
        }
        Ok(taken)
    }
}

impl Devices {
    /// Stops the machine as stalled when the guest's last exit said
    /// nothing is runnable with no timer armed and nothing has been sent
    /// to wake it since: a request still waiting for the APIC goes again
    /// at the exit the guest makes, and one the APIC took is taken right
    /// after that exit. None when the guest can run on.
    fn stall_if_idle(&mut self, obs: &mut dyn Observer) -> Result<Option<Stop>> {
        if !self.idle {
            return Ok(None);
        }
        self.idle = false;
        if self.inspect {
            return Ok(None);
        }
        let stop = Stop::Stalled;
        self.stop = Some(stop);
        self.end_trace(stop, obs)?;
        Ok(Some(stop))
    }

    fn io_out(&mut self, port: u16, data: &[u8], obs: &mut dyn Observer) -> Result<()> {
        let value = port_value(data);
        match port {
            pv::PORT_EMIT => {
                let mut len = [0u8; 4];
                self.ram.read(value as u64, &mut len)?;
                let len = u32::from_le_bytes(len) as usize;
                if !(pv::RECORD_HEADER..=pv::RECORD_MAX).contains(&len) {
                    bail!("VM record at step {} has length {len}", self.step);
                }
                self.record_buffer.resize(len, 0);
                self.ram.read(value as u64, &mut self.record_buffer)?;
                let record = &self.record_buffer;
                // A switch joins the pending batch, which is written first
                // when it had no room for the switch.
                if rewind_trace::switches::is_switch(record)
                    && let Some(encoder) = self.switches.as_mut()
                {
                    if let Some(batch) = encoder.add(self.step, record)? {
                        obs.record(self.step, &batch);
                    }
                    return Ok(());
                }
                obs.record(self.step, record);
            }
            pv::PORT_TIMER => {
                let delta = value as u64;
                let slack = if delta == 0 {
                    0
                } else {
                    self.timer_armed_at = Some(self.step);
                    self.schedule.slack_at(self.step)
                };
                self.clock.arm(delta + slack);
            }
            pv::PORT_IDLE => {
                // With a person typing, idle time passes in real time: wait
                // for input until the next timer is due, and only jump to
                // the timer when none came.
                if let Some(input) = &mut self.input {
                    if !self.typed.is_empty() || self.input_sent {
                        return Ok(());
                    }
                    let timeout = self.clock.deadline.map(|deadline| {
                        std::time::Duration::from_nanos(deadline.saturating_sub(self.clock.now))
                    });
                    let bytes = input.wait(timeout);
                    if !bytes.is_empty() {
                        self.typed.extend(bytes);
                        return Ok(());
                    }
                }
                // A pending request wakes the VM like a timer would.
                // With neither, the guest is idle for good unless an
                // interrupt is sent before it runs again (stall_if_idle).
                if !self.clock.idle() && !self.inspect {
                    self.idle = true;
                }
            }
            pv::PORT_EXIT => {
                self.stop = Some(Stop::Guest(GuestExit::from_port(value)));
            }
            pv::PORT_SETUP => {
                let shared = value as u64;
                let records = pv::check_interface(self.guest_interface(shared)?)?;
                self.ram
                    .write(shared + pv::SHARED_EPOCH, &self.epoch.to_le_bytes())?;
                self.shared = Some(shared);
                self.io = records.io.then(rewind_trace::io_records::Encoder::new);
                self.switches = records.switches.then(rewind_trace::switches::Encoder::new);
            }
            pv::PORT_COM1 => obs.serial(self.step, data[0]),
            _ => {}
        }
        Ok(())
    }

    /// Writes the switches pending from an earlier window of steps as a
    /// batch at this step, ahead of anything else the step records.
    fn flush_switches(&mut self, obs: &mut dyn Observer) {
        if let Some(encoder) = self.switches.as_mut()
            && let Some(batch) = encoder.at_step(self.step)
        {
            obs.record(self.step, &batch);
        }
    }

    /// Writes the switches still pending at the run's end as a batch at
    /// this step.
    fn finish_switches(&mut self, obs: &mut dyn Observer) {
        if let Some(encoder) = self.switches.as_mut()
            && let Some(batch) = encoder.finish()
        {
            obs.record(self.step, &batch);
        }
    }

    /// Ends the trace of a machine that stopped as `stop`, whichever way
    /// a machine stops. The trace holds what the inputs decide up to the
    /// last step, so a replay that pauses at that step and finishes its
    /// switches ([`Machine::finish_switches`]) makes the same trace.
    fn end_trace(&mut self, stop: Stop, obs: &mut dyn Observer) -> Result<()> {
        // A guest that stopped by itself did so at the same instruction in
        // every execution, so the trace takes what the guest buffered since
        // the last step. The time limit lands wherever the host's signal
        // finds the guest between two exits. How many records the guest
        // had buffered by then is the host's doing, so the trace leaves
        // them out.
        if !matches!(stop, Stop::TimedOut(_)) {
            self.drain_io(obs)?;
        }

        // Every pending switch was a step at or before the last one,
        // whatever stopped the machine.
        self.finish_switches(obs);
        Ok(())
    }

    /// Takes the read and write records the guest buffered since the step
    /// before, with how many it had no room for, into the trace at this
    /// step, and sets both back to 0 for the guest to go on from, after
    /// the switches an earlier window left pending. Nothing for a guest
    /// kernel that buffers none.
    fn drain_io(&mut self, obs: &mut dyn Observer) -> Result<()> {
        self.flush_switches(obs);
        let (Some(shared), Some(encoder)) = (self.shared, self.io.as_mut()) else {
            return Ok(());
        };
        let read_u32 = |ram: &Mapping, at: u64| -> Result<u32> {
            let mut bytes = [0u8; 4];
            ram.read(shared + at, &mut bytes)?;
            Ok(u32::from_le_bytes(bytes))
        };
        let head = read_u32(&self.ram, pv::SHARED_IO_HEAD)?;
        let lost = read_u32(&self.ram, pv::SHARED_IO_LOST)?;
        if head == 0 && lost == 0 {
            return Ok(());
        }

        // The buffer, up to its fill, which the guest never sets past its
        // size.
        let size = read_u32(&self.ram, pv::SHARED_IO_SIZE)?;
        let mut at = [0u8; 8];
        self.ram.read(shared + pv::SHARED_IO_BUF, &mut at)?;
        let len = head.min(size) as usize;
        let mut buffered = vec![0u8; len.min(pv::IO_BUF_MAX)];
        self.ram.read(u64::from_le_bytes(at), &mut buffered)?;
        self.ram
            .write(shared + pv::SHARED_IO_HEAD, &0u32.to_le_bytes())?;
        self.ram
            .write(shared + pv::SHARED_IO_LOST, &0u32.to_le_bytes())?;

        for record in encoder.encode(&buffered, lost) {
            obs.record(self.step, &record);
        }
        Ok(())
    }

    /// The interface the guest kernel named in its shared page at
    /// `shared`.
    pub(crate) fn guest_interface(&self, shared: u64) -> Result<u32> {
        let mut version = [0u8; 4];
        self.ram.read(shared + pv::SHARED_INTERFACE, &mut version)?;
        Ok(u32::from_le_bytes(version))
    }

    fn io_in(&mut self, port: u16, data: &mut [u8]) -> Result<()> {
        match port {
            pv::PORT_CLOCK => {
                let shared = self.shared.context("the VM read the clock before setup")?;
                let now = self.clock.now;
                self.ram
                    .write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;

                // A string read (`rep insl`) asks for several reads in one
                // exit, and each gets the clock's low 32 bits.
                let bytes = (now as u32).to_le_bytes();
                for (i, b) in data.iter_mut().enumerate() {
                    *b = bytes[i % bytes.len()];
                }
            }
            pv::PORT_COM1_LSR => data.fill(pv::LSR_IDLE),
            _ => data.fill(0xff),
        }
        Ok(())
    }
}

/// An inspection request as the shared page holds it: each of `args` and
/// a NUL. Refused when the guest kernel would not pass all of it on: more
/// bytes than it reads, or more arguments than it hands init, the rest of
/// which it drops without a word.
fn encode_request(args: &[String]) -> Result<Vec<u8>> {
    if args.len() > pv::REQUEST_ARGS_MAX {
        bail!(
            "the inspection request has {} arguments; the guest kernel passes on {}",
            args.len(),
            pv::REQUEST_ARGS_MAX
        );
    }
    let mut request = Vec::new();
    for arg in args {
        if arg.contains('\0') {
            bail!("an inspection argument contains NUL: {arg:?}");
        }
        request.extend_from_slice(arg.as_bytes());
        request.push(0);
    }
    if request.len() > pv::REQUEST_MAX {
        bail!(
            "the inspection request is {} bytes; the most is {}",
            request.len(),
            pv::REQUEST_MAX
        );
    }
    Ok(request)
}

/// A port write's value, zero-extended from however many bytes it wrote.
fn port_value(data: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    bytes[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
    u32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    // Port reads the guest makes, answered by a machine's devices built
    // without KVM: one page of RAM holding the shared page at 0.
    use super::*;

    const QUANTUM: u64 = 5000;
    const PAGE: usize = 4096;

    #[test]
    fn repeated_switch_exits_do_not_allocate() {
        // Warm the encoder and guest buffer, then emit the same task without a batch flush.
        const SWITCH_KIND: u16 = 14;
        const TASK_PID: u32 = 40;
        const TASK_TID: u32 = 41;
        const TASK_NAME: &[u8] = b"worker\0";
        const WARM_STEPS: u64 = 32;
        const MEASURED_STEPS: u64 = 8;
        let mut dev = devices(0);
        dev.switches = Some(rewind_trace::switches::Encoder::new());
        let mut record = Vec::new();
        record.extend_from_slice(&((pv::RECORD_HEADER + TASK_NAME.len()) as u32).to_le_bytes());
        record.extend_from_slice(&SWITCH_KIND.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&TASK_PID.to_le_bytes());
        record.extend_from_slice(&TASK_TID.to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(TASK_NAME);
        dev.ram.write(IO_BUF, &record).unwrap();
        let pointer = (IO_BUF as u32).to_le_bytes();
        let mut kept = Kept::default();
        for step in 1..=WARM_STEPS {
            dev.step = step;
            dev.io_out(pv::PORT_EMIT, &pointer, &mut kept).unwrap();
        }

        let allocations = test_alloc::count(|| {
            for step in WARM_STEPS + 1..=WARM_STEPS + MEASURED_STEPS {
                dev.step = step;
                dev.io_out(pv::PORT_EMIT, &pointer, &mut kept).unwrap();
            }
        });
        assert_eq!(allocations, 0);
        assert!(kept.0.is_empty());
    }

    #[test]
    fn the_extras_slot_is_reserved_only_where_the_host_reaches_it() {
        // Address widths on both sides of the slot's 39 bits: 36 is an Ivy
        // Bridge's. A narrow host records the host model without the slot
        // and refuses both fixed models, which show the guest 39 bits.
        assert_eq!(EXTRAS_ADDRESS_BITS, 39);
        assert_eq!(extras_for_bits(CpuModel::V3, 39).unwrap(), Extras::Reserved);
        assert_eq!(
            extras_for_bits(CpuModel::V3Compat, 39).unwrap(),
            Extras::Reserved
        );
        assert!(extras_for_bits(CpuModel::V3Compat, 36).is_err());
        assert_eq!(
            extras_for_bits(CpuModel::Host, 48).unwrap(),
            Extras::Reserved
        );
        assert_eq!(extras_for_bits(CpuModel::Host, 36).unwrap(), Extras::Absent);
        let refused = extras_for_bits(CpuModel::V3, 36).unwrap_err().to_string();
        assert!(
            refused.contains("36") && refused.contains("--cpu host"),
            "{refused}"
        );
    }

    #[test]
    fn a_recorded_slot_past_the_host_says_where_it_replays() {
        // A recorded run that reserves the extras slot, rebuilt on hosts on
        // both sides of the slot's 39 bits: the narrower one refuses, says
        // to replay where the bits are, and offers recording again only as
        // a different run.
        recorded_slot_fits(EXTRAS_ADDRESS_BITS).unwrap();
        let refused = recorded_slot_fits(36).unwrap_err().to_string();
        assert_eq!(
            refused,
            "this run reserves the extras slot, which needs 39 physical address bits, and this \
             CPU reaches 36; replay it on a host whose CPU reaches 39. Recording it again here \
             with --cpu host makes a different run, without the slot"
        );
    }

    /// The devices of a machine that has set up its shared page at 0,
    /// with a page after it for the guest's record buffer.
    fn devices(now: u64) -> Devices {
        let mut clock = Clock::new(QUANTUM);
        clock.now = now;
        Devices {
            ram: Mapping::anonymous(2 * PAGE).unwrap(),
            clock,
            schedule: Schedule::default(),
            shared: Some(0),
            epoch: 0,
            step: 0,
            stop: None,
            inspect: false,
            idle: false,
            input: None,
            typed: std::collections::VecDeque::new(),
            input_sent: false,
            timer_armed_at: None,
            io: Some(rewind_trace::io_records::Encoder::new()),
            switches: None,
            record_buffer: Vec::new(),
        }
    }

    /// Where the tests' guest keeps its record buffer, and how big it is.
    const IO_BUF: u64 = PAGE as u64;
    const IO_SIZE: u32 = PAGE as u32;

    /// Lays `buffered` into the record buffer as the guest would, with
    /// `lost` records it had no room for.
    fn buffer(dev: &mut Devices, buffered: &[u8], lost: u32) {
        dev.ram.write(IO_BUF, buffered).unwrap();
        dev.ram
            .write(pv::SHARED_IO_BUF, &IO_BUF.to_le_bytes())
            .unwrap();
        dev.ram
            .write(pv::SHARED_IO_SIZE, &IO_SIZE.to_le_bytes())
            .unwrap();
        dev.ram
            .write(pv::SHARED_IO_HEAD, &(buffered.len() as u32).to_le_bytes())
            .unwrap();
        dev.ram
            .write(pv::SHARED_IO_LOST, &lost.to_le_bytes())
            .unwrap();
    }

    /// A u32 field of the shared page.
    fn shared_u32(dev: &Devices, at: u64) -> u32 {
        let mut bytes = [0u8; 4];
        dev.ram.read(at, &mut bytes).unwrap();
        u32::from_le_bytes(bytes)
    }

    /// One write record as the guest lays it out.
    fn guest_write(tid: u32, pos: u64, count: u32) -> Vec<u8> {
        let mut r = Vec::new();
        let len = rewind_trace::io_records::GUEST_HEADER_LEN + 12;
        r.extend_from_slice(&(len as u16).to_le_bytes());
        r.extend_from_slice(&[1, 0]);
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&7u64.to_le_bytes());
        r.extend_from_slice(&1u32.to_le_bytes());
        r.extend_from_slice(&pos.to_le_bytes());
        r.extend_from_slice(&count.to_le_bytes());
        r
    }

    /// Keeps every record it is given, with its step.
    #[derive(Default)]
    struct Kept(Vec<(u64, Vec<u8>)>);
    impl Observer for Kept {
        fn record(&mut self, step: u64, record: &[u8]) {
            self.0.push((step, record.to_vec()));
        }
    }

    #[test]
    fn buffered_records_go_into_the_trace_at_the_step() {
        // Two writes and two lost records in the guest's buffer at step
        // 12: the trace gets them at step 12, and the buffer and the lost
        // count are back at 0 for the guest to go on from.
        let mut dev = devices(0);
        dev.step = 12;
        let buffered = [guest_write(3, 0, 10), guest_write(3, 10, 5)].concat();
        buffer(&mut dev, &buffered, 2);
        let mut kept = Kept::default();
        dev.drain_io(&mut kept).unwrap();

        assert!(kept.0.iter().all(|(step, _)| *step == 12));
        let log = rewind_trace::io_records::decode(&kept.0).unwrap();
        assert_eq!(log.records.len(), 3);
        assert_eq!(log.lost(), 2);
        assert_eq!(shared_u32(&dev, pv::SHARED_IO_HEAD), 0);
        assert_eq!(shared_u32(&dev, pv::SHARED_IO_LOST), 0);
    }

    /// A time limit that stopped a guest computing in user space.
    fn timed_out() -> Stop {
        Stop::TimedOut(Stall {
            rip: 0,
            mode: CpuMode::User,
            since_exit: std::time::Duration::ZERO,
        })
    }

    #[test]
    fn a_time_limit_leaves_buffered_records_out_of_the_trace() {
        // A write the guest buffered after its last exit, and a machine
        // that stops at its time limit: the trace gets nothing, as a replay
        // paused at the last step makes nothing, and the guest's buffer
        // stays as the guest left it. The same buffer under a triple
        // fault, which the guest reaches by itself, goes into the trace.
        let mut dev = devices(0);
        dev.step = 12;
        buffer(&mut dev, &guest_write(3, 0, 10), 0);
        let mut kept = Kept::default();
        dev.end_trace(timed_out(), &mut kept).unwrap();
        assert!(kept.0.is_empty());
        assert_ne!(shared_u32(&dev, pv::SHARED_IO_HEAD), 0);

        dev.end_trace(Stop::TripleFault, &mut kept).unwrap();
        let log = rewind_trace::io_records::decode(&kept.0).unwrap();
        assert_eq!(log.records.len(), 1);
        assert!(kept.0.iter().all(|(step, _)| *step == 12));
    }

    /// A switch record as the guest lays one out, to the thread `tid`.
    fn guest_switch(tid: u32) -> Vec<u8> {
        const SWITCH_KIND: u16 = 14;
        const TASK_PID: u32 = 40;
        const TASK_NAME: &[u8] = b"worker\0";
        let mut r = Vec::new();
        r.extend_from_slice(&((pv::RECORD_HEADER + TASK_NAME.len()) as u32).to_le_bytes());
        r.extend_from_slice(&SWITCH_KIND.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&TASK_PID.to_le_bytes());
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(TASK_NAME);
        r
    }

    /// Devices whose guest switched to a thread at each of `steps`, with
    /// the switches still pending and the machine at the last of them.
    fn devices_with_pending_switches(steps: &[u64]) -> Devices {
        let mut dev = devices(0);
        dev.switches = Some(rewind_trace::switches::Encoder::new());
        let pointer = (IO_BUF as u32).to_le_bytes();
        let mut kept = Kept::default();
        for (i, step) in steps.iter().enumerate() {
            dev.step = *step;
            dev.ram.write(IO_BUF, &guest_switch(i as u32)).unwrap();
            dev.io_out(pv::PORT_EMIT, &pointer, &mut kept).unwrap();
        }
        assert!(kept.0.is_empty());
        dev
    }

    /// The steps of the switches in the batches among `records`.
    fn switch_steps(records: &[(u64, Vec<u8>)]) -> Vec<u64> {
        let mut decoder = rewind_trace::switches::Decoder::default();
        let mut switches = Vec::new();
        for (step, record) in records {
            assert!(rewind_trace::switches::is_batch(record));
            decoder.decode(*step, record, &mut switches).unwrap();
        }
        switches.iter().map(|s| s.step).collect()
    }

    #[test]
    fn every_stop_writes_the_pending_switches() {
        // Switches at steps 3 and 5 still pending, and a machine that
        // stops at step 6 each way a machine stops: a guest exit, a stall
        // at the idle port, a triple fault and the time limit. Each trace
        // ends with one batch at step 6 holding both switches.
        const SWITCHED_AT: [u64; 2] = [3, 5];
        const STOPPED_AT: u64 = 6;
        let exit_port = |port: u16| {
            let mut dev = devices_with_pending_switches(&SWITCHED_AT);
            dev.step = STOPPED_AT;
            let mut kept = Kept::default();
            dev.io_out(port, &0u32.to_le_bytes(), &mut kept).unwrap();
            (dev, kept)
        };
        let (exited, kept_exit) = exit_port(pv::PORT_EXIT);
        let (mut stalled, mut kept_stall) = exit_port(pv::PORT_IDLE);
        assert_eq!(stalled.stop, None);
        assert_eq!(
            stalled.stall_if_idle(&mut kept_stall).unwrap(),
            Some(Stop::Stalled)
        );
        assert_eq!(stalled.stop, Some(Stop::Stalled));
        let mut faulted = devices_with_pending_switches(&SWITCHED_AT);
        faulted.step = STOPPED_AT;
        faulted.stop = Some(Stop::TripleFault);
        let mut limited = devices_with_pending_switches(&SWITCHED_AT);
        limited.step = STOPPED_AT;
        limited.stop = Some(timed_out());

        for (mut dev, mut kept) in [
            (exited, kept_exit),
            (stalled, kept_stall),
            (faulted, Kept::default()),
            (limited, Kept::default()),
        ] {
            let stop = dev.stop.unwrap();
            dev.end_trace(stop, &mut kept).unwrap();
            assert_eq!(kept.0.len(), 1, "{stop:?}");
            assert_eq!(kept.0[0].0, STOPPED_AT, "{stop:?}");
            assert_eq!(switch_steps(&kept.0), SWITCHED_AT, "{stop:?}");
        }
    }

    #[test]
    fn an_idle_exit_stalls_only_when_nothing_wakes_the_guest() {
        // An idle exit with no timer armed and no request pending does
        // not stop the machine at the exit: the verdict waits for the
        // guest's next entry, where a request sent in between, one the
        // APIC took (which clears `idle`) or one still waiting for it,
        // lets the guest run on, and nothing sent stalls the machine and
        // ends its trace at the exit's step.
        let idle_exit = || {
            let mut dev = devices(0);
            dev.step = 6;
            dev.io_out(pv::PORT_IDLE, &0u32.to_le_bytes(), &mut Kept::default())
                .unwrap();
            assert!(dev.idle);
            assert_eq!(dev.stop, None);
            dev
        };

        let mut woken = idle_exit();
        woken.idle = false;
        assert_eq!(woken.stall_if_idle(&mut Kept::default()).unwrap(), None);
        assert_eq!(woken.stop, None);

        let mut asked = idle_exit();
        asked.inspect = true;
        assert_eq!(asked.stall_if_idle(&mut Kept::default()).unwrap(), None);
        assert_eq!(asked.stop, None);

        let mut alone = idle_exit();
        let mut kept = Kept::default();
        assert_eq!(alone.stall_if_idle(&mut kept).unwrap(), Some(Stop::Stalled));
        assert_eq!(alone.stop, Some(Stop::Stalled));
        assert!(!alone.idle);

        // A timer armed wakes the guest at the exit itself.
        let mut timed = devices(0);
        timed.clock.arm(1000);
        timed
            .io_out(pv::PORT_IDLE, &0u32.to_le_bytes(), &mut Kept::default())
            .unwrap();
        assert!(!timed.idle);
    }

    #[test]
    fn a_trace_is_ended_once() {
        // A replay ends its trace where the recording's time limit fell,
        // and a machine that stopped has ended its own: ending a trace
        // again writes no second batch.
        let mut dev = devices_with_pending_switches(&[3]);
        let mut kept = Kept::default();
        dev.finish_switches(&mut kept);
        dev.end_trace(Stop::Stalled, &mut kept).unwrap();
        dev.finish_switches(&mut kept);
        assert_eq!(kept.0.len(), 1);
    }

    #[test]
    fn a_step_with_nothing_buffered_adds_nothing() {
        // An empty buffer with nothing lost: no trace record.
        let mut dev = devices(0);
        buffer(&mut dev, &[], 0);
        let mut kept = Kept::default();
        dev.drain_io(&mut kept).unwrap();
        assert!(kept.0.is_empty());
    }

    #[test]
    fn a_kernel_without_records_is_not_drained() {
        // A guest kernel of the interface before records: whatever its
        // shared page holds past the reseed bytes is not a buffer, and the
        // monitor neither reads it nor writes it.
        let mut dev = devices(0);
        dev.io = None;
        buffer(&mut dev, &guest_write(3, 0, 10), 0);
        let mut kept = Kept::default();
        dev.drain_io(&mut kept).unwrap();
        assert!(kept.0.is_empty());
        assert_ne!(shared_u32(&dev, pv::SHARED_IO_HEAD), 0);
    }

    #[test]
    fn a_request_holds_no_more_arguments_than_the_kernel_passes_on() {
        // The guest kernel passes eight request arguments to init and
        // drops the rest. A request of eight is laid out as each argument
        // and a NUL. One of nine is refused, and says how many fit.
        let args = |n: usize| -> Vec<String> { (0..n).map(|i| format!("arg{i}")).collect() };
        assert_eq!(pv::REQUEST_ARGS_MAX, 8);
        let request = encode_request(&args(pv::REQUEST_ARGS_MAX)).unwrap();
        assert!(request.starts_with(b"arg0\0arg1\0"));
        assert_eq!(request.iter().filter(|b| **b == 0).count(), 8);
        let refused = encode_request(&args(pv::REQUEST_ARGS_MAX + 1))
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "the inspection request has 9 arguments; the guest kernel passes on 8"
        );

        // The byte limit and an argument with a NUL are refused as before.
        assert!(encode_request(&["a".repeat(pv::REQUEST_MAX)]).is_err());
        assert!(encode_request(&["a\0b".to_string()]).is_err());
    }

    #[test]
    fn the_devices_keep_the_step_the_timer_was_last_armed_at() {
        // Arming the timer marks the step of the exit that armed it;
        // disarming it, by writing 0, does not.
        let mut dev = devices(0);
        dev.step = 7;
        dev.io_out(pv::PORT_TIMER, &1000u32.to_le_bytes(), &mut Ignore)
            .unwrap();
        assert_eq!(dev.timer_armed_at, Some(7));
        dev.step = 9;
        dev.io_out(pv::PORT_TIMER, &0u32.to_le_bytes(), &mut Ignore)
            .unwrap();
        assert_eq!(dev.timer_armed_at, Some(7));
    }

    #[test]
    fn a_repeated_clock_read_gets_the_clock_each_time() {
        // `rep insl` from the clock port asks for two 4-byte reads in one
        // exit: each gets the low 32 bits of the clock. A single read of
        // two bytes gets the low two.
        let now = 0x1122_3344_5566_7788u64;
        let mut dev = devices(now);
        let mut two_reads = [0u8; 8];
        dev.io_in(pv::PORT_CLOCK, &mut two_reads).unwrap();
        assert_eq!(two_reads, [0x88, 0x77, 0x66, 0x55, 0x88, 0x77, 0x66, 0x55]);

        let mut short = [0u8; 2];
        dev.io_in(pv::PORT_CLOCK, &mut short).unwrap();
        assert_eq!(short, [0x88, 0x77]);
    }
}

#[cfg(test)]
mod dirty_log_tests {
    use super::*;

    /// A machine without the dirty log registers its RAM without KVM's
    /// logging flag, and one with the log registers it with the flag.
    #[test]
    fn the_dirty_log_sets_the_ram_slots_flag() {
        assert_eq!(DirtyLog::On.slot_flags(), KVM_MEM_LOG_DIRTY_PAGES);
        assert_eq!(DirtyLog::Off.slot_flags(), 0);
    }

    /// A machine created without the dirty log refuses to list dirty
    /// pages instead of answering with none, so a keyframe or checkpoint
    /// asked of one fails loudly. Needs /dev/kvm; without one the test
    /// says so and passes.
    #[test]
    fn a_machine_without_the_dirty_log_refuses_to_list_dirty_pages() {
        if kvm::open().is_err() {
            eprintln!("skipped: /dev/kvm does not open");
            return;
        }
        let config = Config {
            kernel: PathBuf::new(),
            initrd: Vec::new(),
            image: None,
            mem_bytes: 64 << 20,
            cmdline: String::new(),
            seed: [0; 32],
            epoch: 0,
            quantum: 1,
            schedule: Schedule::default(),
            cpu: CpuModel::V3,
            clock: ClockSource::Exits,
            preemption: Preemption::AtExits,
            extras: Extras::Absent,
        };
        let mut off = Machine::create(&config, DirtyLog::Off).unwrap();
        let err = off.dirty_pages().unwrap_err();
        assert!(err.to_string().contains("without a dirty log"), "{err}");

        let mut on = Machine::create(&config, DirtyLog::On).unwrap();
        assert!(on.dirty_pages().unwrap().is_empty());
    }
}
