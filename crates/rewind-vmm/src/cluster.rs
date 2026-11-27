//! Multi-VM conservative parallel discrete-event cluster coordinator.
//!
//! Runs `N` KVM [`Machine`] instances connected through a deterministic
//! [`NetSwitch`] in virtual-time order. Causality is preserved by computing
//! each node's Chandy–Misra–Bryant safe lookahead horizon from link propagation
//! delays, idle timers (null messages), in-flight packets, and scheduled
//! faults, then executing all eligible runnable nodes concurrently across
//! dedicated per-node host threads.
//!
//! Because every round's runnable set, horizons, PRNG draws, and commit order
//! are pure functions of deterministic cluster state (independent of host
//! thread timing), parallel multi-threaded execution (`parallel = true`) and
//! single-threaded sequential execution (`parallel = false`) produce bit-for-bit
//! identical per-node traces, network logs, and keyframes.
//!
//! Supports:
//! - Independent per-node clocks with path-independent per-boot anchor skew and
//!   drift (ppm),
//! - Partitioned-node decoupling (blocked links do not constrain lookahead
//!   until healed),
//! - Hard node crashes (dropping the VM) and cold restarts (booting a fresh VM
//!   with a deterministic seed derived from `(base_seed, boot_count)`),
//! - Cluster keyframes capturing all nodes + the virtual switch state for
//!   causally-exact seeking, node inspection, and cluster forking.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "macos"))]
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::net::{
    DropReason, FaultAction, FaultKind, InFlightFrame, NetEvent, NetStats, NetSwitch,
    NetSwitchConfig, NetSwitchState,
};
use crate::snapshot::{Keyframe, Pages, PreparedKeyframe, finish_keyframe};
use crate::{Config, DirtyLog, Interrupter, Machine, Observer, Outcome, Stop};

/// Default virtual-time execution slice hint (50 µs). Connected nodes run up
/// to their full link-latency lookahead horizon rather than breaking every
/// `slice_ns`; `slice_ns` acts as a lower bound on the unconstrained window.
pub const DEFAULT_SLICE_NS: u64 = 50_000;

/// Minimum virtual-time lookahead window (5 ms) above global virtual time for
/// nodes whose peers are partitioned, idle, or exited. Keeps partitioned nodes
/// decoupled and parallel while bounding skew at interactive faults and
/// keeping `wait_for` / `advance` responsive.
const UNCONSTRAINED_WINDOW_NS: u64 = 5_000_000;

/// Maximum VM exits a node executes in a single round slice. Large enough that
/// multi-millisecond lookahead windows complete in one round under both
/// `--clock exits` and `--clock branches` without cutting slices artificially
/// short, while still bounding pathological zero-quantum loops.
const MAX_STEPS_PER_SLICE: u64 = 8_192;

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
    /// Minimum unconstrained window hint (ns).
    pub slice_ns: u64,
    /// Execute eligible nodes concurrently across dedicated per-node host
    /// threads (`true`), or sequentially on the calling thread (`false`).
    /// Both modes run the exact same deterministic lookahead schedule and
    /// produce bit-for-bit identical traces and events.
    pub parallel: bool,
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
    /// Raw Ethernet frame transmitted by `src` at cluster virtual time `ns`
    /// (used for on-demand `.pcap` export during replay).
    fn net_tx_frame(&mut self, _ns: u64, _src: usize, _frame: &[u8]) {}
    /// Return true to pause the cluster loop at the end of the current round.
    fn stop(&self) -> bool {
        false
    }
    /// Return true to interrupt an individual node's slice mid-way (used when
    /// running sequentially with an ad-hoc stop condition).
    fn stop_mid_slice(&self) -> bool {
        false
    }
    /// Optional exact guest step at which `node` must pause mid-slice (used by
    /// `rewind cluster seek` / `cat` / `files` / `gdb` in parallel mode).
    fn node_step_limit(&self, _node: usize) -> Option<u64> {
        None
    }
}

/// Serializable state of one node inside a [`ClusterKeyframe`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterNodeKeyframe {
    pub keyframe: Option<Keyframe>,
    pub idle: bool,
    pub net_rx_pending: bool,
    pub cluster_ns: u64,
    pub boot_cluster_ns: u64,
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

/// In-memory trace encoders for one cluster node (`None` when crashed).
pub type NodeEncoders = Option<(
    Option<rewind_trace::io_records::Encoder>,
    Option<rewind_trace::switches::Encoder>,
)>;

/// Cached metadata mirrored from a node's [`Machine`] so coordinator queries
/// (`is_runnable`, `local_now`, `next_timer_cluster_ns`, `node_steps`, etc.)
/// require zero cross-thread or cross-process round-trips.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeMeta {
    step: u64,
    now: u64,
    is_idle: bool,
    net_rx_pending: bool,
    timer_deadline: Option<u64>,
    stopped: Option<Stop>,
}

impl NodeMeta {
    fn from_machine(m: &Machine) -> NodeMeta {
        NodeMeta {
            step: m.step(),
            now: m.now(),
            is_idle: m.is_idle(),
            net_rx_pending: m.net_rx_pending(),
            timer_deadline: m.timer_deadline(),
            stopped: m.stopped(),
        }
    }
}

/// Serializable subset of [`Outcome`] returned by a cluster slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SliceOutcome {
    Paused,
    Idle,
    Interrupted,
    Stopped(Stop),
}

impl SliceOutcome {
    fn from_outcome(outcome: Outcome) -> Result<SliceOutcome> {
        match outcome {
            Outcome::Paused | Outcome::TxReady => Ok(SliceOutcome::Paused),
            Outcome::Idle => Ok(SliceOutcome::Idle),
            Outcome::Interrupted => Ok(SliceOutcome::Interrupted),
            Outcome::Stopped(stop) => Ok(SliceOutcome::Stopped(stop)),
            Outcome::Debug(d) => bail!("unexpected debug stop during cluster slice: {d:?}"),
        }
    }
}

/// Buffered trace records and serial bytes produced by one node during a slice.
#[derive(Default, Serialize, Deserialize)]
struct SliceEvents {
    records: Vec<(u64, Vec<u8>)>,
    serial: Vec<(u64, u8)>,
}

struct BufferingObserver {
    step_base: u64,
    events: SliceEvents,
}

impl Observer for BufferingObserver {
    fn record(&mut self, step: u64, record: &[u8]) {
        self.events
            .records
            .push((self.step_base + step, record.to_vec()));
    }

    fn serial(&mut self, step: u64, byte: u8) {
        self.events.serial.push((self.step_base + step, byte));
    }
}

#[derive(Serialize, Deserialize)]
struct SliceReply {
    outcome: SliceOutcome,
    meta: NodeMeta,
    events: SliceEvents,
    tx_frames: Vec<(u64, Vec<u8>)>,
}

type EncodersPair = (
    Option<rewind_trace::io_records::Encoder>,
    Option<rewind_trace::switches::Encoder>,
);

/// Summary of how [`Cluster::restore_in_place`] restored each node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreStats {
    /// Nodes rolled back in place by rewriting only pages dirtied since the
    /// snapshot (zero VM teardown / child respawn).
    pub in_place_nodes: usize,
    /// Nodes restored by rebuilding a fresh [`Machine`] inside the existing
    /// worker (e.g. nodes that were crashed or on a different `boot_count`).
    pub full_restore_nodes: usize,
    /// Nodes that are crashed in the target snapshot.
    pub crashed_nodes: usize,
    /// Total 4 KiB guest pages rewritten across in-place restored nodes.
    pub pages_rewritten: usize,
}

/// How a cluster worker obtains snapshot pages during `Restore` /
/// `RestoreInPlace`: directly from a `rewind-store` directory on disk (zero
/// page-byte IPC) or inline via [`MemPages`] for in-memory test stores.
#[derive(Debug, Serialize, Deserialize)]
enum PageSource {
    StorePath(PathBuf),
    Inline(MemPages),
}

struct StoreRefPages<'a>(&'a rewind_store::Store);

impl Pages for StoreRefPages<'_> {
    fn put_all(&mut self, _pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        bail!("read-only worker store does not store pages")
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
    fn store_path(&self) -> Option<&Path> {
        Some(self.0.path())
    }
}

struct StoreMutPages<'a>(&'a mut rewind_store::Store);

impl Pages for StoreMutPages<'_> {
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        self.0.put_all(pages)
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
    fn store_path(&self) -> Option<&Path> {
        Some(self.0.path())
    }
}

#[derive(Default)]
struct WorkerStoreCache(Option<(PathBuf, rewind_store::Store)>);

impl WorkerStoreCache {
    fn open(&mut self, path: &Path) -> Result<&mut rewind_store::Store> {
        let needs_open = self
            .0
            .as_ref()
            .is_none_or(|(cached, _)| cached.as_path() != path);
        if needs_open {
            self.0 = None;
            let store = rewind_store::Store::open(path)?;
            self.0 = Some((path.to_path_buf(), store));
        }
        Ok(&mut self.0.as_mut().expect("opened above").1)
    }
}

fn extract_node_chain(chain: &[ClusterKeyframe], idx: usize, boot_count: u32) -> Vec<Keyframe> {
    let mut node_chain = Vec::new();
    for ck in chain {
        let nk = &ck.nodes[idx];
        if nk.boot_count == boot_count
            && let Some(kf) = &nk.keyframe
        {
            if kf.parent.is_none() {
                node_chain.clear();
            }
            node_chain.push(kf.clone());
        }
    }
    node_chain
}

fn build_page_source(chain: &[Keyframe], pages: &(dyn Pages + Sync)) -> Result<PageSource> {
    if let Some(path) = pages.store_path() {
        return Ok(PageSource::StorePath(path.to_path_buf()));
    }
    let mut page_hashes: std::collections::BTreeMap<u32, [u8; 32]> =
        std::collections::BTreeMap::new();
    for kf in chain {
        for &(page, hash) in &kf.pages {
            if hash == crate::snapshot::ZERO_PAGE {
                page_hashes.remove(&page);
            } else {
                page_hashes.insert(page, hash);
            }
        }
    }
    let mut mem_pages = MemPages::default();
    let mut buf = vec![0u8; crate::layout::PAGE_SIZE];
    for hash in page_hashes.into_values() {
        if let std::collections::hash_map::Entry::Vacant(e) = mem_pages.pages.entry(hash) {
            pages.get(&hash, &mut buf)?;
            e.insert(buf.clone());
        }
    }
    Ok(PageSource::Inline(mem_pages))
}

#[allow(clippy::too_many_arguments, reason = "worker restore helper")]
fn worker_restore_full(
    machine: &mut Option<Machine>,
    shared_intr: &Arc<Mutex<Option<Interrupter>>>,
    store_cache: &mut WorkerStoreCache,
    config: &Config,
    chain: &[Keyframe],
    pages: PageSource,
    trace: &[(u64, Vec<u8>)],
    dirty_log: DirtyLog,
    idle: bool,
    net_rx_pending: bool,
) -> Result<NodeMeta> {
    if let Ok(mut g) = shared_intr.lock() {
        *g = None;
    }
    drop(machine.take());
    let mut m = match pages {
        PageSource::StorePath(path) => {
            let store = store_cache.open(&path)?;
            let rp = StoreRefPages(store);
            Machine::restore(config, chain, &rp, trace, dirty_log)?
        }
        PageSource::Inline(mem_pages) => {
            Machine::restore(config, chain, &mem_pages, trace, dirty_log)?
        }
    };
    m.set_cluster_mode(true);
    m.set_idle(idle, net_rx_pending);
    let meta = NodeMeta::from_machine(&m);
    if let Ok(mut g) = shared_intr.lock() {
        *g = Some(m.interrupter());
    }
    *machine = Some(m);
    Ok(meta)
}

#[allow(clippy::too_many_arguments, reason = "worker in-place restore helper")]
fn worker_restore_in_place(
    machine: &mut Option<Machine>,
    shared_intr: &Arc<Mutex<Option<Interrupter>>>,
    store_cache: &mut WorkerStoreCache,
    config: &Config,
    chain: &[Keyframe],
    pages: PageSource,
    trace: &[(u64, Vec<u8>)],
    dirty_log: DirtyLog,
    idle: bool,
    net_rx_pending: bool,
) -> Result<(NodeMeta, bool, usize)> {
    if dirty_log == DirtyLog::On
        && machine.as_ref().is_some_and(Machine::can_restore_in_place)
    {
        let m = machine.as_mut().expect("checked live above");
        let chain_refs: Vec<&Keyframe> = chain.iter().collect();
        let rewritten = match pages {
            PageSource::StorePath(path) => {
                let store = store_cache.open(&path)?;
                let rp = StoreRefPages(store);
                m.restore_in_place_count(&chain_refs, &rp, trace)?
            }
            PageSource::Inline(mem_pages) => {
                m.restore_in_place_count(&chain_refs, &mem_pages, trace)?
            }
        };
        m.set_cluster_mode(true);
        m.set_idle(idle, net_rx_pending);
        return Ok((NodeMeta::from_machine(m), true, rewritten));
    }
    let meta = worker_restore_full(
        machine,
        shared_intr,
        store_cache,
        config,
        chain,
        pages,
        trace,
        dirty_log,
        idle,
        net_rx_pending,
    )?;
    Ok((meta, false, 0))
}

