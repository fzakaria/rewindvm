//! Opening a run from disk.
//!
//! A run is a directory holding `manifest.json` and `trace.bin`, or a bare
//! trace file. The manifest is read with the engine's own types
//! (rewind_trace::manifest). One this build cannot read, such as one
//! another build of the engine wrote, is set aside with a warning, and the
//! run opens as its trace alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rewind_trace::Trace;
use rewind_trace::answers::Carried;
use rewind_trace::ending::{self, ExitStatus};
use rewind_trace::io_records::{self, IoLog};
use rewind_trace::manifest::{MANIFEST, Manifest, Parent, RunId, Source, Spec, TRACE, executing};
use rewind_trace::schedule::{Schedule, stall_words};
use rewind_trace::stop::Stop;
use rewind_trace::verdict::Verdict as RecordedVerdict;

use crate::archive;
use crate::describe::{self, thousands};
use crate::model::{Comparison, Timeline};

/// Where a run came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A run directory on this machine.
    Local,
    /// A bare trace file, without the run directory around it.
    TraceFile,
    /// A `.rwd` export, or a URL of one, with its trace unpacked into the
    /// cache.
    Export(Export),
    /// One of the example runs compiled into the app.
    Example,
}

/// Where an export came from, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// The file or URL the run was opened from.
    pub source: PathBuf,
    /// The export on this machine: the file itself, or the download of
    /// the URL.
    pub file: PathBuf,
    /// Whether it holds the keyframes, pages and inputs replay needs, so
    /// the engine can import it.
    pub replayable: bool,
}

/// What a run opened from memory is: an example compiled in, or an export
/// the browser viewer's page fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InMemoryOrigin {
    Example,
    Export,
}

pub struct Run {
    /// The path the run is read from: its directory, or its trace file. An
    /// export's is the directory it was unpacked into.
    pub path: PathBuf,
    pub origin: Origin,
    /// What the engine recorded of the run; None for a bare trace, and for
    /// a manifest this build cannot read.
    pub manifest: Option<Manifest>,
    /// Why the manifest was set aside, when it did not parse.
    pub manifest_warning: Option<String>,
    /// The run indexed for scrubbing, shared with work in the background
    /// that reads the whole of it, such as building the search index.
    pub timeline: Arc<Timeline>,
    /// The run's read and write records: which step wrote each byte of
    /// the files it made. Empty for a run recorded before runs made them.
    pub io: IoLog,
    /// The bookmarks file an export opened from memory carried; a run on
    /// disk keeps its bookmarks in its directory instead.
    pub carried_bookmarks: Option<Vec<u8>>,
    /// What a replay found at some steps, which an export opened from
    /// memory carried when it was written with answers: the browser
    /// viewer reads files and source there without an engine.
    pub answers: Option<Arc<Carried>>,
}

impl Run {
    /// Whether the run carries answers at `step`, which the browser viewer
    /// shows in place of the desktop app's notice.
    pub fn answers_at(&self, step: u64) -> bool {
        self.answers.as_ref().is_some_and(|a| a.at(step).is_some())
    }

    /// Opens a run directory, a `.rwd` export, an http or https URL of
    /// one, or a bare trace file. The manifest or trace inside a run
    /// directory opens the directory, so one file picker reaches them all.
    pub fn open(path: &Path) -> Result<Run> {
        if let Some(dir) = run_dir_of(path) {
            return Run::open_at(&dir, Origin::Local);
        }
        let file = if archive::is_url(path) {
            Some(archive::download(&path.to_string_lossy())?)
        } else {
            Some(path.to_path_buf()).filter(|p| p.is_file() && archive::is_export(p))
        };
        if let Some(file) = file {
            let unpacked = archive::import_file(&file)?;
            let export = Export {
                source: path.to_path_buf(),
                file,
                replayable: unpacked.replayable,
            };
            return Run::open_at(&unpacked.dir, Origin::Export(export));
        }
        let origin = if path.is_dir() {
            Origin::Local
        } else {
            Origin::TraceFile
        };
        Run::open_at(path, origin)
    }

    /// Opens an example run compiled into the app.
    pub fn open_example(bytes: &[u8]) -> Result<Run> {
        let dir = archive::import_bytes(bytes)?.dir;
        Run::open_at(&dir, Origin::Example)
    }

