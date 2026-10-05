//! How a run's machine stopped, as its manifest's `outcome.stop` records
//! it. The engine writes it, and the command and the desktop app read it
//! and say it in the same words.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What `rewind ls` and the app call a run stopped at its time limit.
pub const TIMED_OUT_ENDING: &str = "timed-out";

/// How a run's machine stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Stop {
    /// The guest powered off, as init powers it off once the job is done.
    PoweredOff,
    /// The guest restarted itself.
    Restarted,
    /// The guest halted.
    Halted,
    /// The vCPU triple faulted.
    TripleFault,
    /// The guest went idle with no timer armed: nothing would ever wake it.
    Idle,
    /// The wall-clock limit passed first. Unlike the others this depends
    /// on the host, not the guest: a faster one may have finished.
    TimedOut(Timeout),
}

/// Where a guest was when its run reached its time limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeout {
    /// How long the guest had gone without an exit, in milliseconds: long
    /// for a guest computing without system calls, short for one still
    /// making them.
    pub since_exit_ms: u64,
    pub doing: Doing,
}

/// What a guest that reached its time limit was doing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Doing {
    /// Still making exits, so not stuck computing.
    MakingExits,
    /// Computing in the kernel at instruction `rip`, named by its symbol
    /// when the kernel's symbol table had one.
    Kernel { rip: u64, symbol: Option<String> },
    /// Computing in user space at instruction `rip`, in `thread` when the
    /// VM's kernel said which thread was on the CPU.
    User {
        rip: u64,
        thread: Option<StalledThread>,
    },
}

/// The thread a run was computing in when it timed out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalledThread {
    pub pid: u32,
    pub tid: u32,
    /// The thread's name, as the kernel keeps it.
    pub name: String,
}

/// How a timeout's words start.
const TIMED_OUT: &str = "timed out";

/// Milliseconds in a second, for saying how long a guest went without an
/// exit.
const MS_PER_SECOND: f64 = 1000.0;

impl Stop {
    /// Whether the run ended as a job that finishes does: its guest
    /// powered off. A timeout, a fault or anything else means the job
    /// never finished.
    pub fn is_clean(&self) -> bool {
        *self == Stop::PoweredOff
    }

    /// The timeout, when the run was stopped at its time limit.
    pub fn timeout(&self) -> Option<&Timeout> {
        match self {
            Stop::TimedOut(timeout) => Some(timeout),
            _ => None,
        }
    }
}

impl Timeout {
    /// The timeout of a guest computing in user space in words, with the
    /// instruction named by `place`: its address, or what the program's
    /// symbols say of it.
    pub fn in_user_space(&self, place: &str) -> String {
        format!("{}, in user space {place}", self.computing())
    }

    /// How long the guest computed without an exit, in words.
    fn computing(&self) -> String {
        format!(
            "{TIMED_OUT} computing without exits for {:.1}s",
            self.since_exit_ms as f64 / MS_PER_SECOND
        )
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::PoweredOff => write!(f, "poweroff"),
            Stop::Restarted => write!(f, "restart"),
            Stop::Halted => write!(f, "halt"),
            Stop::TripleFault => write!(f, "triple fault"),
            Stop::Idle => write!(f, "stalled: idle with no timer armed"),
            Stop::TimedOut(timeout) => write!(f, "{timeout}"),
        }
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.doing {
            Doing::MakingExits => write!(f, "{TIMED_OUT} while still making exits"),
            Doing::Kernel { rip, symbol } => {
                let at = symbol.clone().unwrap_or_else(|| format!("{rip:#x}"));
                write!(f, "{}, in the kernel at {at}", self.computing())
            }
            Doing::User { rip, .. } => write!(f, "{}", self.in_user_space(&format!("at {rip:#x}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    // Stops in the engine's words, and as manifests record them.
    use super::*;

    fn timeout(since_exit_ms: u64, doing: Doing) -> Stop {
        Stop::TimedOut(Timeout {
            since_exit_ms,
            doing,
        })
    }

    #[test]
    fn only_a_guest_that_powered_off_stopped_cleanly() {
        // A power-off is clean; a timeout is a timeout and not clean; a
        // fault is neither.
        assert!(Stop::PoweredOff.is_clean());
        let hung = timeout(2200, Doing::MakingExits);
        assert!(hung.timeout().is_some() && !hung.is_clean());
        assert!(Stop::TripleFault.timeout().is_none() && !Stop::TripleFault.is_clean());
    }

    #[test]
    fn timeouts_say_what_the_guest_was_doing() {
        // Still making exits, computing in the kernel at a symbol or at a
        // bare address, and computing in user space at an address or at
        // a place the program's symbols name.
        assert_eq!(
            timeout(3, Doing::MakingExits).to_string(),
            "timed out while still making exits"
        );
        let kernel = |symbol: Option<&str>| {
            timeout(
                2000,
                Doing::Kernel {
                    rip: 0xffff_ffff_8128_5085,
                    symbol: symbol.map(str::to_string),
                },
            )
            .to_string()
        };
        assert_eq!(
            kernel(Some("rewind_clock_read+0x5")),
            "timed out computing without exits for 2.0s, in the kernel at rewind_clock_read+0x5"
        );
        assert_eq!(
            kernel(None),
            "timed out computing without exits for 2.0s, in the kernel at 0xffffffff81285085"
        );
        let user = Timeout {
            since_exit_ms: 12_300,
            doing: Doing::User {
                rip: 0x55ce_d9ab_18a7,
                thread: None,
            },
        };
        assert_eq!(
            Stop::TimedOut(user.clone()).to_string(),
            "timed out computing without exits for 12.3s, in user space at 0x55ced9ab18a7"
        );
        assert_eq!(
            user.in_user_space("in spin (spin.c:4)"),
            "timed out computing without exits for 12.3s, in user space in spin (spin.c:4)"
        );
        assert_eq!(Stop::PoweredOff.to_string(), "poweroff");
    }

    #[test]
    fn a_stop_reads_back_as_written() {
        // Every kind of stop survives the manifest's JSON.
        let stops = [
            Stop::PoweredOff,
            Stop::Halted,
            Stop::Idle,
            timeout(
                1500,
                Doing::User {
                    rip: 0x401000,
                    thread: Some(StalledThread {
                        pid: 34,
                        tid: 36,
                        name: "worker".into(),
                    }),
                },
            ),
        ];
        for stop in stops {
            let json = serde_json::to_string(&stop).unwrap();
            assert_eq!(serde_json::from_str::<Stop>(&json).unwrap(), stop, "{json}");
        }
    }
}
