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
pub mod layout;
pub mod memory;
pub mod pv;
pub mod snapshot;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_MEM_LOG_DIRTY_PAGES, KVM_MEM_READONLY, kvm_msi, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

use layout::*;
use memory::Mapping;
use pv::{Clock, GuestExit};

/// The memory slot of guest RAM, and of the input image.
const SLOT_RAM: u32 = 0;
const SLOT_PMEM: u32 = 1;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The requested step was reached; the machine can run on from here.
    Paused,
    Stopped(Stop),
}

/// Receives what the guest reports, with the step it arrived on.
pub trait Observer {
    /// One record written to the event port, header included.
    fn record(&mut self, _step: u64, _record: &[u8]) {}
    /// One byte written to the early serial console.
    fn serial(&mut self, _step: u64, _byte: u8) {}
}

/// An observer that ignores everything.
pub struct Ignore;
impl Observer for Ignore {}

/// Device state: everything the monitor keeps besides KVM's own.
pub(crate) struct Devices {
    pub ram: Mapping,
    pub clock: Clock,
    pub shared: Option<u64>,
    pub epoch: u64,
    pub step: u64,
    pub stop: Option<Stop>,
}

pub struct Machine {
    pub(crate) kvm: Kvm,
    pub(crate) vm: VmFd,
    pub(crate) vcpu: VcpuFd,
    pub(crate) dev: Devices,
    pub(crate) pmem: Option<Mapping>,
    pub(crate) config: Config,
    /// Exits counted by port and guest instruction pointer, when profiling.
    pub profile: Option<std::collections::HashMap<(u16, u64), u64>>,
}

impl Machine {
    /// Creates the VM with its memory and devices, but no guest state:
    /// either [`Machine::boot`] or a snapshot restore supplies that.
    fn create(config: &Config) -> Result<Machine> {
        if config.mem_bytes > RAM_MAX || config.mem_bytes < (64 << 20) {
            bail!(
                "guest memory must be between 64 MiB and {} MiB",
                RAM_MAX >> 20
            );
        }

        let kvm = Kvm::new().context("opening /dev/kvm")?;
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

        let vcpu = vm.create_vcpu(0).context("creating the vCPU")?;
        vcpu.set_cpuid2(&cpu::cpuid(&kvm)?)?;

        Ok(Machine {
            kvm,
            vm,
            vcpu,
            dev: Devices {
                ram,
                clock: Clock::new(config.quantum),
                shared: None,
                epoch: config.epoch,
                step: 0,
                stop: None,
            },
            pmem,
            config: config.clone(),
            profile: None,
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

    /// Virtual nanoseconds since boot.
    pub fn now(&self) -> u64 {
        self.dev.clock.now
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Runs until the guest stops, or until step `until` if given.
    pub fn run(&mut self, until: Option<u64>, obs: &mut dyn Observer) -> Result<Outcome> {
        if let Some(stop) = self.dev.stop {
            return Ok(Outcome::Stopped(stop));
        }
        if until.is_some_and(|u| u <= self.dev.step) {
            return Ok(Outcome::Paused);
        }

        loop {
            // The port of a counted exit; MMIO exits count under MMIO_PORT.
            const MMIO_PORT: u16 = 0xffff;
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
                    // A signal to the monitor thread: not the guest's doing,
                    // so not a step.
                    VcpuExit::Intr => None,
                    VcpuExit::Hlt => bail!("the guest executed HLT; is it a Rewind kernel?"),
                    other => bail!("unexpected exit at step {}: {other:?}", self.dev.step),
                },
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => None,
                Err(e) => return Err(e).context("KVM_RUN"),
            };

            if let Some(stop) = self.dev.stop {
                return Ok(Outcome::Stopped(stop));
            }
            let Some(port) = counted else {
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

            self.dev.clock.tick();
            if self.dev.clock.due() && self.inject()? {
                self.dev.clock.deadline = None;
            }

            if until.is_some_and(|u| self.dev.step >= u) {
                return Ok(Outcome::Paused);
            }
        }
    }

    /// Sends the timer interrupt as an MSI to the one local APIC. False when
    /// the APIC is not accepting interrupts yet; the timer stays due and is
    /// sent again at the next exit.
    fn inject(&self) -> Result<bool> {
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
                    bail!("guest record at step {} has length {len}", self.step);
                }
                let mut record = vec![0u8; len];
                self.ram.read(value as u64, &mut record)?;
                obs.record(self.step, &record);
            }
            pv::PORT_TIMER => self.clock.arm(value as u64),
            pv::PORT_IDLE => {
                if !self.clock.idle() {
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
                let shared = self.shared.context("guest read the clock before setup")?;
                let now = self.clock.now;
                self.ram
                    .write(shared + pv::SHARED_NOW, &now.to_le_bytes())?;
                let bytes = (now as u32).to_le_bytes();
                data.copy_from_slice(&bytes[..data.len()]);
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