    /// Opens an export held in memory, without unpacking it to disk, as
    /// the browser build opens the exports its page fetched. `name` stands
    /// in for the path the run was read from.
    pub fn open_bytes(name: &str, bytes: &[u8], origin: InMemoryOrigin) -> Result<Run> {
        let export = archive::read_bytes(bytes).with_context(|| format!("reading {name}"))?;
        let path = PathBuf::from(name);
        let origin = match origin {
            InMemoryOrigin::Example => Origin::Example,
            InMemoryOrigin::Export => Origin::Export(Export {
                source: path.clone(),
                file: path.clone(),
                replayable: export.replayable,
            }),
        };

        // The manifest is optional here too: one that does not parse
        // leaves the trace to open alone.
        let (manifest, manifest_warning) = match Manifest::parse(&export.manifest) {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let (trace, io) = rewind_trace::records_from(&export.trace[..])
            .and_then(|records| decode(&records))
            .with_context(|| format!("reading the trace in {name}"))?;
        let mut run = Run::with_trace(path, origin, manifest, manifest_warning, trace, io);
        run.carried_bookmarks = export.bookmarks;
        run.answers = export.answers.map(Arc::new);
        Ok(run)
    }

    /// A run of a trace already read, and its manifest.
    fn with_trace(
        path: PathBuf,
        origin: Origin,
        manifest: Option<Manifest>,
        manifest_warning: Option<String>,
        trace: Trace,
        io: IoLog,
    ) -> Run {
        let outcome = manifest.as_ref().and_then(|m| m.outcome.as_ref());
        let total_hint = outcome.map(|o| o.step);
        let stop = outcome.map(|o| &o.stop);
        let timeline = Arc::new(Timeline::new(trace, total_hint, stop));
        Run {
            path,
            origin,
            manifest,
            manifest_warning,
            timeline,
            io,
            carried_bookmarks: None,
            answers: None,
        }
    }

    fn open_at(path: &Path, origin: Origin) -> Result<Run> {
        let (trace_path, manifest_path) = if path.is_dir() {
            (path.join(TRACE), Some(path.join(MANIFEST)))
        } else {
            (path.to_path_buf(), None)
        };
        if !trace_path.is_file() {
            if let Some(why) = Some(path).filter(|p| p.is_dir()).and_then(not_ended) {
                bail!(why);
            }
            bail!("no trace at {}", trace_path.display());
        }

        // The manifest is optional, and one that does not read leaves the
        // trace to open alone.
        let (manifest, manifest_warning) = match manifest_path.filter(|p| p.is_file()) {
            None => (None, None),
            Some(p) => match read_manifest(&p) {
                Ok(m) => (Some(m), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            },
        };

        let (trace, io) = rewind_trace::records(&trace_path)
            .and_then(|records| decode(&records))
            .with_context(|| format!("reading {}", trace_path.display()))?;
        Ok(Run::with_trace(
            path.to_path_buf(),
            origin,
            manifest,
            manifest_warning,
            trace,
            io,
        ))
    }

    /// A short name: the manifest's, else the directory or file name.
    pub fn name(&self) -> String {
        if let Some(m) = self.manifest.as_ref().filter(|m| !m.name.is_empty()) {
            return m.name.clone();
        }
        let stem = self.path.file_name().or(self.path.file_stem());
        stem.map_or_else(
            || self.path.display().to_string(),
            |s| s.to_string_lossy().into_owned(),
        )
    }

    /// What the run built or ran: the derivation, for a build nix-daemon
    /// described the build's name, else the command, else the path it was
    /// opened from.
    pub fn subject(&self) -> String {
        let Some(m) = &self.manifest else {
            return self.path.display().to_string();
        };
        match &m.source {
            Source::Nix { drv, .. } => drv.clone(),
            Source::Build { .. } => m.name.clone(),
            Source::Image { .. } => m.spec.job.argv.join(" "),
        }
    }

    /// The engine's id for the run: the hash of its inputs.
    pub fn id(&self) -> Option<&RunId> {
        Some(&self.manifest.as_ref()?.id)
    }

    /// The run this one was forked from, and the step it was forked at.
    pub fn parent(&self) -> Option<&Parent> {
        self.manifest.as_ref()?.parent.as_ref()
    }

    /// The note on the mark of the step this run was forked at.
    pub fn parent_note(&self) -> Option<String> {
        let manifest = self.manifest.as_ref()?;
        let parent = manifest.parent.as_ref()?;
        let spec = &manifest.spec;

        // A fork reseeded where it was made took that seed there.
        let reseed = spec
            .reseeds
            .iter()
            .find(|r| r.step == parent.step)
            .map(|r| r.seed);
        Some(parent_note(
            &parent.run.to_string(),
            parent.step,
            spec.schedule,
            reseed,
        ))
    }

    /// The run's verdict as the engine judged it, from its manifest, once
    /// the engine has recorded the run's end.
    pub fn recorded_verdict(&self) -> Option<&RecordedVerdict> {
        let m = self.manifest.as_ref()?;
        m.verdict.is_final().then_some(&m.verdict)
    }

    /// The verdict of a run with no manifest, or whose manifest has no
    /// verdict yet, judged the way the engine judges a run from its trace
    /// alone: by init's exit mark, with no stop and no outputs to miss.
    fn trace_verdict(&self) -> RecordedVerdict {
        let status = self.timeline.job_exit.map(|exit| exit.status);
        RecordedVerdict::judge(&Stop::PoweredOff, status, &[], &self.timeline.trace)
    }

    /// Whether the run passed or failed: as its manifest's verdict says,
    /// which the engine judged when the run ended; a trace without one is
    /// judged the same way from the trace alone.
    pub fn verdict(&self) -> Verdict {
        let passed = match self.recorded_verdict() {
            Some(verdict) => verdict.passed(),
            None => self.trace_verdict().passed(),
        };
        if passed {
            Verdict::Passed
        } else {
            Verdict::Failed
        }
    }

    /// How the machine stopped, once the run has finished.
    pub fn stop(&self) -> Option<&Stop> {
        Some(&self.manifest.as_ref()?.outcome.as_ref()?.stop)
    }

    /// How the machine stopped, at the step it stopped at, when it
    /// stopped any way but its guest powering off, such as at its time
    /// limit; None at any other step or run.
    pub fn stopped_at(&self, step: u64) -> Option<&Stop> {
        let stop = self.stop().filter(|s| !s.is_clean())?;
        (step >= self.timeline.total).then_some(stop)
    }

    /// How the run ended, in the engine's words (exited:2, killed:SIGSEGV,
    /// timed-out, with "assertion failed" after an exit of 0 when the job
    /// broke an assertion it made with the Rewind SDK), from its manifest;
    /// else by init's exit mark, else passed or failed.
    pub fn verdict_label(&self) -> String {
        if let Some(m) = &self.manifest
            && let Some(outcome) = &m.outcome
        {
            return ending::label(outcome, &m.verdict);
        }
        match self.timeline.job_exit {
            Some(exit) => ExitStatus::from_wait(exit.status).to_string(),
            None => self.verdict().label().to_string(),
        }
    }

    /// How the run is named on screen: its id cut short when the engine
    /// gave it one, since runs of one build share a name; else its name.
    pub fn label(&self) -> String {
        match self.id() {
            Some(id) => short_id(id),
            None => self.name(),
        }
    }

    /// The directory of the run this one was forked from, when it sits
    /// next to this one, as runs in one Rewind home do.
    pub fn parent_dir(&self) -> Option<PathBuf> {
        let parent = self.parent()?;
        let dir = self.path.parent()?.join(&parent.run);
        dir.join(MANIFEST).is_file().then_some(dir)
    }
}

/// Whether a run passed or failed, for the header's pills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
        }
    }
}

