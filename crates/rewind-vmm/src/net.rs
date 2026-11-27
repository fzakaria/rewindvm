//! Deterministic L2 virtual Ethernet switch and fault injector for multi-node
//! Rewind VM clusters.
//!
//! Every frame transmitted by a guest via `PORT_NET_TX` is routed here by
//! Ethernet destination MAC (unicast or broadcast/multicast). Delivery
//! latency, jitter/reordering, packet drops, packet duplication, network
//! partitions (symmetric or asymmetric), heals, and nemesis-driven node
//! crashes/restarts are all pure functions of the switch's seed, virtual
//! timestamps, and frame sequence numbers, so replaying a cluster reproduces
//! the exact same packet deliveries and fault timings every time.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::io::Write;

use serde::{Deserialize, Serialize};

/// Minimum link latency (10 µs) enforced for strict discrete-event lookahead
/// causality: no runnable node ever advances more than `min_latency_ns` ahead
/// of another runnable node, so a packet can never arrive in a peer's past.
pub const MIN_LINK_LATENCY_NS: u64 = 10_000;

/// Default base link latency (1 ms).
pub const DEFAULT_LATENCY_NS: u64 = 1_000_000;

/// Parts-per-million denominator for integer probabilities (no floats in
/// deterministic simulation or manifests).
pub const PPM_SCALE: u32 = 1_000_000;

/// Deterministic locally-administered Ethernet MAC for node `index`.
pub fn mac_for_node(index: usize) -> [u8; 6] {
    let id = (index + 1) as u16;
    [0x02, 0x52, 0x57, 0x00, (id >> 8) as u8, (id & 0xff) as u8]
}

/// Per-directed-link override of latency, jitter, drop, or duplication rate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkOverride {
    pub from: usize,
    pub to: usize,
    pub latency_ns: Option<u64>,
    pub jitter_ns: Option<u64>,
    pub drop_ppm: Option<u32>,
    #[serde(default)]
    pub duplicate_ppm: Option<u32>,
}

/// A deterministic fault scheduled at a specific cluster virtual timestamp
/// (and optionally pinned to a cluster coordinator step for exact interactive
/// replay).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledFault {
    pub at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_step: Option<u64>,
    pub kind: FaultKind,
}

/// Kinds of scripted or interactive cluster faults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FaultKind {
    /// Symmetric network partition: nodes in different groups cannot reach
    /// each other; nodes in the same group can.
    Partition { groups: Vec<Vec<usize>> },
    /// Asymmetric partition: blocks the listed directed `(src, dst)` pairs.
    AsymmetricPartition { blocked: Vec<(usize, usize)> },
    /// Heals all active network partitions.
    Heal,
    /// Heals only the listed directed `(src, dst)` links.
    HealLinks { links: Vec<(usize, usize)> },
    /// Dynamically updates link parameters for `(from, to)`.
    SetLink {
        from: usize,
        to: usize,
        latency_ns: Option<u64>,
        jitter_ns: Option<u64>,
        drop_ppm: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duplicate_ppm: Option<u32>,
    },
    /// Hard power-off of `node` (drops its VM immediately).
    Crash { node: usize },
    /// Cold reboot of a previously crashed `node`.
    Restart { node: usize },
}

/// Schedule-seed-driven nemesis configuration for `rewind cluster check`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NemesisConfig {
    /// Virtual time between nemesis evaluation ticks.
    pub tick_ns: u64,
    /// Probability (ppm) on a tick to inject a random partition when none is active.
    pub partition_prob_ppm: u32,
    /// Probability (ppm) on a tick to heal an active partition.
    pub heal_prob_ppm: u32,
    /// Probability (ppm) on a tick to crash an eligible live node.
    pub crash_prob_ppm: u32,
    /// Probability (ppm) on a tick to restart a crashed node.
    pub restart_prob_ppm: u32,
    /// Maximum number of simultaneously crashed nodes.
    pub max_crashed: usize,
    /// Node indices the nemesis is allowed to crash (typically `service` nodes,
    /// never the test `driver`).
    pub eligible_nodes: Vec<usize>,
}

/// Static configuration of the virtual L2 switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSwitchConfig {
    pub node_count: usize,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub drop_ppm: u32,
    pub duplicate_ppm: u32,
    /// Deterministic PRNG seed (combined with cluster schedule seed).
    pub seed: u64,
    /// Cluster virtual time window `[start, end)` during which stochastic
    /// perturbations (jitter, random drops, duplicates, and nemesis) are active.
    /// `0..u64::MAX` for the whole run; narrowed during `rewind cluster check`
    /// bisection.
    pub fault_window: std::ops::Range<u64>,
    pub links: Vec<LinkOverride>,
    pub faults: Vec<ScheduledFault>,
    pub nemesis: Option<NemesisConfig>,
}

impl Default for NetSwitchConfig {
    fn default() -> NetSwitchConfig {
        NetSwitchConfig {
            node_count: 0,
            latency_ns: DEFAULT_LATENCY_NS,
            jitter_ns: 0,
            drop_ppm: 0,
            duplicate_ppm: 0,
            seed: 0,
            fault_window: 0..u64::MAX,
            links: Vec::new(),
            faults: Vec::new(),
            nemesis: None,
        }
    }
}

impl NetSwitchConfig {
    /// The minimum delivery latency across the default link and all link
    /// overrides, clamped to at least [`MIN_LINK_LATENCY_NS`].
    pub fn min_latency_ns(&self) -> u64 {
        let mut min = self.latency_ns;
        for link in &self.links {
            if let Some(l) = link.latency_ns {
                min = min.min(l);
            }
        }
        min.max(MIN_LINK_LATENCY_NS)
    }
}

/// One Ethernet frame currently in flight on the virtual switch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InFlightFrame {
    pub deliver_at_ns: u64,
    pub seq: u64,
    pub src: usize,
    pub dst: usize,
    pub tx_at_ns: u64,
    pub frame: Vec<u8>,
}

