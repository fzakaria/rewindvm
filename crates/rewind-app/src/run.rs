//! Opening a run from disk.
//!
//! A run is a directory holding `manifest.json` and `trace.bin`, or a bare
//! trace file. The manifest format is not settled yet, so the app reads
//! each field it knows on its own and ignores everything else: a field of
//! an unexpected type costs only that field, and a manifest that is not
//! JSON at all is set aside with a warning rather than refusing the run.
//! Both the draft layout (`drv`, `seed` and `command` at the top) and the
//! engine's (`source.drv`, `spec.seed`, `spec.job`) are understood.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rewind_trace::Trace;
use serde_json::Value;

use rewind_trace::signal_name;

use crate::archive;
use crate::model::{Comparison, ExitStatus, Timeline};

/// The trace inside a run directory.
pub const TRACE_FILE: &str = "trace.bin";
/// The manifest inside a run directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// What a run's manifest says about it. Every field may be missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The engine's id for the run: the hash of its inputs.
    pub id: Option<String>,
    pub name: Option<String>,
    pub command: Option<Vec<String>>,
    /// "nix" or "image"; kept as text so an unknown mode still loads.
    pub mode: Option<String>,
    pub drv: Option<String>,
    pub seed: Option<u64>,
    /// The schedule perturbation the run was made with; 0 for none.
    pub schedule: Option<u64>,
    /// The run this one was forked from, and the step it was forked at.
    pub parent: Option<Parent>,
    pub outcome: Option<Outcome>,
}

/// Where a forked run branched off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parent {
    pub id: String,
    pub step: u64,
}

/// How a run ended, as the engine recorded it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// How the machine stopped, in words.
    pub stop: Option<String>,
    /// The last step of the run.
    pub step: Option<u64>,
    /// The job's wait status, if the guest's init reported one.
    pub status: Option<i64>,
}

impl Manifest {
    /// Reads the fields the app knows from a manifest's JSON, each on its
    /// own, looking in the draft's place first and the engine's second.
    pub fn from_json(json: &Value) -> Manifest {
        let source = json.get("source");
        let spec = json.get("spec");
        let job = spec.and_then(|s| s.get("job"));
        let first_text = |candidates: &[Option<&Value>]| candidates.iter().find_map(|v| text(*v));
        let first_list =
            |candidates: &[Option<&Value>]| candidates.iter().find_map(|v| strings(*v));

        Manifest {
            id: text(json.get("id")),
            name: text(json.get("name")),
            command: first_list(&[
                json.get("command"),
                job.and_then(|j| j.get("argv")),
                job.and_then(|j| j.get("command")),
            ]),
            mode: first_text(&[json.get("mode"), source.and_then(|s| s.get("mode"))]),
            drv: first_text(&[json.get("drv"), source.and_then(|s| s.get("drv"))]),
            seed: seed(json.get("seed")).or_else(|| seed(spec.and_then(|s| s.get("seed")))),
            schedule: spec.and_then(|s| s.get("schedule")).and_then(Value::as_u64),
            parent: parent(json.get("parent")),
            outcome: json
                .get("outcome")
                .filter(|o| o.is_object())
                .map(|o| Outcome {
                    stop: text(o.get("stop")),
                    step: o.get("step").and_then(Value::as_u64),
                    status: o.get("status").and_then(Value::as_i64),
                }),
        }
    }
}

/// A JSON string, if the value is one and is not empty.
fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The strings of a JSON array, if the value is a non-empty array of them.
fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    let list: Vec<String> = value?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!list.is_empty()).then_some(list)
}

/// A parent written as the engine does: `[id, step]`.
fn parent(value: Option<&Value>) -> Option<Parent> {
    let pair = value?.as_array()?;
    let id = pair.first()?.as_str()?.to_string();
    let step = pair.get(1)?.as_u64()?;
    Some(Parent { id, step })
}

/// A seed written as a number, a decimal string or a 0x-prefixed hex
/// string.
fn seed(value: Option<&Value>) -> Option<u64> {
    const HEX_PREFIX: &str = "0x";
    const HEX_RADIX: u32 = 16;
    let value = value?;
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    let s = value.as_str()?.trim();
    match s.strip_prefix(HEX_PREFIX) {
        Some(hex) => u64::from_str_radix(hex, HEX_RADIX).ok(),
        None => s.parse().ok(),
    }
}

