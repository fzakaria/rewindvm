//! Interactive, reactive multi-node cluster controller (`ClusterSession`).
//!
//! Drives a live deterministic multi-VM [`Cluster`] imperatively — advancing
//! virtual time, waiting for guest output/console/marks, mutating partitions
//! and links, crashing/restarting nodes, inspecting live guest files without
//! perturbing determinism, and branching timelines via named snapshots.
//!
//! Every imperative action is recorded with its exact `(cluster_ns,
//! cluster_step)` so calling [`ClusterSession::finish`] persists a standard
//! [`ClusterRun`] that replays bit-for-bit via `rewind cluster replay` without
//! needing the driving script or SDK.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rewind_init::Listing;
use rewind_store::Store;
use rewind_trace::cluster::{
    CLUSTER_MANIFEST, CLUSTER_NETWORK_LOG, CLUSTER_NODES_DIR, ClusterNodeResult, ClusterRunOutcome,
    ClusterSpecFile, ClusterVerdict,
};
use rewind_trace::manifest::{MANIFEST, MANIFEST_VERSION, Manifest, RunOutcome, Source};
use rewind_trace::stop::Stop as TraceStop;
use rewind_trace::verdict::Verdict;
use rewind_trace::{Event, EventKind, Trace, TraceWriter};
use rewind_vmm::DirtyLog;
use rewind_vmm::cluster::{
    Cluster, ClusterKeyframe, ClusterObserver, ClusterStop, NodeEncoders,
};
use rewind_vmm::net::{FaultKind, NetEvent, NetLogWriter, NetStats, NetSwitchConfig, ScheduledFault};

use crate::cluster::{
    CLUSTER_KEYFRAME_INTERVAL_NS, ClusterRun, ClusterRunOptions, PreparedCluster,
    compute_cluster_id, compute_node_parents, format_reason, judge_cluster_node, parse_role,
    parse_stop_when, prepare_cluster, save_cluster_keyframe,
};
use crate::home::{Guest, Home};
use crate::inspect::{Inspection, cat_on_machine, list_on_machine};
use crate::keyframes::{ReadPages, StorePages};
use crate::run::{Keyframes, SpecExt};

/// Which guest stream `wait_for` or `logs` inspects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogStream {
    /// Job stdout/stderr captured from trace `Output` events.
    #[default]
    Output,
    /// Kernel & init serial console output.
    Console,
    /// Guest `Mark` events.
    Mark,
    /// Matches across output, console, and marks.
    Any,
}

impl LogStream {
    pub fn parse(s: &str) -> Result<LogStream> {
        match s {
            "output" | "stdout" => Ok(LogStream::Output),
            "console" | "serial" => Ok(LogStream::Console),
            "mark" | "marks" => Ok(LogStream::Mark),
            "any" => Ok(LogStream::Any),
            other => bail!("unknown stream {other:?} (expected output, console, mark, or any)"),
        }
    }
}

/// Live per-node line buffers and read cursors (snapshot/restore-friendly).
#[derive(Clone, Debug, Default)]
struct NodeBuffers {
    output_lines: Vec<String>,
    output_partial: String,
    output_cursor: usize,
    console_lines: Vec<String>,
    console_partial: String,
    console_cursor: usize,
    mark_lines: Vec<String>,
    mark_cursor: usize,
}

impl NodeBuffers {
    fn push_output(&mut self, bytes: &[u8]) {
        push_bytes_as_lines(bytes, &mut self.output_partial, &mut self.output_lines);
    }

    fn push_console(&mut self, bytes: &[u8]) {
        push_bytes_as_lines(bytes, &mut self.console_partial, &mut self.console_lines);
    }

    fn push_mark(&mut self, text: &str) {
        self.mark_lines.push(text.to_string());
    }

    fn lines(&self, stream: LogStream) -> Vec<String> {
        match stream {
            LogStream::Output => {
                let mut v = self.output_lines.clone();
                if !self.output_partial.is_empty() {
                    v.push(self.output_partial.clone());
                }
                v
            }
            LogStream::Console => {
                let mut v = self.console_lines.clone();
                if !self.console_partial.is_empty() {
                    v.push(self.console_partial.clone());
                }
                v
            }
            LogStream::Mark => self.mark_lines.clone(),
            LogStream::Any => {
                let mut v = self.lines(LogStream::Output);
                v.extend(self.lines(LogStream::Console));
                v.extend(self.mark_lines.iter().cloned());
                v
            }
        }
    }

    /// Searches unread lines (or from start if `from_start`) for `needle`.
    /// Advances the stream's read cursor past the matched line when found.
    fn try_match(&mut self, stream: LogStream, needle: &str, from_start: bool) -> Option<String> {
        match stream {
            LogStream::Output => match_in(
                &self.output_lines,
                &self.output_partial,
                &mut self.output_cursor,
                needle,
                from_start,
            ),
            LogStream::Console => match_in(
                &self.console_lines,
                &self.console_partial,
                &mut self.console_cursor,
                needle,
                from_start,
            ),
            LogStream::Mark => match_in(
                &self.mark_lines,
                "",
                &mut self.mark_cursor,
                needle,
                from_start,
            ),
            LogStream::Any => self
                .try_match(LogStream::Output, needle, from_start)
                .or_else(|| self.try_match(LogStream::Console, needle, from_start))
                .or_else(|| self.try_match(LogStream::Mark, needle, from_start)),
        }
    }
}