/// Why a frame was dropped by the virtual switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    Partitioned,
    RandomDrop,
    NodeCrashed,
    RxOverflow,
}

/// A deterministic cluster network or fault event, recorded to `network.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetEvent {
    Tx {
        ns: u64,
        src: usize,
        dst_mac: String,
        len: usize,
        frame_hash: u64,
        summary: String,
    },
    Deliver {
        ns: u64,
        src: usize,
        dst: usize,
        len: usize,
        latency_ns: u64,
    },
    Drop {
        ns: u64,
        src: usize,
        dst: usize,
        len: usize,
        reason: DropReason,
    },
    Duplicate {
        ns: u64,
        src: usize,
        dst: usize,
        len: usize,
        extra_delay_ns: u64,
    },
    Partition {
        ns: u64,
        blocked_pairs: usize,
        asymmetric: bool,
    },
    Heal {
        ns: u64,
    },
    NodeCrash {
        ns: u64,
        node: usize,
    },
    NodeRestart {
        ns: u64,
        node: usize,
        boot_count: u32,
    },
    LinkUpdated {
        ns: u64,
        from: usize,
        to: usize,
        latency_ns: u64,
        jitter_ns: u64,
        drop_ppm: u32,
        duplicate_ppm: u32,
    },
}

/// Action the switch instructs the cluster coordinator to perform on a VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAction {
    Crash(usize),
    Restart(usize),
}

/// Cumulative switch counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStats {
    pub tx_frames: u64,
    pub delivered_frames: u64,
    pub dropped_frames: u64,
    pub duplicated_frames: u64,
    pub partitions: u64,
    pub crashes: u64,
    pub restarts: u64,
}

/// Serializable snapshot of the virtual switch, stored in a cluster keyframe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSwitchState {
    pub in_flight: Vec<InFlightFrame>,
    pub blocked: BTreeSet<(usize, usize)>,
    pub crashed: BTreeSet<usize>,
    #[serde(default)]
    pub links: BTreeMap<(usize, usize), LinkOverride>,
    pub next_seq: u64,
    pub prng_state: u64,
    pub next_fault_idx: usize,
    pub next_nemesis_ns: u64,
    pub stats: NetStats,
}

