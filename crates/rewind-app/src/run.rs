//! Opening a run from disk.
//!
//! A run is a directory holding `manifest.json` and `trace.bin`, or a bare
//! trace file. The manifest is read with the engine's own types
//! (rewind_trace::manifest). One this build cannot read, such as one
//! another build of the engine wrote, is set aside with a warning, and the
//! run opens as its trace alone.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rewind_trace::Trace;
use rewind_trace::ending::{Ending, ExitStatus};
use rewind_trace::manifest::{MANIFEST, Manifest, Parent, RunId, Source, TRACE};
use rewind_trace::stop::Stop;

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
    pub timeline: Timeline,
}

impl Run {
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

    fn open_at(path: &Path, origin: Origin) -> Result<Run> {
        let (trace_path, manifest_path) = if path.is_dir() {
            (path.join(TRACE), Some(path.join(MANIFEST)))
        } else {
            (path.to_path_buf(), None)
        };
        if !trace_path.is_file() {
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

        let trace = Trace::read(&trace_path)
            .with_context(|| format!("reading {}", trace_path.display()))?;
        let outcome = manifest.as_ref().and_then(|m| m.outcome.as_ref());
        let total_hint = outcome.map(|o| o.step);
        let stop = outcome.map(|o| &o.stop);
        let timeline = Timeline::new(trace, total_hint, stop);
        Ok(Run {
            path: path.to_path_buf(),
            origin,
            manifest,
            manifest_warning,
            timeline,
        })
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

    /// What the run built or ran: the derivation, else the command, else
    /// the path it was opened from.
    pub fn subject(&self) -> String {
        let Some(m) = &self.manifest else {
            return self.path.display().to_string();
        };
        match &m.source {
            Source::Nix { drv, .. } => drv.clone(),
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

    /// How the run ended, in the engine's words, once the engine has
    /// recorded its end. A Nix build that exited 0 without creating every
    /// output ended missing-output, as nix-daemon would fail it.
    pub fn ending(&self) -> Option<Ending> {
        let m = self.manifest.as_ref()?;
        let outcome = m.outcome.as_ref()?;
        let built = self.timeline.trace.outputs();
        let missing: Vec<String> = m
            .spec
            .job
            .outputs
            .iter()
            .filter(|path| !built.iter().any(|(p, _)| p == *path))
            .cloned()
            .collect();
        Some(Ending::of(&outcome.stop, outcome.status, &missing))
    }

    /// Whether the run failed: as its ending says, once the engine has
    /// recorded one. A trace without a manifest goes by init's exit mark,
    /// else by a crash or failing exit in the trace.
    pub fn verdict(&self) -> Verdict {
        let passed = match (self.ending(), self.timeline.job_exit) {
            (Some(ending), _) => ending.passed(),
            (None, Some(exit)) => ExitStatus::from_wait(exit.status).success(),
            (None, None) => self.timeline.failure.is_none(),
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
    /// timed-out), else by init's exit mark, else passed or failed.
    pub fn verdict_label(&self) -> String {
        if let Some(ending) = self.ending() {
            return ending.to_string();
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

/// A run id cut to the length people read and type.
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

pub fn short_id(id: &str) -> String {
    const ID_SHOWN: usize = 8;
    id.chars().take(ID_SHOWN).collect()
}

pub(crate) fn read_manifest(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| {
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
    /// Why the run asked to be compared with could not be opened, such as
    /// one removed meanwhile, for a notice; the run then opens alone.
    pub unopened_compare: Option<String>,
    /// Whether this build of rewind brings the run to a step as recorded.
    pub replays: Replays,
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
        let comparison = other
            .as_ref()
            .map(|o| Comparison::new(&run.timeline, &o.timeline));
        Session {
            run,
            other,
            comparison,
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

    /// The step of the first divergence from the compared run.
    pub fn divergence_step(&self) -> Option<u64> {
        self.comparison.as_ref().and_then(Comparison::step)
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
        let name_here = |pid: u32| self.run.timeline.name_of(pid).map(str::to_string);
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
        let other_run =
            if other.verdict() == Verdict::Passed && self.run.verdict() == Verdict::Failed {
                "the passing run".to_string()
            } else {
                format!("run {other_label}")
            };
        let mut lines = vec![
            before,
            format!("Then in this run, {}.", difference.here),
            format!("In {other_run}, {}.", difference.there),
        ];
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
    fn a_run_without_a_wait_status_fails_however_it_stopped() {
        // The passing example's boot, before its job, under manifests with
        // no wait status and each way a machine stops: one stopped at its
        // time limit says timed-out, as `rewind ls` does, and the others
        // no-status. None of them passed: the job never reported its end.
        let dir = temp_dir("stops");
        std::fs::write(dir.join(TRACE), trace_until(PASSING, Until::JobStart)).unwrap();
        let with_stop = |stop: Stop| {
            let mut manifest = manifest_of(PASSING);
            let outcome = manifest.outcome.as_mut().unwrap();
            outcome.stop = stop;
            outcome.status = None;
            write_manifest(&dir, &manifest);
            Run::open(&dir).unwrap()
        };

        let hung = with_stop(hung_in_user_space());
        assert_eq!(hung.verdict(), Verdict::Failed);
        assert_eq!(hung.verdict_label(), "timed-out");
        assert_eq!(with_stop(Stop::TripleFault).verdict(), Verdict::Failed);
        let powered_off = with_stop(Stop::PoweredOff);
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
    fn a_wait_status_and_the_outputs_built_decide_a_nix_build() {
        // The passing example, a Nix build, under manifests with two wait
        // statuses: 0 passes, 1 fails. Its boot alone, which built none of
        // its outputs, under status 0 fails as nix-daemon would.
        let dir = temp_dir("status");
        let with_status = |status: i32| {
            let mut manifest = manifest_of(PASSING);
            manifest.outcome.as_mut().unwrap().status = Some(status);
            write_manifest(&dir, &manifest);
            Run::open(&dir).unwrap()
        };
        std::fs::write(dir.join(TRACE), trace_of(PASSING)).unwrap();
        assert_eq!(with_status(1 << 8).verdict(), Verdict::Failed);
        assert_eq!(with_status(0).verdict(), Verdict::Passed);

        std::fs::write(dir.join(TRACE), trace_until(PASSING, Until::JobStart)).unwrap();
        let unbuilt = with_status(0);
        assert_eq!(unbuilt.verdict(), Verdict::Failed);
        assert_eq!(unbuilt.verdict_label(), "missing-output");
        std::fs::remove_dir_all(&dir).unwrap();
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
}
