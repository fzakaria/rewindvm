//! Counting the guest's work with the host's performance counters.
//!
//! Virtual time that moves only at exits makes computation free: a thread
//! that computes for a millisecond between two exits sees no time pass,
//! and neither does the scheduler. A hardware counter of retired branches,
//! restricted to guest mode, measures the work between two exits, and
//! because it is read only at exits, which are fixed points in the guest's
//! instruction stream, the count read there is the same on every run, as
//! long as the counter itself is exact. rr relies on the same property of
//! the same counter.

use std::io;

use anyhow::{Context, Result, bail};

/// perf_event_attr, as far as this monitor uses it.
#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved_2: u16,
    aux_sample_size: u32,
    reserved_3: u32,
    sig_data: u64,
    config3: u64,
}

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_RAW: u32 = 4;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;

/// Flag bits in perf_event_attr.flags.
const FLAG_DISABLED: u64 = 1 << 0;
const FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
const FLAG_EXCLUDE_HV: u64 = 1 << 6;
const FLAG_EXCLUDE_HOST: u64 = 1 << 19;

const PERF_EVENT_IOC_ENABLE: u64 = 0x2400;

/// Which events to count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// AMD Zen's retired conditional branches (PMCx0D1), what rr counts on
    /// AMD.
    AmdRetiredConditionalBranches,
    /// Retired instructions, which some microarchitectures overcount.
    Instructions,
}

/// Whether guest kernel mode is counted too. Counting it needs
/// perf_event_paranoid at 1 or lower, or CAP_PERFMON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modes {
    UserOnly,
    UserAndKernel,
}

pub struct Counter {
    fd: i32,
}

impl Counter {
    /// A counter of guest-mode events on the calling thread, which must be
    /// the vCPU's.
    pub fn open(event: Event, modes: Modes) -> Result<Counter> {
        let (type_, config) = match event {
            Event::AmdRetiredConditionalBranches => (PERF_TYPE_RAW, 0xd1),
            Event::Instructions => (PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS),
        };
        let mut flags = FLAG_DISABLED | FLAG_EXCLUDE_HV | FLAG_EXCLUDE_HOST;
        if modes == Modes::UserOnly {
            flags |= FLAG_EXCLUDE_KERNEL;
        }
        let attr = PerfEventAttr {
            type_,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config,
            flags,
            ..Default::default()
        };
        // SAFETY: a valid attr for the duration of the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                0,  // this thread
                -1, // any CPU
                -1, // no group
                0,
            )
        } as i32;
        if fd < 0 {
            let err = io::Error::last_os_error();
            bail!("perf_event_open for {event:?}: {err}; see /proc/sys/kernel/perf_event_paranoid");
        }
        // SAFETY: a counter fd we own.
        if unsafe { libc::ioctl(fd, PERF_EVENT_IOC_ENABLE as _, 0) } != 0 {
            bail!("enabling the counter: {}", io::Error::last_os_error());
        }
        Ok(Counter { fd })
    }

    pub fn read(&self) -> Result<u64> {
        let mut value = 0u64;
        // SAFETY: reading eight bytes into a u64.
        let n = unsafe { libc::read(self.fd, (&mut value as *mut u64).cast(), 8) };
        if n != 8 {
            return Err(io::Error::last_os_error()).context("reading the counter");
        }
        Ok(value)
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        // SAFETY: closing an fd we own.
        unsafe { libc::close(self.fd) };
    }
}
