//! What the app asks of the engine: forking a run at a step, reading a
//! file at a step, finding the line of the program's own code a thread
//! was on at a step, exporting a run, and the command lines for a shell
//! and for gdb at a step.
//!
//! The app talks to the engine through the `Engine` trait, and `CliEngine`
//! implements the trait by running the `rewind` command. Every call
//! blocks, and the UI runs them on a background thread. The shell and gdb
//! are terminal programs, so the engine only names their command lines and
//! the terminal pane runs them in a pty. The engine's environment,
//! REWIND_HOME among it, is the app's.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::run::MANIFEST_FILE;
use crate::source::Located;
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

/// Why an engine call did not do what was asked, in words for a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The engine command is not installed.
    Missing { program: String },
    /// The engine ran and refused, with what it printed.
    Failed { command: String, message: String },
    /// The engine made a run the app cannot find on disk.
    Lost { id: String },
    /// The request was cancelled, superseded by a newer one, before the
    /// engine answered.
    Cancelled,
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
            EngineError::Cancelled => write!(f, "The request was cancelled."),
        }
    }
}

impl std::error::Error for EngineError {}

/// A way to stop an engine command from another thread. The UI keeps one
/// beside each lookup it waits on and cancels it when a newer request
/// supersedes the lookup or the window closes, so the engine's VM and gdb
/// stop instead of running on for an answer nobody reads.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    /// The process group of the engine command while it runs.
    group: Option<u32>,
}

impl Cancel {
    /// Stops the command, now if it runs, or as soon as it starts.
    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        state.cancelled = true;
        if let Some(group) = state.group.take() {
            stop_group(group);
        }
    }

    fn cancelled(&self) -> bool {
        self.0.lock().unwrap().cancelled
    }

    /// Records the command's process group, the engine's process id, as
    /// it starts. False, with the group stopped, when the request was
    /// cancelled meanwhile.
    fn started(&self, group: u32) -> bool {
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            stop_group(group);
            return false;
        }
        state.group = Some(group);
        true
    }

    /// Forgets the group once the command has been waited for, so a later
    /// cancel cannot signal a process group whose id was reused.
    fn finished(&self) {
        self.0.lock().unwrap().group = None;
    }
}

/// Asks every process in the engine command's group to end: the engine,
/// and gdb when it runs one.
fn stop_group(group: u32) {
    let Ok(group) = i32::try_from(group) else {
        return;
    };
    // SAFETY: sends a signal to a process group this app started.
    unsafe { libc::kill(-group, libc::SIGTERM) };
}

/// What the app says of a run this build of rewind replays another way than
/// the build that recorded it, in place of files, source, a shell, gdb or
/// a fork, which all need the run brought to a step.
pub const REPLAYS_ANOTHER_WAY: &str = "This build of rewind runs this run's inputs differently from the build that recorded it, so it cannot bring the run to a step: files, source, shells, gdb and forks are off for it. Record the run again with this build to look inside it.";

/// Whether the engine refused because replaying the run went another way
/// than its recording, which a later request for the same run would only
/// find again.
pub fn goes_another_way(error: &EngineError) -> bool {
    matches!(error, EngineError::Failed { message, .. } if message.contains(rewind_trace::WENT_ANOTHER_WAY))
}

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

/// A run the engine imported from an export, as `rewind import --json`
/// prints it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Imported {
    pub id: String,
    pub dir: PathBuf,
    /// Whether the run has the keyframes and inputs to be brought back to
    /// a step.
    pub replayable: bool,
}

/// What `rewind remove --json` prints on standard output.
#[derive(serde::Deserialize)]
struct RemoveJson {
    removed: Vec<String>,
}

/// The engine's operations on a recorded run.
pub trait Engine: Send + Sync {
    /// Forks `run` at `step`: the same inputs with the schedule perturbed
    /// by seed `schedule` from that step on. Returns the new run.
    fn fork(&self, run: &Path, step: u64, schedule: u64) -> EngineResult<Forked>;

