//! A run as its directory holds it: `manifest.json`, which says exactly
//! what was run and how it ended, beside `trace.bin`. The engine writes
//! manifests and every reader, the desktop app included, reads them with
//! these types.

use std::path::PathBuf;

use rewind_init::Job;
use serde::{Deserialize, Serialize};

use crate::machine::{ClockSource, CpuModel, Extras, Preemption};

/// The manifest inside a run directory.
pub const MANIFEST: &str = "manifest.json";

/// The trace inside a run directory.
pub const TRACE: &str = "trace.bin";

/// The desktop app's bookmarks of a run, kept in the run's directory and
/// carried in its exports. The engine never reads them.
pub const BOOKMARKS: &str = "bookmarks.json";

/// The version of the manifest format.
pub const MANIFEST_VERSION: u32 = 1;

/// How many hex digits of the hash of its inputs a run id keeps.
pub const ID_LEN: usize = 16;

/// Whether `id` is one a spec's hash could give: ID_LEN lowercase hex
/// digits, and so a name that stays inside the runs directory.
pub fn is_run_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
    pub stop: String,
    pub step: u64,
    pub virtual_ns: u64,
    /// The job's wait status, if init reported one.
    pub status: Option<i32>,
    pub wall_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub created: u64,
    pub source: Source,
    pub spec: Spec,
    /// The run this one was forked from, and the step it was forked at.
    pub parent: Option<(String, u64)>,
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
    pub run: String,
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