/// Deterministic L2 Ethernet switch and fault injector.
#[derive(Clone, Debug)]
pub struct NetSwitch {
    config: NetSwitchConfig,
    macs: Vec<[u8; 6]>,
    in_flight: BinaryHeap<Reverse<InFlightFrame>>,
    blocked: BTreeSet<(usize, usize)>,
    crashed: BTreeSet<usize>,
    links: BTreeMap<(usize, usize), LinkOverride>,
    next_seq: u64,
    prng_state: u64,
    next_fault_idx: usize,
    next_nemesis_ns: u64,
    stats: NetStats,
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl NetSwitch {
    /// Creates a new virtual switch for `config`.
    pub fn new(mut config: NetSwitchConfig) -> NetSwitch {
        config.faults.sort_by_key(|f| (f.at_ns, f.at_step.unwrap_or(0)));
        let macs = (0..config.node_count).map(mac_for_node).collect();
        let mut links = BTreeMap::new();
        for l in &config.links {
            links.insert((l.from, l.to), l.clone());
        }
        let next_nemesis_ns = config.nemesis.as_ref().map_or(u64::MAX, |n| n.tick_ns.max(1));
        let prng_state = config.seed ^ 0x5257_4e45_5400_0001;
        NetSwitch {
            config,
            macs,
            in_flight: BinaryHeap::new(),
            blocked: BTreeSet::new(),
            crashed: BTreeSet::new(),
            links,
            next_seq: 1,
            prng_state,
            next_fault_idx: 0,
            next_nemesis_ns,
            stats: NetStats::default(),
        }
    }

    /// Restores a switch from a keyframe snapshot.
    pub fn restore(mut config: NetSwitchConfig, state: NetSwitchState) -> NetSwitch {
        config.faults.sort_by_key(|f| (f.at_ns, f.at_step.unwrap_or(0)));
        let macs = (0..config.node_count).map(mac_for_node).collect();
        let in_flight = state.in_flight.into_iter().map(Reverse).collect();
        let links = if state.links.is_empty() && !config.links.is_empty() {
            config
                .links
                .iter()
                .map(|l| ((l.from, l.to), l.clone()))
                .collect()
        } else {
            state.links
        };
        NetSwitch {
            config,
            macs,
            in_flight,
            blocked: state.blocked,
            crashed: state.crashed,
            links,
            next_seq: state.next_seq,
            prng_state: state.prng_state,
            next_fault_idx: state.next_fault_idx,
            next_nemesis_ns: state.next_nemesis_ns,
            stats: state.stats,
        }
    }

    /// Captures the switch's full state for a cluster keyframe.
    pub fn snapshot(&self) -> NetSwitchState {
        let mut in_flight: Vec<InFlightFrame> =
            self.in_flight.iter().map(|r| r.0.clone()).collect();
        in_flight.sort();
        NetSwitchState {
            in_flight,
            blocked: self.blocked.clone(),
            crashed: self.crashed.clone(),
            links: self.links.clone(),
            next_seq: self.next_seq,
            prng_state: self.prng_state,
            next_fault_idx: self.next_fault_idx,
            next_nemesis_ns: self.next_nemesis_ns,
            stats: self.stats,
        }
    }

    pub fn stats(&self) -> NetStats {
        self.stats
    }

    pub fn min_latency_ns(&self) -> u64 {
        let mut min = self.config.latency_ns;
        for link in self.links.values() {
            if let Some(l) = link.latency_ns {
                min = min.min(l);
            }
        }
        min.max(MIN_LINK_LATENCY_NS)
    }

    pub fn active_partitions(&self) -> Vec<(usize, usize)> {
        self.blocked.iter().copied().collect()
    }

    pub fn is_crashed(&self, node: usize) -> bool {
        self.crashed.contains(&node)
    }

    /// Routes one frame transmitted by `src` at cluster time `now_ns`.
    pub fn on_tx(
        &mut self,
        now_ns: u64,
        src: usize,
        frame: Vec<u8>,
        events: &mut Vec<NetEvent>,
    ) {
        if frame.len() < 14 {
            return;
        }
        self.stats.tx_frames += 1;
        let dst_mac: [u8; 6] = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
        events.push(NetEvent::Tx {
            ns: now_ns,
            src,
            dst_mac: format_mac(dst_mac),
            len: frame.len(),
            frame_hash: hash_frame(&frame),
            summary: summarize_frame(&frame),
        });

        let is_broadcast_or_multicast = dst_mac == [0xff; 6] || (dst_mac[0] & 1) != 0;
        let destinations: Vec<usize> = if is_broadcast_or_multicast {
            (0..self.config.node_count)
                .filter(|&i| i != src)
                .collect()
        } else {
            self.macs
                .iter()
                .enumerate()
                .filter(|&(i, m)| i != src && *m == dst_mac)
                .map(|(i, _)| i)
                .collect()
        };

        let in_window = self.config.fault_window.contains(&now_ns);
        let min_lat = self.min_latency_ns();

        for dst in destinations {
            if self.crashed.contains(&dst) {
                self.stats.dropped_frames += 1;
                events.push(NetEvent::Drop {
                    ns: now_ns,
                    src,
                    dst,
                    len: frame.len(),
                    reason: DropReason::NodeCrashed,
                });
                continue;
            }
            if self.blocked.contains(&(src, dst)) {
                self.stats.dropped_frames += 1;
                events.push(NetEvent::Drop {
                    ns: now_ns,
                    src,
                    dst,
                    len: frame.len(),
                    reason: DropReason::Partitioned,
                });
                continue;
            }

            let link = self.links.get(&(src, dst));
            let base_latency = link
                .and_then(|l| l.latency_ns)
                .unwrap_or(self.config.latency_ns)
                .max(min_lat);
            let jitter = if in_window {
                link.and_then(|l| l.jitter_ns)
                    .unwrap_or(self.config.jitter_ns)
            } else {
                0
            };
            let drop_ppm = if in_window {
                link.and_then(|l| l.drop_ppm)
                    .unwrap_or(self.config.drop_ppm)
            } else {
                0
            };
            let dup_ppm = if in_window {
                link.and_then(|l| l.duplicate_ppm)
                    .unwrap_or(self.config.duplicate_ppm)
            } else {
                0
            };

            if drop_ppm > 0 {
                let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
                if draw < drop_ppm {
                    self.stats.dropped_frames += 1;
                    events.push(NetEvent::Drop {
                        ns: now_ns,
                        src,
                        dst,
                        len: frame.len(),
                        reason: DropReason::RandomDrop,
                    });
                    continue;
                }
            }

            let jitter_draw = if jitter > 0 {
                splitmix64(&mut self.prng_state) % (jitter + 1)
            } else {
                0
            };
            let deliver_at_ns = now_ns.saturating_add(base_latency).saturating_add(jitter_draw);
            let seq = self.next_seq;
            self.next_seq += 1;
            self.in_flight.push(Reverse(InFlightFrame {
                deliver_at_ns,
                seq,
                src,
                dst,
                tx_at_ns: now_ns,
                frame: frame.clone(),
            }));

            if dup_ppm > 0 {
                let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
                if draw < dup_ppm {
                    let extra = if jitter > 0 {
                        (splitmix64(&mut self.prng_state) % (jitter + 1)).max(1)
                    } else {
                        min_lat
                    };
                    let dup_seq = self.next_seq;
                    self.next_seq += 1;
                    self.stats.duplicated_frames += 1;
                    events.push(NetEvent::Duplicate {
                        ns: now_ns,
                        src,
                        dst,
                        len: frame.len(),
                        extra_delay_ns: extra,
                    });
                    self.in_flight.push(Reverse(InFlightFrame {
                        deliver_at_ns: deliver_at_ns.saturating_add(extra),
                        seq: dup_seq,
                        src,
                        dst,
                        tx_at_ns: now_ns,
                        frame: frame.clone(),
                    }));
                }
            }
        }
    }

    /// Earliest delivery timestamp of any in-flight frame.
    pub fn next_delivery_ns(&self) -> Option<u64> {
        self.in_flight.peek().map(|r| r.0.deliver_at_ns)
    }

    /// Earliest delivery timestamp of any in-flight frame addressed to `dst`.
    pub fn next_delivery_for(&self, dst: usize) -> Option<u64> {
        self.in_flight
            .iter()
            .filter(|r| r.0.dst == dst)
            .map(|r| r.0.deliver_at_ns)
            .min()
    }

    /// Pops all frames whose `deliver_at_ns <= up_to_ns`, in deterministic
    /// `(deliver_at_ns, seq)` order.
    pub fn pop_due_frames(&mut self, up_to_ns: u64) -> Vec<InFlightFrame> {
        let mut due = Vec::new();
        while let Some(Reverse(top)) = self.in_flight.peek() {
            if top.deliver_at_ns > up_to_ns {
                break;
            }
            if let Some(Reverse(frame)) = self.in_flight.pop() {
                due.push(frame);
            }
        }
        due
    }

    /// Records a successful delivery in stats & events.
    pub fn note_delivered(&mut self, frame: &InFlightFrame, events: &mut Vec<NetEvent>) {
        self.stats.delivered_frames += 1;
        events.push(NetEvent::Deliver {
            ns: frame.deliver_at_ns,
            src: frame.src,
            dst: frame.dst,
            len: frame.frame.len(),
            latency_ns: frame.deliver_at_ns.saturating_sub(frame.tx_at_ns),
        });
    }

    /// Records a drop of an already in-flight frame (e.g., destination crashed
    /// or was partitioned after transmission, or RX ring overflowed).
    pub fn note_dropped(
        &mut self,
        now_ns: u64,
        frame: &InFlightFrame,
        reason: DropReason,
        events: &mut Vec<NetEvent>,
    ) {
        self.stats.dropped_frames += 1;
        events.push(NetEvent::Drop {
            ns: now_ns,
            src: frame.src,
            dst: frame.dst,
            len: frame.frame.len(),
            reason,
        });
    }

    /// Whether a frame from `src` to `dst` is currently blocked by a partition.
    pub fn is_blocked(&self, src: usize, dst: usize) -> bool {
        self.blocked.contains(&(src, dst))
    }

    /// Earliest timestamp of any upcoming time-scheduled fault (skipping
    /// step-pinned interactive faults so replay preserves the original live
    /// scheduling horizons) or nemesis tick.
    pub fn next_fault_ns(&self) -> Option<u64> {
        let scripted = self.config.faults[self.next_fault_idx..]
            .iter()
            .find(|f| f.at_step.is_none())
            .map(|f| f.at_ns);
        let nemesis = self
            .config
            .nemesis
            .as_ref()
            .filter(|_| self.next_nemesis_ns < self.config.fault_window.end)
            .map(|_| self.next_nemesis_ns);
        match (scripted, nemesis) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// Earliest timestamp of any upcoming fault whose step prerequisite is
    /// already satisfied (or has none), used when all nodes are idle so
    /// virtual time advances to wake for interactive faults like `restart`.
    pub fn next_any_fault_ns(&self, cluster_step: u64) -> Option<u64> {
        let scripted = self.config.faults.get(self.next_fault_idx).and_then(|f| {
            if f.at_step.is_none_or(|s| cluster_step >= s) {
                Some(f.at_ns)
            } else {
                None
            }
        });
        let nemesis = self
            .config
            .nemesis
            .as_ref()
            .filter(|_| self.next_nemesis_ns < self.config.fault_window.end)
            .map(|_| self.next_nemesis_ns);
        match (scripted, nemesis) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// Fires all scripted, interactive, and nemesis faults due at or before
    /// `(event_vt, cluster_ns, cluster_step)`.
    pub fn fire_due_faults(
        &mut self,
        event_vt: u64,
        cluster_ns: u64,
        cluster_step: u64,
        events: &mut Vec<NetEvent>,
    ) -> Vec<FaultAction> {
        let mut actions = Vec::new();

        // 1. Scripted and recorded interactive faults
        while let Some(fault) = self.config.faults.get(self.next_fault_idx).cloned() {
            let due = match fault.at_step {
                Some(step) => cluster_ns >= fault.at_ns && cluster_step >= step,
                None => fault.at_ns <= event_vt,
            };
            if !due {
                break;
            }
            self.next_fault_idx += 1;
            self.apply_fault(fault.at_ns, &fault.kind, events, &mut actions);
        }

        // 2. Schedule-driven nemesis ticks
        if let Some(nem) = self.config.nemesis.clone() {
            let tick = nem.tick_ns.max(1);
            while self.next_nemesis_ns <= event_vt {
                let t = self.next_nemesis_ns;
                self.next_nemesis_ns = self.next_nemesis_ns.saturating_add(tick);
                if !self.config.fault_window.contains(&t) {
                    continue;
                }
                self.run_nemesis_tick(t, &nem, events, &mut actions);
            }
        }

        actions
    }

    /// Immediately applies an interactive fault at virtual time `at_ns`.
    pub fn apply_immediate_fault(
        &mut self,
        at_ns: u64,
        kind: &FaultKind,
        events: &mut Vec<NetEvent>,
    ) -> Vec<FaultAction> {
        let mut actions = Vec::new();
        self.apply_fault(at_ns, kind, events, &mut actions);
        actions
    }

    fn apply_fault(
        &mut self,
        at_ns: u64,
        kind: &FaultKind,
        events: &mut Vec<NetEvent>,
        actions: &mut Vec<FaultAction>,
    ) {
        match kind {
            FaultKind::Partition { groups } => {
                self.blocked.clear();
                for (gi, g1) in groups.iter().enumerate() {
                    for (gj, g2) in groups.iter().enumerate() {
                        if gi == gj {
                            continue;
                        }
                        for &a in g1 {
                            for &b in g2 {
                                self.blocked.insert((a, b));
                            }
                        }
                    }
                }
                self.stats.partitions += 1;
                events.push(NetEvent::Partition {
                    ns: at_ns,
                    blocked_pairs: self.blocked.len(),
                    asymmetric: false,
                });
            }
            FaultKind::AsymmetricPartition { blocked } => {
                self.blocked.clear();
                for &(a, b) in blocked {
                    if a != b && a < self.config.node_count && b < self.config.node_count {
                        self.blocked.insert((a, b));
                    }
                }
                self.stats.partitions += 1;
                events.push(NetEvent::Partition {
                    ns: at_ns,
                    blocked_pairs: self.blocked.len(),
                    asymmetric: true,
                });
            }
            FaultKind::Heal => {
                if !self.blocked.is_empty() {
                    self.blocked.clear();
                    events.push(NetEvent::Heal { ns: at_ns });
                }
            }
            FaultKind::HealLinks { links } => {
                let mut changed = false;
                for &(a, b) in links {
                    if self.blocked.remove(&(a, b)) {
                        changed = true;
                    }
                }
                if changed {
                    events.push(NetEvent::Heal { ns: at_ns });
                }
            }
            FaultKind::SetLink {
                from,
                to,
                latency_ns,
                jitter_ns,
                drop_ppm,
                duplicate_ppm,
            } => {
                if *from < self.config.node_count && *to < self.config.node_count && from != to {
                    let entry = self
                        .links
                        .entry((*from, *to))
                        .or_insert_with(|| LinkOverride {
                            from: *from,
                            to: *to,
                            latency_ns: None,
                            jitter_ns: None,
                            drop_ppm: None,
                            duplicate_ppm: None,
                        });
                    if let Some(l) = latency_ns {
                        entry.latency_ns = Some((*l).max(MIN_LINK_LATENCY_NS));
                    }
                    if let Some(j) = jitter_ns {
                        entry.jitter_ns = Some(*j);
                    }
                    if let Some(d) = drop_ppm {
                        entry.drop_ppm = Some((*d).min(PPM_SCALE));
                    }
                    if let Some(dup) = duplicate_ppm {
                        entry.duplicate_ppm = Some((*dup).min(PPM_SCALE));
                    }
                    events.push(NetEvent::LinkUpdated {
                        ns: at_ns,
                        from: *from,
                        to: *to,
                        latency_ns: entry.latency_ns.unwrap_or(self.config.latency_ns),
                        jitter_ns: entry.jitter_ns.unwrap_or(self.config.jitter_ns),
                        drop_ppm: entry.drop_ppm.unwrap_or(self.config.drop_ppm),
                        duplicate_ppm: entry.duplicate_ppm.unwrap_or(self.config.duplicate_ppm),
                    });
                }
            }
            FaultKind::Crash { node } => {
                if *node < self.config.node_count && self.crashed.insert(*node) {
                    self.stats.crashes += 1;
                    events.push(NetEvent::NodeCrash {
                        ns: at_ns,
                        node: *node,
                    });
                    actions.push(FaultAction::Crash(*node));
                }
            }
            FaultKind::Restart { node } => {
                if *node < self.config.node_count && self.crashed.remove(node) {
                    self.stats.restarts += 1;
                    actions.push(FaultAction::Restart(*node));
                }
            }
        }
    }

    fn run_nemesis_tick(
        &mut self,
        at_ns: u64,
        nem: &NemesisConfig,
        events: &mut Vec<NetEvent>,
        actions: &mut Vec<FaultAction>,
    ) {
        // Partition or heal
        if self.blocked.is_empty() {
            if nem.partition_prob_ppm > 0 && self.config.node_count >= 2 {
                let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
                if draw < nem.partition_prob_ppm {
                    // Isolate one random node from the rest (classic minority/majority split).
                    let victim =
                        (splitmix64(&mut self.prng_state) as usize) % self.config.node_count;
                    let others: Vec<usize> = (0..self.config.node_count)
                        .filter(|&i| i != victim)
                        .collect();
                    self.apply_fault(
                        at_ns,
                        &FaultKind::Partition {
                            groups: vec![vec![victim], others],
                        },
                        events,
                        actions,
                    );
                }
            }
        } else if nem.heal_prob_ppm > 0 {
            let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
            if draw < nem.heal_prob_ppm {
                self.apply_fault(at_ns, &FaultKind::Heal, events, actions);
            }
        }

        // Crash or restart eligible nodes
        if !nem.eligible_nodes.is_empty() {
            let live_eligible: Vec<usize> = nem
                .eligible_nodes
                .iter()
                .copied()
                .filter(|n| !self.crashed.contains(n))
                .collect();
            let crashed_eligible: Vec<usize> = nem
                .eligible_nodes
                .iter()
                .copied()
                .filter(|n| self.crashed.contains(n))
                .collect();

            if !crashed_eligible.is_empty() && nem.restart_prob_ppm > 0 {
                let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
                if draw < nem.restart_prob_ppm {
                    let pick = crashed_eligible
                        [(splitmix64(&mut self.prng_state) as usize) % crashed_eligible.len()];
                    self.apply_fault(at_ns, &FaultKind::Restart { node: pick }, events, actions);
                    return;
                }
            }

            if self.crashed.len() < nem.max_crashed
                && !live_eligible.is_empty()
                && nem.crash_prob_ppm > 0
            {
                let draw = (splitmix64(&mut self.prng_state) % u64::from(PPM_SCALE)) as u32;
                if draw < nem.crash_prob_ppm {
                    let pick = live_eligible
                        [(splitmix64(&mut self.prng_state) as usize) % live_eligible.len()];
                    self.apply_fault(at_ns, &FaultKind::Crash { node: pick }, events, actions);
                }
            }
        }
    }
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Short human-readable summary of an Ethernet frame (ARP, IPv4/TCP/UDP/ICMP, IPv6).
fn summarize_frame(frame: &[u8]) -> String {
    if frame.len() < 14 {
        return "short".into();
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        0x0806 => {
            if frame.len() >= 42 {
                let op = u16::from_be_bytes([frame[20], frame[21]]);
                let op_name = match op {
                    1 => "who-has",
                    2 => "is-at",
                    _ => "op",
                };
                format!(
                    "ARP {op_name} {}.{}.{}.{} -> {}.{}.{}.{}",
                    frame[28], frame[29], frame[30], frame[31],
                    frame[38], frame[39], frame[40], frame[41],
                )
            } else {
                "ARP".into()
            }
        }
        0x0800 if frame.len() >= 34 => {
            let ihl = usize::from(frame[14] & 0x0f) * 4;
            let proto = frame[23];
            let src = format!("{}.{}.{}.{}", frame[26], frame[27], frame[28], frame[29]);
            let dst = format!("{}.{}.{}.{}", frame[30], frame[31], frame[32], frame[33]);
            let l4 = 14 + ihl;
            match proto {
                1 => format!("ICMP {src} -> {dst}"),
                6 if frame.len() >= l4 + 14 => {
                    let sp = u16::from_be_bytes([frame[l4], frame[l4 + 1]]);
                    let dp = u16::from_be_bytes([frame[l4 + 2], frame[l4 + 3]]);
                    let flags = frame[l4 + 13];
                    let mut fl = String::new();
                    if flags & 0x02 != 0 { fl.push('S'); }
                    if flags & 0x10 != 0 { fl.push('A'); }
                    if flags & 0x08 != 0 { fl.push('P'); }
                    if flags & 0x01 != 0 { fl.push('F'); }
                    if flags & 0x04 != 0 { fl.push('R'); }
                    format!("TCP {src}:{sp} -> {dst}:{dp} [{fl}]")
                }
                17 if frame.len() >= l4 + 8 => {
                    let sp = u16::from_be_bytes([frame[l4], frame[l4 + 1]]);
                    let dp = u16::from_be_bytes([frame[l4 + 2], frame[l4 + 3]]);
                    format!("UDP {src}:{sp} -> {dst}:{dp}")
                }
                _ => format!("IPv4 proto={proto} {src} -> {dst}"),
            }
        }
        0x86dd => "IPv6".into(),
        other => format!("ethertype {other:#06x}"),
    }
}

/// Deterministic 64-bit hash of a raw Ethernet frame (FNV-1a mixed with
/// SplitMix64 finalizer) so `network.bin` catches any single-bit wire
/// divergence on replay without storing full payloads on disk.
pub fn hash_frame(frame: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in frame {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= (frame.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Magic header for binary `network.bin` files, matching `rwtrace\0` / `rwckf01\0`.
pub const NET_LOG_MAGIC: &[u8; 8] = b"rwnet01\0";
/// Binary format version of `network.bin`.
pub const NET_LOG_VERSION: u32 = 1;
const NET_LOG_HEADER_LEN: usize = NET_LOG_MAGIC.len() + 4;
const NET_RECORD_MAX: usize = 64 * 1024;

const TAG_TX: u8 = 1;
const TAG_DELIVER: u8 = 2;
const TAG_DROP: u8 = 3;
const TAG_DUPLICATE: u8 = 4;
const TAG_PARTITION: u8 = 5;
const TAG_HEAL: u8 = 6;
const TAG_NODE_CRASH: u8 = 7;
const TAG_NODE_RESTART: u8 = 8;
const TAG_LINK_UPDATED: u8 = 9;

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn get_varint(slice: &mut &[u8]) -> std::io::Result<u64> {
    let mut out = 0u64;
    let mut shift = 0u32;
    loop {
        let Some((&b, rest)) = slice.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated varint in network.bin",
            ));
        };
        *slice = rest;
        if shift >= 64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "varint overflow in network.bin",
            ));
        }
        out |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(out);
        }
        shift += 7;
    }
}

fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for slot in &mut out {
        let hex = parts.next()?;
        *slot = u8::from_str_radix(hex, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

fn get_bytes<'a>(slice: &mut &'a [u8], n: usize) -> std::io::Result<&'a [u8]> {
    if slice.len() < n {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated field in network.bin",
        ));
    }
    let (head, tail) = slice.split_at(n);
    *slice = tail;
    Ok(head)
}

fn get_u8(slice: &mut &[u8]) -> std::io::Result<u8> {
    Ok(get_bytes(slice, 1)?[0])
}

fn get_string(slice: &mut &[u8]) -> std::io::Result<String> {
    let len = get_varint(slice)? as usize;
    let bytes = get_bytes(slice, len)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn encode_event_body(ev: &NetEvent, prev_ns: &mut u64, out: &mut Vec<u8>) {
    out.clear();
    let ns = match ev {
        NetEvent::Tx { ns, .. }
        | NetEvent::Deliver { ns, .. }
        | NetEvent::Drop { ns, .. }
        | NetEvent::Duplicate { ns, .. }
        | NetEvent::Partition { ns, .. }
        | NetEvent::Heal { ns }
        | NetEvent::NodeCrash { ns, .. }
        | NetEvent::NodeRestart { ns, .. }
        | NetEvent::LinkUpdated { ns, .. } => *ns,
    };
    let delta = (ns as i64).wrapping_sub(*prev_ns as i64);
    *prev_ns = ns;

    match ev {
        NetEvent::Tx {
            src,
            dst_mac,
            len,
            frame_hash,
            summary,
            ..
        } => {
            out.push(TAG_TX);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *src as u64);
            let mac = parse_mac(dst_mac).unwrap_or([0; 6]);
            out.extend_from_slice(&mac);
            put_varint(out, *len as u64);
            out.extend_from_slice(&frame_hash.to_le_bytes());
            put_varint(out, summary.len() as u64);
            out.extend_from_slice(summary.as_bytes());
        }
        NetEvent::Deliver {
            src,
            dst,
            len,
            latency_ns,
            ..
        } => {
            out.push(TAG_DELIVER);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *src as u64);
            put_varint(out, *dst as u64);
            put_varint(out, *len as u64);
            put_varint(out, *latency_ns);
        }
        NetEvent::Drop {
            src,
            dst,
            len,
            reason,
            ..
        } => {
            out.push(TAG_DROP);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *src as u64);
            put_varint(out, *dst as u64);
            put_varint(out, *len as u64);
            out.push(match reason {
                DropReason::Partitioned => 0,
                DropReason::RandomDrop => 1,
                DropReason::NodeCrashed => 2,
                DropReason::RxOverflow => 3,
            });
        }
        NetEvent::Duplicate {
            src,
            dst,
            len,
            extra_delay_ns,
            ..
        } => {
            out.push(TAG_DUPLICATE);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *src as u64);
            put_varint(out, *dst as u64);
            put_varint(out, *len as u64);
            put_varint(out, *extra_delay_ns);
        }
        NetEvent::Partition {
            blocked_pairs,
            asymmetric,
            ..
        } => {
            out.push(TAG_PARTITION);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *blocked_pairs as u64);
            out.push(u8::from(*asymmetric));
        }
        NetEvent::Heal { .. } => {
            out.push(TAG_HEAL);
            put_varint(out, zigzag_encode(delta));
        }
        NetEvent::NodeCrash { node, .. } => {
            out.push(TAG_NODE_CRASH);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *node as u64);
        }
        NetEvent::NodeRestart {
            node, boot_count, ..
        } => {
            out.push(TAG_NODE_RESTART);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *node as u64);
            put_varint(out, u64::from(*boot_count));
        }
        NetEvent::LinkUpdated {
            from,
            to,
            latency_ns,
            jitter_ns,
            drop_ppm,
            duplicate_ppm,
            ..
        } => {
            out.push(TAG_LINK_UPDATED);
            put_varint(out, zigzag_encode(delta));
            put_varint(out, *from as u64);
            put_varint(out, *to as u64);
            put_varint(out, *latency_ns);
            put_varint(out, *jitter_ns);
            put_varint(out, u64::from(*drop_ppm));
            put_varint(out, u64::from(*duplicate_ppm));
        }
    }
}

