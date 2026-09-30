//! What the app asks of the engine: forking a run at a step, and later
//! serving gdb at a step, opening a shell at a step, and exporting a run.
//!
//! The app talks to the engine through the `Engine` trait, and `CliEngine`
//! implements the trait by running the `rewind` command. `rewind fork`
//! exists; gdb, shell and export do not yet, and say so without running
//! anything. Every call blocks, and the UI runs them on a background
//! thread. The engine's environment, REWIND_HOME among it, is the app's.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::run::MANIFEST_FILE;
use crate::viewer::{self, FetchedAll};

/// The engine's command, looked up on PATH.
pub const DEFAULT_PROGRAM: &str = "rewind";

/// An environment variable naming a different engine command, for
/// development builds of the engine.
pub const PROGRAM_ENV: &str = "REWIND_BIN";

/// The engine's home, as it reads it (crates/rewind-core/src/home.rs).
pub const HOME_ENV: &str = "REWIND_HOME";

/// Where runs live inside a Rewind home.
const RUNS_DIR: &str = "runs";

/// The home the engine uses when REWIND_HOME is not set, under the XDG
/// data directory.
const DEFAULT_HOME_DIR: &str = "rewind";
const XDG_DATA_ENV: &str = "XDG_DATA_HOME";
const HOME_FALLBACK_DATA: &str = ".local/share";

/// Engine features the app has buttons for but the engine does not have
/// yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Gdb,
    Shell,
    Export,
}

impl Feature {
    pub fn describe(self) -> &'static str {
        match self {
            Feature::Gdb => "Attaching gdb",
            Feature::Shell => "Opening a shell inside the VM",
            Feature::Export => "Exporting a run",
        }
    }
}

/// Why an engine call did not do what was asked, in words for a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The engine command is not installed.
    Missing { program: String },
    /// The engine ran and refused, with what it printed.
    Failed { command: String, message: String },
    /// The engine made a run the app cannot find on disk.
    Lost { id: String },
    /// The engine does not have this yet.
    NotYet(Feature),
}

impl EngineError {
    /// Whether the error is the engine's limit rather than a failure,
    /// which the app shows as information rather than an error.
    pub fn is_not_yet(&self) -> bool {
        matches!(self, EngineError::NotYet(_))
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Missing { program } => write!(
                f,
                "The engine command {program} was not found on PATH. Install it, or set {PROGRAM_ENV} to its path."
            ),
            EngineError::Failed { command, message } => write!(f, "{command} failed: {message}"),
            EngineError::Lost { id } => write!(
                f,
                "The engine made run {id}, but it is not next to its parent or in the Rewind home."
            ),
            EngineError::NotYet(feature) => {
                write!(f, "{} is coming in a later version.", feature.describe())
            }
        }
    }
}

impl std::error::Error for EngineError {}

pub type EngineResult<T> = Result<T, EngineError>;

/// A run the engine forked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forked {
    pub id: String,
    pub dir: PathBuf,
    /// How the fork ended, in words: "exited:2, first differs from its
    /// parent at step 3781".
    pub summary: String,
}

/// What `rewind fork --json` prints on standard output.
#[derive(serde::Deserialize)]
struct ForkJson {
    id: String,
    dir: PathBuf,
    status: Option<i32>,
    first_difference: Option<u64>,
}

impl ForkJson {
    fn summary(&self) -> String {
        let status = match self.status {
            None => "no exit status".to_string(),
            Some(s) if s & 0x7f != 0 => format!("killed by signal {}", s & 0x7f),
            Some(s) => format!("exited:{}", (s >> 8) & 0xff),
        };
        match self.first_difference {
            Some(step) => format!("{status}, first differs from its parent at step {step}"),
            None => format!("{status}, the same as its parent"),
        }
    }
}

/// The engine's operations on a recorded run.
pub trait Engine: Send + Sync {
    /// Forks `run` at `step`: the same inputs with the schedule perturbed
    /// by seed `schedule` from that step on. Returns the new run.
    fn fork(&self, run: &Path, step: u64, schedule: u64) -> EngineResult<Forked>;

    /// Brings `run` back to `step` behind a gdb server, and returns the
    /// address to attach to, as host:port.
    fn gdb(&self, run: &Path, step: u64) -> EngineResult<String>;

    /// Opens a shell inside the VM as it was at `step`.
    fn shell(&self, run: &Path, step: u64) -> EngineResult<()>;

    /// Writes `run` out as a single file others can replay, and returns
    /// its path.
    fn export(&self, run: &Path) -> EngineResult<PathBuf>;

    /// Reads `path` inside the VM as it was at `step` of `run`, as process
    /// `pid` saw it when one is given. The engine brings the run back to
    /// the step to read it, which takes seconds.
    fn cat(&self, run: &Path, step: u64, pid: Option<u32>, path: &str) -> EngineResult<FileAtStep>;
}

