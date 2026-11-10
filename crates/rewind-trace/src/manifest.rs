//! A run as its directory holds it: `manifest.json`, which says exactly
//! what was run and how it ended, beside `trace.bin`. The engine writes
//! manifests and every reader, the desktop app included, reads them with
//! these types.

use std::path::{Path, PathBuf};

use rewind_init::Job;
use serde::{Deserialize, Serialize};

use crate::machine::{ClockSource, CpuModel, Extras, Preemption};
use crate::schedule::StallMax;
use crate::stop::Stop;
use crate::verdict::Verdict;

/// The manifest inside a run directory.
pub const MANIFEST: &str = "manifest.json";

/// The trace inside a run directory.
pub const TRACE: &str = "trace.bin";

/// The desktop app's bookmarks of a run, kept in the run's directory and
/// carried in its exports. The engine never reads them.
pub const BOOKMARKS: &str = "bookmarks.json";

/// The directory of a run's own keyframes, inside the run's directory.
pub const KEYFRAMES_DIR: &str = "keyframes";

/// The file in a run's directory that the process executing the run holds
/// a lock on. The kernel drops the lock when that process dies, however it
/// dies, so a run with no outcome and no lock held was interrupted.
pub const EXECUTING_LOCK: &str = "executing.lock";

/// Whether a process is executing the run in `dir` now.
pub fn executing(dir: &Path) -> bool {
    let Ok(file) = std::fs::File::open(dir.join(EXECUTING_LOCK)) else {
        return false;
    };
    // A shared lock is refused only while an execution holds its own, and
    // goes when the file is dropped.
    file.try_lock_shared().is_err()
}

/// The version of the manifest format. Every manifest before rewind 1.0
/// said 1, whatever its fields, so 1.0's was 2; 3 adds a fork's reseeds,
/// 4 a schedule's longest stall, and 5 the run's verdict.
pub const MANIFEST_VERSION: u32 = 5;

/// The version every manifest before rewind 1.0 named.
const BEFORE_1_0: u32 = 1;

/// Why a manifest does not read.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("a manifest from before rewind 1.0, which this build does not read")]
    BeforeFormats,
    #[error("a manifest of format {0}; this build of rewind reads format {MANIFEST_VERSION}")]
    Version(u32),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl Manifest {
    /// The manifest in `bytes`, read only when it names this build's
    /// version of the format.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let Version { version } = serde_json::from_slice(bytes)?;
        match version {
            MANIFEST_VERSION => Ok(serde_json::from_slice(bytes)?),
            BEFORE_1_0 => Err(ManifestError::BeforeFormats),
            other => Err(ManifestError::Version(other)),
        }
    }
}

/// How many hex digits of the hash of its inputs a run id keeps.
pub const ID_LEN: usize = 16;

