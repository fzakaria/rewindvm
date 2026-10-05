//! How a run ended, in the words `rewind ls`, `rewind check` and the
//! desktop app all use: exited:2, killed:SIGSEGV, timed-out,
//! missing-output or no-status.

use std::fmt;

use crate::signal_name;
use crate::stop::Stop;

/// The bits of a wait status, or of the kernel's exit_code, that hold the
/// terminating signal.
const SIGNAL_MASK: u32 = 0x7f;
/// The bit set when a process killed by a signal dumped core.
const CORE_BIT: u32 = 0x80;
/// The exit code sits in the second byte.
const CODE_SHIFT: u32 = 8;
const CODE_MASK: u32 = 0xff;
/// What a shell gives a process killed by a signal as its exit code: this
/// plus the signal's number.
const SHELL_SIGNAL_BASE: u8 = 128;

/// A process's end, decoded from a wait status or the kernel's exit_code:
/// an exit code in bits 8 to 15, or the terminating signal in bits 0 to
/// 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(u32),
    Signal { signo: u32, core: bool },
}

impl ExitStatus {
    /// The status the kernel's exit_code or a wait status `raw` holds.
    pub fn from_raw(raw: u32) -> ExitStatus {
        let signo = raw & SIGNAL_MASK;
        if signo == 0 {
            return ExitStatus::Code((raw >> CODE_SHIFT) & CODE_MASK);
        }
        ExitStatus::Signal {
            signo,
            core: raw & CORE_BIT != 0,
        }
    }

    /// The status a wait status as waitpid gives it holds.
    pub fn from_wait(status: i32) -> ExitStatus {
        ExitStatus::from_raw(status as u32)
    }

    /// Whether the process exited 0.
    pub fn success(self) -> bool {
        self == ExitStatus::Code(0)
    }

    /// The exit code a shell would give the process: its own, or 128 plus
    /// the signal that killed it.
    pub fn shell_code(self) -> u8 {
        match self {
            ExitStatus::Code(code) => code as u8,
            ExitStatus::Signal { signo, .. } => SHELL_SIGNAL_BASE + signo as u8,
        }
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitStatus::Code(code) => write!(f, "exited:{code}"),
            ExitStatus::Signal { signo, .. } => write!(f, "killed:{}", signal_name(*signo)),
        }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The machine reached its time limit before the run finished, which
    /// may be the host's doing as much as the job's.
    TimedOut,
    /// The job exited 0 without creating every output it was to, which
    /// nix-daemon counts as a failed build.
    MissingOutput,
    /// The job ended this way, as init reported.
    Job(ExitStatus),
    /// The machine stopped before init reported the job's end.
    NoStatus,
}

impl Ending {
    /// How a run ended whose machine stopped as `stop` and whose job init
    /// reported with wait status `status`, given `missing`, the outputs
    /// the job was to create and did not. Lists that do not read traces
    /// pass no missing outputs.
    pub fn of(stop: &Stop, status: Option<i32>, missing: &[String]) -> Ending {
        if stop.timeout().is_some() {
            return Ending::TimedOut;
        }
        match status.map(ExitStatus::from_wait) {
            Some(exit) if exit.success() && !missing.is_empty() => Ending::MissingOutput,
            Some(exit) => Ending::Job(exit),
            None => Ending::NoStatus,
        }
    }

    /// Whether the run passed: its job exited 0 having created every
    /// output.
    pub fn passed(self) -> bool {
        matches!(self, Ending::Job(exit) if exit.success())
    }
}

impl fmt::Display for Ending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ending::TimedOut => write!(f, "{}", crate::stop::TIMED_OUT_ENDING),
            Ending::MissingOutput => write!(f, "missing-output"),
            Ending::Job(exit) => write!(f, "{exit}"),
            Ending::NoStatus => write!(f, "no-status"),
        }
    }
}

#[cfg(test)]
mod tests {
    // Endings worded as rewind ls words them, from the stops and wait
    // statuses the engine records.
    use super::*;
    use crate::stop::{Doing, Timeout};

    const SIGSEGV: u32 = 11;

    #[test]
    fn a_wait_status_is_an_exit_code_or_a_signal() {
        // 2 << 8 is exit code 2; 11 is SIGSEGV, and 0x8b the same with a
        // core dumped; a shell gives those 2 and 139.
        assert_eq!(ExitStatus::from_wait(2 << 8), ExitStatus::Code(2));
        assert_eq!(
            ExitStatus::from_raw(0x8b),
            ExitStatus::Signal {
                signo: SIGSEGV,
                core: true
            }
        );
        assert_eq!(ExitStatus::from_wait(2 << 8).shell_code(), 2);
        assert_eq!(ExitStatus::from_raw(SIGSEGV).shell_code(), 139);
        assert_eq!(ExitStatus::from_raw(SIGSEGV).to_string(), "killed:SIGSEGV");
    }

    #[test]
    fn a_run_ends_as_rewind_ls_says() {
        // A timeout wins over any status; exiting 0 without every output
        // is missing-output; a machine that stopped before the job's end
        // has no status. Only exiting 0 with every output passes.
        let hung = Stop::TimedOut(Timeout {
            since_exit_ms: 0,
            doing: Doing::MakingExits,
        });
        let off = Stop::PoweredOff;
        let missing = ["/nix/store/x-dev".to_string()];
        let ended = |stop: &Stop, status, missing: &[String]| Ending::of(stop, status, missing);
        assert_eq!(ended(&hung, Some(0), &[]), Ending::TimedOut);
        assert_eq!(ended(&off, Some(0), &missing), Ending::MissingOutput);
        assert_eq!(ended(&off, Some(1 << 8), &missing).to_string(), "exited:1");
        assert_eq!(
            ended(&Stop::TripleFault, None, &[]).to_string(),
            "no-status"
        );
        assert_eq!(ended(&hung, None, &[]).to_string(), "timed-out");
        assert!(ended(&off, Some(0), &[]).passed());
        assert!(!ended(&off, Some(0), &missing).passed());
        assert!(!ended(&hung, Some(0), &[]).passed());
    }
}
