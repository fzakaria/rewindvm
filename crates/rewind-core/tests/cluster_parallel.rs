//! Integration tests for conservative parallel discrete-event execution in
//! Rewind VM's multi-node cluster coordinator.
//!
//! Tests require `/dev/kvm` and `REWIND_KERNEL`/`REWIND_INITRD` (skipped
//! cleanly when absent so `cargo test --workspace` still succeeds on hosts
//! without KVM or guest artifacts).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rewind_core::cluster::{
    ClusterRun, ClusterRunOptions, prepare_cluster,
};
use rewind_core::cluster_session::{ClusterSession, LogStream};
use rewind_core::run::{CompareMachine, Keyframes};
use rewind_core::{Guest, Home, pmu};
use rewind_trace::cluster::{
    CLUSTER_NODES_DIR, ClusterSpecFile, FaultSpecFile, NetworkSpecFile, NodeSpecFile,
};
use rewind_vmm::cluster::{Cluster, ClusterObserver, ClusterStop};
use rewind_vmm::net::NetEvent;
use rewind_vmm::{ClockSource, CpuModel, DirtyLog};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempHome {
    dir: PathBuf,
    home: Home,
}

impl TempHome {
    fn new(tag: &str) -> Self {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rewind-cluster-parallel-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let home = Home::at(dir.clone()).expect("creating temp home");
        TempHome { dir, home }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn require_kvm_and_guest() -> Option<(Guest, CpuModel)> {
    let Ok(kvm) = rewind_vmm::kvm::open() else {
        eprintln!("skipped: /dev/kvm unavailable");
        return None;
    };
    let Ok(guest) = Guest::from_env() else {
        eprintln!("skipped: REWIND_KERNEL / REWIND_INITRD not set");
        return None;
    };
    let cpu = rewind_vmm::cpu::default_model(&kvm).expect("default cpu model");
    Some((guest, cpu))
}

fn node_spec(name: &str, ip: &str, role: &str, cmd: &str, skew_ms: i64, drift_ppm: i32) -> NodeSpecFile {
    NodeSpecFile {
        name: name.into(),
        ip: Some(ip.into()),
        root: None,
        packages: Vec::new(),
        cwd: None,
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        env: Vec::new(),
        uid: None,
        gid: None,
        mem_mib: Some(128),
        cores: Some(1),
        clock_skew_ms: skew_ms,
        clock_drift_ppm: drift_ppm,
        role: Some(role.into()),
        files: Vec::new(),
    }
}

fn three_node_tcp_spec(faults: Vec<FaultSpecFile>, latency_us: u64) -> ClusterSpecFile {
    ClusterSpecFile {
        name: "parallel-tcp-test".into(),
        mem_mib: Some(128),
        cores: Some(1),
        stop_when: Some("driver".into()),
        max_virtual_ms: Some(10_000),
        slice_us: None,
        packages: Vec::new(),
        network: NetworkSpecFile {
            subnet: Some("10.0.0.0/24".into()),
            latency_ms: None,
            latency_us: Some(latency_us),
            jitter_ms: None,
            jitter_us: Some(200),
            drop_ppm: Some(50_000),
            duplicate_ppm: Some(50_000),
            links: Vec::new(),
        },
        faults,
        nemesis: None,
        nodes: vec![
            node_spec(
                "s1",
                "10.0.0.1",
                "service",
                "echo READY_S1; while true; do /rewind/tools/bin/nc -l -p 7000 -e /bin/sh -c 'echo ok-s1'; done",
                0,
                0,
            ),
            node_spec(
                "s2",
                "10.0.0.2",
                "service",
                "echo READY_S2; while true; do /rewind/tools/bin/nc -l -p 7000 -e /bin/sh -c 'echo ok-s2'; done",
                15,
                2_000,
            ),
            node_spec(
                "cli",
                "10.0.0.10",
                "driver",
                "set -e; sleep 0.15; \
                 echo R1=$(/rewind/tools/bin/nc -w 1 s1 7000 2>/dev/null || echo blocked); \
                 echo R2=$(/rewind/tools/bin/nc -w 1 s2 7000 2>/dev/null || echo blocked); \
                 sleep 0.25; \
                 echo R3=$(/rewind/tools/bin/nc -w 1 s1 7000 2>/dev/null || echo blocked); \
                 echo R4=$(/rewind/tools/bin/nc -w 1 s2 7000 2>/dev/null || echo blocked); \
                 echo DONE",
                -10,
                -2_000,
            ),
        ],
    }
}

fn assert_runs_bit_for_bit_equal(a: &ClusterRun, b: &ClusterRun) {
    assert_eq!(a.manifest.verdict, b.manifest.verdict, "verdict mismatch");
    assert_eq!(
        a.manifest.trace_hash, b.manifest.trace_hash,
        "combined trace_hash mismatch"
    );
    let oa = a.manifest.outcome.as_ref().expect("outcome a");
    let ob = b.manifest.outcome.as_ref().expect("outcome b");
    assert_eq!(oa.stop, ob.stop);
    assert_eq!(oa.cluster_ns, ob.cluster_ns);
    assert_eq!(oa.cluster_step, ob.cluster_step);
    assert_eq!(oa.tx_frames, ob.tx_frames);
    assert_eq!(oa.delivered_frames, ob.delivered_frames);
    assert_eq!(oa.dropped_frames, ob.dropped_frames);
    assert_eq!(oa.duplicated_frames, ob.duplicated_frames);
    assert_eq!(oa.nodes, ob.nodes);
    assert_eq!(
        a.net_events().expect("net events a"),
        b.net_events().expect("net events b"),
        "network.jsonl diverged"
    );
    for n in &a.manifest.nodes {
        let ta = fs::read(
            a.dir
                .join(CLUSTER_NODES_DIR)
                .join(&n.name)
                .join(rewind_trace::manifest::TRACE),
        )
        .expect("trace a");
        let tb = fs::read(
            b.dir
                .join(CLUSTER_NODES_DIR)
                .join(&n.name)
                .join(rewind_trace::manifest::TRACE),
        )
        .expect("trace b");
        assert_eq!(ta, tb, "trace.bin diverged for node {}", n.name);
    }
}

#[test]
fn parallel_vs_sequential_bit_for_bit_equivalence() {
    let Some((guest, cpu)) = require_kvm_and_guest() else {
        return;
    };

    let tmp_check = TempHome::new("pmu-check");
    let mut clocks = vec![ClockSource::Exits];
    if let Ok(vendor) = pmu::Vendor::detect()
        && let Some(ev) = vendor.event()
        && pmu::counter_usable_this_boot(&tmp_check.home, &guest, &vendor).unwrap_or(false)
    {
        clocks.push(ClockSource::Branches(ev));
    }

    let spec = three_node_tcp_spec(Vec::new(), 2_000);
    let spec_src = serde_json::to_string(&spec).unwrap();

    for clock in clocks {
        let tmp_seq = TempHome::new("eq-seq");
        let tmp_par = TempHome::new("eq-par");
        let opts = ClusterRunOptions {
            seed: 42,
            schedule: 7,
            epoch: Some(1_700_000_000),
            cpu,
            clock,
            keyframes: Keyframes::Take,
            ..ClusterRunOptions::default()
        };

        let mut prep_seq =
            prepare_cluster(&tmp_seq.home, &guest, &spec, PathBuf::from(".").as_path(), &spec_src, &opts)
                .expect("prepare seq");
        prep_seq.cluster_config.parallel = false;
        let run_seq = ClusterRun::record(&tmp_seq.home, &prep_seq, Keyframes::Take, false, None)
            .expect("record seq");

        let mut prep_par =
            prepare_cluster(&tmp_par.home, &guest, &spec, PathBuf::from(".").as_path(), &spec_src, &opts)
                .expect("prepare par");
        prep_par.cluster_config.parallel = true;
        let run_par = ClusterRun::record(&tmp_par.home, &prep_par, Keyframes::Take, false, None)
            .expect("record par");

        assert!(
            run_par.manifest.verdict.passed(),
            "expected passing run under {clock:?}, got {:?}",
            run_par.manifest.verdict
        );
        assert_runs_bit_for_bit_equal(&run_seq, &run_par);

        // Standalone replay must match bit-for-bit.
        let replayed = run_par
            .replay(&tmp_par.home, CompareMachine::Every)
            .expect("replay par");
        assert!(
            replayed.identical,
            "replay diverged under {clock:?}: nodes={:?} net={:?} machine={:?}",
            replayed.node_diffs, replayed.net_diff, replayed.machine_diff
        );
        assert!(replayed.net_events > 0);
        assert!(replayed.keyframes_compared >= 1);
    }
}

#[test]
fn partitioned_node_decoupling_and_heal_equivalence() {
    let Some((guest, cpu)) = require_kvm_and_guest() else {
        return;
    };

    // Short link latency (100 µs) + scripted partition from 50ms to 300ms so
    // partitioned nodes decouple by up to the unconstrained window (5 ms) and
    // cleanly re-synchronize at the 300ms heal barrier.
    let faults = vec![
        FaultSpecFile {
            at_ms: 50,
            kind: "partition".into(),
            groups: vec![vec!["s1".into(), "cli".into()], vec!["s2".into()]],
            blocked: Vec::new(),
            node: None,
        },
        FaultSpecFile {
            at_ms: 300,
            kind: "heal".into(),
            groups: Vec::new(),
            blocked: Vec::new(),
            node: None,
        },
    ];
    let spec = three_node_tcp_spec(faults, 100);
    let spec_src = serde_json::to_string(&spec).unwrap();

    let tmp_seq = TempHome::new("part-seq");
    let tmp_par = TempHome::new("part-par");
    let opts = ClusterRunOptions {
        seed: 11,
        schedule: 0,
        epoch: Some(1_700_000_000),
        cpu,
        clock: ClockSource::Exits,
        keyframes: Keyframes::Take,
        ..ClusterRunOptions::default()
    };

    let mut prep_seq =
        prepare_cluster(&tmp_seq.home, &guest, &spec, PathBuf::from(".").as_path(), &spec_src, &opts)
            .expect("prepare seq");
    prep_seq.cluster_config.parallel = false;
    let run_seq = ClusterRun::record(&tmp_seq.home, &prep_seq, Keyframes::Take, false, None)
        .expect("record seq");

    let mut prep_par =
        prepare_cluster(&tmp_par.home, &guest, &spec, PathBuf::from(".").as_path(), &spec_src, &opts)
            .expect("prepare par");
    prep_par.cluster_config.parallel = true;
    let run_par = ClusterRun::record(&tmp_par.home, &prep_par, Keyframes::Take, false, None)
        .expect("record par");

    assert!(run_par.manifest.verdict.passed());
    assert_runs_bit_for_bit_equal(&run_seq, &run_par);

    let net = run_par.net_events().expect("net events");
    assert!(
        net.iter().any(|e| matches!(e, NetEvent::Partition { .. })),
        "expected Partition event"
    );
    assert!(
        net.iter().any(|e| matches!(e, NetEvent::Heal { .. })),
        "expected Heal event"
    );

    let replayed = run_par
        .replay(&tmp_par.home, CompareMachine::Every)
        .expect("replay");
    assert!(replayed.identical, "partition replay diverged: {replayed:?}");
}

#[derive(Default)]
struct CaptureObserver {
    records: Vec<Vec<(u64, Vec<u8>)>>,
    net: Vec<NetEvent>,
}

impl ClusterObserver for CaptureObserver {
    fn node_record(&mut self, node: usize, step: u64, record: &[u8]) {
        self.records[node].push((step, record.to_vec()));
    }

    fn net_event(&mut self, event: &NetEvent) {
        self.net.push(event.clone());
    }
}

#[test]
fn clock_skew_drift_and_until_ns_chunking_invariance() {
    let Some((guest, cpu)) = require_kvm_and_guest() else {
        return;
    };

    let spec = three_node_tcp_spec(Vec::new(), 1_500);
    let spec_src = serde_json::to_string(&spec).unwrap();
    let tmp = TempHome::new("drift-chunk");
    let opts = ClusterRunOptions {
        seed: 77,
        schedule: 3,
        epoch: Some(1_700_000_000),
        cpu,
        clock: ClockSource::Exits,
        keyframes: Keyframes::Skip,
        ..ClusterRunOptions::default()
    };
    let mut prep =
        prepare_cluster(&tmp.home, &guest, &spec, PathBuf::from(".").as_path(), &spec_src, &opts)
            .expect("prepare");
    prep.cluster_config.parallel = true;

    // 1. Run straight through with until_ns = None.
    let mut c_whole = Cluster::boot(prep.cluster_config.clone(), DirtyLog::Off).expect("boot whole");
    let mut obs_whole = CaptureObserver {
        records: vec![Vec::new(); 3],
        net: Vec::new(),
    };
    let out_whole = c_whole.run(&mut obs_whole, None, None).expect("run whole");
    assert_eq!(out_whole.stop, ClusterStop::Completed);

    // 2. Run in irregular until_ns chunks (37ms, 73ms, 111ms, ...).
    let mut c_chunked =
        Cluster::boot(prep.cluster_config.clone(), DirtyLog::Off).expect("boot chunked");
    let mut obs_chunked = CaptureObserver {
        records: vec![Vec::new(); 3],
        net: Vec::new(),
    };
    let chunk_sizes_ns = [37_000_000u64, 73_000_000, 111_000_000, 19_000_000];
    let mut ci = 0usize;
    let out_chunked = loop {
        let target = c_chunked
            .cluster_ns()
            .saturating_add(chunk_sizes_ns[ci % chunk_sizes_ns.len()]);
        ci += 1;
        let out = c_chunked
            .run(&mut obs_chunked, Some(target), None)
            .expect("run chunk");
        if out.stop != ClusterStop::Paused {
            break out;
        }
    };

    assert_eq!(out_whole.stop, out_chunked.stop);
    assert_eq!(out_whole.cluster_ns, out_chunked.cluster_ns);
    assert_eq!(out_whole.cluster_step, out_chunked.cluster_step);
    assert_eq!(out_whole.nodes, out_chunked.nodes);
    assert_eq!(obs_whole.records, obs_chunked.records);
    assert_eq!(obs_whole.net, obs_chunked.net);
}

#[test]
fn snapshot_fault_snapshot_restore_and_standalone_replay() {
    let Some((guest, cpu)) = require_kvm_and_guest() else {
        return;
    };

    let tmp = TempHome::new("snap-fault-snap");
    let spec = ClusterSpecFile {
        name: "snap-fault-snap".into(),
        mem_mib: Some(128),
        cores: Some(1),
        stop_when: Some("driver".into()),
        max_virtual_ms: Some(10_000),
        slice_us: None,
        packages: Vec::new(),
        network: NetworkSpecFile {
            subnet: Some("10.0.0.0/24".into()),
            latency_ms: Some(2),
            latency_us: None,
            jitter_ms: None,
            jitter_us: Some(100),
            drop_ppm: Some(0),
            duplicate_ppm: Some(0),
            links: Vec::new(),
        },
        faults: Vec::new(),
        nemesis: None,
        nodes: vec![
            node_spec(
                "srv",
                "10.0.0.1",
                "service",
                "echo SRV_READY; while true; do /rewind/tools/bin/nc -l -p 7000 -e /bin/sh -c 'echo pong'; done",
                0,
                0,
            ),
            node_spec(
                "cli",
                "10.0.0.2",
                "driver",
                "set -e; sleep 0.2; \
                 echo P1=$(/rewind/tools/bin/nc -w 1 srv 7000 2>/dev/null || echo blocked); \
                 sleep 0.3; \
                 echo P2=$(/rewind/tools/bin/nc -w 1 srv 7000 2>/dev/null || echo blocked); \
                 echo ALL_DONE",
                5,
                500,
            ),
        ],
    };

    let opts = ClusterRunOptions {
        seed: 5,
        schedule: 0,
        epoch: Some(1_700_000_000),
        cpu,
        clock: ClockSource::Exits,
        keyframes: Keyframes::Take,
        ..ClusterRunOptions::default()
    };

    let mut session =
        ClusterSession::start(tmp.home.clone(), &guest, &spec, PathBuf::from(".").as_path(), &opts)
            .expect("session start");

    let r = session
        .wait_for("srv", "SRV_READY", LogStream::Output, Some(5_000_000_000), false, None)
        .expect("wait srv");
    assert!(r.matched);
    let p1 = session
        .wait_for("cli", "P1=", LogStream::Output, Some(5_000_000_000), false, None)
        .expect("wait P1");
    assert_eq!(p1.line.as_deref(), Some("P1=pong"));

    // Snapshot BEFORE partition, then immediately partition, then immediately
    // snapshot AFTER partition (no virtual time advance in between!).
    let (snap_a, _, step_a) = session.snapshot(Some("before-part".into())).expect("snap a");
    session
        .partition(&["srv".into()], Some(&["cli".into()]), true)
        .expect("partition");
    let (snap_b, _, step_b) = session.snapshot(Some("after-part".into())).expect("snap b");
    assert!(
        step_b > step_a,
        "applying a fault must advance cluster_step so consecutive snapshots are distinct"
    );

    // Advance on partitioned branch -> P2 must be blocked.
    let p2_blocked = session
        .wait_for("cli", "P2=", LogStream::Output, Some(5_000_000_000), false, None)
        .expect("wait P2 blocked");
    assert_eq!(p2_blocked.line.as_deref(), Some("P2=blocked"));

    // Restore `after-part` -> partition MUST still be active!
    session.restore(&snap_b).expect("restore snap_b");
    assert!(
        !session.status().active_partitions.is_empty(),
        "restoring snapshot taken immediately after partition must keep partition active"
    );
    let p2_still_blocked = session
        .wait_for("cli", "P2=", LogStream::Output, Some(5_000_000_000), false, None)
        .expect("wait P2 still blocked");
    assert_eq!(p2_still_blocked.line.as_deref(), Some("P2=blocked"));

    // Restore `before-part` -> partition must be gone -> P2 succeeds!
    session.restore(&snap_a).expect("restore snap_a");
    assert!(session.status().active_partitions.is_empty());
    let p2_ok = session
        .wait_for("cli", "P2=", LogStream::Output, Some(5_000_000_000), false, None)
        .expect("wait P2 ok");
    assert_eq!(p2_ok.line.as_deref(), Some("P2=pong"));

    let exit = session
        .wait_exit(Some("cli"), Some(5_000_000_000), None)
        .expect("wait exit");
    assert!(exit.exited && exit.exit_code == Some(0));

    let run = session.finish(None, None).expect("finish");
    assert!(run.manifest.verdict.passed());

    let replayed = run
        .replay(&tmp.home, CompareMachine::Every)
        .expect("replay finished session");
    assert!(
        replayed.identical,
        "finished session replay diverged: {replayed:?}"
    );

    // Verify exact mid-run node seeking via ClusterRun::seek_node.
    let total_cli_steps = run
        .manifest
        .outcome
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .find(|n| n.name == "cli")
        .unwrap()
        .steps;
    let target_step = (total_cli_steps / 2).max(1);
    let (_m, actual_step) = run
        .seek_node(&tmp.home, "cli", target_step)
        .expect("seek_node");
    assert_eq!(actual_step, target_step);
}
