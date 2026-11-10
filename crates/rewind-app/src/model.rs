//! The scrubber's model of one run.
//!
//! Every question the UI asks on a frame (which lines were printed by step
//! N, which processes were alive, which files were written, which phase the
//! build was in, what happened last) is answered here from tables built
//! once when the run is opened. Each answer is a binary search or a scan of
//! a short list, so dragging the playhead across a trace of any size never
//! decodes or replays the trace again.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use rewind_trace::ending::ExitStatus;
use rewind_trace::sdk::{SdkMark, TestResult};
use rewind_trace::switches::Switch;
use rewind_trace::{Event, EventKind, JobExit, Process, Trace, signal};

use crate::shown_line::{Pen, shown};

/// The file descriptor of standard error.
const STDERR_FD: u32 = 2;

/// Kernel events do not identify a user thread.
const KERNEL_PID: u32 = 0;

/// The line nixpkgs' setup.sh prints as each phase starts.
const NIX_PHASE_PREFIX: &str = "Running phase: ";

/// nixpkgs' setup.sh writes structured messages for nix itself on lines
/// starting with this; nix-build never shows them, and neither does the log.
const NIX_LOG_PREFIX: &str = "@nix ";

/// The suffix nixpkgs puts on phase names, dropped on the timeline.
const NIX_PHASE_SUFFIX: &str = "Phase";

/// The name of the stretch before the first phase or mark, in a trace
/// without init's start mark to say where boot ends.
const OPENING_PHASE: &str = "start";

/// The segment before init's start mark: the guest kernel booting.
const BOOT_PHASE: &str = "boot";
/// The segment from the start mark to the first phase: nixpkgs' setup,
/// before unpackPhase.
const SETUP_PHASE: &str = "setup";
/// The segment from the start mark on, in a job without phases.
const JOB_PHASE: &str = "job";

/// Opens of device files (/dev/null, /dev/rewind and the like) are not
/// files the job wrote, and are left out of the files list.
const DEVICE_PREFIX: &str = "/dev/";

/// The name of the whole run when it has no phases or marks at all.
const WHOLE_RUN_PHASE: &str = "run";

/// Words that make a line of output read as an error, matched on the
/// lowercased line.
const ERROR_MARKERS: &[&str] = &[
    "error",
    "failed",
    "fatal",
    "segfault",
    "segmentation fault",
    "exception",
    "panic",
    "core dumped",
    "aborted",
    "oops",
    "bug:",
];

/// Linux always gives the kernel thread daemon pid 2; its children are
/// kernel threads, which never exec and so have no command line.
const KTHREADD_PID: u32 = 2;
const KTHREADD_NAME: &str = "kthreadd";
const KERNEL_THREAD_NAME: &str = "kernel thread";

/// Marks a process that forked and never exec'd, after its parent's name.
const FORK_SUFFIX: &str = "(fork)";

/// A file written this share of the run ago or less is drawn as fresh.
pub const RECENT_SHARE: f64 = 0.005;

/// Where a log line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// The guest kernel's console.
    Console,
    /// A mark written to /dev/rewind.
    Mark,
    /// What the job asserted with the Rewind SDK and broke, and how its
    /// tests ended.
    Sdk,
}

/// How a log line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    /// A phase starting, or a mark.
    Phase,
    Stderr,
    Error,
    /// Kernel console text, drawn muted.
    Console,
}

/// One line of the build log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// The step the line was completed on.
    pub step: u64,
    pub pid: u32,
    pub stream: Stream,
    pub text: String,
    pub tone: Tone,
    /// The runs of `text` its program drew in colors or weights of their
    /// own, by byte range; the rest is drawn in the tone's color.
    pub pens: Vec<(std::ops::Range<usize>, Pen)>,
}

/// Which lines the build log shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFilter {
    /// Standard output, standard error and marks.
    Output,
    /// The same, with the kernel console interleaved.
    WithConsole,
}

/// A named stretch of the run, as drawn on the timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseSpan {
    pub name: String,
    pub start: u64,
    pub end: u64,
}

/// What a row of the process tree stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Process,
    /// A thread other than its process's main one.
    Thread,
}

/// One row of the process tree, alive from `start` until `end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    /// The thread id: the pid for a process row.
    pub tid: u32,
    pub kind: RowKind,
    /// What the row is called from its process's last exec on; for a row
    /// that never exec'd, at every step.
    pub label: String,
    /// What the row was called before its process's last exec: each label
    /// with the step of the exec that ended it, in step order. First what
    /// a fork is called, until the first exec, then each program until the
    /// next. Empty for a thread and for a process that never exec'd.
    pub earlier: Vec<(u64, String)>,
    pub depth: usize,
    pub start: u64,
    pub end: Option<u64>,
    /// The kernel's exit_code, once the row has ended.
    pub status: Option<u32>,
}

impl ProcRow {
    pub fn alive_at(&self, step: u64) -> bool {
        self.start <= step && self.end.is_none_or(|end| step < end)
    }

    /// The row's label as of `step`: the program its process had exec'd
    /// by then, or the label of a fork before its first exec.
    pub fn label_at(&self, step: u64) -> &str {
        self.earlier
            .iter()
            .find(|(until, _)| step < *until)
            .map_or(&self.label, |(_, label)| label)
    }
}

/// What happened to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOp {
    /// Opened for writing, created or truncated.
    Write,
    Unlink,
    /// Renamed onto `path`.
    Rename,
    /// Built by the job, as init reported after it succeeded; `from` holds
    /// the tree hash.
    Output,
}

/// One change to the guest's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEvent {
    pub step: u64,
    pub pid: u32,
    pub op: FileOp,
    pub path: String,
    /// The old name of a renamed file.
    pub from: Option<String>,
}

impl FileEvent {
    /// A core dump: a file named `core` or `core.<pid>`.
    pub fn is_core_dump(&self) -> bool {
        is_core_path(&self.path)
    }
}

/// A file a directory holds at a step, as far as the trace knows: one a
/// process opened for writing, or renamed there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenFile {
    pub path: String,
    /// The step the file was last opened for writing at, or renamed to
    /// this path at.
    pub step: u64,
    /// The process that did so, whose view of paths the engine reads the
    /// file with.
    pub pid: u32,
}

/// Whether `path` is below directory `dir`, which has no trailing slash.
fn below(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with(PATH_SEPARATOR))
}

/// What separates the names of a path.
const PATH_SEPARATOR: char = '/';

/// Whether a path names a core dump: `core` or `core.<pid>`.
pub fn is_core_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    name == "core"
        || name
            .strip_prefix("core.")
            .is_some_and(|rest| !rest.is_empty())
}

/// How a file row is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTone {
    Normal,
    /// Written within the last `RECENT_SHARE` of the run.
    Recent,
    /// Removed.
    Gone,
    /// A core dump.
    Error,
    /// Something the job built.
    Output,
}

/// Why the app calls a run failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// A crash signal was delivered.
    Signal { signo: u32, addr: u64 },
    /// A process exited with a nonzero exit_code.
    Exit { status: u32 },
    /// The machine stopped before the job exited: at its time limit, or
    /// by a fault.
    Stopped,
    /// The job exited 0, but an assertion it made with the Rewind SDK did
    /// not hold: an `always` failed or an `unreachable` was reached.
    Assertion,
}

/// How a run's machine stopped, as far as finding its failure goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stopped {
    /// Any way but its guest powering off, such as at its time limit.
    Abnormally,
    /// By its guest powering off, or not known.
    Otherwise,
}

/// The step a run failed on, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    /// The index of the failing event in the trace.
    pub index: usize,
    pub kind: FailureKind,
}

/// A way to move the playhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Start,
    End,
    PreviousEvent,
    NextEvent,
    StepBack,
    StepForward,
    PreviousPhase,
    NextPhase,
    Failure,
    Divergence,
}

impl Motion {
    /// Whether the motion leaves the playhead's neighbourhood, so Back
    /// should be able to return from it: the ends, the phases and the
    /// markers, but not an event or a step.
    pub fn is_jump(self) -> bool {
        match self {
            Motion::Start
            | Motion::End
            | Motion::PreviousPhase
            | Motion::NextPhase
            | Motion::Failure
            | Motion::Divergence => true,
            Motion::PreviousEvent | Motion::NextEvent | Motion::StepBack | Motion::StepForward => {
                false
            }
        }
    }
}

/// One run, indexed for scrubbing.
#[derive(Clone)]
pub struct Timeline {
    pub trace: Trace,
    /// The last step of the run: the right end of the timeline.
    pub total: u64,
    output_lines: Vec<LogLine>,
    all_lines: Vec<LogLine>,
    pub phases: Vec<PhaseSpan>,
    pub rows: Vec<ProcRow>,
    /// The processes the rows were made from, in the trace's own order.
    pub procs: Vec<Process>,
    pub files: Vec<FileEvent>,
    pub failure: Option<Failure>,
    /// How the run's machine stopped, which finding the failure again needs.
    stopped: Stopped,
    /// The job's exit, when init reported it.
    pub job_exit: Option<JobExit>,
    /// The step the job started on, when init wrote its start mark:
    /// before it the VM is still booting.
    pub job_start: Option<u64>,
    /// A display name per process row pid, for describing events.
    names: HashMap<u32, String>,
    /// What the job asserted with the Rewind SDK and broke, in step order.
    pub assertions: Vec<Assertion>,
    /// The tests the job marked with the Rewind SDK, in the order they
    /// began.
    pub tests: Vec<TestSpan>,
}

/// An assertion the job made with the Rewind SDK that did not hold: an
/// `always` that failed, or an `unreachable` it reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    /// What failed, in words: `always "even" failed: byte 221 is odd`.
    pub text: String,
}

/// A test the job marked with the Rewind SDK: the steps it began and
/// ended on, and how it ended. A test that never ended has neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestSpan {
    pub name: String,
    pub begin: u64,
    pub end: Option<u64>,
    pub result: Option<TestResult>,
}

