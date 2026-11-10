//! Stopping a running machine from another thread.
//!
//! A guest that computes without exits never returns from KVM_RUN on its
//! own, and the run loop looks for anything but an exit only between two
//! of them. An [`Interrupter`] asks the loop to stop and makes KVM_RUN
//! return at once: the `immediate_exit` flag in the vCPU's kvm_run page
//! makes a KVM_RUN entered after it was set return with EINTR, and a
//! signal to the vCPU thread pulls one already in the guest out the same
//! way. The two together close the gap between the thread deciding to
//! enter the guest and entering it. The interruption is no exit of the
//! guest's: the vCPU stops between two instructions, nothing is recorded,
//! and the next run goes on from exactly there.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::pmu::{install_signal_handler, overflow_signal};

/// The machines this process has, from their making to their dropping,
/// for a stop asked of the whole process ([`stop_all`]).
static MACHINES: Mutex<Vec<Arc<Shared>>> = Mutex::new(Vec::new());

/// Whether a stop was asked of the whole process: every run that begins
/// after is stopped as it begins.
static STOP_ASKED: AtomicBool = AtomicBool::new(false);

/// The write end of the pipe a signal handler tells [`stop_on_signal`]'s
/// thread through; -1 before the handler is installed.
static STOP_PIPE: AtomicI32 = AtomicI32::new(NO_PIPE);
const NO_PIPE: i32 = -1;

/// The vCPU's `immediate_exit` byte, written from any thread. Null once
/// the machine, whose kvm_run page the byte is in, is gone.
struct ImmediateExit(*mut u8);

// SAFETY: the pointer names a byte of the vCPU's kvm_run page, which stays
// mapped until the machine is dropped, and the machine nulls the pointer
// first; every access goes through the mutex that holds it, as an atomic.
unsafe impl Send for ImmediateExit {}

impl ImmediateExit {
    fn store(&self, value: u8) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: the pointer is valid while it is not null, which the
        // holder checked under the lock, and the byte is only ever accessed
        // atomically from here.
        unsafe { AtomicU8::from_ptr(self.0) }.store(value, Ordering::SeqCst);
    }
}

/// What an interrupter and its machine share.
pub(crate) struct Shared {
    /// Set by an interruption, taken by the run loop.
    asked: AtomicBool,
    immediate_exit: Mutex<ImmediateExit>,
    /// The thread running the vCPU, as a pthread_t, while one is in
    /// [`crate::Machine::run`]; 0 when none is.
    thread: AtomicUsize,
}

/// No thread.
const NO_THREAD: usize = 0;

impl Shared {
    /// What a machine about to be made shares with its interrupters,
    /// noted among the process's machines.
    pub(crate) fn new(immediate_exit: *mut u8) -> Arc<Shared> {
        let shared = Arc::new(Shared {
            asked: AtomicBool::new(STOP_ASKED.load(Ordering::SeqCst)),
            immediate_exit: Mutex::new(ImmediateExit(immediate_exit)),
            thread: AtomicUsize::new(NO_THREAD),
        });
        let mut machines = MACHINES.lock().unwrap_or_else(|e| e.into_inner());
        machines.push(shared.clone());
        shared
    }

    /// Sets or clears the vCPU's `immediate_exit`.
    pub(crate) fn set_immediate_exit(&self, value: u8) {
        let byte = self
            .immediate_exit
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        byte.store(value);
    }

    /// Forgets the vCPU's `immediate_exit` byte, before its page goes,
    /// and the machine among the process's.
    pub(crate) fn forget(self: &Arc<Shared>) {
        let mut machines = MACHINES.lock().unwrap_or_else(|e| e.into_inner());
        machines.retain(|shared| !Arc::ptr_eq(shared, self));
        drop(machines);
        let mut byte = self
            .immediate_exit
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        byte.0 = std::ptr::null_mut();
    }

    /// Notes the calling thread as the one running the vCPU. A stop asked
    /// of the whole process stops the run as it begins.
    pub(crate) fn entered(&self) {
        // SAFETY: pthread_self has no preconditions.
        let thread = unsafe { libc::pthread_self() };
        self.thread.store(thread as usize, Ordering::SeqCst);
        if STOP_ASKED.load(Ordering::SeqCst) {
            self.asked.store(true, Ordering::SeqCst);
        }
    }