/// A run's id: ID_LEN lowercase hex digits of the hash of its inputs, and
/// so a name that stays inside the runs directory. A manifest that names
/// anything else as a run, its own id, its parent or the run it shares
/// keyframes with, does not parse.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    /// The id `id` is, if it is one a spec's hash could give.
    pub fn parse(id: &str) -> Option<RunId> {
        let hex = id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (id.len() == ID_LEN && hex).then(|| RunId(id.to_string()))
    }

    /// The id of a run whose inputs hash to `hex`: its first ID_LEN
    /// digits. None for a hash too short or not hex.
    pub fn of_hash(hex: &str) -> Option<RunId> {
        RunId::parse(hex.get(..ID_LEN)?)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunId {
    type Error = String;

    fn try_from(id: String) -> Result<RunId, String> {
        RunId::parse(&id).ok_or_else(|| format!("{id:?} is not a run id"))
    }
}

impl From<RunId> for String {
    fn from(id: RunId) -> String {
        id.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for RunId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for RunId {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl PartialEq<str> for RunId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for RunId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// The run a fork was forked from, and the step it was forked at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub run: RunId,
    pub step: u64,
}

/// Everything that determines a run. Two equal specs make equal runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Where the kernel's DWARF is, for `rewind gdb`: the kernel package's
    /// `debug` output, which may not be on this machine until it is
    /// fetched. Not an input: it is left out of the run's id.
    pub kernel_debug: Option<PathBuf>,
    /// The input image and its BLAKE3 hash; the hash is what makes the
    /// run's id, since the path can be reused.
    pub image: Option<PathBuf>,
    pub image_hash: Option<String>,
    pub mem_mib: u64,
    /// The CPUs the guest tells user space it has, through the affinity
    /// system calls, sysfs and /proc/cpuinfo, and a Nix build's
    /// NIX_BUILD_CORES. The VM has one vCPU whatever this is: threads
    /// sized by the count interleave on it.
    pub cores: u32,
    pub seed: u64,
    pub epoch: u64,
    pub quantum: u64,
    /// Perturbs where timer interrupts, and so preemptions, land; 0 for
    /// none.
    pub schedule: u64,
    /// The steps the schedule perturbation applies to, `from..until`.
    pub schedule_from: u64,
    pub schedule_until: u64,
    /// The longest stall the schedule asks for, as `--stall-max` sets it,
    /// in nanoseconds. Left out at the default, so a run made without the
    /// flag keeps the id and the manifest it always had.
    #[serde(
        default,
        skip_serializing_if = "StallMax::is_default",
        rename = "stall_max_ns"
    )]
    pub stall_max: StallMax,
    /// For a fork of a fork, the perturbations of the runs it came from,
    /// each over its window and all ending by `schedule_from`, so the fork
    /// is its parent up to its own step.
    pub inherited_schedules: Vec<ScheduleSegment>,
    /// Where a fork given another seed than its parent's has the guest's
    /// RNG take in new bytes, each of its own and its ancestors', in step
    /// order. A run from boot takes its seed at boot and has none.
    pub reseeds: Vec<crate::schedule::Reseed>,
    /// The CPU the guest is shown.
    pub cpu: CpuModel,
    /// What moves virtual time besides exits and idling.
    pub clock: ClockSource,
    /// Where a computing guest can be interrupted.
    pub preemption: Preemption,
    /// Whether the machine reserves the extras slot for `rewind shell
    /// --with`.
    pub extras: Extras,
    pub cmdline: String,
    pub job: Job,
}

/// One schedule seed over the steps `from..until`, and the longest stall
/// it asks for there, left out at the default as [`Spec::stall_max`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSegment {
    pub seed: u64,
    pub from: u64,
    pub until: u64,
    #[serde(
        default,
        skip_serializing_if = "StallMax::is_default",
        rename = "stall_max_ns"
    )]
    pub stall_max: StallMax,
}

/// What kind of workload a run is, for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Source {
    /// A command in a root filesystem.
    Image { root: String },
    /// A Nix derivation.
    Nix { drv: String, outputs: Vec<String> },
    /// A build nix-daemon described, as its external-builders setting
    /// hands one to a program: the description's file, and the outputs.
    Build {
        description: String,
        outputs: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// How the machine stopped.
    pub stop: Stop,
    pub step: u64,
    pub virtual_ns: u64,
    /// The job's wait status, if init reported one.
    pub status: Option<i32>,
    pub wall_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub id: RunId,
    pub name: String,
    pub created: u64,
    pub source: Source,
    pub spec: Spec,
    /// The run this one was forked from, and the step it was forked at.
    pub parent: Option<Parent>,
    /// Where the keyframes this run did not take itself are: another run's,
    /// up to the last step the two runs share.
    pub shared_keyframes: Option<SharedKeyframes>,
    pub outcome: Option<RunOutcome>,
    /// Whether the run passed and, when it did not, why: judged by the
    /// engine once, as the run ended, and read by everything that says a
    /// run passed or failed.
    pub verdict: Verdict,
    /// The BLAKE3 hash of trace.bin in hex, once the run has finished. Runs
    /// with equal hashes did the same thing.
    pub trace_hash: Option<String>,
    /// For a fork, the step where its trace first differs from its
    /// parent's; absent when the two are identical.
    pub first_difference: Option<u64>,
    /// The rewind that recorded the run, as [`crate::VERSION`] names it.
    pub recorded_by: String,
}