fn push_bytes_as_lines(bytes: &[u8], partial: &mut String, lines: &mut Vec<String>) {
    let text = String::from_utf8_lossy(bytes);
    for ch in text.chars() {
        if ch == '\n' {
            let line = partial.trim_end_matches('\r').to_string();
            partial.clear();
            lines.push(line);
        } else {
            partial.push(ch);
        }
    }
}

fn match_in(
    lines: &[String],
    partial: &str,
    cursor: &mut usize,
    needle: &str,
    from_start: bool,
) -> Option<String> {
    let start = if from_start { 0 } else { *cursor };
    if start < lines.len() {
        for (idx, line) in lines[start..].iter().enumerate() {
            if line.contains(needle) {
                if !from_start {
                    *cursor = start + idx + 1;
                }
                return Some(line.clone());
            }
        }
    }
    if !partial.is_empty() && partial.contains(needle) {
        if !from_start {
            *cursor = lines.len();
        }
        return Some(partial.to_string());
    }
    None
}

/// In-memory session recorder that captures all node trace records, network
/// events, exit statuses, and live line buffers so snapshots/restores can
/// rewind cleanly without corrupting files on disk.
#[derive(Clone, Debug)]
struct SessionRecorder {
    node_names: Vec<String>,
    node_records: Vec<Vec<(u64, Vec<u8>)>>,
    net_events: Vec<NetEvent>,
    statuses: Vec<Option<i32>>,
    buffers: Vec<NodeBuffers>,
    echo: bool,
}

impl SessionRecorder {
    fn new(node_names: Vec<String>, echo: bool) -> SessionRecorder {
        let n = node_names.len();
        SessionRecorder {
            node_names,
            node_records: vec![Vec::new(); n],
            net_events: Vec::new(),
            statuses: vec![None; n],
            buffers: vec![NodeBuffers::default(); n],
            echo,
        }
    }

    fn record_event(&mut self, node: usize, step: u64, record: &[u8]) {
        self.node_records[node].push((step, record.to_vec()));
        let Ok(event) = Event::decode(step, record) else {
            return;
        };
        match &event.kind {
            EventKind::Output { bytes, .. } => {
                self.buffers[node].push_output(bytes);
                if self.echo {
                    let text = String::from_utf8_lossy(bytes);
                    let name = &self.node_names[node];
                    for line in text.lines() {
                        eprintln!("[{name}] {line}");
                    }
                }
            }
            EventKind::Console { text } => {
                self.buffers[node].push_console(text.as_bytes());
            }
            EventKind::Mark { text } => {
                self.buffers[node].push_mark(text);
                if let Some(rewind_init::Mark::Exit { status }) = event.init_mark() {
                    self.statuses[node] = Some(status);
                }
            }
            _ => {}
        }
    }

    fn record_serial(&mut self, node: usize, byte: u8) {
        self.buffers[node].push_console(&[byte]);
    }

    fn record_net(&mut self, event: &NetEvent) {
        self.net_events.push(event.clone());
    }
}

impl ClusterObserver for SessionRecorder {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        self.record_event(node, step, record);
    }

    fn node_serial(&mut self, node: usize, _step: u64, byte: u8) {
        self.record_serial(node, byte);
    }

    fn net_event(&mut self, event: &NetEvent) {
        self.record_net(event);
    }
}

/// Observer wrapper that stops as soon as `target_node` emits a line containing
/// `needle` on `stream`.
struct WaitForObserver<'a> {
    rec: &'a mut SessionRecorder,
    target_node: usize,
    stream: LogStream,
    needle: &'a str,
    matched: Option<String>,
}

impl ClusterObserver for WaitForObserver<'_> {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        self.rec.record_event(node, step, record);
        if node == self.target_node
            && self.matched.is_none()
            && let Some(line) = self.rec.buffers[node].try_match(self.stream, self.needle, false)
        {
            self.matched = Some(line);
        }
    }

    fn node_serial(&mut self, node: usize, _step: u64, byte: u8) {
        self.rec.record_serial(node, byte);
        if node == self.target_node
            && self.matched.is_none()
            && matches!(self.stream, LogStream::Console | LogStream::Any)
            && let Some(line) = self.rec.buffers[node].try_match(self.stream, self.needle, false)
        {
            self.matched = Some(line);
        }
    }

    fn net_event(&mut self, event: &NetEvent) {
        self.rec.record_net(event);
    }

    fn stop(&self) -> bool {
        self.matched.is_some()
    }
}

/// Observer wrapper that stops as soon as `target_node` exits.
struct WaitExitObserver<'a> {
    rec: &'a mut SessionRecorder,
    target_node: usize,
}

impl ClusterObserver for WaitExitObserver<'_> {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        self.rec.record_event(node, step, record);
    }

    fn node_serial(&mut self, node: usize, _step: u64, byte: u8) {
        self.rec.record_serial(node, byte);
    }

    fn net_event(&mut self, event: &NetEvent) {
        self.rec.record_net(event);
    }

    fn stop(&self) -> bool {
        self.rec.statuses[self.target_node].is_some()
    }
}

/// Saved point-in-time snapshot of an interactive session, supporting
/// multi-branch timeline exploration (`snapshot` -> branch A -> `restore` -> branch B).
#[derive(Clone, Debug)]
struct SessionSnapshot {
    keyframes: Vec<ClusterKeyframe>,
    node_encoders: Vec<NodeEncoders>,
    node_kf_parents: Vec<Option<u64>>,
    node_kf_boots: Vec<u32>,
    next_kf_ns: u64,
    recorded_faults: Vec<ScheduledFault>,
    node_records_lens: Vec<usize>,
    net_events_len: usize,
    statuses: Vec<Option<i32>>,
    buffers: Vec<NodeBuffers>,
    cluster_ns: u64,
    cluster_step: u64,
}