/// An SDK mark in words, and whether it says something broke: what the
/// event card, the log and the timeline show for it.
pub fn sdk_words(mark: &SdkMark) -> (String, bool) {
    let said = |what: String, message: &str| {
        if message.is_empty() {
            what
        } else {
            format!("{what}: {message}")
        }
    };
    match mark {
        SdkMark::Always {
            id,
            held: false,
            message,
        } => (said(format!("always \"{id}\" failed"), message), true),
        SdkMark::Always { id, .. } => (format!("always \"{id}\" held"), false),
        SdkMark::Sometimes { id, hit: true } => (format!("sometimes \"{id}\" was true"), false),
        SdkMark::Sometimes { id, hit: false } => (format!("sometimes \"{id}\" was false"), false),
        SdkMark::Unreachable { id, message } => {
            (said(format!("unreachable \"{id}\" reached"), message), true)
        }
        SdkMark::TestBegin { name } => (format!("test {name} began"), false),
        SdkMark::TestEnd { name, result } => match result {
            TestResult::Pass => (format!("test {name} passed"), false),
            TestResult::Fail => (format!("test {name} failed"), true),
            TestResult::Skip => (format!("test {name} was skipped"), false),
        },
    }
}

/// The assertions that broke and the tests, from the job's SDK marks.
fn sdk_findings(trace: &Trace) -> (Vec<Assertion>, Vec<TestSpan>) {
    let mut assertions = Vec::new();
    let mut tests: Vec<TestSpan> = Vec::new();
    for (e, mark) in trace.sdk_marks() {
        match &mark {
            SdkMark::Always { held: false, .. } | SdkMark::Unreachable { .. } => {
                assertions.push(Assertion {
                    step: e.step,
                    pid: e.pid,
                    tid: e.tid,
                    text: sdk_words(&mark).0,
                });
            }
            SdkMark::TestBegin { name } => tests.push(TestSpan {
                name: name.clone(),
                begin: e.step,
                end: None,
                result: None,
            }),
            // The test's latest unfinished begin, or one of its own when
            // the job marked no begin.
            SdkMark::TestEnd { name, result } => {
                match tests
                    .iter_mut()
                    .rev()
                    .find(|t| t.name == *name && t.end.is_none())
                {
                    Some(test) => {
                        test.end = Some(e.step);
                        test.result = Some(*result);
                    }
                    None => tests.push(TestSpan {
                        name: name.clone(),
                        begin: e.step,
                        end: Some(e.step),
                        result: Some(*result),
                    }),
                }
            }
            SdkMark::Always { .. } | SdkMark::Sometimes { .. } => {}
        }
    }
    (assertions, tests)
}

impl Timeline {
    /// Indexes a trace. `total_hint` stretches the timeline past the last
    /// event or switch, for a run whose manifest says it went on longer.
    /// The timeline is stopped as the engine's `stop` says.
    pub fn new(
        trace: Trace,
        total_hint: Option<u64>,
        stop: Option<&rewind_trace::stop::Stop>,
    ) -> Timeline {
        let last_switch = trace.switches.last().map_or(0, |s| s.step);
        let total = trace
            .last_step()
            .max(last_switch)
            .max(total_hint.unwrap_or(0));
        let stopped = match stop {
            Some(stop) if !stop.is_clean() => Stopped::Abnormally,
            _ => Stopped::Otherwise,
        };
        let output_lines = output_lines(&trace);
        let all_lines = merge_by_step(&output_lines, &console_lines(&trace));
        let phases = phase_spans(&trace, &output_lines, total);
        let procs = trace.processes();
        let rows = process_rows(&trace, &procs);
        let files = file_events(&trace);
        let job_exit = trace.job_exit();
        let job_start = trace.job_start();
        let failure = find_failure(&trace, job_exit, total, stopped, None);
        let (assertions, tests) = sdk_findings(&trace);

        // Command names for event descriptions: the latest process per pid
        // wins, which is the right one for any pid that was not reused.
        let mut names = HashMap::new();
        for row in rows.iter().filter(|r| r.kind == RowKind::Process) {
            names.insert(row.pid, command_name(&row.label));
        }

        Timeline {
            trace,
            total,
            output_lines,
            all_lines,
            phases,
            rows,
            procs,
            files,
            failure,
            stopped,
            job_exit,
            job_start,
            names,
            assertions,
            tests,
        }
    }

    /// Finds the failure again with the steps before `shared` left out:
    /// steps a passing run compared with this one made too, so nothing in
    /// them failed this run, such as a child that exits nonzero in both.
    /// With none, or nothing failing after them, the failure is the one
    /// the run has alone.
    pub fn fail_past(&mut self, shared: Option<u64>) {
        let find =
            |shared| find_failure(&self.trace, self.job_exit, self.total, self.stopped, shared);
        self.failure = shared.and_then(|s| find(Some(s))).or_else(|| find(None));
    }

    /// Every log line under a filter, in step order.
    pub fn lines(&self, filter: LogFilter) -> &[LogLine] {
        match filter {
            LogFilter::Output => &self.output_lines,
            LogFilter::WithConsole => &self.all_lines,
        }
    }

    /// How many log lines under a filter were complete by `step`.
    pub fn line_count_at(&self, filter: LogFilter, step: u64) -> usize {
        self.lines(filter).partition_point(|l| l.step <= step)
    }

    /// How many file events happened by `step`.
    pub fn file_count_at(&self, step: u64) -> usize {
        self.files.partition_point(|f| f.step <= step)
    }

    /// Whether the trace has a file below `dir` at any step, which makes
    /// `dir` a directory of the run.
    pub fn has_files_under(&self, dir: &str) -> bool {
        let dir = dir.trim_end_matches(PATH_SEPARATOR);
        self.files
            .iter()
            .any(|f| f.op != FileOp::Output && below(&f.path, dir))
    }

    /// Whether the trace has no event that writes `path` or renames a
    /// file to it. Such a path may name a directory, which appears and
    /// goes with no event to say so.
    pub fn never_written(&self, path: &str) -> bool {
        !self
            .files
            .iter()
            .any(|f| f.path == path && matches!(f.op, FileOp::Write | FileOp::Rename))
    }

    /// The files below `dir` at `step`, by path: each file a process
    /// opened for writing or renamed there by then and no process removed
    /// or renamed away since. A renamed directory takes its files along.
    /// The trace has no event for a symbolic link, an empty directory or
    /// a file of the input image, so the listing has none of them.
    pub fn files_under(&self, dir: &str, step: u64) -> Vec<WrittenFile> {
        let dir = dir.trim_end_matches(PATH_SEPARATOR);

        // Every file the trace knows at the step, with the step and
        // process that last wrote or named it. Files outside `dir` are
        // kept too: a rename may bring their directory under it.
        let mut live: HashMap<Cow<str>, (u64, u32)> = HashMap::new();
        for file in &self.files[..self.file_count_at(step)] {
            let at = (file.step, file.pid);
            match file.op {
                FileOp::Write => {
                    live.insert(Cow::Borrowed(&file.path), at);
                }
                FileOp::Unlink => {
                    live.remove(file.path.as_str());
                }
                FileOp::Rename => {
                    let Some(from) = file.from.as_deref() else {
                        continue;
                    };

                    // The files of a renamed directory go to the same
                    // places under its new name.
                    let moved: Vec<Cow<str>> = live
                        .keys()
                        .filter(|path| below(path, from))
                        .cloned()
                        .collect();
                    let renamed_directory = !moved.is_empty();
                    for path in moved {
                        live.remove(&path);
                        let renamed = format!("{}{}", file.path, &path[from.len()..]);
                        live.insert(Cow::Owned(renamed), at);
                    }
                    if renamed_directory {
                        continue;
                    }

                    // Anything else renamed is taken for a file.
                    live.remove(from);
                    live.insert(Cow::Borrowed(&file.path), at);
                }
                FileOp::Output => {}
            }
        }

        // A renamed directory with no file in it yet was taken for a
        // file; a file written under it since shows it is a directory.
        let mut directories: HashSet<&str> = HashSet::new();
        for path in live.keys() {
            let mut rest: &str = path;
            while let Some((parent, _)) = rest.rsplit_once(PATH_SEPARATOR) {
                if !directories.insert(parent) {
                    break;
                }
                rest = parent;
            }
        }

        let mut under: Vec<WrittenFile> = live
            .iter()
            .filter(|(path, _)| below(path, dir) && !directories.contains(path.as_ref()))
            .map(|(path, &(step, pid))| WrittenFile {
                path: path.to_string(),
                step,
                pid,
            })
            .collect();
        under.sort_by(|a, b| a.path.cmp(&b.path));
        under
    }

    /// The process tree rows alive at `step`, in tree order.
    pub fn alive_rows(&self, step: u64) -> impl Iterator<Item = &ProcRow> {
        self.rows.iter().filter(move |r| r.alive_at(step))
    }

    /// The last switch at or before `step`, including idle and kernel tasks.
    pub fn switch_at(&self, step: u64) -> Option<&Switch> {
        let after = self.trace.switches.partition_point(|s| s.step <= step);
        after.checked_sub(1).map(|i| &self.trace.switches[i])
    }

    /// The user thread named by the latest event or switch. Older traces
    /// without switches keep following their last event.
    pub fn active_thread(&self, step: u64) -> Option<(u32, u32)> {
        let event = self.event_index_at(step).and_then(|i| self.event(i));

        // A switch replaces the last event's thread, even without a new event.
        if let Some(switch) = self.switch_at(step)
            && event.is_none_or(|event| switch.step >= event.step)
        {
            return (switch.memory && !switch.is_idle()).then_some((switch.pid, switch.tid));
        }

        // A later event can change identity at exec, or name a thread that exits.
        let event = event?;
        (event.pid != KERNEL_PID).then_some((event.pid, event.tid))
    }

