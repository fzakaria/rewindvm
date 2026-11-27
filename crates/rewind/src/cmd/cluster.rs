//! The `rewind cluster` subcommands: interactive JSON-RPC control server
//! (`serve`), parallel script/nemesis exploration & bisection (`check`),
//! standalone replay, forking, listing, showing, and network inspection.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rewind_core::cluster::{
    ClusterRun, ClusterRunOptions, format_net_event, node_index_map, parse_spec_file,
    prepare_cluster,
};
use rewind_core::cluster_rpc::{ServeDefaults, serve_stream, serve_unix_socket};
use rewind_core::{Guest, Home, Keyframes};
use rewind_trace::cluster::ClusterVerdict;
use rewind_vmm::net::NetEvent;
use rewind_vmm::{ClockSource, CpuModel};

use crate::cmd::view::MachineArg;
use crate::color;
use crate::exit;

/// Top-level `rewind cluster` command.
#[derive(clap::Args)]
pub(crate) struct ClusterArgs {
    #[command(subcommand)]
    pub(crate) action: ClusterAction,
}

#[derive(clap::Subcommand)]
pub(crate) enum ClusterAction {
    /// Start the interactive JSON-RPC 2.0 cluster controller (stdio or Unix socket).
    Serve(ClusterServeArgs),
    /// Explore schedules, seeds, and nemesis faults by running a test script in parallel.
    Check(ClusterCheckArgs),
    /// Boot and run a cluster from a JSON spec file.
    Run(ClusterRunArgs),
    /// Replay a recorded cluster run standalone and verify every node and network event matches.
    Replay(ClusterReplayArgs),
    /// Fork a recorded cluster run from a virtual timestamp under another schedule or seed.
    Fork(ClusterForkArgs),
    /// List recorded cluster runs.
    Ls(ClusterLsArgs),
    /// Show details, node outcomes, and network summary of a cluster run.
    Show(ClusterShowArgs),
    /// Show the deterministic network packet & fault timeline of a cluster run.
    Net(ClusterNetArgs),
    /// Reconstruct a full Ethernet `.pcap` capture (for Wireshark/tshark) by replaying a cluster run.
    Pcap(ClusterPcapArgs),
    /// Remove recorded cluster runs.
    Remove(ClusterRemoveArgs),
}

#[derive(clap::Args)]
pub(crate) struct ClusterServeArgs {
    /// Listen on a Unix domain socket instead of stdin/stdout.
    #[arg(long, value_name = "PATH")]
    pub(crate) socket: Option<PathBuf>,
    /// CPU model shown to every node (`v3`, `v3-compat`, or `host`).
    #[arg(long, value_enum)]
    pub(crate) cpu: Option<CpuArg>,
    /// Virtual clock source (`exits` or `branches`).
    #[arg(long, value_enum)]
    pub(crate) clock: Option<ClockArg>,
}

