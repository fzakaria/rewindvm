//! Deterministic ARM64 virtual machine monitor on macOS Hypervisor.framework.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};

use crate::arm64::debug::{
    DebugStop, EC_BRK64, EC_HW_BP_LOWER, EC_SW_STEP_LOWER, EC_WATCHPOINT_LOWER, MDSCR_SS,
    PSTATE_SS, Stepping, TrapSet, match_watchpoints,
};
use crate::hvf::boot::{self, BootSpec};
use crate::hvf::dirty::DirtyTracker;
use crate::hvf::layout::*;
use crate::hvf::sys;
use crate::hvf::{HvfVcpu, HvfVm};
use crate::memory::Mapping;
use crate::pv::{self, Clock};
use crate::{
    BootInto, Config, CpuMode, Devices, DirtyLog, Input, Interrupter, Observer, Origin, Outcome,
    Stall, Stop, checkpoint, interrupt, watchdog,
};

const PMEM_ALIGN: usize = 2 << 20;

/// Exception classes (`ESR_EL2.EC`, bits 31:26).
const EC_WFX: u64 = 0x01;
const EC_HVC64: u64 = 0x16;
const EC_SYS64: u64 = 0x18;
const EC_DATA_ABORT_LOWER: u64 = 0x24;

/// What one HVF exception exit means for `run_loop`.
enum ExitAction {
    Continue,
    Counted(u16),
    Debug(DebugStop),
}

/// Hypercall immediate numbers (`HVC #imm`) matching Rewind's PV ports.
const HVC_EMIT: u16 = pv::PORT_EMIT;
const HVC_CLOCK: u16 = pv::PORT_CLOCK;
const HVC_NET_TX: u16 = pv::PORT_NET_TX;
const HVC_TIMER: u16 = pv::PORT_TIMER;
const HVC_IDLE: u16 = pv::PORT_IDLE;
const HVC_EXIT: u16 = pv::PORT_EXIT;
const HVC_SETUP: u16 = pv::PORT_SETUP;
const HVC_SERIAL: u16 = pv::PORT_COM1;
const HVC_IRQ_ACK: u16 = pv::HVC_IRQ_ACK;

/// Whether a run on this Apple Silicon host reserves the extras slot.
pub fn extras_for(cpu: crate::CpuModel) -> Result<crate::Extras> {
    let bits = crate::hvf::max_ipa_bits()?;
    if bits >= EXTRAS_ADDRESS_BITS {
        return Ok(crate::Extras::Reserved);
    }
    match cpu {
        crate::CpuModel::Host => Ok(crate::Extras::Absent),
        _ => bail!(
            "this Apple Silicon host reaches {bits} IPA bits, where the extras slot needs \
             {EXTRAS_ADDRESS_BITS}"
        ),
    }
}

pub struct Machine {
    // Field order matters: vCPU MUST be dropped before VM, and VM before host
    // memory mappings (`dev.ram`, `pmem`, `extras`).
    pub(crate) vcpu: HvfVcpu,
    pub(crate) vm: HvfVm,
    pub(crate) dev: Devices,
    pub(crate) pmem: Option<Mapping>,
    pub(crate) extras: Option<Mapping>,
    pub(crate) config: Config,
    pub(crate) dirty_log: DirtyLog,
    pub(crate) dirty: DirtyTracker,
    pub(crate) irq_pending: bool,
    pub profile: Option<std::collections::HashMap<(u16, u64), u64>>,
    pub pmu: Option<(crate::pmu::Counter, u64)>,
    pub(crate) work_base: u64,
    pub(crate) debugging: Option<crate::debug::Debugging>,
    pub(crate) passing_breakpoint: bool,
    pub(crate) watching_step: Option<(u64, TrapSet)>,
    pub(crate) int3s: crate::debug::Int3s,
    pub(crate) deadline: Option<crate::Deadline>,
    pub(crate) last_exit: std::time::Instant,
    pub(crate) origin: Origin,
    pub(crate) checkpoints: checkpoint::Checkpoints,
    pub(crate) interrupt: Arc<interrupt::Shared>,
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.interrupt.forget();
    }
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("config", &self.config)
            .field("step", &self.dev.step)
            .finish_non_exhaustive()
    }
}

