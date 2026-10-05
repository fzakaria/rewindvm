//! The scrubber's model of one run.
//!
//! Every question the UI asks on a frame (which lines were printed by step
//! N, which processes were alive, which files were written, which phase the
//! build was in, what happened last) is answered here from tables built
//! once when the run is opened. Each answer is a binary search or a scan of
//! a short list, so dragging the playhead across a trace of any size never
//! decodes or replays the trace again.

use std::collections::HashMap;

use rewind_trace::{Event, EventKind, Trace};

use crate::shown_line::shown;

/// Signal numbers the app treats as a crash.
pub mod signo {
    pub const SIGILL: u32 = 4;
    pub const SIGABRT: u32 = 6;
    pub const SIGBUS: u32 = 7;
    pub const SIGFPE: u32 = 8;
    pub const SIGSEGV: u32 = 11;

    /// A delivered signal in this list means the run crashed.
    pub const FATAL: [u32; 5] = [SIGSEGV, SIGBUS, SIGABRT, SIGILL, SIGFPE];
}

/// The file descriptor of standard error.
const STDERR_FD: u32 = 2;

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

/// Marks the guest's init writes to /dev/rewind (the constants in
/// crates/rewind-init/src/lib.rs). They shape the timeline and the verdict
/// and never show in the log.
mod init_mark {
    /// Every init mark starts with this.
    pub const PREFIX: &str = "rewind-";
    /// Written right before the job starts: everything before is boot.
    pub const START: &str = "rewind-start";
    /// Written when the job exits, followed by its wait status.
    pub const EXIT: &str = "rewind-exit ";
    /// Written per output after a successful job: the path, a space, and
    /// the output's tree hash.
    pub const OUTPUT: &str = "rewind-output ";
}

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

/// The kernel's exit_code keeps the terminating signal in these bits.
const EXIT_SIGNAL_MASK: u32 = 0x7f;
/// The bit of the kernel's exit_code set when a core was dumped.
const EXIT_CORE_BIT: u32 = 0x80;
/// The exit status sits in the second byte of the kernel's exit_code.
const EXIT_STATUS_SHIFT: u32 = 8;
const EXIT_STATUS_MASK: u32 = 0xff;

/// Where a log line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// The guest kernel's console.
    Console,
    /// A mark written to /dev/rewind.
    Mark,
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
    pub label: String,
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

/// A process's end, decoded from the kernel's exit_code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(u32),
    Signal { signo: u32, core: bool },
}

impl ExitStatus {
    pub fn from_raw(raw: u32) -> ExitStatus {
        let signo = raw & EXIT_SIGNAL_MASK;
        if signo == 0 {
            return ExitStatus::Code((raw >> EXIT_STATUS_SHIFT) & EXIT_STATUS_MASK);
        }
        ExitStatus::Signal {
            signo,
            core: raw & EXIT_CORE_BIT != 0,
        }
    }
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

/// The job's exit, as the guest's init reported it with its exit mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobExit {
    pub step: u64,
    /// The job's wait status.
    pub status: u32,
}

/// One run, indexed for scrubbing.
pub struct Timeline {
    pub trace: Trace,
    /// The last step of the run: the right end of the timeline.
    pub total: u64,
    output_lines: Vec<LogLine>,
    all_lines: Vec<LogLine>,
    pub phases: Vec<PhaseSpan>,
    pub rows: Vec<ProcRow>,
    pub files: Vec<FileEvent>,
    pub failure: Option<Failure>,
    /// The job's exit, when init reported it.
    pub job_exit: Option<JobExit>,
    /// The step the job started on, when init wrote its start mark:
    /// before it the VM is still booting.
    pub job_start: Option<u64>,
    /// A display name per process row pid, for describing events.
    names: HashMap<u32, String>,
}

impl Timeline {
    /// Indexes a trace. `total_hint` stretches the timeline past the last
    /// event, for a run whose manifest says it went on longer.
    /// The timeline of `trace`, `total_hint` steps long when the run went
    /// on past its last event, and stopped as the engine's `stop` says.
    pub fn new(trace: Trace, total_hint: Option<u64>, stop: Option<&str>) -> Timeline {
        let total = trace.last_step().max(total_hint.unwrap_or(0));
        let stopped = match stop {
            Some(stop) if !rewind_trace::stop::clean(stop) => Stopped::Abnormally,
            _ => Stopped::Otherwise,
        };
        let output_lines = output_lines(&trace);
        let all_lines = merge_by_step(&output_lines, &console_lines(&trace));
        let phases = phase_spans(&trace, &output_lines, total);
        let rows = process_rows(&trace);
        let files = file_events(&trace);
        let job_exit = job_exit(&trace);
        let job_start = job_start(&trace);
        let failure = find_failure(&trace, job_exit, total, stopped);

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
            files,
            failure,
            job_exit,
            job_start,
            names,
        }
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