#[derive(clap::Args)]
pub(crate) struct ClusterCommonArgs {
    /// Cluster RNG seed (each node derives its own deterministic seed from this).
    #[arg(long, default_value_t = 0)]
    pub(crate) seed: u64,
    /// Schedule & fault perturbation seed (0 = unperturbed).
    #[arg(long, default_value_t = 0)]
    pub(crate) schedule: u64,
    /// Boot wall clock in seconds since Unix epoch.
    #[arg(long)]
    pub(crate) epoch: Option<u64>,
    /// CPU model shown to every node (`v3`, `v3-compat`, or `host`).
    #[arg(long, value_enum)]
    pub(crate) cpu: Option<CpuArg>,
    /// Virtual clock source (`exits` or `branches`).
    #[arg(long, value_enum)]
    pub(crate) clock: Option<ClockArg>,
    /// Override per-node RAM in MiB.
    #[arg(long)]
    pub(crate) mem: Option<u64>,
    /// Override virtual core count reported to each node.
    #[arg(long)]
    pub(crate) cores: Option<u32>,
    /// Override maximum cluster virtual time in milliseconds.
    #[arg(long)]
    pub(crate) max_virtual_ms: Option<u64>,
    /// Enable schedule-driven nemesis (random partitions, heals, node crashes & restarts).
    #[arg(long)]
    pub(crate) nemesis: bool,
    /// Host wall-clock timeout in seconds.
    #[arg(long)]
    pub(crate) timeout: Option<u64>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum CpuArg {
    V3,
    V3Compat,
    Host,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum ClockArg {
    Exits,
    Branches,
}

#[derive(clap::Args)]
pub(crate) struct ClusterRunArgs {
    /// Path to a JSON cluster spec file.
    pub(crate) spec: PathBuf,
    /// Skip taking cluster keyframes (faster, disables mid-run seeking).
    #[arg(long)]
    pub(crate) no_keyframes: bool,
    /// Suppress live node stdout/stderr echo.
    #[arg(long, short)]
    pub(crate) quiet: bool,
    /// Print the cluster manifest as JSON on stdout.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) common: ClusterCommonArgs,
}

#[derive(clap::Args)]
pub(crate) struct ClusterCheckArgs {
    /// Starting RNG seed.
    #[arg(long, default_value_t = 0)]
    pub(crate) seed: u64,
    /// Number of perturbed schedules to explore besides schedule 0.
    #[arg(long, default_value_t = 16)]
    pub(crate) schedules: u64,
    /// Number of cluster RNG seeds to try.
    #[arg(long, default_value_t = 1)]
    pub(crate) seeds: u64,
    /// Enable schedule-driven nemesis on perturbed runs (schedule > 0).
    #[arg(long)]
    pub(crate) nemesis: bool,
    /// Parallel script executions across host cores.
    #[arg(long, short)]
    pub(crate) jobs: Option<usize>,
    /// Run every schedule instead of stopping at the first differing verdict.
    #[arg(long)]
    pub(crate) all: bool,
    /// Skip bisecting the deciding virtual-time fault window.
    #[arg(long)]
    pub(crate) no_narrow: bool,
    /// Virtual clock source forwarded to `REWIND_CLOCK` (`exits` or `branches`).
    #[arg(long, value_enum)]
    pub(crate) clock: Option<ClockArg>,
    /// Print results as JSON.
    #[arg(long)]
    pub(crate) json: bool,
    /// Command to run for each trial (e.g. `-- python3 examples/cluster/fragile_rpc.py`).
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) command: Vec<String>,
}

#[derive(clap::Args)]
pub(crate) struct ClusterReplayArgs {
    /// Cluster run id, prefix, name, or `@N`.
    pub(crate) cluster: String,
    /// Where to compare machine & switch state (`every`, `last`, or `none`).
    #[arg(long, value_enum, default_value_t = MachineArg::Every)]
    pub(crate) machine: MachineArg,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct ClusterForkArgs {
    /// Parent cluster run id, prefix, name, or `@N`.
    pub(crate) cluster: String,
    /// Cluster virtual timestamp (ms) from which the new schedule takes effect.
    #[arg(long = "at-ms", value_name = "MS")]
    pub(crate) at_ms: u64,
    /// New schedule seed from `at-ms` onward.
    #[arg(long, default_value_t = 1)]
    pub(crate) schedule: u64,
    /// Optional new cluster RNG seed.
    #[arg(long)]
    pub(crate) seed: Option<u64>,
    #[arg(long)]
    pub(crate) no_keyframes: bool,
    #[arg(long, short)]
    pub(crate) quiet: bool,
}

#[derive(clap::Args)]
pub(crate) struct ClusterLsArgs {
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct ClusterShowArgs {
    /// Cluster run id, prefix, name, or `@N`.
    pub(crate) cluster: String,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct ClusterNetArgs {
    /// Cluster run id, prefix, name, or `@N`.
    pub(crate) cluster: String,
    /// Filter events involving this node name.
    #[arg(long)]
    pub(crate) node: Option<String>,
    /// Show only fault events (partitions, heals, link updates, crashes, restarts, drops).
    #[arg(long)]
    pub(crate) faults: bool,
    /// Show only dropped packets.
    #[arg(long)]
    pub(crate) drops: bool,
    /// Start of virtual-time window in ms.
    #[arg(long = "from-ms")]
    pub(crate) from_ms: Option<u64>,
    /// End of virtual-time window in ms.
    #[arg(long = "to-ms")]
    pub(crate) to_ms: Option<u64>,
    /// Output raw JSONL events.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct ClusterPcapArgs {
    /// Cluster run id, prefix, name, or `@N`.
    pub(crate) cluster: String,
    /// Output `.pcap` file path (or `-` to write raw pcap bytes to stdout).
    #[arg(short, long, value_name = "PATH")]
    pub(crate) output: PathBuf,
    /// Only include frames sent by, addressed to, or broadcast to this node.
    #[arg(long)]
    pub(crate) node: Option<String>,
    /// Start of virtual-time window in ms (fast-forwards via nearest keyframe).
    #[arg(long = "from-ms")]
    pub(crate) from_ms: Option<u64>,
    /// End of virtual-time window in ms.
    #[arg(long = "to-ms")]
    pub(crate) to_ms: Option<u64>,
}

#[derive(clap::Args)]
pub(crate) struct ClusterRemoveArgs {
    /// Cluster run ids, prefixes, names, or `@N`.
    #[arg(required = true)]
    pub(crate) clusters: Vec<String>,
}

impl ClusterArgs {
    pub(crate) fn holds_home(&self) -> bool {
        matches!(
            self.action,
            ClusterAction::Serve(_)
                | ClusterAction::Run(_)
                | ClusterAction::Check(_)
                | ClusterAction::Replay(_)
                | ClusterAction::Fork(_)
                | ClusterAction::Pcap(_)
        )
    }
}

pub(crate) fn dispatch(home: &Home, args: ClusterArgs) -> Result<ExitCode> {
    match args.action {
        ClusterAction::Serve(a) => cmd_serve(home, a),
        ClusterAction::Check(a) => cmd_check(home, a),
        ClusterAction::Run(a) => cmd_run(home, a),
        ClusterAction::Replay(a) => cmd_replay(home, a),
        ClusterAction::Fork(a) => cmd_fork(home, a),
        ClusterAction::Ls(a) => cmd_ls(home, a),
        ClusterAction::Show(a) => cmd_show(home, a),
        ClusterAction::Net(a) => cmd_net(home, a),
        ClusterAction::Pcap(a) => cmd_pcap(home, a),
        ClusterAction::Remove(a) => cmd_remove(home, a),
    }
}

fn resolve_cpu(arg: Option<CpuArg>) -> Result<CpuModel> {
    match arg {
        Some(CpuArg::V3) => Ok(CpuModel::V3),
        Some(CpuArg::V3Compat) => Ok(CpuModel::V3Compat),
        Some(CpuArg::Host) => Ok(CpuModel::Host),
        None => {
            let kvm = rewind_vmm::kvm::open()?;
            rewind_vmm::cpu::default_model(&kvm)
        }
    }
}

fn resolve_clock(home: &Home, guest: &Guest, arg: Option<ClockArg>) -> Result<ClockSource> {
    use rewind_core::pmu::{self, Vendor};
    let effective = arg.or_else(|| {
        std::env::var("REWIND_CLOCK").ok().and_then(|v| match v.as_str() {
            "exits" => Some(ClockArg::Exits),
            "branches" => Some(ClockArg::Branches),
            _ => None,
        })
    });
    let vendor = Vendor::detect()?;
    let branches = || {
        vendor
            .event()
            .map(ClockSource::Branches)
            .context("this CPU has no branch counter rewind knows")
    };
    match effective {
        Some(ClockArg::Exits) => Ok(ClockSource::Exits),
        Some(ClockArg::Branches) => {
            let clock = branches()?;
            if !pmu::counter_usable_this_boot(home, guest, &vendor)? {
                bail!(pmu::branch_clock_refusal(&vendor));
            }
            Ok(clock)
        }
        None => {
            if vendor.event().is_some() && pmu::counter_usable_this_boot(home, guest, &vendor)? {
                return branches();
            }
            Ok(ClockSource::Exits)
        }
    }
}

fn cmd_serve(home: &Home, args: ClusterServeArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let guest = Guest::from_env()?;
    let defaults = ServeDefaults {
        home: home.clone(),
        guest: guest.clone(),
        cpu: resolve_cpu(args.cpu)?,
        clock: resolve_clock(home, &guest, args.clock)?,
        workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    if let Some(sock) = args.socket {
        serve_unix_socket(&sock, &defaults)?;
    } else {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        serve_stream(BufReader::new(stdin.lock()), stdout.lock(), &defaults)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Debug)]
struct ScriptTrial {
    seed: u64,
    schedule: u64,
    passed: bool,
    run: Option<ClusterRun>,
    cluster_ns: u64,
    differs: bool,
}

#[derive(serde::Deserialize)]
struct TrialResultFile {
    id: String,
    passed: bool,
    cluster_ns: u64,
}

static TRIAL_SEQ: AtomicU64 = AtomicU64::new(1);

#[allow(clippy::too_many_arguments, reason = "script trial invocation parameters")]
fn run_script_trial(
    home: &Home,
    rewind_bin: &PathBuf,
    command: &[String],
    clock: Option<ClockArg>,
    seed: u64,
    schedule: u64,
    nemesis: bool,
    from_ms: Option<u64>,
    until_ms: Option<u64>,
    keyframes: bool,
) -> Result<ScriptTrial> {
    let seq = TRIAL_SEQ.fetch_add(1, Ordering::Relaxed);
    let result_path = std::env::temp_dir().join(format!(
        "rewind-cluster-trial-{}-{seq}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&result_path);

    let (prog, rest) = command
        .split_first()
        .context("cluster check requires a command after `--`")?;
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .env("REWIND_BIN", rewind_bin)
        .env("REWIND_HOME", home.root())
        .env("REWIND_SEED", seed.to_string())
        .env("REWIND_SCHEDULE", schedule.to_string())
        .env("REWIND_NEMESIS", if nemesis { "1" } else { "0" })
        .env("REWIND_KEYFRAMES", if keyframes { "1" } else { "0" })
        .env("REWIND_RESULT_FILE", &result_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(c) = clock {
        cmd.env(
            "REWIND_CLOCK",
            match c {
                ClockArg::Exits => "exits",
                ClockArg::Branches => "branches",
            },
        );
    }
    if let Some(f) = from_ms {
        cmd.env("REWIND_FAULT_FROM_MS", f.to_string());
    } else {
        cmd.env_remove("REWIND_FAULT_FROM_MS");
    }
    if let Some(u) = until_ms {
        cmd.env("REWIND_FAULT_UNTIL_MS", u.to_string());
    } else {
        cmd.env_remove("REWIND_FAULT_UNTIL_MS");
    }

    let output = cmd
        .output()
        .with_context(|| format!("spawning cluster test script `{prog}`"))?;

    let parsed: Option<TrialResultFile> = std::fs::read(&result_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let _ = std::fs::remove_file(&result_path);

    let run = match &parsed {
        Some(r) => ClusterRun::find(home, &r.id).ok(),
        None => None,
    };
    let passed = match &parsed {
        Some(r) => output.status.success() && r.passed,
        None => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if run.is_none() && stderr.contains("No such file") {
                    bail!("cluster test script failed to run: {}", stderr.trim());
                }
            }
            output.status.success()
        }
    };
    let cluster_ns = parsed.as_ref().map_or(0, |r| r.cluster_ns);
    Ok(ScriptTrial {
        seed,
        schedule,
        passed,
        run,
        cluster_ns,
        differs: false,
    })
}

fn cmd_check(home: &Home, args: ClusterCheckArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let rewind_bin = std::env::current_exe().context("locating current rewind executable")?;
    let jobs = args.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(4)
    });

    // 1. Run unperturbed baseline (schedule 0, nemesis off, keyframes on).
    let baseline = run_script_trial(
        home,
        &rewind_bin,
        &args.command,
        args.clock,
        args.seed,
        0,
        false,
        None,
        None,
        true,
    )?;
    let base_passed = baseline.passed;

    let mut work: Vec<(u64, u64)> = Vec::new();
    for s_off in 0..args.seeds {
        let seed = args.seed + s_off;
        if s_off > 0 {
            work.push((seed, 0));
        }
        for sched in 1..=args.schedules {
            work.push((seed, sched));
        }
    }

    let workers = jobs.max(1);
    let mut entries: Vec<ScriptTrial> = Vec::new();
    let mut first_diff: Option<ScriptTrial> = None;

    let bin_ref = &rewind_bin;
    let cmd_ref: &[String] = &args.command;
    let clock = args.clock;
    let nemesis = args.nemesis;

    for chunk in work.chunks(workers) {
        let results: Vec<Result<ScriptTrial>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&(seed, sched)| {
                    s.spawn(move || {
                        let mut t = run_script_trial(
                            home,
                            bin_ref,
                            cmd_ref,
                            clock,
                            seed,
                            sched,
                            nemesis && sched != 0,
                            None,
                            None,
                            false,
                        )?;
                        t.differs = t.passed != base_passed;
                        Ok(t)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("cluster check worker panicked"))
                .collect()
        });

        for res in results {
            let entry = res?;
            if !args.json && (entry.differs || args.all) {
                print_trial_line(&entry);
            }
            if entry.differs && first_diff.is_none() {
                first_diff = Some(entry.clone());
            }
            entries.push(entry);
            if !args.all && first_diff.is_some() {
                break;
            }
        }
        if !args.all && first_diff.is_some() {
            break;
        }
    }

    // 2. Bisect the virtual-time fault window of the first differing schedule.
    let mut narrowed_info: Option<(u64, u64, u64, u64, Option<ClusterRun>)> = None;
    if !args.no_narrow
        && let Some(diff) = &first_diff
        && diff.schedule != 0
    {
        let end_ms = (diff.cluster_ns / 1_000_000).max(1);
        let seed = diff.seed;
        let sched = diff.schedule;
        let narrowed = rewind_core::check::narrow(
            (0, end_ms + 1),
            workers,
            diff.clone(),
            |windows| {
                let res: Vec<Result<ScriptTrial>> = std::thread::scope(|s| {
                    let handles: Vec<_> = windows
                        .iter()
                        .map(|&(from_ms, until_ms)| {
                            s.spawn(move || {
                                run_script_trial(
                                    home,
                                    bin_ref,
                                    cmd_ref,
                                    clock,
                                    seed,
                                    sched,
                                    nemesis,
                                    Some(from_ms),
                                    Some(until_ms),
                                    false,
                                )
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("cluster narrow worker panicked"))
                        .collect()
                });
                res.into_iter().collect()
            },
            |t| Ok(t.passed != base_passed),
        )?;

        // Re-record the narrowed failing run with full keyframes so it can be inspected!
        let final_trial = run_script_trial(
            home,
            bin_ref,
            cmd_ref,
            clock,
            seed,
            sched,
            nemesis,
            Some(narrowed.from),
            Some(narrowed.until),
            true,
        )?;
        narrowed_info = Some((seed, sched, narrowed.from, narrowed.until, final_trial.run));
    }

    if args.json {
        let out = serde_json::json!({
            "baseline": {
                "passed": baseline.passed,
                "id": baseline.run.as_ref().map(|r| r.manifest.id.as_str()),
            },
            "checked": entries.len(),
            "differing": entries.iter().filter(|e| e.differs).count(),
            "narrowed": narrowed_info.as_ref().map(|(seed, sched, from_ms, until_ms, run)| {
                serde_json::json!({
                    "seed": seed,
                    "schedule": sched,
                    "from_ms": from_ms,
                    "until_ms": until_ms,
                    "failing_id": run.as_ref().map(|r| r.manifest.id.as_str()),
                })
            }),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let diff_count = entries.iter().filter(|e| e.differs).count();
        let base_id = baseline
            .run
            .as_ref()
            .map_or("(no cluster)", |r| r.manifest.id.as_str());
        println!(
            "\nbaseline (schedule 0): {}  cluster {base_id}",
            if baseline.passed { "passed" } else { "FAILED" }
        );
        println!("checked {} perturbed runs: {diff_count} ended differently", entries.len());
        if let Some((seed, sched, from_ms, until_ms, run)) = &narrowed_info {
            println!("narrowed schedule {sched} (seed {seed}) to fault window {from_ms}ms..{until_ms}ms:");
            if let Some(r) = run {
                println!("  failing: cluster {}", r.manifest.id);
                println!(
                    "  inspect: rewind cluster net {} --from-ms {} --to-ms {}",
                    r.manifest.id,
                    from_ms.saturating_sub(50),
                    until_ms + 50
                );
            }
        }
    }

    if first_diff.is_some() || !baseline.passed {
        return Ok(ExitCode::from(exit::DIFFERS));
    }
    Ok(ExitCode::SUCCESS)
}

fn print_trial_line(t: &ScriptTrial) {
    let status = if t.passed { "passed" } else { "FAILED" };
    let ms = t.cluster_ns as f64 / 1_000_000.0;
    let id = t.run.as_ref().map_or("-", |r| r.manifest.id.as_str());
    let (tx, drops) = t
        .run
        .as_ref()
        .and_then(|r| r.manifest.outcome.as_ref())
        .map_or((0, 0), |o| (o.tx_frames, o.dropped_frames));
    println!(
        "seed {:>2} schedule {:>3}: {:<10} {:>8.1}ms  {:>5} tx ({} drop)  cluster {id}",
        t.seed, t.schedule, status, ms, tx, drops
    );
}

fn build_options(
    home: &Home,
    guest: &Guest,
    common: &ClusterCommonArgs,
    keyframes: Keyframes,
    echo: bool,
) -> Result<ClusterRunOptions> {
    Ok(ClusterRunOptions {
        seed: common.seed,
        schedule: common.schedule,
        fault_window_from_ns: 0,
        fault_window_until_ns: u64::MAX,
        epoch: common.epoch,
        cpu: resolve_cpu(common.cpu)?,
        clock: resolve_clock(home, guest, common.clock)?,
        mem_mib_override: common.mem,
        cores_override: common.cores,
        max_virtual_ms_override: common.max_virtual_ms,
        nemesis_enabled: common.nemesis,
        keyframes,
        echo,
        wall_timeout: common.timeout.map(Duration::from_secs),
        parent: None,
    })
}

fn cmd_run(home: &Home, args: ClusterRunArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let guest = Guest::from_env()?;
    let spec_file = parse_spec_file(&args.spec)?;
    let spec_dir = args
        .spec
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let spec_source = std::fs::read_to_string(&args.spec)?;
    let keyframes = if args.no_keyframes {
        Keyframes::Skip
    } else {
        Keyframes::Take
    };
    let echo = !args.quiet && !args.json;
    let opts = build_options(home, &guest, &args.common, keyframes, echo)?;
    let prepared = prepare_cluster(home, &guest, &spec_file, &spec_dir, &spec_source, &opts)?;
    let run = ClusterRun::record(home, &prepared, keyframes, echo, opts.wall_timeout)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&run.manifest)?);
    } else {
        print_cluster_summary(&run);
    }
    if run.manifest.verdict.failed() {
        return Ok(ExitCode::from(exit::DIFFERS));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_replay(home: &Home, args: ClusterReplayArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let run = ClusterRun::find(home, &args.cluster)?;
    let replayed = run.replay(home, args.machine.into())?;

    if args.json {
        let out = serde_json::json!({
            "cluster": run.manifest.id.as_str(),
            "identical": replayed.identical,
            "total_events": replayed.total_events,
            "net_events": replayed.net_events,
            "keyframes_compared": replayed.keyframes_compared,
            "node_diffs": replayed.node_diffs,
            "net_diff": replayed.net_diff,
            "machine_diff": replayed.machine_diff,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if replayed.identical {
        println!(
            "identical to the recording: {} nodes, {} trace records, {} network events, {} cluster keyframe(s) matched",
            run.manifest.nodes.len(),
            replayed.total_events,
            replayed.net_events,
            replayed.keyframes_compared,
        );
    } else {
        let paint = color::stderr();
        eprintln!(
            "{} replay of cluster {} diverged:",
            paint.prefix(),
            run.manifest.id
        );
        for d in &replayed.node_diffs {
            eprintln!("  - {d}");
        }
        if let Some(d) = &replayed.net_diff {
            eprintln!("  - {d}");
        }
        if let Some(d) = &replayed.machine_diff {
            eprintln!("  - {d}");
        }
        return Ok(ExitCode::from(exit::DIFFERS));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_fork(home: &Home, args: ClusterForkArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let parent = ClusterRun::find(home, &args.cluster)?;
    let keyframes = if args.no_keyframes {
        Keyframes::Skip
    } else {
        Keyframes::Take
    };
    let forked = parent.fork(
        home,
        args.at_ms,
        args.schedule,
        args.seed,
        keyframes,
        !args.quiet,
    )?;
    print_cluster_summary(&forked);
    if forked.manifest.verdict.failed() {
        return Ok(ExitCode::from(exit::DIFFERS));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_ls(home: &Home, args: ClusterLsArgs) -> Result<ExitCode> {
    let runs = ClusterRun::list(home)?;
    for run in &runs {
        if args.json {
            println!("{}", serde_json::to_string(&run.manifest)?);
        } else {
            let status = verdict_tag(&run.manifest.verdict);
            let nodes = run.manifest.nodes.len();
            let ms = run
                .manifest
                .outcome
                .as_ref()
                .map_or(0.0, |o| o.cluster_ns as f64 / 1_000_000.0);
            println!(
                "{}  {:<20}  {:<10}  {} nodes  {:>8.1}ms  sched={}",
                run.manifest.id, run.manifest.name, status, nodes, ms, run.manifest.schedule
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_show(home: &Home, args: ClusterShowArgs) -> Result<ExitCode> {
    let run = ClusterRun::find(home, &args.cluster)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&run.manifest)?);
        return Ok(ExitCode::SUCCESS);
    }
    print_cluster_summary(&run);
    Ok(ExitCode::SUCCESS)
}

fn cmd_net(home: &Home, args: ClusterNetArgs) -> Result<ExitCode> {
    let run = ClusterRun::find(home, &args.cluster)?;
    let events = run.net_events()?;
    let names: Vec<String> = run.manifest.nodes.iter().map(|n| n.name.clone()).collect();
    let idx_map = node_index_map(&run.manifest);
    let filter_node = match &args.node {
        Some(name) => {
            let &i = idx_map.get(name).with_context(|| {
                format!(
                    "cluster {} has no node {name:?} (nodes: {})",
                    run.manifest.id,
                    names.join(", ")
                )
            })?;
            Some(i)
        }
        None => None,
    };
    let from_ns = args.from_ms.map_or(0, |ms| ms.saturating_mul(1_000_000));
    let to_ns = args
        .to_ms
        .map_or(u64::MAX, |ms| ms.saturating_mul(1_000_000));

    for ev in &events {
        let ns = net_event_ns(ev);
        if ns < from_ns || ns > to_ns {
            continue;
        }
        if args.drops && !matches!(ev, NetEvent::Drop { .. }) {
            continue;
        }
        if args.faults && matches!(ev, NetEvent::Tx { .. } | NetEvent::Deliver { .. }) {
            continue;
        }
        if let Some(target) = filter_node
            && !net_event_involves(ev, target)
        {
            continue;
        }
        if args.json {
            println!("{}", serde_json::to_string(ev)?);
        } else {
            println!("{}", format_net_event(ev, &names));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_pcap(home: &Home, args: ClusterPcapArgs) -> Result<ExitCode> {
    rewind_vmm::kvm::open()?;
    let run = ClusterRun::find(home, &args.cluster)?;
    let to_stdout = args.output.as_os_str() == "-";
    let stats = if to_stdout {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        run.export_pcap(
            home,
            &mut out,
            args.from_ms,
            args.to_ms,
            args.node.as_deref(),
        )?
    } else {
        let file = std::fs::File::create(&args.output)
            .with_context(|| format!("creating {}", args.output.display()))?;
        let mut out = std::io::BufWriter::new(file);
        run.export_pcap(
            home,
            &mut out,
            args.from_ms,
            args.to_ms,
            args.node.as_deref(),
        )?
    };
    if !to_stdout {
        eprintln!(
            "wrote {} ({} packets, {} bytes, {:.3}ms..{:.3}ms)",
            args.output.display(),
            stats.packets,
            stats.bytes,
            stats.from_ns as f64 / 1_000_000.0,
            stats.to_ns as f64 / 1_000_000.0,
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_remove(home: &Home, args: ClusterRemoveArgs) -> Result<ExitCode> {
    for q in &args.clusters {
        let run = ClusterRun::find(home, q)?;
        let id = run.manifest.id.clone();
        ClusterRun::remove(home, &id)?;
        println!("removed cluster {id}");
    }
    Ok(ExitCode::SUCCESS)
}

fn net_event_ns(ev: &NetEvent) -> u64 {
    match ev {
        NetEvent::Tx { ns, .. }
        | NetEvent::Deliver { ns, .. }
        | NetEvent::Drop { ns, .. }
        | NetEvent::Duplicate { ns, .. }
        | NetEvent::Partition { ns, .. }
        | NetEvent::Heal { ns }
        | NetEvent::NodeCrash { ns, .. }
        | NetEvent::NodeRestart { ns, .. }
        | NetEvent::LinkUpdated { ns, .. } => *ns,
    }
}

fn net_event_involves(ev: &NetEvent, node: usize) -> bool {
    match ev {
        NetEvent::Tx { src, .. } => *src == node,
        NetEvent::Deliver { src, dst, .. }
        | NetEvent::Drop { src, dst, .. }
        | NetEvent::Duplicate { src, dst, .. } => *src == node || *dst == node,
        NetEvent::LinkUpdated { from, to, .. } => *from == node || *to == node,
        NetEvent::NodeCrash { node: n, .. } | NetEvent::NodeRestart { node: n, .. } => *n == node,
        NetEvent::Partition { .. } | NetEvent::Heal { .. } => true,
    }
}

fn verdict_tag(v: &ClusterVerdict) -> String {
    match v {
        ClusterVerdict::Unfinished => "unfinished".into(),
        ClusterVerdict::Passed => "passed".into(),
        ClusterVerdict::Failed { .. } => "FAILED".into(),
    }
}

fn print_cluster_summary(run: &ClusterRun) {
    let m = &run.manifest;
    println!("cluster {} ({})", m.id, m.name);
    println!(
        "  verdict: {}  seed={}  schedule={}  stop_when={}",
        verdict_tag(&m.verdict),
        m.seed,
        m.schedule,
        m.stop_when
    );
    if let Some(o) = &m.outcome {
        println!(
            "  virtual: {:.3}ms  wall: {}ms  stop: {}  frames: {} tx / {} delivered / {} dropped / {} dup",
            o.cluster_ns as f64 / 1_000_000.0,
            o.wall_ms,
            o.stop,
            o.tx_frames,
            o.delivered_frames,
            o.dropped_frames,
            o.duplicated_frames
        );
        if o.partitions > 0 || o.crashes > 0 || o.restarts > 0 {
            println!(
                "  faults:  {} partitions, {} crashes, {} restarts",
                o.partitions, o.crashes, o.restarts
            );
        }
        println!("  nodes:");
        for (nm, nr) in m.nodes.iter().zip(&o.nodes) {
            let state = if let Some(raw) = nr.status {
                match rewind_trace::ending::ExitStatus::from_wait(raw) {
                    rewind_trace::ending::ExitStatus::Code(c) => format!("exited:{c}"),
                    rewind_trace::ending::ExitStatus::Signal { signo, .. } => {
                        format!("signal:{signo}")
                    }
                }
            } else {
                "running".to_string()
            };
            println!(
                "    {:<12} {:<15} {:<8} {:>7} steps {:>9.2}ms  boots={}  {state}",
                nm.name,
                nm.ip,
                nm.role,
                nr.steps,
                nr.local_ns as f64 / 1_000_000.0,
                nr.boot_count,
            );
        }
    }
    if let ClusterVerdict::Failed { reasons } = &m.verdict {
        println!("  failures:");
        for r in reasons {
            println!("    - {r}");
        }
    }
    if let Some(hash) = &m.trace_hash {
        println!("  trace:   {}", &hash[..12.min(hash.len())]);
    }
}

/// Resolves either a single-node `Run` (when `node` is `None`) or one node's
/// `Run` view inside a multi-node `ClusterRun` (when `node` is `Some(name)`).
pub(crate) fn resolve_run_or_node(
    home: &Home,
    query: &str,
    node: Option<&str>,
) -> Result<rewind_core::Run> {
    if let Some(name) = node {
        let cluster = ClusterRun::find(home, query)?;
        cluster.node_run(name)
    } else {
        rewind_core::Run::find(home, query)
    }
}
