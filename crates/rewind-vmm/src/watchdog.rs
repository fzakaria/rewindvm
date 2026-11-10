//! Stopping a machine at a wall-clock deadline.
//!
//! A guest that computes without exits never returns from KVM_RUN on its
//! own, so a deadline needs another thread: at the deadline it marks the
//! machine expired and signals the vCPU thread, which pulls it out of
//! KVM_RUN to see the mark.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::pmu::{install_signal_handler, overflow_signal};

/// How often the watchdog signals again until its thread has stopped
/// waiting: a signal that lands just before the thread enters KVM_RUN is
/// handled outside it and does not pull it out.
const RESIGNAL: Duration = Duration::from_millis(50);

/// How often a watchdog whose limit is not set yet looks again.
const UNSET_POLL: Duration = Duration::from_millis(100);

/// A time limit another thread decides after the runs it bounds have
/// started, shared with them. Until it is set, a run under it has no
/// deadline. The first limit set is the one that holds.
#[derive(Clone, Debug, Default)]
pub struct LateLimit(Arc<OnceLock<Duration>>);

impl LateLimit {
    pub fn new() -> LateLimit {
        LateLimit::default()
    }

    /// Sets the limit, unless an earlier call set it.
    pub fn set(&self, limit: Duration) {
        let _ = self.0.set(limit);
    }

    /// The limit, once set.
    pub fn get(&self) -> Option<Duration> {
        self.0.get().copied()
    }
}

/// When a machine's run stops as timed out.
#[derive(Clone, Debug)]
pub enum Deadline {
    /// At this instant.
    At(Instant),
    /// `limit` after `from`, once another thread sets the limit.
    Late { from: Instant, limit: LateLimit },
}

impl Deadline {
    /// The instant the run stops at, once known.
    fn when(&self) -> Option<Instant> {
        match self {
            Deadline::At(at) => Some(*at),
            Deadline::Late { from, limit } => limit.get().map(|l| *from + l),
        }
    }
}

/// A thread to signal. pthread_t is a number with glibc and a pointer with
/// musl, which the static release build uses, and a pointer is not Send.
struct Target(libc::pthread_t);

// SAFETY: a pthread_t only names a thread; the watchdog passes it to
// pthread_kill and never dereferences it.
unsafe impl Send for Target {}

impl Target {
    /// Sends the thread the signal that pulls it out of KVM_RUN. A method
    /// rather than a field access, so a closure that calls it captures the
    /// whole Target, which is Send, and not the pthread_t inside.
    fn signal(&self) {
        // SAFETY: the target thread is alive until it drops the watchdog,
        // which joins the watchdog's thread first.
        unsafe { libc::pthread_kill(self.0, overflow_signal()) };
    }
}

/// Signals the thread that started it once a deadline passes, until
/// dropped. Dropping it, which the thread does once it has stopped
/// waiting, ends the signals.
pub(crate) struct Watchdog {
    expired: Arc<AtomicBool>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Watches the calling thread until `deadline`.
    pub(crate) fn start(deadline: Deadline) -> Watchdog {
        install_signal_handler();
        let expired = Arc::new(AtomicBool::new(false));
        // SAFETY: pthread_self has no preconditions.
        let target = Target(unsafe { libc::pthread_self() });
        let (stop, stopped) = mpsc::channel::<()>();
        let mark = expired.clone();
        let thread = std::thread::spawn(move || {
            // To the deadline, looking again every UNSET_POLL while a late
            // limit is not set.
            loop {
                let now = Instant::now();
                let wait = match deadline.when() {
                    Some(at) if at <= now => break,
                    Some(at) => at - now,
                    None => UNSET_POLL,
                };
                if stopped.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
                    return;
                }
            }
            mark.store(true, Ordering::Relaxed);
            loop {
                target.signal();
                if stopped.recv_timeout(RESIGNAL) != Err(RecvTimeoutError::Timeout) {
                    return;
                }
            }
        });
        Watchdog {
            expired,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Set once the deadline has passed.
    pub(crate) fn expired(&self) -> Arc<AtomicBool> {
        self.expired.clone()
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    // A thread blocked in pause(), as the vCPU thread is in KVM_RUN, sees
    // the watchdog's mark once the deadline passes, and not before.
    use super::*;

    #[test]
    fn a_blocked_thread_is_woken_at_the_deadline() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let blocked = std::thread::spawn(move || {
            let watchdog = Watchdog::start(Deadline::At(deadline));
            let expired = watchdog.expired();
            assert!(!expired.load(Ordering::Relaxed));
            while !expired.load(Ordering::Relaxed) {
                // SAFETY: pause has no preconditions; a signal ends it.
                unsafe { libc::pause() };
            }
            Instant::now()
        });
        let woke = blocked.join().unwrap();
        assert!(woke >= deadline);
        // Generous, so a loaded build machine does not fail it; a thread the
        // watchdog never woke would not get here at all.
        assert!(woke < deadline + Duration::from_secs(60));
    }

    // A watchdog whose limit is not set yet leaves its thread alone. Once
    // another thread sets the limit, the watchdog wakes its thread at the
    // limit from the instant the deadline counts from, and not before.
    #[test]
    fn a_limit_set_later_wakes_the_thread_at_that_limit() {
        const UNSET_FOR: Duration = Duration::from_millis(300);
        const LIMIT: Duration = Duration::from_millis(500);
        let from = Instant::now();
        let limit = LateLimit::new();
        let deadline = Deadline::Late {
            from,
            limit: limit.clone(),
        };
        let blocked = std::thread::spawn(move || {
            let watchdog = Watchdog::start(deadline);
            let expired = watchdog.expired();
            while !expired.load(Ordering::Relaxed) {
                // SAFETY: pause has no preconditions; a signal ends it.
                unsafe { libc::pause() };
            }
            Instant::now()
        });

        // Unset, the limit wakes no one.
        std::thread::sleep(UNSET_FOR);
        assert!(!blocked.is_finished());

        // Set, the limit counts from `from`, not from when it was set.
        limit.set(LIMIT);
        let woke = blocked.join().unwrap();
        assert!(woke >= from + LIMIT);
        assert!(woke < from + LIMIT + Duration::from_secs(60));
    }
}