    /// The process tree rows alive at `step`, in tree order.
    pub fn alive_rows(&self, step: u64) -> impl Iterator<Item = &ProcRow> {
        self.rows.iter().filter(move |r| r.alive_at(step))
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

    /// The display name of a pid, if the trace saw it.
    pub fn name_of(&self, pid: u32) -> Option<&str> {
        self.names.get(&pid).map(String::as_str)
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

/// Two runs side by side: where the one on screen first behaves
/// differently from the other.
///
/// When the failing run names a culprit program (the one that crashed or
/// failed first), only that program's own events are compared, with
/// threads numbered by the order they appear and steps ignored, as
/// `Trace::divergence_in` does. Otherwise, or when the program behaved the
/// same in both runs, every event is compared by what happened and in
/// which process and thread, still ignoring steps. Either way a difference
/// that is only a shift in steps is not a divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The program compared, when the comparison is about one.
    pub program: Option<Vec<String>>,
    /// Where the runs part; None when they did the same things.
    pub point: Option<DivergencePoint>,
}

/// One run's side of a divergence: the first event that differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Side {
    /// The index of the event in the run's trace.
    pub index: usize,
    /// The event's thread number within the program (0 for its first
    /// thread), when the comparison is about one program.
    pub thread: Option<usize>,
}

/// Where two runs part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DivergencePoint {
    /// How many events matched before.
    pub matched: usize,
    /// The first differing event on this run's side and on the other's;
    /// None on a side whose events ran out first.
    pub here: Option<Side>,
    pub there: Option<Side>,
    /// The step the difference is at in this run: its differing event,
    /// or the last matching one when this run's events ran out first.
    pub step: u64,
}

impl Comparison {
    pub fn new(this: &Timeline, other: &Timeline) -> Comparison {
        let (a, b) = (&this.trace, &other.trace);

        // The culprit's own events first.
        let program = a.culprit_against(b).or_else(|| b.culprit_against(a));
        if let Some(argv) = &program
            && let Some(d) = a.divergence_in(b, argv)
        {
            let side = |event: Option<(usize, usize)>| {
                event.map(|(index, thread)| Side {
                    index,
                    thread: Some(thread),
                })
            };
            let here = side(d.left_event());
            let last_same = d.position.checked_sub(1).map(|i| d.left.indices[i]);
            let point = DivergencePoint {
                matched: d.position,
                here,
                there: side(d.right_event()),
                step: step_of(a, here.map(|s| s.index).or(last_same), this.total),
            };
            return Comparison {
                program,
                point: Some(point),
            };
        }

        // Every event, by what happened and where, but not when.
        let same = |x: &Event, y: &Event| x.pid == y.pid && x.tid == y.tid && x.kind == y.kind;
        let n = a.events.len().min(b.events.len());
        let position = (0..n)
            .find(|&i| !same(&a.events[i], &b.events[i]))
            .or((a.events.len() != b.events.len()).then_some(n));
        let point = position.map(|i| {
            let side = |t: &Trace| {
                (i < t.events.len()).then_some(Side {
                    index: i,
                    thread: None,
                })
            };
            let here = side(a);
            DivergencePoint {
                matched: i,
                here,
                there: side(b),
                step: step_of(a, here.map(|s| s.index).or(i.checked_sub(1)), this.total),
            }
        });
        Comparison {
            program: None,
            point,
        }
    }

    /// The step of the first divergence, in the run on screen.
    pub fn step(&self) -> Option<u64> {
        self.point.as_ref().map(|p| p.step)
    }
}

/// The step of event `index`, or the end of the run without one.
fn step_of(trace: &Trace, index: Option<usize>, total: u64) -> u64 {
    index
        .and_then(|i| trace.events.get(i))
        .map_or(total, |e| e.step)
}

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

/// A mark someone wrote to /dev/rewind on purpose: not empty, and not one
/// of init's own.
fn is_user_mark(text: &str) -> bool {
    !text.trim().is_empty() && !text.starts_with(init_mark::PREFIX)
}

