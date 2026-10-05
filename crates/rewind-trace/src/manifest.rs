//! A run as its directory holds it: `manifest.json`, which says exactly
//! what was run and how it ended, beside `trace.bin`. The engine writes
//! manifests and every reader, the desktop app included, reads them with
//! these types.

use std::path::PathBuf;

use rewind_init::Job;
use serde::{Deserialize, Serialize};

use crate::machine::{ClockSource, CpuModel, Extras, Preemption};
use crate::stop::Stop;

/// The manifest inside a run directory.
pub const MANIFEST: &str = "manifest.json";

/// The trace inside a run directory.
pub const TRACE: &str = "trace.bin";

/// The desktop app's bookmarks of a run, kept in the run's directory and
/// carried in its exports. The engine never reads them.
pub const BOOKMARKS: &str = "bookmarks.json";

/// The directory of a run's own keyframes, inside the run's directory.
pub const KEYFRAMES_DIR: &str = "keyframes";

/// The version of the manifest format.
pub const MANIFEST_VERSION: u32 = 1;

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

impl AsRef<std::path::Path> for RunId {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
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
    /// For a fork of a fork, the perturbations of the runs it came from,
    /// each over its window and all ending by `schedule_from`, so the fork
    /// is its parent up to its own step.
    pub inherited_schedules: Vec<ScheduleSegment>,
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

/// One schedule seed over the steps `from..until`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSegment {
    pub seed: u64,
    pub from: u64,
    pub until: u64,
}

/// What kind of workload a run is, for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Source {
    /// A command in a root filesystem.
    Image { root: String },
    /// A Nix derivation.
    Nix { drv: String, outputs: Vec<String> },
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
    /// this run's perturbations up to the step, the new one from it.
    pub fn fork(&self, step: u64, seed: u64) -> Spec {
        let own = ScheduleSegment {
            seed: self.schedule,
            from: self.schedule_from,
            until: self.schedule_until,
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
        Spec {
            schedule: seed,
            schedule_from: step,
            schedule_until: u64::MAX,
            inherited_schedules: inherited,
            ..self.clone()
        }
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
            inherited_schedules: Vec::new(),
            image: None,
            kernel_debug: None,
            ..self.clone()
        }
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
