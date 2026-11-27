//! Language-agnostic NDJSON-RPC 2.0 server (`rewind cluster serve`) driving an
//! interactive [`ClusterSession`].
//!
//! Speaks one JSON object per line over `stdin`/`stdout` (default) or a Unix
//! domain socket (`--socket <PATH>`), allowing any language (Python, TypeScript,
//! Go, Rust, shell) to programmatically build, step, wait on, fault-inject,
//! inspect, snapshot, branch, and finalize deterministic Rewind clusters.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rewind_trace::cluster::{
    ClusterSpecFile, FaultSpecFile, LinkSpecFile, NemesisSpecFile, NetworkSpecFile, NodeFileSpec,
    NodeSpecFile,
};
use rewind_vmm::{ClockSource, CpuModel};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cluster::ClusterRunOptions;
use crate::cluster_session::{ClusterSession, LogStream};
use crate::home::{Guest, Home};
use crate::run::Keyframes;

/// Server defaults inherited from CLI flags and environment variables (such as
/// `REWIND_SEED`, `REWIND_SCHEDULE`, `REWIND_NEMESIS`, `REWIND_FAULT_FROM_MS`,
/// `REWIND_FAULT_UNTIL_MS`, `REWIND_KEYFRAMES` injected by `rewind cluster check`).
#[derive(Clone, Debug)]
pub struct ServeDefaults {
    pub home: Home,
    pub guest: Guest,
    pub cpu: CpuModel,
    pub clock: ClockSource,
    pub workdir: PathBuf,
}

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

/// Serves NDJSON-RPC 2.0 requests from `reader` and writes responses to `writer`.
pub fn serve_stream<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    defaults: &ServeDefaults,
) -> Result<()> {
    let mut session: Option<ClusterSession> = None;

    for line_res in reader.lines() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                    }),
                };
                writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                writer.flush()?;
                continue;
            }
        };

        let is_shutdown = req.method == "shutdown";
        let resp = match dispatch(&req.method, req.params, &mut session, defaults) {
            Ok(val) => RpcResponse {
                jsonrpc: "2.0",
                id: req.id,
                result: Some(val),
                error: None,
            },
            Err(e) => RpcResponse {
                jsonrpc: "2.0",
                id: req.id,
                result: None,
                error: Some(RpcError {
                    code: -32000,
                    message: format!("{e:#}"),
                }),
            },
        };
        writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
        writer.flush()?;
        if is_shutdown {
            break;
        }
    }
    Ok(())
}

