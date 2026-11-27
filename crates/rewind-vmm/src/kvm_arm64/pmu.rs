//! Stub performance counter interface for Linux ARM64 KVM.
//!
//! Branch-counter virtual time (`--clock branches`) is x86-64 only; ARM64 KVM
//! uses deterministic exit-quantum virtual time (`--clock exits`).

use anyhow::{Result, bail};

use crate::CounterEvent;

/// Signal used by watchdog/interrupter to interrupt `KVM_RUN`.
pub(crate) fn overflow_signal() -> i32 {
    libc::SIGUSR1
}

/// Installs an empty signal handler for [`overflow_signal`] without
/// `SA_RESTART`, so `pthread_kill` wakes `KVM_RUN` cleanly.
pub(crate) fn install_signal_handler() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        extern "C" fn noop(_: libc::c_int) {}
        // SAFETY: installing an async-signal-safe no-op handler.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = noop as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(overflow_signal(), &action, std::ptr::null_mut());
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modes {
    UserOnly,
    UserAndKernel,
}

#[derive(Debug)]
pub struct Counter;

impl Counter {
    pub fn open(_event: CounterEvent, _modes: Modes) -> Result<Counter> {
        bail!(
            "branch-counter virtual time is not supported on ARM64 KVM; \
             use exit time (--clock exits)"
        )
    }

    pub fn read(&self) -> Result<u64> {
        bail!("branch counters are not supported on ARM64 KVM")
    }

    pub fn pause(&self) -> Result<()> {
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct KindOfCore {
    pub pmu: String,
    pub cpu_list: String,
    pub cpus: Vec<usize>,
}

pub fn kinds_of_core() -> Result<Vec<KindOfCore>> {
    Ok(Vec::new())
}

#[derive(Debug)]
pub struct Pinned;

impl Pinned {
    pub fn to(_cpus: &[usize]) -> Result<Option<Pinned>> {
        Ok(None)
    }
}