    /// Writes `run` to `out` as a single .rwd file another machine can
    /// replay: its trace, keyframes, pages, inputs and kernel.
    fn export(&self, run: &Path, out: &Path) -> EngineResult<()>;

    /// Reads the .rwd file `file` into the engine's runs, where it can be
    /// forked, and returns the run.
    fn import(&self, file: &Path) -> EngineResult<Imported>;

    /// Removes the forks under `run` whose trace repeats an older fork's,
    /// and returns the ids of the runs removed.
    fn prune_identical(&self, run: &Path) -> EngineResult<Vec<String>>;

    /// Removes `run` and every fork descended from it, and returns the
    /// ids of the runs removed, `run` first.
    fn remove(&self, run: &Path) -> EngineResult<Vec<String>>;

    /// The command that opens an interactive shell inside a fork of `run`
    /// at `step`, in process `pid`'s root and working directory, or the
    /// job's when no process is given.
    fn shell_command(&self, run: &Path, step: u64, pid: Option<u32>) -> CommandLine;

    /// The command that forks `run` at `step` behind a GDB server and runs
    /// gdb attached to it.
    fn gdb_command(&self, run: &Path, step: u64) -> CommandLine;

    /// Reads `path` inside the VM as it was at `step` of `run`, as process
    /// `pid` saw it when one is given. The engine brings the run back to
    /// the step to read it, which takes seconds.
    /// `cancel` stops it early.
    fn cat(
        &self,
        run: &Path,
        step: u64,
        pid: Option<u32>,
        path: &str,
        cancel: &Cancel,
    ) -> EngineResult<FileAtStep>;

    /// Where in the program's own code thread `tid` of process `pid` was
    /// at `step` of `run`: the thread's frames, the innermost of them in
    /// the program's own code, and its source. The engine forks the run at
    /// the step and walks the thread's stack in gdb, which takes seconds,
    /// or a minute the first time gdb downloads a library's debug info;
    /// `progress` is handed each line the engine says while it works, and
    /// `cancel` stops it early.
    fn locate(
        &self,
        run: &Path,
        step: u64,
        pid: u32,
        tid: u32,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Located>;
}

/// A program and its arguments, for the terminal pane to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLine {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl CommandLine {
    /// The command as one line, for titles and messages.
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
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

