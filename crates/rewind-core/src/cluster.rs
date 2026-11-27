//! Multi-node deterministic cluster engine: spec parsing, recording,
//! replaying, cluster keyframes, node seeking, forking, and schedule/fault
//! exploration (`rewind cluster check`).

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rewind_init::{Job, JobFile, NetConfig, Output, Root, Sandbox};
use rewind_store::Store;
use rewind_trace::cluster::{
    CLUSTER_KEYFRAME_EXT, CLUSTER_KEYFRAMES_DIR, CLUSTER_MANIFEST, CLUSTER_MANIFEST_VERSION,
    CLUSTER_NETWORK_LOG, CLUSTER_NODES_DIR, ClusterManifest, ClusterNodeManifest,
    ClusterNodeResult, ClusterParent, ClusterRunOutcome, ClusterSpecFile, ClusterVerdict,
};
use rewind_trace::ending::ExitStatus;
use rewind_trace::manifest::{MANIFEST, MANIFEST_VERSION, Manifest, RunId, RunOutcome, Source, Spec};
use rewind_trace::schedule::StallMax;
use rewind_trace::stop::Stop as TraceStop;
use rewind_trace::verdict::{Reason, Verdict};
use rewind_trace::{Event, EventKind, Trace, TraceWriter};
use rewind_vmm::cluster::{
    Cluster, ClusterConfig, ClusterKeyframe, ClusterNodeConfig, ClusterObserver, ClusterStop,
    DEFAULT_SLICE_NS, NodeRole, StopWhen,
};
use rewind_vmm::net::{
    DEFAULT_LATENCY_NS, FaultKind, LinkOverride, NemesisConfig, NetEvent, NetSwitchConfig,
    ScheduledFault, mac_for_node,
};
use rewind_vmm::{ClockSource, CpuModel, DirtyLog, Machine, NetDeviceConfig, Preemption};

use crate::home::{Guest, Home};
use crate::keyframes::{ReadPages, StorePages};
use crate::run::{
    BASE_CMDLINE, CompareMachine, DEFAULT_CORES, DEFAULT_QUANTUM, Keyframes, Run, SpecExt,
    default_epoch,
};

/// Default per-node RAM in MiB for cluster runs.
pub const DEFAULT_CLUSTER_MEM_MIB: u64 = 256;

/// Default maximum virtual time (60s) if the spec does not specify one.
pub const DEFAULT_MAX_VIRTUAL_MS: u64 = 60_000;

/// Virtual-time interval between recorded cluster keyframes (500 ms virtual time).
pub(crate) const CLUSTER_KEYFRAME_INTERVAL_NS: u64 = 500_000_000;

/// Magic header for `.ckf` cluster keyframe files.
const CKF_MAGIC: &[u8; 8] = b"rwckeyf\0";
const CKF_VERSION: u32 = 1;

/// Options controlling how a cluster spec is resolved and executed.
#[derive(Clone, Debug)]
pub struct ClusterRunOptions {
    pub seed: u64,
    pub schedule: u64,
    pub fault_window_from_ns: u64,
    pub fault_window_until_ns: u64,
    pub epoch: Option<u64>,
    pub cpu: CpuModel,
    pub clock: ClockSource,
    pub mem_mib_override: Option<u64>,
    pub cores_override: Option<u32>,
    pub max_virtual_ms_override: Option<u64>,
    pub nemesis_enabled: bool,
    pub keyframes: Keyframes,
    pub echo: bool,
    pub wall_timeout: Option<Duration>,
    pub parent: Option<ClusterParent>,
}

impl Default for ClusterRunOptions {
    fn default() -> ClusterRunOptions {
        ClusterRunOptions {
            seed: 0,
            schedule: 0,
            fault_window_from_ns: 0,
            fault_window_until_ns: u64::MAX,
            epoch: None,
            cpu: CpuModel::V3,
            clock: ClockSource::Exits,
            mem_mib_override: None,
            cores_override: None,
            max_virtual_ms_override: None,
            nemesis_enabled: false,
            keyframes: Keyframes::Take,
            echo: false,
            wall_timeout: None,
            parent: None,
        }
    }
}

/// Parses a JSON cluster spec file.
pub fn parse_spec_file(path: &Path) -> Result<ClusterSpecFile> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading cluster spec {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing JSON cluster spec {}", path.display()))
}

/// Encodes a [`ClusterKeyframe`] to bytes with its versioned magic header.
pub fn encode_keyframe(ckf: &ClusterKeyframe) -> Result<Vec<u8>> {
    let mut out = CKF_MAGIC.to_vec();
    out.extend_from_slice(&CKF_VERSION.to_le_bytes());
    out.extend_from_slice(&bincode::serialize(ckf)?);
    Ok(out)
}

/// Decodes a [`ClusterKeyframe`] from `.ckf` bytes.
pub fn decode_keyframe(bytes: &[u8]) -> Result<ClusterKeyframe> {
    let header_len = CKF_MAGIC.len() + 4;
    if bytes.len() < header_len || &bytes[..CKF_MAGIC.len()] != CKF_MAGIC {
        bail!("not a valid cluster keyframe file");
    }
    let version = u32::from_le_bytes(
        bytes[CKF_MAGIC.len()..header_len]
            .try_into()
            .expect("four bytes"),
    );
    if version != CKF_VERSION {
        bail!("cluster keyframe version {version} is not supported (expected {CKF_VERSION})");
    }
    Ok(bincode::deserialize(&bytes[header_len..])?)
}

/// Prepared, deterministic cluster ready to boot and execute.
#[derive(Clone, Debug)]
pub struct PreparedCluster {
    pub id: RunId,
    pub manifest: ClusterManifest,
    pub cluster_config: ClusterConfig,
}

fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .with_context(|| format!("subnet {cidr:?} must be CIDR like 10.0.0.0/24"))?;
    let ip: Ipv4Addr = ip_str
        .parse()
        .with_context(|| format!("invalid subnet IP {ip_str:?}"))?;
    let prefix: u8 = prefix_str
        .parse()
        .with_context(|| format!("invalid subnet prefix {prefix_str:?}"))?;
    if prefix > 30 {
        bail!("subnet prefix /{prefix} is too narrow for a multi-node cluster");
    }
    Ok((ip, prefix))
}

pub(crate) fn parse_role(s: Option<&str>) -> Result<NodeRole> {
    match s.unwrap_or("service") {
        "service" => Ok(NodeRole::Service),
        "driver" => Ok(NodeRole::Driver),
        other => bail!("unknown node role {other:?}; expected \"service\" or \"driver\""),
    }
}

fn role_str(r: NodeRole) -> &'static str {
    match r {
        NodeRole::Service => "service",
        NodeRole::Driver => "driver",
    }
}

pub(crate) fn parse_stop_when(s: Option<&str>, has_driver: bool) -> Result<StopWhen> {
    match s {
        None => Ok(if has_driver {
            StopWhen::Driver
        } else {
            StopWhen::All
        }),
        Some("all") => Ok(StopWhen::All),
        Some("any") => Ok(StopWhen::Any),
        Some("driver") => Ok(StopWhen::Driver),
        Some(other) => bail!("unknown stop_when {other:?}; expected \"all\", \"any\", or \"driver\""),
    }
}

fn stop_when_str(s: StopWhen) -> &'static str {
    match s {
        StopWhen::All => "all",
        StopWhen::Any => "any",
        StopWhen::Driver => "driver",
    }
}

