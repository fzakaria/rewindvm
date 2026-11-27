//! Deterministic ARM64 virtual machine monitor on Linux KVM.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_ARM_IRQ_CPU_IRQ, KVM_ARM_IRQ_TYPE_CPU, KVM_ARM_IRQ_TYPE_SHIFT, KVM_ARM_IRQ_VCPU_SHIFT,
    KVM_MEM_LOG_DIRTY_PAGES, KVM_MEM_READONLY, KVM_VM_TYPE_ARM_IPA_SIZE_MASK, kvm_vcpu_init,
    kvm_userspace_memory_region,
};
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd, VmFd};

use crate::arm64::boot::{self, BootSpec};
use crate::arm64::layout::*;
use crate::kvm_arm64::cpu;
use crate::memory::Mapping;
use crate::pv::{self, Clock, GuestExit};
use crate::{
    BootInto, Config, CpuMode, Devices, DirtyLog, Input, Interrupter, Observer, Origin, Outcome,
    Stall, Stop, checkpoint, interrupt, watchdog,
};

const SLOT_RAM: u32 = 0;
const SLOT_PMEM: u32 = 1;
const SLOT_EXTRAS: u32 = 2;

const PMEM_ALIGN: usize = 2 << 20;

/// Virtual IRQ line targeting vCPU 0's `HCR_EL2.VI` without requiring a GIC.
pub(crate) const ARM_CPU_IRQ: u32 =
    (KVM_ARM_IRQ_TYPE_CPU << KVM_ARM_IRQ_TYPE_SHIFT)
        | (0 << KVM_ARM_IRQ_VCPU_SHIFT)
        | KVM_ARM_IRQ_CPU_IRQ;

/// Whether a run on this ARM64 Linux host reserves the extras slot.
pub fn extras_for(cpu_model: crate::CpuModel) -> Result<crate::Extras> {
    let kvm = crate::kvm_arm64::kvm::open()?;
    let bits = cpu::address_bits(&kvm)?;
    if bits >= EXTRAS_ADDRESS_BITS {
        return Ok(crate::Extras::Reserved);
    }
    match cpu_model {
        crate::CpuModel::Host => Ok(crate::Extras::Absent),
        _ => bail!(
            "this ARM64 host reaches {bits} IPA bits, where the extras slot needs \
             {EXTRAS_ADDRESS_BITS}"
        ),
    }
}