/// The run directory `path` names when it is the manifest or trace inside
/// one: a directory with a manifest.
fn run_dir_of(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    if name != MANIFEST && name != TRACE {
        return None;
    }
    let dir = path.parent()?;
    dir.join(MANIFEST).is_file().then(|| dir.to_path_buf())
}

/// Why the run in `dir` has no trace, when its manifest says the run has
/// not ended: the engine is still executing it, as it does a fork until
/// the fork finishes, or the execution was killed before it wrote one.
fn not_ended(dir: &Path) -> Option<String> {
    let manifest = read_manifest(&dir.join(MANIFEST)).ok()?;
    if manifest.outcome.is_some() {
        return None;
    }
    let id = short_id(manifest.id.as_str());
    if executing(dir) {
        return Some(format!(
            "run {id} is still running; it opens once it has finished"
        ));
    }
    Some(format!(
        "run {id} was interrupted before it finished, so it has no trace to open"
    ))
}

/// A run id cut to the length people read and type.
pub fn short_id(id: &str) -> String {
    const ID_SHOWN: usize = 8;
    id.chars().take(ID_SHOWN).collect()
}

pub(crate) fn read_manifest(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&bytes).with_context(|| {
        format!(
            "{} was written by another build of rewind, which this app cannot read",
            path.display()
        )
    })
}

/// The run on screen and, optionally, the run it is compared with.
pub struct Session {
    pub run: Run,
    pub other: Option<Run>,
    pub comparison: Option<Comparison>,
    /// Where the two runs' schedules part, when they are runs of the same
    /// inputs on the same machine under different schedules.
    pub split: Option<ScheduleSplit>,
    /// The Compare tab's rows: the two runs' events around where they part.
    pub rows: Vec<crate::compare::Row>,
    /// Why the run asked to be compared with could not be opened, such as
    /// one removed meanwhile, for a notice; the run then opens alone.
    pub unopened_compare: Option<String>,
    /// Whether this build of rewind brings the run to a step as recorded.
    pub replays: Replays,
}

/// Which program's events a session compares its two runs by.
enum Program {
    /// The culprit of the failure, as `Comparison::of` finds it.
    Culprit,
    /// This one, or every event for None.
    Given(Option<Vec<String>>),
}

/// Whether this build of rewind brings a run to a step as it was recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Replays {
    /// As far as any request for the run has found.
    #[default]
    AsRecorded,
    /// A replay went another way than the recording, which every later
    /// one would too: the run was recorded by another build.
    AnotherWay,
}

impl Session {
    pub fn new(run: Run, other: Option<Run>) -> Session {
        Session::compared(run, other, Program::Culprit)
    }

    /// A session of `run` compared with `other` by `program`'s events.
    fn compared(mut run: Run, other: Option<Run>, program: Program) -> Session {
        let comparison = other.as_ref().map(|o| {
            let (this, other) = (&run.timeline, &o.timeline);
            match program {
                Program::Culprit => {
                    Comparison::of(&this.trace, this.total, &other.trace, other.total)
                }
                Program::Given(argv) => {
                    Comparison::about(&this.trace, this.total, &other.trace, other.total, argv)
                }
            }
        });
        let split = match (
            &run.manifest,
            other.as_ref().and_then(|o| o.manifest.as_ref()),
        ) {
            (Some(this), Some(that)) => ScheduleSplit::of(&this.spec, &that.spec),
            _ => None,
        };
        let rows = match (&other, &comparison) {
            (Some(o), Some(c)) => crate::compare::rows(&run.timeline.trace, &o.timeline.trace, c),
            _ => Vec::new(),
        };

        // Beside a passing run, what the two did alike up to the divergence
        // did not fail this one, such as a child exiting nonzero in both.
        let passing_beside = other
            .as_ref()
            .is_some_and(|o| o.verdict() == Verdict::Passed)
            && run.verdict() == Verdict::Failed;
        let shared = match passing_beside {
            true => comparison.as_ref().and_then(Comparison::step),
            false => None,
        };
        // A timeline some background work still reads is copied first.
        Arc::make_mut(&mut run.timeline).fail_past(shared);
        Session {
            run,
            other,
            comparison,
            split,
            rows,
            unopened_compare: None,
            replays: Replays::AsRecorded,
        }
    }

    /// Opens a run and the run to compare it with: the one given, else
    /// the run it was forked from when that is next to it, else none.
    pub fn open(path: &Path, compare: Option<&Path>) -> Result<Session> {
        let run = Run::open(path)?;
        let compare = compare.map(Path::to_path_buf).or_else(|| run.parent_dir());

        // A comparison that does not open leaves the run to open alone.
        let (other, unopened) = match compare.as_deref().map(Run::open) {
            None => (None, None),
            Some(Ok(other)) => (Some(other), None),
            Some(Err(e)) => (None, Some(format!("{e:#}"))),
        };
        let mut session = Session::new(run, other);
        session.unopened_compare = unopened;
        Ok(session)
    }

    /// The step of the compared run that matches `step` of this one: the
    /// same step before their schedules part, since until then they are
    /// the same machine, else as the Compare tab pairs their events. None
    /// without a compared run.
    pub fn matching_step(&self, step: u64) -> Option<u64> {
        let other = self.other.as_ref()?;
        let end = other.timeline.total;
        if self.split.as_ref().is_some_and(|s| step < s.step) {
            return Some(step.min(end));
        }
        let comparison = self.comparison.as_ref()?;
        let (this, that) = (&self.run.timeline.trace, &other.timeline.trace);
        Some(crate::compare::matching_step(this, that, comparison, step).unwrap_or(step.min(end)))
    }