fn decode_event_body(mut s: &[u8], prev_ns: &mut u64) -> std::io::Result<NetEvent> {
    let tag = get_u8(&mut s)?;
    let delta = zigzag_decode(get_varint(&mut s)?);
    let ns = (*prev_ns as i64).wrapping_add(delta) as u64;
    *prev_ns = ns;

    let ev = match tag {
        TAG_TX => {
            let src = get_varint(&mut s)? as usize;
            let mac_b = get_bytes(&mut s, 6)?;
            let mac = [mac_b[0], mac_b[1], mac_b[2], mac_b[3], mac_b[4], mac_b[5]];
            let len = get_varint(&mut s)? as usize;
            let hash_b = get_bytes(&mut s, 8)?;
            let frame_hash = u64::from_le_bytes(hash_b.try_into().expect("8 bytes"));
            let summary = get_string(&mut s)?;
            NetEvent::Tx {
                ns,
                src,
                dst_mac: format_mac(mac),
                len,
                frame_hash,
                summary,
            }
        }
        TAG_DELIVER => NetEvent::Deliver {
            ns,
            src: get_varint(&mut s)? as usize,
            dst: get_varint(&mut s)? as usize,
            len: get_varint(&mut s)? as usize,
            latency_ns: get_varint(&mut s)?,
        },
        TAG_DROP => {
            let src = get_varint(&mut s)? as usize;
            let dst = get_varint(&mut s)? as usize;
            let len = get_varint(&mut s)? as usize;
            let reason = match get_u8(&mut s)? {
                0 => DropReason::Partitioned,
                1 => DropReason::RandomDrop,
                2 => DropReason::NodeCrashed,
                3 => DropReason::RxOverflow,
                other => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown drop reason {other} in network.bin"),
                    ));
                }
            };
            NetEvent::Drop {
                ns,
                src,
                dst,
                len,
                reason,
            }
        }
        TAG_DUPLICATE => NetEvent::Duplicate {
            ns,
            src: get_varint(&mut s)? as usize,
            dst: get_varint(&mut s)? as usize,
            len: get_varint(&mut s)? as usize,
            extra_delay_ns: get_varint(&mut s)?,
        },
        TAG_PARTITION => NetEvent::Partition {
            ns,
            blocked_pairs: get_varint(&mut s)? as usize,
            asymmetric: get_u8(&mut s)? != 0,
        },
        TAG_HEAL => NetEvent::Heal { ns },
        TAG_NODE_CRASH => NetEvent::NodeCrash {
            ns,
            node: get_varint(&mut s)? as usize,
        },
        TAG_NODE_RESTART => NetEvent::NodeRestart {
            ns,
            node: get_varint(&mut s)? as usize,
            boot_count: get_varint(&mut s)? as u32,
        },
        TAG_LINK_UPDATED => NetEvent::LinkUpdated {
            ns,
            from: get_varint(&mut s)? as usize,
            to: get_varint(&mut s)? as usize,
            latency_ns: get_varint(&mut s)?,
            jitter_ns: get_varint(&mut s)?,
            drop_ppm: get_varint(&mut s)? as u32,
            duplicate_ppm: get_varint(&mut s)? as u32,
        },
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown event tag {other} in network.bin"),
            ));
        }
    };
    if !s.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} trailing bytes in network.bin record", s.len()),
        ));
    }
    Ok(ev)
}