pub struct Machine {
    // Field order matters: vCPU and VM must drop before host memory mappings.
    pub(crate) vcpu: VcpuFd,
    pub(crate) vm: VmFd,
    pub(crate) _kvm: Kvm,
    pub(crate) dev: Devices,
    pub(crate) pmem: Option<Mapping>,
    pub(crate) extras: Option<Mapping>,
    pub(crate) config: Config,
    pub(crate) dirty_log: DirtyLog,
    pub(crate) irq_pending: bool,
    pub(crate) pending_exit: bool,
    pub profile: Option<std::collections::HashMap<(u16, u64), u64>>,
    pub pmu: Option<(crate::pmu::Counter, u64)>,
    pub(crate) work_base: u64,
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

fn map_region(vm: &VmFd, slot: u32, gpa: u64, mapping: &Mapping, flags: u32) -> Result<()> {
    let region = kvm_userspace_memory_region {
        slot,
        flags,
        guest_phys_addr: gpa,
        memory_size: mapping.len() as u64,
        userspace_addr: mapping.host_addr(),
    };
    // SAFETY: `mapping` is owned by `Machine` and outlives `vm`.
    unsafe { vm.set_user_memory_region(region) }
        .with_context(|| format!("mapping KVM slot {slot} at {gpa:#x}"))
}

impl Machine {
    pub(crate) fn create(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        if config.mem_bytes > MEM_MAX || config.mem_bytes < (64 << 20) {
            bail!("VM memory must be between 64 MiB and {} MiB", MEM_MAX >> 20);
        }
        if matches!(config.clock, crate::ClockSource::Branches(_)) {
            bail!(
                "branch-counter virtual time is not supported on ARM64 KVM; use `--clock exits`"
            );
        }
        if config.preemption == crate::Preemption::AtBranchCounts {
            bail!("branch-count preemption is not supported on ARM64 KVM");
        }

        // Pin to homogeneous cores (e.g. P-cores on Apple Silicon Linux) before
        // querying KVM_ARM_PREFERRED_TARGET so MIDR_EL1 is deterministic.
        cpu::pin_homogeneous_cores();

        let kvm = crate::kvm_arm64::kvm::open()?;
        let max_ipa = kvm.check_extension_int(Cap::ArmVmIPASize);
        let ipa_bits = if max_ipa > 0 {
            (max_ipa as u32).min(40)
        } else {
            40
        };
        let vm = if max_ipa > 0 {
            kvm.create_vm_with_type(u64::from(ipa_bits & KVM_VM_TYPE_ARM_IPA_SIZE_MASK))
                .with_context(|| format!("creating ARM64 KVM VM ({ipa_bits}-bit IPA)"))?
        } else {
            kvm.create_vm().context("creating ARM64 KVM VM")?
        };

        let ram_len = (ram_span(config.mem_bytes) as usize).next_multiple_of(HOST_PAGE_SIZE);
        let ram = Mapping::anonymous(ram_len)?;
        let _advice = ram.prefer_huge_pages();
        let ram_flags = match dirty_log {
            DirtyLog::On => KVM_MEM_LOG_DIRTY_PAGES,
            DirtyLog::Off => 0,
        };
        map_region(&vm, SLOT_RAM, RAM_BASE, &ram, ram_flags)?;

        let pmem = match &config.image {
            Some(path) => {
                let image = Mapping::file(path, PMEM_ALIGN)?;
                let start = pmem_start(ram.len() as u64);
                if start + image.len() as u64 > EXTRAS_START {
                    bail!("the image does not fit below the extras slot");
                }
                map_region(&vm, SLOT_PMEM, start, &image, KVM_MEM_READONLY)?;
                Some(image)
            }
            None => None,
        };

        let extras = match config.extras {
            crate::Extras::Absent => None,
            crate::Extras::Reserved => {
                if ipa_bits < EXTRAS_ADDRESS_BITS {
                    bail!(
                        "this run reserves the extras slot ({EXTRAS_ADDRESS_BITS} IPA bits), \
                         and this host reaches {ipa_bits}"
                    );
                }
                let slot = Mapping::anonymous(EXTRAS_LEN as usize)?;
                map_region(&vm, SLOT_EXTRAS, EXTRAS_START, &slot, KVM_MEM_READONLY)?;
                Some(slot)
            }
        };

        let mut vcpu = vm.create_vcpu(0).context("creating ARM64 KVM vCPU 0")?;
        let mut kvi = kvm_vcpu_init::default();
        vm.get_preferred_target(&mut kvi)
            .context("KVM_ARM_PREFERRED_TARGET")?;
        // Keep feature bits clean (no PMU_V3, no SVE) for determinism.
        kvi.features = [0; 7];
        vcpu.vcpu_init(&kvi).context("KVM_ARM_VCPU_INIT")?;

        let immediate_exit = &raw mut vcpu.get_kvm_run().immediate_exit;
        let interrupt = interrupt::Shared::new(immediate_exit);

        Ok(Machine {
            vcpu,
            vm,
            _kvm: kvm,
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
            },
            pmem,
            extras,
            config: config.clone(),
            dirty_log,
            irq_pending: false,
            pending_exit: false,
            profile: None,
            pmu: None,
            work_base: 0,
            int3s: crate::debug::Int3s::default(),
            deadline: None,
            last_exit: std::time::Instant::now(),
            origin: Origin::Boot,
            checkpoints: checkpoint::Checkpoints::default(),
            interrupt,
        })
    }