    /// The active thread's row among the rows alive at `step`, or its
    /// process's row when the thread has no row of its own.
    pub fn active_row(&self, step: u64) -> Option<usize> {
        let (pid, tid) = self.active_thread(step)?;
        self.alive_rows(step)
            .position(|r| r.pid == pid && r.tid == tid)
            .or_else(|| {
                self.alive_rows(step)
                    .position(|r| r.kind == RowKind::Process && r.pid == pid)
            })
    }

    /// The phase `step` falls in: the last one starting at or before it.
    pub fn phase_index_at(&self, step: u64) -> Option<usize> {
        if self.phases.is_empty() {
            return None;
        }
        let after = self.phases.partition_point(|p| p.start <= step);
        Some(after.saturating_sub(1))
    }

    /// The index of the last event at or before `step`.
    pub fn event_index_at(&self, step: u64) -> Option<usize> {
        self.trace.index_after(step).checked_sub(1)
    }

    pub fn event(&self, index: usize) -> Option<&Event> {
        self.trace.events.get(index)
    }

    /// The display name of a pid, if the trace saw it: of the last
    /// process that had the pid, as the process ended. For what is said of
    /// a pid over the whole run; an event's process is `name_at`'s.
    pub fn name_of(&self, pid: u32) -> Option<&str> {
        self.names.get(&pid).map(String::as_str)
    }

    /// The display name of the process that had `pid` at `step`, as the
    /// process was then: the program it had exec'd by the step. A pid is
    /// given out again once its process has exited, and a process execs
    /// more than once, so the name depends on the step.
    pub fn name_at(&self, pid: u32, step: u64) -> Option<String> {
        let row = self.process_row_at(pid, step)?;
        Some(command_name(row.label_at(step)))
    }

    /// The process tree's row of the process that had `pid` at `step`:
    /// the one alive then, else the nearest before, else the first after,
    /// as `Trace::process_at` finds a process.
    fn process_row_at(&self, pid: u32, step: u64) -> Option<&ProcRow> {
        let with_pid = || {
            self.rows
                .iter()
                .filter(move |r| r.kind == RowKind::Process && r.pid == pid)
        };
        if let Some(alive) = with_pid().find(|r| r.alive_at(step)) {
            return Some(alive);
        }
        let before = with_pid()
            .filter(|r| r.start <= step)
            .max_by_key(|r| r.start);
        before.or_else(|| with_pid().min_by_key(|r| r.start))
    }

    /// How a file row is drawn at `step`.
    pub fn file_tone(&self, file: &FileEvent, step: u64) -> FileTone {
        if file.is_core_dump() {
            return FileTone::Error;
        }
        if file.op == FileOp::Unlink {
            return FileTone::Gone;
        }
        if file.op == FileOp::Output {
            return FileTone::Output;
        }
        let recent = (self.total as f64 * RECENT_SHARE) as u64;
        if step.saturating_sub(file.step) <= recent {
            return FileTone::Recent;
        }
        FileTone::Normal
    }

    /// Where a motion takes the playhead from `step`. `divergence` is the
    /// step of the first divergence from a compared run, if there is one.
    /// A motion with nowhere to go leaves the playhead where it is.
    pub fn seek(&self, motion: Motion, step: u64, divergence: Option<u64>) -> u64 {
        let step = step.min(self.total);
        match motion {
            Motion::Start => 0,
            Motion::End => self.total,
            Motion::PreviousEvent => self.trace.previous_step(step).unwrap_or(0),
            Motion::NextEvent => self
                .trace
                .next_step(step)
                .unwrap_or(self.total)
                .min(self.total),
            Motion::StepBack => step.saturating_sub(1),
            Motion::StepForward => (step + 1).min(self.total),
            Motion::PreviousPhase => self
                .phases
                .iter()
                .rev()
                .map(|p| p.start)
                .find(|start| *start < step)
                .unwrap_or(0),
            Motion::NextPhase => self
                .phases
                .iter()
                .map(|p| p.start)
                .find(|start| *start > step)
                .unwrap_or(self.total),
            Motion::Failure => self.failure.map_or(step, |f| f.step),
            Motion::Divergence => divergence.unwrap_or(step),
        }
    }

    /// The step under a point `fraction` of the way along the timeline.
    pub fn step_at_fraction(&self, fraction: f32) -> u64 {
        let fraction = fraction.clamp(0.0, 1.0) as f64;
        (fraction * self.total as f64).round() as u64
    }

    /// Where `step` sits along the timeline, from 0 to 1.
    pub fn fraction_of(&self, step: u64) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (step.min(self.total) as f64 / self.total as f64) as f32
    }
}

pub use rewind_trace::compare::{Comparison, DivergencePoint, Side};

/// Whether a line reads as an error.
pub fn is_error_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    ERROR_MARKERS.iter().any(|m| starts_a_word(&lower, m))
}

/// Whether `marker` is in `text` at the start of a word: not inside one,
/// as "error" is in a compiler flag such as -Werror or -Wno-error=x.
fn starts_a_word(text: &str, marker: &str) -> bool {
    text.match_indices(marker).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'))
    })
}

/// The text of a mark someone wrote to /dev/rewind on purpose: not
/// empty, and not one of init's own, which shape the timeline and the
/// verdict instead, nor one of the Rewind SDK's, which the log and the
/// timeline show as assertions and tests.
fn user_mark(event: &Event) -> Option<&str> {
    let EventKind::Mark { text } = &event.kind else {
        return None;
    };
    if text.trim().is_empty() || event.init_mark().is_some() || event.sdk_mark().is_some() {
        return None;
    }
    Some(text)
}

/// Standard output and error as lines, plus user marks, in step order.
fn output_lines(trace: &Trace) -> Vec<LogLine> {
    // Each line as a terminal shows it: a builder's output is a terminal,
    // which programs color and write progress counters over.
    let mut lines: Vec<LogLine> = trace
        .lines_until(u64::MAX)
        .into_iter()
        .map(|line| (shown(&line.text), line))
        .filter(|(shown, _)| !shown.text.starts_with(NIX_LOG_PREFIX))
        .map(|(shown, line)| {
            let stream = if line.fd == STDERR_FD {
                Stream::Stderr
            } else {
                Stream::Stdout
            };
            let tone = tone_of(&shown.text, stream);
            LogLine {
                step: line.step,
                pid: line.pid,
                stream,
                text: shown.text,
                tone,
                pens: shown.pens,
            }
        })
        .collect();

    // User marks join the log as lines of their own, in step order. Init's
    // marks and empty ones stay out.
    let marks: Vec<LogLine> = trace
        .events
        .iter()
        .filter_map(|e| {
            let text = user_mark(e)?;
            Some(LogLine {
                step: e.step,
                pid: e.pid,
                stream: Stream::Mark,
                text: format!("{MARK_LINE_PREFIX}{text}"),
                tone: Tone::Phase,
                pens: Vec::new(),
            })
        })
        .collect();
    if !marks.is_empty() {
        lines = merge_by_step(&lines, &marks);
    }

    // What the job asserted with the SDK and broke, and how its tests
    // ended, join the log too; the rest of its marks stay out.
    let sdk: Vec<LogLine> = trace
        .sdk_marks()
        .filter(|(_, mark)| {
            matches!(
                mark,
                SdkMark::Always { held: false, .. }
                    | SdkMark::Unreachable { .. }
                    | SdkMark::TestEnd { .. }
            )
        })
        .map(|(e, mark)| {
            let (text, broke) = sdk_words(&mark);
            LogLine {
                step: e.step,
                pid: e.pid,
                stream: Stream::Sdk,
                text,
                tone: if broke { Tone::Error } else { Tone::Normal },
                pens: Vec::new(),
            }
        })
        .collect();
    if !sdk.is_empty() {
        lines = merge_by_step(&lines, &sdk);
    }
    lines
}

/// The kernel console, one log line per line of text.
fn console_lines(trace: &Trace) -> Vec<LogLine> {
    let mut lines = Vec::new();
    for e in &trace.events {
        let EventKind::Console { text } = &e.kind else {
            continue;
        };
        for part in text.split('\n').filter(|p| !p.trim().is_empty()) {
            let tone = if is_error_text(part) {
                Tone::Error
            } else {
                Tone::Console
            };
            lines.push(LogLine {
                step: e.step,
                pid: e.pid,
                stream: Stream::Console,
                text: part.trim_end().to_string(),
                tone,
                pens: Vec::new(),
            });
        }
    }
    lines
}

fn tone_of(text: &str, stream: Stream) -> Tone {
    if text.trim_start().starts_with(NIX_PHASE_PREFIX) {
        return Tone::Phase;
    }
    if is_error_text(text) {
        return Tone::Error;
    }
    if stream == Stream::Stderr {
        return Tone::Stderr;
    }
    Tone::Normal
}

/// Two step-ordered lists as one, `a` first among lines of equal step.
fn merge_by_step(a: &[LogLine], b: &[LogLine]) -> Vec<LogLine> {
    let mut merged = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if b[j].step < a[i].step {
            merged.push(b[j].clone());
            j += 1;
            continue;
        }
        merged.push(a[i].clone());
        i += 1;
    }
    merged.extend_from_slice(&a[i..]);
    merged.extend_from_slice(&b[j..]);
    merged
}