    /// This session the other way round: the compared run on screen,
    /// compared with this one by the same program's events, so the two
    /// views part at the same events. Without a compared run, itself.
    pub fn swapped(self) -> Session {
        let program = self.comparison.as_ref().and_then(|c| c.program.clone());
        match self.other {
            Some(other) => Session::compared(other, Some(self.run), Program::Given(program)),
            None => self,
        }
    }

    /// This session with `run` in place of its run: the same run read
    /// from another directory, as after `rewind import`. The compared run
    /// stays the run it is, opened from where it was, and the two are
    /// compared by the same program's events.
    pub fn with_run(self, run: Run) -> Session {
        let program = self.comparison.as_ref().and_then(|c| c.program.clone());
        let mut session = match self.other {
            Some(other) => Session::compared(run, Some(other), Program::Given(program)),
            None => Session::new(run, None),
        };
        session.replays = self.replays;
        session
    }

    /// The step of the first divergence from the compared run.
    pub fn divergence_step(&self) -> Option<u64> {
        self.comparison.as_ref().and_then(Comparison::step)
    }

    /// The compared run as the divergence card names it: "the passing
    /// run" beside a failing one, else by its id. None without one.
    pub fn other_run(&self) -> Option<String> {
        let other = self.other.as_ref()?;
        Some(
            if other.verdict() == Verdict::Passed && self.run.verdict() == Verdict::Failed {
                "the passing run".to_string()
            } else {
                format!("run {}", other.label())
            },
        )
    }

    /// How the run on screen and the compared run relate, in words. None
    /// without a compared run.
    pub fn agreement(&self) -> Option<Agreement> {
        let other = self.other.as_ref()?;
        let comparison = self.comparison.as_ref()?;
        let other_label = other.label();
        let program = comparison
            .program
            .as_ref()
            .map(|argv| crate::model::command_name(&argv.join(" ")));

        let Some(point) = &comparison.point else {
            let line = match &program {
                Some(p) => format!("{p} did the same things in the same order in both runs."),
                None => "Both runs did the same things in the same order.".to_string(),
            };
            return Some(Agreement::Same {
                title: format!("Same as {other_label}"),
                lines: vec![line],
            });
        };

        // What each run did next, and what differs.
        let name_here = |pid: u32| self.run.timeline.name_at(pid, point.step);
        let difference = describe::difference(
            party(&self.run, point.here),
            party(other, point.there),
            program.as_deref(),
            &name_here,
        );
        // Each line says which run and which step it means.
        let step = thousands(point.step);
        let before = match &program {
            Some(p) => {
                format!("{p} did the same things in the same order in both runs until step {step}.")
            }
            None => format!("Both runs did the same things in the same order until step {step}."),
        };
        // The card leads with how each run ended, then where the two
        // stop being identical, then the first thing done differently.
        let other_run = self.other_run().unwrap_or_default();
        let mut lines = vec![outcome_line(
            self.run.verdict(),
            other.verdict(),
            &other_run,
        )];
        lines.extend(self.split.iter().map(|s| s.line(&other_run)));
        lines.extend([
            before,
            format!("Then in this run, {}.", difference.here),
            format!("In {other_run}, {}.", difference.there),
        ]);
        lines.extend(difference.detail);
        Some(Agreement::Parted {
            step: point.step,
            title: format!(
                "Diverged from {other_label} at step {}",
                thousands(point.step)
            ),
            lines,
        })
    }
}

/// Where two runs of the same inputs on the same machine first differ
/// because their schedules do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSplit {
    /// The first step the two schedules treat differently. Until it the
    /// two runs are the same machine, exit for exit.
    pub step: u64,
    /// What Rewind does to this run there, as [`done_at`] says it, when
    /// its schedule asks for a reschedule or a stall. None when it does
    /// nothing there, or only makes a timer late.
    pub here: Option<String>,
    /// The same for the other run.
    pub there: Option<String>,
}

impl ScheduleSplit {
    /// Where runs of `this` and `other` part by their schedules; None
    /// when anything else differs between them, or nothing does.
    pub fn of(this: &Spec, other: &Spec) -> Option<ScheduleSplit> {
        let step = this.same_through(other)?.checked_add(1)?;
        Some(ScheduleSplit {
            step,
            here: done_at(&this.schedule(), step),
            there: done_at(&other.schedule(), step),
        })
    }

    /// What Rewind does at the split, and to which run, as the end of a
    /// sentence whose subject is Rewind: "pauses the running thread for
    /// 1280 µs in this run only". `other_run` names the run compared with.
    fn what_differs(&self, other_run: &str) -> Option<String> {
        match (&self.here, &self.there) {
            (Some(what), None) => Some(format!("{what} in this run only")),
            (None, Some(what)) => Some(format!("{what} in {other_run} only")),
            _ => None,
        }
    }

    /// The split as a line of the divergence card, `other_run` naming the
    /// run compared with.
    pub fn line(&self, other_run: &str) -> String {
        let step = thousands(self.step);
        let identical = format!("The two runs are identical until step {step}.");
        match self.what_differs(other_run) {
            Some(what) => format!("{identical} There, Rewind {what}."),
            None => format!("{identical} There, Rewind first schedules their threads differently."),
        }
    }

    /// The split for the mark on the timeline, when pointed at.
    pub fn tooltip(&self, other_run: &str) -> String {
        let step = thousands(self.step);
        let before = "The two runs are identical before this step.";
        match self.what_differs(other_run) {
            Some(what) => format!("Step {step}: Rewind {what}. {before}"),
            None => format!(
                "Step {step}: Rewind first schedules the two runs' threads differently. {before}"
            ),
        }
    }
}