impl Machine {
    pub(crate) fn create(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        if config.mem_bytes > MEM_MAX || config.mem_bytes < (64 << 20) {
            bail!("VM memory must be between 64 MiB and {} MiB", MEM_MAX >> 20);
        }
        if matches!(config.clock, crate::ClockSource::Branches(_)) {
            bail!(
                "branch-counter virtual time is not supported on macOS Hypervisor.framework; \
                 use `--clock exits`"
            );
        }
        if config.preemption == crate::Preemption::AtBranchCounts {
            bail!("branch-count preemption is not supported on macOS Hypervisor.framework");
        }

        let vm = HvfVm::create()?;
        let ram_len = (ram_span(config.mem_bytes) as usize).next_multiple_of(HOST_PAGE_SIZE);
        let ram = Mapping::anonymous(ram_len)?;
        let _advice = ram.prefer_huge_pages();

        let ram_flags = sys::HV_MEMORY_READ | sys::HV_MEMORY_WRITE | sys::HV_MEMORY_EXEC;
        // SAFETY: `ram` outlives `vm` (field drop order), and `ram.host_addr()`,
        // `RAM_BASE`, and `ram.len()` are all 16 KiB aligned.
        unsafe { vm.map(ram.host_addr(), RAM_BASE, ram.len(), ram_flags)? };

        let pmem = match &config.image {
            Some(path) => {
                let image = Mapping::file(path, PMEM_ALIGN)?;
                let start = pmem_start(ram.len() as u64);
                if start + image.len() as u64 > EXTRAS_START {
                    bail!("the image does not fit below the extras slot");
                }
                // SAFETY: `image` outlives `vm`, mapped read-only at 2 MiB alignment.
                unsafe { vm.map(image.host_addr(), start, image.len(), sys::HV_MEMORY_READ)? };
                Some(image)
            }
            None => None,
        };

        let extras = match config.extras {
            crate::Extras::Absent => None,
            crate::Extras::Reserved => {
                if vm.ipa_bits() < EXTRAS_ADDRESS_BITS {
                    bail!(
                        "this run reserves the extras slot ({EXTRAS_ADDRESS_BITS} IPA bits), \
                         and this host reaches {}",
                        vm.ipa_bits()
                    );
                }
                let slot = Mapping::anonymous(EXTRAS_LEN as usize)?;
                // SAFETY: `slot` outlives `vm`, mapped read-only.
                unsafe {
                    vm.map(
                        slot.host_addr(),
                        EXTRAS_START,
                        slot.len(),
                        sys::HV_MEMORY_READ,
                    )?
                };
                Some(slot)
            }
        };

        let vcpu = HvfVcpu::create()?;
        let interrupt = interrupt::Shared::new(vcpu.id());
        let dirty = DirtyTracker::new(ram.len(), dirty_log == DirtyLog::On);

        Ok(Machine {
            vcpu,
            vm,
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
                event_pending: false,
                event_buffer: Vec::new(),
                net_config: config.net,
                net_rx_ring: None,
                net_rx_slots: 0,
                tx_queue: std::collections::VecDeque::new(),
                tx_pending: None,
                cluster_mode: false,
                net_rx_pending: false,
                monitor_dirty_pages: Vec::new(),
            },
            pmem,
            extras,
            config: config.clone(),
            dirty_log,
            dirty,
            irq_pending: false,
            profile: None,
            pmu: None,
            work_base: 0,
            debugging: None,
            passing_breakpoint: false,
            watching_step: None,
            int3s: crate::debug::Int3s::default(),
            deadline: None,
            last_exit: std::time::Instant::now(),
            origin: Origin::Boot,
            checkpoints: checkpoint::Checkpoints::default(),
            interrupt,
        })
    }

    /// Boots an ARM64 Linux kernel `Image` on Hypervisor.framework at step 0.
    pub fn boot(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        let mut m = Self::create(config, dirty_log)?;
        let addrs = m.write_boot(BootInto::Ram)?;
        crate::hvf::cpu::setup_boot_vcpu(&m.vcpu, addrs.kernel_entry, addrs.dtb_gpa)?;
        if m.dirty.is_enabled() {
            m.dirty.arm_all(&m.vm)?;
        }
        Ok(m)
    }

    pub(crate) fn write_boot(&mut self, into: BootInto) -> Result<boot::BootAddresses> {
        let kernel = std::fs::read(&self.config.kernel)
            .with_context(|| format!("reading {}", self.config.kernel.display()))?;
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &self.config.initrd,
            cmdline: &self.config.cmdline,
            seed: self.config.seed,
            pmem_len: self.pmem.as_ref().map_or(0, |p| p.len() as u64),
            extras_len: self.extras.as_ref().map_or(0, |e| e.len() as u64),
        };
        match into {
            BootInto::Ram => boot::load(&mut self.dev.ram, &spec),
            BootInto::Scratch => {
                let mut ram = Mapping::anonymous(self.dev.ram.len())?;
                let addrs = boot::load(&mut ram, &spec)?;
                self.checkpoints.booted = Some(ram);
                Ok(addrs)
            }
        }
    }

    pub(crate) fn drain_dirty_log(&mut self) -> Result<Vec<usize>> {
        self.dirty.drain(&self.vm, self.dev.ram.len())
    }

    pub fn step(&self) -> u64 {
        self.dev.step
    }

    pub fn timer_armed_at(&self) -> Option<u64> {
        self.dev.timer_armed_at
    }

    pub fn branches(&self) -> u64 {
        self.work_base
    }

    pub fn release_counters(&mut self) {}

    pub fn now(&self) -> u64 {
        self.dev.clock.now
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn ram_pages(&self) -> usize {
        self.dev.ram.len() / PAGE_SIZE
    }

    pub fn set_deadline(&mut self, deadline: Option<std::time::Instant>) {
        self.deadline = deadline.map(crate::Deadline::At);
    }

    pub fn set_late_deadline(&mut self, from: std::time::Instant, limit: crate::LateLimit) {
        self.deadline = Some(crate::Deadline::Late { from, limit });
    }

    pub fn finish_switches(&mut self, obs: &mut dyn Observer) {
        self.dev.finish_switches(obs);
    }

    pub fn interrupter(&self) -> Interrupter {
        Interrupter(self.interrupt.clone())
    }

    fn stall(&self) -> Result<Stall> {
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        let mode = if (cpsr & 0xf) == 0 {
            CpuMode::User
        } else {
            CpuMode::Kernel
        };
        Ok(Stall {
            rip: pc,
            mode,
            since_exit: self.last_exit.elapsed(),
        })
    }

    pub fn run(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
        self.run_bounded(until, None, false, obs)
    }

    pub fn set_cluster_mode(&mut self, enabled: bool) {
        self.dev.cluster_mode = enabled;
    }

    pub fn encoders(
        &self,
    ) -> (
        Option<rewind_trace::io_records::Encoder>,
        Option<rewind_trace::switches::Encoder>,
    ) {
        (self.dev.io.clone(), self.dev.switches.clone())
    }

    pub fn set_encoders(
        &mut self,
        io: Option<rewind_trace::io_records::Encoder>,
        switches: Option<rewind_trace::switches::Encoder>,
    ) {
        self.dev.io = io;
        self.dev.switches = switches;
    }

    pub fn run_cluster_slice(
        &mut self,
        horizon_ns: u64,
        max_steps: u64,
        allow_idle_skip: bool,
        obs: &mut dyn Observer,
    ) -> Result<Outcome> {
        let until_step = self.dev.step.saturating_add(max_steps);
        self.run_bounded(Some(until_step), Some(horizon_ns), allow_idle_skip, obs)
    }

    pub fn take_tx_frames(&mut self) -> Vec<(u64, Vec<u8>)> {
        self.dev.tx_queue.drain(..).collect()
    }

    pub fn deliver_rx_frame(&mut self, frame: &[u8]) -> Result<bool> {
        let (Some(shared), Some(ring)) = (self.dev.shared, self.dev.net_rx_ring) else {
            return Ok(false);
        };
        let slots = self.dev.net_rx_slots;
        if slots == 0 || frame.len() > pv::NET_FRAME_MAX {
            return Ok(false);
        }
        let mut prod_b = [0u8; 4];
        let mut cons_b = [0u8; 4];
        self.dev.ram_read(shared + pv::SHARED_NET_RX_PROD, &mut prod_b)?;
        self.dev.ram_read(shared + pv::SHARED_NET_RX_CONS, &mut cons_b)?;
        let prod = u32::from_le_bytes(prod_b);
        let cons = u32::from_le_bytes(cons_b);
        if prod.wrapping_sub(cons) >= slots {
            return Ok(false);
        }
        let slot_idx = u64::from(prod % slots);
        let slot_addr = ring + slot_idx * pv::NET_SLOT_SIZE as u64;
        self.dev
            .ram_write(slot_addr, &(frame.len() as u32).to_le_bytes())?;
        self.dev.ram_write(slot_addr + 4, frame)?;
        let next_prod = prod.wrapping_add(1);
        self.dev
            .ram_write(shared + pv::SHARED_NET_RX_PROD, &next_prod.to_le_bytes())?;

        if self.dirty_log == DirtyLog::On
            && let Some(off) = slot_addr.checked_sub(RAM_BASE)
        {
            let first_page = (off as usize) / PAGE_SIZE;
            let last_page = ((off as usize) + 4 + frame.len().saturating_sub(1)) / PAGE_SIZE;
            for p in first_page..=last_page {
                self.dev.monitor_dirty_pages.push(p);
                self.checkpoints.since.insert(p as u32);
            }
        }

        self.dev.net_rx_pending = !self.inject(pv::PENDING_NET)?;
        self.dev.idle = false;
        Ok(true)
    }

    pub fn advance_idle_to(&mut self, target_local_ns: u64) -> Result<()> {
        if target_local_ns > self.dev.clock.now {
            self.dev.clock.now = target_local_ns;
        }
        if let Some(shared) = self.dev.shared {
            let now = self.dev.clock.now.to_le_bytes();
            self.dev.ram_write(shared + pv::SHARED_NOW, &now)?;
        }
        if self.dev.clock.due() {
            if self.inject(pv::PENDING_TIMER)? {
                self.dev.clock.deadline = None;
            }
            self.dev.idle = false;
        }
        Ok(())
    }

    pub fn is_idle(&self) -> bool {
        self.dev.idle
    }

    pub(crate) fn set_idle(&mut self, idle: bool, net_rx_pending: bool) {
        self.dev.idle = idle;
        self.dev.net_rx_pending = net_rx_pending;
    }

    pub(crate) fn net_rx_pending(&self) -> bool {
        self.dev.net_rx_pending
    }

    pub fn timer_deadline(&self) -> Option<u64> {
        self.dev.clock.deadline
    }

    pub fn stopped(&self) -> Option<Stop> {
        self.dev.stop
    }

    pub fn set_initial_now(&mut self, now: u64) -> Result<()> {
        self.dev.clock.now = now;
        if let Some(shared) = self.dev.shared {
            self.dev.ram_write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;
        }
        Ok(())
    }

    fn run_bounded(
        &mut self,
        until: Option<u64>,
        until_ns: Option<u64>,
        allow_idle_skip: bool,
        obs: &mut dyn Observer,
    ) -> Result<Outcome> {
        self.interrupt.entered();
        let outcome = self.run_loop(until, until_ns, allow_idle_skip, obs);
        self.interrupt.left();
        outcome
    }

    fn run_loop(
        &mut self,
        until: Option<u64>,
        until_ns: Option<u64>,
        allow_idle_skip: bool,
        obs: &mut dyn Observer,
    ) -> Result<Outcome> {
        if let Some(stop) = self.dev.stop {
            return Ok(Outcome::Stopped(stop));
        }
        if until.is_some_and(|u| u <= self.dev.step) {
            return Ok(Outcome::Paused);
        }
        if self.dev.cluster_mode && self.dev.idle {
            return Ok(Outcome::Idle);
        }
        if until_ns.is_some_and(|h| self.dev.clock.now >= h) {
            return Ok(Outcome::Paused);
        }

        let watchdog = self
            .deadline
            .clone()
            .map(|d| watchdog::Watchdog::start_with_interrupter(d, Some(self.interrupter())));
        let expired = watchdog.as_ref().map(|w| w.expired());
        self.last_exit = std::time::Instant::now();

        loop {
            if expired
                .as_ref()
                .is_some_and(|e| e.load(Ordering::Relaxed))
            {
                let stop = Stop::TimedOut(self.stall()?);
                self.dev.stop = Some(stop);
                self.dev.end_trace(stop, obs)?;
                return Ok(Outcome::Stopped(stop));
            }
            if let Some(stop) = self.dev.stall_if_idle(obs)? {
                return Ok(Outcome::Stopped(stop));
            }
            if self.interrupt.take() {
                return Ok(Outcome::Interrupted);
            }

            // HVF clears pending interrupt level on every `hv_vcpu_run` return,
            // so re-assert the virtual IRQ before entry whenever pending.
            self.vcpu.set_pending_irq(self.irq_pending)?;
            self.arm_single_step_if_needed()?;

            let exit = self.vcpu.run()?;
            let action = match exit.reason {
                sys::HV_EXIT_REASON_CANCELED => ExitAction::Continue,
                sys::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    self.vcpu.set_vtimer_mask(true)?;
                    ExitAction::Continue
                }
                sys::HV_EXIT_REASON_EXCEPTION => self.handle_exception(exit.exception, obs)?,
                other => bail!("unexpected HVF exit reason {other} at step {}", self.dev.step),
            };

            if let Some(stop) = self.dev.stop {
                self.dev.end_trace(stop, obs)?;
                return Ok(Outcome::Stopped(stop));
            }
            let port = match action {
                ExitAction::Debug(stop) => return Ok(Outcome::Debug(stop)),
                ExitAction::Continue => {
                    if self.interrupt.take() {
                        return Ok(Outcome::Interrupted);
                    }
                    continue;
                }
                ExitAction::Counted(port) => port,
            };
            if self.deadline.is_some() {
                self.last_exit = std::time::Instant::now();
            }
            if let Some(profile) = &mut self.profile {
                let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
                *profile.entry((port, pc)).or_default() += 1;
            }
            self.after_step()?;
            self.dev.write_event(obs);
            if let Some(frame) = self.dev.tx_pending.take() {
                self.dev.tx_queue.push_back((self.dev.clock.now, frame));
            }

            if self.passing_breakpoint {
                self.finish_pass_breakpoint()?;
                if self
                    .debugging
                    .as_ref()
                    .is_some_and(|d| d.stepping == Stepping::Yes)
                {
                    return Ok(Outcome::Debug(DebugStop::Step));
                }
            } else if let Some((address, traps)) = self.watching_step.take() {
                self.apply_guest_debug()?;
                return Ok(Outcome::Debug(DebugStop::Watchpoint { address, traps }));
            } else if self
                .debugging
                .as_ref()
                .is_some_and(|d| d.stepping == Stepping::Yes)
            {
                return Ok(Outcome::Debug(DebugStop::Step));
            }

            if self.dev.cluster_mode && self.dev.idle {
                if allow_idle_skip
                    && let (Some(deadline), Some(horizon)) = (self.dev.clock.deadline, until_ns)
                    && deadline < horizon
                {
                    self.advance_idle_to(deadline)?;
                    if until.is_some_and(|u| self.dev.step >= u) || obs.stop() {
                        return Ok(Outcome::Paused);
                    }
                    continue;
                }
                return Ok(Outcome::Idle);
            }

            if until.is_some_and(|u| self.dev.step >= u)
                || until_ns.is_some_and(|h| self.dev.clock.now >= h)
                || obs.stop()
            {
                return Ok(Outcome::Paused);
            }
        }
    }

    fn handle_exception(
        &mut self,
        ex: sys::hv_vcpu_exit_exception_t,
        obs: &mut dyn Observer,
    ) -> Result<ExitAction> {
        const MMIO_PORT: u16 = 0xffff;
        let ec = (ex.syndrome >> 26) & 0x3f;
        match ec {
            EC_HVC64 => {
                // On ARM64, HVC already sets ELR_EL2 (PC) to the instruction after HVC.
                let imm = (ex.syndrome & 0xffff) as u16;
                if imm == HVC_IRQ_ACK {
                    self.irq_pending = false;
                    return Ok(ExitAction::Continue);
                }
                let arg = self.vcpu.get_reg(sys::HV_REG_X0)?;
                self.dev.step += 1;
                self.dev.drain_io(obs)?;
                match imm {
                    HVC_CLOCK => {
                        let shared = self
                            .dev
                            .shared
                            .context("the VM read the clock before setup")?;
                        let now = self.dev.clock.now;
                        self.dev
                            .ram_write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;
                        self.vcpu.set_reg(sys::HV_REG_X0, now)?;
                    }
                    HVC_SERIAL => {
                        obs.serial(self.dev.step, arg as u8);
                    }
                    HVC_EMIT | HVC_NET_TX | HVC_TIMER | HVC_IDLE | HVC_EXIT | HVC_SETUP => {
                        self.dev.io_out_u64(imm, arg, obs)?;
                    }
                    other => {
                        bail!("unknown Rewind HVC #{other:#x} (x0={arg:#x}) at step {}", self.dev.step);
                    }
                }
                Ok(ExitAction::Counted(imm))
            }
            EC_DATA_ABORT_LOWER => {
                let is_write = (ex.syndrome & (1 << 6)) != 0;
                let ipa = ex.physical_address;
                if is_write && self.dirty.handle_write_fault(&self.vm, ipa)? {
                    // Retry the store with write permissions enabled (do not advance PC).
                    return Ok(ExitAction::Continue);
                }
                let isv = (ex.syndrome & (1 << 24)) != 0;
                let srt = ((ex.syndrome >> 16) & 0x1f) as u32;
                // Rewind paravirtual MMIO register page (shared with Linux ARM64 KVM).
                if (REWIND_MMIO_BASE..REWIND_MMIO_BASE + REWIND_MMIO_SIZE).contains(&ipa) {
                    if !isv {
                        bail!(
                            "Rewind MMIO access at {ipa:#x} without valid syndrome (ESR={:#x})",
                            ex.syndrome
                        );
                    }
                    let offset = (ipa - REWIND_MMIO_BASE) as u16;
                    let Some(port) = mmio_offset_to_port(offset) else {
                        bail!(
                            "unknown Rewind MMIO access +{offset:#x} (write={is_write}) at step {}",
                            self.dev.step
                        );
                    };
                    self.advance_pc()?;
                    if is_write && port == HVC_IRQ_ACK {
                        self.irq_pending = false;
                        return Ok(ExitAction::Continue);
                    }
                    self.dev.step += 1;
                    self.dev.drain_io(obs)?;
                    if is_write {
                        let arg = if srt < 31 {
                            self.vcpu.get_reg(srt)?
                        } else {
                            0
                        };
                        match port {
                            HVC_SERIAL => obs.serial(self.dev.step, arg as u8),
                            HVC_EMIT | HVC_NET_TX | HVC_TIMER | HVC_IDLE | HVC_EXIT | HVC_SETUP => {
                                self.dev.io_out_u64(port, arg, obs)?;
                            }
                            other => {
                                bail!(
                                    "unexpected Rewind MMIO write port {other:#x} (val={arg:#x}) \
                                     at step {}",
                                    self.dev.step
                                );
                            }
                        }
                    } else if port == HVC_CLOCK {
                        let shared = self
                            .dev
                            .shared
                            .context("the VM read the clock before setup")?;
                        let now = self.dev.clock.now;
                        self.dev
                            .ram_write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;
                        if srt < 31 {
                            self.vcpu.set_reg(srt, now)?;
                        }
                    } else if srt < 31 {
                        self.vcpu.set_reg(srt, 0)?;
                    }
                    return Ok(ExitAction::Counted(port));
                }
                // Unmapped MMIO or write to read-only pmem/extras: emulate RAZ/WI and advance PC.
                if !is_write && isv && srt < 31 {
                    self.vcpu.set_reg(srt, 0)?;
                }
                self.advance_pc()?;
                self.dev.step += 1;
                self.dev.drain_io(obs)?;
                Ok(ExitAction::Counted(MMIO_PORT))
            }
            EC_SYS64 => {
                // Trapped MSR/MRS (e.g., OSLSR_EL1, OSLAR_EL1, LORC_EL1, PMU regs when EL2 is off):
                // emulate deterministically as RAZ/WI so no host counter leaks.
                let is_read = (ex.syndrome & 1) != 0;
                let rt = ((ex.syndrome >> 5) & 0x1f) as u32;
                if is_read && rt < 31 {
                    // OSLSR_EL1 (op0=2, op1=0, crn=1, crm=1, op2=4) reports OSLM=1, unlocked (0x8).
                    // Mask out Rt (bits 9:5) and Direction (bit 0).
                    let sysreg = ex.syndrome & 0x003f_fc1e;
                    const SYSREG_OSLSR_EL1: u64 = (2 << 20) | (4 << 17) | (1 << 10) | (1 << 1);
                    let val = if sysreg == SYSREG_OSLSR_EL1 { 0x8 } else { 0 };
                    self.vcpu.set_reg(rt, val)?;
                }
                self.advance_pc()?;
                Ok(ExitAction::Continue)
            }
            EC_WFX => {
                self.advance_pc()?;
                Ok(ExitAction::Continue)
            }
            EC_HW_BP_LOWER => {
                let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
                Ok(ExitAction::Debug(DebugStop::Breakpoint(pc)))
            }
            EC_BRK64 => {
                let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
                let ours = self.translate_active(pc)?.is_some_and(|(pa, _)| {
                    self.int3s.replaced.contains_key(&pa) && self.int3s.lifted != Some(pa)
                });
                if ours {
                    Ok(ExitAction::Debug(DebugStop::Breakpoint(pc)))
                } else {
                    self.reinject_brk_el1(ex.syndrome)?;
                    Ok(ExitAction::Continue)
                }
            }
            EC_WATCHPOINT_LOWER => {
                let far = ex.virtual_address;
                let traps = self.debugging.as_ref().map_or(&[][..], |d| &d.traps);
                let (address, hit_set) = match_watchpoints(far, traps)
                    .unwrap_or_else(|| (far, TrapSet::default()));
                // ARM64 watchpoints abort *before* the access commits; step the
                // watched instruction once with watchpoints suppressed so the
                // write lands in RAM and PC advances, matching x86 semantics.
                self.watching_step = Some((address, hit_set));
                self.apply_guest_debug()?;
                Ok(ExitAction::Continue)
            }
            EC_SW_STEP_LOWER => {
                if self.passing_breakpoint {
                    self.finish_pass_breakpoint()?;
                    if self
                        .debugging
                        .as_ref()
                        .is_some_and(|d| d.stepping == Stepping::Yes)
                    {
                        return Ok(ExitAction::Debug(DebugStop::Step));
                    }
                    return Ok(ExitAction::Continue);
                }
                if let Some((address, traps)) = self.watching_step.take() {
                    self.apply_guest_debug()?;
                    return Ok(ExitAction::Debug(DebugStop::Watchpoint { address, traps }));
                }
                if self
                    .debugging
                    .as_ref()
                    .is_some_and(|d| d.stepping == Stepping::Yes)
                {
                    return Ok(ExitAction::Debug(DebugStop::Step));
                }
                // Stray single-step: clear SS bits and continue.
                let mdscr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_MDSCR_EL1)?;
                self.vcpu
                    .set_sys_reg(sys::HV_SYS_REG_MDSCR_EL1, mdscr & !MDSCR_SS)?;
                let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
                self.vcpu.set_reg(sys::HV_REG_CPSR, cpsr & !PSTATE_SS)?;
                Ok(ExitAction::Continue)
            }
            other => {
                let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
                bail!(
                    "unexpected ARM64 exception EC={other:#x} ESR={:#x} FAR={:#x} IPA={:#x} \
                     PC={pc:#x} at step {}",
                    ex.syndrome,
                    ex.virtual_address,
                    ex.physical_address,
                    self.dev.step
                );
            }
        }
    }

    fn advance_pc(&self) -> Result<()> {
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        self.vcpu.set_reg(sys::HV_REG_PC, pc.wrapping_add(4))
    }

    fn after_step(&mut self) -> Result<()> {
        self.dev.clock.tick();
        if let Some(shared) = self.dev.shared {
            let now = self.dev.clock.now.to_le_bytes();
            self.dev.ram_write(shared + pv::SHARED_NOW, &now)?;
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
                .ram_write(shared + pv::SHARED_STALL_NS, &(ns as u32).to_le_bytes())?;
            reasons |= pv::PENDING_STALL;
        }
        if let (Some(seed), Some(shared)) =
            (self.dev.schedule.reseed_at(self.dev.step), self.dev.shared)
        {
            let bytes = pv::Schedule::reseed_bytes(seed, self.dev.step);
            self.dev.ram_write(shared + pv::SHARED_RESEED, &bytes)?;
            reasons |= pv::PENDING_RESEED;
        }
        if self.dev.inspect {
            reasons |= pv::PENDING_INSPECT;
        }
        if self.dev.input.is_some() && self.deliver_input()? {
            reasons |= pv::PENDING_INPUT;
        }
        if self.dev.net_rx_pending {
            reasons |= pv::PENDING_NET;
        }
        if reasons != 0 && self.inject(reasons)? {
            if reasons & pv::PENDING_TIMER != 0 {
                self.dev.clock.deadline = None;
            }
            if reasons & pv::PENDING_NET != 0 {
                self.dev.net_rx_pending = false;
            }
            self.dev.inspect = false;
            self.dev.input_sent = false;
        }
        Ok(())
    }

    /// Maps `image` into the extras slot and refreshes the HVF stage-2 mapping.
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
        self.vm.unmap(EXTRAS_START, slot.len())?;
        // SAFETY: target range lies inside `slot`, owned by this machine.
        let mapped = unsafe {
            libc::mmap(
                slot.host_addr() as *mut libc::c_void,
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            // SAFETY: re-mapping the still-owned slot range.
            let _ = unsafe {
                self.vm.map(
                    slot.host_addr(),
                    EXTRAS_START,
                    slot.len(),
                    sys::HV_MEMORY_READ,
                )
            };
            bail!("mapping {}: {err}", image.display());
        }
        // SAFETY: `slot` outlives `self.vm` and is 16 KiB aligned.
        unsafe {
            self.vm.map(
                slot.host_addr(),
                EXTRAS_START,
                slot.len(),
                sys::HV_MEMORY_READ,
            )?;
        }
        Ok(())
    }

    pub fn set_input(&mut self, input: Box<dyn Input>) {
        self.dev.input = Some(input);
    }

    fn deliver_input(&mut self) -> Result<bool> {
        if self.dev.step.is_multiple_of(crate::INPUT_POLL_STEPS)
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
        let mut len = [0u8; 4];
        self.dev.ram_read(shared + pv::SHARED_INPUT_LEN, &mut len)?;
        if u32::from_le_bytes(len) != 0 {
            return Ok(false);
        }
        let n = self.dev.typed.len().min(pv::INPUT_MAX);
        let chunk: Vec<u8> = self.dev.typed.drain(..n).collect();
        self.dev.ram_write(shared + pv::SHARED_INPUT, &chunk)?;
        self.dev
            .ram_write(shared + pv::SHARED_INPUT_LEN, &(n as u32).to_le_bytes())?;
        self.dev.input_sent = true;
        Ok(true)
    }

    pub fn request_inspection(&mut self, args: &[String]) -> Result<()> {
        let shared = self
            .dev
            .shared
            .context("the VM has not finished booting at this step")?;
        let request = crate::encode_request(args)?;
        self.dev.ram_write(
            shared + pv::SHARED_REQUEST_LEN,
            &(request.len() as u32).to_le_bytes(),
        )?;
        self.dev.ram_write(shared + pv::SHARED_REQUEST, &request)?;
        self.dev.inspect = !self.inject(pv::PENDING_INSPECT)?;
        Ok(())
    }

    fn inject(&mut self, reasons: u32) -> Result<bool> {
        let Some(shared) = self.dev.shared else {
            return Ok(false);
        };
        let at = shared + pv::SHARED_PENDING;
        let mut pending = [0u8; 4];
        self.dev.ram_read(at, &mut pending)?;
        let pending = u32::from_le_bytes(pending) | reasons;
        self.dev.ram_write(at, &pending.to_le_bytes())?;
        self.irq_pending = true;
        self.dev.idle = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            kernel: std::path::PathBuf::new(),
            initrd: Vec::new(),
            image: None,
            mem_bytes: 64 << 20,
            cmdline: String::new(),
            seed: [0; 32],
            epoch: 0,
            quantum: 1,
            schedule: pv::Schedule::default(),
            cpu: crate::CpuModel::Host,
            clock: crate::ClockSource::Exits,
            preemption: crate::Preemption::AtExits,
            extras: crate::Extras::Absent,
            net: None,
        }
    }

    #[test]
    fn hvf_machine_dirty_log_and_state_roundtrip() {
        if crate::hvf::kvm::open().is_err() {
            eprintln!("skipped: Hypervisor.framework entitlement not present on test binary");
            return;
        }
        {
            let mut off = Machine::create(&test_config(), DirtyLog::Off).unwrap();
            let err = off.dirty_pages().unwrap_err();
            assert!(err.to_string().contains("without a dirty log"), "{err}");
            let (cpu, irqchips, devices) = off.state().unwrap();
            off.apply_state(&cpu, &irqchips, &devices).unwrap();
        }
        {
            let mut on = Machine::create(&test_config(), DirtyLog::On).unwrap();
            assert!(on.dirty_pages().unwrap().is_empty());
        }
    }

    #[test]
    fn hvf_executes_arm64_guest_dirty_fault_and_hypercalls() {
        if crate::hvf::kvm::open().is_err() {
            eprintln!("skipped: Hypervisor.framework entitlement not present on test binary");
            return;
        }
        let mut m = Machine::create(&test_config(), DirtyLog::On).unwrap();
        // Tiny ARM64 program at RAM_BASE + 0x20000 (128 KiB):
        //   movz x1, #0x4000, lsl #16      ; x1 = 0x4000_0000 (RAM_BASE)
        //   add  x1, x1, #1, lsl #12       ; x1 = 0x4000_1000 (page 1)
        //   movz x2, #0x1234
        //   str  x2, [x1]                  ; triggers stage-2 dirty write fault
        //   movz x0, #'A'
        //   hvc  #0x3f8                    ; HVC_SERIAL
        //   movz x0, #0
        //   hvc  #0x5f4                    ; HVC_EXIT (PowerOff)
        let code: [u32; 8] = [
            0xd2a8_0001,
            0x9140_0421,
            0xd282_4682,
            0xf900_0022,
            0xd280_0820,
            0xd400_0002 | (u32::from(HVC_SERIAL) << 5),
            0xd280_0000,
            0xd400_0002 | (u32::from(HVC_EXIT) << 5),
        ];
        let entry_off = 0x20000u64;
        for (i, insn) in code.iter().enumerate() {
            m.dev
                .ram
                .write(entry_off + (i * 4) as u64, &insn.to_le_bytes())
                .unwrap();
        }
        crate::hvf::cpu::setup_boot_vcpu(&m.vcpu, RAM_BASE + entry_off, 0).unwrap();
        m.dirty.arm_all(&m.vm).unwrap();

        struct Capture(Vec<u8>);
        impl Observer for Capture {
            fn serial(&mut self, _step: u64, byte: u8) {
                self.0.push(byte);
            }
        }
        let mut obs = Capture(Vec::new());
        let outcome = m.run(None, &mut obs).unwrap();
        assert_eq!(outcome, Outcome::Stopped(Stop::Guest(pv::GuestExit::PowerOff)));
        assert_eq!(obs.0, b"A");

        let mut written = [0u8; 8];
        m.dev.ram.read(0x1000, &mut written).unwrap();
        assert_eq!(u64::from_le_bytes(written), 0x1234);

        let dirty = m.dirty_pages().unwrap();
        assert!(dirty.contains(&1), "expected page 1 in dirty set: {dirty:?}");
    }

    #[test]
    fn hvf_executes_arm64_guest_mmio_transport() {
        if crate::hvf::kvm::open().is_err() {
            eprintln!("skipped: Hypervisor.framework entitlement not present on test binary");
            return;
        }
        let mut m = Machine::create(&test_config(), DirtyLog::Off).unwrap();
        // Same MMIO guest program as arm64 KVM test:
        //   movz x3, #0x0900, lsl #16      ; x3 = REWIND_MMIO_BASE
        //   movz x0, #'M'
        //   str  x0, [x3, #0x38]           ; MMIO_SERIAL (imm12 = 7)
        //   movz x0, #0
        //   str  x0, [x3, #0x20]           ; MMIO_EXIT (imm12 = 4, PowerOff)
        let code: [u32; 5] = [
            0xd2a1_2003,
            0xd280_09a0,
            0xf900_1c60,
            0xd280_0000,
            0xf900_1060,
        ];
        let entry_off = 0x20000u64;
        for (i, insn) in code.iter().enumerate() {
            m.dev
                .ram
                .write(entry_off + (i * 4) as u64, &insn.to_le_bytes())
                .unwrap();
        }
        crate::hvf::cpu::setup_boot_vcpu(&m.vcpu, RAM_BASE + entry_off, 0).unwrap();

        struct Capture(Vec<u8>);
        impl Observer for Capture {
            fn serial(&mut self, _step: u64, byte: u8) {
                self.0.push(byte);
            }
        }
        let mut obs = Capture(Vec::new());
        let outcome = m.run(None, &mut obs).unwrap();
        assert_eq!(
            outcome,
            Outcome::Stopped(Stop::Guest(pv::GuestExit::PowerOff))
        );
        assert_eq!(obs.0, b"M");
    }

    #[test]
    fn hvf_hardware_and_software_debugging() {
        use crate::debug::{Access, DebugStop, Stepping, Trap};

        if crate::hvf::kvm::open().is_err() {
            eprintln!("skipped: Hypervisor.framework entitlement not present on test binary");
            return;
        }
        let mut m = Machine::create(&test_config(), DirtyLog::On).unwrap();
        // ARM64 program at RAM_BASE + 0x20000:
        //  +0x00: movz x1, #0x4000, lsl #16   ; x1 = RAM_BASE
        //  +0x04: add  x1, x1, #2, lsl #12    ; x1 = RAM_BASE + 0x2000 (watched addr)
        //  +0x08: movz x2, #0x55              ; <-- HW breakpoint
        //  +0x0c: str  x2, [x1]               ; <-- HW write watchpoint (writes 0x55)
        //  +0x10: movz x0, #'D'               ; <-- SW BRK #0 breakpoint
        //  +0x14: hvc  #0x3f8                 ; HVC_SERIAL
        //  +0x18: movz x0, #0
        //  +0x1c: hvc  #0x5f4                 ; HVC_EXIT (PowerOff)
        let code: [u32; 8] = [
            0xd2a8_0001,
            0x9140_0821,
            0xd280_0aa2,
            0xf900_0022,
            0xd280_0880,
            0xd400_0002 | (u32::from(HVC_SERIAL) << 5),
            0xd280_0000,
            0xd400_0002 | (u32::from(HVC_EXIT) << 5),
        ];
        let entry_off = 0x20000u64;
        let entry_pc = RAM_BASE + entry_off;
        for (i, insn) in code.iter().enumerate() {
            m.dev
                .ram
                .write(entry_off + (i * 4) as u64, &insn.to_le_bytes())
                .unwrap();
        }
        crate::hvf::cpu::setup_boot_vcpu(&m.vcpu, entry_pc, 0).unwrap();
        m.dirty.arm_all(&m.vm).unwrap();

        struct Capture(Vec<u8>);
        impl Observer for Capture {
            fn serial(&mut self, _step: u64, byte: u8) {
                self.0.push(byte);
            }
        }
        let mut obs = Capture(Vec::new());

        // 1. Single-step two instructions.
        m.set_debug(Stepping::Yes, &[]).unwrap();
        assert_eq!(m.run(None, &mut obs).unwrap(), Outcome::Debug(DebugStop::Step));
        assert_eq!(m.arm64_registers().unwrap().pc, entry_pc + 4);
        assert_eq!(m.run(None, &mut obs).unwrap(), Outcome::Debug(DebugStop::Step));
        assert_eq!(m.arm64_registers().unwrap().pc, entry_pc + 8);
        assert_eq!(m.arm64_registers().unwrap().x[1], RAM_BASE + 0x2000);

        // 2. Hardware breakpoint at +0x08, hardware write watchpoint at RAM_BASE + 0x2000,
        //    and software BRK #0 breakpoint at +0x10.
        let watch_addr = RAM_BASE + 0x2000;
        let sw_bp_addr = entry_pc + 0x10;
        m.insert_int3(sw_bp_addr).unwrap();
        // Memory read must shadow BRK #0 and return original instruction bytes.
        let mut read_back = [0u8; 4];
        assert_eq!(m.read_virtual(sw_bp_addr, &mut read_back).unwrap(), 4);
        assert_eq!(read_back, code[4].to_le_bytes());

        let traps = [
            Trap::Execute(entry_pc + 8),
            Trap::Watch {
                address: watch_addr,
                len: 8,
                access: Access::Write,
            },
        ];
        m.set_debug(Stepping::No, &traps).unwrap();
        assert_eq!(
            m.run(None, &mut obs).unwrap(),
            Outcome::Debug(DebugStop::Breakpoint(entry_pc + 8))
        );

        // Pass the HW breakpoint and hit the HW write watchpoint (with write already committed!).
        m.pass_breakpoint().unwrap();
        match m.run(None, &mut obs).unwrap() {
            Outcome::Debug(DebugStop::Watchpoint { address, traps }) => {
                assert_eq!(address, watch_addr);
                assert!(traps.contains(1));
            }
            other => panic!("expected Watchpoint, got {other:?}"),
        }
        assert_eq!(m.arm64_registers().unwrap().pc, sw_bp_addr);
        let mut mem = [0u8; 8];
        m.read_physical(watch_addr, &mut mem).unwrap();
        assert_eq!(u64::from_le_bytes(mem), 0x55);

        // 3. Continue into software BRK #0 breakpoint at +0x10, then pass it and run to PowerOff.
        assert_eq!(
            m.run(None, &mut obs).unwrap(),
            Outcome::Debug(DebugStop::Breakpoint(sw_bp_addr))
        );
        m.pass_breakpoint().unwrap();
        assert_eq!(
            m.run(None, &mut obs).unwrap(),
            Outcome::Stopped(Stop::Guest(pv::GuestExit::PowerOff))
        );
        assert_eq!(obs.0, b"D");
    }
}