/// Streaming binary writer for `clusters/<id>/network.bin`, matching
/// `rewind_trace::TraceWriter` (versioned magic header + length-prefixed
/// binary records, cut-off-safe if killed mid-run).
#[derive(Debug)]
pub struct NetLogWriter<W: Write> {
    out: std::io::BufWriter<W>,
    headed: bool,
    prev_ns: u64,
    scratch: Vec<u8>,
}

impl<W: Write> NetLogWriter<W> {
    pub fn new(out: W) -> Self {
        NetLogWriter {
            out: std::io::BufWriter::new(out),
            headed: false,
            prev_ns: 0,
            scratch: Vec::with_capacity(128),
        }
    }

    fn head(&mut self) -> std::io::Result<()> {
        if self.headed {
            return Ok(());
        }
        self.out.write_all(NET_LOG_MAGIC)?;
        self.out.write_all(&NET_LOG_VERSION.to_le_bytes())?;
        self.headed = true;
        Ok(())
    }

    pub fn record(&mut self, event: &NetEvent) -> std::io::Result<()> {
        self.head()?;
        encode_event_body(event, &mut self.prev_ns, &mut self.scratch);
        let len = self.scratch.len() as u32;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(&self.scratch)
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        self.head()?;
        self.out.flush()?;
        self.out.into_inner().map_err(|e| e.into_error())
    }
}

