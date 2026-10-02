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
use crate::describe::{self, thousands};
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
    /// The BLAKE3 hash of the run's input image, which with the command
    /// names a build that is not a derivation.
    pub image_hash: Option<String>,
    /// The BLAKE3 hash of the run's trace: runs with equal hashes did the
    /// same thing.
    pub trace_hash: Option<String>,
    /// For a fork, the step where it first differs from its parent.
    pub first_difference: Option<u64>,
    /// When the run was made, in seconds since the epoch.
    pub created: Option<u64>,
    /// The vCPUs the VM had.
    pub cores: Option<u64>,
    /// The steps a perturbed schedule was confined to, when it was: the
    /// first and the last.
    pub window: Option<(u64, u64)>,
    /// Whether the run came from an export: `rewind import` keeps an
    /// imported run's kernel among its inputs, under the run's id.
    pub imported: bool,
    /// A key for the run's inputs less its schedule: runs with equal keys
    /// are the same build on the same machine, perturbed or not.
    pub inputs: Option<String>,
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
            image_hash: text(spec.and_then(|s| s.get("image_hash"))),
            trace_hash: text(json.get("trace_hash")),
            first_difference: json.get("first_difference").and_then(Value::as_u64),
            created: json.get("created").and_then(Value::as_u64),
            cores: spec.and_then(|s| s.get("cores")).and_then(Value::as_u64),
            imported: imported(text(json.get("id")).as_deref(), spec),
            window: window(spec),
            inputs: spec.and_then(inputs_key),
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
/// The spec's fields that say how the schedule is perturbed, and the
/// paths on this machine of inputs the spec also names by content or the
/// run id counts by content.
const NOT_INPUTS: [&str; 7] = [
    "schedule",
    "schedule_from",
    "schedule_until",
    "inherited_schedules",
    "kernel",
    "initrd",
    "image",
];

/// Where `rewind import` puts an imported run's kernel, initrd and image:
/// a directory named for the run in the home's inputs.
const IMPORTED_INPUTS_DIR: &str = "inputs";

/// The steps the spec confines its schedule to, when it has an end: the
/// engine writes the largest u64 for a schedule that runs to the end.
fn window(spec: Option<&Value>) -> Option<(u64, u64)> {
    let field = |name| spec?.get(name)?.as_u64();
    let until = field("schedule_until").filter(|u| *u != u64::MAX)?;
    Some((field("schedule_from").unwrap_or(0), until))
}

/// Whether the spec's kernel is in the imported inputs of run `id`.
fn imported(id: Option<&str>, spec: Option<&Value>) -> bool {
    let (Some(id), Some(kernel)) = (id, spec.and_then(|s| text(s.get("kernel")))) else {
        return false;
    };
    let Some(dir) = Path::new(&kernel).parent() else {
        return false;
    };
    let named = |p: Option<&Path>, name: &str| {
        p.and_then(Path::file_name)
            .is_some_and(|n| n.to_string_lossy() == name)
    };
    named(Some(dir), id) && named(dir.parent(), IMPORTED_INPUTS_DIR)
}

/// A hash of a manifest's spec without the fields in `NOT_INPUTS`.
fn inputs_key(spec: &Value) -> Option<String> {
    use std::hash::{Hash, Hasher};

    let mut spec = spec.as_object()?.clone();
    for field in NOT_INPUTS {
        spec.remove(field);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Value::Object(spec).to_string().hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

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
    pub manifest: Manifest,
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
/// The run directory `path` names when it is the manifest or trace inside
/// one: a directory with a manifest.
fn run_dir_of(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    if name != MANIFEST_FILE && name != TRACE_FILE {
        return None;
    }
    let dir = path.parent()?;
    dir.join(MANIFEST_FILE).is_file().then(|| dir.to_path_buf())
}

pub fn short_id(id: &str) -> String {
    const ID_SHOWN: usize = 8;
    id.chars().take(ID_SHOWN).collect()
}

pub(crate) fn read_manifest(path: &Path) -> Result<Manifest> {
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
        std::fs::write(run.join(MANIFEST_FILE), "{}").unwrap();
        assert_eq!(run_dir_of(&run.join(MANIFEST_FILE)), Some(run.clone()));
        assert_eq!(run_dir_of(&run.join(TRACE_FILE)), Some(run.clone()));
        assert_eq!(run_dir_of(&dir.join(TRACE_FILE)), None);
        assert_eq!(run_dir_of(&run.join("other.json")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        assert_eq!(
            run.origin,
            Origin::Export(Export {
                source: file.clone(),
                file: file.clone(),
                replayable: false,
            })
        );
        assert!(run.path.join(TRACE_FILE).is_file());
        assert_eq!(run.verdict(), Verdict::Failed);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
