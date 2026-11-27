//! Multi-VM discrete-event cluster coordinator.
//!
//! Runs `N` KVM [`Machine`] instances connected through a deterministic
//! [`NetSwitch`] in virtual-time order. Causality is preserved by enforcing
//! that no runnable node advances more than `min_latency_ns` ahead of any
//! other runnable node, so a packet emitted by a slower node can never arrive
//! in a faster node's past.
//!
//! Supports:
//! - Independent per-node clocks with configurable skew and drift (ppm),
//! - Hard node crashes (dropping the VM) and cold restarts (booting a fresh VM
//!   with a deterministic seed derived from `(base_seed, boot_count)`),
//! - Cluster keyframes capturing all nodes + the virtual switch state for
//!   causally-exact seeking, node inspection, and cluster forking.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::net::{
    DropReason, FaultAction, FaultKind, InFlightFrame, NetEvent, NetStats, NetSwitch,
    NetSwitchConfig, NetSwitchState,
};
use crate::snapshot::{Keyframe, Pages};
use crate::{Config, DirtyLog, Machine, Observer, Outcome, Stop};

/// Default virtual-time execution slice per node turn (50 µs, clamped to the
/// switch's `min_latency_ns`).
pub const DEFAULT_SLICE_NS: u64 = 50_000;

/// Maximum VM exits a node executes in a single coordinator slice before
/// yielding back so other nodes stay tightly interleaved even when exits
/// advance time slowly.
const MAX_STEPS_PER_SLICE: u64 = 256;

/// Role of a node in a cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    /// Long-running daemon / replica (e.g., server, database, consensus node).
    #[default]
    Service,
    /// Workload driver / test client whose exit terminates the cluster when
    /// `stop_when == StopWhen::Driver`.
    Driver,
}

/// Condition under which the cluster stops cleanly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopWhen {
    /// Stop once all nodes have exited.
    #[default]
    All,
    /// Stop as soon as any node exits.
    Any,
    /// Stop once all `NodeRole::Driver` nodes have exited (falls back to `All`
    /// if no node has `NodeRole::Driver`).
    Driver,
}

/// Configuration of one node in a cluster.
#[derive(Clone, Debug)]
pub struct ClusterNodeConfig {
    pub name: String,
    pub config: Config,
    pub role: NodeRole,
    /// Initial virtual clock offset relative to cluster time, in nanoseconds.
    pub clock_skew_ns: i64,
    /// Clock rate drift relative to cluster time, in parts per million
    /// (e.g., `+500` = 0.05% faster, `-500` = 0.05% slower).
    pub clock_drift_ppm: i32,
    /// Base seed used to derive deterministic RNG seeds on reboot.
    pub base_seed: u64,
}

/// Full configuration of a multi-VM cluster.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub nodes: Vec<ClusterNodeConfig>,
    pub network: NetSwitchConfig,
    pub stop_when: StopWhen,
    /// Optional cap on cluster virtual time (ns).
    pub max_virtual_ns: Option<u64>,
    /// Optional exact coordinator step at which replay stops (recorded by
    /// interactive sessions on `cluster.finish`).
    pub stop_at_step: Option<u64>,
    /// Virtual time slice per scheduling turn (clamped to `network.min_latency_ns()`).
    pub slice_ns: u64,
}

/// Why a cluster execution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterStop {
    /// Stop condition (`StopWhen`) was satisfied.
    Completed,
    /// Every live node is idle with no armed timer, no in-flight packets, and
    /// no scheduled faults remaining.
    Stalled,
    /// Cluster reached `max_virtual_ns`.
    MaxVirtualTime,
    /// Host wall-clock deadline expired.
    TimedOut,
    /// Observer asked to pause (e.g., seeking to a specific node step).
    Paused,
}

/// Summary of one node when a cluster run ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNodeOutcome {
    pub name: String,
    pub role: NodeRole,
    pub steps: u64,
    pub local_ns: u64,
    pub boot_count: u32,
    pub crashed: bool,
    pub stop: Option<String>,
}

/// Summary of a finished (or paused) cluster execution.
#[derive(Clone, Debug)]
pub struct ClusterOutcome {
    pub stop: ClusterStop,
    pub cluster_ns: u64,
    pub cluster_step: u64,
    pub nodes: Vec<ClusterNodeOutcome>,
    pub net_stats: NetStats,
}