/// Keyframes a run reads from another run's directory instead of keeping
/// copies: every one of that run's keyframes at or before `through`, the
/// last step at which the two runs were still the same run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedKeyframes {
    pub run: RunId,
    pub through: u64,
}

impl Spec {
    /// The spec of a fork of this run at `step` under schedule `seed`:
    /// this run's perturbations up to the step, each with its own longest
    /// stall, and the new one from it with the default, as a run's own
    /// schedule has unless [`Spec::with_stall_max`] gives it another.
    pub fn fork(&self, step: u64, seed: u64) -> Spec {
        let own = ScheduleSegment {
            seed: self.schedule,
            from: self.schedule_from,
            until: self.schedule_until,
            stall_max: self.stall_max,
        };

        // Each earlier perturbation is cut off at the fork step, and one
        // that perturbs nothing before it is dropped.
        let inherited = self
            .inherited_schedules
            .iter()
            .chain(std::iter::once(&own))
            .map(|s| ScheduleSegment {
                until: s.until.min(step),
                ..s.clone()
            })
            .filter(|s| s.seed != 0 && s.from < s.until)
            .collect();
        // A reseed at or after the step is the parent's alone.
        let reseeds = self
            .reseeds
            .iter()
            .filter(|r| r.step < step)
            .cloned()
            .collect();
        Spec {
            schedule: seed,
            schedule_from: step,
            schedule_until: u64::MAX,
            stall_max: StallMax::default(),
            inherited_schedules: inherited,
            reseeds,
            ..self.clone()
        }
    }

    /// This spec with its own schedule's longest stall `max`. Schedule 0
    /// stalls nothing, so an unperturbed spec keeps the default, and its
    /// id, whatever `max` is.
    pub fn with_stall_max(self, max: StallMax) -> Spec {
        let stall_max = if self.schedule == 0 {
            StallMax::default()
        } else {
            max
        };
        Spec { stall_max, ..self }
    }

    /// This spec with the guest's RNG reseeded with `seed` at `step`, as
    /// a fork given another seed at that step is.
    pub fn reseeded(&self, step: u64, seed: u64) -> Spec {
        let mut reseeds = self.reseeds.clone();
        reseeds.retain(|r| r.step != step);
        reseeds.push(crate::schedule::Reseed { step, seed });
        reseeds.sort_by_key(|r| r.step);
        Spec {
            reseeds,
            ..self.clone()
        }
    }

    /// Where and how this spec's schedule perturbs the run.
    pub fn schedule(&self) -> crate::schedule::Schedule {
        crate::schedule::Schedule {
            seed: self.schedule,
            window: self.schedule_from..self.schedule_until,
            stall_max: self.stall_max,
            earlier: self
                .inherited_schedules
                .iter()
                .map(|s| crate::schedule::Segment {
                    seed: s.seed,
                    window: s.from..s.until,
                    stall_max: s.stall_max,
                })
                .collect(),
            reseeds: self.reseeds.clone(),
        }
    }

    /// The last step through which a run of this spec and a run of
    /// `other` are the same run: they differ only in their schedules, and
    /// the schedules perturb the same steps the same way up to it. None
    /// when anything else differs, u64::MAX when nothing does. A keyframe
    /// of one run at or before this step is a keyframe of the other.
    pub fn same_through(&self, other: &Spec) -> Option<u64> {
        // Everything but the schedule must match, compared the way the id
        // compares it.
        if self.unscheduled() != other.unscheduled() {
            return None;
        }
        Some(self.schedule().same_through(&other.schedule()))
    }

    /// The steps the schedule perturbs, first and end, when it has an
    /// end: `rewind check` confines a schedule to such a window as it
    /// narrows one down. A schedule that runs to the end of the run has
    /// none.
    pub fn window(&self) -> Option<(u64, u64)> {
        (self.schedule_until != u64::MAX).then_some((self.schedule_from, self.schedule_until))
    }