/// A run, read and indexed.
/// Where a run came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A run directory or a bare trace file on this machine.
    Local,
    /// A `.rwd` export, unpacked into the cache.
    Export(PathBuf),
    /// One of the example runs compiled into the app.
    Example,
}

pub struct Run {
    /// The path the run is read from: its directory, or its trace file. An
    /// export's is the directory it was unpacked into.
    pub path: PathBuf,
    pub origin: Origin,
    pub manifest: Manifest,
    /// Why the manifest was set aside, when it did not parse.
    pub manifest_warning: Option<String>,
    pub timeline: Timeline,
}

impl Run {
    /// Opens a run directory or a bare trace file.
    /// Opens a run directory, a `.rwd` export, or a bare trace file.
    pub fn open(path: &Path) -> Result<Run> {
        if path.is_file() && archive::is_export(path) {
            let dir = archive::import_file(path)?;
            return Run::open_at(&dir, Origin::Export(path.to_path_buf()));
        }
        Run::open_at(path, Origin::Local)
    }

    /// Opens an example run compiled into the app.
    pub fn open_example(bytes: &[u8]) -> Result<Run> {
        let dir = archive::import_bytes(bytes)?;
        Run::open_at(&dir, Origin::Example)
    }

    fn open_at(path: &Path, origin: Origin) -> Result<Run> {
        let (trace_path, manifest_path) = if path.is_dir() {
            (path.join(TRACE_FILE), Some(path.join(MANIFEST_FILE)))
        } else {
            (path.to_path_buf(), None)
        };
        if !trace_path.is_file() {
            bail!("no trace at {}", trace_path.display());
        }

        // The manifest is optional, and a broken one only costs its fields.
        let (manifest, manifest_warning) = match manifest_path.filter(|p| p.is_file()) {
            None => (Manifest::default(), None),
            Some(p) => match read_manifest(&p) {
                Ok(m) => (m, None),
                Err(e) => (Manifest::default(), Some(format!("{e:#}"))),
            },
        };

        let trace = Trace::read(&trace_path)
            .with_context(|| format!("reading {}", trace_path.display()))?;
        let total_hint = manifest.outcome.as_ref().and_then(|o| o.step);
        let timeline = Timeline::new(trace, total_hint);
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
        if let Some(name) = self.manifest.name.as_ref().filter(|n| !n.is_empty()) {
            return name.clone();
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
        if let Some(drv) = self.manifest.drv.as_ref().filter(|d| !d.is_empty()) {
            return drv.clone();
        }
        if let Some(cmd) = self.manifest.command.as_ref().filter(|c| !c.is_empty()) {
            return cmd.join(" ");
        }
        self.path.display().to_string()
    }

    /// The seed the run was recorded with, for `rewind fork`; 0 when the
    /// manifest does not say.
    pub fn seed(&self) -> u64 {
        self.manifest.seed.unwrap_or(0)
    }

    /// Whether the run failed. The job's wait status decides when the
    /// manifest or init's exit mark gives one; otherwise a crash or
    /// failing exit in the trace, or an outcome that says it stopped on an
    /// error.
    pub fn verdict(&self) -> Verdict {
        const FAILING_STOPS: &[&str] = &["fail", "error", "crash", "panic", "signal", "timeout"];
        if let Some(status) = self.status() {
            return if status == 0 {
                Verdict::Passed
            } else {
                Verdict::Failed
            };
        }
        if self.timeline.failure.is_some() {
            return Verdict::Failed;
        }
        let stop = self
            .manifest
            .outcome
            .as_ref()
            .and_then(|o| o.stop.as_deref())
            .unwrap_or_default()
            .to_lowercase();
        if FAILING_STOPS.iter().any(|word| stop.contains(word)) {
            return Verdict::Failed;
        }
        Verdict::Passed
    }

    /// The job's wait status: the manifest's, else init's exit mark's.
    pub fn status(&self) -> Option<u32> {
        let from_manifest = self
            .manifest
            .outcome
            .as_ref()
            .and_then(|o| o.status)
            .and_then(|s| u32::try_from(s).ok());
        from_manifest.or(self.timeline.job_exit.map(|j| j.status))
    }

    /// How the run ended, in the engine's words when there is a wait
    /// status (exited:2, killed:SIGSEGV), else passed or failed.
    pub fn verdict_label(&self) -> String {
        match self.status().map(ExitStatus::from_raw) {
            Some(ExitStatus::Code(code)) => format!("exited:{code}"),
            Some(ExitStatus::Signal { signo, .. }) => format!("killed:{}", signal_name(signo)),
            None => self.verdict().label().to_string(),
        }
    }

    /// How the run is named on screen: its id cut short when the engine
    /// gave it one, since runs of one build share a name; else its name.
    pub fn label(&self) -> String {
        match &self.manifest.id {
            Some(id) => short_id(id),
            None => self.name(),
        }
    }

    /// How many runs next to this one name it as their parent.
    pub fn count_forks(&self) -> usize {
        let Some(id) = &self.manifest.id else {
            return 0;
        };
        let Some(Ok(entries)) = self.path.parent().map(std::fs::read_dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                let Ok(text) = std::fs::read_to_string(entry.path().join(MANIFEST_FILE)) else {
                    return false;
                };
                let Ok(json) = serde_json::from_str::<Value>(&text) else {
                    return false;
                };
                parent(json.get("parent")).is_some_and(|p| &p.id == id)
            })
            .count()
    }

