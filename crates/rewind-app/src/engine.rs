//! What the app asks of the engine: forking a run at a step, serving gdb
//! at a step, opening a shell at a step, and exporting a run.
//!
//! The engine does not implement these yet. The app talks to it through
//! the `Engine` trait, and `CliEngine` implements the trait by running the
//! `rewind` command, so that the app works unchanged once the engine grows
//! the subcommands. Every call blocks, and the UI runs them on a
//! background thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The engine's command, looked up on PATH.
pub const DEFAULT_PROGRAM: &str = "rewind";

/// An environment variable naming a different engine command, for
/// development builds of the engine.
pub const PROGRAM_ENV: &str = "REWIND_BIN";

/// Why an engine call failed, in words for a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The engine command is not installed.
    Missing { program: String },
    /// The engine ran and refused, with what it printed.
    Failed { command: String, message: String },
    /// The engine ran and printed nothing the app could use.
    NoOutput { command: String },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Missing { program } => {
                write!(
                    f,
                    "The engine command {program} was not found on PATH. Install it, or set {PROGRAM_ENV} to its path."
                )
            }
            EngineError::Failed { command, message } => write!(f, "{command} failed: {message}"),
            EngineError::NoOutput { command } => write!(f, "{command} printed nothing"),
        }
    }
}

impl std::error::Error for EngineError {}

pub type EngineResult<T> = Result<T, EngineError>;

/// The engine's operations on a recorded run.
pub trait Engine: Send + Sync {
    /// Forks `run` at `step` with a new scheduling `seed`, and returns the
    /// new run's directory.
    fn fork(&self, run: &Path, step: u64, seed: u64) -> EngineResult<PathBuf>;

    /// Brings `run` back to `step` behind a gdb server, and returns the
    /// address to attach to, as host:port.
    fn gdb(&self, run: &Path, step: u64) -> EngineResult<String>;

    /// Opens a shell inside the guest as it was at `step`.
    fn shell(&self, run: &Path, step: u64) -> EngineResult<()>;

    /// Writes `run` out as a single file others can replay, and returns
    /// its path.
    fn export(&self, run: &Path) -> EngineResult<PathBuf>;
}

/// The engine as the `rewind` command line.
pub struct CliEngine {
    program: OsString,
}

impl CliEngine {
    /// The `rewind` on PATH, or the command `REWIND_BIN` names.
    pub fn from_env() -> CliEngine {
        let program = std::env::var_os(PROGRAM_ENV).unwrap_or_else(|| DEFAULT_PROGRAM.into());
        CliEngine { program }
    }

    pub fn new(program: impl Into<OsString>) -> CliEngine {
        CliEngine {
            program: program.into(),
        }
    }

    /// Runs the engine with `args` and returns the last line it printed
    /// on standard output.
    fn run(&self, args: &[OsString]) -> EngineResult<String> {
        let command = self.command_line(args);
        let output = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => EngineError::Missing {
                    program: self.program.to_string_lossy().into_owned(),
                },
                _ => EngineError::Failed {
                    command: command.clone(),
                    message: e.to_string(),
                },
            })?;

        // A failure's message is the last thing it said on standard error,
        // or its exit status when it said nothing.
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = last_line(&stderr)
                .map(str::to_string)
                .unwrap_or_else(|| output.status.to_string());
            return Err(EngineError::Failed { command, message });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(last_line(&stdout).unwrap_or_default().to_string())
    }

    fn command_line(&self, args: &[OsString]) -> String {
        let mut parts = vec![self.program.to_string_lossy().into_owned()];
        parts.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
        parts.join(" ")
    }

    /// Like `run`, for commands whose answer is the line they print.
    fn run_for_answer(&self, args: &[OsString]) -> EngineResult<String> {
        let answer = self.run(args)?;
        if answer.is_empty() {
            return Err(EngineError::NoOutput {
                command: self.command_line(args),
            });
        }
        Ok(answer)
    }
}

impl Engine for CliEngine {
    fn fork(&self, run: &Path, step: u64, seed: u64) -> EngineResult<PathBuf> {
        let args = [
            "fork".into(),
            run.into(),
            step.to_string().into(),
            "--seed".into(),
            seed.to_string().into(),
        ];
        self.run_for_answer(&args).map(PathBuf::from)
    }

    fn gdb(&self, run: &Path, step: u64) -> EngineResult<String> {
        let args = ["gdb".into(), run.into(), step.to_string().into()];
        self.run_for_answer(&args)
    }

    fn shell(&self, run: &Path, step: u64) -> EngineResult<()> {
        let args = ["shell".into(), run.into(), step.to_string().into()];
        self.run(&args).map(|_| ())
    }

    fn export(&self, run: &Path) -> EngineResult<PathBuf> {
        let args = ["export".into(), run.into()];
        self.run_for_answer(&args).map(PathBuf::from)
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    // The command line engine against stand-in programs: `true`, `false`
    // and `echo` from the test machine play the engine, and a name that
    // does not exist plays a machine without it.
    use super::*;

    #[test]
    fn a_missing_engine_says_so() {
        // A program that does not exist reads as a missing engine.
        let engine = CliEngine::new("rewind-engine-that-does-not-exist");
        let err = engine.fork(Path::new("/run"), 5, 1).unwrap_err();
        assert!(matches!(err, EngineError::Missing { .. }));
        assert!(err.to_string().contains("not found on PATH"));
    }

    #[test]
    fn a_failing_engine_reports_its_command() {
        // false exits 1, which reads as a failure naming the command line.
        let engine = CliEngine::new("false");
        let err = engine.gdb(Path::new("/run"), 5).unwrap_err();
        let EngineError::Failed { command, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(command, "false gdb /run 5");
    }

    #[test]
    fn answers_are_the_last_line_printed() {
        // echo prints its arguments back, which stands in for an engine
        // that prints the new run's path.
        let engine = CliEngine::new("echo");
        let path = engine.fork(Path::new("/run"), 7, 2).unwrap();
        assert_eq!(path, PathBuf::from("fork /run 7 --seed 2"));
        let silent = CliEngine::new("true");
        assert!(matches!(
            silent.gdb(Path::new("/run"), 1),
            Err(EngineError::NoOutput { .. })
        ));
        assert_eq!(silent.shell(Path::new("/run"), 1), Ok(()));
    }
}