/// What Rewind does to a run at `step` under `schedule`, as a verb phrase
/// with Rewind as its subject: "asks the kernel to switch threads",
/// "pauses the running thread for 1280 µs", or both joined with "and".
/// None when the schedule asks for neither there. A late timer is left
/// out: it changes the run only if the guest armed a timer at that exit,
/// and only the monitor saw whether it did.
fn done_at(schedule: &Schedule, step: u64) -> Option<String> {
    let mut parts = Vec::new();

    // A reschedule asks the guest's kernel to run another thread, if one
    // is ready.
    if schedule.preempt_at(step) {
        parts.push("asks the kernel to switch threads".to_string());
    }

    // A stall puts the task running then to sleep when it next returns
    // to user space.
    if let Some(ns) = schedule.stall_at(step) {
        parts.push(format!("pauses the running thread for {}", stall_words(ns)));
    }

    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" and "))
}

/// The divergence card's first line: how the run on screen and
/// `other_run`, the run it is compared with, ended.
fn outcome_line(this: Verdict, other: Verdict, other_run: &str) -> String {
    match (this, other) {
        (Verdict::Failed, Verdict::Passed) => format!("This run failed and {other_run} did not."),
        (Verdict::Passed, Verdict::Failed) => format!("This run passed and {other_run} failed."),
        (both, _) => format!("Both runs {}.", both.label()),
    }
}

/// The mark of the step a fork was made at, when pointed at: the run it
/// came from, its schedule, and the seed it was reseeded with there, if
/// any.
pub fn parent_note(parent: &str, step: u64, schedule: u64, reseed: Option<u64>) -> String {
    let start = format!(
        "Step {}: forked from run {} under schedule {schedule}",
        thousands(step),
        short_id(parent)
    );
    match reseed {
        Some(seed) => format!("{start}, with the program's random seed changed to {seed}"),
        None => start,
    }
}

/// One run's side of a divergence as the event it names.
fn party(run: &Run, side: Option<crate::model::Side>) -> Option<describe::Party<'_>> {
    let side = side?;
    let event = run.timeline.event(side.index)?;
    Some(describe::Party {
        event,
        thread: side.thread,
    })
}

/// How two runs relate, in words for the divergence card and notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agreement {
    /// The runs did the same things.
    Same { title: String, lines: Vec<String> },
    /// The runs part at `step` of the run on screen.
    Parted {
        step: u64,
        title: String,
        lines: Vec<String>,
    },
}

