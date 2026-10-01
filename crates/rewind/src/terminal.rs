//! The person's end of `rewind shell`: this process's terminal, in raw
//! mode for as long as the shell runs, as the input of a forked machine.
//! Keys go to the VM as typed, and a change of window size is sent as a
//! resize message the VM's side applies to its pty.

use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use rewind_init::resize_message;
use rewind_vmm::Input;

/// The size a terminal is taken to be when it cannot be asked.
pub const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// How many bytes one read takes from standard input.
const READ_CHUNK: usize = 4096;

/// The window size of the terminal on `fd`, as (columns, rows).
pub fn size(fd: RawFd) -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer for TIOCGWINSZ.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as libc::Ioctl, &mut size) };
    (rc == 0 && size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// Standard input in raw mode until dropped, when it is a terminal: keys
/// arrive one by one, unechoed, with Ctrl-C and friends as bytes for the
/// shell inside the VM rather than signals for this process.
pub struct RawMode {
    saved: Option<libc::termios>,
}

impl RawMode {
    pub fn enter() -> RawMode {
        let fd = std::io::stdin().as_raw_fd();
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and termios.
        if unsafe { libc::isatty(fd) } != 1 || unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return RawMode { saved: None };
        }
        let mut raw = saved;
        // SAFETY: cfmakeraw only edits the struct.
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
        }
        RawMode { saved: Some(saved) }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            // SAFETY: restoring the settings read in enter.
            unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, saved) };
        }
    }
}

/// Standard input as the VM's input. Ends for good at end of file.
pub struct Keyboard {
    /// The size last sent, to notice when the window changes.
    size: (u16, u16),
    closed: bool,
}

impl Keyboard {
    /// A keyboard whose shell was started at `size`.
    pub fn new(size: (u16, u16)) -> Keyboard {
        Keyboard {
            size,
            closed: false,
        }
    }

    /// A resize message when the window has changed since the last one.
    fn resized(&mut self) -> Option<[u8; rewind_init::RESIZE_LEN]> {
        let now = size(std::io::stdout().as_raw_fd())?;
        if now == self.size {
            return None;
        }
        self.size = now;
        Some(resize_message(now.0, now.1))
    }
}

/// Makes a window resize interrupt a wait for keys, so the new size is sent
/// at once rather than with the next key: SIGWINCH is ignored by default,
/// and a handler that does nothing, installed without SA_RESTART, makes
/// poll return early.
pub fn wake_on_resize() {
    extern "C" fn nothing(_: libc::c_int) {}
    // SAFETY: installing a handler that touches nothing.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = nothing as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

impl Input for Keyboard {
    fn wait(&mut self, timeout: Option<Duration>) -> Vec<u8> {
        if self.closed {
            return Vec::new();
        }
        loop {
            if let Some(message) = self.resized() {
                return message.to_vec();
            }

            // Wait for a key, or the timeout; poll counts in milliseconds,
            // and -1 is forever.
            let millis = match timeout {
                Some(t) => t.as_millis().min(i32::MAX as u128) as i32,
                None => -1,
            };
            let mut fds = libc::pollfd {
                fd: std::io::stdin().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut fds, 1, millis) };

            // A resize woke a wait with no end: look at the size again. An
            // empty answer to a wait with no end would mean no more input.
            if ready < 0 && timeout.is_none() {
                continue;
            }
            if ready <= 0 {
                return Vec::new();
            }

            let mut buf = [0u8; READ_CHUNK];
            return match std::io::stdin().lock().read(&mut buf) {
                Ok(0) | Err(_) => {
                    self.closed = true;
                    Vec::new()
                }
                Ok(n) => buf[..n].to_vec(),
            };
        }
    }
}