    /// Notes that no thread runs the vCPU.
    pub(crate) fn left(&self) {
        self.thread.store(NO_THREAD, Ordering::SeqCst);
    }

    /// Whether an interruption was asked for since the last take, taking
    /// it, with `immediate_exit` cleared first so that an interruption
    /// asked for between the two is still seen by the next KVM_RUN.
    pub(crate) fn take(&self) -> bool {
        self.set_immediate_exit(0);
        self.asked.swap(false, Ordering::SeqCst)
    }
}

/// A way to stop a machine's run from another thread, at the next
/// instruction boundary, from [`crate::Machine::interrupter`]. Cloned
/// freely; outlives the machine harmlessly.
#[derive(Clone)]
pub struct Interrupter(pub(crate) Arc<Shared>);

impl std::fmt::Debug for Interrupter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interrupter")
            .field("asked", &self.0.asked.load(Ordering::Relaxed))
            .finish()
    }
}

impl Interrupter {
    /// Stops the machine's run, now if one is under way, else the next
    /// one as it begins: [`crate::Machine::run`] returns
    /// [`crate::Outcome::Interrupted`]. The ask comes first, then the
    /// flag KVM_RUN returns at once for, then the signal that pulls the
    /// vCPU thread out of the guest; the run loop clears the flag before
    /// looking at the ask, so no interruption is lost between the two.
    pub fn interrupt(&self) {
        install_signal_handler();
        self.0.asked.store(true, Ordering::SeqCst);
        self.0.set_immediate_exit(1);
        let thread = self.0.thread.load(Ordering::SeqCst);
        if thread == NO_THREAD {
            return;
        }
        // SAFETY: the thread noted itself as running the vCPU, and clears
        // the note before it leaves the run; a thread that runs a machine
        // outlives the threads that interrupt it, as the watchdog's and
        // a debugger's connection watch are joined by the thread itself.
        unsafe { libc::pthread_kill(thread as libc::pthread_t, overflow_signal()) };
    }
}

/// A descriptor, such as a debugger's connection, watched on a thread of
/// its own: the thread interrupts a machine as soon as the descriptor has
/// something to read or closes, without reading it or changing its
/// flags, since the thread serving the debugger reads it. Dropping the
/// watch wakes the thread through a pipe of its own and waits for it, so
/// ending a watch costs no poll interval.
pub struct FdWatch {
    /// The pipe's write end, closed to wake the thread.
    wake: Option<std::os::fd::OwnedFd>,
    /// The thread, which says whether it interrupted the machine.
    thread: Option<std::thread::JoinHandle<bool>>,
    /// The machine's interrupter, whose ask the watch takes back when it
    /// ends: an ask the run did not take, as when the descriptor became
    /// readable just after the run stopped for something else, would
    /// stop the machine's next run at once.
    interrupter: Interrupter,
}

impl std::fmt::Debug for FdWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FdWatch").finish_non_exhaustive()
    }
}

/// The two ends of a pipe, made close-on-exec.
fn pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: a two-element array, as pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 made both descriptors, and nothing else owns them.
    Ok(unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    })
}

impl FdWatch {
    /// Watches `fd`, which the caller keeps open until the watch is
    /// dropped, interrupting the machine `interrupter` belongs to. With no
    /// pipe to be had, nothing is watched.
    pub fn start(fd: std::os::fd::RawFd, interrupter: Interrupter) -> FdWatch {
        use std::os::fd::AsRawFd;
        let Ok((woken, wake)) = pipe() else {
            return FdWatch {
                wake: None,
                thread: None,
                interrupter,
            };
        };
        let watcher = interrupter.clone();
        let thread = std::thread::spawn(move || {
            let mut fds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: woken.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            const FOREVER: i32 = -1;
            loop {
                // SAFETY: two pollfds, as the count says, which poll only
                // reads and writes for the length of the call; both
                // descriptors are open until the thread is joined.
                let ready =
                    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, FOREVER) };
                if ready < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break;
            }

            // The watch ending wakes the thread with nothing to read.
            if fds[1].revents != 0 || fds[0].revents == 0 {
                return false;
            }
            watcher.interrupt();
            true
        });
        FdWatch {
            wake: Some(wake),
            thread: Some(thread),
            interrupter,
        }
    }
}

