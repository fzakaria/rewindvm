//! Cluster manifests, declarative spec files (TOML/JSON), and verdicts for
//! multi-node Rewind VM runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::machine::{ClockSource, CpuModel};
use crate::manifest::{RunId, Spec};
use crate::verdict::Verdict;

/// The manifest file inside a cluster run directory (`clusters/<id>/cluster.json`).
pub const CLUSTER_MANIFEST: &str = "cluster.json";

/// The binary network & fault event log inside a cluster run directory
/// (`clusters/<id>/network.bin`).
pub const CLUSTER_NETWORK_LOG: &str = "network.bin";

/// The directory holding per-node run directories inside a cluster run directory.
pub const CLUSTER_NODES_DIR: &str = "nodes";

/// The directory holding cluster keyframes (`*.ckf`) inside a cluster run directory.
pub const CLUSTER_KEYFRAMES_DIR: &str = "keyframes";

/// Extension for cluster keyframe files.
pub const CLUSTER_KEYFRAME_EXT: &str = "ckf";

/// Version of the cluster manifest format.
pub const CLUSTER_MANIFEST_VERSION: u32 = 1;

/// User-facing declarative cluster specification (loaded from `.toml` or `.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSpecFile {
    pub name: String,
    #[serde(default)]
    pub mem_mib: Option<u64>,
    #[serde(default)]
    pub cores: Option<u32>,
    /// `"all"`, `"any"`, or `"driver"` (defaults to `"driver"` if any node has
    /// `role = "driver"`, else `"all"`).
    #[serde(default)]
    pub stop_when: Option<String>,
    /// Maximum cluster virtual time in milliseconds before stopping.
    #[serde(default)]
    pub max_virtual_ms: Option<u64>,
    /// Virtual-time slice per node turn in microseconds (default 50 µs).
    #[serde(default)]
    pub slice_us: Option<u64>,
    /// Cluster-wide default Nix installables or `/nix/store/...` paths mounted
    /// at `/nix/store` on every node (merged with each node's `packages`).
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub network: NetworkSpecFile,
    #[serde(default)]
    pub faults: Vec<FaultSpecFile>,
    #[serde(default)]
    pub nemesis: Option<NemesisSpecFile>,
    pub nodes: Vec<NodeSpecFile>,
}

/// Network settings in a cluster spec file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSpecFile {
    /// IPv4 CIDR subnet (default `"10.0.0.0/24"`).
    #[serde(default)]
    pub subnet: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub latency_us: Option<u64>,
    #[serde(default)]
    pub jitter_ms: Option<u64>,
    #[serde(default)]
    pub jitter_us: Option<u64>,
    /// Packet drop probability in parts per million (`0..=1_000_000`).
    #[serde(default)]
    pub drop_ppm: Option<u32>,
    /// Packet duplication probability in parts per million (`0..=1_000_000`).
    #[serde(default)]
    pub duplicate_ppm: Option<u32>,
    #[serde(default)]
    pub links: Vec<LinkSpecFile>,
}

/// Per-directed-link override in a cluster spec file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSpecFile {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub latency_us: Option<u64>,
    #[serde(default)]
    pub jitter_ms: Option<u64>,
    #[serde(default)]
    pub jitter_us: Option<u64>,
    #[serde(default)]
    pub drop_ppm: Option<u32>,
}

/// One scripted fault entry in a cluster spec file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultSpecFile {
    pub at_ms: u64,
    /// `"partition"`, `"asymmetric_partition"`, `"heal"`, `"crash"`, `"restart"`.
    pub kind: String,
    #[serde(default)]
    pub groups: Vec<Vec<String>>,
    #[serde(default)]
    pub blocked: Vec<(String, String)>,
    #[serde(default)]
    pub node: Option<String>,
}

/// Schedule-driven nemesis settings in a cluster spec file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NemesisSpecFile {
    #[serde(default = "default_nemesis_tick_ms")]
    pub tick_ms: u64,
    #[serde(default)]
    pub partition_prob_ppm: u32,
    #[serde(default = "default_heal_prob_ppm")]
    pub heal_prob_ppm: u32,
    #[serde(default)]
    pub crash_prob_ppm: u32,
    #[serde(default = "default_restart_prob_ppm")]
    pub restart_prob_ppm: u32,
    #[serde(default = "default_max_crashed")]
    pub max_crashed: usize,
}