/// Receives per-node trace records and cluster-wide network/fault events.
pub trait ClusterObserver {
    fn node_record(&mut self, _node: usize, _step: u64, _record: &[u8]) {}
    fn node_serial(&mut self, _node: usize, _step: u64, _byte: u8) {}
    fn net_event(&mut self, _event: &NetEvent) {}
    /// Return true to pause the cluster loop at the end of the current slice.
    fn stop(&self) -> bool {
        false
    }
    /// Return true to also interrupt an individual node's slice mid-way (used
    /// when seeking to an exact guest exit step).
    fn stop_mid_slice(&self) -> bool {
        false
    }
}

/// Adapter forwarding a single node's [`Observer`] callbacks to a
/// [`ClusterObserver`].
struct NodeObserver<'a> {
    node: usize,
    step_base: u64,
    inner: &'a mut dyn ClusterObserver,
}

impl Observer for NodeObserver<'_> {
    fn record(&mut self, step: u64, record: &[u8]) {
        self.inner
            .node_record(self.node, self.step_base + step, record);
    }

    fn serial(&mut self, step: u64, byte: u8) {
        self.inner
            .node_serial(self.node, self.step_base + step, byte);
    }

    fn stop(&self) -> bool {
        self.inner.stop_mid_slice()
    }
}

/// Serializable state of one node inside a [`ClusterKeyframe`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterNodeKeyframe {
    pub keyframe: Option<Keyframe>,
    pub idle: bool,
    pub net_rx_pending: bool,
    pub cluster_ns: u64,
    pub local_base_ns: u64,
    pub boot_count: u32,
    pub step_base: u64,
    pub exited: bool,
    pub pending_rx: Vec<InFlightFrame>,
}

/// Whole-cluster snapshot at a deterministic cluster point, sharing the same
/// content-addressed page store as single-node keyframes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterKeyframe {
    pub cluster_step: u64,
    pub cluster_ns: u64,
    pub parent: Option<u64>,
    pub switch: NetSwitchState,
    pub nodes: Vec<ClusterNodeKeyframe>,
}

/// Live runtime state of one node in the cluster.
struct LiveNode {
    cfg: ClusterNodeConfig,
    machine: Option<Machine>,
    /// Cluster virtual time this node has reached.
    cluster_ns: u64,
    /// Local clock value at the start of the current boot (after skew).
    local_base_ns: u64,
    /// Number of times this node has booted (1 = initial boot).
    boot_count: u32,
    /// Total steps accumulated in prior boots before a crash/restart.
    step_base: u64,
    /// Whether this node's VM exited cleanly or faulted (distinct from nemesis crash).
    exited: bool,
    last_stop: Option<Stop>,
    /// Frames that arrived while the guest RX ring was full or before setup.
    pending_rx: VecDeque<InFlightFrame>,
}

impl LiveNode {
    /// Converts a cluster duration to this node's local clock duration using
    /// its integer drift rate (ppm).
    fn cluster_to_local_delta(&self, delta_cluster_ns: u64) -> u64 {
        let rate = (1_000_000i128 + i128::from(self.cfg.clock_drift_ppm)).max(1);
        ((u128::from(delta_cluster_ns) * rate as u128) / 1_000_000u128) as u64
    }

    /// Converts a local clock duration to cluster virtual time duration.
    fn local_to_cluster_delta(&self, delta_local_ns: u64) -> u64 {
        let rate = (1_000_000i128 + i128::from(self.cfg.clock_drift_ppm)).max(1);
        ((u128::from(delta_local_ns) * 1_000_000u128) / rate as u128) as u64
    }

    /// Current total steps across all boots of this node.
    fn total_steps(&self) -> u64 {
        self.step_base + self.machine.as_ref().map_or(0, Machine::step)
    }

    /// Current local virtual clock of this node.
    fn local_now(&self) -> u64 {
        self.machine
            .as_ref()
            .map_or(self.local_base_ns, Machine::now)
    }

    /// Next armed local timer deadline converted to cluster virtual time,
    /// rounded up so a nonzero local wait never rounds down to zero cluster ns.
    fn next_timer_cluster_ns(&self) -> Option<u64> {
        let m = self.machine.as_ref()?;
        let deadline_local = m.timer_deadline()?;
        let now_local = m.now();
        let delta_local = deadline_local.saturating_sub(now_local);
        let rate = (1_000_000i128 + i128::from(self.cfg.clock_drift_ppm)).max(1) as u128;
        let delta_cluster = (u128::from(delta_local) * 1_000_000u128).div_ceil(rate) as u64;
        Some(self.cluster_ns.saturating_add(delta_cluster))
    }

    fn is_runnable(&self) -> bool {
        !self.exited
            && self
                .machine
                .as_ref()
                .is_some_and(|m| !m.is_idle() && m.stopped().is_none())
    }