/// Resolves a node root path (directory, `.erofs`, or `.tar`) into a cached
/// EROFS image in `home`, identical to `rewind run --root`.
fn resolve_root_image(home: &Home, root: &Path) -> Result<(PathBuf, String)> {
    if root.is_dir() {
        let key = crate::image::dir_key(root)?;
        let out = home.images().join(format!("dir-{}.erofs", &key[..32]));
        if !out.exists() {
            let tmp = crate::image::temp_beside(&out);
            crate::image::from_dir(root, &tmp)?;
            crate::image::place(&tmp, &out)?;
        }
        let hash = crate::image::hash_file(&out)?;
        return Ok((out, hash));
    }
    if root.extension().is_some_and(|e| e == "erofs") {
        let hash = crate::image::hash_file(root)?;
        return Ok((root.to_path_buf(), hash));
    }
    let key = crate::image::hash_file(root)?;
    let out = home.images().join(format!("tar-{}.erofs", &key[..32]));
    if !out.exists() {
        let tmp = crate::image::temp_beside(&out);
        crate::image::from_tar(root, &tmp)?;
        crate::image::place(&tmp, &out)?;
    }
    let hash = crate::image::hash_file(&out)?;
    Ok((out, hash))
}

/// Resolves Nix installables or `/nix/store/...` paths into a cached EROFS
/// store image in `home` and returns `(image_path, image_hash, bin_dirs)`.
fn resolve_nix_packages(
    home: &Home,
    installables: &[String],
    cache: &mut HashMap<Vec<String>, (PathBuf, String, Vec<String>)>,
) -> Result<(PathBuf, String, Vec<String>)> {
    let mut cache_key = installables.to_vec();
    cache_key.sort();
    cache_key.dedup();
    if let Some(hit) = cache.get(&cache_key) {
        return Ok(hit.clone());
    }

    let (outputs, closure) = crate::nix::packages(installables)?;
    let bins: Vec<String> = outputs
        .iter()
        .map(|o| o.join("bin"))
        .filter(|b| b.is_dir())
        .map(|b| b.display().to_string())
        .collect();

    let key = blake3::hash(
        closure
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    )
    .to_hex();
    let out = home.images().join(format!("nix-{}.erofs", &key[..32]));
    if !out.exists() {
        let tmp = crate::image::temp_beside(&out);
        let store = Path::new(crate::nix::STORE);
        crate::image::from_store_paths(store, &closure, &tmp)?;
        crate::image::place(&tmp, &out)?;
    }
    let hash = crate::image::hash_file(&out)?;
    let resolved = (out, hash, bins);
    cache.insert(cache_key, resolved.clone());
    Ok(resolved)
}

fn merge_packages(cluster_pkgs: &[String], node_pkgs: &[String]) -> Vec<String> {
    let mut merged = Vec::with_capacity(cluster_pkgs.len() + node_pkgs.len());
    for p in cluster_pkgs.iter().chain(node_pkgs.iter()) {
        if !merged.contains(p) {
            merged.push(p.clone());
        }
    }
    merged
}

fn apply_path_env(env: &mut Vec<(String, String)>, extra_bins: &[String]) {
    const DEFAULT_PATH: &str =
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/rewind/tools/bin";
    if let Some((_, val)) = env.iter_mut().find(|(k, _)| k == "PATH") {
        if !extra_bins.is_empty() {
            let prefix = extra_bins.join(":");
            *val = format!("{prefix}:{val}");
        }
    } else if extra_bins.is_empty() {
        env.push(("PATH".into(), DEFAULT_PATH.into()));
    } else {
        env.push(("PATH".into(), format!("{}:{DEFAULT_PATH}", extra_bins.join(":"))));
    }
}