fn default_nemesis_tick_ms() -> u64 {
    100
}
fn default_heal_prob_ppm() -> u32 {
    250_000
}
fn default_restart_prob_ppm() -> u32 {
    300_000
}
fn default_max_crashed() -> usize {
    1
}

/// One node entry in a cluster spec file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpecFile {
    pub name: String,
    #[serde(default)]
    pub ip: Option<String>,
    /// Root filesystem (directory, `.erofs`, or `.tar`). Omit to run inside
    /// the guest initramfs (e.g. `/bin/sh` or `/rewind/tools/bin/...`).
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Nix installables (e.g. `nixpkgs#redis`, `.#my-server`) or `/nix/store/...`
    /// paths whose closure is packed into a cached EROFS image and mounted at
    /// `/nix/store`, with each package's `bin/` prepended to `PATH`.
    /// Mutually exclusive with `root`.
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub mem_mib: Option<u64>,
    #[serde(default)]
    pub cores: Option<u32>,
    #[serde(default)]
    pub clock_skew_ms: i64,
    #[serde(default)]
    pub clock_drift_ppm: i32,
    /// `"service"` (default) or `"driver"`.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub files: Vec<NodeFileSpec>,
}

/// An inline file written into a node's filesystem before its command starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFileSpec {
    pub path: String,
    pub contents: String,
}

/// Resolved cluster manifest persisted in `clusters/<id>/cluster.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterManifest {
    pub version: u32,
    pub id: RunId,
    pub name: String,
    pub created: u64,
    pub spec_source: String,
    pub seed: u64,
    pub schedule: u64,
    pub fault_window_from_ns: u64,
    pub fault_window_until_ns: u64,
    pub epoch: u64,
    pub cpu: CpuModel,
    pub clock: ClockSource,
    pub stop_when: String,
    pub max_virtual_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_at_step: Option<u64>,
    pub slice_ns: u64,
    pub network_json: serde_json::Value,
    pub nodes: Vec<ClusterNodeManifest>,
    pub parent: Option<ClusterParent>,
    pub outcome: Option<ClusterRunOutcome>,
    pub verdict: ClusterVerdict,
    pub trace_hash: Option<String>,
    pub recorded_by: String,
}

/// Parent cluster run from which a forked cluster branched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterParent {
    pub cluster: RunId,
    pub at_ns: u64,
}

/// Per-node resolved inputs inside a [`ClusterManifest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNodeManifest {
    pub name: String,
    pub ip: String,
    pub mac: [u8; 6],
    pub role: String,
    pub clock_skew_ns: i64,
    pub clock_drift_ppm: i32,
    pub spec: Spec,
}

/// Recorded outcome of a cluster execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterRunOutcome {
    pub stop: String,
    pub cluster_ns: u64,
    pub cluster_step: u64,
    pub wall_ms: u64,
    pub nodes: Vec<ClusterNodeResult>,
    pub tx_frames: u64,
    pub delivered_frames: u64,
    pub dropped_frames: u64,
    pub duplicated_frames: u64,
    pub partitions: u64,
    pub crashes: u64,
    pub restarts: u64,
}

/// Per-node final outcome inside [`ClusterRunOutcome`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNodeResult {
    pub name: String,
    pub role: String,
    pub steps: u64,
    pub local_ns: u64,
    pub boot_count: u32,
    pub status: Option<i32>,
    pub verdict: Verdict,
}

/// Whole-cluster pass/fail verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClusterVerdict {
    Unfinished,
    Passed,
    Failed { reasons: Vec<String> },
}

impl ClusterVerdict {
    pub fn passed(&self) -> bool {
        *self == ClusterVerdict::Passed
    }

    pub fn failed(&self) -> bool {
        matches!(self, ClusterVerdict::Failed { .. })
    }
}