    /// The directory of the run this one was forked from, when it sits
    /// next to this one, as runs in one Rewind home do.
    pub fn parent_dir(&self) -> Option<PathBuf> {
        let parent = self.manifest.parent.as_ref()?;
        let dir = self.path.parent()?.join(&parent.id);
        dir.join(MANIFEST_FILE).is_file().then_some(dir)
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
pub fn short_id(id: &str) -> String {
    const ID_SHOWN: usize = 8;
    id.chars().take(ID_SHOWN).collect()
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let json: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Manifest::from_json(&json))
}

/// The run on screen and, optionally, the run it is compared with.
pub struct Session {
    pub run: Run,
    pub other: Option<Run>,
    pub comparison: Option<Comparison>,
    /// Forks of the run already on disk next to it, for picking the next
    /// fork's schedule seed.
    pub forks_on_disk: usize,
}

impl Session {
    pub fn new(run: Run, other: Option<Run>) -> Session {
        let comparison = other
            .as_ref()
            .map(|o| Comparison::new(&run.timeline, &o.timeline));
        let forks_on_disk = run.count_forks();
        Session {
            run,
            other,
            comparison,
            forks_on_disk,
        }
    }

    /// Opens a run and the run to compare it with: the one given, else
    /// the run it was forked from when that is next to it, else none.
    pub fn open(path: &Path, compare: Option<&Path>) -> Result<Session> {
        let run = Run::open(path)?;
        let compare = compare.map(Path::to_path_buf).or_else(|| run.parent_dir());
        let other = compare.as_deref().map(Run::open).transpose()?;
        Ok(Session::new(run, other))
    }

    /// The step of the first divergence from the compared run.
    pub fn divergence_step(&self) -> Option<u64> {
        self.comparison.as_ref().and_then(Comparison::step)
    }
}

#[cfg(test)]
mod tests {
    // Opening runs from a temporary directory: each test writes a trace and
    // perhaps a manifest, opens the run, and checks what the app shows.
    use super::*;
    use crate::synth::{self, SynthConfig, Variant};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn small_trace(dir: &Path) {
        let config = SynthConfig::small(Variant::Failing);
        std::fs::write(dir.join(TRACE_FILE), synth::generate(&config)).unwrap();
    }