/// The timeline's segments: nixpkgs' phases with the "Phase" suffix
/// dropped and user marks, each running to the next. Before them come boot
/// (up to init's start mark) and setup (from the start mark to the first
/// phase); a trace without a start mark gets one opening segment instead.
fn phase_spans(trace: &Trace, lines: &[LogLine], total: u64) -> Vec<PhaseSpan> {
    // Where each named stretch starts: phase lines from the log, and user
    // marks, which the log already holds as lines of their own.
    let mut starts: Vec<(u64, String)> = Vec::new();
    for line in lines {
        let text = line.text.trim();
        if let Some(name) = text.strip_prefix(NIX_PHASE_PREFIX) {
            starts.push((line.step, short_phase_name(name.trim())));
            continue;
        }
        if line.stream == Stream::Mark {
            let name = line.text.trim_start_matches(MARK_LINE_PREFIX);
            starts.push((line.step, name.to_string()));
        }
    }

    // The start mark, if init wrote one, splits boot from the job.
    let start_mark = trace.job_start();
    let first = starts.first().map(|(step, _)| *step);
    let mut openers: Vec<(u64, &str)> = Vec::new();
    match (start_mark, first) {
        (Some(mark), Some(first)) => {
            openers.push((0, BOOT_PHASE));
            if first > mark {
                openers.push((mark, SETUP_PHASE));
            }
        }
        (Some(mark), None) => {
            openers.push((0, BOOT_PHASE));
            openers.push((mark, JOB_PHASE));
        }
        (None, Some(first)) if first > 0 => openers.push((0, OPENING_PHASE)),
        (None, Some(_)) => {}
        (None, None) => openers.push((0, WHOLE_RUN_PHASE)),
    }
    let mut all: Vec<(u64, String)> = openers
        .into_iter()
        .map(|(step, name)| (step, name.to_string()))
        .collect();
    all.extend(starts);

    // Each stretch runs to the next one's start, the last to the end.
    let mut spans = Vec::with_capacity(all.len());
    for (i, (start, name)) in all.iter().enumerate() {
        let end = all.get(i + 1).map_or(total, |(next, _)| *next);
        spans.push(PhaseSpan {
            name: name.clone(),
            start: *start,
            end: end.max(*start),
        });
    }
    spans
}

/// How a user mark's log line starts.
const MARK_LINE_PREFIX: &str = "mark: ";

fn short_phase_name(name: &str) -> String {
    match name.strip_suffix(NIX_PHASE_SUFFIX) {
        Some(short) if !short.is_empty() => short.to_string(),
        _ => name.to_string(),
    }
}

/// Where the run failed. A job whose init reports status 0 did not fail.
/// Otherwise the first crash signal is the failure, and without one, the
/// start of the chain of nonzero exits that ended the run: from the last
/// process to exit nonzero, down through the child it exited after. When
/// a signal killed that child, such as SIGPIPE, the failure is the signal
/// reaching it rather than its exit. A signal or exit before step
/// `shared`, which a passing run made too, is not the failure.
fn find_failure(
    trace: &Trace,
    job_exit: Option<JobExit>,
    total: u64,
    stopped: Stopped,
    shared: Option<u64>,
) -> Option<Failure> {
    // A job that exited 0 failed only where an assertion it made did not
    // hold, as `rewind check`'s sdk property judges it.
    let made_too = |step: u64| shared.is_some_and(|s| step < s);
    if job_exit.is_some_and(|j| j.status == 0) {
        let until = job_exit.map_or(u64::MAX, |j| j.step);
        return trace.events.iter().enumerate().find_map(|(index, e)| {
            if e.step > until || made_too(e.step) {
                return None;
            }
            let broke = matches!(
                e.sdk_mark()?,
                SdkMark::Always { held: false, .. } | SdkMark::Unreachable { .. }
            );
            broke.then_some(Failure {
                step: e.step,
                pid: e.pid,
                tid: e.tid,
                index,
                kind: FailureKind::Assertion,
            })
        });
    }

    // Only what happened up to the job's exit, when init reported one:
    // after it, init tears down what the job left running, and those
    // processes die by its hand, not the job's failure.
    let until = job_exit.map_or(u64::MAX, |j| j.step);

    // A crash signal anywhere wins: the exits after it are its
    // consequences.
    let signal = trace.events.iter().enumerate().find_map(|(index, e)| {
        if e.step > until || made_too(e.step) {
            return None;
        }
        let EventKind::Signal { signo, addr, .. } = e.kind else {
            return None;
        };
        if !signal::FATAL.contains(&signo) {
            return None;
        }
        Some(Failure {
            step: e.step,
            pid: e.pid,
            tid: e.tid,
            index,
            kind: FailureKind::Signal { signo, addr },
        })
    });
    if signal.is_some() {
        return signal;
    }

    // A machine that stopped before the job exited failed where it
    // stopped: the nonzero exits along the way did not end the job. The
    // failing event is the last one the run made.
    if stopped == Stopped::Abnormally && job_exit.is_none() {
        let (index, last) = trace.events.iter().enumerate().next_back()?;
        return Some(Failure {
            step: total,
            pid: last.pid,
            tid: last.tid,
            index,
            kind: FailureKind::Stopped,
        });
    }

    // Processes that exited nonzero; threads are left out, since a
    // thread's exit_code says nothing about its process.
    let procs = trace.processes();
    let failed: Vec<&rewind_trace::Process> = procs
        .iter()
        .filter(|p| p.end.is_some_and(|end| end <= until && !made_too(end)))
        .filter(|p| p.status.is_some_and(|s| s != 0))
        .collect();

    // Walk down from the last nonzero exit to the child it followed.
    let mut current = *failed.iter().max_by_key(|p| p.end)?;
    loop {
        let cause = failed
            .iter()
            .filter(|c| c.parent == current.pid && c.start >= current.start && c.end <= current.end)
            .max_by_key(|c| c.end);
        match cause {
            Some(c) => current = c,
            None => break,
        }
    }

    // A process a signal killed, such as a writer killed by SIGPIPE when
    // the reader of its pipe exited, failed where the signal reached it:
    // its exit only follows from that.
    let status = ExitStatus::from_raw(current.status.unwrap_or_default());
    let end = current.end?;
    if let ExitStatus::Signal {
        signo: killed_by, ..
    } = status
    {
        let delivered = trace
            .events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, e)| {
                if e.pid != current.pid || e.step > end {
                    return None;
                }
                let EventKind::Signal { signo, addr, .. } = e.kind else {
                    return None;
                };
                (signo == killed_by).then_some(Failure {
                    step: e.step,
                    pid: e.pid,
                    tid: e.tid,
                    index,
                    kind: FailureKind::Signal { signo, addr },
                })
            });
        if delivered.is_some() {
            return delivered;
        }
    }

    // Otherwise the failure is that process's exit event.
    let step = end;
    let index = trace.events.iter().position(|e| {
        e.step == step
            && e.pid == current.pid
            && matches!(e.kind, EventKind::Exit { thread: false, .. })
    })?;
    Some(Failure {
        step,
        pid: current.pid,
        tid: current.pid,
        index,
        kind: FailureKind::Exit {
            status: current.status.unwrap_or_default(),
        },
    })
}

/// Opens for writing, unlinks and renames, and init's output marks, in
/// step order. Device files are left out.
fn file_events(trace: &Trace) -> Vec<FileEvent> {
    trace
        .events
        .iter()
        .filter_map(|e| {
            let (op, path, from) = match &e.kind {
                EventKind::Open { path, .. } => (FileOp::Write, path.clone(), None),
                EventKind::Unlink { path } => (FileOp::Unlink, path.clone(), None),
                EventKind::Rename { from, to } => (FileOp::Rename, to.clone(), Some(from.clone())),
                EventKind::Mark { .. } => match e.init_mark()? {
                    rewind_trace::Mark::Output { path, hash } => (FileOp::Output, path, Some(hash)),
                    _ => return None,
                },
                _ => return None,
            };
            if path.starts_with(DEVICE_PREFIX) {
                return None;
            }
            Some(FileEvent {
                step: e.step,
                pid: e.pid,
                op,
                path,
                from,
            })
        })
        .collect()
}

/// The process tree as rows in depth-first order: each process, then its
/// threads, then its children in the order they started.
fn process_rows(trace: &Trace, procs: &[Process]) -> Vec<ProcRow> {
    // The name each thread and process had when it exited, keyed by thread
    // id and exit step so that a reused id finds its own name.
    let mut comms: HashMap<(u32, u64), String> = HashMap::new();
    for e in &trace.events {
        if let EventKind::Exit { comm, .. } = &e.kind {
            comms.insert((e.tid, e.step), comm.clone());
        }
    }

    // Each process's parent is the process with the parent's pid that was
    // alive when the child was forked, which tells reused pids apart.
    let mut by_pid: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        by_pid.entry(p.pid).or_default().push(i);
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); procs.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        let parent = by_pid
            .get(&p.parent)
            .into_iter()
            .flatten()
            .filter(|j| **j != i && procs[**j].start <= p.start)
            .max_by_key(|j| procs[**j].start)
            .copied();
        match parent {
            Some(j) if p.parent != 0 => children[j].push(i),
            _ => roots.push(i),
        }
    }
    let order = |a: &usize, b: &usize| {
        (procs[*a].start, procs[*a].pid).cmp(&(procs[*b].start, procs[*b].pid))
    };
    roots.sort_by(order);
    for list in &mut children {
        list.sort_by(order);
    }

    // Walk the tree depth first with an explicit stack, children pushed in
    // reverse so they pop in start order.
    // Each stack entry carries the parent's command name as of the fork,
    // which names a child until the child execs.
    let mut rows = Vec::with_capacity(procs.len());
    let mut stack: Vec<(usize, usize, Option<String>)> =
        roots.iter().rev().map(|i| (*i, 0, None)).collect();
    while let Some((i, depth, parent_command)) = stack.pop() {
        let p = &procs[i];
        let comm = p.end.and_then(|end| comms.get(&(p.pid, end)));

        // What the process is called while it has not exec'd. The name it
        // exited with is the name of its last program, so it names only a
        // process that never exec'd.
        let unexecd = |exit_name: Option<&String>| {
            if let Some(comm) = exit_name {
                comm.clone()
            } else if p.pid == KTHREADD_PID && p.parent == 0 {
                KTHREADD_NAME.to_string()
            } else if p.parent == KTHREADD_PID {
                KERNEL_THREAD_NAME.to_string()
            } else if let Some(parent) = &parent_command {
                format!("{parent} {FORK_SUFFIX}")
            } else {
                format!("pid {}", p.pid)
            }
        };
        let execd = p.execd && !p.argv.is_empty();
        let label = if execd {
            program_label(&p.argv)
        } else {
            unexecd(comm)
        };

        // What the row was called up to each of its execs: a fork, then
        // each program it exec'd but the last, which `label` holds.
        let mut earlier = Vec::new();
        if execd {
            let mut current = unexecd(None);
            for (at, argv) in &p.execs {
                let next = match argv.is_empty() {
                    true => current.clone(),
                    false => program_label(argv),
                };
                earlier.push((*at, std::mem::replace(&mut current, next)));
            }
        }
        let row = ProcRow {
            pid: p.pid,
            tid: p.pid,
            kind: RowKind::Process,
            label,
            earlier,
            depth,
            start: p.start,
            end: p.end,
            status: p.status,
        };

        // Each child is named after the program this process ran when it
        // forked the child.
        for child in children[i].iter().rev() {
            let command = command_name(row.label_at(procs[*child].start));
            stack.push((*child, depth + 1, Some(command)));
        }
        rows.push(row);

        // Threads sit one level under their process, named by the name
        // they exited with when that differs from the process's own.
        for (tid, start, end) in &p.threads {
            let thread_comm = end.and_then(|end| comms.get(&(*tid, end)));
            let label = match thread_comm {
                Some(name) if Some(name) != comm => format!("thread {name}"),
                _ => format!("thread {tid}"),
            };
            rows.push(ProcRow {
                pid: p.pid,
                tid: *tid,
                kind: RowKind::Thread,
                label,
                earlier: Vec::new(),
                depth: depth + 1,
                start: *start,
                end: *end,
                status: None,
            });
        }
    }
    rows
}