/// Per-node live snapshot returned by `status` / `advance`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct NodeStatus {
    pub name: String,
    pub ip: String,
    pub mac: String,
    pub role: String,
    pub step: u64,
    pub local_ns: u64,
    pub boot_count: u32,
    pub crashed: bool,
    pub exited: bool,
    pub exit_code: Option<i32>,
}

/// Overall live cluster status.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClusterStatus {
    pub id: String,
    pub name: String,
    pub seed: u64,
    pub schedule: u64,
    pub cluster_ns: u64,
    pub cluster_step: u64,
    pub stats: NetStats,
    pub active_partitions: Vec<(String, String)>,
    pub nodes: Vec<NodeStatus>,
}

/// Result of `ClusterSession::advance`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AdvanceResult {
    pub cluster_ns: u64,
    pub cluster_step: u64,
    pub stop: String,
    pub nodes: Vec<NodeStatus>,
}

/// Result of `ClusterSession::wait_for`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WaitForResult {
    pub matched: bool,
    pub line: Option<String>,
    pub cluster_ns: u64,
    pub cluster_step: u64,
    pub node_step: u64,
}

/// Result of `ClusterSession::wait_exit`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WaitExitResult {
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub cluster_ns: u64,
    pub cluster_step: u64,
}

/// Result of `ClusterSession::cat`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CatResult {
    pub found: bool,
    pub content: Option<String>,
    pub content_base64: Option<String>,
    pub error: Option<String>,
}

/// One directory entry returned by `ClusterSession::files`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: String,
    pub mode: u32,
    pub size: u64,
    pub target: Option<String>,
}

/// Result of `ClusterSession::files`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FilesResult {
    pub found: bool,
    pub entries: Vec<FileEntry>,
    pub error: Option<String>,
}

/// Interactive multi-node deterministic simulation session.
pub struct ClusterSession {
    home: Home,
    prepared: PreparedCluster,
    cluster: Cluster,
    store: Option<Store>,
    keyframes_mode: Keyframes,
    started: Instant,
    node_names: Vec<String>,
    name_to_idx: HashMap<String, usize>,
    recorder: SessionRecorder,
    recorded_faults: Vec<ScheduledFault>,
    keyframes: Vec<ClusterKeyframe>,
    node_kf_parents: Vec<Option<u64>>,
    node_kf_boots: Vec<u32>,
    next_kf_ns: u64,
    snapshots: HashMap<String, SessionSnapshot>,
    next_snap_id: u64,
    last_stop: ClusterStop,
    finished: bool,
}