/// A trace file's records as the run's events and its read and write
/// records. Read and write records that do not decode leave the run
/// without them, as a run recorded before they existed is: its events
/// still show.
fn decode(records: &[(u64, Vec<u8>)]) -> std::io::Result<(Trace, IoLog)> {
    let trace = Trace::decode(records)?;
    let io = io_records::decode(records).unwrap_or_else(|e| {
        log::warn!("the run's read and write records do not decode: {e}");
        IoLog::default()
    });
    Ok((trace, io))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_runs_manifest_or_trace_opens_its_directory() {
        // Picking a file inside a run directory reaches the run; a trace
        // with no manifest beside it stays a bare trace.
        let dir = std::env::temp_dir().join(format!("rewind-app-rundir-{}", std::process::id()));
        let run = dir.join("abc");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(MANIFEST), "{}").unwrap();
        assert_eq!(run_dir_of(&run.join(MANIFEST)), Some(run.clone()));
        assert_eq!(run_dir_of(&run.join(TRACE)), Some(run.clone()));
        assert_eq!(run_dir_of(&dir.join(TRACE)), None);
        assert_eq!(run_dir_of(&run.join("other.json")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Opening runs from a temporary directory: each test writes a trace and
    // perhaps a manifest, opens the run, and checks what the app shows.
    use super::*;
    use crate::examples::{FAILING, PASSING, Until, manifest_of, trace_of, trace_until};
    use rewind_trace::manifest::EXECUTING_LOCK;
    use rewind_trace::verdict::{FailedAssertion, Reason};
    use std::os::fd::AsRawFd;

    /// A run stopped at its time limit while computing in user space.
    fn hung_in_user_space() -> Stop {
        Stop::TimedOut(rewind_trace::stop::Timeout {
            since_exit_ms: 2200,
            doing: rewind_trace::stop::Doing::User {
                rip: 0x41b33e,
                thread: None,
            },
        })
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The failing example's trace, which crashes, in `dir`.
    fn small_trace(dir: &Path) {
        std::fs::write(dir.join(TRACE), trace_of(FAILING)).unwrap();
    }

    /// Writes `manifest` into the run directory `dir`.
    fn write_manifest(dir: &Path, manifest: &Manifest) {
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(manifest).unwrap()).unwrap();
    }

    #[test]
    fn a_manifest_names_the_run_and_fields_it_does_not_know_are_ignored() {
        // The failing example's manifest, renamed and with a field the
        // engine does not write: the run takes the name and the derivation
        // it built, with no warning.
        let dir = temp_dir("manifest");
        small_trace(&dir);
        let mut manifest = serde_json::to_value(manifest_of(FAILING)).unwrap();
        manifest["name"] = "run #3".into();
        manifest["extra"] = serde_json::json!([1, 2]);
        std::fs::write(dir.join(MANIFEST), manifest.to_string()).unwrap();
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.name(), "run #3");
        assert!(
            run.subject().ends_with("-mylib-0.3.0.drv"),
            "{}",
            run.subject()
        );
        assert_eq!(run.manifest_warning, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_manifest_is_set_aside_with_a_warning() {
        // A manifest that is not JSON, and one another build wrote without
        // a field this one needs, leave the run named after its directory.
        let dir = temp_dir("broken");
        small_trace(&dir);
        std::fs::write(dir.join(MANIFEST), "{not json").unwrap();
        let run = Run::open(&dir).unwrap();
        assert!(run.manifest_warning.is_some());
        assert_eq!(run.name(), dir.file_name().unwrap().to_string_lossy());

        let mut older = serde_json::to_value(manifest_of(FAILING)).unwrap();
        older.as_object_mut().unwrap().remove("recorded_by");
        std::fs::write(dir.join(MANIFEST), older.to_string()).unwrap();
        let run = Run::open(&dir).unwrap();
        assert!(run.manifest.is_none());
        assert!(run.manifest_warning.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bare_trace_file_opens_and_a_missing_one_does_not() {
        // A trace file opened by itself, and a path with nothing there.
        let dir = temp_dir("bare");
        small_trace(&dir);
        let run = Run::open(&dir.join(TRACE)).unwrap();
        assert_eq!(run.name(), TRACE);
        assert_eq!(run.verdict(), Verdict::Failed);
        assert!(Run::open(&dir.join("nope.bin")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_with_no_trace_yet_says_whether_it_is_still_running() {
        // A fork the engine is still executing has a manifest with no
        // outcome and no trace until it finishes; one whose execution was
        // killed stays that way. Opening either says which of the two it
        // is.
        let dir = temp_dir("no-trace-yet");
        let mut manifest = manifest_of(FAILING);
        manifest.outcome = None;
        write_manifest(&dir, &manifest);
        let id = short_id(manifest.id.as_str());

        let lock = std::fs::File::create(dir.join(EXECUTING_LOCK)).unwrap();
        lock.try_lock().unwrap();
        let running = format!("{:#}", Run::open(&dir).err().unwrap());
        assert_eq!(
            running,
            format!("run {id} is still running; it opens once it has finished")
        );

        // The lock is released by hand rather than by dropping the file:
        // a child another test forks meanwhile inherits the descriptor and
        // keeps the lock held until the child execs, so a dropped lock can
        // still be held when the run is opened again.
        // SAFETY: flock on a descriptor this test owns and keeps open.
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
        let interrupted = format!("{:#}", Run::open(&dir).err().unwrap());
        assert_eq!(
            interrupted,
            format!("run {id} was interrupted before it finished, so it has no trace to open")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_without_a_wait_status_fails_however_it_stopped() {
        // The passing example's boot, before its job, under manifests with
        // no wait status and each way a machine stops, judged as the
        // engine judges them: one stopped at its time limit says
        // timed-out, as `rewind ls` does, and the others no-status. None
        // of them passed: the job never reported its end.
        let dir = temp_dir("stops");
        std::fs::write(dir.join(TRACE), trace_until(PASSING, Until::JobStart)).unwrap();
        let with_stop = |stop: Stop, reason: Reason| {
            let mut manifest = manifest_of(PASSING);
            let outcome = manifest.outcome.as_mut().unwrap();
            outcome.stop = stop;
            outcome.status = None;
            manifest.verdict = RecordedVerdict::Failed {
                reasons: vec![reason],
            };
            write_manifest(&dir, &manifest);
            Run::open(&dir).unwrap()
        };

        let hung = with_stop(hung_in_user_space(), Reason::TimedOut);
        assert_eq!(hung.verdict(), Verdict::Failed);
        assert_eq!(hung.verdict_label(), "timed-out");
        assert_eq!(
            with_stop(Stop::TripleFault, Reason::Crashed).verdict(),
            Verdict::Failed
        );
        let powered_off = with_stop(Stop::PoweredOff, Reason::Crashed);
        assert_eq!(powered_off.verdict(), Verdict::Failed);
        assert_eq!(powered_off.verdict_label(), "no-status");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_stopped_at_its_time_limit_says_so_where_it_stopped() {
        // A run the engine stopped at its time limit gives the engine's
        // words for it at the step it stopped at and after, and none
        // before; a run that powered off gives none anywhere.
        let dir = temp_dir("hang-words");
        std::fs::write(dir.join(TRACE), trace_until(PASSING, Until::JobExit)).unwrap();
        let hung = hung_in_user_space();
        let with_stop = |stop: &Stop| {
            let mut manifest = manifest_of(PASSING);
            let outcome = manifest.outcome.as_mut().unwrap();
            outcome.stop = stop.clone();
            outcome.status = None;
            outcome.step = 0;
            write_manifest(&dir, &manifest);
            Run::open(&dir).unwrap()
        };

        let run = with_stop(&hung);
        let end = run.timeline.total;
        assert_eq!(run.stopped_at(end), Some(&hung));
        assert_eq!(run.stopped_at(end - 1), None);
        assert_eq!(with_stop(&Stop::PoweredOff).stopped_at(end), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_whose_comparison_is_gone_opens_alone() {
        // A run asked to open beside a run that was removed meanwhile opens
        // by itself, with no comparison, and says why for a notice; beside
        // a run that is there, it says nothing.
        let dir = temp_dir("compare-gone");
        small_trace(&dir);
        let session = Session::open(&dir, Some(&dir.join("removed"))).unwrap();
        assert!(session.other.is_none());
        assert!(session.comparison.is_none());
        assert!(session.unopened_compare.is_some());
        let beside_itself = Session::open(&dir, Some(&dir)).unwrap();
        assert!(beside_itself.unopened_compare.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_header_reads_the_verdict_the_engine_wrote() {
        // The passing example under manifests whose verdicts the engine
        // judged: failed for an exit of 1, passed for an exit of 0, failed
        // for an output the job did not create, and failed for an
        // assertion the job broke before exiting 0. The header, the label
        // and the Runs panel's entry all read the one field, so a run
        // that fails by assertion alone is failed in each.
        let dir = temp_dir("verdict");
        std::fs::write(dir.join(TRACE), trace_of(PASSING)).unwrap();
        let judged = |status: i32, verdict: RecordedVerdict| {
            let mut manifest = manifest_of(PASSING);
            manifest.outcome.as_mut().unwrap().status = Some(status);
            manifest.verdict = verdict;
            write_manifest(&dir, &manifest);
            let entry = crate::family::RunEntry::read(&dir).unwrap();
            (Run::open(&dir).unwrap(), entry)
        };
        let failed = |reason| RecordedVerdict::Failed {
            reasons: vec![reason],
        };

        let (run, entry) = judged(1 << 8, failed(Reason::Exited { code: 1 }));
        assert_eq!(run.verdict(), Verdict::Failed);
        assert_eq!(run.verdict_label(), "exited:1");
        assert!(entry.failed && !entry.passed);

        let (run, entry) = judged(0, RecordedVerdict::Passed);
        assert_eq!(run.verdict(), Verdict::Passed);
        assert_eq!(run.verdict_label(), "exited:0");
        assert!(entry.passed && !entry.failed);

        let dev = "/nix/store/0000000000000000000000000000000c-x-dev".to_string();
        let (run, entry) = judged(0, failed(Reason::MissingOutputs { paths: vec![dev] }));
        assert_eq!(run.verdict(), Verdict::Failed);
        assert_eq!(run.verdict_label(), "missing-output");
        assert_eq!(entry.ending, "missing-output");
        assert!(entry.failed);

        let broke = Reason::FailedAssertions {
            assertions: vec![FailedAssertion {
                step: 30,
                pid: 7,
                tid: 7,
                text: "always \"even\" failed".into(),
            }],
        };
        let (run, entry) = judged(0, failed(broke));
        assert_eq!(run.verdict(), Verdict::Failed);
        assert_eq!(run.verdict_label(), "exited:0, assertion failed");
        assert_eq!(entry.ending, "exited:0, assertion failed");
        assert!(entry.failed && !entry.passed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_trace_without_a_verdict_is_judged_from_init_s_exit_mark() {
        // A bare trace, and a manifest with no verdict yet, go by the
        // job's exit mark: the passing example's trace passes and the
        // failing one's fails, as the engine would judge them.
        let dir = temp_dir("unjudged");
        std::fs::write(dir.join(TRACE), trace_of(PASSING)).unwrap();
        assert_eq!(
            Run::open(&dir.join(TRACE)).unwrap().verdict(),
            Verdict::Passed
        );
        let mut manifest = manifest_of(PASSING);
        manifest.verdict = RecordedVerdict::Unfinished;
        write_manifest(&dir, &manifest);
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.recorded_verdict(), None);
        assert_eq!(run.verdict(), Verdict::Passed);

        std::fs::write(dir.join(TRACE), trace_of(FAILING)).unwrap();
        assert_eq!(
            Run::open(&dir.join(TRACE)).unwrap().verdict(),
            Verdict::Failed
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_runs_of_one_build_part_where_their_schedules_do() {
        // The failing example perturbs its seed over a window. The same
        // spec one step shorter is the same run until the window's last
        // step, where only the failing run is perturbed; schedule 0 parts
        // from it where the window starts. A run of other inputs, or of
        // the same schedule, has no such step.
        let failing = manifest_of(FAILING).spec;
        let (from, until) = failing.window().unwrap();
        let shorter = Spec {
            schedule_until: until - 1,
            ..failing.clone()
        };
        let split = ScheduleSplit::of(&failing, &shorter).unwrap();
        assert_eq!(split.step, until - 1);
        assert_eq!(split.here, done_at(&failing.schedule(), until - 1));
        assert_eq!(split.there, None);
        let unperturbed = Spec {
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            ..failing.clone()
        };
        assert_eq!(
            ScheduleSplit::of(&failing, &unperturbed).unwrap().step,
            from
        );
        let more_cores = Spec {
            cores: failing.cores + 1,
            ..failing.clone()
        };
        assert_eq!(ScheduleSplit::of(&failing, &more_cores), None);
        assert_eq!(ScheduleSplit::of(&failing, &failing), None);
    }

    #[test]
    fn a_split_reads_as_what_rewind_did_to_one_run_only() {
        // The card's line and the mark's tooltip for a split where Rewind
        // switched or paused a thread in one run only, and for one where
        // anything else differs: each says the runs are identical before
        // the step, and what Rewind did differently there in plain words.
        let split = |here: Option<&str>, there: Option<&str>| ScheduleSplit {
            step: 5139,
            here: here.map(String::from),
            there: there.map(String::from),
        };
        let here = split(Some("asks the kernel to switch threads"), None);
        assert_eq!(
            here.line("the passing run"),
            "The two runs are identical until step 5,139. There, Rewind asks the kernel to switch threads in this run only."
        );
        assert_eq!(
            here.tooltip("the passing run"),
            "Step 5,139: Rewind asks the kernel to switch threads in this run only. The two runs are identical before this step."
        );
        let there = split(None, Some("pauses the running thread for 160 µs"));
        assert_eq!(
            there.line("run 1989f6d0"),
            "The two runs are identical until step 5,139. There, Rewind pauses the running thread for 160 µs in run 1989f6d0 only."
        );
        assert_eq!(
            there.tooltip("run 1989f6d0"),
            "Step 5,139: Rewind pauses the running thread for 160 µs in run 1989f6d0 only. The two runs are identical before this step."
        );
        assert_eq!(
            split(None, None).line("run 1989f6d0"),
            "The two runs are identical until step 5,139. There, Rewind first schedules their threads differently."
        );
        assert_eq!(
            split(None, None).tooltip("run 1989f6d0"),
            "Step 5,139: Rewind first schedules the two runs' threads differently. The two runs are identical before this step."
        );
    }

    #[test]
    fn what_rewind_does_at_a_step_reads_as_a_verb() {
        // A schedule's step that asks for a reschedule, one that stalls,
        // one that does both, and one that does neither, each against its
        // words.
        let schedule = manifest_of(FAILING).spec.schedule();
        let (from, until) = manifest_of(FAILING).spec.window().unwrap();
        let find = |preempts: bool, stalls: bool| {
            (from..until).find(|&s| {
                schedule.preempt_at(s) == preempts && schedule.stall_at(s).is_some() == stalls
            })
        };
        let preempt = find(true, false).unwrap();
        assert_eq!(
            done_at(&schedule, preempt).as_deref(),
            Some("asks the kernel to switch threads")
        );
        let stall = find(false, true).unwrap();
        let us = schedule.stall_at(stall).unwrap() / 1_000;
        assert_eq!(
            done_at(&schedule, stall),
            Some(format!("pauses the running thread for {us} µs"))
        );
        if let Some(both) = find(true, true) {
            let us = schedule.stall_at(both).unwrap() / 1_000;
            assert_eq!(
                done_at(&schedule, both),
                Some(format!(
                    "asks the kernel to switch threads and pauses the running thread for {us} µs"
                ))
            );
        }
        let neither = find(false, false).unwrap();
        assert_eq!(done_at(&schedule, neither), None);
    }

    #[test]
    fn the_divergence_card_leads_with_how_each_run_ended() {
        // A failing run beside a passing one, a passing run beside a
        // failing one, and two runs that ended alike: the card's first
        // line says how each ended.
        assert_eq!(
            outcome_line(Verdict::Failed, Verdict::Passed, "the passing run"),
            "This run failed and the passing run did not."
        );
        assert_eq!(
            outcome_line(Verdict::Passed, Verdict::Failed, "run 1989f6d0"),
            "This run passed and run 1989f6d0 failed."
        );
        assert_eq!(
            outcome_line(Verdict::Failed, Verdict::Failed, "run 1989f6d0"),
            "Both runs failed."
        );
        assert_eq!(
            outcome_line(Verdict::Passed, Verdict::Passed, "run 1989f6d0"),
            "Both runs passed."
        );
    }

    #[test]
    fn forks_are_counted_and_the_parent_is_compared_with() {
        // Two runs name the base as their parent and one does not; opening a
        // fork without --compare picks its parent from next door. The runs
        // next door are read once, all four of them, and kept for the
        // families the app lists.
        let dir = temp_dir("forks");
        let id = |id: &str| RunId::parse(id).unwrap();
        let write = |run: &str, parent: Option<(&str, u64)>| {
            let mut manifest = manifest_of(FAILING);
            manifest.id = id(run);
            manifest.parent = parent.map(|(run, step)| Parent { run: id(run), step });
            let dir = dir.join(run);
            std::fs::create_dir_all(&dir).unwrap();
            small_trace(&dir);
            write_manifest(&dir, &manifest);
        };
        const BASE: &str = "0000000000000ba5";
        const F1: &str = "00000000000000f1";
        write(BASE, None);
        write(F1, Some((BASE, 10)));
        write("00000000000000f2", Some((BASE, 20)));
        write("0000000000000003", Some((F1, 5)));
        let session = Session::open(&dir.join(BASE), None).unwrap();
        assert!(session.other.is_none());
        let fork = Session::open(&dir.join(F1), None).unwrap();
        assert_eq!(fork.other.unwrap().path, dir.join(BASE));
        assert_eq!(
            fork.run.parent(),
            Some(&Parent {
                run: id(BASE),
                step: 10
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_exported_run_opens_from_its_file() {
        // The example export written to disk and opened by path unpacks
        // into the cache and remembers the file it came from.
        crate::archive::test_cache();
        let dir = temp_dir("export");
        let file = dir.join("mylib-fail.rwd");
        std::fs::write(&file, crate::examples::FAILING).unwrap();
        let run = Run::open(&file).unwrap();
        assert_eq!(
            run.origin,
            Origin::Export(Export {
                source: file.clone(),
                file: file.clone(),
                replayable: false,
            })
        );
        assert!(run.path.join(TRACE).is_file());
        assert_eq!(run.verdict(), Verdict::Failed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_read_again_keeps_the_run_it_is_compared_with() {
        // Two exports open from their files, one compared with the other.
        // The first is then read again from a run directory, as after
        // `rewind import`, and takes its place in the session. The
        // compared run stays an export of its file: opened again by the
        // directory it was unpacked into, it would pass for a run of this
        // machine, whose files the engine can read.
        crate::archive::test_cache();
        let dir = temp_dir("import-compare");
        let (fail, pass) = (dir.join("mylib-fail.rwd"), dir.join("mylib-pass.rwd"));
        std::fs::write(&fail, crate::examples::FAILING).unwrap();
        std::fs::write(&pass, crate::examples::PASSING).unwrap();
        let session = Session::open(&fail, Some(&pass)).unwrap();
        let compared = session.other.as_ref().unwrap().origin.clone();
        assert!(matches!(compared, Origin::Export(_)), "{compared:?}");
        let divergence = session.divergence_step();
        assert!(divergence.is_some());

        let read_again = Run::open(&session.run.path).unwrap();
        assert_eq!(read_again.origin, Origin::Local);
        let session = session.with_run(read_again);
        assert_eq!(session.run.origin, Origin::Local);
        assert_eq!(session.other.as_ref().unwrap().origin, compared);
        assert_eq!(session.divergence_step(), divergence);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_parent_mark_names_the_parent_schedule_and_seed() {
        // A fork's mark names the run it came from and its schedule, and
        // the seed when the fork was reseeded at its step.
        assert_eq!(
            parent_note("95701620b9331847", 340, 0, Some(8)),
            "Step 340: forked from run 95701620 under schedule 0, with the program's random seed changed to 8"
        );
        assert_eq!(
            parent_note("95701620b9331847", 4200, 3, None),
            "Step 4,200: forked from run 95701620 under schedule 3"
        );
    }
}