    /// The error for a command that did not start.
    fn spawn_error(&self, e: std::io::Error, command: &str) -> EngineError {
        match e.kind() {
            std::io::ErrorKind::NotFound => EngineError::Missing {
                program: self.program.to_string_lossy().into_owned(),
            },
            _ => EngineError::Failed {
                command: command.to_string(),
                message: e.to_string(),
            },
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

    fn import(&self, file: &Path) -> EngineResult<Imported> {
        // rewind import <file> --json
        let args: [OsString; 3] = ["import".into(), file.into(), "--json".into()];
        let command = self.command_line(&args);
        let output = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(e, &command))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let imported = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Imported>(line).ok());
        match imported {
            Some(imported) if output.status.success() => Ok(imported),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = last_line(&stderr)
                    .map(str::to_string)
                    .unwrap_or_else(|| output.status.to_string());
                Err(EngineError::Failed { command, message })
            }
        }
    }

    fn prune_identical(&self, run: &Path) -> EngineResult<Vec<String>> {
        // rewind prune <run> --identical --json
        let args: [OsString; 4] = [
            "prune".into(),
            run.into(),
            "--identical".into(),
            "--json".into(),
        ];
        let command = self.command_line(&args);
        let output = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(e, &command))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let removed = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Vec<String>>(line).ok());
        match removed {
            Some(removed) if output.status.success() => Ok(removed),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = last_line(&stderr)
                    .map(str::to_string)
                    .unwrap_or_else(|| output.status.to_string());
                Err(EngineError::Failed { command, message })
            }
        }
    }

    fn remove(&self, run: &Path) -> EngineResult<Vec<String>> {
        // rewind remove <run> --json
        let args: [OsString; 3] = ["remove".into(), run.into(), "--json".into()];
        let command = self.command_line(&args);
        let output = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(e, &command))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let removed = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<RemoveJson>(line).ok());
        match removed {
            Some(removed) if output.status.success() => Ok(removed.removed),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let message = last_line(&stderr)
                    .map(str::to_string)
                    .unwrap_or_else(|| output.status.to_string());
                Err(EngineError::Failed { command, message })
            }
        }
    }

    fn export(&self, run: &Path, out: &Path) -> EngineResult<()> {
        // rewind export <run> --replayable -o <out>
        let args: [OsString; 5] = [
            "export".into(),
            run.into(),
            "--replayable".into(),
            "-o".into(),
            out.into(),
        ];
        let command = self.command_line(&args);
        let output = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(e, &command))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = last_line(&stderr)
            .map(str::to_string)
            .unwrap_or_else(|| output.status.to_string());
        Err(EngineError::Failed { command, message })
    }

    fn shell_command(&self, run: &Path, step: u64, pid: Option<u32>) -> CommandLine {
        // rewind shell <run> <step> [--pid P]
        let mut args: Vec<OsString> = vec!["shell".into(), run.into(), step.to_string().into()];
        if let Some(pid) = pid {
            args.push("--pid".into());
            args.push(pid.to_string().into());
        }
        CommandLine {
            program: self.program.clone(),
            args,
        }
    }

    fn gdb_command(&self, run: &Path, step: u64) -> CommandLine {
        // rewind gdb <run> <step>
        CommandLine {
            program: self.program.clone(),
            args: vec!["gdb".into(), run.into(), step.to_string().into()],
        }
    }

    fn cat(
        &self,
        run: &Path,
        step: u64,
        pid: Option<u32>,
        path: &str,
        cancel: &Cancel,
    ) -> EngineResult<FileAtStep> {
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

        // In a process group of its own, which cancelling stops whole.
        if cancel.cancelled() {
            return Err(EngineError::Cancelled);
        }
        let mut child = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => EngineError::Missing {
                    program: self.program.to_string_lossy().into_owned(),
                },
                _ => failed(e.to_string()),
            })?;
        if !cancel.started(child.id()) {
            let _ = child.wait();
            return Err(EngineError::Cancelled);
        }

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
            cancel.finished();
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
        cancel.finished();
        if cancel.cancelled() {
            return Err(EngineError::Cancelled);
        }
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

    fn locate(
        &self,
        run: &Path,
        step: u64,
        pid: u32,
        tid: u32,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Located> {
        self.where_json(run, step, pid, tid, cancel, progress)
    }
}

impl CliEngine {
    /// `rewind where`, as Engine::locate describes it.
    fn where_json(
        &self,
        run: &Path,
        step: u64,
        pid: u32,
        tid: u32,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Located> {
        // rewind where <run> <step> --pid P --tid T --json
        let args: [OsString; 8] = [
            "where".into(),
            run.into(),
            step.to_string().into(),
            "--pid".into(),
            pid.to_string().into(),
            "--tid".into(),
            tid.to_string().into(),
            "--json".into(),
        ];
        let command = self.command_line(&args);

        // In a process group of its own, with the gdb it starts, which
        // cancelling stops whole.
        if cancel.cancelled() {
            return Err(EngineError::Cancelled);
        }
        let mut child = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| self.spawn_error(e, &command))?;
        if !cancel.started(child.id()) {
            let _ = child.wait();
            return Err(EngineError::Cancelled);
        }

        // The answer is read beside the engine's lines, so a full pipe
        // cannot stop the engine.
        let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
        let answer = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut bytes);
            bytes
        });

        // The engine's lines, each handed on as it says it.
        let mut stderr = String::new();
        if let Some(pipe) = child.stderr.take() {
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else {
                    break;
                };
                progress(&line);
                stderr.push_str(&line);
                stderr.push('\n');
            }
        }
        let status = child.wait().map_err(|e| self.spawn_error(e, &command))?;
        cancel.finished();
        let stdout = answer.join().unwrap_or_default();
        if cancel.cancelled() {
            return Err(EngineError::Cancelled);
        }

        let stdout = String::from_utf8_lossy(&stdout);
        let located = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Located>(line).ok());
        match located {
            Some(located) if status.success() => Ok(located),
            _ => {
                let message = last_line(&stderr)
                    .map(str::to_string)
                    .unwrap_or_else(|| status.to_string());
                Err(EngineError::Failed { command, message })
            }
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
    if let Some(data) = default_home() {
        candidates.push(data.join(RUNS_DIR).join(id));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(MANIFEST_FILE).is_file())
}

