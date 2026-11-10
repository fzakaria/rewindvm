//! The exit statuses rewind's commands share, so a script can tell what
//! a command found from a command that could not do its job: `rewind
//! diff a b || report_flaky` must not report a flaky build when run b
//! does not exist.
//!
//! `run`, `nix` and `fork` exit with the job's status instead, as a shell
//! reports a child, and `cat` and `files` have statuses of their own for
//! a path that is not there (see cmd/inspect.rs).

use std::process::ExitCode;

/// The command did its job, and what it found is the answer "no": `diff`,
/// `replay` and `blame` saw the runs differ, `check` saw a run fail,
/// `summary` was asked about a run that failed, `doctor` found a machine
/// that cannot record, and `pmu status` a counter runs cannot use.
pub(crate) const DIFFERS: u8 = 1;

/// The command could not do its job: a run that does not exist, a step
/// past the run's end, a file that does not read. Clap exits with the
/// same status for a command line it refused, which is the same news:
/// nothing was compared or checked.
pub(crate) const ERROR: u8 = 2;

/// The status a command's result exits with: the status the command
/// chose, or [`ERROR`] when it failed.
pub(crate) fn status(result: &anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => *code,
        Err(_) => ExitCode::from(ERROR),
    }
}

#[cfg(test)]
mod tests {
    // The mapping from a command's result to the process's exit status.
    use super::*;

    #[test]
    fn a_failure_exits_2_and_a_difference_1() {
        // A command that found a difference keeps its own status, and one
        // that failed exits 2, apart from both 0 and 1.
        assert_eq!(status(&Ok(ExitCode::SUCCESS)), ExitCode::SUCCESS);
        assert_eq!(
            status(&Ok(ExitCode::from(DIFFERS))),
            ExitCode::from(DIFFERS)
        );
        assert_eq!(
            status(&Err(anyhow::anyhow!("no run matches \"b\""))),
            ExitCode::from(ERROR)
        );
        assert_ne!(ExitCode::from(ERROR), ExitCode::from(DIFFERS));
        assert_ne!(ExitCode::from(ERROR), ExitCode::SUCCESS);
    }
}