/// Serves NDJSON-RPC 2.0 on a Unix domain socket at `path` (accepting one
/// client connection at a time until `shutdown` or EOF).
pub fn serve_unix_socket(path: &Path, defaults: &ServeDefaults) -> Result<()> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding cluster RPC socket at {}", path.display()))?;
    eprintln!("rewind cluster RPC listening on {}", path.display());
    for stream in listener.incoming() {
        let stream = stream?;
        let reader = BufReader::new(stream.try_clone()?);
        let res = serve_stream(reader, stream, defaults);
        if let Err(e) = res {
            eprintln!("rewind cluster RPC client error: {e:#}");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct StartParams {
    #[serde(default = "default_cluster_name")]
    name: String,
    seed: Option<u64>,
    schedule: Option<u64>,
    nemesis_enabled: Option<bool>,
    fault_window_from_ms: Option<u64>,
    fault_window_until_ms: Option<u64>,
    epoch: Option<u64>,
    mem_mib: Option<u64>,
    cores: Option<u32>,
    max_virtual_ms: Option<u64>,
    slice_us: Option<u64>,
    #[serde(default)]
    packages: Vec<String>,
    stop_when: Option<String>,
    keyframes: Option<bool>,
    #[serde(default)]
    echo: bool,
    #[serde(default)]
    network: NetworkSpecFile,
    #[serde(default)]
    links: Vec<LinkSpecFile>,
    #[serde(default)]
    faults: Vec<FaultSpecFile>,
    nemesis: Option<NemesisSpecFile>,
    nodes: Vec<NodeStartParams>,
}

#[derive(Deserialize)]
struct NodeStartParams {
    name: String,
    ip: Option<String>,
    role: Option<String>,
    root: Option<PathBuf>,
    #[serde(default)]
    packages: Vec<String>,
    command: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    cwd: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
    mem_mib: Option<u64>,
    cores: Option<u32>,
    #[serde(default)]
    clock_skew_ms: i64,
    #[serde(default)]
    clock_drift_ppm: i32,
    #[serde(default)]
    files: Vec<NodeFileSpec>,
}

fn default_cluster_name() -> String {
    "cluster".into()
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn env_bool(key: &str) -> Option<bool> {
    std::env::var(key).ok().and_then(|v| match v.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    })
}

fn dispatch(
    method: &str,
    params: Value,
    session: &mut Option<ClusterSession>,
    defaults: &ServeDefaults,
) -> Result<Value> {
    match method {
        "ping" => Ok(json!({ "ok": true, "version": crate::VERSION })),
        "shutdown" => {
            *session = None;
            Ok(json!({ "ok": true }))
        }
        "cluster.start" => {
            let p: StartParams = serde_json::from_value(params)
                .context("invalid params for cluster.start")?;
            let mut network = p.network;
            network.links.extend(p.links);
            let spec = ClusterSpecFile {
                name: p.name,
                mem_mib: p.mem_mib,
                cores: p.cores,
                stop_when: p.stop_when,
                max_virtual_ms: p.max_virtual_ms,
                slice_us: p.slice_us,
                packages: p.packages,
                network,
                faults: p.faults,
                nemesis: p.nemesis,
                nodes: p
                    .nodes
                    .into_iter()
                    .map(|n| NodeSpecFile {
                        name: n.name,
                        ip: n.ip,
                        role: n.role,
                        root: n.root,
                        packages: n.packages,
                        command: n.command,
                        env: n.env,
                        cwd: n.cwd,
                        uid: n.uid,
                        gid: n.gid,
                        mem_mib: n.mem_mib,
                        cores: n.cores,
                        clock_skew_ms: n.clock_skew_ms,
                        clock_drift_ppm: n.clock_drift_ppm,
                        files: n.files,
                    })
                    .collect(),
            };

            // Environment variables from `rewind cluster check` override defaults
            // so any user script works seamlessly under schedule/nemesis exploration.
            let seed = env_u64("REWIND_SEED").or(p.seed).unwrap_or(0);
            let schedule = env_u64("REWIND_SCHEDULE").or(p.schedule).unwrap_or(0);
            let nemesis_enabled = env_bool("REWIND_NEMESIS")
                .or(p.nemesis_enabled)
                .unwrap_or(false);
            let from_ms = env_u64("REWIND_FAULT_FROM_MS").or(p.fault_window_from_ms);
            let until_ms = env_u64("REWIND_FAULT_UNTIL_MS").or(p.fault_window_until_ms);
            let take_kf = env_bool("REWIND_KEYFRAMES")
                .or(p.keyframes)
                .unwrap_or(true);

            let opts = ClusterRunOptions {
                seed,
                schedule,
                fault_window_from_ns: from_ms.map_or(0, |m| m.saturating_mul(1_000_000)),
                fault_window_until_ns: until_ms.map_or(u64::MAX, |m| m.saturating_mul(1_000_000)),
                epoch: p.epoch,
                cpu: defaults.cpu,
                clock: defaults.clock,
                mem_mib_override: None,
                cores_override: None,
                max_virtual_ms_override: None,
                nemesis_enabled,
                keyframes: if take_kf {
                    Keyframes::Take
                } else {
                    Keyframes::Skip
                },
                echo: p.echo,
                wall_timeout: None,
                parent: None,
            };

            let s = ClusterSession::start(
                defaults.home.clone(),
                &defaults.guest,
                &spec,
                &defaults.workdir,
                &opts,
            )?;
            let status = s.status();
            *session = Some(s);
            Ok(serde_json::to_value(status)?)
        }
        "cluster.status" => {
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.status())?)
        }
        "cluster.advance" => {
            #[derive(Deserialize, Default)]
            struct P {
                ms: Option<u64>,
                us: Option<u64>,
                ns: Option<u64>,
                until_ms: Option<u64>,
                wall_timeout_ms: Option<u64>,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let delta_ns = p
                .ns
                .or_else(|| p.us.map(|u| u.saturating_mul(1_000)))
                .or_else(|| p.ms.map(|m| m.saturating_mul(1_000_000)));
            let until_ns = p.until_ms.map(|m| m.saturating_mul(1_000_000));
            let wall = p.wall_timeout_ms.map(Duration::from_millis);
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.advance(delta_ns, until_ns, wall)?)?)
        }
        "cluster.wait_for" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
                contains: String,
                stream: Option<String>,
                timeout_ms: Option<u64>,
                #[serde(default)]
                from_start: bool,
                wall_timeout_ms: Option<u64>,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.wait_for")?;
            let stream = match p.stream.as_deref() {
                Some(st) => LogStream::parse(st)?,
                None => LogStream::Output,
            };
            let timeout_ns = p.timeout_ms.map(|m| m.saturating_mul(1_000_000));
            let wall = p.wall_timeout_ms.map(Duration::from_millis);
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.wait_for(
                &p.node,
                &p.contains,
                stream,
                timeout_ns,
                p.from_start,
                wall,
            )?)?)
        }
        "cluster.wait_exit" => {
            #[derive(Deserialize, Default)]
            struct P {
                node: Option<String>,
                timeout_ms: Option<u64>,
                wall_timeout_ms: Option<u64>,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let timeout_ns = p.timeout_ms.map(|m| m.saturating_mul(1_000_000));
            let wall = p.wall_timeout_ms.map(Duration::from_millis);
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.wait_exit(
                p.node.as_deref(),
                timeout_ns,
                wall,
            )?)?)
        }
        "cluster.partition" => {
            #[derive(Deserialize)]
            struct P {
                a: Vec<String>,
                b: Option<Vec<String>>,
                #[serde(default = "default_true")]
                symmetric: bool,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.partition")?;
            let s = require_session(session)?;
            let blocked = s.partition(&p.a, p.b.as_deref(), p.symmetric)?;
            let st = s.status();
            Ok(json!({
                "cluster_ns": st.cluster_ns,
                "cluster_step": st.cluster_step,
                "blocked_links": blocked,
            }))
        }
        "cluster.heal" => {
            #[derive(Deserialize, Default)]
            struct P {
                a: Option<Vec<String>>,
                b: Option<Vec<String>>,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let s = require_session(session)?;
            s.heal(p.a.as_deref(), p.b.as_deref())?;
            let st = s.status();
            Ok(json!({
                "cluster_ns": st.cluster_ns,
                "cluster_step": st.cluster_step,
            }))
        }
        "cluster.set_link" => {
            #[derive(Deserialize)]
            struct P {
                from: String,
                to: String,
                latency_ms: Option<u64>,
                latency_us: Option<u64>,
                jitter_ms: Option<u64>,
                jitter_us: Option<u64>,
                drop_ppm: Option<u32>,
                duplicate_ppm: Option<u32>,
                #[serde(default = "default_true")]
                bidirectional: bool,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.set_link")?;
            let latency_ns = p
                .latency_us
                .map(|u| u.saturating_mul(1_000))
                .or_else(|| p.latency_ms.map(|m| m.saturating_mul(1_000_000)));
            let jitter_ns = p
                .jitter_us
                .map(|u| u.saturating_mul(1_000))
                .or_else(|| p.jitter_ms.map(|m| m.saturating_mul(1_000_000)));
            let s = require_session(session)?;
            s.set_link(
                &p.from,
                &p.to,
                latency_ns,
                jitter_ns,
                p.drop_ppm,
                p.duplicate_ppm,
                p.bidirectional,
            )?;
            let st = s.status();
            Ok(json!({
                "cluster_ns": st.cluster_ns,
                "cluster_step": st.cluster_step,
            }))
        }
        "cluster.crash" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.crash")?;
            let s = require_session(session)?;
            let boot_count = s.crash(&p.node)?;
            let st = s.status();
            Ok(json!({
                "cluster_ns": st.cluster_ns,
                "cluster_step": st.cluster_step,
                "boot_count": boot_count,
            }))
        }
        "cluster.restart" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.restart")?;
            let s = require_session(session)?;
            let boot_count = s.restart(&p.node)?;
            let st = s.status();
            Ok(json!({
                "cluster_ns": st.cluster_ns,
                "cluster_step": st.cluster_step,
                "boot_count": boot_count,
            }))
        }
        "cluster.logs" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
                stream: Option<String>,
                #[serde(default)]
                since: usize,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.logs")?;
            let stream = match p.stream.as_deref() {
                Some(st) => LogStream::parse(st)?,
                None => LogStream::Output,
            };
            let s = require_session(session)?;
            let (lines, next_offset) = s.logs(&p.node, stream, p.since)?;
            Ok(json!({ "lines": lines, "next_offset": next_offset }))
        }
        "cluster.net_events" => {
            #[derive(Deserialize, Default)]
            struct P {
                #[serde(default)]
                since: usize,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let s = require_session(session)?;
            let (events, next_offset) = s.net_events(p.since);
            Ok(json!({ "events": events, "next_offset": next_offset }))
        }
        "cluster.cat" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
                path: String,
                pid: Option<u32>,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.cat")?;
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.cat(&p.node, &p.path, p.pid)?)?)
        }
        "cluster.files" => {
            #[derive(Deserialize)]
            struct P {
                node: String,
                path: String,
                pid: Option<u32>,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.files")?;
            let s = require_session(session)?;
            Ok(serde_json::to_value(s.files(&p.node, &p.path, p.pid)?)?)
        }
        "cluster.snapshot" => {
            #[derive(Deserialize, Default)]
            struct P {
                name: Option<String>,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let s = require_session(session)?;
            let (snapshot, cluster_ns, cluster_step) = s.snapshot(p.name)?;
            Ok(json!({
                "snapshot": snapshot,
                "cluster_ns": cluster_ns,
                "cluster_step": cluster_step,
            }))
        }
        "cluster.restore" => {
            #[derive(Deserialize)]
            struct P {
                snapshot: String,
            }
            let p: P = serde_json::from_value(params).context("invalid params for cluster.restore")?;
            let s = require_session(session)?;
            let (cluster_ns, cluster_step) = s.restore(&p.snapshot)?;
            Ok(json!({
                "snapshot": p.snapshot,
                "cluster_ns": cluster_ns,
                "cluster_step": cluster_step,
            }))
        }
        "cluster.finish" => {
            #[derive(Deserialize, Default)]
            struct P {
                verdict: Option<String>,
                reason: Option<String>,
            }
            let p: P = serde_json::from_value(params).unwrap_or_default();
            let verdict_override = match p.verdict.as_deref() {
                Some("passed" | "pass") => Some(true),
                Some("failed" | "fail") => Some(false),
                Some(other) => bail!("unknown verdict {other:?} (expected \"passed\" or \"failed\")"),
                None => None,
            };
            let s = require_session(session)?;
            let run = s.finish(verdict_override, p.reason)?;
            let outcome = run.manifest.outcome.clone();
            Ok(json!({
                "id": run.manifest.id.as_str(),
                "name": run.manifest.name,
                "verdict": if run.manifest.verdict.passed() { "passed" } else { "failed" },
                "trace_hash": run.manifest.trace_hash,
                "outcome": outcome,
            }))
        }
        other => bail!("unknown RPC method {other:?}"),
    }
}

fn require_session(session: &mut Option<ClusterSession>) -> Result<&mut ClusterSession> {
    session
        .as_mut()
        .context("no active cluster session; call cluster.start first")
}

fn default_true() -> bool {
    true
}