    /// The spec without its schedule, and without the paths of inputs a
    /// run's id counts by content: two runs whose specs are equal this way
    /// are the same build on the same machine, perturbed or not.
    pub fn unscheduled(&self) -> Spec {
        Spec {
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            stall_max: StallMax::default(),
            inherited_schedules: Vec::new(),
            reseeds: Vec::new(),
            image: None,
            kernel_debug: None,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod version_tests {
    // A manifest's version is read before the rest of it: one of another
    // version is refused, naming it, and one of this version goes on to be
    // read whole.
    use super::*;

    #[test]
    fn a_manifest_is_read_by_its_version_first() {
        let newer = format!(r#"{{"version": {}}}"#, MANIFEST_VERSION + 1);
        let err = Manifest::parse(newer.as_bytes()).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("format {}", MANIFEST_VERSION + 1)),
            "{err}"
        );

        let before = br#"{"version": 1, "id": "0123456789abcdef", "spec": {}}"#;
        let err = Manifest::parse(before).unwrap_err();
        assert!(err.to_string().contains("before rewind 1.0"), "{err}");

        let current = format!(r#"{{"version": {MANIFEST_VERSION}}}"#);
        let err = Manifest::parse(current.as_bytes()).unwrap_err();
        assert!(matches!(err, ManifestError::Json(_)), "{err}");

        // The format before the verdict is refused by its version alone.
        let before_verdict = format!(r#"{{"version": {}}}"#, MANIFEST_VERSION - 1);
        let err = Manifest::parse(before_verdict.as_bytes()).unwrap_err();
        assert!(matches!(err, ManifestError::Version(4)), "{err}");
        let err = Manifest::parse(b"not json").unwrap_err();
        assert!(matches!(err, ManifestError::Json(_)), "{err}");
    }
}

#[cfg(test)]
mod tests {
    // Run ids as manifests carry them: what parses as one, and a manifest
    // field naming something else.
    use super::*;

    #[test]
    fn a_run_id_is_sixteen_lowercase_hex_digits() {
        // A spec's hash cut to length is an id; a path, an id in capitals
        // and one too short or too long are not.
        let id = RunId::parse("0123456789abcdef").unwrap();
        assert_eq!(id.as_str(), "0123456789abcdef");
        assert_eq!(RunId::of_hash("0123456789abcdef0123456789abcdef"), Some(id));
        for not_an_id in [
            "../../etc/passwd",
            "0123456789ABCDEF",
            "0123",
            "0123456789abcdef0",
            "",
        ] {
            assert_eq!(RunId::parse(not_an_id), None, "{not_an_id:?}");
        }
    }

    #[test]
    fn a_run_executes_while_its_lock_is_held() {
        // A run directory with no lock file, or with one nobody holds, is
        // not executing; one whose lock a process holds is, until the lock
        // is dropped.
        let dir = std::env::temp_dir().join(format!("rewind-trace-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!executing(&dir));

        let lock = std::fs::File::create(dir.join(EXECUTING_LOCK)).unwrap();
        assert!(!executing(&dir));
        lock.try_lock().unwrap();
        assert!(executing(&dir));
        drop(lock);
        assert!(!executing(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_parent_or_shared_run_that_is_no_id_does_not_parse() {
        // An imported manifest could name any path as the run it was forked
        // from or reads keyframes from; either is refused when it is read.
        let parent: serde_json::Result<Parent> =
            serde_json::from_str(r#"{"run": "../../../tmp/x", "step": 1}"#);
        assert!(parent.is_err());
        let shared: serde_json::Result<SharedKeyframes> =
            serde_json::from_str(r#"{"run": "../other", "through": 9}"#);
        assert!(shared.is_err());
        let good: Parent =
            serde_json::from_str(r#"{"run": "fedcba9876543210", "step": 400}"#).unwrap();
        assert_eq!(good.run, "fedcba9876543210");
    }
}