enum WorkerReq {
    Boot {
        config: Box<Config>,
        dirty_log: DirtyLog,
        initial_local: u64,
    },
    #[cfg(not(target_os = "macos"))]
    Attach(Box<Machine>),
    Restore {
        config: Box<Config>,
        chain: Vec<Keyframe>,
        pages: PageSource,
        trace: Vec<(u64, Vec<u8>)>,
        dirty_log: DirtyLog,
        idle: bool,
        net_rx_pending: bool,
    },
    RestoreInPlace {
        config: Box<Config>,
        chain: Vec<Keyframe>,
        pages: PageSource,
        trace: Vec<(u64, Vec<u8>)>,
        dirty_log: DirtyLog,
        idle: bool,
        net_rx_pending: bool,
    },
    RunSlice {
        horizon_local: u64,
        max_steps: u64,
        step_base: u64,
        allow_idle_skip: bool,
    },
    DeliverRx {
        frame: Vec<u8>,
    },
    AdvanceIdleTo {
        target_local: u64,
    },
    KeyframeDirect {
        parent: Option<u64>,
        store_path: PathBuf,
    },
    PrepareKeyframe {
        parent: Option<u64>,
    },
    SetKeyframeOrigin(Vec<(u32, [u8; 32])>),
    SnapshotNonDestructive,
    Encoders,
    SetEncoders(Box<EncodersPair>),
    #[cfg(not(target_os = "macos"))]
    TakeMachine,
    Crash,
    #[cfg(not(target_os = "macos"))]
    Shutdown,
}

enum WorkerResp {
    Meta(Result<NodeMeta>),
    RestoredInPlace(Result<(NodeMeta, bool, usize)>),
    Slice(Box<Result<SliceReply>>),
    DeliverRx(Result<(bool, NodeMeta)>),
    Keyframe(Box<Result<Option<Keyframe>>>),
    PreparedKeyframe(Box<Result<Option<PreparedKeyframe>>>),
    NonDestructive(Box<Result<(Keyframe, MemPages, NodeEncoders)>>),
    Encoders(Box<NodeEncoders>),
    Unit,
    #[cfg(not(target_os = "macos"))]
    Machine(Option<Box<Machine>>),
    Crashed(u64),
}

/// Dedicated OS thread owning one cluster node's [`Machine`] for its entire
/// lifetime so KVM vCPU execution and thread-bound PMU branch counters stay
/// permanently pinned to the same thread.
#[cfg(not(target_os = "macos"))]
struct NodeWorker {
    name: String,
    req_tx: SyncSender<WorkerReq>,
    resp_rx: Receiver<WorkerResp>,
    interrupter: Arc<Mutex<Option<Interrupter>>>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(not(target_os = "macos"))]
