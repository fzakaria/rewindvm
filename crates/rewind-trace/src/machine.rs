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
    /// The V3 model with ERMS (fast REP MOVSB/STOSB) hidden, for hosts
    /// that lack ERMS, such as an EPYC 7763 (Zen 3) host whose CPUID does
    /// not show it. Everything else is the V3 model's, so a run made this
    /// way replays on hosts with or without ERMS. A V3Compat run is not a
    /// V3 run: the guest sees a different CPU, so the same inputs make a
    /// different run.
    V3Compat,
}

impl CpuModel {
    /// The model as `--cpu` takes it.
    pub fn cli_name(self) -> &'static str {
        match self {
            CpuModel::Host => "host",
            CpuModel::V3 => "v3",
            CpuModel::V3Compat => "v3-compat",
        }
    }
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
    /// Intel's retired conditional branches (BR_INST_RETIRED.COND), what rr
    /// counts on Intel: event 0xc4, with the umask of the host's
    /// microarchitecture and kind of core, 0x01 up to Skylake and 0x11,
    /// 0x111, 0x7e or 0x07 after it (rewind-vmm's pmu module).
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
/// reserves it, except a host-model run on a CPU whose physical address
/// bits do not reach it; the PMU's self-test machine does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extras {
    #[default]
    Absent,
    Reserved,
}

#[cfg(test)]
mod tests {
    // How the CPU models are spelled in a manifest, by serializing each
    // model to JSON and reading it back.
    use super::*;

    #[test]
    fn every_cpu_model_round_trips_under_its_manifest_name() {
        // The names are what existing manifests hold, so they must not
        // change: "v3" and "host" are in every run made so far, and
        // "v3_compat" is the v3 model without ERMS.
        for (model, name) in [
            (CpuModel::Host, "\"host\""),
            (CpuModel::V3, "\"v3\""),
            (CpuModel::V3Compat, "\"v3_compat\""),
        ] {
            assert_eq!(serde_json::to_string(&model).unwrap(), name);
            assert_eq!(serde_json::from_str::<CpuModel>(name).unwrap(), model);
        }
    }
}