fn parse_env_list(items: &[String]) -> Result<Vec<(String, String)>> {
    items
        .iter()
        .map(|item| {
            let (k, v) = item
                .split_once('=')
                .with_context(|| format!("env entry {item:?} must be KEY=VALUE"))?;
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Resolves a [`ClusterSpecFile`] into a deterministic [`PreparedCluster`].
pub fn prepare_cluster(
    home: &Home,
    guest: &Guest,
    spec_file: &ClusterSpecFile,
    spec_dir: &Path,
    spec_source: &str,
    opts: &ClusterRunOptions,
) -> Result<PreparedCluster> {
    if spec_file.nodes.is_empty() {
        bail!("cluster spec {:?} defines no nodes", spec_file.name);
    }

    let subnet = spec_file
        .network
        .subnet
        .as_deref()
        .unwrap_or("10.0.0.0/24");
    let (base_ip, prefix_len) = parse_cidr(subnet)?;
    let base_ip_u32 = u32::from(base_ip);

    // 1. Resolve unique names, IPs, MACs, roles, and /etc/hosts table.
    let mut name_to_idx: HashMap<String, usize> = HashMap::new();
    let mut resolved_ips: Vec<String> = Vec::with_capacity(spec_file.nodes.len());
    let mut hosts: Vec<(String, String)> = Vec::with_capacity(spec_file.nodes.len());
    let mut roles: Vec<NodeRole> = Vec::with_capacity(spec_file.nodes.len());

    for (idx, n) in spec_file.nodes.iter().enumerate() {
        if n.name.is_empty()
            || !n
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!(
                "invalid node name {:?}; use ASCII letters, digits, '-' or '_'",
                n.name
            );
        }
        if name_to_idx.insert(n.name.clone(), idx).is_some() {
            bail!("duplicate node name {:?}", n.name);
        }
        if n.command.is_empty() {
            bail!("node {:?} has an empty command", n.name);
        }
        let ip = match &n.ip {
            Some(ip) => {
                let _: Ipv4Addr = ip
                    .parse()
                    .with_context(|| format!("invalid IPv4 {ip:?} on node {}", n.name))?;
                ip.clone()
            }
            None => Ipv4Addr::from(base_ip_u32 + (idx as u32) + 1).to_string(),
        };
        hosts.push((ip.clone(), n.name.clone()));
        resolved_ips.push(ip);
        roles.push(parse_role(n.role.as_deref())?);
    }

    let has_driver = roles.contains(&NodeRole::Driver);
    let stop_when = parse_stop_when(spec_file.stop_when.as_deref(), has_driver)?;
    let epoch = opts.epoch.unwrap_or_else(default_epoch);
    let extras = rewind_vmm::extras_for(opts.cpu)?;
    let default_mem = opts
        .mem_mib_override
        .or(spec_file.mem_mib)
        .unwrap_or(DEFAULT_CLUSTER_MEM_MIB);
    let default_cores = opts
        .cores_override
        .or(spec_file.cores)
        .unwrap_or(DEFAULT_CORES);
    let max_virtual_ns = opts
        .max_virtual_ms_override
        .or(spec_file.max_virtual_ms)
        .or(Some(DEFAULT_MAX_VIRTUAL_MS))
        .map(|ms| ms.saturating_mul(1_000_000));
    let slice_ns = spec_file
        .slice_us
        .map_or(DEFAULT_SLICE_NS, |us| us.saturating_mul(1_000));

    // 2. Build network & fault config.
    let latency_ns = spec_file
        .network
        .latency_us
        .map(|us| us * 1_000)
        .or_else(|| spec_file.network.latency_ms.map(|ms| ms * 1_000_000))
        .unwrap_or(DEFAULT_LATENCY_NS);
    let jitter_ns = spec_file
        .network
        .jitter_us
        .map(|us| us * 1_000)
        .or_else(|| spec_file.network.jitter_ms.map(|ms| ms * 1_000_000))
        .unwrap_or(0);
    // When schedule != 0 and no explicit jitter was set, introduce mild deterministic
    // jitter (25% of base latency) so schedule exploration reorders packets too!
    let effective_jitter_ns = if opts.schedule != 0 && jitter_ns == 0 {
        latency_ns / 4
    } else {
        jitter_ns
    };

    let mut links = Vec::new();
    for l in &spec_file.network.links {
        let &from = name_to_idx
            .get(&l.from)
            .with_context(|| format!("link references unknown node {:?}", l.from))?;
        let &to = name_to_idx
            .get(&l.to)
            .with_context(|| format!("link references unknown node {:?}", l.to))?;
        links.push(LinkOverride {
            from,
            to,
            latency_ns: l
                .latency_us
                .map(|us| us * 1_000)
                .or_else(|| l.latency_ms.map(|ms| ms * 1_000_000)),
            jitter_ns: l
                .jitter_us
                .map(|us| us * 1_000)
                .or_else(|| l.jitter_ms.map(|ms| ms * 1_000_000)),
            drop_ppm: l.drop_ppm,
            duplicate_ppm: None,
        });
    }

    let mut faults = Vec::new();
    for f in &spec_file.faults {
        let at_ns = f.at_ms.saturating_mul(1_000_000);
        let kind = match f.kind.as_str() {
            "partition" => {
                let mut groups = Vec::new();
                for g in &f.groups {
                    let mut idxs = Vec::new();
                    for name in g {
                        let &i = name_to_idx.get(name).with_context(|| {
                            format!("partition fault references unknown node {name:?}")
                        })?;
                        idxs.push(i);
                    }
                    groups.push(idxs);
                }
                FaultKind::Partition { groups }
            }
            "asymmetric_partition" => {
                let mut blocked = Vec::new();
                for (a, b) in &f.blocked {
                    let &ai = name_to_idx
                        .get(a)
                        .with_context(|| format!("fault references unknown node {a:?}"))?;
                    let &bi = name_to_idx
                        .get(b)
                        .with_context(|| format!("fault references unknown node {b:?}"))?;
                    blocked.push((ai, bi));
                }
                FaultKind::AsymmetricPartition { blocked }
            }
            "heal" => FaultKind::Heal,
            "crash" => {
                let name = f
                    .node
                    .as_deref()
                    .context("crash fault requires `node = \"...\"`")?;
                let &i = name_to_idx
                    .get(name)
                    .with_context(|| format!("crash fault references unknown node {name:?}"))?;
                FaultKind::Crash { node: i }
            }
            "restart" => {
                let name = f
                    .node
                    .as_deref()
                    .context("restart fault requires `node = \"...\"`")?;
                let &i = name_to_idx
                    .get(name)
                    .with_context(|| format!("restart fault references unknown node {name:?}"))?;
                FaultKind::Restart { node: i }
            }
            other => bail!("unknown fault kind {other:?}"),
        };
        faults.push(ScheduledFault {
            at_ns,
            at_step: None,
            kind,
        });
    }

    let eligible_service_nodes: Vec<usize> = roles
        .iter()
        .enumerate()
        .filter(|(_, r)| **r == NodeRole::Service)
        .map(|(i, _)| i)
        .collect();
    let nemesis = if let Some(n) = &spec_file.nemesis {
        if opts.schedule != 0 || opts.nemesis_enabled {
            Some(NemesisConfig {
                tick_ns: n.tick_ms.saturating_mul(1_000_000).max(1_000_000),
                partition_prob_ppm: n.partition_prob_ppm,
                heal_prob_ppm: n.heal_prob_ppm,
                crash_prob_ppm: n.crash_prob_ppm,
                restart_prob_ppm: n.restart_prob_ppm,
                max_crashed: n.max_crashed,
                eligible_nodes: eligible_service_nodes,
            })
        } else {
            None
        }
    } else if opts.nemesis_enabled && opts.schedule != 0 {
        // Default nemesis profile when `--nemesis` is passed on CLI without a `[nemesis]` block.
        Some(NemesisConfig {
            tick_ns: 100_000_000,
            partition_prob_ppm: 100_000,
            heal_prob_ppm: 250_000,
            crash_prob_ppm: 40_000,
            restart_prob_ppm: 300_000,
            max_crashed: 1,
            eligible_nodes: eligible_service_nodes,
        })
    } else {
        None
    };

    let switch_seed = mix64(opts.seed ^ opts.schedule.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let net_config = NetSwitchConfig {
        node_count: spec_file.nodes.len(),
        latency_ns,
        jitter_ns: effective_jitter_ns,
        drop_ppm: spec_file.network.drop_ppm.unwrap_or(0),
        duplicate_ppm: spec_file.network.duplicate_ppm.unwrap_or(0),
        seed: switch_seed,
        fault_window: opts.fault_window_from_ns..opts.fault_window_until_ns,
        links,
        faults,
        nemesis,
    };

    // 3. Build each node's Spec + Config.
    let mut node_manifests = Vec::with_capacity(spec_file.nodes.len());
    let mut node_configs = Vec::with_capacity(spec_file.nodes.len());
    let mut nix_cache: HashMap<Vec<String>, (PathBuf, String, Vec<String>)> = HashMap::new();

    for (idx, n) in spec_file.nodes.iter().enumerate() {
        let mac = mac_for_node(idx);
        let merged_pkgs = merge_packages(&spec_file.packages, &n.packages);
        if n.root.is_some() && !merged_pkgs.is_empty() {
            bail!(
                "node {:?} cannot specify both `root` and Nix `packages`",
                n.name
            );
        }
        let (image, image_hash, root_kind, nix_bins) = if !merged_pkgs.is_empty() {
            let (img, hash, bins) = resolve_nix_packages(home, &merged_pkgs, &mut nix_cache)?;
            (Some(img), Some(hash), Root::Store, bins)
        } else if let Some(p) = &n.root {
            let resolved = if p.is_absolute() {
                p.clone()
            } else {
                spec_dir.join(p)
            };
            let (img, hash) = resolve_root_image(home, &resolved)?;
            (Some(img), Some(hash), Root::Image, Vec::new())
        } else {
            (None, None, Root::Initramfs, Vec::new())
        };
        let mut env = parse_env_list(&n.env)?;
        apply_path_env(&mut env, &nix_bins);
        let files: Vec<JobFile> = n
            .files
            .iter()
            .map(|f| JobFile {
                path: f.path.clone(),
                contents: f.contents.clone(),
            })
            .collect();
        let default_cwd = if root_kind == Root::Store {
            crate::nix::BUILD_DIR
        } else {
            "/"
        };
        let job = Job {
            program: None,
            argv: n.command.clone(),
            env,
            cwd: n.cwd.clone().unwrap_or_else(|| default_cwd.into()),
            uid: n.uid.unwrap_or(0),
            gid: n.gid.unwrap_or(0),
            hostname: n.name.clone(),
            root: root_kind,
            files,
            outputs: Vec::new(),
            output: Output::Plain,
            sandbox: Sandbox::Shared,
            net: Some(NetConfig {
                interface: "eth0".into(),
                ip: resolved_ips[idx].clone(),
                prefix_len,
                gateway: None,
                hosts: hosts.clone(),
            }),
        };

        let node_seed = mix64(opts.seed ^ (idx as u64).wrapping_add(1));
        let node_schedule = if opts.schedule == 0 {
            0
        } else {
            mix64(opts.schedule ^ ((idx as u64 + 1).wrapping_mul(0x517c_c1b7_2722_0a95))).max(1)
        };
        let spec = Spec {
            kernel: guest.kernel.clone(),
            initrd: guest.initrd.clone(),
            kernel_debug: guest.kernel_debug.clone(),
            image,
            image_hash,
            mem_mib: n.mem_mib.unwrap_or(default_mem),
            cores: n.cores.unwrap_or(default_cores),
            seed: node_seed,
            epoch,
            quantum: DEFAULT_QUANTUM,
            schedule: node_schedule,
            schedule_from: 0,
            schedule_until: u64::MAX,
            stall_max: StallMax::default(),
            inherited_schedules: Vec::new(),
            reseeds: Vec::new(),
            cpu: opts.cpu,
            clock: opts.clock,
            preemption: Preemption::AtExits,
            extras,
            cmdline: BASE_CMDLINE.to_string(),
            job,
        };
        let mut cfg = spec.config()?;
        cfg.net = Some(NetDeviceConfig { mac });

        let skew_ns = n.clock_skew_ms.saturating_mul(1_000_000);
        node_manifests.push(ClusterNodeManifest {
            name: n.name.clone(),
            ip: resolved_ips[idx].clone(),
            mac,
            role: role_str(roles[idx]).to_string(),
            clock_skew_ns: skew_ns,
            clock_drift_ppm: n.clock_drift_ppm,
            spec,
        });
        node_configs.push(ClusterNodeConfig {
            name: n.name.clone(),
            config: cfg,
            role: roles[idx],
            clock_skew_ns: skew_ns,
            clock_drift_ppm: n.clock_drift_ppm,
            base_seed: node_seed,
        });
    }

    let network_json = serde_json::to_value(&net_config)?;
    let id = compute_cluster_id(&node_manifests, &network_json, stop_when, max_virtual_ns, slice_ns);
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    let manifest = ClusterManifest {
        version: CLUSTER_MANIFEST_VERSION,
        id: id.clone(),
        name: spec_file.name.clone(),
        created,
        spec_source: spec_source.to_string(),
        seed: opts.seed,
        schedule: opts.schedule,
        fault_window_from_ns: opts.fault_window_from_ns,
        fault_window_until_ns: opts.fault_window_until_ns,
        epoch,
        cpu: opts.cpu,
        clock: opts.clock,
        stop_when: stop_when_str(stop_when).to_string(),
        max_virtual_ns,
        stop_at_step: None,
        slice_ns,
        network_json,
        nodes: node_manifests,
        parent: opts.parent.clone(),
        outcome: None,
        verdict: ClusterVerdict::Unfinished,
        trace_hash: None,
        recorded_by: crate::VERSION.to_string(),
    };

    let cluster_config = ClusterConfig {
        nodes: node_configs,
        network: net_config,
        stop_when,
        max_virtual_ns,
        stop_at_step: None,
        slice_ns,
    };

    Ok(PreparedCluster {
        id,
        manifest,
        cluster_config,
    })
}

pub(crate) fn compute_cluster_id(
    nodes: &[ClusterNodeManifest],
    network_json: &serde_json::Value,
    stop_when: StopWhen,
    max_virtual_ns: Option<u64>,
    slice_ns: u64,
) -> RunId {
    let mut hasher = blake3::Hasher::new();
    for n in nodes {
        hasher.update(n.name.as_bytes());
        hasher.update(n.ip.as_bytes());
        hasher.update(&n.mac);
        hasher.update(n.role.as_bytes());
        hasher.update(&n.clock_skew_ns.to_le_bytes());
        hasher.update(&n.clock_drift_ppm.to_le_bytes());
        hasher.update(n.spec.id().as_bytes());
    }
    hasher.update(network_json.to_string().as_bytes());
    hasher.update(stop_when_str(stop_when).as_bytes());
    if let Some(m) = max_virtual_ns {
        hasher.update(&m.to_le_bytes());
    }
    hasher.update(&slice_ns.to_le_bytes());
    let hex = hasher.finalize().to_hex();
    RunId::of_hash(&hex).expect("BLAKE3 hex is longer than RunId")
}

/// Rebuilds a [`ClusterConfig`] from a persisted [`ClusterManifest`].
pub fn config_from_manifest(manifest: &ClusterManifest) -> Result<ClusterConfig> {
    let network: NetSwitchConfig = serde_json::from_value(manifest.network_json.clone())
        .context("decoding cluster network configuration from manifest")?;
    let stop_when = parse_stop_when(Some(&manifest.stop_when), false)?;
    let mut nodes = Vec::with_capacity(manifest.nodes.len());
    for n in &manifest.nodes {
        let mut cfg = n.spec.config()?;
        cfg.net = Some(NetDeviceConfig { mac: n.mac });
        nodes.push(ClusterNodeConfig {
            name: n.name.clone(),
            config: cfg,
            role: parse_role(Some(&n.role))?,
            clock_skew_ns: n.clock_skew_ns,
            clock_drift_ppm: n.clock_drift_ppm,
            base_seed: n.spec.seed,
        });
    }
    Ok(ClusterConfig {
        nodes,
        network,
        stop_when,
        max_virtual_ns: manifest.max_virtual_ns,
        stop_at_step: manifest.stop_at_step,
        slice_ns: manifest.slice_ns,
    })
}

/// Recorder that writes per-node `trace.bin` files and `network.jsonl`.
struct ClusterRecorder {
    node_names: Vec<String>,
    traces: Vec<TraceWriter<File>>,
    statuses: Vec<Option<i32>>,
    net_log: BufWriter<File>,
    echo: bool,
    error: Option<anyhow::Error>,
}

impl ClusterRecorder {
    fn create(dir: &Path, node_names: Vec<String>, echo: bool) -> Result<ClusterRecorder> {
        let mut traces = Vec::with_capacity(node_names.len());
        for name in &node_names {
            let ndir = dir.join(CLUSTER_NODES_DIR).join(name);
            fs::create_dir_all(&ndir)?;
            let f = File::create(ndir.join(rewind_trace::manifest::TRACE))?;
            traces.push(TraceWriter::new(f));
        }
        let net_file = File::create(dir.join(CLUSTER_NETWORK_LOG))?;
        Ok(ClusterRecorder {
            statuses: vec![None; node_names.len()],
            node_names,
            traces,
            net_log: BufWriter::new(net_file),
            echo,
            error: None,
        })
    }

    fn flush(mut self) -> Result<Vec<Option<i32>>> {
        if let Some(err) = self.error {
            return Err(err);
        }
        for t in self.traces {
            t.finish()?;
        }
        self.net_log.flush()?;
        Ok(self.statuses)
    }
}

impl ClusterObserver for ClusterRecorder {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        if let Err(e) = self.traces[node].record(step, record) {
            self.error.get_or_insert(e.into());
        }
        let Ok(event) = Event::decode(step, record) else {
            return;
        };
        match &event.kind {
            EventKind::Output { fd, bytes } if self.echo => {
                let text = String::from_utf8_lossy(bytes);
                let name = &self.node_names[node];
                for line in text.lines() {
                    if *fd == 2 {
                        eprintln!("[{name}] {line}");
                    } else {
                        println!("[{name}] {line}");
                    }
                }
            }
            EventKind::Mark { .. } => {
                if let Some(rewind_init::Mark::Exit { status }) = event.init_mark() {
                    self.statuses[node] = Some(status);
                }
            }
            _ => {}
        }
    }

    fn net_event(&mut self, event: &NetEvent) {
        if let Ok(line) = serde_json::to_string(event)
            && let Err(e) = writeln!(self.net_log, "{line}")
        {
            self.error.get_or_insert(e.into());
        }
    }
}

/// A recorded multi-node cluster run on disk.
#[derive(Clone, Debug)]
pub struct ClusterRun {
    pub dir: PathBuf,
    pub manifest: ClusterManifest,
}

impl ClusterRun {
    /// Opens a cluster run directory.
    pub fn open(dir: &Path) -> Result<ClusterRun> {
        let bytes = fs::read(dir.join(CLUSTER_MANIFEST))
            .with_context(|| format!("reading {}", dir.join(CLUSTER_MANIFEST).display()))?;
        let manifest: ClusterManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", dir.join(CLUSTER_MANIFEST).display()))?;
        if manifest.version != CLUSTER_MANIFEST_VERSION {
            bail!(
                "cluster manifest format {}; this build reads format {CLUSTER_MANIFEST_VERSION}",
                manifest.version
            );
        }
        Ok(ClusterRun {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    /// Lists all cluster runs in `home`, newest first.
    pub fn list(home: &Home) -> Result<Vec<ClusterRun>> {
        let mut runs = Vec::new();
        let entries = match fs::read_dir(home.clusters()) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(runs),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Ok(run) = ClusterRun::open(&entry.path())
            {
                runs.push(run);
            }
        }
        runs.sort_by_key(|r| std::cmp::Reverse(r.manifest.created));
        Ok(runs)
    }

    /// Finds a cluster run by id, prefix, name, `@N`, or directory path.
    pub fn find(home: &Home, query: &str) -> Result<ClusterRun> {
        let as_path = Path::new(query);
        if as_path.join(CLUSTER_MANIFEST).is_file() {
            return ClusterRun::open(as_path);
        }
        let all = ClusterRun::list(home)?;
        if let Some(rest) = query.strip_prefix('@') {
            let n: usize = if rest.is_empty() {
                1
            } else {
                rest.parse()
                    .with_context(|| format!("invalid cluster index {query:?}"))?
            };
            if n == 0 || n > all.len() {
                bail!("no cluster run {query} ({} recorded)", all.len());
            }
            return Ok(all[n - 1].clone());
        }
        let matches: Vec<&ClusterRun> = all
            .iter()
            .filter(|r| r.manifest.id.as_str().starts_with(query))
            .collect();
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if matches.len() > 1 {
            bail!(
                "cluster prefix {query:?} matches {} runs; give more digits",
                matches.len()
            );
        }
        if let Some(found) = all.iter().find(|r| r.manifest.name == query) {
            return Ok(found.clone());
        }
        bail!("no cluster run matching {query:?}")
    }

    /// Opens one node's standard [`Run`] view inside this cluster directory
    /// (so `rewind log`, `events`, `ps`, `threads` read it directly!).
    pub fn node_run(&self, node_name: &str) -> Result<Run> {
        let ndir = self.dir.join(CLUSTER_NODES_DIR).join(node_name);
        if !ndir.is_dir() {
            let names: Vec<&str> = self.manifest.nodes.iter().map(|n| n.name.as_str()).collect();
            bail!(
                "cluster {} has no node {node_name:?} (nodes: {})",
                self.manifest.id,
                names.join(", ")
            );
        }
        Run::open(&ndir)
    }

    /// Reads all recorded [`NetEvent`]s from `network.jsonl`.
    pub fn net_events(&self) -> Result<Vec<NetEvent>> {
        let file = File::open(self.dir.join(CLUSTER_NETWORK_LOG))?;
        let mut events = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                events.push(serde_json::from_str(&line)?);
            }
        }
        Ok(events)
    }

    /// Executes and records a [`PreparedCluster`] into `home`.
    pub fn record(
        home: &Home,
        prepared: &PreparedCluster,
        keyframes: Keyframes,
        echo: bool,
        wall_timeout: Option<Duration>,
    ) -> Result<ClusterRun> {
        let _in_use = home.in_use()?;
        let dir = home.clusters().join(prepared.id.as_str());
        fs::create_dir_all(&dir)?;

        let dirty_log = match keyframes {
            Keyframes::Take | Keyframes::AtExit => DirtyLog::On,
            Keyframes::Skip => DirtyLog::Off,
        };
        let started = Instant::now();
        let deadline = wall_timeout.map(|d| started + d);

        let mut cluster = Cluster::boot(prepared.cluster_config.clone(), dirty_log)?;
        let node_names: Vec<String> = prepared
            .manifest
            .nodes
            .iter()
            .map(|n| n.name.clone())
            .collect();
        let mut recorder = ClusterRecorder::create(&dir, node_names.clone(), echo)?;

        let mut store = if keyframes == Keyframes::Take {
            Some(Store::open(&home.store())?)
        } else {
            None
        };
        let mut last_ckf_step: Option<u64> = None;
        let mut node_kf_parents: Vec<Option<u64>> = vec![None; node_names.len()];
        let mut node_kf_boots: Vec<u32> = vec![1; node_names.len()];
        let mut next_kf_ns = CLUSTER_KEYFRAME_INTERVAL_NS;

        let outcome = loop {
            let slice_target = if keyframes == Keyframes::Take {
                Some(next_kf_ns)
            } else {
                None
            };
            let out = cluster.run(&mut recorder, slice_target, deadline)?;
            if out.stop != ClusterStop::Paused {
                // Capture a final keyframe when keyframes are enabled and the
                // cluster advanced past the previous keyframe's step.
                if let Some(st) = store.as_mut()
                    && last_ckf_step != Some(cluster.cluster_step())
                {
                    let mut pages = StorePages(st);
                    let parents = compute_node_parents(&cluster, &node_kf_parents, &node_kf_boots);
                    let ckf = cluster.keyframe(&mut pages, last_ckf_step, &parents)?;
                    save_cluster_keyframe(&dir, &ckf)?;
                }
                break out;
            }
            if let Some(st) = store.as_mut() {
                if last_ckf_step != Some(cluster.cluster_step()) {
                    let mut pages = StorePages(st);
                    let parents = compute_node_parents(&cluster, &node_kf_parents, &node_kf_boots);
                    let ckf = cluster.keyframe(&mut pages, last_ckf_step, &parents)?;
                    save_cluster_keyframe(&dir, &ckf)?;
                    last_ckf_step = Some(ckf.cluster_step);
                    for (i, nk) in ckf.nodes.iter().enumerate() {
                        if let Some(kf) = &nk.keyframe {
                            node_kf_parents[i] = Some(kf.step);
                            node_kf_boots[i] = nk.boot_count;
                        } else {
                            node_kf_parents[i] = None;
                        }
                    }
                }
                next_kf_ns = cluster.cluster_ns().saturating_add(CLUSTER_KEYFRAME_INTERVAL_NS);
            }
        };
        if let Some(st) = store {
            st.sync()?;
        }

        let statuses = recorder.flush()?;
        let wall_ms = started.elapsed().as_millis() as u64;

        // Write standard per-node manifests and compute verdicts + combined trace hash.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&fs::read(dir.join(CLUSTER_NETWORK_LOG))?);

        let mut node_results = Vec::with_capacity(node_names.len());
        let mut failure_reasons = Vec::new();
        let stop_when = parse_stop_when(Some(&prepared.manifest.stop_when), false)?;

        for (idx, nm) in prepared.manifest.nodes.iter().enumerate() {
            let ndir = dir.join(CLUSTER_NODES_DIR).join(&nm.name);
            let trace_bytes = fs::read(ndir.join(rewind_trace::manifest::TRACE))?;
            hasher.update(&trace_bytes);
            let node_trace_hash = blake3::hash(&trace_bytes).to_hex().to_string();
            let records = rewind_trace::Records::new(std::io::Cursor::new(&trace_bytes))?
                .collect::<std::io::Result<Vec<_>>>()?;
            let trace = Trace::decode(&records)?;

            let no = &outcome.nodes[idx];
            let status = statuses[idx];
            let role = parse_role(Some(&nm.role))?;

            // A service node still running when the driver finished cleanly is not a failure;
            // only check assertions and fatal signals on it.
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
                name: format!("{}/{}", prepared.manifest.name, nm.name),
                created: prepared.manifest.created,
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
                    status,
                    wall_ms,
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

        if outcome.stop == ClusterStop::Stalled {
            failure_reasons.push("cluster stalled (deadlock: all nodes idle with no timers or packets)".into());
        } else if outcome.stop == ClusterStop::TimedOut {
            failure_reasons.push("cluster timed out on host wall clock".into());
        } else if outcome.stop == ClusterStop::MaxVirtualTime && stop_when != StopWhen::All {
            failure_reasons.push("cluster reached max_virtual_ms before driver completed".into());
        }

        let verdict = if failure_reasons.is_empty() {
            ClusterVerdict::Passed
        } else {
            ClusterVerdict::Failed {
                reasons: failure_reasons,
            }
        };

        let mut manifest = prepared.manifest.clone();
        manifest.outcome = Some(ClusterRunOutcome {
            stop: format!("{:?}", outcome.stop),
            cluster_ns: outcome.cluster_ns,
            cluster_step: outcome.cluster_step,
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

        Ok(ClusterRun { dir, manifest })
    }

    /// Replays this cluster run from boot and verifies every node's trace
    /// records and every network/fault event match byte-for-byte.
    pub fn replay(&self, home: &Home, compare: CompareMachine) -> Result<ClusterReplayed> {
        let cfg = config_from_manifest(&self.manifest)?;
        let node_count = self.manifest.nodes.len();

        let mut expected_traces: Vec<Vec<(u64, Vec<u8>)>> = Vec::with_capacity(node_count);
        for n in &self.manifest.nodes {
            let trace_path = self
                .dir
                .join(CLUSTER_NODES_DIR)
                .join(&n.name)
                .join(rewind_trace::manifest::TRACE);
            let records = rewind_trace::Records::new(BufReader::new(File::open(&trace_path)?))?
                .collect::<std::io::Result<Vec<_>>>()?;
            expected_traces.push(records);
        }
        let expected_net = self.net_events()?;

        let dirty_log = if compare == CompareMachine::Never {
            DirtyLog::Off
        } else {
            DirtyLog::On
        };
        let mut cluster = Cluster::boot(cfg, dirty_log)?;

        let mut observer = ReplayObserver {
            actual_nodes: vec![Vec::new(); node_count],
            actual_net: Vec::new(),
        };
        let _outcome = cluster.run(&mut observer, None, None)?;

        let mut node_diffs = Vec::new();
        for (idx, n) in self.manifest.nodes.iter().enumerate() {
            let exp = &expected_traces[idx];
            let act = &observer.actual_nodes[idx];
            if exp != act {
                let first = exp
                    .iter()
                    .zip(act.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| exp.len().min(act.len()));
                node_diffs.push(format!(
                    "node {} diverged at record {} (expected {} records, got {})",
                    n.name,
                    first,
                    exp.len(),
                    act.len()
                ));
            }
        }
        let net_diff = if expected_net != observer.actual_net {
            let first = expected_net
                .iter()
                .zip(observer.actual_net.iter())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| expected_net.len().min(observer.actual_net.len()));
            let exp_ev = expected_net.get(first);
            let act_ev = observer.actual_net.get(first);
            Some(format!(
                "network timeline diverged at event {} (expected {}, got {}): expected {:?}, got {:?}",
                first,
                expected_net.len(),
                observer.actual_net.len(),
                exp_ev,
                act_ev,
            ))
        } else {
            None
        };

        // Optional machine-state comparison at the final cluster keyframe.
        let mut keyframes_compared = 0;
        let mut machine_diff = None;
        if compare != CompareMachine::Never
            && let Some(last_ckf) = load_cluster_keyframes(&self.dir)?.pop()
        {
            let _store = Store::open(&home.store())?;
            if last_ckf.cluster_step == cluster.cluster_step()
                && last_ckf.cluster_ns == cluster.cluster_ns()
            {
                let snap = cluster.switch_snapshot();
                if snap != last_ckf.switch {
                    machine_diff = Some("virtual switch state differs at final keyframe".into());
                } else {
                    keyframes_compared = 1;
                }
            }
        }

        let identical = node_diffs.is_empty() && net_diff.is_none() && machine_diff.is_none();
        Ok(ClusterReplayed {
            identical,
            node_diffs,
            net_diff,
            machine_diff,
            keyframes_compared,
            total_events: observer.actual_nodes.iter().map(Vec::len).sum(),
            net_events: observer.actual_net.len(),
        })
    }

    /// Restores the cluster to the exact causal point where `node_name` reaches
    /// `target_step`, returning that node's live [`Machine`] ready for
    /// inspection (`cat`, `files`, `shell`, `gdb`).
    pub fn seek_node(&self, home: &Home, node_name: &str, target_step: u64) -> Result<(Machine, u64)> {
        let node_idx = self
            .manifest
            .nodes
            .iter()
            .position(|n| n.name == node_name)
            .with_context(|| format!("cluster {} has no node {node_name:?}", self.manifest.id))?;
        let cfg = config_from_manifest(&self.manifest)?;

        // Try restoring from the latest cluster keyframe where node_idx <= target_step.
        let keyframes = load_cluster_keyframes(&self.dir)?;
        let best_idx = keyframes.iter().rposition(|ckf| {
            let nk = &ckf.nodes[node_idx];
            let steps = nk.step_base + nk.keyframe.as_ref().map_or(0, |k| k.step);
            nk.keyframe.is_some() && steps <= target_step
        });

        let mut cluster = if let Some(idx) = best_idx {
            let store = Store::open(&home.store())?;
            let pages = ReadPages(&store);
            let mut node_traces = Vec::with_capacity(self.manifest.nodes.len());
            for n in &self.manifest.nodes {
                let tp = self
                    .dir
                    .join(CLUSTER_NODES_DIR)
                    .join(&n.name)
                    .join(rewind_trace::manifest::TRACE);
                node_traces.push(
                    rewind_trace::Records::new(BufReader::new(File::open(&tp)?))?
                        .collect::<std::io::Result<Vec<_>>>()?,
                );
            }
            Cluster::restore(cfg, &keyframes[..=idx], &pages, &node_traces, DirtyLog::On)?
        } else {
            Cluster::boot(cfg, DirtyLog::On)?
        };

        let mut seeker = SeekNodeObserver {
            target_node: node_idx,
            target_step,
            reached: cluster.node_steps(node_idx) >= target_step,
        };
        if !seeker.reached {
            let _ = cluster.run(&mut seeker, None, None)?;
        }
        let actual_step = cluster.node_steps(node_idx);
        let machine = cluster.take_node_machine(node_idx)?;
        Ok((machine, actual_step))
    }

    /// Forks this cluster run at virtual time `at_ms` under a new `schedule`
    /// (and optional `seed`), returning the recorded fork.
    pub fn fork(
        &self,
        home: &Home,
        at_ms: u64,
        schedule: u64,
        seed: Option<u64>,
        keyframes: Keyframes,
        echo: bool,
    ) -> Result<ClusterRun> {
        let at_ns = at_ms.saturating_mul(1_000_000);
        let mut net_cfg: NetSwitchConfig = serde_json::from_value(self.manifest.network_json.clone())?;
        let new_seed = seed.unwrap_or(self.manifest.seed);
        net_cfg.seed = mix64(new_seed ^ schedule.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        net_cfg.fault_window = at_ns..u64::MAX;
        if schedule != 0 && net_cfg.jitter_ns == 0 {
            net_cfg.jitter_ns = net_cfg.latency_ns / 4;
        }

        let mut nodes_manifest = self.manifest.nodes.clone();
        let mut nodes_cfg = Vec::with_capacity(nodes_manifest.len());
        for (idx, nm) in nodes_manifest.iter_mut().enumerate() {
            if let Some(s) = seed {
                nm.spec.seed = mix64(s ^ (idx as u64).wrapping_add(1));
            }
            nm.spec.schedule = if schedule == 0 {
                0
            } else {
                mix64(schedule ^ ((idx as u64 + 1).wrapping_mul(0x517c_c1b7_2722_0a95))).max(1)
            };
            let mut cfg = nm.spec.config()?;
            cfg.net = Some(NetDeviceConfig { mac: nm.mac });
            nodes_cfg.push(ClusterNodeConfig {
                name: nm.name.clone(),
                config: cfg,
                role: parse_role(Some(&nm.role))?,
                clock_skew_ns: nm.clock_skew_ns,
                clock_drift_ppm: nm.clock_drift_ppm,
                base_seed: nm.spec.seed,
            });
        }

        let stop_when = parse_stop_when(Some(&self.manifest.stop_when), false)?;
        let network_json = serde_json::to_value(&net_cfg)?;
        let id = compute_cluster_id(
            &nodes_manifest,
            &network_json,
            stop_when,
            self.manifest.max_virtual_ns,
            self.manifest.slice_ns,
        );
        let mut manifest = self.manifest.clone();
        manifest.id = id.clone();
        manifest.seed = new_seed;
        manifest.schedule = schedule;
        manifest.fault_window_from_ns = at_ns;
        manifest.fault_window_until_ns = u64::MAX;
        manifest.network_json = network_json;
        manifest.nodes = nodes_manifest;
        manifest.parent = Some(ClusterParent {
            cluster: self.manifest.id.clone(),
            at_ns,
        });
        manifest.outcome = None;
        manifest.verdict = ClusterVerdict::Unfinished;
        manifest.trace_hash = None;
        manifest.stop_at_step = None;
        manifest.created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();

        let prepared = PreparedCluster {
            id,
            manifest,
            cluster_config: ClusterConfig {
                nodes: nodes_cfg,
                network: net_cfg,
                stop_when,
                max_virtual_ns: self.manifest.max_virtual_ns,
                stop_at_step: None,
                slice_ns: self.manifest.slice_ns,
            },
        };
        ClusterRun::record(home, &prepared, keyframes, echo, None)
    }

    /// Removes a cluster run directory from `home`.
    pub fn remove(home: &Home, id: &RunId) -> Result<()> {
        let dir = home.clusters().join(id.as_str());
        if !dir.is_dir() {
            bail!("cluster run {id} does not exist");
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
    }
}

/// Result of replaying a cluster run.
#[derive(Clone, Debug)]
pub struct ClusterReplayed {
    pub identical: bool,
    pub node_diffs: Vec<String>,
    pub net_diff: Option<String>,
    pub machine_diff: Option<String>,
    pub keyframes_compared: usize,
    pub total_events: usize,
    pub net_events: usize,
}

struct ReplayObserver {
    actual_nodes: Vec<Vec<(u64, Vec<u8>)>>,
    actual_net: Vec<NetEvent>,
}

impl ClusterObserver for ReplayObserver {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        self.actual_nodes[node].push((step, record.to_vec()));
    }

    fn net_event(&mut self, event: &NetEvent) {
        self.actual_net.push(event.clone());
    }
}

struct SeekNodeObserver {
    target_node: usize,
    target_step: u64,
    reached: bool,
}

impl ClusterObserver for SeekNodeObserver {
    fn node_record(&mut self, node: usize, step: u64, _record: &[u8]) {
        if node == self.target_node && step >= self.target_step {
            self.reached = true;
        }
    }

    fn stop(&self) -> bool {
        self.reached
    }

    fn stop_mid_slice(&self) -> bool {
        self.reached
    }
}

pub(crate) fn compute_node_parents(
    cluster: &Cluster,
    prev_parents: &[Option<u64>],
    prev_boots: &[u32],
) -> Vec<Option<u64>> {
    (0..prev_parents.len())
        .map(|i| {
            if cluster.node_boot_count(i) == prev_boots[i] {
                prev_parents[i]
            } else {
                // Node rebooted since last keyframe: capture a full keyframe for it.
                None
            }
        })
        .collect()
}

pub(crate) fn save_cluster_keyframe(dir: &Path, ckf: &ClusterKeyframe) -> Result<()> {
    let kf_dir = dir.join(CLUSTER_KEYFRAMES_DIR);
    fs::create_dir_all(&kf_dir)?;
    let path = kf_dir.join(format!(
        "{:016}.{CLUSTER_KEYFRAME_EXT}",
        ckf.cluster_step
    ));
    crate::image::write_atomic(&path, &encode_keyframe(ckf)?)
}

fn load_cluster_keyframes(dir: &Path) -> Result<Vec<ClusterKeyframe>> {
    let kf_dir = dir.join(CLUSTER_KEYFRAMES_DIR);
    let entries = match fs::read_dir(&kf_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == CLUSTER_KEYFRAME_EXT))
        .collect();
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        out.push(decode_keyframe(&fs::read(&path)?)?);
    }
    Ok(out)
}

pub(crate) fn judge_cluster_node(
    role: NodeRole,
    stop_when: StopWhen,
    cluster_stop: ClusterStop,
    status: Option<i32>,
    crashed_by_fault: bool,
    trace: &Trace,
) -> Verdict {
    let mut reasons = Vec::new();
    let assertions = trace.failed_assertions();
    if !assertions.is_empty() {
        reasons.push(Reason::FailedAssertions { assertions });
    }
    // Check exit status for drivers, or for all nodes when stop_when == All.
    let must_exit_zero = role == NodeRole::Driver || stop_when == StopWhen::All;
    if let Some(raw) = status {
        match ExitStatus::from_wait(raw) {
            ExitStatus::Code(0) => {}
            ExitStatus::Code(code) => reasons.push(Reason::Exited { code }),
            ExitStatus::Signal { signo, core } => reasons.push(Reason::Killed {
                signal: signo,
                core,
            }),
        }
    } else if must_exit_zero && !crashed_by_fault && cluster_stop == ClusterStop::Completed {
        reasons.push(Reason::Crashed);
    }
    if reasons.is_empty() {
        Verdict::Passed
    } else {
        Verdict::Failed { reasons }
    }
}

pub(crate) fn format_reason(r: &Reason) -> String {
    match r {
        Reason::TimedOut => "timed out".into(),
        Reason::Exited { code } => format!("exited with code {code}"),
        Reason::Killed { signal, core } => {
            if *core {
                format!("killed by signal {signal} (core dumped)")
            } else {
                format!("killed by signal {signal}")
            }
        }
        Reason::Crashed => "crashed before exiting".into(),
        Reason::MissingOutputs { paths } => format!("missing outputs: {}", paths.join(", ")),
        Reason::FailedAssertions { assertions } => assertions
            .iter()
            .map(|a| format!("step {}: {}", a.step, a.text))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// Summary of one explored schedule in `rewind cluster check`.
#[derive(Clone, Debug)]
pub struct ClusterCheckEntry {
    pub seed: u64,
    pub schedule: u64,
    pub run: ClusterRun,
    pub differs: bool,
}

/// Narrowed fault window from `rewind cluster check`.
#[derive(Clone, Debug)]
pub struct ClusterNarrowed {
    pub schedule: u64,
    pub seed: u64,
    pub from_ms: u64,
    pub until_ms: u64,
    pub failing: ClusterRun,
    pub passing: Option<ClusterRun>,
}

/// Full result of `rewind cluster check`.
#[derive(Clone, Debug)]
pub struct ClusterCheckResult {
    pub baseline: ClusterRun,
    pub entries: Vec<ClusterCheckEntry>,
    pub narrowed: Option<ClusterNarrowed>,
}

/// Explores schedules/seeds/nemesis faults on a cluster spec in parallel across
/// host cores, and narrows the first differing schedule's fault window via
/// bisection.
#[allow(clippy::too_many_arguments, reason = "cluster check parameters")]
pub fn check_cluster(
    home: &Home,
    guest: &Guest,
    spec_file: &ClusterSpecFile,
    spec_dir: &Path,
    spec_source: &str,
    base_opts: &ClusterRunOptions,
    schedules: u64,
    seeds: u64,
    jobs: usize,
    all: bool,
    narrow: bool,
    mut on_entry: impl FnMut(&ClusterCheckEntry),
) -> Result<ClusterCheckResult> {
    // 1. Run unperturbed baseline (schedule 0, base seed, nemesis off).
    let mut base_run_opts = base_opts.clone();
    base_run_opts.schedule = 0;
    base_run_opts.nemesis_enabled = false;
    base_run_opts.keyframes = Keyframes::Take;
    let base_prepared = prepare_cluster(home, guest, spec_file, spec_dir, spec_source, &base_run_opts)?;
    let baseline = ClusterRun::record(
        home,
        &base_prepared,
        Keyframes::Take,
        false,
        base_opts.wall_timeout,
    )?;
    let base_passed = baseline.manifest.verdict.passed();

    let mut work_items: Vec<(u64, u64)> = Vec::new();
    for s_off in 0..seeds {
        let seed = base_opts.seed + s_off;
        if s_off > 0 {
            work_items.push((seed, 0));
        }
        for sched in 1..=schedules {
            work_items.push((seed, sched));
        }
    }

    let workers = jobs.max(1);
    let mut entries = Vec::new();
    let mut first_diff: Option<ClusterCheckEntry> = None;

    for chunk in work_items.chunks(workers) {
        let batch_results: Vec<Result<ClusterCheckEntry>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&(seed, schedule)| {
                    s.spawn(move || -> Result<ClusterCheckEntry> {
                        let mut opts = base_opts.clone();
                        opts.seed = seed;
                        opts.schedule = schedule;
                        opts.keyframes = Keyframes::Skip;
                        let prep = prepare_cluster(home, guest, spec_file, spec_dir, spec_source, &opts)?;
                        let run = ClusterRun::record(
                            home,
                            &prep,
                            Keyframes::Skip,
                            false,
                            base_opts.wall_timeout,
                        )?;
                        let differs = run.manifest.verdict.passed() != base_passed;
                        Ok(ClusterCheckEntry {
                            seed,
                            schedule,
                            run,
                            differs,
                        })
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("cluster check worker panicked"))
                .collect()
        });

        for res in batch_results {
            let entry = res?;
            on_entry(&entry);
            if entry.differs && first_diff.is_none() {
                first_diff = Some(entry.clone());
            }
            entries.push(entry);
            if !all && first_diff.is_some() {
                break;
            }
        }
        if !all && first_diff.is_some() {
            break;
        }
    }

    // 2. Bisect the virtual-time fault window (in milliseconds) of the first
    //    differing perturbed schedule!
    let narrowed = if narrow
        && let Some(diff) = first_diff
        && diff.schedule != 0
    {
        let end_ms = diff
            .run
            .manifest
            .outcome
            .as_ref()
            .map_or(1_000, |o| (o.cluster_ns / 1_000_000).max(1));
        let seed = diff.seed;
        let schedule = diff.schedule;

        let narrowed = crate::check::narrow(
            (0, end_ms + 1),
            workers,
            diff.run.clone(),
            |windows| {
                let results: Vec<Result<ClusterRun>> = std::thread::scope(|s| {
                    let handles: Vec<_> = windows
                        .iter()
                        .map(|&(from_ms, until_ms)| {
                            s.spawn(move || -> Result<ClusterRun> {
                                let mut opts = base_opts.clone();
                                opts.seed = seed;
                                opts.schedule = schedule;
                                opts.fault_window_from_ns = from_ms.saturating_mul(1_000_000);
                                opts.fault_window_until_ns = until_ms.saturating_mul(1_000_000);
                                opts.keyframes = Keyframes::Skip;
                                let prep = prepare_cluster(
                                    home,
                                    guest,
                                    spec_file,
                                    spec_dir,
                                    spec_source,
                                    &opts,
                                )?;
                                ClusterRun::record(
                                    home,
                                    &prep,
                                    Keyframes::Skip,
                                    false,
                                    base_opts.wall_timeout,
                                )
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("cluster narrow worker panicked"))
                        .collect()
                });
                results.into_iter().collect()
            },
            |run| Ok(run.manifest.verdict.passed() != base_passed),
        )?;

        // Re-record the narrowed failing run with keyframes so it can be inspected!
        let mut final_opts = base_opts.clone();
        final_opts.seed = seed;
        final_opts.schedule = schedule;
        final_opts.fault_window_from_ns = narrowed.from.saturating_mul(1_000_000);
        final_opts.fault_window_until_ns = narrowed.until.saturating_mul(1_000_000);
        final_opts.keyframes = Keyframes::Take;
        let final_prep = prepare_cluster(home, guest, spec_file, spec_dir, spec_source, &final_opts)?;
        let failing = ClusterRun::record(
            home,
            &final_prep,
            Keyframes::Take,
            false,
            base_opts.wall_timeout,
        )?;

        Some(ClusterNarrowed {
            schedule,
            seed,
            from_ms: narrowed.from,
            until_ms: narrowed.until,
            failing,
            passing: narrowed.without_last,
        })
    } else {
        None
    };

    Ok(ClusterCheckResult {
        baseline,
        entries,
        narrowed,
    })
}

/// Formats a `NetEvent` as one readable line using node names from `names`.
pub fn format_net_event(event: &NetEvent, names: &[String]) -> String {
    let name = |idx: usize| {
        names
            .get(idx)
            .map(String::as_str)
            .unwrap_or("?")
    };
    match event {
        NetEvent::Tx {
            ns,
            src,
            dst_mac,
            len,
            summary,
        } => format!(
            "{:>10.3}ms  TX       {} -> {}  ({} B)  {}",
            *ns as f64 / 1_000_000.0,
            name(*src),
            dst_mac,
            len,
            summary
        ),
        NetEvent::Deliver {
            ns,
            src,
            dst,
            len,
            latency_ns,
        } => format!(
            "{:>10.3}ms  DELIVER  {} -> {}  ({} B, lat {:.3}ms)",
            *ns as f64 / 1_000_000.0,
            name(*src),
            name(*dst),
            len,
            *latency_ns as f64 / 1_000_000.0
        ),
        NetEvent::Drop {
            ns,
            src,
            dst,
            len,
            reason,
        } => format!(
            "{:>10.3}ms  DROP     {} -> {}  ({} B, {:?})",
            *ns as f64 / 1_000_000.0,
            name(*src),
            name(*dst),
            len,
            reason
        ),
        NetEvent::Duplicate {
            ns,
            src,
            dst,
            len,
            extra_delay_ns,
        } => format!(
            "{:>10.3}ms  DUP      {} -> {}  ({} B, +{:.3}ms)",
            *ns as f64 / 1_000_000.0,
            name(*src),
            name(*dst),
            len,
            *extra_delay_ns as f64 / 1_000_000.0
        ),
        NetEvent::Partition {
            ns,
            blocked_pairs,
            asymmetric,
        } => format!(
            "{:>10.3}ms  PART     {} ({} directed links blocked)",
            *ns as f64 / 1_000_000.0,
            if *asymmetric { "asymmetric" } else { "symmetric" },
            blocked_pairs
        ),
        NetEvent::Heal { ns } => format!("{:>10.3}ms  HEAL     all partitions cleared", *ns as f64 / 1_000_000.0),
        NetEvent::NodeCrash { ns, node } => format!(
            "{:>10.3}ms  CRASH    {}",
            *ns as f64 / 1_000_000.0,
            name(*node)
        ),
        NetEvent::NodeRestart {
            ns,
            node,
            boot_count,
        } => format!(
            "{:>10.3}ms  RESTART  {} (boot #{})",
            *ns as f64 / 1_000_000.0,
            name(*node),
            boot_count
        ),
        NetEvent::LinkUpdated {
            ns,
            from,
            to,
            latency_ns,
            jitter_ns,
            drop_ppm,
            duplicate_ppm,
        } => format!(
            "{:>10.3}ms  LINK     {} -> {}  lat={:.3}ms jit={:.3}ms drop={}ppm dup={}ppm",
            *ns as f64 / 1_000_000.0,
            name(*from),
            name(*to),
            *latency_ns as f64 / 1_000_000.0,
            *jitter_ns as f64 / 1_000_000.0,
            drop_ppm,
            duplicate_ppm,
        ),
    }
}

/// Summary map of node names -> node indices for filtering.
pub fn node_index_map(manifest: &ClusterManifest) -> BTreeMap<String, usize> {
    manifest
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.name.clone(), i))
        .collect()
}