impl Drop for FdWatch {
    fn drop(&mut self) {
        drop(self.wake.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        if thread.join().unwrap_or(false) {
            self.interrupter.0.take();
        }
    }
}

/// Stops every machine this process has, between two instructions, and
/// every run that begins after: each returns
/// [`crate::Outcome::Interrupted`]. Returns how many machines there were.
pub fn stop_all() -> usize {
    STOP_ASKED.store(true, Ordering::SeqCst);
    let machines = MACHINES.lock().unwrap_or_else(|e| e.into_inner());
    for shared in machines.iter() {
        Interrupter(shared.clone()).interrupt();
    }
    machines.len()
}

/// Has `signal`, such as SIGTERM, stop every machine this process has
/// ([`stop_all`]) in place of ending the process at once, so a command
/// stopped by the desktop app's Cancel ends the way an interrupted run
/// does. A process with no machine, or still alive `grace` after the
/// signal, as one restoring a machine from many pages can be, is ended
/// with `exit_code`, as the signal would have ended it. The handler
/// writes a byte to a pipe, which is all a handler may do; a thread of
/// its own does the rest.
pub fn stop_on_signal(signal: i32, grace: Duration, exit_code: i32) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: a two-element array, as pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let [read, write] = fds;
    STOP_PIPE.store(write, Ordering::SeqCst);

    extern "C" fn tell(_: i32) {
        let fd = STOP_PIPE.load(Ordering::SeqCst);
        if fd == NO_PIPE {
            return;
        }
        let byte = [0u8];
        // SAFETY: write is async-signal-safe, and the pipe's write end is
        // open for the life of the process.
        unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
    }
    // SAFETY: installing a handler that only writes to a pipe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = tell as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    std::thread::Builder::new()
        .name("stop-on-signal".into())
        .spawn(move || {
            let mut byte = [0u8];
            loop {
                // SAFETY: a one-byte buffer, as the length says, on the
                // pipe's read end, which stays open for the life of the
                // process.
                let got = unsafe { libc::read(read, byte.as_mut_ptr().cast(), byte.len()) };
                if got > 0 {
                    break;
                }
                if got < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return;
            }
            if stop_all() > 0 {
                std::thread::sleep(grace);
            }
            std::process::exit(exit_code);
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    // The protocol between an interrupter and the run loop over a byte of
    // this test's own in place of the vCPU's.
    use super::*;

    #[test]
    fn an_interruption_is_taken_once_and_leaves_immediate_exit_clear() {
        // Nothing asked: a take finds nothing and clears the byte. An
        // interruption sets the ask and the byte; the take finds it,
        // clears the byte, and the next take finds nothing. A forgotten
        // byte is left alone.
        let mut byte = 1u8;
        let shared = Shared::new(&mut byte);
        let interrupter = Interrupter(shared.clone());
        assert!(!shared.take());
        assert_eq!(byte, 0);

        interrupter.interrupt();
        assert_eq!(byte, 1);
        assert!(shared.take());
        assert_eq!(byte, 0);
        assert!(!shared.take());

        shared.forget();
        interrupter.interrupt();
        assert_eq!(byte, 0);
        assert!(shared.take());
    }

    #[test]
    fn a_watch_interrupts_when_its_descriptor_is_readable_and_takes_the_ask_back() {
        // A pipe watched: nothing written, the watch ends with no ask
        // made. A byte written, the machine is asked to stop, as a run
        // under way would see; the watch, ended before any run took the
        // ask, takes it back, so no later run stops for it.
        use std::io::Write;
        let mut byte = 0u8;
        let shared = Shared::new(&mut byte);
        let (read, write) = pipe().unwrap();
        use std::os::fd::AsRawFd;

        let watch = FdWatch::start(read.as_raw_fd(), Interrupter(shared.clone()));
        drop(watch);
        assert!(!shared.asked.load(Ordering::SeqCst));

        let watch = FdWatch::start(read.as_raw_fd(), Interrupter(shared.clone()));
        std::fs::File::from(write).write_all(b"x").unwrap();
        let asked = || shared.asked.load(Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !asked() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(asked());
        drop(watch);
        assert!(!asked());
        assert!(!shared.take());
    }
}