/// A file as it was at a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileAtStep {
    /// The file existed; `bytes` holds all of it unless `complete` says
    /// the engine's answer was cut at `viewer::MAX_SHOWN` bytes.
    Exists {
        bytes: Vec<u8>,
        complete: FetchedAll,
    },
    /// The file did not exist at the step.
    Missing,
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

    fn command_line(&self, args: &[OsString]) -> String {
        let mut parts = vec![self.program.to_string_lossy().into_owned()];
        parts.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
        parts.join(" ")
    }
}

impl Engine for CliEngine {
    fn fork(&self, run: &Path, step: u64, schedule: u64) -> EngineResult<Forked> {
        let args: [OsString; 6] = [
            "fork".into(),
            run.into(),
            step.to_string().into(),
            "--schedule".into(),
            schedule.to_string().into(),
            "--json".into(),
        ];
        let command = self.command_line(&args);
        let output = Command::new(&self.program)
            .args(&args)
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

        // The fork's exit code is the forked job's, so a fork of a failing
        // build exits nonzero; the JSON on standard output is what says a
        // run was made.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some(result) = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<ForkJson>(line).ok())
        else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = last_line(&stderr)
                .map(str::to_string)
                .unwrap_or_else(|| output.status.to_string());
            return Err(EngineError::Failed { command, message });
        };
        let summary = result.summary();
        let dir = if result.dir.join(MANIFEST_FILE).is_file() {
            result.dir
        } else {
            locate_run(&result.id, run).ok_or(EngineError::Lost {
                id: result.id.clone(),
            })?
        };
        Ok(Forked {
            id: result.id,
            dir,
            summary,
        })
    }

    fn gdb(&self, _run: &Path, _step: u64) -> EngineResult<String> {
        Err(EngineError::NotYet(Feature::Gdb))
    }

    fn shell(&self, _run: &Path, _step: u64) -> EngineResult<()> {
        Err(EngineError::NotYet(Feature::Shell))
    }

    fn export(&self, _run: &Path) -> EngineResult<PathBuf> {
        Err(EngineError::NotYet(Feature::Export))
    }

    fn cat(&self, run: &Path, step: u64, pid: Option<u32>, path: &str) -> EngineResult<FileAtStep> {
        // `rewind cat` exits 2 for a file that did not exist at the step.
        const MISSING_STATUS: i32 = 2;

        // rewind cat <run> <step> <path> [--pid P]
        let mut args: Vec<OsString> = vec![
            "cat".into(),
            run.into(),
            step.to_string().into(),
            path.into(),
        ];
        if let Some(pid) = pid {
            args.push("--pid".into());
            args.push(pid.to_string().into());
        }
        let command = self.command_line(&args);
        let failed = |message: String| EngineError::Failed {
            command: command.clone(),
            message,
        };
        let mut child = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => EngineError::Missing {
                    program: self.program.to_string_lossy().into_owned(),
                },
                _ => failed(e.to_string()),
            })?;

        // Read no more than the viewer shows, and one byte more to know
        // whether there was more; stop the engine if there was.
        let limit = viewer::MAX_SHOWN as u64 + 1;
        let mut bytes = Vec::new();
        let stdout = child.stdout.take().expect("stdout is piped");
        stdout
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|e| failed(e.to_string()))?;
        if bytes.len() > viewer::MAX_SHOWN {
            bytes.truncate(viewer::MAX_SHOWN);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(FileAtStep::Exists {
                bytes,
                complete: FetchedAll::No,
            });
        }
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let status = child.wait().map_err(|e| failed(e.to_string()))?;
        match status.code() {
            Some(0) => Ok(FileAtStep::Exists {
                bytes,
                complete: FetchedAll::Yes,
            }),
            Some(MISSING_STATUS) => Ok(FileAtStep::Missing),
            _ => Err(failed(
                last_line(&stderr)
                    .map(str::to_string)
                    .unwrap_or_else(|| status.to_string()),
            )),
        }
    }
}

