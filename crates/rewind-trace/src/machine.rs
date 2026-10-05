//! The machine a run's spec asks for, as its manifest records it: the CPU
//! the guest is shown, what moves virtual time, where the guest may be
//! interrupted, and whether the extras slot is there. rewind-vmm builds
//! the machine from these.

use serde::{Deserialize, Serialize};

/// The CPU a guest is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuModel {
    /// The host's features, less the ones that break determinism. A run
    /// made this way replays only on the same CPU model: software picks
    /// code paths by the features it sees.
    Host,
    /// A fixed x86-64-v3 CPU: the same features, cache sizes, family and
    /// address widths on every host that supports them, so a run replays
    /// across machines.
    #[default]
    V3,
}

/// What virtual time follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// Exits and idling only: computation between exits takes no time.
    #[default]
    Exits,
    /// Also the guest's work: every retired conditional branch in guest
    /// user mode, counted by the host's performance counter, adds a fixed
    /// share of a nanosecond (rewind-vmm's PS_PER_BRANCH).
    Branches(CounterEvent),
}

/// Which events the host's performance counter counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterEvent {
    /// AMD Zen's retired conditional branches (PMCx0D1), what rr counts on
    /// AMD.
    AmdRetiredConditionalBranches,
    /// Intel's retired conditional branches (BR_INST_RETIRED.CONDITIONAL,
    /// event 0xc4 umask 0x01), what rr counts on Intel.
    IntelRetiredConditionalBranches,
    /// Retired instructions, which some microarchitectures overcount.
    Instructions,
}

/// Where the monitor may interrupt the guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preemption {
    /// Only at exits the guest makes: a thread computing without system
    /// calls runs until it makes one.
    #[default]
    AtExits,
    /// Experimental, with counter time only: also at the branch count
    /// where the armed timer falls due, reached by arming the counter's
    /// overflow short of it and single-stepping the rest. Single-stepping
    /// sets the trap flag, which the guest can see through pushf and
    /// syscall; a process that saves and restores it takes a SIGTRAP that
    /// would not happen outside the VM, as a nixpkgs build of GNU hello
    /// does. So it stays off until the steps are made invisible.
    AtBranchCounts,
}

/// Whether a machine reserves the extras slot, empty persistent memory a
/// fork can fill with more Nix packages (`rewind shell --with`). Every run
/// reserves it; the PMU's self-test machine does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extras {
    #[default]
    Absent,
    Reserved,
}