impl NodeWorker {
    fn spawn(idx: usize, name: String) -> Result<NodeWorker> {
        let (req_tx, req_rx) = mpsc::sync_channel::<WorkerReq>(1);
        let (resp_tx, resp_rx) = mpsc::sync_channel::<WorkerResp>(1);
        let interrupter = Arc::new(Mutex::new(None));
        let intr_worker = interrupter.clone();
        let thread_name = format!("rewind-node-{idx}");
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_loop(req_rx, resp_tx, intr_worker))
            .with_context(|| format!("spawning worker thread for cluster node {name}"))?;
        Ok(NodeWorker {
            name,
            req_tx,
            resp_rx,
            interrupter,
            handle: Some(handle),
        })
    }

    fn send(&self, req: WorkerReq) -> Result<()> {
        self.req_tx
            .send(req)
            .map_err(|_| anyhow!("cluster worker for node {} terminated unexpectedly", self.name))
    }

    fn recv(&self) -> Result<WorkerResp> {
        self.resp_rx
            .recv()
            .map_err(|_| anyhow!("cluster worker for node {} panicked or disconnected", self.name))
    }

    fn recv_timeout(&self, timeout: Duration) -> std::result::Result<WorkerResp, RecvTimeoutError> {
        self.resp_rx.recv_timeout(timeout)
    }

    fn interrupt(&self) {
        if let Ok(guard) = self.interrupter.lock()
            && let Some(intr) = guard.as_ref()
        {
            intr.interrupt();
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for NodeWorker {
    fn drop(&mut self) {
        let _ = self.req_tx.send(WorkerReq::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn worker_loop(
    req_rx: Receiver<WorkerReq>,
    resp_tx: SyncSender<WorkerResp>,
    shared_intr: Arc<Mutex<Option<Interrupter>>>,
) {
    let mut machine: Option<Machine> = None;
    let mut store_cache = WorkerStoreCache::default();

    while let Ok(req) = req_rx.recv() {
        let resp = match req {
            WorkerReq::Boot {
                config,
                dirty_log,
                initial_local,
            } => {
                let res = (|| -> Result<NodeMeta> {
                    let mut m = Machine::boot(&config, dirty_log)?;
                    m.set_cluster_mode(true);
                    if initial_local > 0 {
                        m.set_initial_now(initial_local)?;
                    }
                    let meta = NodeMeta::from_machine(&m);
                    if let Ok(mut g) = shared_intr.lock() {
                        *g = Some(m.interrupter());
                    }
                    machine = Some(m);
                    Ok(meta)
                })();
                WorkerResp::Meta(res)
            }
            WorkerReq::Attach(m) => {
                let meta = NodeMeta::from_machine(&m);
                if let Ok(mut g) = shared_intr.lock() {
                    *g = Some(m.interrupter());
                }
                machine = Some(*m);
                WorkerResp::Meta(Ok(meta))
            }
            WorkerReq::Restore {
                config,
                chain,
                pages,
                trace,
                dirty_log,
                idle,
                net_rx_pending,
            } => {
                let res = worker_restore_full(
                    &mut machine,
                    &shared_intr,
                    &mut store_cache,
                    &config,
                    &chain,
                    pages,
                    &trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                );
                WorkerResp::Meta(res)
            }
            WorkerReq::RestoreInPlace {
                config,
                chain,
                pages,
                trace,
                dirty_log,
                idle,
                net_rx_pending,
            } => {
                let res = worker_restore_in_place(
                    &mut machine,
                    &shared_intr,
                    &mut store_cache,
                    &config,
                    &chain,
                    pages,
                    &trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                );
                WorkerResp::RestoredInPlace(res)
            }
            WorkerReq::RunSlice {
                horizon_local,
                max_steps,
                step_base,
                allow_idle_skip,
            } => {
                let res = match machine.as_mut() {
                    Some(m) => {
                        let mut obs = BufferingObserver {
                            step_base,
                            events: SliceEvents::default(),
                        };
                        m.run_cluster_slice(horizon_local, max_steps, allow_idle_skip, &mut obs)
                            .and_then(|outcome| {
                                let outcome = SliceOutcome::from_outcome(outcome)?;
                                let tx_frames = m.take_tx_frames();
                                let meta = NodeMeta::from_machine(m);
                                Ok(SliceReply {
                                    outcome,
                                    meta,
                                    events: obs.events,
                                    tx_frames,
                                })
                            })
                    }
                    None => Err(anyhow!("worker has no live Machine for RunSlice")),
                };
                WorkerResp::Slice(Box::new(res))
            }
            WorkerReq::DeliverRx { frame } => {
                let res = match machine.as_mut() {
                    Some(m) => m
                        .deliver_rx_frame(&frame)
                        .map(|ok| (ok, NodeMeta::from_machine(m))),
                    None => Err(anyhow!("worker has no live Machine for DeliverRx")),
                };
                WorkerResp::DeliverRx(res)
            }
            WorkerReq::AdvanceIdleTo { target_local } => {
                let res = match machine.as_mut() {
                    Some(m) => m
                        .advance_idle_to(target_local)
                        .map(|()| NodeMeta::from_machine(m)),
                    None => Err(anyhow!("worker has no live Machine for AdvanceIdleTo")),
                };
                WorkerResp::Meta(res)
            }
            WorkerReq::KeyframeDirect { parent, store_path } => {
                let res = match machine.as_mut() {
                    Some(m) => store_cache.open(&store_path).and_then(|store| {
                        let mut sp = StoreMutPages(store);
                        let kf = m.keyframe(&mut sp, parent)?;
                        sp.0.sync()?;
                        Ok(Some(kf))
                    }),
                    None => Ok(None),
                };
                WorkerResp::Keyframe(Box::new(res))
            }
            WorkerReq::PrepareKeyframe { parent } => {
                let res = match machine.as_mut() {
                    Some(m) => m.prepare_keyframe(parent).map(Some),
                    None => Ok(None),
                };
                WorkerResp::PreparedKeyframe(Box::new(res))
            }
            WorkerReq::SetKeyframeOrigin(origin_pages) => {
                if let Some(m) = machine.as_mut() {
                    m.set_keyframe_origin(origin_pages);
                }
                WorkerResp::Unit
            }
            WorkerReq::SnapshotNonDestructive => {
                let res = match machine.as_mut() {
                    Some(m) => {
                        let enc = m.encoders();
                        let mut mem_pages = MemPages::default();
                        m.snapshot_non_destructive(&mut mem_pages)
                            .map(|kf| (kf, mem_pages, Some(enc)))
                    }
                    None => Err(anyhow!("node is currently crashed")),
                };
                WorkerResp::NonDestructive(Box::new(res))
            }
            WorkerReq::Encoders => {
                WorkerResp::Encoders(Box::new(machine.as_ref().map(Machine::encoders)))
            }
            WorkerReq::SetEncoders(pair) => {
                let (io, switches) = *pair;
                if let Some(m) = machine.as_mut() {
                    m.set_encoders(io, switches);
                }
                WorkerResp::Unit
            }
            WorkerReq::TakeMachine => {
                if let Ok(mut g) = shared_intr.lock() {
                    *g = None;
                }
                let taken = machine.take().map(|mut m| {
                    m.release_counters();
                    Box::new(m)
                });
                WorkerResp::Machine(taken)
            }
            WorkerReq::Crash => {
                if let Ok(mut g) = shared_intr.lock() {
                    *g = None;
                }
                let steps = machine.take().map_or(0, |m| m.step());
                WorkerResp::Crashed(steps)
            }
            WorkerReq::Shutdown => {
                if let Ok(mut g) = shared_intr.lock() {
                    *g = None;
                }
                drop(machine.take());
                break;
            }
        };
        if resp_tx.send(resp).is_err() {
            break;
        }
    }
}

/// Runs a single cluster node's VM inside a dedicated child worker process
/// communicating over stdin/stdout (used on macOS Hypervisor.framework where
/// each OS process may hold at most one VM).
#[cfg(not(target_os = "macos"))]
pub fn run_process_worker_stdio() -> Result<()> {
    bail!("cluster _worker is only used on macOS Hypervisor.framework")
}

#[cfg(target_os = "macos")]
mod proc_worker {
    use super::*;
    use std::io::{BufReader, BufWriter, Read, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};

    #[derive(Serialize, Deserialize)]
    pub(super) enum WireReq {
        Boot {
            config: Box<Config>,
            dirty_log: DirtyLog,
            initial_local: u64,
        },
        Restore {
            config: Box<Config>,
            chain: Vec<Keyframe>,
            pages: PageSource,
            trace: Vec<(u64, Vec<u8>)>,
            dirty_log: DirtyLog,
            idle: bool,
            net_rx_pending: bool,
        },
        RestoreInPlace {
            config: Box<Config>,
            chain: Vec<Keyframe>,
            pages: PageSource,
            trace: Vec<(u64, Vec<u8>)>,
            dirty_log: DirtyLog,
            idle: bool,
            net_rx_pending: bool,
        },
        RunSlice {
            horizon_local: u64,
            max_steps: u64,
            step_base: u64,
            allow_idle_skip: bool,
        },
        DeliverRx {
            frame: Vec<u8>,
        },
        AdvanceIdleTo {
            target_local: u64,
        },
        KeyframeDirect {
            parent: Option<u64>,
            store_path: PathBuf,
        },
        PrepareKeyframe {
            parent: Option<u64>,
        },
        SetKeyframeOrigin(Vec<(u32, [u8; 32])>),
        SnapshotNonDestructive,
        Encoders,
        SetEncoders(Box<EncodersPair>),
        Crash,
        Interrupt,
        Shutdown,
    }

    #[derive(Serialize, Deserialize)]
    pub(super) enum WireResp {
        Meta(std::result::Result<NodeMeta, String>),
        RestoredInPlace(std::result::Result<(NodeMeta, bool, usize), String>),
        Slice(Box<std::result::Result<SliceReply, String>>),
        DeliverRx(std::result::Result<(bool, NodeMeta), String>),
        Keyframe(Box<std::result::Result<Option<Keyframe>, String>>),
        PreparedKeyframe(Box<std::result::Result<Option<PreparedKeyframe>, String>>),
        NonDestructive(Box<std::result::Result<(Keyframe, MemPages, NodeEncoders), String>>),
        Encoders(Box<NodeEncoders>),
        Unit,
        Crashed(u64),
    }

    fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
        let bytes = bincode::serialize(msg).context("serializing cluster worker IPC message")?;
        let len = u32::try_from(bytes.len()).context("cluster worker IPC message exceeds 4 GiB")?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&bytes)?;
        w.flush()?;
        Ok(())
    }

    fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>> {
        let mut len_b = [0u8; 4];
        match r.read_exact(&mut len_b) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading cluster worker IPC length"),
        }
        let len = u32::from_le_bytes(len_b) as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)
            .context("reading cluster worker IPC payload")?;
        let msg = bincode::deserialize(&buf).context("deserializing cluster worker IPC message")?;
        Ok(Some(msg))
    }

    /// Per-node child process owning one HVF [`Machine`] on macOS.
    pub(super) struct NodeWorker {
        name: String,
        stdin: Mutex<BufWriter<ChildStdin>>,
        resp_rx: Receiver<WorkerResp>,
        reader_handle: Option<JoinHandle<()>>,
        child: Option<Child>,
    }

    impl NodeWorker {
        pub(super) fn spawn(idx: usize, name: String) -> Result<NodeWorker> {
            let exe = match std::env::var_os("REWIND_CLUSTER_WORKER_BIN") {
                Some(p) if !p.is_empty() => PathBuf::from(p),
                _ => std::env::current_exe().context("locating current rewind executable")?,
            };
            let mut child = Command::new(&exe)
                .args(["cluster", "_worker"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| {
                    format!(
                        "spawning macOS HVF worker process ({}) for cluster node {name}",
                        exe.display()
                    )
                })?;
            let child_stdin = child
                .stdin
                .take()
                .context("capturing cluster worker stdin")?;
            let child_stdout = child
                .stdout
                .take()
                .context("capturing cluster worker stdout")?;

            let (resp_tx, resp_rx) = mpsc::sync_channel::<WorkerResp>(1);
            let reader_name = format!("rewind-node-{idx}-rx");
            let reader_handle = std::thread::Builder::new()
                .name(reader_name)
                .spawn(move || {
                    let mut r = BufReader::new(child_stdout);
                    while let Ok(Some(wire)) = read_frame::<_, WireResp>(&mut r) {
                        let resp = match wire {
                            WireResp::Meta(r) => WorkerResp::Meta(r.map_err(|e| anyhow!(e))),
                            WireResp::RestoredInPlace(r) => {
                                WorkerResp::RestoredInPlace(r.map_err(|e| anyhow!(e)))
                            }
                            WireResp::Slice(r) => {
                                WorkerResp::Slice(Box::new((*r).map_err(|e| anyhow!(e))))
                            }
                            WireResp::DeliverRx(r) => {
                                WorkerResp::DeliverRx(r.map_err(|e| anyhow!(e)))
                            }
                            WireResp::Keyframe(r) => {
                                WorkerResp::Keyframe(Box::new((*r).map_err(|e| anyhow!(e))))
                            }
                            WireResp::PreparedKeyframe(r) => {
                                WorkerResp::PreparedKeyframe(Box::new((*r).map_err(|e| anyhow!(e))))
                            }
                            WireResp::NonDestructive(r) => {
                                WorkerResp::NonDestructive(Box::new((*r).map_err(|e| anyhow!(e))))
                            }
                            WireResp::Encoders(e) => WorkerResp::Encoders(e),
                            WireResp::Unit => WorkerResp::Unit,
                            WireResp::Crashed(s) => WorkerResp::Crashed(s),
                        };
                        if resp_tx.send(resp).is_err() {
                            break;
                        }
                    }
                })
                .with_context(|| format!("spawning IPC reader thread for cluster node {name}"))?;

            Ok(NodeWorker {
                name,
                stdin: Mutex::new(BufWriter::new(child_stdin)),
                resp_rx,
                reader_handle: Some(reader_handle),
                child: Some(child),
            })
        }

        pub(super) fn send(&self, req: WorkerReq) -> Result<()> {
            let wire = match req {
                WorkerReq::Boot {
                    config,
                    dirty_log,
                    initial_local,
                } => WireReq::Boot {
                    config,
                    dirty_log,
                    initial_local,
                },
                WorkerReq::Restore {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                } => WireReq::Restore {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                },
                WorkerReq::RestoreInPlace {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                } => WireReq::RestoreInPlace {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                },
                WorkerReq::RunSlice {
                    horizon_local,
                    max_steps,
                    step_base,
                    allow_idle_skip,
                } => WireReq::RunSlice {
                    horizon_local,
                    max_steps,
                    step_base,
                    allow_idle_skip,
                },
                WorkerReq::DeliverRx { frame } => WireReq::DeliverRx { frame },
                WorkerReq::AdvanceIdleTo { target_local } => {
                    WireReq::AdvanceIdleTo { target_local }
                }
                WorkerReq::KeyframeDirect { parent, store_path } => {
                    WireReq::KeyframeDirect { parent, store_path }
                }
                WorkerReq::PrepareKeyframe { parent } => WireReq::PrepareKeyframe { parent },
                WorkerReq::SetKeyframeOrigin(p) => WireReq::SetKeyframeOrigin(p),
                WorkerReq::SnapshotNonDestructive => WireReq::SnapshotNonDestructive,
                WorkerReq::Encoders => WireReq::Encoders,
                WorkerReq::SetEncoders(p) => WireReq::SetEncoders(p),
                WorkerReq::Crash => WireReq::Crash,
            };
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| anyhow!("cluster worker stdin lock poisoned for {}", self.name))?;
            write_frame(&mut *stdin, &wire).with_context(|| {
                format!("sending request to HVF worker process for node {}", self.name)
            })
        }

        pub(super) fn recv(&self) -> Result<WorkerResp> {
            self.resp_rx.recv().map_err(|_| {
                anyhow!(
                    "HVF worker process for node {} exited or disconnected unexpectedly",
                    self.name
                )
            })
        }

        pub(super) fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> std::result::Result<WorkerResp, RecvTimeoutError> {
            self.resp_rx.recv_timeout(timeout)
        }

        pub(super) fn interrupt(&self) {
            if let Ok(mut stdin) = self.stdin.lock() {
                let _ = write_frame(&mut *stdin, &WireReq::Interrupt);
            }
        }
    }

    impl Drop for NodeWorker {
        fn drop(&mut self) {
            if let Ok(mut stdin) = self.stdin.lock() {
                let _ = write_frame(&mut *stdin, &WireReq::Shutdown);
            }
            if let Some(mut child) = self.child.take() {
                let deadline = Instant::now() + Duration::from_millis(500);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                    }
                }
            }
            if let Some(h) = self.reader_handle.take() {
                let _ = h.join();
            }
        }
    }

    pub(super) fn run_stdio() -> Result<()> {
        let shared_intr: Arc<Mutex<Option<Interrupter>>> = Arc::new(Mutex::new(None));
        let intr_reader = shared_intr.clone();
        let (req_tx, req_rx) = mpsc::sync_channel::<WireReq>(1);

        let reader = std::thread::spawn(move || {
            let mut stdin = BufReader::new(std::io::stdin().lock());
            while let Ok(Some(req)) = read_frame::<_, WireReq>(&mut stdin) {
                if matches!(req, WireReq::Interrupt) {
                    if let Ok(g) = intr_reader.lock()
                        && let Some(intr) = g.as_ref()
                    {
                        intr.interrupt();
                    }
                    continue;
                }
                let is_shutdown = matches!(req, WireReq::Shutdown);
                if req_tx.send(req).is_err() || is_shutdown {
                    break;
                }
            }
        });

        let mut stdout = BufWriter::new(std::io::stdout().lock());
        let mut machine: Option<Machine> = None;
        let mut store_cache = WorkerStoreCache::default();

        while let Ok(req) = req_rx.recv() {
            let resp = match req {
                WireReq::Boot {
                    config,
                    dirty_log,
                    initial_local,
                } => {
                    // Destroy any previous VM before creating a new one (HVF 1-VM limit).
                    if let Ok(mut g) = shared_intr.lock() {
                        *g = None;
                    }
                    drop(machine.take());
                    let res = (|| -> Result<NodeMeta> {
                        let mut m = Machine::boot(&config, dirty_log)?;
                        m.set_cluster_mode(true);
                        if initial_local > 0 {
                            m.set_initial_now(initial_local)?;
                        }
                        let meta = NodeMeta::from_machine(&m);
                        if let Ok(mut g) = shared_intr.lock() {
                            *g = Some(m.interrupter());
                        }
                        machine = Some(m);
                        Ok(meta)
                    })();
                    WireResp::Meta(res.map_err(|e| format!("{e:#}")))
                }
                WireReq::Restore {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                } => {
                    let res = worker_restore_full(
                        &mut machine,
                        &shared_intr,
                        &mut store_cache,
                        &config,
                        &chain,
                        pages,
                        &trace,
                        dirty_log,
                        idle,
                        net_rx_pending,
                    );
                    WireResp::Meta(res.map_err(|e| format!("{e:#}")))
                }
                WireReq::RestoreInPlace {
                    config,
                    chain,
                    pages,
                    trace,
                    dirty_log,
                    idle,
                    net_rx_pending,
                } => {
                    let res = worker_restore_in_place(
                        &mut machine,
                        &shared_intr,
                        &mut store_cache,
                        &config,
                        &chain,
                        pages,
                        &trace,
                        dirty_log,
                        idle,
                        net_rx_pending,
                    );
                    WireResp::RestoredInPlace(res.map_err(|e| format!("{e:#}")))
                }
                WireReq::RunSlice {
                    horizon_local,
                    max_steps,
                    step_base,
                    allow_idle_skip,
                } => {
                    let res = match machine.as_mut() {
                        Some(m) => {
                            let mut obs = BufferingObserver {
                                step_base,
                                events: SliceEvents::default(),
                            };
                            m.run_cluster_slice(horizon_local, max_steps, allow_idle_skip, &mut obs)
                                .and_then(|outcome| {
                                    let outcome = SliceOutcome::from_outcome(outcome)?;
                                    let tx_frames = m.take_tx_frames();
                                    let meta = NodeMeta::from_machine(m);
                                    Ok(SliceReply {
                                        outcome,
                                        meta,
                                        events: obs.events,
                                        tx_frames,
                                    })
                                })
                        }
                        None => Err(anyhow!("worker has no live Machine for RunSlice")),
                    };
                    WireResp::Slice(Box::new(res.map_err(|e| format!("{e:#}"))))
                }
                WireReq::DeliverRx { frame } => {
                    let res = match machine.as_mut() {
                        Some(m) => m
                            .deliver_rx_frame(&frame)
                            .map(|ok| (ok, NodeMeta::from_machine(m))),
                        None => Err(anyhow!("worker has no live Machine for DeliverRx")),
                    };
                    WireResp::DeliverRx(res.map_err(|e| format!("{e:#}")))
                }
                WireReq::AdvanceIdleTo { target_local } => {
                    let res = match machine.as_mut() {
                        Some(m) => m
                            .advance_idle_to(target_local)
                            .map(|()| NodeMeta::from_machine(m)),
                        None => Err(anyhow!("worker has no live Machine for AdvanceIdleTo")),
                    };
                    WireResp::Meta(res.map_err(|e| format!("{e:#}")))
                }
                WireReq::KeyframeDirect { parent, store_path } => {
                    let res = match machine.as_mut() {
                        Some(m) => store_cache.open(&store_path).and_then(|store| {
                            let mut sp = StoreMutPages(store);
                            let kf = m.keyframe(&mut sp, parent)?;
                            sp.0.sync()?;
                            Ok(Some(kf))
                        }),
                        None => Ok(None),
                    };
                    WireResp::Keyframe(Box::new(res.map_err(|e| format!("{e:#}"))))
                }
                WireReq::PrepareKeyframe { parent } => {
                    let res = match machine.as_mut() {
                        Some(m) => m.prepare_keyframe(parent).map(Some),
                        None => Ok(None),
                    };
                    WireResp::PreparedKeyframe(Box::new(res.map_err(|e| format!("{e:#}"))))
                }
                WireReq::SetKeyframeOrigin(origin_pages) => {
                    if let Some(m) = machine.as_mut() {
                        m.set_keyframe_origin(origin_pages);
                    }
                    WireResp::Unit
                }
                WireReq::SnapshotNonDestructive => {
                    let res = match machine.as_mut() {
                        Some(m) => {
                            let enc = m.encoders();
                            let mut mem_pages = MemPages::default();
                            m.snapshot_non_destructive(&mut mem_pages)
                                .map(|kf| (kf, mem_pages, Some(enc)))
                        }
                        None => Err(anyhow!("node is currently crashed")),
                    };
                    WireResp::NonDestructive(Box::new(res.map_err(|e| format!("{e:#}"))))
                }
                WireReq::Encoders => {
                    WireResp::Encoders(Box::new(machine.as_ref().map(Machine::encoders)))
                }
                WireReq::SetEncoders(pair) => {
                    let (io, switches) = *pair;
                    if let Some(m) = machine.as_mut() {
                        m.set_encoders(io, switches);
                    }
                    WireResp::Unit
                }
                WireReq::Crash => {
                    if let Ok(mut g) = shared_intr.lock() {
                        *g = None;
                    }
                    let steps = machine.take().map_or(0, |m| m.step());
                    WireResp::Crashed(steps)
                }
                WireReq::Interrupt => continue,
                WireReq::Shutdown => {
                    if let Ok(mut g) = shared_intr.lock() {
                        *g = None;
                    }
                    drop(machine.take());
                    break;
                }
            };
            if write_frame(&mut stdout, &resp).is_err() {
                break;
            }
        }
        drop(machine.take());
        let _ = reader.join();
        Ok(())
    }
}

#[cfg(target_os = "macos")]
use proc_worker::NodeWorker;

/// Runs a single cluster node's VM inside a dedicated child worker process
/// communicating over stdin/stdout (used on macOS Hypervisor.framework where
/// each OS process may hold at most one VM).
#[cfg(target_os = "macos")]
pub fn run_process_worker_stdio() -> Result<()> {
    proc_worker::run_stdio()
}

/// Where a node's [`Machine`] lives: inline on the calling thread (sequential
/// mode or 1-node cluster) or pinned to a dedicated [`NodeWorker`] thread.
enum NodeBackend {
    Local(Option<Box<Machine>>),
    Worker(NodeWorker),
}