    /// Boots an ARM64 Linux kernel `Image` on Linux KVM at step 0.
    pub fn boot(config: &Config, dirty_log: DirtyLog) -> Result<Machine> {
        let mut m = Self::create(config, dirty_log)?;
        let addrs = m.write_boot(BootInto::Ram)?;
        cpu::setup_boot_vcpu(&m.vcpu, addrs.kernel_entry, addrs.dtb_gpa)?;
        if m.dirty_log == DirtyLog::On {
            let _ = m.drain_dirty_log()?;
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

    /// Reads and clears the KVM dirty log, expanding host pages (4 KiB or
    /// 16 KiB on Apple Silicon Linux) into 4 KiB guest page indices relative to
    /// `RAM_BASE`.
    pub(crate) fn drain_dirty_log(&mut self) -> Result<Vec<usize>> {
        // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions or side effects.
        let host_page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
            n if n > 0 => n as usize,
            _ => PAGE_SIZE,
        };
        let guest_per_host = (host_page / PAGE_SIZE).max(1);
        let max_guest_pages = self.dev.ram.len() / PAGE_SIZE;
        let host_pages = self.dev.ram.len().div_ceil(host_page);
        let dirty = self
            .vm
            .get_dirty_log(SLOT_RAM, self.dev.ram.len())
            .context("reading the ARM64 KVM dirty log")?;
        let mut pages = Vec::new();
        for (word_idx, &word) in dirty.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let h = word_idx * 64 + bits.trailing_zeros() as usize;
                if h < host_pages {
                    let first = h * guest_per_host;
                    let last = (first + guest_per_host).min(max_guest_pages);
                    pages.extend(first..last);
                }
                bits &= bits - 1;
            }
        }
        Ok(pages)
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
        let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let mode = if (pstate & 0xf) == 0 {
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
        cpu::pin_homogeneous_cores();
        self.interrupt.entered();
        let outcome = self.run_loop(until, obs);
        self.interrupt.left();
        outcome
    }

    fn run_loop(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
        const MMIO_PORT: u16 = 0xffff;

        if let Some(stop) = self.dev.stop {
            return Ok(Outcome::Stopped(stop));
        }
        if until.is_some_and(|u| u <= self.dev.step) {
            return Ok(Outcome::Paused);
        }

        let watchdog = self.deadline.clone().map(watchdog::Watchdog::start);
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

            let exit = match self.vcpu.run() {
                Ok(exit) => exit,
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(e).context("KVM_RUN"),
            };

            let counted = match exit {
                VcpuExit::Intr => None,
                VcpuExit::MmioWrite(addr, data) => {
                    self.pending_exit = true;
                    if (REWIND_MMIO_BASE..REWIND_MMIO_BASE + REWIND_MMIO_SIZE).contains(&addr) {
                        let offset = (addr - REWIND_MMIO_BASE) as u16;
                        let Some(port) = mmio_offset_to_port(offset) else {
                            bail!(
                                "unknown Rewind MMIO write +{offset:#x} at step {}",
                                self.dev.step
                            );
                        };
                        if port == pv::HVC_IRQ_ACK {
                            self.irq_pending = false;
                            self.vm
                                .set_irq_line(ARM_CPU_IRQ, false)
                                .context("deasserting ARM64 virtual IRQ")?;
                            None
                        } else {
                            let val = read_le_u64(data);
                            self.dev.step += 1;
                            self.dev.drain_io(obs)?;
                            if port == pv::PORT_COM1 {
                                obs.serial(self.dev.step, val as u8);
                            } else {
                                self.dev.io_out_u64(port, val, obs)?;
                            }
                            Some(port)
                        }
                    } else {
                        // Unmapped MMIO write: ignore.
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        Some(MMIO_PORT)
                    }
                }
                VcpuExit::MmioRead(addr, data) => {
                    self.pending_exit = true;
                    if (REWIND_MMIO_BASE..REWIND_MMIO_BASE + REWIND_MMIO_SIZE).contains(&addr) {
                        let offset = (addr - REWIND_MMIO_BASE) as u16;
                        let Some(port) = mmio_offset_to_port(offset) else {
                            bail!(
                                "unknown Rewind MMIO read +{offset:#x} at step {}",
                                self.dev.step
                            );
                        };
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        if port == pv::PORT_CLOCK {
                            let shared = self
                                .dev
                                .shared
                                .context("the VM read the clock before setup")?;
                            let now = self.dev.clock.now;
                            self.dev
                                .ram_write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;
                            write_le_u64(data, now);
                        } else {
                            data.fill(0);
                        }
                        Some(port)
                    } else {
                        data.fill(0);
                        self.dev.step += 1;
                        self.dev.drain_io(obs)?;
                        Some(MMIO_PORT)
                    }
                }
                VcpuExit::SystemEvent(kind, _flags) => {
                    // KVM_SYSTEM_EVENT_SHUTDOWN = 1, KVM_SYSTEM_EVENT_RESET = 2.
                    let guest_exit = if kind == 2 {
                        GuestExit::Restart
                    } else {
                        GuestExit::PowerOff
                    };
                    self.dev.stop = Some(Stop::Guest(guest_exit));
                    None
                }
                other => {
                    bail!("unexpected ARM64 KVM exit {other:?} at step {}", self.dev.step);
                }
            };

            if let Some(stop) = self.dev.stop {
                self.dev.end_trace(stop, obs)?;
                return Ok(Outcome::Stopped(stop));
            }
            if counted.is_some() && self.deadline.is_some() {
                self.last_exit = std::time::Instant::now();
            }
            let Some(port) = counted else {
                if self.interrupt.take() {
                    return Ok(Outcome::Interrupted);
                }
                continue;
            };
            if let Some(profile) = &mut self.profile {
                let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
                *profile.entry((port, pc)).or_default() += 1;
            }
            self.after_step()?;
            self.dev.write_event(obs);

            if until.is_some_and(|u| self.dev.step >= u) || obs.stop() {
                return Ok(Outcome::Paused);
            }
        }
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
        if reasons != 0 && self.inject(reasons)? {
            if reasons & pv::PENDING_TIMER != 0 {
                self.dev.clock.deadline = None;
            }
            self.dev.inspect = false;
            self.dev.input_sent = false;
        }
        Ok(())
    }

    /// Maps `image` into the extras slot.
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
            bail!(
                "mapping {}: {}",
                image.display(),
                std::io::Error::last_os_error()
            );
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
        self.vm
            .set_irq_line(ARM_CPU_IRQ, true)
            .context("asserting ARM64 virtual IRQ")?;
        self.irq_pending = true;
        self.dev.idle = false;
        Ok(true)
    }
}

