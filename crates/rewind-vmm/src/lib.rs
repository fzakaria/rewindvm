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
pub mod cpu;
pub mod debug;
pub mod kvm;
pub mod layout;
pub mod memory;
pub mod pmu;
pub mod pv;
pub mod snapshot;
mod watchdog;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_MEM_LOG_DIRTY_PAGES, KVM_MEM_READONLY, kvm_msi, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

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
pub struct Ignore;
impl Observer for Ignore {}

/// Where a forked machine's console input comes from: a person at a
/// terminal, typing into a shell inside the VM. A machine with an input
/// waits for it in real time whenever the VM goes idle, up to the next
/// timer, so the VM's clock keeps pace with the person rather than racing
/// ahead to the next timer. Recordings have no input.
pub trait Input {
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
    /// The console's input, when a person is typing into the VM.
    pub input: Option<Box<dyn Input>>,
    /// Typed bytes not yet given to the VM.
    pub typed: std::collections::VecDeque<u8>,
    /// Input placed in the shared page whose interrupt the VM's APIC has
    /// not yet taken.
    pub input_sent: bool,
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
}

impl Machine {
    /// Creates the VM with its memory and devices, but no guest state:
    /// either [`Machine::boot`] or a snapshot restore supplies that.
    fn create(config: &Config) -> Result<Machine> {
        if config.mem_bytes > RAM_MAX || config.mem_bytes < (64 << 20) {
            bail!("VM memory must be between 64 MiB and {} MiB", RAM_MAX >> 20);
        }

        let kvm = kvm::open()?;
        let vm = kvm.create_vm().context("creating the VM")?;
        vm.set_tss_address(TSS_ADDRESS)?;
        vm.create_irq_chip()
            .context("creating the in-kernel interrupt controller")?;

        let ram = Mapping::anonymous(config.mem_bytes as usize)?;
        // SAFETY: the mapping outlives the VM (both live in Machine, and the
        // VM is dropped first), and nothing else maps this range.
        unsafe {
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: SLOT_RAM,
                flags: KVM_MEM_LOG_DIRTY_PAGES,
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
                let slot = Mapping::anonymous(layout::EXTRAS_LEN as usize)?;
                // SAFETY: the mapping outlives the VM; the slot is read-only.
                unsafe {
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        slot: SLOT_EXTRAS,
                        flags: KVM_MEM_READONLY,
                        guest_phys_addr: layout::EXTRAS_START,
                        memory_size: slot.len() as u64,
                        userspace_addr: slot.host_addr(),
                    })?;
                }
                Some(slot)
            }
        };

        let vcpu = vm.create_vcpu(0).context("creating the vCPU")?;
        vcpu.set_cpuid2(&cpu::cpuid(&kvm, config.cpu)?)?;

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
                input: None,
                typed: std::collections::VecDeque::new(),
                input_sent: false,
            },
            pmem,
            extras,
            config: config.clone(),
            profile: None,
            pmu: None,
            work: None,
            work_base: 0,
            debugging: None,
            int3s: debug::Int3s::default(),
            deadline: None,
            last_exit: std::time::Instant::now(),
        })
    }

    /// A machine at step 0: the kernel loaded, the vCPU at its entry point.
    pub fn boot(config: &Config) -> Result<Machine> {
        let mut m = Self::create(config)?;
        let kernel = std::fs::read(&config.kernel)
            .with_context(|| format!("reading {}", config.kernel.display()))?;
        let spec = boot::BootSpec {
            kernel: &kernel,
            initrd: &config.initrd,
            cmdline: &config.cmdline,
            seed: config.seed,
            pmem_len: m.pmem.as_ref().map_or(0, |p| p.len() as u64),
            extras_len: m.extras.as_ref().map_or(0, |e| e.len() as u64),
        };
        let entry = boot::load(&mut m.dev.ram, &spec)?;
        boot::write_tables(&mut m.dev.ram)?;

        cpu::setup_msrs(&m.vcpu)?;
        cpu::setup_sregs(&m.vcpu)?;
        cpu::setup_regs(&m.vcpu, entry)?;
        cpu::setup_fpu(&m.vcpu)?;
        cpu::setup_lapic(&m.vcpu)?;
        Ok(m)
    }

    /// The number of exits so far.
    pub fn step(&self) -> u64 {
        self.dev.step
    }

    /// Guest branches counted so far, when virtual time follows them.
    pub fn branches(&self) -> u64 {
        self.work.as_ref().map_or(self.work_base, |w| w.total)
    }

    /// Virtual nanoseconds since boot.
    pub fn now(&self) -> u64 {
        self.dev.clock.now
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Stops the machine at `deadline` with [`Stop::TimedOut`], wherever
    /// the guest is then, even computing without exits. The thread that
    /// calls [`Machine::run`] is the one stopped.
    pub fn set_deadline(&mut self, deadline: Option<std::time::Instant>) {
        self.deadline = deadline;
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
        let outcome = self.run_counted(until, obs);
        let paused = self.pause_counting();
        let outcome = outcome?;
        paused?;
        Ok(outcome)
    }

    /// Counts again, and aims again at the timer the guest was running
    /// toward when the machine last stopped: the overflow a margin short
    /// of its branch count, or single steps when it is closer than that.
    fn resume_counting(&mut self) -> Result<()> {
        if let Some((counter, _)) = &self.pmu {
            counter.resume()?;
        }
        let Some(work) = &self.work else {
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
        if let Some(work) = &self.work {
            work.counter.pause()?;
            work.overflow.disarm()?;
        }
        Ok(())
    }

    fn run_counted(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
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
                return Ok(Outcome::Stopped(stop));
            }
            if self.work.is_some() && self.config.preemption == Preemption::AtBranchCounts {
                self.aim_preemption()?;
            }

            // The port of a counted exit; MMIO exits count under MMIO_PORT.
            const MMIO_PORT: u16 = 0xffff;
            let mut debug_stop = None;
            let counted = match self.vcpu.run() {
                Ok(exit) => match exit {
                    VcpuExit::IoOut(port, data) => {
                        self.dev.step += 1;
                        self.dev.io_out(port, data, obs)?;
                        Some(port)
                    }
                    VcpuExit::IoIn(port, data) => {
                        self.dev.step += 1;
                        self.dev.io_in(port, data)?;
                        Some(port)
                    }
                    VcpuExit::MmioRead(_, data) => {
                        self.dev.step += 1;
                        data.fill(0xff);
                        Some(MMIO_PORT)
                    }
                    VcpuExit::MmioWrite(..) => {
                        self.dev.step += 1;
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

            if let Some(stop) = self.dev.stop {
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
                    self.after_step(Quantum::Skip)?;
                    preempted = true;
                }
                if let Some(stop) = debug_stop {
                    return Ok(Outcome::Debug(stop));
                }
                if preempted && until.is_some_and(|u| self.dev.step >= u) {
                    return Ok(Outcome::Paused);
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
        let work = self.work.as_mut().unwrap();
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
    /// exactly on the target; single-stepping once it is within the margin.
    fn reached_preemption(&mut self) -> Result<bool> {
        let work = self.work.as_ref().unwrap();
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
        if !work.stepping && target - count <= PREEMPT_MARGIN {
            self.start_stepping()?;
        }
        Ok(false)
    }

    /// Single-steps the guest, interrupts held, alongside whatever a
    /// debugger traps.
    fn start_stepping(&mut self) -> Result<()> {
        self.work.as_mut().unwrap().stepping = true;
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
        Ok(self.vm.signal_msi(msi)? > 0)
    }
}

impl Devices {
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
                let mut record = vec![0u8; len];
                self.ram.read(value as u64, &mut record)?;
                obs.record(self.step, &record);
            }
            pv::PORT_TIMER => {
                let delta = value as u64;
                let slack = if delta == 0 {
                    0
                } else {
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
                if !self.clock.idle() && !self.inspect {
                    self.stop = Some(Stop::Stalled);
                }
            }
            pv::PORT_EXIT => self.stop = Some(Stop::Guest(GuestExit::from_port(value))),
            pv::PORT_SETUP => {
                let shared = value as u64;
                self.ram
                    .write(shared + pv::SHARED_EPOCH, &self.epoch.to_le_bytes())?;
                self.shared = Some(shared);
            }
            pv::PORT_COM1 => obs.serial(self.step, data[0]),
            _ => {}
        }
        Ok(())
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

    /// The devices of a machine that has set up its shared page at 0.
    fn devices(now: u64) -> Devices {
        let mut clock = Clock::new(QUANTUM);
        clock.now = now;
        Devices {
            ram: Mapping::anonymous(PAGE).unwrap(),
            clock,
            schedule: Schedule::default(),
            shared: Some(0),
            epoch: 0,
            step: 0,
            stop: None,
            inspect: false,
            input: None,
            typed: std::collections::VecDeque::new(),
            input_sent: false,
        }
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