impl std::fmt::Debug for ClusterSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterSession")
            .field("name", &self.prepared.manifest.name)
            .field("cluster_ns", &self.cluster.cluster_ns())
            .field("cluster_step", &self.cluster.cluster_step())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ClusterSession {
    /// Boots a new interactive multi-node cluster from `spec_file` and `opts`.
    pub fn start(
        home: Home,
        guest: &Guest,
        spec_file: &ClusterSpecFile,
        spec_dir: &Path,
        opts: &ClusterRunOptions,
    ) -> Result<ClusterSession> {
        let spec_source = serde_json::to_string(spec_file)?;
        let prepared = prepare_cluster(&home, guest, spec_file, spec_dir, &spec_source, opts)?;
        // Always enable KVM dirty logging in interactive sessions so `snapshot()`
        // and periodic keyframes work seamlessly on demand.
        let dirty_log = DirtyLog::On;
        let cluster = Cluster::boot(prepared.cluster_config.clone(), dirty_log)?;

        let node_names: Vec<String> = prepared
            .manifest
            .nodes
            .iter()
            .map(|n| n.name.clone())
            .collect();
        let name_to_idx: HashMap<String, usize> = node_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        let n_count = node_names.len();

        let store = if opts.keyframes == Keyframes::Take {
            Some(Store::open(&home.store())?)
        } else {
            None
        };

        let recorder = SessionRecorder::new(node_names.clone(), opts.echo);

        Ok(ClusterSession {
            home,
            prepared,
            cluster,
            store,
            keyframes_mode: opts.keyframes,
            started: Instant::now(),
            node_names,
            name_to_idx,
            recorder,
            recorded_faults: Vec::new(),
            keyframes: Vec::new(),
            node_kf_parents: vec![None; n_count],
            node_kf_boots: vec![1; n_count],
            next_kf_ns: CLUSTER_KEYFRAME_INTERVAL_NS,
            snapshots: HashMap::new(),
            next_snap_id: 1,
            last_stop: ClusterStop::Paused,
            finished: false,
        })
    }

    pub fn id(&self) -> &str {
        self.prepared.id.as_str()
    }

    pub fn node_index(&self, name: &str) -> Result<usize> {
        self.name_to_idx.get(name).copied().with_context(|| {
            format!(
                "unknown node {name:?} (nodes: {})",
                self.node_names.join(", ")
            )
        })
    }

    /// Returns the live status of the cluster and every node.
    pub fn status(&self) -> ClusterStatus {
        let active_partitions = self
            .cluster
            .active_partitions()
            .into_iter()
            .map(|(a, b)| (self.node_names[a].clone(), self.node_names[b].clone()))
            .collect();
        ClusterStatus {
            id: self.prepared.id.as_str().to_string(),
            name: self.prepared.manifest.name.clone(),
            seed: self.prepared.manifest.seed,
            schedule: self.prepared.manifest.schedule,
            cluster_ns: self.cluster.cluster_ns(),
            cluster_step: self.cluster.cluster_step(),
            stats: self.cluster.switch_stats(),
            active_partitions,
            nodes: self.node_statuses(),
        }
    }

    fn node_statuses(&self) -> Vec<NodeStatus> {
        let outcomes = self.cluster.node_outcomes();
        self.prepared
            .manifest
            .nodes
            .iter()
            .enumerate()
            .map(|(i, nm)| {
                let o = &outcomes[i];
                let exit_code = self.recorder.statuses[i].map(decode_exit_code);
                NodeStatus {
                    name: nm.name.clone(),
                    ip: nm.ip.clone(),
                    mac: format!(
                        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        nm.mac[0], nm.mac[1], nm.mac[2], nm.mac[3], nm.mac[4], nm.mac[5]
                    ),
                    role: nm.role.clone(),
                    step: o.steps,
                    local_ns: o.local_ns,
                    boot_count: o.boot_count,
                    crashed: o.crashed,
                    exited: self.cluster.node_exited(i),
                    exit_code,
                }
            })
            .collect()
    }

    /// Captures a periodic cluster keyframe if enabled and due.
    fn maybe_capture_periodic_keyframe(&mut self) -> Result<()> {
        if self.keyframes_mode != Keyframes::Take {
            return Ok(());
        }
        if self.cluster.cluster_ns() < self.next_kf_ns {
            return Ok(());
        }
        self.capture_keyframe_now()?;
        self.next_kf_ns = self
            .cluster
            .cluster_ns()
            .saturating_add(CLUSTER_KEYFRAME_INTERVAL_NS);
        Ok(())
    }

    fn capture_keyframe_now(&mut self) -> Result<()> {
        let step = self.cluster.cluster_step();
        if self.keyframes.last().is_some_and(|k| k.cluster_step == step) {
            return Ok(());
        }
        if self.store.is_none() {
            self.store = Some(Store::open(&self.home.store())?);
        }
        let store = self.store.as_mut().expect("store opened above");
        let mut pages = StorePages(store);
        let parent_step = self.keyframes.last().map(|k| k.cluster_step);
        let parents = compute_node_parents(&self.cluster, &self.node_kf_parents, &self.node_kf_boots);
        let ckf = self.cluster.keyframe(&mut pages, parent_step, &parents)?;
        for (i, nk) in ckf.nodes.iter().enumerate() {
            if let Some(kf) = &nk.keyframe {
                self.node_kf_parents[i] = Some(kf.step);
                self.node_kf_boots[i] = nk.boot_count;
            } else {
                self.node_kf_parents[i] = None;
            }
        }
        self.keyframes.push(ckf);
        Ok(())
    }

    /// Advances virtual time by `delta_ns` (or to `until_ns`, or until stop
    /// condition if both are `None`).
    pub fn advance(
        &mut self,
        delta_ns: Option<u64>,
        until_ns: Option<u64>,
        wall_timeout: Option<Duration>,
    ) -> Result<AdvanceResult> {
        let target_ns = match (delta_ns, until_ns) {
            (_, Some(u)) => Some(u),
            (Some(d), None) => Some(self.cluster.cluster_ns().saturating_add(d)),
            (None, None) => None,
        };
        let deadline = wall_timeout.map(|d| Instant::now() + d);

        // Run in keyframe-sized chunks so periodic keyframes are captured cleanly.
        let stop = loop {
            let chunk_target = match (target_ns, self.keyframes_mode) {
                (Some(t), Keyframes::Take) => Some(t.min(self.next_kf_ns)),
                (Some(t), Keyframes::Skip | Keyframes::AtExit) => Some(t),
                (None, Keyframes::Take) => Some(self.next_kf_ns),
                (None, Keyframes::Skip | Keyframes::AtExit) => None,
            };
            let out = self
                .cluster
                .run(&mut self.recorder, chunk_target, deadline)?;
            self.last_stop = out.stop;
            if out.stop != ClusterStop::Paused {
                break out.stop;
            }
            self.maybe_capture_periodic_keyframe()?;
            if let Some(t) = target_ns
                && self.cluster.cluster_ns() >= t
            {
                break ClusterStop::Paused;
            }
        };

        Ok(AdvanceResult {
            cluster_ns: self.cluster.cluster_ns(),
            cluster_step: self.cluster.cluster_step(),
            stop: format!("{stop:?}"),
            nodes: self.node_statuses(),
        })
    }

    /// Waits until `node_name` emits a line containing `needle` on `stream`, or
    /// until `timeout_ns` virtual time elapses.
    pub fn wait_for(
        &mut self,
        node_name: &str,
        needle: &str,
        stream: LogStream,
        timeout_ns: Option<u64>,
        from_start: bool,
        wall_timeout: Option<Duration>,
    ) -> Result<WaitForResult> {
        let idx = self.node_index(node_name)?;
        if let Some(line) = self.recorder.buffers[idx].try_match(stream, needle, from_start) {
            return Ok(WaitForResult {
                matched: true,
                line: Some(line),
                cluster_ns: self.cluster.cluster_ns(),
                cluster_step: self.cluster.cluster_step(),
                node_step: self.cluster.node_steps(idx),
            });
        }

        let target_ns = timeout_ns.map(|d| self.cluster.cluster_ns().saturating_add(d));
        let deadline = wall_timeout.map(|d| Instant::now() + d);

        let matched = loop {
            let chunk_target = match (target_ns, self.keyframes_mode) {
                (Some(t), Keyframes::Take) => Some(t.min(self.next_kf_ns)),
                (Some(t), Keyframes::Skip | Keyframes::AtExit) => Some(t),
                (None, Keyframes::Take) => Some(self.next_kf_ns),
                (None, Keyframes::Skip | Keyframes::AtExit) => None,
            };
            let mut obs = WaitForObserver {
                rec: &mut self.recorder,
                target_node: idx,
                stream,
                needle,
                matched: None,
            };
            let out = self.cluster.run(&mut obs, chunk_target, deadline)?;
            let found = obs.matched.take();
            self.last_stop = out.stop;
            if let Some(line) = found {
                break Some(line);
            }
            if out.stop != ClusterStop::Paused {
                break None;
            }
            self.maybe_capture_periodic_keyframe()?;
            if let Some(t) = target_ns
                && self.cluster.cluster_ns() >= t
            {
                break None;
            }
        };

        Ok(WaitForResult {
            matched: matched.is_some(),
            line: matched,
            cluster_ns: self.cluster.cluster_ns(),
            cluster_step: self.cluster.cluster_step(),
            node_step: self.cluster.node_steps(idx),
        })
    }

    /// Waits until `node_name` exits (or until the cluster stop condition is
    /// satisfied when `node_name` is `None`), up to `timeout_ns` virtual time.
    pub fn wait_exit(
        &mut self,
        node_name: Option<&str>,
        timeout_ns: Option<u64>,
        wall_timeout: Option<Duration>,
    ) -> Result<WaitExitResult> {
        let target_idx = match node_name {
            Some(n) => Some(self.node_index(n)?),
            None => None,
        };
        if let Some(idx) = target_idx
            && let Some(raw) = self.recorder.statuses[idx]
        {
            return Ok(WaitExitResult {
                exited: true,
                exit_code: Some(decode_exit_code(raw)),
                cluster_ns: self.cluster.cluster_ns(),
                cluster_step: self.cluster.cluster_step(),
            });
        }

        let target_ns = timeout_ns.map(|d| self.cluster.cluster_ns().saturating_add(d));
        let deadline = wall_timeout.map(|d| Instant::now() + d);

        let exited = loop {
            let chunk_target = match (target_ns, self.keyframes_mode) {
                (Some(t), Keyframes::Take) => Some(t.min(self.next_kf_ns)),
                (Some(t), Keyframes::Skip | Keyframes::AtExit) => Some(t),
                (None, Keyframes::Take) => Some(self.next_kf_ns),
                (None, Keyframes::Skip | Keyframes::AtExit) => None,
            };
            let out = if let Some(idx) = target_idx {
                let mut obs = WaitExitObserver {
                    rec: &mut self.recorder,
                    target_node: idx,
                };
                self.cluster.run(&mut obs, chunk_target, deadline)?
            } else {
                self.cluster
                    .run(&mut self.recorder, chunk_target, deadline)?
            };
            self.last_stop = out.stop;
            if let Some(idx) = target_idx
                && self.recorder.statuses[idx].is_some()
            {
                break true;
            }
            if out.stop == ClusterStop::Completed {
                break true;
            }
            if out.stop != ClusterStop::Paused {
                break false;
            }
            self.maybe_capture_periodic_keyframe()?;
            if let Some(t) = target_ns
                && self.cluster.cluster_ns() >= t
            {
                break false;
            }
        };

        let exit_code = target_idx
            .and_then(|idx| self.recorder.statuses[idx])
            .map(decode_exit_code);
        Ok(WaitExitResult {
            exited,
            exit_code,
            cluster_ns: self.cluster.cluster_ns(),
            cluster_step: self.cluster.cluster_step(),
        })
    }

    /// Applies an interactive fault right now and records it at
    /// `(cluster_ns, Some(cluster_step))` for exact standalone replay.
    fn apply_interactive_fault(&mut self, kind: FaultKind) -> Result<()> {
        let fault = ScheduledFault {
            at_ns: self.cluster.cluster_ns(),
            at_step: Some(self.cluster.cluster_step()),
            kind: kind.clone(),
        };
        self.cluster.apply_fault_now(&kind, &mut self.recorder)?;
        self.recorded_faults.push(fault);
        Ok(())
    }

    /// Partitions group `a` from group `b` (or isolates `a` from all other
    /// nodes if `b` is `None`). When `symmetric` is `false`, only `a -> b` is
    /// blocked.
    pub fn partition(
        &mut self,
        a: &[String],
        b: Option<&[String]>,
        symmetric: bool,
    ) -> Result<usize> {
        let a_idx: Vec<usize> = a
            .iter()
            .map(|n| self.node_index(n))
            .collect::<Result<_>>()?;
        let b_idx: Vec<usize> = match b {
            Some(bs) => bs
                .iter()
                .map(|n| self.node_index(n))
                .collect::<Result<_>>()?,
            None => (0..self.node_names.len())
                .filter(|i| !a_idx.contains(i))
                .collect(),
        };
        let kind = if symmetric {
            FaultKind::Partition {
                groups: vec![a_idx, b_idx],
            }
        } else {
            let mut blocked = Vec::new();
            for &x in &a_idx {
                for &y in &b_idx {
                    if x != y {
                        blocked.push((x, y));
                    }
                }
            }
            FaultKind::AsymmetricPartition { blocked }
        };
        self.apply_interactive_fault(kind)?;
        Ok(self.cluster.active_partitions().len())
    }

    /// Heals all active partitions (when `a` is `None`), or heals links between
    /// `a` and `b` (or all links touching `a` if `b` is `None`).
    pub fn heal(&mut self, a: Option<&[String]>, b: Option<&[String]>) -> Result<()> {
        let kind = match a {
            None => FaultKind::Heal,
            Some(a_names) => {
                let a_idx: Vec<usize> = a_names
                    .iter()
                    .map(|n| self.node_index(n))
                    .collect::<Result<_>>()?;
                let b_idx: Vec<usize> = match b {
                    Some(b_names) => b_names
                        .iter()
                        .map(|n| self.node_index(n))
                        .collect::<Result<_>>()?,
                    None => (0..self.node_names.len())
                        .filter(|i| !a_idx.contains(i))
                        .collect(),
                };
                let mut links = Vec::new();
                for &x in &a_idx {
                    for &y in &b_idx {
                        links.push((x, y));
                        links.push((y, x));
                    }
                }
                FaultKind::HealLinks { links }
            }
        };
        self.apply_interactive_fault(kind)
    }

    /// Dynamically updates link parameters between `from` and `to` (and
    /// optionally `to -> from` when `bidirectional` is true).
    #[allow(clippy::too_many_arguments, reason = "link configuration fields")]
    pub fn set_link(
        &mut self,
        from: &str,
        to: &str,
        latency_ns: Option<u64>,
        jitter_ns: Option<u64>,
        drop_ppm: Option<u32>,
        duplicate_ppm: Option<u32>,
        bidirectional: bool,
    ) -> Result<()> {
        let from_idx = self.node_index(from)?;
        let to_idx = self.node_index(to)?;
        self.apply_interactive_fault(FaultKind::SetLink {
            from: from_idx,
            to: to_idx,
            latency_ns,
            jitter_ns,
            drop_ppm,
            duplicate_ppm,
        })?;
        if bidirectional {
            self.apply_interactive_fault(FaultKind::SetLink {
                from: to_idx,
                to: from_idx,
                latency_ns,
                jitter_ns,
                drop_ppm,
                duplicate_ppm,
            })?;
        }
        Ok(())
    }

    /// Hard-crashes `node` immediately.
    pub fn crash(&mut self, node: &str) -> Result<u32> {
        let idx = self.node_index(node)?;
        self.apply_interactive_fault(FaultKind::Crash { node: idx })?;
        Ok(self.cluster.node_boot_count(idx))
    }

    /// Cold-reboots `node` (crashing it first if it is currently running).
    pub fn restart(&mut self, node: &str) -> Result<u32> {
        let idx = self.node_index(node)?;
        if !self.cluster.node_is_crashed(idx) {
            self.apply_interactive_fault(FaultKind::Crash { node: idx })?;
        }
        self.apply_interactive_fault(FaultKind::Restart { node: idx })?;
        // Reset exit status for the new boot so wait_exit tracks the rebooted process.
        self.recorder.statuses[idx] = None;
        Ok(self.cluster.node_boot_count(idx))
    }

    /// Reads captured lines for `node` on `stream` starting at `since`.
    pub fn logs(&self, node: &str, stream: LogStream, since: usize) -> Result<(Vec<String>, usize)> {
        let idx = self.node_index(node)?;
        let all = self.recorder.buffers[idx].lines(stream);
        let total = all.len();
        let slice = if since < total {
            all[since..].to_vec()
        } else {
            Vec::new()
        };
        Ok((slice, total))
    }

    /// Reads recorded network/fault events starting at `since`.
    pub fn net_events(&self, since: usize) -> (Vec<NetEvent>, usize) {
        let total = self.recorder.net_events.len();
        let slice = if since < total {
            self.recorder.net_events[since..].to_vec()
        } else {
            Vec::new()
        };
        (slice, total)
    }

    /// Reads a file from a live node's filesystem via a non-destructive VM
    /// clone (zero perturbation to the live simulation!).
    pub fn cat(&mut self, node: &str, path: &str, pid: Option<u32>) -> Result<CatResult> {
        use base64::Engine;
        let idx = self.node_index(node)?;
        let (mut cloned, step) = self
            .cluster
            .clone_node_machine(idx, &self.recorder.node_records[idx])?;
        match cat_on_machine(&mut cloned, step, pid, path)? {
            Inspection::Contents(bytes) => {
                let text = String::from_utf8(bytes.clone()).ok();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(CatResult {
                    found: true,
                    content: text,
                    content_base64: Some(b64),
                    error: None,
                })
            }
            Inspection::NotFound(msg) => Ok(CatResult {
                found: false,
                content: None,
                content_base64: None,
                error: Some(msg),
            }),
            Inspection::Failed(msg) => Ok(CatResult {
                found: false,
                content: None,
                content_base64: None,
                error: Some(msg),
            }),
        }
    }

    /// Lists a directory inside a live node's filesystem via a non-destructive
    /// VM clone (zero perturbation to the live simulation!).
    pub fn files(&mut self, node: &str, path: &str, pid: Option<u32>) -> Result<FilesResult> {
        let idx = self.node_index(node)?;
        let (mut cloned, step) = self
            .cluster
            .clone_node_machine(idx, &self.recorder.node_records[idx])?;
        match list_on_machine(&mut cloned, step, pid, path)? {
            Inspection::Contents(bytes) => {
                let Some(listing) = Listing::parse(&bytes) else {
                    bail!("VM returned malformed directory listing for {path}");
                };
                let entries = listing
                    .entries
                    .into_iter()
                    .map(|e| FileEntry {
                        path: e.path,
                        kind: format!("{:?}", e.kind).to_lowercase(),
                        mode: e.mode,
                        size: e.size,
                        target: e.target,
                    })
                    .collect();
                Ok(FilesResult {
                    found: true,
                    entries,
                    error: None,
                })
            }
            Inspection::NotFound(msg) | Inspection::Failed(msg) => Ok(FilesResult {
                found: false,
                entries: Vec::new(),
                error: Some(msg),
            }),
        }
    }

    /// Captures a named whole-cluster snapshot (VMs + switch + recorded faults
    /// + log buffers) that can be restored later to branch timelines.
    pub fn snapshot(&mut self, name: Option<String>) -> Result<(String, u64, u64)> {
        self.capture_keyframe_now()?;
        let label = name.unwrap_or_else(|| {
            let id = self.next_snap_id;
            self.next_snap_id += 1;
            format!("snap-{id}")
        });
        let snap = SessionSnapshot {
            keyframes: self.keyframes.clone(),
            node_encoders: self.cluster.node_encoders(),
            node_kf_parents: self.node_kf_parents.clone(),
            node_kf_boots: self.node_kf_boots.clone(),
            next_kf_ns: self.next_kf_ns,
            recorded_faults: self.recorded_faults.clone(),
            node_records_lens: self.recorder.node_records.iter().map(Vec::len).collect(),
            net_events_len: self.recorder.net_events.len(),
            statuses: self.recorder.statuses.clone(),
            buffers: self.recorder.buffers.clone(),
            cluster_ns: self.cluster.cluster_ns(),
            cluster_step: self.cluster.cluster_step(),
        };
        let ns = snap.cluster_ns;
        let step = snap.cluster_step;
        self.snapshots.insert(label.clone(), snap);
        Ok((label, ns, step))
    }

    /// Rewinds the entire live cluster back to a previously captured named
    /// snapshot, ready to explore an alternate timeline.
    pub fn restore(&mut self, name: &str) -> Result<(u64, u64)> {
        let snap = self
            .snapshots
            .get(name)
            .with_context(|| format!("unknown cluster snapshot {name:?}"))?
            .clone();
        let store = self
            .store
            .as_ref()
            .context("page store is not open for snapshot restore")?;

        // Rewind recorder state back to the snapshot point before restoring VMs.
        for (i, &len) in snap.node_records_lens.iter().enumerate() {
            self.recorder.node_records[i].truncate(len);
        }
        self.recorder.net_events.truncate(snap.net_events_len);
        self.recorder.statuses = snap.statuses;
        self.recorder.buffers = snap.buffers;
        self.recorded_faults = snap.recorded_faults;
        self.keyframes = snap.keyframes;
        self.node_kf_parents = snap.node_kf_parents;
        self.node_kf_boots = snap.node_kf_boots;
        self.next_kf_ns = snap.next_kf_ns;

        let pages = ReadPages(store);
        self.cluster = Cluster::restore(
            self.prepared.cluster_config.clone(),
            &self.keyframes,
            &pages,
            &self.recorder.node_records,
            DirtyLog::On,
        )?;
        self.cluster.set_node_encoders(&snap.node_encoders);
        self.last_stop = ClusterStop::Paused;
        Ok((snap.cluster_ns, snap.cluster_step))
    }

    /// Finalizes the session on disk as a standalone, bit-for-bit replayable
    /// [`ClusterRun`].
    pub fn finish(
        &mut self,
        verdict_override: Option<bool>,
        failure_reason: Option<String>,
    ) -> Result<ClusterRun> {
        if self.finished {
            bail!("cluster session is already finished");
        }
        self.finished = true;

        if self.keyframes_mode == Keyframes::Take {
            self.capture_keyframe_now()?;
        }
        if let Some(store) = self.store.as_ref() {
            store.sync()?;
        }

        let final_ns = self.cluster.cluster_ns();
        let final_step = self.cluster.cluster_step();
        let wall_ms = self.started.elapsed().as_millis() as u64;

        // Fold all recorded interactive faults & barriers into network_json so
        // `rewind cluster replay` reproduces the exact session standalone.
        let mut net_cfg: NetSwitchConfig =
            serde_json::from_value(self.prepared.manifest.network_json.clone())?;
        net_cfg.faults.extend(self.recorded_faults.clone());
        net_cfg
            .faults
            .sort_by_key(|f| (f.at_ns, f.at_step.unwrap_or(0)));
        let network_json = serde_json::to_value(&net_cfg)?;

        let stop_when = parse_stop_when(Some(&self.prepared.manifest.stop_when), false)?;
        let id = compute_cluster_id(
            &self.prepared.manifest.nodes,
            &network_json,
            stop_when,
            Some(final_ns),
            self.prepared.manifest.slice_ns,
        );

        let dir = self.home.clusters().join(id.as_str());
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;

        // Write per-node trace.bin files and binary network.bin.
        for (idx, name) in self.node_names.iter().enumerate() {
            let ndir = dir.join(CLUSTER_NODES_DIR).join(name);
            fs::create_dir_all(&ndir)?;
            let mut writer = TraceWriter::new(File::create(
                ndir.join(rewind_trace::manifest::TRACE),
            )?);
            for (step, rec) in &self.recorder.node_records[idx] {
                writer.record(*step, rec)?;
            }
            writer.finish()?;
        }
        {
            let mut net_log = NetLogWriter::new(File::create(dir.join(CLUSTER_NETWORK_LOG))?);
            for ev in &self.recorder.net_events {
                net_log.record(ev)?;
            }
            net_log.finish()?;
        }
        if self.keyframes_mode == Keyframes::Take {
            for ckf in &mut self.keyframes {
                // Advance `next_fault_idx` to match the faults already applied
                // at this keyframe's `(cluster_ns, cluster_step)` so both
                // standalone replay and mid-run seeking (`rewind cat/files`)
                // start from the exact post-fault switch cursor.
                ckf.switch.next_fault_idx = net_cfg
                    .faults
                    .iter()
                    .take_while(|f| match f.at_step {
                        Some(s) => ckf.cluster_ns >= f.at_ns && ckf.cluster_step >= s,
                        None => f.at_ns <= ckf.cluster_ns,
                    })
                    .count();
                save_cluster_keyframe(&dir, ckf)?;
            }
        }

        // Compute combined trace hash, per-node manifests, and verdict.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&fs::read(dir.join(CLUSTER_NETWORK_LOG))?);

        let outcome = self.cluster.current_outcome(self.last_stop);
        let mut node_results = Vec::with_capacity(self.node_names.len());
        let mut failure_reasons = Vec::new();

        for (idx, nm) in self.prepared.manifest.nodes.iter().enumerate() {
            let ndir = dir.join(CLUSTER_NODES_DIR).join(&nm.name);
            let trace_bytes = fs::read(ndir.join(rewind_trace::manifest::TRACE))?;
            hasher.update(&trace_bytes);
            let node_trace_hash = blake3::hash(&trace_bytes).to_hex().to_string();
            let trace = Trace::decode(&self.recorder.node_records[idx])?;

            let no = &outcome.nodes[idx];
            let status = self.recorder.statuses[idx];
            let role = parse_role(Some(&nm.role))?;
            let node_verdict = judge_cluster_node(
                role,
                stop_when,
                outcome.stop,
                status,
                no.crashed,
                &trace,
            );
            if let Verdict::Failed { reasons } = &node_verdict {
                for r in reasons {
                    failure_reasons.push(format!("node {}: {}", nm.name, format_reason(r)));
                }
            }

            let node_manifest = Manifest {
                version: MANIFEST_VERSION,
                id: nm.spec.id(),
                name: format!("{}/{}", self.prepared.manifest.name, nm.name),
                created: self.prepared.manifest.created,
                source: Source::Image {
                    root: nm
                        .spec
                        .image
                        .as_ref()
                        .map_or_else(|| "initramfs".into(), |p| p.display().to_string()),
                },
                spec: nm.spec.clone(),
                parent: None,
                shared_keyframes: None,
                outcome: Some(RunOutcome {
                    stop: TraceStop::PoweredOff,
                    step: no.steps,
                    virtual_ns: no.local_ns,
                    wall_ms,
                    status,
                }),
                verdict: node_verdict.clone(),
                trace_hash: Some(node_trace_hash),
                first_difference: None,
                recorded_by: crate::VERSION.to_string(),
            };
            crate::image::write_atomic(
                &ndir.join(MANIFEST),
                &serde_json::to_vec_pretty(&node_manifest)?,
            )?;

            node_results.push(ClusterNodeResult {
                name: nm.name.clone(),
                role: nm.role.clone(),
                steps: no.steps,
                local_ns: no.local_ns,
                boot_count: no.boot_count,
                status,
                verdict: node_verdict,
            });
        }

        let verdict = match verdict_override {
            Some(true) => ClusterVerdict::Passed,
            Some(false) => {
                if let Some(reason) = failure_reason {
                    failure_reasons.insert(0, reason);
                } else if failure_reasons.is_empty() {
                    failure_reasons.push("marked failed by controller script".into());
                }
                ClusterVerdict::Failed {
                    reasons: failure_reasons,
                }
            }
            None => {
                if let Some(reason) = failure_reason {
                    failure_reasons.push(reason);
                }
                if failure_reasons.is_empty() {
                    ClusterVerdict::Passed
                } else {
                    ClusterVerdict::Failed {
                        reasons: failure_reasons,
                    }
                }
            }
        };

        let mut manifest = self.prepared.manifest.clone();
        manifest.id = id.clone();
        manifest.max_virtual_ns = Some(final_ns);
        manifest.stop_at_step = Some(final_step);
        manifest.network_json = network_json;
        manifest.outcome = Some(ClusterRunOutcome {
            stop: format!("{:?}", outcome.stop),
            cluster_ns: final_ns,
            cluster_step: final_step,
            wall_ms,
            nodes: node_results,
            tx_frames: outcome.net_stats.tx_frames,
            delivered_frames: outcome.net_stats.delivered_frames,
            dropped_frames: outcome.net_stats.dropped_frames,
            duplicated_frames: outcome.net_stats.duplicated_frames,
            partitions: outcome.net_stats.partitions,
            crashes: outcome.net_stats.crashes,
            restarts: outcome.net_stats.restarts,
        });
        manifest.verdict = verdict;
        manifest.trace_hash = Some(hasher.finalize().to_hex().to_string());

        crate::image::write_atomic(
            &dir.join(CLUSTER_MANIFEST),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;

        // If launched by `rewind cluster check -- <script>`, report the
        // recorded run ID and verdict back to the check coordinator.
        if let Ok(result_file) = std::env::var("REWIND_RESULT_FILE")
            && !result_file.is_empty()
        {
            let report = serde_json::json!({
                "id": manifest.id.as_str(),
                "passed": manifest.verdict.passed(),
                "cluster_ns": final_ns,
            });
            let _ = crate::image::write_atomic(
                &PathBuf::from(result_file),
                &serde_json::to_vec(&report)?,
            );
        }

        Ok(ClusterRun { dir, manifest })
    }
}

fn decode_exit_code(raw: i32) -> i32 {
    match rewind_trace::ending::ExitStatus::from_wait(raw) {
        rewind_trace::ending::ExitStatus::Code(c) => c as i32,
        rewind_trace::ending::ExitStatus::Signal { signo, .. } => 128 + signo as i32,
    }
}