fn read_le_u64(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = data.len().min(8);
    bytes[..n].copy_from_slice(&data[..n]);
    u64::from_le_bytes(bytes)
}

fn write_le_u64(data: &mut [u8], value: u64) {
    let bytes = value.to_le_bytes();
    let n = data.len().min(8);
    data[..n].copy_from_slice(&bytes[..n]);
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
        }
    }

    #[test]
    fn arm64_kvm_executes_guest_mmio_and_state_roundtrip() {
        if crate::kvm_arm64::kvm::open().is_err() {
            eprintln!("skipped: /dev/kvm does not open");
            return;
        }
        let mut m = Machine::create(&test_config(), DirtyLog::On).unwrap();
        // Tiny ARM64 program at RAM_BASE + 0x20000:
        //   movz x1, #0x4000, lsl #16      ; x1 = 0x4000_0000 (RAM_BASE)
        //   add  x1, x1, #1, lsl #12       ; x1 = 0x4000_1000 (page 1)
        //   movz x2, #0x1234
        //   str  x2, [x1]                  ; dirty page 1
        //   movz x3, #0x0900, lsl #16      ; x3 = REWIND_MMIO_BASE (0x0900_0000)
        //   movz x0, #'A'
        //   str  x0, [x3, #0x38]           ; MMIO_SERIAL (imm12 = 7)
        //   movz x0, #0
        //   str  x0, [x3, #0x20]           ; MMIO_EXIT (imm12 = 4, PowerOff)
        let code: [u32; 9] = [
            0xd2a8_0001,
            0x9140_0421,
            0xd282_4682,
            0xf900_0022,
            0xd2a1_2003,
            0xd280_0820,
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
        cpu::setup_boot_vcpu(&m.vcpu, RAM_BASE + entry_off, 0).unwrap();
        let _ = m.drain_dirty_log().unwrap();

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
            Outcome::Stopped(Stop::Guest(GuestExit::PowerOff))
        );
        assert_eq!(obs.0, b"A");

        let mut written = [0u8; 8];
        m.dev.ram.read(0x1000, &mut written).unwrap();
        assert_eq!(u64::from_le_bytes(written), 0x1234);

        let dirty = m.dirty_pages().unwrap();
        assert!(dirty.contains(&1), "expected page 1 in dirty set: {dirty:?}");

        m.complete_pending_exit().unwrap();
        let (cpu_state, irqchips, devices) = m.state().unwrap();
        m.apply_state(&cpu_state, &irqchips, &devices).unwrap();
    }
}