/// A command line as a process label: the program by its file name, since
/// argv[0] is often a long store path, then the arguments as they are.
/// Control characters are escaped, so an `sh -c` script of several lines
/// keeps its process tree row to one line.
fn program_label(argv: &[String]) -> String {
    let program = argv[0].rsplit('/').next().unwrap_or(&argv[0]);
    let mut parts = vec![program];
    parts.extend(argv[1..].iter().map(String::as_str));
    crate::describe::escape_controls(&parts.join(" "))
}

/// The command name in a process label: the file name of its first word,
/// with a fork's suffix left off.
pub fn command_name(label: &str) -> String {
    let first = label.split_whitespace().next().unwrap_or(label);
    first.rsplit('/').next().unwrap_or(first).to_string()
}

/// A spacing between timeline ticks that gives about `target` ticks over
/// `total` steps: 1, 2 or 5 times a power of ten.
pub fn tick_interval(total: u64, target: u64) -> u64 {
    const MULTIPLIERS: [u64; 3] = [1, 2, 5];
    const BASE: u64 = 10;
    let raw = total.div_ceil(target.max(1)).max(1);
    let mut power = 1u64;
    loop {
        for m in MULTIPLIERS {
            let candidate = m.saturating_mul(power);
            if candidate >= raw {
                return candidate;
            }
        }
        power = power.saturating_mul(BASE);
    }
}