    fn is_alive(&self) -> bool {
        !self.exited && self.machine.as_ref().is_some_and(|m| m.stopped().is_none())
    }
}

/// Deterministic RNG seed for a node reboot, derived from `(base_seed, boot_count)`.
fn reboot_seed(base_seed: u64, boot_count: u32) -> [u8; 32] {
    let mut state = base_seed
        .wrapping_add(u64::from(boot_count).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        ^ 0x5257_424f_4f54;
    let mut out = [0u8; 32];
    for chunk in out.chunks_mut(8) {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    out
}

/// In-memory trace encoders for one cluster node (`None` when crashed).
pub type NodeEncoders = Option<(
    Option<rewind_trace::io_records::Encoder>,
    Option<rewind_trace::switches::Encoder>,
)>;

/// Multi-VM deterministic cluster.
pub struct Cluster {
    config: ClusterConfig,
    dirty_log: DirtyLog,
    nodes: Vec<LiveNode>,
    switch: NetSwitch,
    cluster_ns: u64,
    cluster_step: u64,
    min_skew_ns: i64,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field("nodes", &self.nodes.len())
            .field("cluster_ns", &self.cluster_ns)
            .field("cluster_step", &self.cluster_step)
            .finish_non_exhaustive()
    }
}

impl Cluster {
    /// Boots all nodes of `config` at cluster time 0.
    pub fn boot(config: ClusterConfig, dirty_log: DirtyLog) -> Result<Cluster> {
        if config.nodes.is_empty() {
            bail!("a cluster must have at least one node");
        }
        let min_skew_ns = config
            .nodes
            .iter()
            .map(|n| n.clock_skew_ns)
            .min()
            .unwrap_or(0)
            .min(0);

        let mut nodes = Vec::with_capacity(config.nodes.len());
        for node_cfg in &config.nodes {
            let mut m = Machine::boot(&node_cfg.config, dirty_log)
                .with_context(|| format!("booting cluster node {}", node_cfg.name))?;
            m.set_cluster_mode(true);
            let initial_local = (node_cfg.clock_skew_ns - min_skew_ns).max(0) as u64;
            if initial_local > 0 {
                m.set_initial_now(initial_local)?;
            }
            nodes.push(LiveNode {
                cfg: node_cfg.clone(),
                machine: Some(m),
                cluster_ns: 0,
                local_base_ns: initial_local,
                boot_count: 1,
                step_base: 0,
                exited: false,
                last_stop: None,
                pending_rx: VecDeque::new(),
            });
        }
        let switch = NetSwitch::new(config.network.clone());
        Ok(Cluster {
            config,
            dirty_log,
            nodes,
            switch,
            cluster_ns: 0,
            cluster_step: 0,
            min_skew_ns,
        })
    }

    /// Restores a cluster from a chain of [`ClusterKeyframe`]s (full first,
    /// target last).
    pub fn restore(
        config: ClusterConfig,
        chain: &[ClusterKeyframe],
        pages: &(dyn Pages + Sync),
        node_traces: &[Vec<(u64, Vec<u8>)>],
        dirty_log: DirtyLog,
    ) -> Result<Cluster> {
        let last = chain.last().context("restoring from no cluster keyframes")?;
        if last.nodes.len() != config.nodes.len() {
            bail!(
                "cluster keyframe has {} nodes, spec has {}",
                last.nodes.len(),
                config.nodes.len()
            );
        }
        let min_skew_ns = config
            .nodes
            .iter()
            .map(|n| n.clock_skew_ns)
            .min()
            .unwrap_or(0)
            .min(0);

        let mut nodes = Vec::with_capacity(config.nodes.len());
        for (idx, node_cfg) in config.nodes.iter().enumerate() {
            let last_nk = &last.nodes[idx];
            let machine = if last_nk.keyframe.is_some() {
                // Collect this node's keyframe chain from the last full keyframe
                // of its current boot.
                let mut node_chain = Vec::new();
                for ck in chain {
                    let nk = &ck.nodes[idx];
                    if nk.boot_count == last_nk.boot_count
                        && let Some(kf) = &nk.keyframe
                    {
                        if kf.parent.is_none() {
                            node_chain.clear();
                        }
                        node_chain.push(kf.clone());
                    }
                }
                let mut m_cfg = node_cfg.config.clone();
                if last_nk.boot_count > 1 {
                    m_cfg.seed = reboot_seed(node_cfg.base_seed, last_nk.boot_count);
                }
                let trace = node_traces.get(idx).map_or(&[][..], Vec::as_slice);
                let mut m = Machine::restore(&m_cfg, &node_chain, pages, trace, dirty_log)
                    .with_context(|| format!("restoring cluster node {}", node_cfg.name))?;
                m.set_cluster_mode(true);
                m.set_idle(last_nk.idle, last_nk.net_rx_pending);
                Some(m)
            } else {
                None
            };

            nodes.push(LiveNode {
                cfg: node_cfg.clone(),
                machine,
                cluster_ns: last_nk.cluster_ns,
                local_base_ns: last_nk.local_base_ns,
                boot_count: last_nk.boot_count,
                step_base: last_nk.step_base,
                exited: last_nk.exited,
                last_stop: None,
                pending_rx: last_nk.pending_rx.iter().cloned().collect(),
            });
        }

        let switch = NetSwitch::restore(config.network.clone(), last.switch.clone());
        Ok(Cluster {
            config,
            dirty_log,
            nodes,
            switch,
            cluster_ns: last.cluster_ns,
            cluster_step: last.cluster_step,
            min_skew_ns,
        })
    }

    /// Captures a synchronized keyframe of the entire cluster (every live node
    /// + the virtual switch).
    pub fn keyframe(
        &mut self,
        pages: &mut dyn Pages,
        parent: Option<u64>,
        node_parents: &[Option<u64>],
    ) -> Result<ClusterKeyframe> {
        let mut node_kfs = Vec::with_capacity(self.nodes.len());
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            let kf = match &mut node.machine {
                Some(m) => {
                    let np = node_parents.get(idx).copied().flatten();
                    Some(m.keyframe(pages, np)?)
                }
                None => None,
            };
            node_kfs.push(ClusterNodeKeyframe {
                keyframe: kf,
                idle: node.machine.as_ref().is_some_and(Machine::is_idle),
                net_rx_pending: node.machine.as_ref().is_some_and(Machine::net_rx_pending),
                cluster_ns: node.cluster_ns,
                local_base_ns: node.local_base_ns,
                boot_count: node.boot_count,
                step_base: node.step_base,
                exited: node.exited,
                pending_rx: node.pending_rx.iter().cloned().collect(),
            });
        }
        Ok(ClusterKeyframe {
            cluster_step: self.cluster_step,
            cluster_ns: self.cluster_ns,
            parent,
            switch: self.switch.snapshot(),
            nodes: node_kfs,
        })
    }

    pub fn cluster_ns(&self) -> u64 {
        self.cluster_ns
    }

    pub fn cluster_step(&self) -> u64 {
        self.cluster_step
    }

    pub fn node_steps(&self, node: usize) -> u64 {
        self.nodes.get(node).map_or(0, LiveNode::total_steps)
    }

    pub fn node_boot_count(&self, node: usize) -> u32 {
        self.nodes.get(node).map_or(0, |n| n.boot_count)
    }

    pub fn switch_snapshot(&self) -> NetSwitchState {
        self.switch.snapshot()
    }

    pub fn switch_stats(&self) -> NetStats {
        self.switch.stats()
    }

    pub fn active_partitions(&self) -> Vec<(usize, usize)> {
        self.switch.active_partitions()
    }

    pub fn node_exited(&self, node: usize) -> bool {
        self.nodes.get(node).is_some_and(|n| n.exited)
    }

    pub fn node_is_crashed(&self, node: usize) -> bool {
        self.switch.is_crashed(node)
    }

    pub fn node_outcomes(&self) -> Vec<ClusterNodeOutcome> {
        self.outcome(ClusterStop::Paused).nodes
    }

    pub fn current_outcome(&self, stop: ClusterStop) -> ClusterOutcome {
        self.outcome(stop)
    }

    pub fn node_machine_mut(&mut self, node: usize) -> Option<&mut Machine> {
        self.nodes.get_mut(node)?.machine.as_mut()
    }

    /// Clones each live node's in-memory trace encoders (including unflushed
    /// switch batches) for interactive session snapshots.
    pub fn node_encoders(&self) -> Vec<NodeEncoders> {
        self.nodes
            .iter()
            .map(|n| n.machine.as_ref().map(Machine::encoders))
            .collect()
    }

    /// Restores each live node's in-memory trace encoders after
    /// [`Cluster::restore`] on an in-progress interactive snapshot.
    pub fn set_node_encoders(&mut self, encoders: &[NodeEncoders]) {
        for (node, enc) in self.nodes.iter_mut().zip(encoders.iter()) {
            if let (Some(m), Some((io, switches))) = (node.machine.as_mut(), enc) {
                m.set_encoders(io.clone(), switches.clone());
            }
        }
    }

    /// Immediately applies an interactive fault (partition, heal, set_link,
    /// crash, restart) at the cluster's current `(cluster_ns, cluster_step)`.
    pub fn apply_fault_now(
        &mut self,
        kind: &FaultKind,
        obs: &mut dyn ClusterObserver,
    ) -> Result<()> {
        let mut net_events = Vec::new();
        let actions = self
            .switch
            .apply_immediate_fault(self.cluster_ns, kind, &mut net_events);
        self.flush_net_events(&mut net_events, obs);
        for action in actions {
            self.apply_fault_action(action, &mut net_events)?;
        }
        self.flush_net_events(&mut net_events, obs);
        Ok(())
    }

    /// Clones a live node's [`Machine`] via a non-destructive in-memory
    /// snapshot so callers can run `cat`/`files` mid-simulation without
    /// perturbing the live VM's state or dirty-page log.
    pub fn clone_node_machine(
        &mut self,
        node: usize,
        node_trace: &[(u64, Vec<u8>)],
    ) -> Result<(Machine, u64)> {
        let live = self
            .nodes
            .get_mut(node)
            .with_context(|| format!("node index {node} out of range"))?;
        let m = live
            .machine
            .as_mut()
            .with_context(|| format!("node {} is currently crashed", live.cfg.name))?;
        let (io, switches) = m.encoders();
        let mut mem_pages = MemPages::default();
        let kf = m.snapshot_non_destructive(&mut mem_pages)?;
        let step = kf.step;
        let mut cfg = live.cfg.config.clone();
        if live.boot_count > 1 {
            cfg.seed = reboot_seed(live.cfg.base_seed, live.boot_count);
        }
        let mut cloned = Machine::restore(&cfg, &[kf], &mem_pages, node_trace, DirtyLog::Off)?;
        cloned.set_encoders(io, switches);
        Ok((cloned, step))
    }

    /// Takes ownership of one node's live [`Machine`] (for `rewind cat`,
    /// `files`, `shell`, or `gdb` after seeking the cluster to a step).
    pub fn take_node_machine(&mut self, node: usize) -> Result<Machine> {
        let live = self
            .nodes
            .get_mut(node)
            .with_context(|| format!("node index {node} out of range"))?;
        let mut m = live
            .machine
            .take()
            .with_context(|| format!("node {} is crashed at this point", live.cfg.name))?;
        m.set_cluster_mode(false);
        Ok(m)
    }

    /// Runs the cluster forward until its stop condition, `until_ns`,
    /// `deadline`, or `obs.stop()` triggers.
    pub fn run(
        &mut self,
        obs: &mut dyn ClusterObserver,
        until_ns: Option<u64>,
        deadline: Option<Instant>,
    ) -> Result<ClusterOutcome> {
        let mut net_events = Vec::new();

        loop {
            let min_lat = self.switch.min_latency_ns();
            let slice_ns = self.config.slice_ns.clamp(1_000, min_lat);

            if obs.stop() {
                return Ok(self.outcome(ClusterStop::Paused));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(self.outcome(ClusterStop::TimedOut));
            }
            if self.stop_condition_met() {
                return Ok(self.outcome(ClusterStop::Completed));
            }
            if let Some(stop_step) = self.config.stop_at_step
                && self.cluster_step >= stop_step
                && self.config.max_virtual_ns.is_none_or(|cap| self.cluster_ns >= cap)
            {
                return Ok(self.outcome(ClusterStop::Completed));
            }
            if let Some(cap) = self.config.max_virtual_ns
                && self.cluster_ns >= cap
                && self.config.stop_at_step.is_none()
            {
                return Ok(self.outcome(ClusterStop::MaxVirtualTime));
            }
            if let Some(target) = until_ns
                && self.cluster_ns >= target
            {
                return Ok(self.outcome(ClusterStop::Paused));
            }

            // Global virtual time floor across runnable nodes (or cluster_ns if none runnable).
            let min_runnable_ns = self
                .nodes
                .iter()
                .filter(|n| n.is_runnable())
                .map(|n| n.cluster_ns)
                .min();
            let event_vt = min_runnable_ns.unwrap_or(self.cluster_ns);

            // 1. Fire any due scripted, interactive, or nemesis faults.
            let actions = self.switch.fire_due_faults(
                event_vt,
                self.cluster_ns,
                self.cluster_step,
                &mut net_events,
            );
            self.flush_net_events(&mut net_events, obs);
            for action in actions {
                self.apply_fault_action(action, &mut net_events)?;
            }
            self.flush_net_events(&mut net_events, obs);

            // 2. Wake any idle nodes whose armed local timer is due at <= event_vt.
            for node in &mut self.nodes {
                if !node.is_alive() || node.is_runnable() {
                    continue;
                }
                if let Some(timer_ns) = node.next_timer_cluster_ns()
                    && timer_ns <= event_vt
                {
                    let wake = timer_ns.max(node.cluster_ns);
                    let delta_c = wake - node.cluster_ns;
                    let mut target_local = node
                        .local_now()
                        .saturating_add(node.cluster_to_local_delta(delta_c));
                    if let Some(deadline_local) =
                        node.machine.as_ref().and_then(Machine::timer_deadline)
                    {
                        target_local = target_local.max(deadline_local);
                    }
                    node.cluster_ns = wake;
                    if let Some(m) = node.machine.as_mut() {
                        m.advance_idle_to(target_local)?;
                    }
                }
            }

            // 3. Deliver any in-flight frames due at <= event_vt (causality-safe:
            //    every runnable node is at >= event_vt, or idle at <= deliver_at_ns).
            self.deliver_due_frames(event_vt, &mut net_events)?;
            self.flush_net_events(&mut net_events, obs);

            // 4. Pick the runnable node with the smallest cluster_ns (tie-break by index).
            let next_runnable = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.is_runnable())
                .min_by_key(|&(i, n)| (n.cluster_ns, i))
                .map(|(i, _)| i);

            if let Some(idx) = next_runnable {
                let node_cns = self.nodes[idx].cluster_ns;

                // Causality horizon: never advance past any other runnable node + min_lat,
                // any idle node's upcoming timer + min_lat, any pending packet for this node,
                // any pending packet for an idle node + min_lat, or the next fault.
                let mut horizon_cns = node_cns.saturating_add(slice_ns);
                if let Some(other_min) = self
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|&(i, n)| i != idx && n.is_runnable())
                    .map(|(_, n)| n.cluster_ns)
                    .min()
                {
                    horizon_cns = horizon_cns.min(other_min.saturating_add(min_lat));
                }
                if let Some(idle_timer) = self
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|&(i, n)| i != idx && n.is_alive() && !n.is_runnable())
                    .filter_map(|(_, n)| n.next_timer_cluster_ns())
                    .min()
                {
                    horizon_cns = horizon_cns.min(idle_timer.saturating_add(min_lat));
                }
                if let Some(rx_t) = self.switch.next_delivery_for(idx) {
                    horizon_cns = horizon_cns.min(rx_t);
                }
                if let Some(any_rx) = self.switch.next_delivery_ns() {
                    horizon_cns = horizon_cns.min(any_rx.saturating_add(min_lat));
                }
                if let Some(fault_t) = self.switch.next_fault_ns() {
                    horizon_cns = horizon_cns.min(fault_t);
                }
                if self.config.stop_at_step.is_none()
                    && let Some(cap) = self.config.max_virtual_ns
                {
                    horizon_cns = horizon_cns.min(cap);
                }
                let delta_cns = horizon_cns.saturating_sub(node_cns).max(1);
                let before_local = self.nodes[idx].local_now();
                let horizon_local =
                    before_local.saturating_add(self.nodes[idx].cluster_to_local_delta(delta_cns));

                let step_base = self.nodes[idx].step_base;
                let outcome = {
                    let m = self.nodes[idx]
                        .machine
                        .as_mut()
                        .expect("runnable node has a live Machine");
                    let mut adapter = NodeObserver {
                        node: idx,
                        step_base,
                        inner: obs,
                    };
                    m.run_cluster_slice(horizon_local, MAX_STEPS_PER_SLICE, &mut adapter)?
                };

                let after_local = self.nodes[idx].local_now();
                let advanced_local = after_local.saturating_sub(before_local);
                let advanced_cluster = self.nodes[idx].local_to_cluster_delta(advanced_local);
                self.nodes[idx].cluster_ns = self.nodes[idx]
                    .cluster_ns
                    .saturating_add(advanced_cluster);
                self.cluster_ns = self.cluster_ns.max(self.nodes[idx].cluster_ns);
                self.cluster_step += 1;

                // Drain any outbound Ethernet frames immediately into the switch.
                let tx_frames = self.nodes[idx]
                    .machine
                    .as_mut()
                    .map_or_else(Vec::new, Machine::take_tx_frames);
                let tx_ns = self.nodes[idx].cluster_ns;
                for frame in tx_frames {
                    self.switch.on_tx(tx_ns, idx, frame, &mut net_events);
                }
                self.flush_net_events(&mut net_events, obs);

                // Retry any queued RX frames for this node if its ring freed up.
                self.drain_pending_rx(idx, &mut net_events)?;
                self.flush_net_events(&mut net_events, obs);

                if let Outcome::Stopped(stop) = outcome {
                    self.nodes[idx].exited = true;
                    self.nodes[idx].last_stop = Some(stop);
                }
                continue;
            }

            // 5. No node is currently runnable: jump virtual time directly to
            //    the earliest upcoming timer, in-flight packet, or fault.
            let next_timer = self
                .nodes
                .iter()
                .filter(|n| n.is_alive())
                .filter_map(LiveNode::next_timer_cluster_ns)
                .min();
            let next_rx = self.switch.next_delivery_ns();
            let next_fault = self.switch.next_any_fault_ns(self.cluster_step);

            let next_wake = [next_timer, next_rx, next_fault]
                .into_iter()
                .flatten()
                .min();

            let Some(wake_ns) = next_wake else {
                let stop = if self.nodes.iter().all(|n| n.exited) {
                    ClusterStop::Completed
                } else {
                    ClusterStop::Stalled
                };
                return Ok(self.outcome(stop));
            };

            if let Some(cap) = self.config.max_virtual_ns
                && wake_ns > cap
            {
                self.cluster_ns = cap;
                return Ok(self.outcome(ClusterStop::MaxVirtualTime));
            }
            if let Some(target) = until_ns
                && wake_ns > target
            {
                self.cluster_ns = target;
                return Ok(self.outcome(ClusterStop::Paused));
            }

            let jump_to = wake_ns.max(self.cluster_ns);
            self.cluster_ns = jump_to;

            // Advance all alive idle nodes up to `jump_to`, firing any timers
            // that are now due.
            for node in &mut self.nodes {
                if !node.is_alive() {
                    continue;
                }
                let timer_due = node
                    .next_timer_cluster_ns()
                    .is_some_and(|t| t <= jump_to);
                if jump_to > node.cluster_ns || timer_due {
                    let delta_c = jump_to.saturating_sub(node.cluster_ns);
                    let delta_l = node.cluster_to_local_delta(delta_c);
                    let mut target_local = node.local_now().saturating_add(delta_l);
                    if timer_due
                        && let Some(deadline_local) =
                            node.machine.as_ref().and_then(Machine::timer_deadline)
                    {
                        target_local = target_local.max(deadline_local);
                    }
                    node.cluster_ns = node.cluster_ns.max(jump_to);
                    if let Some(m) = node.machine.as_mut() {
                        m.advance_idle_to(target_local)?;
                    }
                }
            }
        }
    }

    fn apply_fault_action(
        &mut self,
        action: FaultAction,
        events: &mut Vec<NetEvent>,
    ) -> Result<()> {
        match action {
            FaultAction::Crash(idx) => {
                if let Some(node) = self.nodes.get_mut(idx) {
                    if let Some(m) = node.machine.take() {
                        node.step_base += m.step();
                    }
                    node.pending_rx.clear();
                }
            }
            FaultAction::Restart(idx) => {
                let cluster_ns = self.cluster_ns;
                let min_skew_ns = self.min_skew_ns;
                let dirty_log = self.dirty_log;
                if let Some(node) = self.nodes.get_mut(idx) {
                    node.boot_count += 1;
                    let mut cfg = node.cfg.config.clone();
                    cfg.seed = reboot_seed(node.cfg.base_seed, node.boot_count);
                    // Advance wall-clock epoch to reflect elapsed cluster time.
                    cfg.epoch = cfg.epoch.saturating_add(cluster_ns / 1_000_000_000);
                    let mut m = Machine::boot(&cfg, dirty_log).with_context(|| {
                        format!(
                            "restarting cluster node {} (boot {})",
                            node.cfg.name, node.boot_count
                        )
                    })?;
                    m.set_cluster_mode(true);
                    let initial_local = (node.cfg.clock_skew_ns - min_skew_ns).max(0) as u64;
                    if initial_local > 0 {
                        m.set_initial_now(initial_local)?;
                    }
                    node.machine = Some(m);
                    node.cluster_ns = cluster_ns;
                    node.local_base_ns = initial_local;
                    node.exited = false;
                    node.last_stop = None;
                    node.pending_rx.clear();
                    events.push(NetEvent::NodeRestart {
                        ns: cluster_ns,
                        node: idx,
                        boot_count: node.boot_count,
                    });
                }
            }
        }
        Ok(())
    }

    fn deliver_due_frames(&mut self, up_to_ns: u64, events: &mut Vec<NetEvent>) -> Result<()> {
        // Pop all frames due at or before `up_to_ns`.
        let due = self.switch.pop_due_frames(up_to_ns);
        for frame in due {
            let dst = frame.dst;
            if self.switch.is_crashed(dst) || !self.nodes[dst].is_alive() {
                self.switch
                    .note_dropped(frame.deliver_at_ns, &frame, DropReason::NodeCrashed, events);
                continue;
            }
            if self.switch.is_blocked(frame.src, dst) {
                self.switch
                    .note_dropped(frame.deliver_at_ns, &frame, DropReason::Partitioned, events);
                continue;
            }
            // Bring idle destination node's clock up to the packet arrival time
            // before delivering the RX interrupt.
            if self.nodes[dst].cluster_ns < frame.deliver_at_ns {
                let delta_c = frame.deliver_at_ns - self.nodes[dst].cluster_ns;
                let delta_l = self.nodes[dst].cluster_to_local_delta(delta_c);
                let target_local = self.nodes[dst].local_now().saturating_add(delta_l);
                self.nodes[dst].cluster_ns = frame.deliver_at_ns;
                if let Some(m) = self.nodes[dst].machine.as_mut() {
                    m.advance_idle_to(target_local)?;
                }
            }

            let delivered = match self.nodes[dst].machine.as_mut() {
                Some(m) => m.deliver_rx_frame(&frame.frame)?,
                None => false,
            };
            if delivered {
                self.switch.note_delivered(&frame, events);
            } else if self.nodes[dst].pending_rx.len() < 512 {
                self.nodes[dst].pending_rx.push_back(frame);
            } else {
                self.switch
                    .note_dropped(frame.deliver_at_ns, &frame, DropReason::RxOverflow, events);
            }
        }
        Ok(())
    }

    fn drain_pending_rx(&mut self, idx: usize, events: &mut Vec<NetEvent>) -> Result<()> {
        while let Some(front) = self.nodes[idx].pending_rx.front().cloned() {
            let delivered = match self.nodes[idx].machine.as_mut() {
                Some(m) => m.deliver_rx_frame(&front.frame)?,
                None => false,
            };
            if delivered {
                self.nodes[idx].pending_rx.pop_front();
                self.switch.note_delivered(&front, events);
            } else {
                break;
            }
        }
        Ok(())
    }

    fn flush_net_events(&self, events: &mut Vec<NetEvent>, obs: &mut dyn ClusterObserver) {
        for ev in events.drain(..) {
            obs.net_event(&ev);
        }
    }

    fn stop_condition_met(&self) -> bool {
        match self.config.stop_when {
            StopWhen::All => self.nodes.iter().all(|n| n.exited),
            StopWhen::Any => self.nodes.iter().any(|n| n.exited),
            StopWhen::Driver => {
                let has_driver = self.nodes.iter().any(|n| n.cfg.role == NodeRole::Driver);
                if has_driver {
                    self.nodes
                        .iter()
                        .filter(|n| n.cfg.role == NodeRole::Driver)
                        .all(|n| n.exited)
                } else {
                    self.nodes.iter().all(|n| n.exited)
                }
            }
        }
    }

    fn outcome(&self, stop: ClusterStop) -> ClusterOutcome {
        let nodes = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| ClusterNodeOutcome {
                name: n.cfg.name.clone(),
                role: n.cfg.role,
                steps: n.total_steps(),
                local_ns: n.local_now(),
                boot_count: n.boot_count,
                crashed: self.switch.is_crashed(i),
                stop: n.last_stop.map(|s| format!("{s:?}")),
            })
            .collect();
        ClusterOutcome {
            stop,
            cluster_ns: self.cluster_ns,
            cluster_step: self.cluster_step,
            nodes,
            net_stats: self.switch.stats(),
        }
    }
}

/// Ephemeral in-memory page map used by [`Cluster::clone_node_machine`] for
/// zero-perturbation live file inspection. Uses monotonic non-zero page IDs
/// to avoid hashing overhead while keeping zero pages distinct.
#[derive(Default)]
struct MemPages {
    next_id: u64,
    pages: HashMap<[u8; 32], Vec<u8>>,
}

impl Pages for MemPages {
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        let mut hashes = Vec::with_capacity(pages.len());
        for p in pages {
            self.next_id += 1;
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&self.next_id.to_le_bytes());
            self.pages.insert(key, p.to_vec());
            hashes.push(key);
        }
        Ok(hashes)
    }

    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        let data = self
            .pages
            .get(hash)
            .context("missing page in ephemeral snapshot")?;
        out.copy_from_slice(data);
        Ok(())
    }
}