/// Live runtime state of one node in the cluster.
struct LiveNode {
    cfg: ClusterNodeConfig,
    backend: NodeBackend,
    meta: Option<NodeMeta>,
    /// Cluster virtual time this node has reached.
    cluster_ns: u64,
    /// Cluster virtual time at which the current boot started (anchor for
    /// path-independent clock drift conversion).
    boot_cluster_ns: u64,
    /// Local clock value at `boot_cluster_ns` (after skew).
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

/// Pure, path-independent conversion from cluster virtual time to local virtual
/// time relative to a per-boot anchor `(boot_cluster_ns, local_base_ns, drift_ppm)`.
fn anchor_cluster_to_local_ns(
    boot_cluster_ns: u64,
    local_base_ns: u64,
    clock_drift_ppm: i32,
    cluster_ns: u64,
) -> u64 {
    let elapsed_c = cluster_ns.saturating_sub(boot_cluster_ns);
    let rate = (1_000_000i128 + i128::from(clock_drift_ppm)).max(1) as u128;
    let elapsed_l = (u128::from(elapsed_c) * rate / 1_000_000u128) as u64;
    local_base_ns.saturating_add(elapsed_l)
}

/// Pure, path-independent conversion from local virtual time to cluster virtual
/// time relative to a per-boot anchor.
fn anchor_local_to_cluster_ns(
    boot_cluster_ns: u64,
    local_base_ns: u64,
    clock_drift_ppm: i32,
    local_ns: u64,
) -> u64 {
    let elapsed_l = local_ns.saturating_sub(local_base_ns);
    let rate = (1_000_000i128 + i128::from(clock_drift_ppm)).max(1) as u128;
    let elapsed_c = (u128::from(elapsed_l) * 1_000_000u128 / rate) as u64;
    boot_cluster_ns.saturating_add(elapsed_c)
}

/// Pure, path-independent conversion of an armed local timer deadline to
/// cluster virtual time, rounded up so a timer never fires before
/// `local_now >= deadline_local`.
fn anchor_local_deadline_to_cluster_ns(
    boot_cluster_ns: u64,
    local_base_ns: u64,
    clock_drift_ppm: i32,
    deadline_local: u64,
) -> u64 {
    let elapsed_l = deadline_local.saturating_sub(local_base_ns);
    let rate = (1_000_000i128 + i128::from(clock_drift_ppm)).max(1) as u128;
    let elapsed_c = (u128::from(elapsed_l) * 1_000_000u128).div_ceil(rate) as u64;
    boot_cluster_ns.saturating_add(elapsed_c)
}

impl LiveNode {
    fn cluster_to_local_ns(&self, cluster_ns: u64) -> u64 {
        anchor_cluster_to_local_ns(
            self.boot_cluster_ns,
            self.local_base_ns,
            self.cfg.clock_drift_ppm,
            cluster_ns,
        )
    }

    fn local_to_cluster_ns(&self, local_ns: u64) -> u64 {
        anchor_local_to_cluster_ns(
            self.boot_cluster_ns,
            self.local_base_ns,
            self.cfg.clock_drift_ppm,
            local_ns,
        )
    }

    fn local_deadline_to_cluster_ns(&self, deadline_local: u64) -> u64 {
        anchor_local_deadline_to_cluster_ns(
            self.boot_cluster_ns,
            self.local_base_ns,
            self.cfg.clock_drift_ppm,
            deadline_local,
        )
    }

    /// Current total steps across all boots of this node.
    fn total_steps(&self) -> u64 {
        self.step_base + self.meta.as_ref().map_or(0, |m| m.step)
    }

    /// Current local virtual clock of this node.
    fn local_now(&self) -> u64 {
        self.meta.as_ref().map_or(self.local_base_ns, |m| m.now)
    }

    fn timer_deadline(&self) -> Option<u64> {
        self.meta.as_ref()?.timer_deadline
    }

    /// Next armed local timer deadline converted to cluster virtual time.
    fn next_timer_cluster_ns(&self) -> Option<u64> {
        let deadline_local = self.timer_deadline()?;
        Some(
            self.local_deadline_to_cluster_ns(deadline_local)
                .max(self.cluster_ns),
        )
    }

    fn is_runnable(&self) -> bool {
        !self.exited
            && self
                .meta
                .as_ref()
                .is_some_and(|m| !m.is_idle && m.stopped.is_none())
    }

    fn is_alive(&self) -> bool {
        !self.exited && self.meta.as_ref().is_some_and(|m| m.stopped.is_none())
    }

    fn deliver_rx_frame(&mut self, frame: &[u8]) -> Result<bool> {
        match &mut self.backend {
            NodeBackend::Local(opt) => {
                let Some(m) = opt.as_mut() else {
                    return Ok(false);
                };
                let ok = m.deliver_rx_frame(frame)?;
                self.meta = Some(NodeMeta::from_machine(m));
                Ok(ok)
            }
            NodeBackend::Worker(w) => {
                if self.meta.is_none() {
                    return Ok(false);
                }
                w.send(WorkerReq::DeliverRx {
                    frame: frame.to_vec(),
                })?;
                match w.recv()? {
                    WorkerResp::DeliverRx(res) => {
                        let (ok, meta) = res?;
                        self.meta = Some(meta);
                        Ok(ok)
                    }
                    _ => bail!("unexpected response to DeliverRx from node {}", self.cfg.name),
                }
            }
        }
    }

    fn advance_idle_to(&mut self, target_local: u64) -> Result<()> {
        match &mut self.backend {
            NodeBackend::Local(opt) => {
                if let Some(m) = opt.as_mut() {
                    m.advance_idle_to(target_local)?;
                    self.meta = Some(NodeMeta::from_machine(m));
                }
                Ok(())
            }
            NodeBackend::Worker(w) => {
                if self.meta.is_none() {
                    return Ok(());
                }
                w.send(WorkerReq::AdvanceIdleTo { target_local })?;
                match w.recv()? {
                    WorkerResp::Meta(res) => {
                        self.meta = Some(res?);
                        Ok(())
                    }
                    _ => bail!(
                        "unexpected response to AdvanceIdleTo from node {}",
                        self.cfg.name
                    ),
                }
            }
        }
    }

    fn encoders(&self) -> NodeEncoders {
        match &self.backend {
            NodeBackend::Local(opt) => opt.as_deref().map(Machine::encoders),
            NodeBackend::Worker(w) => {
                self.meta.as_ref()?;
                if w.send(WorkerReq::Encoders).is_err() {
                    return None;
                }
                match w.recv() {
                    Ok(WorkerResp::Encoders(e)) => *e,
                    _ => None,
                }
            }
        }
    }

    fn set_encoders(
        &mut self,
        io: Option<rewind_trace::io_records::Encoder>,
        switches: Option<rewind_trace::switches::Encoder>,
    ) {
        match &mut self.backend {
            NodeBackend::Local(opt) => {
                if let Some(m) = opt.as_deref_mut() {
                    m.set_encoders(io, switches);
                }
            }
            NodeBackend::Worker(w) => {
                if self.meta.is_some()
                    && w.send(WorkerReq::SetEncoders(Box::new((io, switches))))
                        .is_ok()
                {
                    let _ = w.recv();
                }
            }
        }
    }

    fn crash(&mut self) -> Result<()> {
        let steps = match &mut self.backend {
            NodeBackend::Local(opt) => opt.take().map_or(0, |m| m.step()),
            NodeBackend::Worker(w) => {
                w.send(WorkerReq::Crash)?;
                match w.recv()? {
                    WorkerResp::Crashed(s) => s,
                    _ => bail!("unexpected response to Crash from node {}", self.cfg.name),
                }
            }
        };
        self.step_base += steps;
        self.meta = None;
        self.pending_rx.clear();
        Ok(())
    }

    fn boot_fresh(
        &mut self,
        cfg: Config,
        dirty_log: DirtyLog,
        boot_cluster_ns: u64,
        initial_local: u64,
    ) -> Result<()> {
        let meta = match &mut self.backend {
            NodeBackend::Local(opt) => {
                let mut m = Machine::boot(&cfg, dirty_log).with_context(|| {
                    format!(
                        "restarting cluster node {} (boot {})",
                        self.cfg.name, self.boot_count
                    )
                })?;
                m.set_cluster_mode(true);
                if initial_local > 0 {
                    m.set_initial_now(initial_local)?;
                }
                let meta = NodeMeta::from_machine(&m);
                *opt = Some(Box::new(m));
                meta
            }
            NodeBackend::Worker(w) => {
                w.send(WorkerReq::Boot {
                    config: Box::new(cfg),
                    dirty_log,
                    initial_local,
                })?;
                match w.recv()? {
                    WorkerResp::Meta(res) => res.with_context(|| {
                        format!(
                            "restarting cluster node {} (boot {})",
                            self.cfg.name, self.boot_count
                        )
                    })?,
                    _ => bail!("unexpected response to Boot from node {}", self.cfg.name),
                }
            }
        };
        self.meta = Some(meta);
        self.cluster_ns = boot_cluster_ns;
        self.boot_cluster_ns = boot_cluster_ns;
        self.local_base_ns = initial_local;
        self.exited = false;
        self.last_stop = None;
        self.pending_rx.clear();
        Ok(())
    }