/// The steps of the tick marks strictly inside the timeline.
pub fn ticks(total: u64, target: u64) -> Vec<u64> {
    let interval = tick_interval(total, target);
    (1..)
        .map(|k| k * interval)
        .take_while(|step| *step < total)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    // The model over small hand-built traces: each test builds the events a
    // guest would report, indexes them, and checks one answer the UI asks
    // for per frame.
    use super::*;
    use rewind_trace::Event;

    pub(crate) fn ev(step: u64, pid: u32, tid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    pub(crate) fn out(step: u64, pid: u32, fd: u32, text: &str) -> Event {
        ev(
            step,
            pid,
            pid,
            EventKind::Output {
                fd,
                bytes: text.as_bytes().to_vec(),
            },
        )
    }

    pub(crate) fn fork(step: u64, parent: u32, child: u32, thread: bool) -> Event {
        ev(step, parent, parent, EventKind::Fork { child, thread })
    }

    pub(crate) fn exec(step: u64, pid: u32, argv: &[&str]) -> Event {
        ev(
            step,
            pid,
            pid,
            EventKind::Exec {
                filename: argv[0].to_string(),
                argv: argv.iter().map(|a| a.to_string()).collect(),
                old_pid: pid,
            },
        )
    }

    pub(crate) fn exit(
        step: u64,
        pid: u32,
        tid: u32,
        status: u32,
        comm: &str,
        thread: bool,
    ) -> Event {
        ev(
            step,
            pid,
            tid,
            EventKind::Exit {
                status,
                comm: comm.to_string(),
                thread,
            },
        )
    }

    pub(crate) fn signal(step: u64, pid: u32, tid: u32, signo: u32) -> Event {
        ev(
            step,
            pid,
            tid,
            EventKind::Signal {
                signo,
                code: 1,
                addr: 0x10,
            },
        )
    }

    /// A small build: bash runs make, make runs a compiler, a test with a
    /// worker thread crashes, and make exits nonzero afterwards.
    pub(crate) fn build() -> Timeline {
        let events = vec![
            exec(1, 1, &["bash", "-e", "builder.sh"]),
            out(2, 1, 1, "Running phase: buildPhase\n"),
            fork(3, 1, 2, false),
            exec(4, 2, &["make", "-j4"]),
            fork(5, 2, 3, false),
            exec(6, 3, &["cc", "-c", "pool.c"]),
            out(7, 3, 2, "pool.c:3: warning: unused\n"),
            ev(
                8,
                3,
                3,
                EventKind::Open {
                    path: "/build/pool.o".into(),
                    flags: 0o101,
                },
            ),
            exit(9, 3, 3, 0, "cc", false),
            out(10, 1, 1, "Running phase: checkPhase\n"),
            fork(11, 2, 4, false),
            exec(12, 4, &["test_pool"]),
            fork(13, 4, 5, true),
            signal(14, 4, 5, signal::SIGSEGV),
            exit(15, 4, 5, 11, "worker-0", true),
            exit(16, 4, 4, 0x8b, "test_pool", false),
            ev(
                17,
                2,
                2,
                EventKind::Open {
                    path: "/build/core.4".into(),
                    flags: 0o101,
                },
            ),
            out(18, 2, 2, "make: *** [check] Error 2\n"),
            exit(19, 2, 2, 2 << 8, "make", false),
        ];
        Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        )
    }

    #[test]
    fn log_lines_are_counted_by_step_and_toned() {
        // Line counts come from a binary search over the precomputed lines,
        // and each line's tone follows its stream and text.
        let t = build();
        assert_eq!(t.line_count_at(LogFilter::Output, 1), 0);
        assert_eq!(t.line_count_at(LogFilter::Output, 2), 1);
        assert_eq!(t.line_count_at(LogFilter::Output, 9), 2);
        assert_eq!(t.line_count_at(LogFilter::Output, u64::MAX), 4);
        let lines = t.lines(LogFilter::Output);
        assert_eq!(lines[0].tone, Tone::Phase);
        assert_eq!(lines[1].tone, Tone::Stderr);
        assert_eq!(lines[3].tone, Tone::Error);
    }

    #[test]
    fn console_lines_join_the_log_only_when_asked() {
        // A console record lands between output lines by step, and only in
        // the filter that includes the console.
        let mut events = build().trace.events;
        events.insert(
            2,
            ev(
                2,
                0,
                0,
                EventKind::Console {
                    text: "[0.1] booted\n".into(),
                },
            ),
        );
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(t.lines(LogFilter::Output).len(), 4);
        let all = t.lines(LogFilter::WithConsole);
        assert_eq!(all.len(), 5);
        assert_eq!(all[1].stream, Stream::Console);
        assert_eq!(all[1].text, "[0.1] booted");
    }

    #[test]
    fn a_line_written_to_a_terminal_shows_as_the_terminal_shows_it() {
        // A colored line and a counter rewritten in place show their text
        // without escape sequences, and the error in red still reads as
        // an error.
        let t = Timeline::new(
            Trace {
                events: vec![
                    ev(
                        1,
                        7,
                        7,
                        EventKind::Output {
                            fd: 1,
                            bytes: b"[1/2] cc a.c\r[2/2] cc b.c\n".to_vec(),
                        },
                    ),
                    ev(
                        2,
                        7,
                        7,
                        EventKind::Output {
                            fd: 2,
                            bytes: b"\x1b[01;31merror:\x1b[m bad\n".to_vec(),
                        },
                    ),
                ],
                ..Default::default()
            },
            None,
            None,
        );
        let lines = t.lines(LogFilter::Output);
        assert_eq!(lines[0].text, "[2/2] cc b.c");
        assert_eq!(lines[1].text, "error: bad");
        assert_eq!(lines[1].tone, Tone::Error);
    }

    #[test]
    fn an_error_word_marks_a_line_and_a_flag_that_names_one_does_not() {
        // Error words at the start of a word mark a line as an error,
        // plural or in capitals; inside a word, as in a compiler flag such
        // as -Werror, they do not.
        assert!(is_error_text("pool.c:77: error: bad"));
        assert!(is_error_text("3 errors generated."));
        assert!(is_error_text("FAILED: 1 test"));
        assert!(is_error_text("Segmentation fault (core dumped)"));
        assert!(!is_error_text(
            "Compiler for C++ supports arguments -Werror=documentation: NO"
        ));
        assert!(!is_error_text("gcc -Wno-error=unused -c a.c"));
        assert!(!is_error_text("all 12 tests passed"));
    }

    #[test]
    fn a_crash_signal_is_the_failure_even_before_nonzero_exits() {
        // The SIGSEGV at step 14 is chosen over make's exit at step 19.
        let t = build();
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid, f.tid), (14, 4, 5));
        assert_eq!(
            f.kind,
            FailureKind::Signal {
                signo: signal::SIGSEGV,
                addr: 0x10
            }
        );
    }

    #[test]
    fn without_a_crash_the_failure_is_the_deepest_nonzero_exit() {
        // Dropping the signal leaves make exiting 2 last; the child it
        // failed because of is test_pool, dead by a signal. The thread's
        // exit before it does not count.
        let mut events = build().trace.events;
        events.retain(|e| !matches!(e.kind, EventKind::Signal { .. }));
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (16, 4));
        assert_eq!(f.kind, FailureKind::Exit { status: 0x8b });
    }

    #[test]
    fn an_exit_a_passing_run_made_too_is_not_the_failure() {
        // A test whose task fails on purpose: the test process (40) forks
        // the task's shell (43), which exits 1, and later fails itself with
        // 101. Alone, the walk down from 40 ends at the shell. Compared
        // with a passing run that is the same until step 1273, the shell's
        // exit at 1266 is that run's too, and the test's own exit is the
        // failure. With every failure inside the shared steps, the run's
        // own guess stays, and so it does with no passing run to share any.
        let events = vec![
            fork(1264, 40, 43, false),
            exec(1265, 43, &["/bin/sh", "/build/script.sh"]),
            exit(1266, 43, 43, 1 << 8, "script.sh", false),
            exit(1338, 40, 40, 101 << 8, "devenv-tasks-te", false),
        ];
        let mut t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(t.failure.unwrap().step, 1266);
        t.fail_past(Some(1273));
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (1338, 40));
        t.fail_past(Some(2000));
        assert_eq!(t.failure.unwrap().step, 1266);
        t.fail_past(None);
        assert_eq!(t.failure.unwrap().step, 1266);
    }

    fn mark(step: u64, text: &str) -> Event {
        ev(step, 1, 1, EventKind::Mark { text: text.into() })
    }

    #[test]
    fn sdk_marks_are_assertions_and_tests_not_phases() {
        // A job that marks a phase of its own, then a test whose always
        // fails, a passing test, and an unreachable it reaches. Only the
        // phase starts a segment; the failures are assertions in step
        // order, the tests are spans with their results, and the log has
        // a line for each failure and each test's end.
        let sdk = |step: u64, text: &str| ev(step, 41, 41, EventKind::Mark { text: text.into() });
        let events = vec![
            mark(10, "rewind-start"),
            fork(11, 1, 41, false),
            ev(
                20,
                41,
                41,
                EventKind::Mark {
                    text: "load".into(),
                },
            ),
            sdk(30, "rewind-sdk 1 test-begin coin flip"),
            sdk(31, "rewind-sdk 1 always pass even"),
            sdk(32, "rewind-sdk 1 always fail even byte 221 is odd"),
            sdk(33, "rewind-sdk 1 test-end fail coin flip"),
            sdk(40, "rewind-sdk 1 test-begin steady"),
            sdk(41, "rewind-sdk 1 sometimes miss huge-byte"),
            sdk(42, "rewind-sdk 1 test-end pass steady"),
            sdk(50, "rewind-sdk 1 unreachable retry-after-close pool closed"),
            mark(60, "rewind-exit 0"),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );

        let phases: Vec<&str> = t.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(phases, vec![BOOT_PHASE, SETUP_PHASE, "load"]);

        // The job exited 0, so its failure is the first assertion that
        // broke.
        let failure = t.failure.unwrap();
        assert_eq!((failure.step, failure.kind), (32, FailureKind::Assertion));

        let failed: Vec<(u64, &str)> = t
            .assertions
            .iter()
            .map(|a| (a.step, a.text.as_str()))
            .collect();
        assert_eq!(
            failed,
            vec![
                (32, "always \"even\" failed: byte 221 is odd"),
                (50, "unreachable \"retry-after-close\" reached: pool closed"),
            ]
        );
        assert_eq!(
            t.tests,
            vec![
                TestSpan {
                    name: "coin flip".into(),
                    begin: 30,
                    end: Some(33),
                    result: Some(TestResult::Fail),
                },
                TestSpan {
                    name: "steady".into(),
                    begin: 40,
                    end: Some(42),
                    result: Some(TestResult::Pass),
                },
            ]
        );

        let sdk_lines: Vec<(&str, Tone)> = t
            .lines(LogFilter::Output)
            .iter()
            .filter(|l| l.stream == Stream::Sdk)
            .map(|l| (l.text.as_str(), l.tone))
            .collect();
        assert_eq!(
            sdk_lines,
            vec![
                ("always \"even\" failed: byte 221 is odd", Tone::Error),
                ("test coin flip failed", Tone::Error),
                ("test steady passed", Tone::Normal),
                (
                    "unreachable \"retry-after-close\" reached: pool closed",
                    Tone::Error
                ),
            ]
        );
    }

    /// A job the way the guest's init runs one: boot, the start mark, a
    /// shell whose subshell exits 1 along the way (as bash's do), a phase,
    /// make failing because its compiler did, and the exit mark.
    fn job(exit_status: u32) -> Timeline {
        let events = vec![
            mark(10, "rewind-start"),
            mark(10, ""),
            fork(11, 1, 41, false),
            exec(
                12,
                41,
                &[
                    "/nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash",
                    "-e",
                    "builder.sh",
                ],
            ),
            fork(13, 41, 42, false),
            exit(14, 42, 42, 1 << 8, "bash", false),
            ev(
                15,
                41,
                41,
                EventKind::Open {
                    path: "/dev/null".into(),
                    flags: 0o1101,
                },
            ),
            out(20, 41, 1, "Running phase: buildPhase\n"),
            out(
                20,
                41,
                1,
                "@nix { \"action\": \"setPhase\", \"phase\": \"buildPhase\" }\n",
            ),
            fork(21, 41, 43, false),
            exec(22, 43, &["make"]),
            fork(23, 43, 44, false),
            exec(24, 44, &["cc", "-c", "bad.c"]),
            exit(25, 44, 44, 1 << 8, "cc", false),
            exit(26, 43, 43, 2 << 8, "make", false),
            exit(27, 41, 41, exit_status, "bash", false),
            mark(28, &format!("rewind-exit {exit_status}")),
            mark(
                29,
                "rewind-output /nix/store/jgr1axsv7hwwf37n19ssg0fiyaj3bvk7-mylib-0.3.0 ab12",
            ),
        ];
        Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        )
    }

    #[test]
    fn init_marks_shape_the_timeline_and_stay_out_of_the_log() {
        // The start mark ends the boot segment, the stretch up to the
        // first phase is setup, and no init mark, empty mark or @nix line
        // reaches the log.
        let t = job(2 << 8);
        let names: Vec<(&str, u64, u64)> = t
            .phases
            .iter()
            .map(|p| (p.name.as_str(), p.start, p.end))
            .collect();
        assert_eq!(
            names,
            vec![("boot", 0, 10), ("setup", 10, 20), ("build", 20, 29)]
        );
        let texts: Vec<&str> = t
            .lines(LogFilter::Output)
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, vec!["Running phase: buildPhase"]);
        assert_eq!(
            t.job_exit,
            Some(JobExit {
                step: 28,
                status: 2 << 8
            })
        );
    }

    #[test]
    fn a_job_cannot_write_inits_marks() {
        // The failing job, with cc (pid 44) writing init's exit words with
        // status 0 before init's own exit mark: the job still failed with
        // init's status, and the forged words show in the log as the job's
        // own mark.
        let mut events = job(2 << 8).trace.events;
        events.push(ev(
            26,
            44,
            44,
            EventKind::Mark {
                text: "rewind-exit 0".into(),
            },
        ));
        events.sort_by_key(|e| e.step);
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(t.job_exit.map(|j| j.status), Some(2 << 8));
        assert!(t.failure.is_some());
        assert!(
            t.lines(LogFilter::Output)
                .iter()
                .any(|l| l.stream == Stream::Mark && l.text.ends_with("rewind-exit 0"))
        );
    }

    #[test]
    fn device_opens_are_left_out_and_outputs_are_listed() {
        // /dev/null is not a file the job wrote; the output mark is.
        let t = job(0);
        let files: Vec<(FileOp, &str)> = t.files.iter().map(|f| (f.op, f.path.as_str())).collect();
        assert_eq!(
            files,
            vec![(
                FileOp::Output,
                "/nix/store/jgr1axsv7hwwf37n19ssg0fiyaj3bvk7-mylib-0.3.0"
            )]
        );
        assert_eq!(t.files[0].from.as_deref(), Some("ab12"));
    }

    /// A timeline of file events alone: opens for writing, unlinks and
    /// renames by pid 7, at the steps given.
    fn files_timeline(events: Vec<(u64, EventKind)>) -> Timeline {
        const WRITER: u32 = 7;
        let events = events
            .into_iter()
            .map(|(step, kind)| ev(step, WRITER, WRITER, kind))
            .collect();
        Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        )
    }

    fn open(path: &str) -> EventKind {
        EventKind::Open {
            path: path.into(),
            flags: 0,
        }
    }

    #[test]
    fn a_directory_lists_the_files_written_under_it_by_the_step() {
        // Files written under /out and beside it, one of them written
        // twice: the listing at a step has the files under /out written by
        // then, by path, each with the step of its last write, and nothing
        // from /out-dev, whose name only starts the same.
        let t = files_timeline(vec![
            (10, open("/out/bin/shuffle")),
            (20, open("/out-dev/include/shuffle.h")),
            (30, open("/out/README")),
            (40, open("/build/main.o")),
            (50, open("/out/bin/shuffle")),
        ]);
        let listed = |step: u64| -> Vec<(String, u64)> {
            t.files_under("/out", step)
                .into_iter()
                .map(|f| (f.path, f.step))
                .collect()
        };
        assert_eq!(listed(9), vec![]);
        assert_eq!(listed(10), vec![("/out/bin/shuffle".to_string(), 10)]);
        assert_eq!(
            listed(60),
            vec![
                ("/out/README".to_string(), 30),
                ("/out/bin/shuffle".to_string(), 50)
            ]
        );
        assert_eq!(t.files_under("/out/", 60), t.files_under("/out", 60));
        assert!(t.has_files_under("/out"));
        assert!(t.has_files_under("/out/bin"));
        assert!(!t.has_files_under("/out/bin/shuffle"));
        assert!(!t.has_files_under("/nix"));

        // No event writes a directory; each file has one.
        assert!(t.never_written("/out"));
        assert!(!t.never_written("/out/README"));
    }

    #[test]
    fn a_listing_follows_unlinks_and_renames() {
        // A temporary file removed, a file renamed into place, a whole
        // directory renamed into the listed one, and an empty directory
        // renamed and then written under: a removed file leaves the
        // listing, a renamed one is listed under its new name from the
        // step of the rename, and so is each file of a renamed directory.
        // No directory is listed as a file.
        let rename = |from: &str, to: &str| EventKind::Rename {
            from: from.into(),
            to: to.into(),
        };
        let t = files_timeline(vec![
            (10, open("/out/bin/stYiXh1J")),
            (11, open("/out/bin/shuffle.tmp")),
            (12, open("/staging/man/shuffle.1")),
            (
                20,
                EventKind::Unlink {
                    path: "/out/bin/stYiXh1J".into(),
                },
            ),
            (21, rename("/out/bin/shuffle.tmp", "/out/bin/shuffle")),
            (22, rename("/staging", "/out/share")),
            (23, rename("/empty", "/out/lib")),
            (24, open("/out/lib/libshuffle.so")),
        ]);
        let listed: Vec<(String, u64)> = t
            .files_under("/out", 30)
            .into_iter()
            .map(|f| (f.path, f.step))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("/out/bin/shuffle".to_string(), 21),
                ("/out/lib/libshuffle.so".to_string(), 24),
                ("/out/share/man/shuffle.1".to_string(), 22)
            ]
        );
    }

    #[test]
    fn a_job_that_exits_zero_has_no_failure() {
        // The subshell's and make's nonzero exits do not fail a job whose
        // init reports status 0.
        assert_eq!(job(0).failure, None);
    }

    #[test]
    fn a_failing_job_fails_where_its_exit_chain_starts() {
        // bash exits 2 because make did, because cc did: the failure is
        // cc's exit, not the subshell's earlier one.
        let f = job(2 << 8).failure.unwrap();
        assert_eq!((f.step, f.pid), (25, 44));
        assert_eq!(f.kind, FailureKind::Exit { status: 1 << 8 });
    }

    /// SIGPIPE's number on x86-64 Linux.
    const SIGPIPE: u32 = 13;

    /// A shell running `producer | head -1`: head (4) reads a line and
    /// exits 0, the producer subshell (3) gets SIGPIPE on its next write
    /// and dies of it, the shell (2) exits 141 as bash reports that, and
    /// the job's shell (1) fails after it. As Nix's gc-closure test does.
    fn pipeline() -> Vec<rewind_trace::Event> {
        vec![
            exec(1, 1, &["bash", "-e", "builder.sh"]),
            fork(2, 1, 2, false),
            exec(3, 2, &["bash", "-c", "producer | head -1"]),
            fork(4, 2, 3, false),
            fork(5, 2, 4, false),
            exec(6, 4, &["head", "-1"]),
            exit(7, 4, 4, 0, "head", false),
            signal(8, 3, 3, SIGPIPE),
            exit(9, 3, 3, SIGPIPE, "bash", false),
            exit(10, 2, 2, (128 + SIGPIPE) << 8, "bash", false),
            exit(11, 1, 1, 1 << 8, "bash", false),
        ]
    }

    #[test]
    fn a_failure_that_a_signal_killed_is_that_signal() {
        // The exit chain from the job's shell ends at the producer, which
        // SIGPIPE killed: the failure is the SIGPIPE at step 8, where the
        // pipe broke, not the exit it caused a step later.
        let t = Timeline::new(
            Trace {
                events: pipeline(),
                ..Default::default()
            },
            None,
            None,
        );
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid, f.tid), (8, 3, 3));
        assert_eq!(
            f.kind,
            FailureKind::Signal {
                signo: SIGPIPE,
                addr: 0x10
            }
        );
    }

    #[test]
    fn a_pipe_that_broke_harmlessly_is_not_the_failure() {
        // The same pipeline, with the shell shrugging off the broken pipe
        // and exiting 0, and a compiler (5) failing later: SIGPIPE is not
        // a crash, so the failure is the compiler's exit.
        let mut events = pipeline();
        events.retain(|e| e.step < 10);
        events.push(exit(10, 2, 2, 0, "bash", false));
        events.push(fork(11, 1, 5, false));
        events.push(exec(12, 5, &["cc", "-c", "pool.c"]));
        events.push(exit(13, 5, 5, 1 << 8, "cc", false));
        events.push(exit(14, 1, 1, 1 << 8, "bash", false));
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (13, 5));
        assert_eq!(f.kind, FailureKind::Exit { status: 1 << 8 });
    }

    #[test]
    fn what_happens_after_the_job_exits_is_not_its_failure() {
        // The failing job, with a background process the shell started
        // that is still alive when init writes its exit mark at step 28:
        // in the teardown after it, the process crashes and is killed. The
        // failure is still cc's exit, where the job failed.
        let mut events = job(2 << 8).trace.events;
        events.push(fork(21, 41, 45, false));
        events.push(signal(30, 45, 45, signal::SIGSEGV));
        events.push(exit(31, 45, 45, SIGKILL_STATUS, "sleep", false));
        events.sort_by_key(|e| e.step);
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (25, 44));
    }

    /// The exit_code of a process killed by SIGKILL.
    const SIGKILL_STATUS: u32 = 9;

    /// A run the engine stopped at its time limit.
    const HUNG: rewind_trace::stop::Stop =
        rewind_trace::stop::Stop::TimedOut(rewind_trace::stop::Timeout {
            since_exit_ms: 2200,
            doing: rewind_trace::stop::Doing::MakingExits,
        });

    #[test]
    fn a_run_stopped_before_its_job_exited_fails_where_it_stopped() {
        // The failing job without its exit mark, as a hang leaves it, and
        // stopped at step 50: its nonzero exits along the way are not the
        // failure, the step it was stopped at is. A crash signal before
        // the hang still wins.
        let mut events = job(2 << 8).trace.events;
        events.retain(
            |e| !matches!(&e.kind, EventKind::Mark { text } if text.starts_with("rewind-exit")),
        );
        let hung = Timeline::new(
            Trace {
                events: events.clone(),
                ..Default::default()
            },
            Some(50),
            Some(&HUNG),
        );
        let f = hung.failure.unwrap();
        assert_eq!((f.step, f.kind), (50, FailureKind::Stopped));
        assert_eq!(hung.seek(Motion::Failure, 0, None), 50);

        events.push(signal(30, 44, 44, signal::SIGSEGV));
        let crashed = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            Some(50),
            Some(&HUNG),
        );
        assert_eq!(crashed.failure.unwrap().step, 30);
    }

    #[test]
    fn processes_are_named_by_the_file_name_of_their_program() {
        // A store path in argv[0] shows as its file name; the arguments
        // stay as they are.
        let t = job(0);
        let bash = t.rows.iter().find(|r| r.pid == 41).unwrap();
        assert_eq!(bash.label, "bash -e builder.sh");
    }
    #[test]
    fn a_clean_run_has_no_failure() {
        // SIGCHLD is not a crash and a zero exit is not a failure.
        let events = vec![
            exec(1, 1, &["sh"]),
            fork(2, 1, 2, false),
            exit(3, 2, 2, 0, "true", false),
            signal(4, 1, 1, 17),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(t.failure, None);
    }

    #[test]
    fn process_rows_follow_the_tree_with_threads_under_their_process() {
        // Rows come out depth first with depths from the fork parents, and
        // a thread named at exit carries that name.
        let t = build();
        let rows: Vec<(u32, u32, usize, &str)> = t
            .rows
            .iter()
            .map(|r| (r.pid, r.tid, r.depth, r.label.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, 1, 0, "bash -e builder.sh"),
                (2, 2, 1, "make -j4"),
                (3, 3, 2, "cc -c pool.c"),
                (4, 4, 2, "test_pool"),
                (4, 5, 3, "thread worker-0"),
            ]
        );
    }

    #[test]
    fn processes_without_a_command_line_are_named_by_what_they_are() {
        // kthreadd and its children never exec, and a shell's child that
        // forked without exec'ing is named after the shell.
        let events = vec![
            fork(1, 2, 30, false),
            exec(2, 1, &["/bin/sh", "/init"]),
            fork(3, 1, 40, false),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let labels: Vec<(u32, &str)> = t.rows.iter().map(|r| (r.pid, r.label.as_str())).collect();
        assert_eq!(
            labels,
            vec![
                (1, "sh /init"),
                (40, "sh (fork)"),
                (2, "kthreadd"),
                (30, "kernel thread"),
            ]
        );
        assert_eq!(t.name_of(1), Some("sh"));
    }

    #[test]
    fn a_process_is_named_as_it_was_at_the_step() {
        // sh, pid 1, forks pid 5 and pid 6, then execs make. Pid 5 execs
        // sh -c, then gcc, and exits; pid 6 never execs. make forks a new
        // pid 5, which execs ld. Each name is the program the process
        // with the pid at the step had exec'd by then, and a fork's is
        // the program it was forked from at the time.
        let events = vec![
            exec(1, 1, &["/bin/sh", "/init"]),
            fork(10, 1, 5, false),
            fork(12, 1, 6, false),
            exec(20, 5, &["/bin/sh", "-c", "x"]),
            exec(50, 5, &["/usr/bin/gcc", "a.c"]),
            exit(60, 5, 5, 0, "gcc", false),
            exec(65, 1, &["/bin/make"]),
            fork(70, 1, 5, false),
            exec(80, 5, &["/bin/ld", "a.o"]),
        ];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        let name = |pid, step| t.name_at(pid, step);
        assert_eq!(name(5, 15).as_deref(), Some("sh"));
        assert_eq!(name(5, 30).as_deref(), Some("sh"));
        assert_eq!(name(5, 55).as_deref(), Some("gcc"));
        assert_eq!(name(5, 75).as_deref(), Some("make"));
        assert_eq!(name(5, 85).as_deref(), Some("ld"));
        assert_eq!(name(1, 30).as_deref(), Some("sh"));
        assert_eq!(name(1, 66).as_deref(), Some("make"));
        assert_eq!(name(6, 90).as_deref(), Some("sh"));
        assert_eq!(name(9, 30), None);

        // The rows of the process tree are labelled the same way, and a
        // row's own label is the one it ended with.
        let first = t.rows.iter().find(|r| r.pid == 5 && r.start == 10).unwrap();
        assert_eq!(first.label_at(15), "sh (fork)");
        assert_eq!(first.label_at(30), "sh -c x");
        assert_eq!(first.label_at(55), "gcc a.c");
        assert_eq!(first.label, "gcc a.c");
        let never = t.rows.iter().find(|r| r.pid == 6).unwrap();
        assert_eq!(never.label_at(90), "sh (fork)");
    }

    #[test]
    fn a_command_line_with_newlines_stays_on_one_row() {
        // An `sh -c` script of several lines is labelled with its newlines
        // and tabs escaped, as the log shows them, so the process tree row
        // holds one line and draws nothing over the rows below it.
        let events = vec![exec(1, 1, &["/bin/sh", "-c", "cd src\n\tmake"])];
        let t = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(t.rows[0].label, r"sh -c cd src\n\tmake");
        assert_eq!(t.name_of(1), Some("sh"));
    }

    #[test]
    fn alive_rows_change_with_the_step() {
        // At step 7 the compiler runs; at step 14 it has exited and the
        // test and its thread are running.
        let t = build();
        let at = |step| t.alive_rows(step).map(|r| r.tid).collect::<Vec<_>>();
        assert_eq!(at(7), vec![1, 2, 3]);
        assert_eq!(at(14), vec![1, 2, 4, 5]);
        assert_eq!(at(19), vec![1]);
    }

    #[test]
    fn switches_move_the_active_row_without_an_event() {
        // A worker takes the CPU after its parent writes, then idle and kernel tasks run.
        use rewind_trace::switches::Switch;
        let t = Timeline::new(
            Trace {
                events: vec![
                    exec(1, 1, &["parent"]),
                    fork(2, 1, 2, false),
                    fork(3, 2, 3, true),
                    out(4, 1, 1, "waiting\n"),
                    exec(10, 2, &["child"]),
                ],
                switches: vec![
                    Switch {
                        step: 5,
                        pid: 2,
                        tid: 3,
                        memory: true,
                        name: "worker".into(),
                    },
                    Switch {
                        step: 7,
                        pid: 0,
                        tid: 0,
                        memory: false,
                        name: "swapper".into(),
                    },
                    Switch {
                        step: 9,
                        pid: 20,
                        tid: 20,
                        memory: false,
                        name: "kworker".into(),
                    },
                    Switch {
                        step: 12,
                        pid: 2,
                        tid: 2,
                        memory: true,
                        name: "child".into(),
                    },
                ],
            },
            None,
            None,
        );
        assert_eq!(t.active_row(0), None);
        assert_eq!(t.active_row(4), Some(0));
        assert_eq!(t.active_row(5), Some(2));
        assert_eq!(t.active_row(6), Some(2));
        assert_eq!(t.active_row(7), None);
        assert_eq!(t.active_row(9), None);
        assert_eq!(t.active_row(10), Some(1));
        assert_eq!(t.active_row(12), Some(1));
        assert_eq!(t.total, 12);
        assert!(t.switch_at(4).is_none());
        assert_eq!(t.switch_at(5).unwrap().tid, 3);
        assert_eq!(t.switch_at(6).unwrap().tid, 3);
    }

    #[test]
    fn the_last_events_row_is_its_thread_or_else_its_process() {
        // Indexes into the rows alive at each step: the compiler's open at
        // step 8, the worker thread's SIGSEGV at 14, the worker's exit at
        // 15 (its row gone, so the test's), and the test's own exit at 16,
        // when no row of it is alive.
        let t = build();
        assert_eq!(t.active_row(8), Some(2));
        assert_eq!(t.active_row(14), Some(3));
        assert_eq!(t.active_row(15), Some(2));
        assert_eq!(t.active_row(16), None);
    }

    #[test]
    fn files_are_counted_by_step_and_core_dumps_stand_out() {
        // Two opens: the object file and the core dump, which is drawn as
        // an error at any step.
        let t = build();
        assert_eq!(t.file_count_at(7), 0);
        assert_eq!(t.file_count_at(8), 1);
        assert_eq!(t.file_count_at(19), 2);
        assert_eq!(t.file_tone(&t.files[1], 19), FileTone::Error);
        assert!(!t.files[0].is_core_dump());
    }

    #[test]
    fn phases_get_short_names_and_an_opening_segment() {
        // buildPhase starts at step 2, so steps 0 to 2 are the start, and the
        // last phase runs to the end of the timeline.
        let t = build();
        let names: Vec<(&str, u64, u64)> = t
            .phases
            .iter()
            .map(|p| (p.name.as_str(), p.start, p.end))
            .collect();
        assert_eq!(
            names,
            vec![("start", 0, 2), ("build", 2, 10), ("check", 10, 19)]
        );
        assert_eq!(t.phase_index_at(0), Some(0));
        assert_eq!(t.phase_index_at(9), Some(1));
        assert_eq!(t.phase_index_at(19), Some(2));
    }

    #[test]
    fn a_run_without_phases_is_one_segment() {
        // A trace with no phase lines or marks, stretched by a manifest's step.
        let t = Timeline::new(
            Trace {
                events: vec![exec(5, 1, &["sh"])],
                ..Default::default()
            },
            Some(50),
            None,
        );
        assert_eq!(t.total, 50);
        assert_eq!(
            t.phases,
            vec![PhaseSpan {
                name: "run".into(),
                start: 0,
                end: 50
            }]
        );
    }

    #[test]
    fn motions_move_between_events_steps_and_phases() {
        // Every keyboard motion from a few starting points, clamped to the
        // ends of the run.
        let t = build();
        assert_eq!(t.seek(Motion::NextEvent, 9, None), 10);
        assert_eq!(t.seek(Motion::PreviousEvent, 9, None), 8);
        assert_eq!(t.seek(Motion::PreviousEvent, 1, None), 0);
        assert_eq!(t.seek(Motion::NextEvent, 19, None), 19);
        assert_eq!(t.seek(Motion::StepBack, 0, None), 0);
        assert_eq!(t.seek(Motion::StepForward, 19, None), 19);
        assert_eq!(t.seek(Motion::StepForward, 4, None), 5);
        assert_eq!(t.seek(Motion::Start, 12, None), 0);
        assert_eq!(t.seek(Motion::End, 12, None), 19);
        assert_eq!(t.seek(Motion::NextPhase, 0, None), 2);
        assert_eq!(t.seek(Motion::NextPhase, 2, None), 10);
        assert_eq!(t.seek(Motion::NextPhase, 10, None), 19);
        assert_eq!(t.seek(Motion::PreviousPhase, 12, None), 10);
        assert_eq!(t.seek(Motion::PreviousPhase, 10, None), 2);
        assert_eq!(t.seek(Motion::PreviousPhase, 2, None), 0);
        assert_eq!(t.seek(Motion::Failure, 0, None), 14);
        assert_eq!(t.seek(Motion::Divergence, 3, None), 3);
        assert_eq!(t.seek(Motion::Divergence, 3, Some(11)), 11);
    }

    #[test]
    fn fractions_and_steps_convert_both_ways() {
        // The playhead's position along the track and the step under a
        // pointer are inverses, clamped to the track.
        let t = build();
        assert_eq!(t.fraction_of(0), 0.0);
        assert_eq!(t.fraction_of(19), 1.0);
        assert_eq!(t.step_at_fraction(0.5), 10);
        assert_eq!(t.step_at_fraction(-1.0), 0);
        assert_eq!(t.step_at_fraction(2.0), 19);
    }

    #[test]
    fn the_divergence_marker_sits_at_the_first_differing_event() {
        // A second run whose compiler prints a different line at step 7
        // diverges there: test_pool, the culprit, behaves the same in both,
        // so every event is compared. An identical run does not diverge.
        let a = build();
        let mut events = a.trace.events.clone();
        events[6] = out(7, 3, 2, "pool.c:3: warning: other\n");
        let b = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(
            Comparison::of(&a.trace, a.total, &b.trace, b.total).step(),
            Some(7)
        );
        let same = Timeline::new(a.trace.clone(), None, None);
        assert_eq!(
            Comparison::of(&a.trace, a.total, &same.trace, same.total).step(),
            None
        );
    }

    #[test]
    fn a_shift_in_steps_is_not_a_divergence() {
        // The second run does everything two steps later from make's exec
        // on, and where the first run's worker thread takes a SIGSEGV the
        // second run's exits. The first raw difference is the shift at
        // event 3; the comparison is about test_pool and finds the crash.
        let a = build();
        let mut events = a.trace.events.clone();
        for e in &mut events[3..] {
            e.step += 2;
        }
        events[13] = exit(16, 4, 5, 0, "worker-0", true);
        let b = Timeline::new(
            Trace {
                events,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(a.trace.divergence(&b.trace).unwrap().index, 3);

        let c = Comparison::of(&a.trace, a.total, &b.trace, b.total);
        assert_eq!(c.program, Some(vec!["test_pool".to_string()]));
        let p = c.point.unwrap();
        assert_eq!(p.matched, 2);
        assert_eq!(
            p.here,
            Some(Side {
                index: 13,
                thread: Some(1)
            })
        );
        assert_eq!(p.there.map(|s| s.index), Some(13));
        assert_eq!(p.step, 14);
    }

    #[test]
    fn ticks_use_round_intervals() {
        // About twelve ticks at 1, 2 or 5 times a power of ten, never on
        // either end of the timeline.
        assert_eq!(tick_interval(11_760, 12), 1_000);
        assert_eq!(tick_interval(100, 12), 10);
        assert_eq!(tick_interval(1_800_000, 12), 200_000);
        assert_eq!(tick_interval(0, 12), 1);
        assert_eq!(ticks(5_000, 5), vec![1_000, 2_000, 3_000, 4_000]);
    }
}