/// Streaming binary reader for `clusters/<id>/network.bin`, cut-off-safe on
/// `UnexpectedEof` just like `rewind_trace::Records`.
#[derive(Debug)]
pub struct NetLogReader<R: std::io::Read> {
    reader: R,
    prev_ns: u64,
    ended: bool,
}

impl<R: std::io::Read> NetLogReader<R> {
    pub fn new(mut reader: R) -> std::io::Result<Self> {
        let mut header = [0u8; NET_LOG_HEADER_LEN];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(NetLogReader {
                    reader,
                    prev_ns: 0,
                    ended: true,
                });
            }
            Err(e) => return Err(e),
        }
        let (magic, ver_b) = header.split_at(NET_LOG_MAGIC.len());
        if magic != NET_LOG_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a valid Rewind network.bin log",
            ));
        }
        let version = u32::from_le_bytes(ver_b.try_into().expect("four bytes"));
        if version != NET_LOG_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "network.bin version {version} is not supported (expected {NET_LOG_VERSION})"
                ),
            ));
        }
        Ok(NetLogReader {
            reader,
            prev_ns: 0,
            ended: false,
        })
    }

    fn read_one(&mut self) -> std::io::Result<Option<NetEvent>> {
        let mut len_b = [0u8; 4];
        match self.reader.read_exact(&mut len_b) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(len_b) as usize;
        if len == 0 || len > NET_RECORD_MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid network.bin record length {len}"),
            ));
        }
        let mut buf = vec![0u8; len];
        match self.reader.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(decode_event_body(&buf, &mut self.prev_ns)?))
    }
}