    fn take_machine(&mut self) -> Result<Machine> {
        let taken = match &mut self.backend {
            NodeBackend::Local(opt) => opt.take().map(|m| *m),
            NodeBackend::Worker(w) => {
                #[cfg(not(target_os = "macos"))]
                {
                    w.send(WorkerReq::TakeMachine)?;
                    match w.recv()? {
                        WorkerResp::Machine(m) => m.map(|m| *m),
                        _ => bail!(
                            "unexpected response to TakeMachine from node {}",
                            self.cfg.name
                        ),
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    if self.meta.is_none() {
                        None
                    } else {
                        w.send(WorkerReq::SnapshotNonDestructive)?;
                        let (kf, mem_pages, enc) = match w.recv()? {
                            WorkerResp::NonDestructive(res) => (*res)?,
                            _ => bail!(
                                "unexpected response to SnapshotNonDestructive from node {}",
                                self.cfg.name
                            ),
                        };
                        w.send(WorkerReq::Crash)?;
                        let _ = w.recv()?;
                        let mut cfg = self.cfg.config.clone();
                        if self.boot_count > 1 {
                            cfg.seed = reboot_seed(self.cfg.base_seed, self.boot_count);
                        }
                        let mut m = Machine::restore(&cfg, &[kf], &mem_pages, &[], DirtyLog::Off)?;
                        if let Some((io, switches)) = enc {
                            m.set_encoders(io, switches);
                        }
                        Some(m)
                    }
                }
            }
        };
        self.meta = None;
        taken.with_context(|| format!("node {} is crashed at this point", self.cfg.name))
    }

    fn ensure_local_machine_mut(&mut self) -> Option<&mut Machine> {
        if matches!(self.backend, NodeBackend::Worker(_)) {
            let m = self.take_machine().ok()?;
            self.meta = Some(NodeMeta::from_machine(&m));
            self.backend = NodeBackend::Local(Some(Box::new(m)));
        }
        match &mut self.backend {
            NodeBackend::Local(opt) => opt.as_deref_mut(),
            NodeBackend::Worker(_) => unreachable!(),
        }
    }
}

/// Abstract node execution state used by [`compute_lookahead_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeExecState {
    Runnable { cluster_ns: u64 },
    Idle { next_timer_ns: Option<u64> },
    Dead,
}

/// Computes `(earliest_tx, incoming_bound)` for every node via multi-hop
/// Chandy–Misra–Bryant fixed-point relaxation over unblocked directed links.
///
/// - A runnable node `A` can transmit immediately at `cluster_ns(A)`.
/// - An idle node `A` advertises its earliest possible wakeup: the minimum of
///   its armed local timer, any already in-flight frame addressed to `A`, and
///   any transitive incoming path `C -> ... -> A` over unblocked links.
/// - Crashed or exited (`Dead`) nodes cannot transmit (`u64::MAX`) until a
///   fault restarts them (and `next_timed_fault_ns()` bounds all horizons).
/// - Blocked links (`is_blocked(A, B)`) do not constrain `B`'s lookahead.
fn compute_lookahead_from(
    states: &[NodeExecState],
    next_rx: &[Option<u64>],
    is_blocked: impl Fn(usize, usize) -> bool,
    link_min_latency: impl Fn(usize, usize) -> u64,
) -> (Vec<u64>, Vec<u64>) {
    let n = states.len();
    let mut earliest_tx = vec![u64::MAX; n];

    for i in 0..n {
        earliest_tx[i] = match states[i] {
            NodeExecState::Runnable { cluster_ns } => cluster_ns,
            NodeExecState::Idle { next_timer_ns } => [next_timer_ns, next_rx[i]]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(u64::MAX),
            NodeExecState::Dead => u64::MAX,
        };
    }

    // Relax multi-hop null messages across idle nodes to fixed point (<= n passes).
    for _ in 0..n {
        let mut changed = false;
        for b in 0..n {
            if !matches!(states[b], NodeExecState::Idle { .. }) {
                continue;
            }
            for a in 0..n {
                let et_a = earliest_tx[a];
                if a == b || et_a == u64::MAX || is_blocked(a, b) {
                    continue;
                }
                let arrival = et_a.saturating_add(link_min_latency(a, b));
                if arrival < earliest_tx[b] {
                    earliest_tx[b] = arrival;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut incoming_bound = vec![u64::MAX; n];
    for (b, bound) in incoming_bound.iter_mut().enumerate() {
        for (a, &et_a) in earliest_tx.iter().enumerate() {
            if a == b || et_a == u64::MAX || is_blocked(a, b) {
                continue;
            }
            let arrival = et_a.saturating_add(link_min_latency(a, b));
            if arrival < *bound {
                *bound = arrival;
            }
        }
    }

    (earliest_tx, incoming_bound)
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

/// Multi-VM deterministic cluster.
pub struct Cluster {
    config: ClusterConfig,
    dirty_log: DirtyLog,
    nodes: Vec<LiveNode>,
    switch: NetSwitch,
    /// Monotone global virtual time floor across the cluster.
    cluster_ns: u64,
    /// Monotone deterministic coordinator step counter (bumped on every
    /// execution round and every applied fault).
    cluster_step: u64,
    min_skew_ns: i64,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field("nodes", &self.nodes.len())
            .field("cluster_ns", &self.cluster_ns)
            .field("cluster_step", &self.cluster_step)
            .field("parallel", &self.config.parallel)
            .finish_non_exhaustive()
    }
}

impl Cluster {
    fn use_workers(config: &ClusterConfig) -> bool {
        #[cfg(target_os = "macos")]
        {
            // macOS Hypervisor.framework allows at most one VM per OS process,
            // so multi-node clusters always isolate each VM in a child process.
            config.nodes.len() > 1
        }
        #[cfg(not(target_os = "macos"))]
        {
            config.parallel && config.nodes.len() > 1
        }
    }

    /// Boots all nodes of `config` at cluster time 0 (concurrently across
    /// dedicated worker threads when `config.parallel && nodes.len() > 1`).
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

        let parallel_workers = Self::use_workers(&config);
        let mut nodes = Vec::with_capacity(config.nodes.len());

        if parallel_workers {
            let mut workers = Vec::with_capacity(config.nodes.len());
            let mut initial_locals = Vec::with_capacity(config.nodes.len());
            for (idx, node_cfg) in config.nodes.iter().enumerate() {
                let worker = NodeWorker::spawn(idx, node_cfg.name.clone())?;
                let initial_local = (node_cfg.clock_skew_ns - min_skew_ns).max(0) as u64;
                worker.send(WorkerReq::Boot {
                    config: Box::new(node_cfg.config.clone()),
                    dirty_log,
                    initial_local,
                })?;
                workers.push(worker);
                initial_locals.push(initial_local);
            }
            for ((node_cfg, worker), initial_local) in config
                .nodes
                .iter()
                .zip(workers)
                .zip(initial_locals)
            {
                let meta = match worker.recv()? {
                    WorkerResp::Meta(res) => res
                        .with_context(|| format!("booting cluster node {}", node_cfg.name))?,
                    _ => bail!("unexpected response to Boot from node {}", node_cfg.name),
                };
                nodes.push(LiveNode {
                    cfg: node_cfg.clone(),
                    backend: NodeBackend::Worker(worker),
                    meta: Some(meta),
                    cluster_ns: 0,
                    boot_cluster_ns: 0,
                    local_base_ns: initial_local,
                    boot_count: 1,
                    step_base: 0,
                    exited: false,
                    last_stop: None,
                    pending_rx: VecDeque::new(),
                });
            }
        } else {
            for node_cfg in &config.nodes {
                let mut m = Machine::boot(&node_cfg.config, dirty_log)
                    .with_context(|| format!("booting cluster node {}", node_cfg.name))?;
                m.set_cluster_mode(true);
                let initial_local = (node_cfg.clock_skew_ns - min_skew_ns).max(0) as u64;
                if initial_local > 0 {
                    m.set_initial_now(initial_local)?;
                }
                let meta = NodeMeta::from_machine(&m);
                nodes.push(LiveNode {
                    cfg: node_cfg.clone(),
                    backend: NodeBackend::Local(Some(Box::new(m))),
                    meta: Some(meta),
                    cluster_ns: 0,
                    boot_cluster_ns: 0,
                    local_base_ns: initial_local,
                    boot_count: 1,
                    step_base: 0,
                    exited: false,
                    last_stop: None,
                    pending_rx: VecDeque::new(),
                });
            }
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

        let parallel_workers = Self::use_workers(&config);
        let mut nodes = Vec::with_capacity(config.nodes.len());

        #[cfg(target_os = "macos")]
        if parallel_workers {
            let mut workers = Vec::with_capacity(config.nodes.len());
            let mut live_flags = Vec::with_capacity(config.nodes.len());
            for (idx, node_cfg) in config.nodes.iter().enumerate() {
                let last_nk = &last.nodes[idx];
                let worker = NodeWorker::spawn(idx, node_cfg.name.clone())?;
                if last_nk.keyframe.is_some() {
                    let node_chain = extract_node_chain(chain, idx, last_nk.boot_count);
                    let mut m_cfg = node_cfg.config.clone();
                    if last_nk.boot_count > 1 {
                        m_cfg.seed = reboot_seed(node_cfg.base_seed, last_nk.boot_count);
                    }
                    let page_source = build_page_source(&node_chain, pages)?;
                    let trace = node_traces.get(idx).cloned().unwrap_or_default();
                    worker.send(WorkerReq::Restore {
                        config: Box::new(m_cfg),
                        chain: node_chain,
                        pages: page_source,
                        trace,
                        dirty_log,
                        idle: last_nk.idle,
                        net_rx_pending: last_nk.net_rx_pending,
                    })?;
                    live_flags.push(true);
                } else {
                    live_flags.push(false);
                }
                workers.push(worker);
            }

            for ((idx, node_cfg), (worker, is_live)) in config
                .nodes
                .iter()
                .enumerate()
                .zip(workers.into_iter().zip(live_flags))
            {
                let last_nk = &last.nodes[idx];
                let meta = if is_live {
                    match worker.recv()? {
                        WorkerResp::Meta(res) => Some(res.with_context(|| {
                            format!("restoring cluster node {}", node_cfg.name)
                        })?),
                        _ => bail!(
                            "unexpected response to Restore from node {}",
                            node_cfg.name
                        ),
                    }
                } else {
                    None
                };
                nodes.push(LiveNode {
                    cfg: node_cfg.clone(),
                    backend: NodeBackend::Worker(worker),
                    meta,
                    cluster_ns: last_nk.cluster_ns,
                    boot_cluster_ns: last_nk.boot_cluster_ns,
                    local_base_ns: last_nk.local_base_ns,
                    boot_count: last_nk.boot_count,
                    step_base: last_nk.step_base,
                    exited: last_nk.exited,
                    last_stop: None,
                    pending_rx: last_nk.pending_rx.iter().cloned().collect(),
                });
            }

            let switch = NetSwitch::restore(config.network.clone(), last.switch.clone());
            return Ok(Cluster {
                config,
                dirty_log,
                nodes,
                switch,
                cluster_ns: last.cluster_ns,
                cluster_step: last.cluster_step,
                min_skew_ns,
            });
        }

        // Restore each live node's Machine (in parallel across threads on Linux
        // when parallel mode is enabled; Machine::restore never runs KVM_RUN or
        // opens thread-bound PMU counters).
        let restore_one = |idx: usize| -> Result<Option<Machine>> {
            let node_cfg = &config.nodes[idx];
            let last_nk = &last.nodes[idx];
            if last_nk.keyframe.is_none() {
                return Ok(None);
            }
            let node_chain = extract_node_chain(chain, idx, last_nk.boot_count);
            let mut m_cfg = node_cfg.config.clone();
            if last_nk.boot_count > 1 {
                m_cfg.seed = reboot_seed(node_cfg.base_seed, last_nk.boot_count);
            }
            let trace = node_traces.get(idx).map_or(&[][..], Vec::as_slice);
            let mut m = Machine::restore(&m_cfg, &node_chain, pages, trace, dirty_log)
                .with_context(|| format!("restoring cluster node {}", node_cfg.name))?;
            m.set_cluster_mode(true);
            m.set_idle(last_nk.idle, last_nk.net_rx_pending);
            Ok(Some(m))
        };

        let machines: Vec<Option<Machine>> = if parallel_workers {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..config.nodes.len())
                    .map(|idx| s.spawn(move || restore_one(idx)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| anyhow!("cluster restore worker panicked"))?
                    })
                    .collect::<Result<Vec<_>>>()
            })?
        } else {
            (0..config.nodes.len())
                .map(restore_one)
                .collect::<Result<Vec<_>>>()?
        };

        for (idx, (node_cfg, machine)) in config.nodes.iter().zip(machines).enumerate() {
            let last_nk = &last.nodes[idx];
            #[cfg(not(target_os = "macos"))]
            let (backend, meta) = if parallel_workers {
                let worker = NodeWorker::spawn(idx, node_cfg.name.clone())?;
                let meta = if let Some(m) = machine {
                    worker.send(WorkerReq::Attach(Box::new(m)))?;
                    match worker.recv()? {
                        WorkerResp::Meta(res) => Some(res?),
                        _ => bail!("unexpected response to Attach from node {}", node_cfg.name),
                    }
                } else {
                    None
                };
                (NodeBackend::Worker(worker), meta)
            } else {
                let meta = machine.as_ref().map(NodeMeta::from_machine);
                (NodeBackend::Local(machine.map(Box::new)), meta)
            };
            #[cfg(target_os = "macos")]
            let (backend, meta) = {
                let meta = machine.as_ref().map(NodeMeta::from_machine);
                (NodeBackend::Local(machine.map(Box::new)), meta)
            };

            nodes.push(LiveNode {
                cfg: node_cfg.clone(),
                backend,
                meta,
                cluster_ns: last_nk.cluster_ns,
                boot_cluster_ns: last_nk.boot_cluster_ns,
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

    /// Rolls the live cluster back in place to the state at the end of `chain`
    /// (full cluster keyframe first, target last).
    ///
    /// Live nodes whose `boot_count` matches the target snapshot and whose VM
    /// has dirty logging enabled roll back in place via
    /// [`Machine::restore_in_place`], rewriting only pages dirtied since the
    /// snapshot without tearing down the VM or respawning worker processes.
    /// Nodes that were crashed or on a different `boot_count` perform a full
    /// restore inside their existing worker.
    pub fn restore_in_place(
        &mut self,
        chain: &[ClusterKeyframe],
        pages: &(dyn Pages + Sync),
        node_traces: &[Vec<(u64, Vec<u8>)>],
    ) -> Result<RestoreStats> {
        let last = chain.last().context("restoring from no cluster keyframes")?;
        if last.nodes.len() != self.nodes.len() {
            bail!(
                "cluster keyframe has {} nodes, cluster has {}",
                last.nodes.len(),
                self.nodes.len()
            );
        }

        enum PendingRestore {
            InPlace,
            Full,
            Crash,
        }

        let mut stats = RestoreStats::default();
        let mut pending: Vec<(usize, PendingRestore)> = Vec::new();

        // Dispatch all worker restores concurrently first.
        for (idx, node) in self.nodes.iter().enumerate() {
            if let NodeBackend::Worker(w) = &node.backend {
                let last_nk = &last.nodes[idx];
                if last_nk.keyframe.is_some() {
                    let node_chain = extract_node_chain(chain, idx, last_nk.boot_count);
                    let mut m_cfg = node.cfg.config.clone();
                    if last_nk.boot_count > 1 {
                        m_cfg.seed = reboot_seed(node.cfg.base_seed, last_nk.boot_count);
                    }
                    let page_source = build_page_source(&node_chain, pages)?;
                    let trace = node_traces.get(idx).cloned().unwrap_or_default();
                    let can_reuse = self.dirty_log == DirtyLog::On
                        && node.meta.is_some()
                        && node.boot_count == last_nk.boot_count;
                    if can_reuse {
                        w.send(WorkerReq::RestoreInPlace {
                            config: Box::new(m_cfg),
                            chain: node_chain,
                            pages: page_source,
                            trace,
                            dirty_log: self.dirty_log,
                            idle: last_nk.idle,
                            net_rx_pending: last_nk.net_rx_pending,
                        })?;
                        pending.push((idx, PendingRestore::InPlace));
                    } else {
                        w.send(WorkerReq::Restore {
                            config: Box::new(m_cfg),
                            chain: node_chain,
                            pages: page_source,
                            trace,
                            dirty_log: self.dirty_log,
                            idle: last_nk.idle,
                            net_rx_pending: last_nk.net_rx_pending,
                        })?;
                        pending.push((idx, PendingRestore::Full));
                    }
                } else {
                    w.send(WorkerReq::Crash)?;
                    pending.push((idx, PendingRestore::Crash));
                }
            }
        }

        // Handle inline (Local) nodes while workers run concurrently.
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            let NodeBackend::Local(opt) = &mut node.backend else {
                continue;
            };
            let last_nk = &last.nodes[idx];
            if last_nk.keyframe.is_some() {
                let node_chain = extract_node_chain(chain, idx, last_nk.boot_count);
                let mut m_cfg = node.cfg.config.clone();
                if last_nk.boot_count > 1 {
                    m_cfg.seed = reboot_seed(node.cfg.base_seed, last_nk.boot_count);
                }
                let trace = node_traces.get(idx).map_or(&[][..], Vec::as_slice);
                let can_reuse = self.dirty_log == DirtyLog::On
                    && node.boot_count == last_nk.boot_count
                    && opt.as_ref().is_some_and(|m| m.can_restore_in_place());
                if can_reuse {
                    let m = opt.as_mut().expect("checked live above");
                    let chain_refs: Vec<&Keyframe> = node_chain.iter().collect();
                    let rewritten = m
                        .restore_in_place_count(&chain_refs, pages, trace)
                        .with_context(|| {
                            format!("restoring cluster node {} in place", node.cfg.name)
                        })?;
                    m.set_cluster_mode(true);
                    m.set_idle(last_nk.idle, last_nk.net_rx_pending);
                    node.meta = Some(NodeMeta::from_machine(m));
                    stats.in_place_nodes += 1;
                    stats.pages_rewritten += rewritten;
                } else {
                    *opt = None;
                    let mut m = Machine::restore(&m_cfg, &node_chain, pages, trace, self.dirty_log)
                        .with_context(|| format!("restoring cluster node {}", node.cfg.name))?;
                    m.set_cluster_mode(true);
                    m.set_idle(last_nk.idle, last_nk.net_rx_pending);
                    node.meta = Some(NodeMeta::from_machine(&m));
                    *opt = Some(Box::new(m));
                    stats.full_restore_nodes += 1;
                }
            } else {
                *opt = None;
                node.meta = None;
                stats.crashed_nodes += 1;
            }
        }

        // Collect all worker replies.
        for (idx, kind) in pending {
            let node = &mut self.nodes[idx];
            let NodeBackend::Worker(w) = &node.backend else {
                unreachable!("pending entry is always a worker");
            };
            match kind {
                PendingRestore::InPlace => match w.recv()? {
                    WorkerResp::RestoredInPlace(res) => {
                        let (meta, used_in_place, rewritten) = res.with_context(|| {
                            format!("restoring cluster node {} in place", node.cfg.name)
                        })?;
                        node.meta = Some(meta);
                        if used_in_place {
                            stats.in_place_nodes += 1;
                            stats.pages_rewritten += rewritten;
                        } else {
                            stats.full_restore_nodes += 1;
                        }
                    }
                    _ => bail!(
                        "unexpected response to RestoreInPlace from node {}",
                        node.cfg.name
                    ),
                },
                PendingRestore::Full => match w.recv()? {
                    WorkerResp::Meta(res) => {
                        let meta = res.with_context(|| {
                            format!("restoring cluster node {}", node.cfg.name)
                        })?;
                        node.meta = Some(meta);
                        stats.full_restore_nodes += 1;
                    }
                    _ => bail!("unexpected response to Restore from node {}", node.cfg.name),
                },
                PendingRestore::Crash => match w.recv()? {
                    WorkerResp::Crashed(_) => {
                        node.meta = None;
                        stats.crashed_nodes += 1;
                    }
                    _ => bail!("unexpected response to Crash from node {}", node.cfg.name),
                },
            }
        }

        // Update cluster-level metadata on every node and restore the switch.
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            let last_nk = &last.nodes[idx];
            node.cluster_ns = last_nk.cluster_ns;
            node.boot_cluster_ns = last_nk.boot_cluster_ns;
            node.local_base_ns = last_nk.local_base_ns;
            node.boot_count = last_nk.boot_count;
            node.step_base = last_nk.step_base;
            node.exited = last_nk.exited;
            node.last_stop = None;
            node.pending_rx = last_nk.pending_rx.iter().cloned().collect();
        }
        self.switch = NetSwitch::restore(self.config.network.clone(), last.switch.clone());
        self.cluster_ns = last.cluster_ns;
        self.cluster_step = last.cluster_step;
        Ok(stats)
    }

    /// Captures a synchronized keyframe of the entire cluster (every live node
    /// and the virtual switch). In parallel mode, workers capture dirty pages
    /// and VM state concurrently; when `pages` is backed by a `rewind-store`
    /// directory, workers write pages directly to the store in parallel without
    /// copying raw page bytes over IPC.
    pub fn keyframe(
        &mut self,
        pages: &mut dyn Pages,
        parent: Option<u64>,
        node_parents: &[Option<u64>],
    ) -> Result<ClusterKeyframe> {
        let store_path = pages.store_path().map(Path::to_path_buf);

        let mut node_kfs_opt: Vec<Option<Keyframe>> = vec![None; self.nodes.len()];

        if let Some(store_path) = store_path {
            // Fast path: every worker hashes, compresses, and writes its own
            // pages directly into the shared rewind-store in parallel.
            let mut worker_indices = Vec::new();
            for (idx, node) in self.nodes.iter().enumerate() {
                if let NodeBackend::Worker(w) = &node.backend {
                    let np = node_parents.get(idx).copied().flatten();
                    w.send(WorkerReq::KeyframeDirect {
                        parent: np,
                        store_path: store_path.clone(),
                    })?;
                    worker_indices.push(idx);
                }
            }
            for (idx, node) in self.nodes.iter_mut().enumerate() {
                if let NodeBackend::Local(Some(m)) = &mut node.backend {
                    let np = node_parents.get(idx).copied().flatten();
                    node_kfs_opt[idx] = Some(m.keyframe(pages, np)?);
                }
            }
            for idx in worker_indices {
                if let NodeBackend::Worker(w) = &self.nodes[idx].backend {
                    match w.recv()? {
                        WorkerResp::Keyframe(res) => {
                            node_kfs_opt[idx] = (*res)?;
                        }
                        _ => bail!(
                            "unexpected response to KeyframeDirect from node {}",
                            self.nodes[idx].cfg.name
                        ),
                    }
                }
            }
        } else {
            // In-memory Pages fallback (e.g. unit tests with MemPages).
            let mut worker_indices = Vec::new();
            for (idx, node) in self.nodes.iter().enumerate() {
                if let NodeBackend::Worker(w) = &node.backend {
                    let np = node_parents.get(idx).copied().flatten();
                    w.send(WorkerReq::PrepareKeyframe { parent: np })?;
                    worker_indices.push(idx);
                }
            }
            for (idx, node) in self.nodes.iter_mut().enumerate() {
                if let NodeBackend::Local(Some(m)) = &mut node.backend {
                    let np = node_parents.get(idx).copied().flatten();
                    node_kfs_opt[idx] = Some(m.keyframe(pages, np)?);
                }
            }
            let mut origin_acks = Vec::new();
            for idx in worker_indices {
                if let NodeBackend::Worker(w) = &self.nodes[idx].backend {
                    let prep = match w.recv()? {
                        WorkerResp::PreparedKeyframe(res) => (*res)?,
                        _ => bail!(
                            "unexpected response to PrepareKeyframe from node {}",
                            self.nodes[idx].cfg.name
                        ),
                    };
                    if let Some(p) = prep {
                        let is_full = p.parent.is_none();
                        let kf = finish_keyframe(p, pages)?;
                        if is_full && self.dirty_log == DirtyLog::On {
                            w.send(WorkerReq::SetKeyframeOrigin(kf.pages.clone()))?;
                            origin_acks.push(idx);
                        }
                        node_kfs_opt[idx] = Some(kf);
                    }
                }
            }
            for idx in origin_acks {
                if let NodeBackend::Worker(w) = &self.nodes[idx].backend {
                    match w.recv()? {
                        WorkerResp::Unit => {}
                        _ => bail!(
                            "unexpected response to SetKeyframeOrigin from node {}",
                            self.nodes[idx].cfg.name
                        ),
                    }
                }
            }
        }

        let mut node_kfs = Vec::with_capacity(self.nodes.len());
        for (node, kf) in self.nodes.iter().zip(node_kfs_opt) {
            node_kfs.push(ClusterNodeKeyframe {
                keyframe: kf,
                idle: node.meta.as_ref().is_some_and(|m| m.is_idle),
                net_rx_pending: node.meta.as_ref().is_some_and(|m| m.net_rx_pending),
                cluster_ns: node.cluster_ns,
                boot_cluster_ns: node.boot_cluster_ns,
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
        self.nodes.get_mut(node)?.ensure_local_machine_mut()
    }

    /// Clones each live node's in-memory trace encoders (including unflushed
    /// switch batches) for interactive session snapshots.
    pub fn node_encoders(&self) -> Vec<NodeEncoders> {
        self.nodes.iter().map(LiveNode::encoders).collect()
    }

    /// Restores each live node's in-memory trace encoders after
    /// [`Cluster::restore`] on an in-progress interactive snapshot.
    pub fn set_node_encoders(&mut self, encoders: &[NodeEncoders]) {
        for (node, enc) in self.nodes.iter_mut().zip(encoders.iter()) {
            if let Some((io, switches)) = enc {
                node.set_encoders(io.clone(), switches.clone());
            }
        }
    }

    /// Immediately applies an interactive fault (partition, heal, set_link,
    /// crash, restart) at the cluster's current `(cluster_ns, cluster_step)`
    /// and bumps `cluster_step` so each fault has a unique deterministic point.
    pub fn apply_fault_now(
        &mut self,
        kind: &FaultKind,
        obs: &mut dyn ClusterObserver,
    ) -> Result<()> {
        let mut net_events = Vec::new();
        let at_ns = self.cluster_ns;
        let actions = self
            .switch
            .apply_immediate_fault(at_ns, kind, &mut net_events);
        self.flush_net_events(&mut net_events, obs);
        for action in actions {
            self.apply_fault_action(action, at_ns, &mut net_events)?;
        }
        self.flush_net_events(&mut net_events, obs);
        self.cluster_step += 1;
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
        let (kf, mem_pages, enc) = match &mut live.backend {
            NodeBackend::Local(opt) => {
                let m = opt
                    .as_mut()
                    .with_context(|| format!("node {} is currently crashed", live.cfg.name))?;
                let enc = m.encoders();
                let mut mem_pages = MemPages::default();
                let kf = m.snapshot_non_destructive(&mut mem_pages)?;
                (kf, mem_pages, Some(enc))
            }
            NodeBackend::Worker(w) => {
                if live.meta.is_none() {
                    bail!("node {} is currently crashed", live.cfg.name);
                }
                w.send(WorkerReq::SnapshotNonDestructive)?;
                match w.recv()? {
                    WorkerResp::NonDestructive(res) => (*res)?,
                    _ => bail!(
                        "unexpected response to SnapshotNonDestructive from node {}",
                        live.cfg.name
                    ),
                }
            }
        };
        let step = kf.step;
        let mut cfg = live.cfg.config.clone();
        if live.boot_count > 1 {
            cfg.seed = reboot_seed(live.cfg.base_seed, live.boot_count);
        }
        let mut cloned = Machine::restore(&cfg, &[kf], &mem_pages, node_trace, DirtyLog::Off)?;
        if let Some((io, switches)) = enc {
            cloned.set_encoders(io, switches);
        }
        Ok((cloned, step))
    }

    /// Takes ownership of one node's live [`Machine`] (for `rewind cat`,
    /// `files`, `shell`, or `gdb` after seeking the cluster to a step).
    pub fn take_node_machine(&mut self, node: usize) -> Result<Machine> {
        let live = self
            .nodes
            .get_mut(node)
            .with_context(|| format!("node index {node} out of range"))?;
        let mut m = live.take_machine()?;
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
        let n = self.nodes.len();

        loop {
            if obs.stop()
                || (0..n).any(|i| {
                    obs.node_step_limit(i)
                        .is_some_and(|limit| self.nodes[i].total_steps() >= limit)
                })
            {
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
                && self
                    .config
                    .max_virtual_ns
                    .is_none_or(|cap| self.cluster_ns >= cap)
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

            // 1. Fire at most one due step-pinned interactive fault at
            //    `(cluster_ns, cluster_step)` so each pinned fault bumps
            //    `cluster_step` identically to `apply_fault_now`.
            if let Some((fault_ns, actions)) = self.switch.fire_next_pinned_fault(
                self.cluster_ns,
                self.cluster_step,
                &mut net_events,
            ) {
                self.flush_net_events(&mut net_events, obs);
                for action in actions {
                    self.apply_fault_action(action, fault_ns, &mut net_events)?;
                }
                self.flush_net_events(&mut net_events, obs);
                self.cluster_step += 1;
                continue;
            }

            // 2. Check whether any node is currently runnable.
            let min_runnable = self
                .nodes
                .iter()
                .filter(|node| node.is_runnable())
                .map(|node| node.cluster_ns)
                .min();

            let Some(min_r) = min_runnable else {
                // No node is runnable: jump directly to the earliest upcoming
                // timer, in-flight packet, timed fault, or ready pinned fault.
                let next_timer = self
                    .nodes
                    .iter()
                    .filter(|node| node.is_alive())
                    .filter_map(LiveNode::next_timer_cluster_ns)
                    .min();
                let next_rx = self.switch.next_delivery_ns();
                let next_timed_fault = self.switch.next_timed_fault_ns();
                let next_pinned_fault = self.switch.next_ready_pinned_fault_ns(self.cluster_step);

                let next_wake = [next_timer, next_rx, next_timed_fault, next_pinned_fault]
                    .into_iter()
                    .flatten()
                    .min();

                let Some(wake_ns) = next_wake else {
                    let stop = if self.nodes.iter().all(|node| node.exited) {
                        ClusterStop::Completed
                    } else {
                        ClusterStop::Stalled
                    };
                    return Ok(self.outcome(stop));
                };

                if self.config.stop_at_step.is_none()
                    && let Some(cap) = self.config.max_virtual_ns
                    && wake_ns > cap
                {
                    self.cluster_ns = self.cluster_ns.max(cap);
                    return Ok(self.outcome(ClusterStop::MaxVirtualTime));
                }
                if let Some(target) = until_ns
                    && wake_ns > target
                {
                    self.cluster_ns = self.cluster_ns.max(target);
                    return Ok(self.outcome(ClusterStop::Paused));
                }

                let jump_to = wake_ns.max(self.cluster_ns);
                self.cluster_ns = jump_to;

                // If a pinned interactive fault is now due, let step 1 fire it
                // before waking timers or delivering frames.
                if self
                    .switch
                    .has_due_pinned_fault(self.cluster_ns, self.cluster_step)
                {
                    continue;
                }

                // Fire timed faults / nemesis ticks due at <= jump_to.
                let fault_actions = self.switch.fire_due_timed_faults(jump_to, &mut net_events);
                let fired_timed = !net_events.is_empty() || !fault_actions.is_empty();
                self.flush_net_events(&mut net_events, obs);
                for (at_ns, action) in fault_actions {
                    self.apply_fault_action(action, at_ns, &mut net_events)?;
                }
                self.flush_net_events(&mut net_events, obs);
                if fired_timed {
                    self.cluster_step += 1;
                    continue;
                }

                // Wake any alive idle nodes whose timer is due at <= jump_to.
                for idx in 0..n {
                    if self.nodes[idx].is_alive()
                        && !self.nodes[idx].is_runnable()
                        && let Some(timer_ns) = self.nodes[idx].next_timer_cluster_ns()
                        && timer_ns <= jump_to
                    {
                        let wake = timer_ns.max(self.nodes[idx].cluster_ns);
                        self.advance_idle_node_to(idx, wake)?;
                    }
                }

                // Deliver (or drop for dead/partitioned nodes) all in-flight
                // frames due at <= jump_to.
                self.deliver_due_frames_all(jump_to, &mut net_events)?;
                self.flush_net_events(&mut net_events, obs);
                continue;
            };

            // 3. At least one node is runnable. Compute GVT across runnable
            //    nodes, alive idle timers, and in-flight frames.
            let min_idle_timer = self
                .nodes
                .iter()
                .filter(|node| node.is_alive() && !node.is_runnable())
                .filter_map(LiveNode::next_timer_cluster_ns)
                .min();
            let min_rx = self.switch.next_delivery_ns();
            let gvt = [Some(min_r), min_idle_timer, min_rx]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(min_r);

            if self.config.stop_at_step.is_none()
                && let Some(cap) = self.config.max_virtual_ns
                && gvt >= cap
            {
                self.cluster_ns = self.cluster_ns.max(cap);
                return Ok(self.outcome(ClusterStop::MaxVirtualTime));
            }
            self.cluster_ns = self.cluster_ns.max(gvt);
            if let Some(target) = until_ns
                && self.cluster_ns >= target
            {
                return Ok(self.outcome(ClusterStop::Paused));
            }
            if self
                .switch
                .has_due_pinned_fault(self.cluster_ns, self.cluster_step)
            {
                continue;
            }

            // 3a. Fire timed/nemesis faults due at <= gvt.
            if self
                .switch
                .next_timed_fault_ns()
                .is_some_and(|t| t <= gvt)
            {
                let fault_actions = self.switch.fire_due_timed_faults(gvt, &mut net_events);
                self.flush_net_events(&mut net_events, obs);
                for (at_ns, action) in fault_actions {
                    self.apply_fault_action(action, at_ns, &mut net_events)?;
                }
                self.flush_net_events(&mut net_events, obs);
                self.cluster_step += 1;
                continue;
            }

            // 3b. Purge in-flight frames addressed to dead (!is_alive) nodes
            //     whose delivery timestamp <= gvt so they cannot pin GVT.
            for idx in 0..n {
                if !self.nodes[idx].is_alive() {
                    let due = self.switch.pop_due_frames_for(idx, gvt);
                    for frame in due {
                        self.switch.note_dropped(
                            frame.deliver_at_ns,
                            &frame,
                            DropReason::NodeCrashed,
                            &mut net_events,
                        );
                    }
                }
            }
            self.flush_net_events(&mut net_events, obs);

            let cap = if self.config.stop_at_step.is_none() {
                self.config.max_virtual_ns.unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            let unconstrained_window = self
                .switch
                .max_link_latency_ns()
                .max(self.config.slice_ns)
                .max(UNCONSTRAINED_WINDOW_NS);
            let max_frontier = gvt.saturating_add(unconstrained_window);

            // 3c. Perform causally-safe local wakes and deliveries (decoupled
            //     across partitions within `max_frontier`) until fixed point.
            loop {
                let (_earliest_tx, incoming_bound) = self.compute_lookahead();
                let next_fault = self.switch.next_timed_fault_ns().unwrap_or(u64::MAX);
                let mut woke_any = false;

                for (idx, &bound) in incoming_bound.iter().enumerate() {
                    if !self.nodes[idx].is_alive() {
                        continue;
                    }
                    let local_safe = bound.min(next_fault).min(max_frontier);
                    if self.nodes[idx].is_runnable() {
                        let deliver_up_to = self.nodes[idx].cluster_ns.min(local_safe);
                        self.deliver_due_frames_for(idx, deliver_up_to, &mut net_events)?;
                        self.drain_pending_rx(idx, &mut net_events)?;
                    } else {
                        let t_timer = self.nodes[idx].next_timer_cluster_ns();
                        let t_rx = self.switch.next_delivery_for(idx);
                        let wake_ev = [t_timer, t_rx].into_iter().flatten().min();
                        if let Some(w) = wake_ev
                            && w <= local_safe
                            && w <= cap
                        {
                            if t_timer.is_some_and(|t| t <= w) {
                                let wake = w.max(self.nodes[idx].cluster_ns);
                                self.advance_idle_node_to(idx, wake)?;
                            }
                            self.deliver_due_frames_for(idx, w, &mut net_events)?;
                            self.drain_pending_rx(idx, &mut net_events)?;
                            if self.nodes[idx].is_runnable() {
                                woke_any = true;
                            }
                        }
                    }
                }
                self.flush_net_events(&mut net_events, obs);
                if !woke_any {
                    break;
                }
            }

            // 3d. Compute final safe lookahead horizons for all runnable nodes.
            let (_earliest_tx, incoming_bound) = self.compute_lookahead();
            let next_fault = self.switch.next_timed_fault_ns().unwrap_or(u64::MAX);

            let mut batch: Vec<(usize, u64, u64, bool)> = Vec::new();
            for (idx, &bound) in incoming_bound.iter().enumerate() {
                if !self.nodes[idx].is_runnable() {
                    continue;
                }
                let horizon_cns = bound
                    .min(self.switch.next_delivery_for(idx).unwrap_or(u64::MAX))
                    .min(next_fault)
                    .min(cap)
                    .min(max_frontier);
                if horizon_cns <= self.nodes[idx].cluster_ns {
                    continue;
                }
                let mut max_steps = MAX_STEPS_PER_SLICE;
                if let Some(limit) = obs.node_step_limit(idx) {
                    let rem = limit.saturating_sub(self.nodes[idx].total_steps());
                    if rem == 0 {
                        continue;
                    }
                    max_steps = max_steps.min(rem);
                }

                let before_local = self.nodes[idx].local_now();
                let target_local = self.nodes[idx].cluster_to_local_ns(horizon_cns);
                // Guarantee at least 1 ns local advance so negative drift never
                // creates a zero-progress slice.
                let horizon_local = target_local.max(before_local.saturating_add(1));
                let allow_idle_skip = self.nodes[idx].pending_rx.is_empty();
                batch.push((idx, horizon_local, max_steps, allow_idle_skip));
            }

            if batch.is_empty() {
                if obs.stop() {
                    return Ok(self.outcome(ClusterStop::Paused));
                }
                if self.config.stop_at_step.is_none()
                    && self.config.max_virtual_ns.is_some_and(|c| {
                        self.nodes
                            .iter()
                            .filter(|node| node.is_runnable())
                            .all(|node| node.cluster_ns >= c)
                    })
                {
                    return Ok(self.outcome(ClusterStop::MaxVirtualTime));
                }
                bail!(
                    "cluster scheduler made no progress at cluster_ns={} step={}",
                    self.cluster_ns,
                    self.cluster_step
                );
            }

            // 4. Execute all eligible nodes in `batch` (in parallel on their
            //    dedicated worker threads when workers are enabled).
            let replies = self.execute_batch(&batch, deadline)?;

            // Check for wall-clock deadline expiration or external SIGTERM
            // interruption before committing partial rounds.
            let any_interrupted = replies
                .iter()
                .any(|(_, r)| r.outcome == SliceOutcome::Interrupted);
            if any_interrupted {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Ok(self.outcome(ClusterStop::TimedOut));
                }
                return Ok(self.outcome(ClusterStop::Paused));
            }

            // 5. Commit round outputs in strict deterministic order.
            let mut all_tx: Vec<(u64, usize, usize, Vec<u8>)> = Vec::new();
            let mut made_progress = false;

            for (idx, reply) in replies {
                let prev_steps = self.nodes[idx].total_steps();
                let prev_local = self.nodes[idx].local_now();
                let prev_idle = self.nodes[idx].meta.as_ref().is_some_and(|m| m.is_idle);

                for (step, rec) in &reply.events.records {
                    obs.node_record(idx, *step, rec);
                }
                for (step, byte) in &reply.events.serial {
                    obs.node_serial(idx, *step, *byte);
                }

                let after_local = reply.meta.now;
                let after_idle = reply.meta.is_idle;
                self.nodes[idx].meta = Some(reply.meta);
                let new_cns = self.nodes[idx].local_to_cluster_ns(after_local);
                self.nodes[idx].cluster_ns = self.nodes[idx].cluster_ns.max(new_cns);

                if self.nodes[idx].total_steps() > prev_steps
                    || after_local > prev_local
                    || after_idle != prev_idle
                    || matches!(reply.outcome, SliceOutcome::Stopped(_))
                {
                    made_progress = true;
                }

                for (seq, (tx_local_ns, frame)) in reply.tx_frames.into_iter().enumerate() {
                    let tx_cns = self.nodes[idx].local_to_cluster_ns(tx_local_ns);
                    all_tx.push((tx_cns, idx, seq, frame));
                }

                // Drain queued RX frames for EVERY node that ran — including
                // nodes that freed ring slots and went idle in the same slice.
                self.drain_pending_rx(idx, &mut net_events)?;

                if let SliceOutcome::Stopped(stop) = reply.outcome {
                    self.nodes[idx].exited = true;
                    self.nodes[idx].last_stop = Some(stop);
                }
            }

            // Sort collected TX frames canonically by `(tx_cluster_ns, src, seq)`
            // before touching switch state or emitting `NetEvent`s.
            all_tx.sort_by_key(|&(tx_cns, src, seq, _)| (tx_cns, src, seq));
            for (tx_cns, src, _seq, frame) in all_tx {
                obs.net_tx_frame(tx_cns, src, &frame);
                self.switch.on_tx(tx_cns, src, frame, &mut net_events);
            }
            self.flush_net_events(&mut net_events, obs);

            // Advance monotone cluster_ns to the post-round GVT floor.
            self.advance_cluster_ns_after_round();
            self.cluster_step += 1;

            if !made_progress {
                bail!(
                    "cluster round made no progress at cluster_ns={} step={}",
                    self.cluster_ns,
                    self.cluster_step
                );
            }
        }
    }

    fn execute_batch(
        &mut self,
        batch: &[(usize, u64, u64, bool)],
        deadline: Option<Instant>,
    ) -> Result<Vec<(usize, SliceReply)>> {
        let mut results: HashMap<usize, SliceReply> = HashMap::with_capacity(batch.len());

        if self.config.parallel {
            // Dispatch all worker-backed nodes concurrently first.
            let mut dispatched_workers: Vec<usize> = Vec::new();
            for &(idx, horizon_local, max_steps, allow_idle_skip) in batch {
                let step_base = self.nodes[idx].step_base;
                if let NodeBackend::Worker(w) = &self.nodes[idx].backend {
                    w.send(WorkerReq::RunSlice {
                        horizon_local,
                        max_steps,
                        step_base,
                        allow_idle_skip,
                    })?;
                    dispatched_workers.push(idx);
                }
            }

            // Run any local-backed nodes inline.
            for &(idx, horizon_local, max_steps, allow_idle_skip) in batch {
                let step_base = self.nodes[idx].step_base;
                if let NodeBackend::Local(opt) = &mut self.nodes[idx].backend {
                    let m = opt.as_mut().expect("runnable node has a live Machine");
                    let mut buf = BufferingObserver {
                        step_base,
                        events: SliceEvents::default(),
                    };
                    let outcome = SliceOutcome::from_outcome(m.run_cluster_slice(
                        horizon_local,
                        max_steps,
                        allow_idle_skip,
                        &mut buf,
                    )?)?;
                    let tx_frames = m.take_tx_frames();
                    let meta = NodeMeta::from_machine(m);
                    results.insert(
                        idx,
                        SliceReply {
                            outcome,
                            meta,
                            events: buf.events,
                            tx_frames,
                        },
                    );
                }
            }

            // Collect all worker replies, interrupting active workers if `deadline` expires.
            let mut timed_out = false;
            for &idx in &dispatched_workers {
                let w = match &self.nodes[idx].backend {
                    NodeBackend::Worker(w) => w,
                    NodeBackend::Local(_) => unreachable!(),
                };
                let resp = if let Some(d) = deadline
                    && !timed_out
                {
                    let rem = d.saturating_duration_since(Instant::now());
                    match w.recv_timeout(rem) {
                        Ok(r) => r,
                        Err(RecvTimeoutError::Timeout) => {
                            timed_out = true;
                            for &widx in &dispatched_workers {
                                if let NodeBackend::Worker(worker) = &self.nodes[widx].backend {
                                    worker.interrupt();
                                }
                            }
                            w.recv()?
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            bail!(
                                "cluster worker for node {} panicked or disconnected",
                                self.nodes[idx].cfg.name
                            );
                        }
                    }
                } else {
                    w.recv()?
                };
                let reply = match resp {
                    WorkerResp::Slice(res) => (*res)?,
                    _ => bail!(
                        "unexpected response to RunSlice from node {}",
                        self.nodes[idx].cfg.name
                    ),
                };
                results.insert(idx, reply);
            }
        } else {
            // Sequential execution: run one node at a time even when nodes are
            // isolated in worker processes (macOS HVF).
            for &(idx, horizon_local, max_steps, allow_idle_skip) in batch {
                let step_base = self.nodes[idx].step_base;
                let reply = match &mut self.nodes[idx].backend {
                    NodeBackend::Local(opt) => {
                        let m = opt.as_mut().expect("runnable node has a live Machine");
                        let mut buf = BufferingObserver {
                            step_base,
                            events: SliceEvents::default(),
                        };
                        let outcome = SliceOutcome::from_outcome(m.run_cluster_slice(
                            horizon_local,
                            max_steps,
                            allow_idle_skip,
                            &mut buf,
                        )?)?;
                        let tx_frames = m.take_tx_frames();
                        let meta = NodeMeta::from_machine(m);
                        SliceReply {
                            outcome,
                            meta,
                            events: buf.events,
                            tx_frames,
                        }
                    }
                    NodeBackend::Worker(w) => {
                        w.send(WorkerReq::RunSlice {
                            horizon_local,
                            max_steps,
                            step_base,
                            allow_idle_skip,
                        })?;
                        let resp = if let Some(d) = deadline {
                            let rem = d.saturating_duration_since(Instant::now());
                            match w.recv_timeout(rem) {
                                Ok(r) => r,
                                Err(RecvTimeoutError::Timeout) => {
                                    w.interrupt();
                                    w.recv()?
                                }
                                Err(RecvTimeoutError::Disconnected) => {
                                    bail!(
                                        "cluster worker for node {} panicked or disconnected",
                                        self.nodes[idx].cfg.name
                                    );
                                }
                            }
                        } else {
                            w.recv()?
                        };
                        match resp {
                            WorkerResp::Slice(res) => (*res)?,
                            _ => bail!(
                                "unexpected response to RunSlice from node {}",
                                self.nodes[idx].cfg.name
                            ),
                        }
                    }
                };
                let interrupted = reply.outcome == SliceOutcome::Interrupted;
                results.insert(idx, reply);
                if interrupted {
                    break;
                }
            }
        }

        // Return replies in strict `batch` (ascending node index) order.
        let mut ordered = Vec::with_capacity(results.len());
        for &(idx, _, _, _) in batch {
            if let Some(reply) = results.remove(&idx) {
                ordered.push((idx, reply));
            }
        }
        Ok(ordered)
    }

    fn compute_lookahead(&self) -> (Vec<u64>, Vec<u64>) {
        let states: Vec<NodeExecState> = self
            .nodes
            .iter()
            .map(|node| {
                if node.is_runnable() {
                    NodeExecState::Runnable {
                        cluster_ns: node.cluster_ns,
                    }
                } else if node.is_alive() {
                    NodeExecState::Idle {
                        next_timer_ns: node.next_timer_cluster_ns(),
                    }
                } else {
                    NodeExecState::Dead
                }
            })
            .collect();
        let next_rx: Vec<Option<u64>> = (0..self.nodes.len())
            .map(|i| self.switch.next_delivery_for(i))
            .collect();
        compute_lookahead_from(
            &states,
            &next_rx,
            |a, b| self.switch.is_blocked(a, b),
            |a, b| self.switch.link_min_latency_ns(a, b),
        )
    }

    fn advance_cluster_ns_after_round(&mut self) {
        let min_runnable = self
            .nodes
            .iter()
            .filter(|node| node.is_runnable())
            .map(|node| node.cluster_ns)
            .min();
        let min_idle_timer = self
            .nodes
            .iter()
            .filter(|node| node.is_alive() && !node.is_runnable())
            .filter_map(LiveNode::next_timer_cluster_ns)
            .min();
        let min_rx = self.switch.next_delivery_ns();

        let next_ns = if let Some(min_r) = min_runnable {
            [Some(min_r), min_idle_timer, min_rx]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(min_r)
        } else {
            let max_node = self
                .nodes
                .iter()
                .map(|node| node.cluster_ns)
                .max()
                .unwrap_or(0);
            let floor = [min_idle_timer, min_rx]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(u64::MAX);
            max_node.min(floor)
        };
        self.cluster_ns = self.cluster_ns.max(next_ns);
    }

    fn advance_idle_node_to(&mut self, idx: usize, wake_cluster_ns: u64) -> Result<()> {
        let wake = wake_cluster_ns.max(self.nodes[idx].cluster_ns);
        let mut target_local = self.nodes[idx].cluster_to_local_ns(wake);
        if let Some(deadline_local) = self.nodes[idx].timer_deadline()
            && self.nodes[idx].local_deadline_to_cluster_ns(deadline_local) <= wake
        {
            target_local = target_local.max(deadline_local);
        }
        self.nodes[idx].cluster_ns = wake;
        self.nodes[idx].advance_idle_to(target_local)?;
        Ok(())
    }

    fn apply_fault_action(
        &mut self,
        action: FaultAction,
        at_ns: u64,
        events: &mut Vec<NetEvent>,
    ) -> Result<()> {
        match action {
            FaultAction::Crash(idx) => {
                if let Some(node) = self.nodes.get_mut(idx) {
                    node.crash()?;
                }
            }
            FaultAction::Restart(idx) => {
                let boot_ns = at_ns.max(self.cluster_ns);
                let min_skew_ns = self.min_skew_ns;
                let dirty_log = self.dirty_log;
                if let Some(node) = self.nodes.get_mut(idx) {
                    node.boot_count += 1;
                    let mut cfg = node.cfg.config.clone();
                    cfg.seed = reboot_seed(node.cfg.base_seed, node.boot_count);
                    // Advance wall-clock epoch to reflect elapsed cluster time.
                    cfg.epoch = cfg.epoch.saturating_add(boot_ns / 1_000_000_000);
                    let initial_local = (node.cfg.clock_skew_ns - min_skew_ns).max(0) as u64;
                    node.boot_fresh(cfg, dirty_log, boot_ns, initial_local)?;
                    events.push(NetEvent::NodeRestart {
                        ns: boot_ns,
                        node: idx,
                        boot_count: node.boot_count,
                    });
                }
            }
        }
        Ok(())
    }

    fn deliver_due_frames_all(&mut self, up_to_ns: u64, events: &mut Vec<NetEvent>) -> Result<()> {
        let due = self.switch.pop_due_frames(up_to_ns);
        for frame in due {
            self.deliver_single_frame(frame, events)?;
        }
        Ok(())
    }

    fn deliver_due_frames_for(
        &mut self,
        dst: usize,
        up_to_ns: u64,
        events: &mut Vec<NetEvent>,
    ) -> Result<()> {
        let due = self.switch.pop_due_frames_for(dst, up_to_ns);
        for frame in due {
            self.deliver_single_frame(frame, events)?;
        }
        Ok(())
    }

    fn deliver_single_frame(
        &mut self,
        frame: InFlightFrame,
        events: &mut Vec<NetEvent>,
    ) -> Result<()> {
        let dst = frame.dst;
        if self.switch.is_crashed(dst) || !self.nodes[dst].is_alive() {
            self.switch
                .note_dropped(frame.deliver_at_ns, &frame, DropReason::NodeCrashed, events);
            return Ok(());
        }
        if self.switch.is_blocked(frame.src, dst) {
            self.switch
                .note_dropped(frame.deliver_at_ns, &frame, DropReason::Partitioned, events);
            return Ok(());
        }
        // Bring idle destination node's clock up to the packet arrival time
        // before delivering the RX interrupt.
        if !self.nodes[dst].is_runnable() && self.nodes[dst].cluster_ns < frame.deliver_at_ns {
            self.advance_idle_node_to(dst, frame.deliver_at_ns)?;
        }

        // Preserve strict FIFO ordering when older frames are queued in pending_rx.
        self.drain_pending_rx(dst, events)?;
        let delivered_at = frame.deliver_at_ns.max(self.nodes[dst].cluster_ns);
        if !self.nodes[dst].pending_rx.is_empty() {
            if self.nodes[dst].pending_rx.len() < 512 {
                self.nodes[dst].pending_rx.push_back(frame);
            } else {
                self.switch
                    .note_dropped(delivered_at, &frame, DropReason::RxOverflow, events);
            }
            return Ok(());
        }

        let delivered = self.nodes[dst].deliver_rx_frame(&frame.frame)?;
        if delivered {
            self.switch.note_delivered_at(delivered_at, &frame, events);
        } else if self.nodes[dst].pending_rx.len() < 512 {
            self.nodes[dst].pending_rx.push_back(frame);
        } else {
            self.switch
                .note_dropped(delivered_at, &frame, DropReason::RxOverflow, events);
        }
        Ok(())
    }

    fn drain_pending_rx(&mut self, idx: usize, events: &mut Vec<NetEvent>) -> Result<()> {
        while let Some(front) = self.nodes[idx].pending_rx.front().cloned() {
            let delivered_at = front.deliver_at_ns.max(self.nodes[idx].cluster_ns);
            let delivered = self.nodes[idx].deliver_rx_frame(&front.frame)?;
            if delivered {
                self.nodes[idx].pending_rx.pop_front();
                self.switch.note_delivered_at(delivered_at, &front, events);
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
#[derive(Debug, Default, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookahead_respects_per_link_latencies_and_partitions() {
        // Node 0 & 1 connected with 2ms latency; Node 2 symmetrically
        // partitioned from both.
        let states = [
            NodeExecState::Runnable {
                cluster_ns: 10_000_000,
            },
            NodeExecState::Runnable {
                cluster_ns: 11_000_000,
            },
            NodeExecState::Runnable {
                cluster_ns: 1_000_000,
            },
        ];
        let next_rx = [None, None, None];
        let is_blocked = |a: usize, b: usize| (a == 2) != (b == 2);
        let lat = |a: usize, b: usize| {
            if a == 0 && b == 1 {
                2_000_000
            } else {
                3_000_000
            }
        };

        let (et, bound) = compute_lookahead_from(&states, &next_rx, is_blocked, lat);
        assert_eq!(et, [10_000_000, 11_000_000, 1_000_000]);
        // Node 0 constrained only by Node 1 (11ms + 3ms = 14ms), NOT Node 2.
        assert_eq!(bound[0], 14_000_000);
        // Node 1 constrained only by Node 0 (10ms + 2ms = 12ms), NOT Node 2.
        assert_eq!(bound[1], 12_000_000);
        // Node 2 is partitioned from all peers -> completely unconstrained!
        assert_eq!(bound[2], u64::MAX);
    }

    #[test]
    fn lookahead_propagates_idle_timers_and_multihop_null_messages() {
        // Node 0 runnable at 0, Node 1 idle with timer at 100ms, Node 2 runnable at 0.
        // Link 0 -> 2 is blocked, so 0 can only reach 2 via idle Node 1 (0 -> 1 -> 2).
        let states = [
            NodeExecState::Runnable { cluster_ns: 0 },
            NodeExecState::Idle {
                next_timer_ns: Some(100_000_000),
            },
            NodeExecState::Runnable { cluster_ns: 0 },
        ];
        let next_rx = [None, None, None];
        let is_blocked = |a: usize, b: usize| (a == 0 && b == 2) || (a == 2 && b == 0);
        let lat = |_: usize, _: usize| 2_000_000;

        let (et, bound) = compute_lookahead_from(&states, &next_rx, is_blocked, lat);
        // Node 1 could be woken by Node 0 or Node 2 at 0 + 2ms = 2ms!
        assert_eq!(et[1], 2_000_000);
        // Therefore Node 0 and Node 2 can safely advance up to 2ms + 2ms = 4ms!
        assert_eq!(bound[0], 4_000_000);
        assert_eq!(bound[2], 4_000_000);

        // If Node 0 is idle with a timer at 50ms and Node 1 is runnable at 0 (no 1->0 link),
        // Node 1 does not block on idle Node 0 until 50ms + 2ms = 52ms!
        let states2 = [
            NodeExecState::Idle {
                next_timer_ns: Some(50_000_000),
            },
            NodeExecState::Runnable { cluster_ns: 0 },
        ];
        let is_blocked2 = |a: usize, b: usize| a == 1 && b == 0;
        let (et2, bound2) = compute_lookahead_from(&states2, &[None, None], is_blocked2, lat);
        assert_eq!(et2[0], 50_000_000);
        assert_eq!(bound2[1], 52_000_000);
    }

    #[test]
    fn anchor_clock_drift_is_path_independent() {
        for drift_ppm in [-15_000, -500, -1, 0, 1, 500, 15_000] {
            let boot_c = 1_234_567;
            let local_base = 25_000_000;
            // Converting directly to `t` matches regardless of any intermediate pauses.
            let t1 = boot_c + 100_001;
            let t2 = boot_c + 500_003;
            let l1 = anchor_cluster_to_local_ns(boot_c, local_base, drift_ppm, t1);
            let l2 = anchor_cluster_to_local_ns(boot_c, local_base, drift_ppm, t2);
            assert!(l2 > l1);
            // Deadline rounding up ensures cluster_to_local(deadline_to_cluster(d)) >= d.
            for delta in [1u64, 7, 13, 1_000, 999_999, 10_000_001] {
                let d_local = local_base + delta;
                let d_cluster =
                    anchor_local_deadline_to_cluster_ns(boot_c, local_base, drift_ppm, d_local);
                let back = anchor_cluster_to_local_ns(boot_c, local_base, drift_ppm, d_cluster);
                assert!(
                    back >= d_local,
                    "drift={drift_ppm} delta={delta}: back={back} < d_local={d_local}"
                );
            }
        }
    }
}