/// Where the run with `id` is: next to its parent, else in the Rewind home
/// REWIND_HOME names, else in the default home.
fn locate_run(id: &str, parent: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(runs) = parent.parent() {
        candidates.push(runs.join(id));
    }
    if let Some(home) = std::env::var_os(HOME_ENV) {
        candidates.push(PathBuf::from(home).join(RUNS_DIR).join(id));
    }
    let data = std::env::var_os(XDG_DATA_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_FALLBACK_DATA)));
    if let Some(data) = data {
        candidates.push(data.join(DEFAULT_HOME_DIR).join(RUNS_DIR).join(id));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(MANIFEST_FILE).is_file())
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    // The command line engine against stand-in programs: shell scripts
    // written to a temporary directory play `rewind`, and a name that does
    // not exist plays a machine without it.
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rewind-app-engine-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs an engine call, again if the stand-in script was busy: a test
    /// running in parallel may fork while the script is still open for
    /// writing, and exec then fails with ETXTBSY for a moment.
    fn retrying<T>(call: impl Fn() -> EngineResult<T>) -> EngineResult<T> {
        const ATTEMPTS: usize = 20;
        const PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
        const BUSY: &str = "Text file busy";
        for _ in 1..ATTEMPTS {
            match call() {
                Err(EngineError::Failed { message, .. }) if message.contains(BUSY) => {
                    std::thread::sleep(PAUSE)
                }
                other => return other,
            }
        }
        call()
    }

    /// A stand-in engine: a script that prints `stdout` and `stderr` and
    /// exits `code`.
    fn fake_engine(dir: &Path, stdout: &str, stderr: &str, code: i32) -> CliEngine {
        let script = dir.join("rewind");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > \"{}/args\"\nprintf '{stdout}'\nprintf '{stderr}' >&2\nexit {code}\n", dir.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        CliEngine::new(script)
    }

    #[test]
    fn a_missing_engine_says_so() {
        // A program that does not exist reads as a missing engine.
        let engine = CliEngine::new("rewind-engine-that-does-not-exist");
        let err = engine.fork(Path::new("/run"), 5, 1).unwrap_err();
        assert!(matches!(err, EngineError::Missing { .. }));
        assert!(err.to_string().contains("not found on PATH"));
    }

    #[test]
    fn a_fork_of_a_failing_run_is_found_next_to_its_parent() {
        // The engine exits 2, as the forked build did, and names the new
        // run; the app finds it next to the parent and passes the schedule.
        let dir = temp_dir("fork");
        let runs = dir.join("runs");
        std::fs::create_dir_all(runs.join("parent")).unwrap();
        std::fs::create_dir_all(runs.join("abc123")).unwrap();
        std::fs::write(runs.join("abc123").join(MANIFEST_FILE), "{}").unwrap();
        let json = format!(
            r#"{{"id":"abc123","dir":"{}","status":512,"first_difference":3492}}\n"#,
            runs.join("abc123").display()
        );
        let engine = fake_engine(&dir, &json, "", 2);
        let forked = retrying(|| engine.fork(&runs.join("parent"), 3480, 3)).unwrap();
        assert_eq!(forked.id, "abc123");
        assert_eq!(forked.dir, runs.join("abc123"));
        assert_eq!(
            forked.summary,
            "exited:2, first differs from its parent at step 3492"
        );
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.contains("fork") && args.contains("3480 --schedule 3 --json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_fork_reports_the_engines_last_words() {
        // No finished line: the last thing on standard error is the reason.
        let dir = temp_dir("refused");
        let engine = fake_engine(&dir, "", "rewind: schedule 0 is the unperturbed run\\n", 1);
        let err = retrying(|| engine.fork(&dir, 1, 0)).unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: schedule 0 is the unperturbed run");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gdb_shell_and_export_are_not_there_yet() {
        // The three say so without running anything, as information.
        let engine = CliEngine::new("rewind-engine-that-does-not-exist");
        let err = engine.gdb(Path::new("/run"), 1).unwrap_err();
        assert!(err.is_not_yet());
        assert_eq!(
            err.to_string(),
            "Attaching gdb is coming in a later version."
        );
        assert!(engine.shell(Path::new("/run"), 1).unwrap_err().is_not_yet());
        assert!(engine.export(Path::new("/run")).unwrap_err().is_not_yet());
    }

    #[test]
    fn cat_returns_the_file_or_says_it_was_missing() {
        // The stand-in prints a file and exits 0, exits 2 for a file that
        // did not exist, or exits 1 with a reason; the arguments name the
        // step, the process and the path.
        let dir = temp_dir("cat");
        let run = dir.join("run");
        let engine = fake_engine(&dir, "CFLAGS = -O1\\n", "", 0);
        let read = retrying(|| engine.cat(&run, 3_795, Some(174), "/build/Makefile")).unwrap();
        assert_eq!(
            read,
            FileAtStep::Exists {
                bytes: b"CFLAGS = -O1\n".to_vec(),
                complete: FetchedAll::Yes
            }
        );
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.contains("cat") && args.contains("3795 /build/Makefile --pid 174"));

        let engine = fake_engine(&dir, "", "rewind: no such file then\\n", 2);
        let read = retrying(|| engine.cat(&run, 10, None, "/build/core")).unwrap();
        assert_eq!(read, FileAtStep::Missing);

        let engine = fake_engine(&dir, "", "rewind: no keyframes for this run\\n", 1);
        let err = retrying(|| engine.cat(&run, 10, None, "/x")).unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: no keyframes for this run");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