impl<R: std::io::Read> Iterator for NetLogReader<R> {
    type Item = std::io::Result<NetEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        let res = self.read_one();
        self.ended = !matches!(res, Ok(Some(_)));
        res.transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth_frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(14 + payload.len());
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&0x0800u16.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn deterministic_routing_partitions_and_snapshot_roundtrip() {
        let cfg = NetSwitchConfig {
            node_count: 3,
            latency_ns: 1_000_000,
            jitter_ns: 500_000,
            drop_ppm: 100_000,
            duplicate_ppm: 100_000,
            seed: 42,
            fault_window: 0..u64::MAX,
            links: Vec::new(),
            faults: vec![
                ScheduledFault {
                    at_ns: 5_000_000,
                    at_step: None,
                    kind: FaultKind::Partition {
                        groups: vec![vec![0], vec![1, 2]],
                    },
                },
                ScheduledFault {
                    at_ns: 10_000_000,
                    at_step: None,
                    kind: FaultKind::Heal,
                },
            ],
            nemesis: None,
        };

        let mut a = NetSwitch::new(cfg.clone());
        let mut b = NetSwitch::new(cfg.clone());
        let mut ev_a = Vec::new();
        let mut ev_b = Vec::new();

        for i in 0..20u64 {
            let t = i * 600_000;
            a.fire_due_faults(t, t, i, &mut ev_a);
            b.fire_due_faults(t, t, i, &mut ev_b);
            let f = eth_frame(mac_for_node(1), mac_for_node(0), &[i as u8; 32]);
            a.on_tx(t, 0, f.clone(), &mut ev_a);
            b.on_tx(t, 0, f, &mut ev_b);
            if i == 10 {
                let snap = a.snapshot();
                a = NetSwitch::restore(cfg.clone(), snap);
            }
        }
        assert_eq!(ev_a, ev_b);
        assert_eq!(a.pop_due_frames(u64::MAX), b.pop_due_frames(u64::MAX));
    }

    #[test]
    fn binary_net_log_roundtrip_and_cutoff_safety() {
        let events = vec![
            NetEvent::Tx {
                ns: 1_000_000,
                src: 0,
                dst_mac: "02:52:57:00:00:02".into(),
                len: 74,
                frame_hash: hash_frame(b"hello-world-frame"),
                summary: "TCP 10.0.0.1:7000 -> 10.0.0.2:41234 [S]".into(),
            },
            NetEvent::Deliver {
                ns: 3_120_000,
                src: 0,
                dst: 1,
                len: 74,
                latency_ns: 2_120_000,
            },
            NetEvent::Drop {
                ns: 3_000_000, // intentionally out-of-order to test signed zigzag delta
                src: 1,
                dst: 2,
                len: 120,
                reason: DropReason::Partitioned,
            },
            NetEvent::Duplicate {
                ns: 4_000_000,
                src: 0,
                dst: 1,
                len: 64,
                extra_delay_ns: 500_000,
            },
            NetEvent::Partition {
                ns: 5_000_000,
                blocked_pairs: 2,
                asymmetric: true,
            },
            NetEvent::Heal { ns: 6_000_000 },
            NetEvent::NodeCrash {
                ns: 7_000_000,
                node: 2,
            },
            NetEvent::NodeRestart {
                ns: 8_000_000,
                node: 2,
                boot_count: 2,
            },
            NetEvent::LinkUpdated {
                ns: 9_000_000,
                from: 0,
                to: 1,
                latency_ns: 5_000_000,
                jitter_ns: 250_000,
                drop_ppm: 10_000,
                duplicate_ppm: 1_000,
            },
        ];

        let mut writer = NetLogWriter::new(Vec::new());
        for ev in &events {
            writer.record(ev).unwrap();
        }
        let bytes = writer.finish().unwrap();
        assert_eq!(&bytes[..NET_LOG_MAGIC.len()], NET_LOG_MAGIC);

        let decoded: Vec<NetEvent> = NetLogReader::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .collect::<std::io::Result<_>>()
            .unwrap();
        assert_eq!(decoded, events);

        // Truncating mid-record yields all preceding complete records cleanly.
        let truncated = &bytes[..bytes.len() - 5];
        let partial: Vec<NetEvent> = NetLogReader::new(std::io::Cursor::new(truncated))
            .unwrap()
            .collect::<std::io::Result<_>>()
            .unwrap();
        assert_eq!(partial, events[..events.len() - 1]);

        // Bad magic is refused.
        assert!(NetLogReader::new(std::io::Cursor::new(b"notanetlog12")).is_err());
    }
}