/// Standard output and error as lines, plus user marks, in step order.
fn output_lines(trace: &Trace) -> Vec<LogLine> {
    // Each line as a terminal shows it: a builder's output is a terminal,
    // which programs color and write progress counters over.
    let mut lines: Vec<LogLine> = trace
        .lines_until(u64::MAX)
        .into_iter()
        .map(|line| (shown(&line.text), line))
        .filter(|(text, _)| !text.starts_with(NIX_LOG_PREFIX))
        .map(|(text, line)| {
            let stream = if line.fd == STDERR_FD {
                Stream::Stderr
            } else {
                Stream::Stdout
            };
            let tone = tone_of(&text, stream);
            LogLine {
                step: line.step,
                pid: line.pid,
                stream,
                text,
                tone,
            }
        })
        .collect();

    // User marks join the log as lines of their own, in step order. Init's
    // marks and empty ones stay out.
    let marks: Vec<LogLine> = trace
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Mark { text } if is_user_mark(text) => Some(LogLine {
                step: e.step,
                pid: e.pid,
                stream: Stream::Mark,
                text: format!("{MARK_LINE_PREFIX}{text}"),
                tone: Tone::Phase,
            }),
            _ => None,
        })
        .collect();
    if !marks.is_empty() {
        lines = merge_by_step(&lines, &marks);
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
    let start_mark = job_start(trace);
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

/// The step the job started on, from init's start mark.
fn job_start(trace: &Trace) -> Option<u64> {
    trace.events.iter().find_map(|e| match &e.kind {
        EventKind::Mark { text } if text.trim() == init_mark::START => Some(e.step),
        _ => None,
    })
}

/// The job's exit, from init's exit mark.
fn job_exit(trace: &Trace) -> Option<JobExit> {
    trace.events.iter().find_map(|e| {
        let EventKind::Mark { text } = &e.kind else {
            return None;
        };
        let status = text.strip_prefix(init_mark::EXIT)?.trim().parse().ok()?;
        Some(JobExit {
            step: e.step,
            status,
        })
    })
}

/// Where the run failed. A job whose init reports status 0 did not fail.
/// Otherwise the first crash signal is the failure, and without one, the
/// start of the chain of nonzero exits that ended the run: from the last
/// process to exit nonzero, down through the child it exited after.
fn find_failure(
    trace: &Trace,
    job_exit: Option<JobExit>,
    total: u64,
    stopped: Stopped,
) -> Option<Failure> {
    if job_exit.is_some_and(|j| j.status == 0) {
        return None;
    }

    // Only what happened up to the job's exit, when init reported one:
    // after it, init tears down what the job left running, and those
    // processes die by its hand, not the job's failure.
    let until = job_exit.map_or(u64::MAX, |j| j.step);

    // A crash signal anywhere wins: the exits after it are its
    // consequences.
    let signal = trace.events.iter().enumerate().find_map(|(index, e)| {
        if e.step > until {
            return None;
        }
        let EventKind::Signal { signo, addr, .. } = e.kind else {
            return None;
        };
        if !signo::FATAL.contains(&signo) {
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
        .filter(|p| p.end.is_some_and(|end| end <= until) && p.status.is_some_and(|s| s != 0))
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

    // The failure is that process's exit event.
    let step = current.end?;
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
                EventKind::Mark { text } => {
                    let rest = text.strip_prefix(init_mark::OUTPUT)?;
                    let (path, hash) = rest.trim().rsplit_once(' ')?;
                    (FileOp::Output, path.to_string(), Some(hash.to_string()))
                }
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
fn process_rows(trace: &Trace) -> Vec<ProcRow> {
    let procs = trace.processes();

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
    // Each stack entry carries the parent's command name, which names a
    // child that forked without exec'ing.
    let mut rows = Vec::with_capacity(procs.len());
    let mut stack: Vec<(usize, usize, Option<String>)> =
        roots.iter().rev().map(|i| (*i, 0, None)).collect();
    while let Some((i, depth, parent_command)) = stack.pop() {
        let p = &procs[i];
        let comm = p.end.and_then(|end| comms.get(&(p.pid, end)));
        let label = if p.execd && !p.argv.is_empty() {
            program_label(&p.argv)
        } else if let Some(comm) = comm {
            comm.clone()
        } else if p.pid == KTHREADD_PID && p.parent == 0 {
            KTHREADD_NAME.to_string()
        } else if p.parent == KTHREADD_PID {
            KERNEL_THREAD_NAME.to_string()
        } else if let Some(parent) = &parent_command {
            format!("{parent} {FORK_SUFFIX}")
        } else {
            format!("pid {}", p.pid)
        };
        let command = command_name(&label);
        rows.push(ProcRow {
            pid: p.pid,
            tid: p.pid,
            kind: RowKind::Process,
            label,
            depth,
            start: p.start,
            end: p.end,
            status: p.status,
        });

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
                depth: depth + 1,
                start: *start,
                end: *end,
                status: None,
            });
        }

        for child in children[i].iter().rev() {
            stack.push((*child, depth + 1, Some(command.clone())));
        }
    }
    rows
}

/// A command line as a process label: the program by its file name, since
/// argv[0] is often a long store path, then the arguments as they are.
fn program_label(argv: &[String]) -> String {
    let program = argv[0].rsplit('/').next().unwrap_or(&argv[0]);
    let mut parts = vec![program];
    parts.extend(argv[1..].iter().map(String::as_str));
    parts.join(" ")
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
mod tests {
    // The model over small hand-built traces: each test builds the events a
    // guest would report, indexes them, and checks one answer the UI asks
    // for per frame.
    use super::*;
    use rewind_trace::Event;

    fn ev(step: u64, pid: u32, tid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    fn out(step: u64, pid: u32, fd: u32, text: &str) -> Event {
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

    fn fork(step: u64, parent: u32, child: u32, thread: bool) -> Event {
        ev(step, parent, parent, EventKind::Fork { child, thread })
    }

    fn exec(step: u64, pid: u32, argv: &[&str]) -> Event {
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

    fn exit(step: u64, pid: u32, tid: u32, status: u32, comm: &str, thread: bool) -> Event {
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

    fn signal(step: u64, pid: u32, tid: u32, signo: u32) -> Event {
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
    fn build() -> Timeline {
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
            signal(14, 4, 5, signo::SIGSEGV),
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
        Timeline::new(Trace { events }, None, None)
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
        let t = Timeline::new(Trace { events }, None, None);
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
                signo: signo::SIGSEGV,
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
        let t = Timeline::new(Trace { events }, None, None);
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (16, 4));
        assert_eq!(f.kind, FailureKind::Exit { status: 0x8b });
    }

    fn mark(step: u64, text: &str) -> Event {
        ev(step, 1, 1, EventKind::Mark { text: text.into() })
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
        Timeline::new(Trace { events }, None, None)
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

    #[test]
    fn what_happens_after_the_job_exits_is_not_its_failure() {
        // The failing job, with a background process the shell started
        // that is still alive when init writes its exit mark at step 28:
        // in the teardown after it, the process crashes and is killed. The
        // failure is still cc's exit, where the job failed.
        let mut events = job(2 << 8).trace.events;
        events.push(fork(21, 41, 45, false));
        events.push(signal(30, 45, 45, signo::SIGSEGV));
        events.push(exit(31, 45, 45, SIGKILL_STATUS, "sleep", false));
        events.sort_by_key(|e| e.step);
        let t = Timeline::new(Trace { events }, None, None);
        let f = t.failure.unwrap();
        assert_eq!((f.step, f.pid), (25, 44));
    }

    /// The exit_code of a process killed by SIGKILL.
    const SIGKILL_STATUS: u32 = 9;

    /// How the engine words a run it stopped at its time limit.
    const HUNG: &str = "timed out computing without exits for 2.2s, in user space at 0x41b33e";

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
            },
            Some(50),
            Some(HUNG),
        );
        let f = hung.failure.unwrap();
        assert_eq!((f.step, f.kind), (50, FailureKind::Stopped));
        assert_eq!(hung.seek(Motion::Failure, 0, None), 50);

        events.push(signal(30, 44, 44, signo::SIGSEGV));
        let crashed = Timeline::new(Trace { events }, Some(50), Some(HUNG));
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
        let t = Timeline::new(Trace { events }, None, None);
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
        let t = Timeline::new(Trace { events }, None, None);
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
        let b = Timeline::new(Trace { events }, None, None);
        assert_eq!(Comparison::new(&a, &b).step(), Some(7));
        let same = Timeline::new(a.trace.clone(), None, None);
        assert_eq!(Comparison::new(&a, &same).step(), None);
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
        let b = Timeline::new(Trace { events }, None, None);
        assert_eq!(a.trace.divergence(&b.trace).unwrap().index, 3);

        let c = Comparison::new(&a, &b);
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
    fn exit_status_decodes_codes_and_signals() {
        // exit(2) and a SIGSEGV with a core dump, as the kernel encodes them.
        assert_eq!(ExitStatus::from_raw(2 << 8), ExitStatus::Code(2));
        assert_eq!(
            ExitStatus::from_raw(0x8b),
            ExitStatus::Signal {
                signo: 11,
                core: true
            }
        );
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
