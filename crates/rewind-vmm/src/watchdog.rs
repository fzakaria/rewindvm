//! Stopping a machine at a wall-clock deadline.
//!
//! A guest that computes without exits never returns from KVM_RUN on its
//! own, so a deadline needs another thread: at the deadline it marks the
//! machine expired and signals the vCPU thread, which pulls it out of
//! KVM_RUN to see the mark.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::pmu::{install_signal_handler, overflow_signal};

/// How often the watchdog signals again until its thread has stopped
/// waiting: a signal that lands just before the thread enters KVM_RUN is
/// handled outside it and does not pull it out.
const RESIGNAL: Duration = Duration::from_millis(50);

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
    pub(crate) fn start(deadline: Instant) -> Watchdog {
        install_signal_handler();
        let expired = Arc::new(AtomicBool::new(false));
        // SAFETY: pthread_self has no preconditions.
        let target = unsafe { libc::pthread_self() };
        let (stop, stopped) = mpsc::channel::<()>();
        let mark = expired.clone();
        let thread = std::thread::spawn(move || {
            let wait = deadline.saturating_duration_since(Instant::now());
            if stopped.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
                return;
            }
            mark.store(true, Ordering::Relaxed);
            loop {
                // SAFETY: the target thread is alive until it drops the
                // watchdog, which joins this thread first.
                unsafe { libc::pthread_kill(target, overflow_signal()) };
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
            let watchdog = Watchdog::start(deadline);
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
        assert!(woke < deadline + Duration::from_secs(5));
    }
}