    #[test]
    fn a_manifest_names_the_run_and_unknown_fields_are_ignored() {
        // The draft layout, with a mode and a field the app does not know.
        let dir = temp_dir("manifest");
        small_trace(&dir);
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"name": "run #3", "drv": "/nix/store/x-mylib.drv", "seed": "0x10",
                "mode": "spaceship", "extra": [1, 2], "outcome": {"stop": "exit", "step": 5}}"#,
        )
        .unwrap();
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.name(), "run #3");
        assert_eq!(run.subject(), "/nix/store/x-mylib.drv");
        assert_eq!(run.seed(), 16);
        assert_eq!(run.manifest.mode.as_deref(), Some("spaceship"));
        assert_eq!(run.manifest_warning, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_manifest_is_set_aside_with_a_warning() {
        // A manifest that is not JSON leaves the run named after its directory.
        let dir = temp_dir("broken");
        small_trace(&dir);
        std::fs::write(dir.join(MANIFEST_FILE), "{not json").unwrap();
        let run = Run::open(&dir).unwrap();
        assert!(run.manifest_warning.is_some());
        assert_eq!(run.name(), dir.file_name().unwrap().to_string_lossy());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bare_trace_file_opens_and_a_missing_one_does_not() {
        // A trace file opened by itself, and a path with nothing there.
        let dir = temp_dir("bare");
        small_trace(&dir);
        let run = Run::open(&dir.join(TRACE_FILE)).unwrap();
        assert_eq!(run.name(), TRACE_FILE);
        assert_eq!(run.verdict(), Verdict::Failed);
        assert!(Run::open(&dir.join("nope.bin")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_engines_manifest_layout_is_understood() {
        // The layout rewind-core writes: the derivation under source, the
        // seed and the job under spec, and a wait status in the outcome.
        let json: Value = serde_json::from_str(
            r#"{"version": 1, "id": "ab12", "name": "mylib", "created": 1,
                "source": {"mode": "nix", "drv": "/nix/store/x-mylib.drv", "outputs": ["out"]},
                "spec": {"seed": 7, "job": {"argv": ["make", "check"]}},
                "parent": null,
                "outcome": {"stop": "exit", "step": 99, "virtual_ns": 1, "status": 512, "wall_ms": 3}}"#,
        )
        .unwrap();
        let m = Manifest::from_json(&json);
        assert_eq!(m.name.as_deref(), Some("mylib"));
        assert_eq!(m.drv.as_deref(), Some("/nix/store/x-mylib.drv"));
        assert_eq!(m.mode.as_deref(), Some("nix"));
        assert_eq!(m.seed, Some(7));
        assert_eq!(
            m.command,
            Some(vec!["make".to_string(), "check".to_string()])
        );
        let outcome = m.outcome.unwrap();
        assert_eq!((outcome.step, outcome.status), (Some(99), Some(512)));
    }

    #[test]
    fn a_field_of_the_wrong_type_costs_only_that_field() {
        // Wrong types for the name, seed and outcome; the drv still reads.
        let json: Value =
            serde_json::from_str(r#"{"name": 5, "seed": "not a seed", "drv": "/d", "outcome": 3}"#)
                .unwrap();
        let m = Manifest::from_json(&json);
        assert_eq!(m.name, None);
        assert_eq!(m.seed, None);
        assert_eq!(m.drv.as_deref(), Some("/d"));
        assert_eq!(m.outcome, None);
    }

    #[test]
    fn a_nonzero_wait_status_fails_a_run_whose_trace_looks_clean() {
        // The passing synthetic trace under manifests with two wait statuses.
        let dir = temp_dir("status");
        let config = SynthConfig::small(Variant::Passing);
        std::fs::write(dir.join(TRACE_FILE), synth::generate(&config)).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), r#"{"outcome": {"status": 256}}"#).unwrap();
        assert_eq!(Run::open(&dir).unwrap().verdict(), Verdict::Failed);
        std::fs::write(dir.join(MANIFEST_FILE), r#"{"outcome": {"status": 0}}"#).unwrap();
        assert_eq!(Run::open(&dir).unwrap().verdict(), Verdict::Passed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn forks_are_counted_and_the_parent_is_compared_with() {
        // Two runs name "base" as their parent and one does not; opening a
        // fork without --compare picks its parent from next door.
        let dir = temp_dir("forks");
        let write = |name: &str, manifest: &str| {
            let run = dir.join(name);
            std::fs::create_dir_all(&run).unwrap();
            small_trace(&run);
            std::fs::write(run.join(MANIFEST_FILE), manifest).unwrap();
        };
        write("base", r#"{"id": "base"}"#);
        write("f1", r#"{"id": "f1", "parent": ["base", 10]}"#);
        write("f2", r#"{"id": "f2", "parent": ["base", 20]}"#);
        write("other", r#"{"id": "other", "parent": ["f1", 5]}"#);
        let session = Session::open(&dir.join("base"), None).unwrap();
        assert_eq!(session.forks_on_disk, 2);
        assert!(session.other.is_none());
        let fork = Session::open(&dir.join("f1"), None).unwrap();
        assert_eq!(fork.other.unwrap().path, dir.join("base"));
        assert_eq!(
            fork.run.manifest.parent,
            Some(Parent {
                id: "base".into(),
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
        assert_eq!(run.origin, Origin::Export(file.clone()));
        assert!(run.path.join(TRACE_FILE).is_file());
        assert_eq!(run.verdict(), Verdict::Failed);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