/// The directory the engine keeps its runs in: under REWIND_HOME when it
/// is set, else under the default home.
pub fn runs_dir() -> Option<PathBuf> {
    let home = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .or_else(default_home)?;
    Some(home.join(RUNS_DIR))
}

/// The engine's home when REWIND_HOME is not set: rewind under the XDG
/// data directory.
fn default_home() -> Option<PathBuf> {
    let data = std::env::var_os(XDG_DATA_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_FALLBACK_DATA)))?;
    Some(data.join(DEFAULT_HOME_DIR))
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

    #[test]
    fn a_replay_that_went_another_way_is_told_apart() {
        // The engine's refusal for a replay that went another way, in its
        // words, is recognized; another refusal and a missing engine are
        // not.
        let failed = |message: String| EngineError::Failed {
            command: "rewind cat".into(),
            message,
        };
        let words = format!(
            "rewind: replaying run 9d540a70 {} 506",
            rewind_trace::WENT_ANOTHER_WAY
        );
        assert!(goes_another_way(&failed(words)));
        assert!(!goes_another_way(&failed("rewind: no process 7".into())));
        assert!(!goes_another_way(&EngineError::Missing {
            program: "rewind".into()
        }));
    }
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

    /// The first `Some` that `attempt` gives, trying again a moment later
    /// while it gives None; `what` names the wait when it never ends.
    fn wait_for<T>(what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
        const TRIES: u32 = 500;
        const PAUSE: std::time::Duration = std::time::Duration::from_millis(10);
        for _ in 0..TRIES {
            if let Some(found) = attempt() {
                return found;
            }
            std::thread::sleep(PAUSE);
        }
        panic!("gave up waiting for {what}");
    }

    /// A stand-in engine that writes its process id to `pid` in `dir` and
    /// then waits half a minute, as a lookup on a long run does.
    fn slow_engine(dir: &Path) -> CliEngine {
        let script = dir.join("rewind");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > \"{}/pid\"\nexec sleep 30\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        CliEngine::new(script)
    }

    #[test]
    fn a_cancelled_lookup_stops_the_engine() {
        // A `rewind cat` that would take half a minute, cancelled once the
        // engine has started: the call returns as cancelled within a few
        // seconds, and the engine's process is gone.
        const GIVE_UP: std::time::Duration = std::time::Duration::from_secs(5);
        let dir = temp_dir("cancel");
        let engine = slow_engine(&dir);
        let cancel = Cancel::default();
        let call = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                engine.cat(Path::new("/run"), 5, None, "/etc/hosts", &cancel)
            })
        };
        let pid_file = dir.join("pid");
        let pid: i32 = wait_for("the engine to start", || {
            std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
        });

        let asked = std::time::Instant::now();
        cancel.cancel();
        let result = call.join().unwrap();
        assert!(asked.elapsed() < GIVE_UP);
        assert!(matches!(result, Err(EngineError::Cancelled)), "{result:?}");
        // SAFETY: signal 0 only asks whether the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lookup_cancelled_before_it_starts_runs_nothing() {
        // A request superseded before its engine call begins returns as
        // cancelled without starting the engine.
        let dir = temp_dir("cancel-early");
        let engine = slow_engine(&dir);
        let cancel = Cancel::default();
        cancel.cancel();
        let result = engine.cat(Path::new("/run"), 5, None, "/etc/hosts", &cancel);
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(!dir.join("pid").exists());
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn an_import_returns_the_run_the_engine_names() {
        // The stand-in prints what `rewind import --json` does; the app
        // takes the run's directory from it.
        let dir = temp_dir("import");
        let file = dir.join("run.rwd");
        let json =
            r#"{"id":"e8e7","dir":"/home/me/.local/share/rewind/runs/e8e7","replayable":true}\n"#;
        let engine = fake_engine(&dir, json, "", 0);
        let imported = retrying(|| engine.import(&file)).unwrap();
        assert_eq!(
            imported,
            Imported {
                id: "e8e7".into(),
                dir: PathBuf::from("/home/me/.local/share/rewind/runs/e8e7"),
                replayable: true,
            }
        );
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(args.trim(), format!("import {} --json", file.display()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_import_reports_the_engines_last_words() {
        // A failed import prints no run, and its reason on standard error.
        let dir = temp_dir("import-refused");
        let engine = fake_engine(&dir, "", "rewind: run.rwd: not a zstd stream\n", 1);
        let err = retrying(|| engine.import(&dir.join("run.rwd"))).unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: run.rwd: not a zstd stream");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pruning_returns_the_runs_the_engine_removed() {
        // The stand-in prints the removed ids as `rewind prune --json`
        // does; the arguments name the run and ask for identical forks.
        let dir = temp_dir("prune");
        let run = dir.join("base");
        let engine = fake_engine(&dir, r#"["dup1","dup2"]\n"#, "", 0);
        let removed = retrying(|| engine.prune_identical(&run)).unwrap();
        assert_eq!(removed, vec!["dup1".to_string(), "dup2".to_string()]);
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(
            args.trim(),
            format!("prune {} --identical --json", run.display())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removing_returns_the_run_and_its_forks() {
        // The stand-in prints what `rewind remove --json` does; a refusal
        // reports the engine's reason.
        let dir = temp_dir("remove");
        let run = dir.join("fork");
        let engine = fake_engine(&dir, r#"{"removed":["fork","grandfork"]}\n"#, "", 0);
        let removed = retrying(|| engine.remove(&run)).unwrap();
        assert_eq!(removed, vec!["fork".to_string(), "grandfork".to_string()]);
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(args.trim(), format!("remove {} --json", run.display()));

        let refusing = fake_engine(&dir, "", "rewind: run y reads its keyframes from fork\n", 1);
        let err = retrying(|| refusing.remove(&run)).unwrap_err();
        assert!(err.to_string().contains("reads its keyframes"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn export_asks_for_a_replayable_file_at_the_path() {
        // The stand-in exits 0 for a written export, and 1 with a reason
        // for a refused one; the arguments name the run and the output.
        let dir = temp_dir("export");
        let (run, out) = (dir.join("run"), dir.join("out.rwd"));
        let engine = fake_engine(&dir, "", "rewind: wrote out.rwd (3.1 MB)\\n", 0);
        retrying(|| engine.export(&run, &out)).unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(
            args.trim(),
            format!("export {} --replayable -o {}", run.display(), out.display())
        );

        let engine = fake_engine(&dir, "", "rewind: no keyframes for this run\\n", 1);
        let err = retrying(|| engine.export(&run, &out)).unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: no keyframes for this run");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_and_gdb_name_the_run_the_step_and_the_process() {
        // The command lines the terminal pane runs, built without running
        // anything.
        let engine = CliEngine::new("rewind");
        let run = Path::new("/runs/abc");
        assert_eq!(
            engine.shell_command(run, 4_392, Some(165)).display(),
            "rewind shell /runs/abc 4392 --pid 165"
        );
        assert_eq!(
            engine.shell_command(run, 10, None).display(),
            "rewind shell /runs/abc 10"
        );
        assert_eq!(
            engine.gdb_command(run, 4_392).display(),
            "rewind gdb /runs/abc 4392"
        );
    }

    /// The stand-in prints what `rewind where --json` does; the app reads
    /// the chosen frame from it, and the arguments name the step, the
    /// process and the thread. A refusal reports the engine's reason.
    #[test]
    fn locate_returns_the_chosen_frame_or_the_engines_reason() {
        let dir = temp_dir("where");
        let run = dir.join("run");
        let json = r#"{"run":"r","step":5060,"pid":166,"tid":174,"process":"test_pool_shutdown","frames":[{"level":0,"function":"worker","file":"src/pool.c","fullname":null,"line":77,"pc":"0x55bfaf437437","object":"/build/mylib/tests/test_pool_shutdown"}],"chosen":0,"files":{}}\n"#;
        let engine = fake_engine(&dir, json, "rewind: walking the stack in gdb\n", 0);
        let located =
            retrying(|| engine.locate(&run, 5_060, 166, 174, &Cancel::default(), &mut |_| {}))
                .unwrap();
        assert_eq!(
            located.chosen_frame().unwrap().place_label(),
            "src/pool.c:77"
        );
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(
            args.trim(),
            format!("where {} 5060 --pid 166 --tid 174 --json", run.display())
        );

        let refusing = fake_engine(&dir, "", "rewind: no process 166 at this step\n", 1);
        let err = retrying(|| refusing.locate(&run, 10, 166, 166, &Cancel::default(), &mut |_| {}))
            .unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: no process 166 at this step");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The engine's lines reach the caller while it runs. The stand-in
    /// says a line, then waits up to five seconds for a file the caller
    /// makes when the line reaches it, and answers only once the file is
    /// there; a caller that read the lines after the engine exited gets
    /// no answer.
    #[test]
    fn locate_hands_on_the_engine_s_lines_as_they_come() {
        let dir = temp_dir("where-progress");
        let run = dir.join("run");
        let go = dir.join("go");
        let script = dir.join("rewind");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'rewind: downloading debug info for libc.so.6; first time only\\n' >&2\n\
                 i=0; while [ ! -e '{go}' ] && [ $i -lt 500 ]; do sleep 0.01; i=$((i + 1)); done\n\
                 [ -e '{go}' ] || exit 1\n\
                 printf '%s\\n' '{{\"run\":\"r\",\"step\":1,\"pid\":2,\"tid\":2,\"process\":\"p\",\"frames\":[],\"chosen\":null,\"files\":{{}}}}'\n",
                go = go.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let engine = CliEngine::new(script);

        let said = std::cell::RefCell::new(Vec::new());
        let located = retrying(|| {
            engine.locate(&run, 1, 2, 2, &Cancel::default(), &mut |line| {
                said.borrow_mut().push(line.to_string());
                std::fs::write(&go, "").unwrap();
            })
        })
        .unwrap();
        assert_eq!(located.process, "p");
        assert_eq!(
            said.into_inner(),
            vec!["rewind: downloading debug info for libc.so.6; first time only".to_string()]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cat_returns_the_file_or_says_it_was_missing() {
        // The stand-in prints a file and exits 0, exits 2 for a file that
        // did not exist, or exits 1 with a reason; the arguments name the
        // step, the process and the path.
        let dir = temp_dir("cat");
        let run = dir.join("run");
        let engine = fake_engine(&dir, "CFLAGS = -O1\\n", "", 0);
        let read = retrying(|| {
            engine.cat(
                &run,
                3_795,
                Some(174),
                "/build/Makefile",
                &Cancel::default(),
            )
        })
        .unwrap();
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
        let read =
            retrying(|| engine.cat(&run, 10, None, "/build/core", &Cancel::default())).unwrap();
        assert_eq!(read, FileAtStep::Missing);

        let engine = fake_engine(&dir, "", "rewind: no keyframes for this run\\n", 1);
        let err = retrying(|| engine.cat(&run, 10, None, "/x", &Cancel::default())).unwrap_err();
        let EngineError::Failed { message, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(message, "rewind: no keyframes for this run");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
