//! `rewind show`: the commands that make a run again.
//!
//! A run's id is the hash of its inputs, so the same command on a machine
//! with the same CPU vendor and guest, and the features of the run's CPU
//! model, makes the same run. Every input a command line sets is spelled
//! out, the epoch above all: by default it is the start of the day the run
//! was made, so the same command a day later makes another run.

use rewind_core::run::{BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM, Parent};
use rewind_core::{Manifest, Source};
use rewind_init::Output;

/// The arguments after `rewind` that make the run `m` describes again: a
/// fork of its parent, or a run or Nix build from boot. Inputs no
/// command line sets, such as the quantum or a fork's bounded schedule,
/// are named in `Unset`.
pub(crate) fn command(m: &Manifest) -> Result<Vec<String>, Unset> {
    let spec = &m.spec;
    let mut args: Vec<String> = Vec::new();

    // A fork is made from its parent.
    if let Some(parent) = &m.parent {
        return fork_command(m, parent);
    }

    // Inputs this rewind always sets one way.
    if spec.quantum != DEFAULT_QUANTUM {
        return Err(Unset(format!(
            "its quantum of {} ns, where this rewind takes {DEFAULT_QUANTUM}",
            spec.quantum
        )));
    }
    // A run of a fixed model, v3 or v3-compat, always reserves the extras
    // slot. A host-model run leaves the slot out on a CPU that cannot reach
    // the slot, and the same command leaves the slot out again on that CPU.
    if spec.extras != rewind_vmm::Extras::Reserved && spec.cpu != rewind_vmm::CpuModel::Host {
        return Err(Unset(
            "a machine without the slot `shell --with` uses".into(),
        ));
    }

    // What runs: a derivation, or a command in a root filesystem with its
    // working directory and environment. A command's environment starts
    // with the default PATH unless --env replaced it.
    let program = match &m.source {
        Source::Nix { drv, .. } => {
            let program = vec!["nix".to_string(), drv.clone()];
            Some(program)
        }
        // A described build's machine is in its description, the
        // derivation's rewindFlags, and `rewind build` takes no other.
        Source::Build { description, .. } => {
            return Ok(vec!["build".to_string(), description.clone()]);
        }
        Source::Image { root } => {
            let job = &spec.job;
            flag(&mut args, "root", root.clone());
            if job.cwd != "/" {
                flag(&mut args, "cwd", job.cwd.clone());
            }
            let default_path = (
                "PATH".to_string(),
                crate::cmd::record::DEFAULT_PATH.to_string(),
            );
            let env = match job.env.first() {
                Some(first) if *first == default_path => &job.env[1..],
                _ => &job.env[..],
            };
            for (key, value) in env {
                flag(&mut args, "env", format!("{key}={value}"));
            }
            if job.output == Output::Terminal {
                args.push("--tty".into());
            }
            None
        }
    };

    // The machine: the epoch, the CPU model and the clock always, since
    // none of them is the same everywhere by default, and every other
    // option away from its default.
    flag(&mut args, "epoch", spec.epoch.to_string());
    if spec.seed != 0 {
        flag(&mut args, "seed", spec.seed.to_string());
    }
    if spec.schedule != 0 {
        flag(&mut args, "schedule", spec.schedule.to_string());
    }
    if spec.schedule_from != 0 {
        flag(&mut args, "schedule-from", spec.schedule_from.to_string());
    }
    if spec.schedule_until != u64::MAX {
        flag(&mut args, "schedule-until", spec.schedule_until.to_string());
    }
    if !spec.stall_max.is_default() {
        flag(&mut args, "stall-max", spec.stall_max.to_string());
    }
    if spec.cores != DEFAULT_CORES {
        flag(&mut args, "cores", spec.cores.to_string());
    }
    if spec.mem_mib != crate::cmd::record::DEFAULT_MEM_MIB {
        flag(&mut args, "mem", spec.mem_mib.to_string());
    }
    flag(&mut args, "cpu", spec.cpu.cli_name().into());
    let clock = match spec.clock {
        rewind_vmm::ClockSource::Exits => "exits",
        rewind_vmm::ClockSource::Branches(rewind_vmm::CounterEvent::Instructions) => {
            return Err(Unset("a clock that counts instructions".into()));
        }
        rewind_vmm::ClockSource::Branches(_) => "branches",
    };
    flag(&mut args, "clock", clock.into());
    if spec.preemption == rewind_vmm::Preemption::AtBranchCounts {
        args.push("--experimental-preempt".into());
    }
    let extra = spec
        .cmdline
        .strip_prefix(BASE_CMDLINE)
        .map(str::trim)
        .ok_or_else(|| Unset(format!("the kernel command line {:?}", spec.cmdline)))?;
    if !extra.is_empty() {
        flag(&mut args, "kernel-args", extra.to_string());
    }
    if let Some(seconds) = timed_out_after(m) {
        flag(&mut args, "timeout", seconds.to_string());
    }
    flag(&mut args, "name", m.name.clone());

    // The derivation leads; a command follows the options.
    Ok(match program {
        Some(mut program) => {
            program.extend(args);
            program
        }
        None => {
            let mut command = vec!["run".to_string()];
            command.extend(args);
            command.push("--".into());
            command.extend(spec.job.argv.iter().cloned());
            command
        }
    })
}

/// The arguments after `rewind` that make the fork `m` of `parent` again.
/// A fork is its parent's run up to its step, then its own schedule, with
/// its own longest stall and, when it was given one, its own seed; the
/// schedules and seeds from before its step are the parent's. `rewind
/// check` makes forks `rewind fork` has no flags for, and those are named
/// in `Unset`.
fn fork_command(m: &Manifest, parent: &Parent) -> Result<Vec<String>, Unset> {
    let spec = &m.spec;

    // `rewind fork` perturbs from the fork's step to the end of the run.
    if spec.schedule_until != u64::MAX {
        return Err(Unset(format!(
            "a schedule that stops at step {}, where `rewind fork` perturbs to the end of the run",
            spec.schedule_until
        )));
    }

    // The fork's own seeds: `rewind fork --seed` gives one, at the fork's
    // step. A seed from before the step is the parent's.
    let mut own = spec.reseeds.iter().filter(|r| r.step >= parent.step);
    let seed = match (own.next(), own.next()) {
        (None, _) => None,
        (Some(reseed), None) if reseed.step == parent.step => Some(reseed.seed),
        (Some(reseed), _) => {
            return Err(Unset(format!(
                "seed {} from step {}, where `rewind fork` gives a seed from the fork's step {}",
                reseed.seed, reseed.step, parent.step
            )));
        }
    };

    // `rewind fork` refuses a fork that would repeat its parent.
    if spec.schedule == 0 && seed.is_none() {
        return Err(Unset(
            "schedule 0 and no seed of its own, which `rewind fork` refuses".into(),
        ));
    }

    let mut args = vec![
        "fork".to_string(),
        parent.run.to_string(),
        parent.step.to_string(),
    ];
    flag(&mut args, "schedule", spec.schedule.to_string());
    if let Some(seed) = seed {
        flag(&mut args, "seed", seed.to_string());
    }
    if !spec.stall_max.is_default() {
        flag(&mut args, "stall-max", spec.stall_max.to_string());
    }
    if let Some(seconds) = timed_out_after(m) {
        flag(&mut args, "timeout", seconds.to_string());
    }
    Ok(args)
}

/// Adds `--name value` to `args`.
fn flag(args: &mut Vec<String>, name: &str, value: String) {
    args.push(format!("--{name}"));
    args.push(value);
}

/// For a run stopped at its time limit, the whole seconds it had, which
/// `--timeout` gives again; the step it stops at still depends on how fast
/// the machine is.
fn timed_out_after(m: &Manifest) -> Option<u64> {
    let o = m.outcome.as_ref()?;
    o.stop.timeout()?;
    Some(o.wall_ms.div_ceil(1000))
}

/// An input of a run that no command line of this rewind sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unset(pub String);

#[cfg(test)]
pub(crate) mod tests {
    // Commands for manifests built by hand: a command in a root
    // filesystem with every machine option away from its default, a Nix
    // build with them all at their defaults, and a fork.
    use super::*;
    use rewind_core::Spec;
    use rewind_core::run::{RunOutcome, ScheduleSegment};
    use rewind_init::{Job, Root};
    use rewind_trace::schedule::StallMax;

    pub(crate) fn spec(job: Job) -> Spec {
        Spec {
            kernel: "/k".into(),
            initrd: "/i".into(),
            kernel_debug: None,
            image: Some("/images/x.erofs".into()),
            image_hash: Some("ab".into()),
            mem_mib: crate::cmd::record::DEFAULT_MEM_MIB,
            cores: DEFAULT_CORES,
            seed: 0,
            epoch: 1_791_072_000,
            quantum: DEFAULT_QUANTUM,
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            stall_max: StallMax::default(),
            inherited_schedules: Vec::new(),
            reseeds: Vec::new(),
            cpu: rewind_vmm::CpuModel::V3,
            clock: rewind_vmm::ClockSource::Exits,
            preemption: rewind_vmm::Preemption::AtExits,
            extras: rewind_vmm::Extras::Reserved,
            cmdline: BASE_CMDLINE.into(),
            job,
        }
    }

    pub(crate) fn job(argv: &[&str], env: &[(&str, &str)], cwd: &str) -> Job {
        Job {
            program: None,
            argv: argv.iter().map(|a| a.to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: cwd.into(),
            uid: 0,
            gid: 0,
            hostname: "localhost".into(),
            root: Root::Image,
            files: Vec::new(),
            outputs: Vec::new(),
            output: Output::Plain,
        }
    }

    fn manifest(name: &str, source: Source, spec: Spec) -> Manifest {
        serde_json::from_value(serde_json::json!({
            "version": rewind_trace::manifest::MANIFEST_VERSION,
            "id": "0123456789abcdef",
            "name": name,
            "created": 0,
            "source": source,
            "spec": spec,
            "parent": null,
            "outcome": null,
            "recorded_by": rewind_core::VERSION,
        }))
        .unwrap()
    }

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn a_command_in_a_root_spells_out_every_input() {
        // Every machine option away from its default, an environment
        // variable after the default PATH, a working directory, a
        // terminal, a longest stall, extra kernel arguments, and a run
        // that timed out after 2.5 s.
        let mut s = spec(job(
            &["make", "check"],
            &[
                ("PATH", crate::cmd::record::DEFAULT_PATH),
                ("CC", "gcc -O1"),
            ],
            "/src",
        ));
        s.job.output = Output::Terminal;
        s.seed = 3;
        s.schedule = 4;
        s.schedule_from = 100;
        s.schedule_until = 200;
        s.stall_max = "50ms".parse().unwrap();
        s.cores = 4;
        s.mem_mib = 2048;
        s.cpu = rewind_vmm::CpuModel::Host;
        s.clock = rewind_vmm::ClockSource::Branches(
            rewind_vmm::CounterEvent::AmdRetiredConditionalBranches,
        );
        s.cmdline = format!("{BASE_CMDLINE} norandmaps");
        let mut m = manifest(
            "make check",
            Source::Image {
                root: "mylib.tar".into(),
            },
            s,
        );
        m.outcome = Some(RunOutcome {
            stop: rewind_trace::stop::Stop::TimedOut(rewind_trace::stop::Timeout {
                since_exit_ms: 0,
                doing: rewind_trace::stop::Doing::MakingExits,
            }),
            step: 9,
            virtual_ns: 0,
            status: None,
            wall_ms: 2_500,
        });
        let mut want = words(
            "run --root mylib.tar --cwd /src --env CC=gcc_-O1 --tty --epoch 1791072000 --seed 3 \
             --schedule 4 --schedule-from 100 --schedule-until 200 --stall-max 50ms --cores 4 \
             --mem 2048 --cpu host --clock branches --kernel-args norandmaps --timeout 3 --name",
        );
        want[6] = "CC=gcc -O1".into();
        want.extend([
            "make check".into(),
            "--".into(),
            "make".into(),
            "check".into(),
        ]);
        assert_eq!(command(&m).unwrap(), want);
    }

    #[test]
    fn a_nix_build_names_its_derivation_epoch_cpu_clock_and_name() {
        // A Nix build with every option at its default still names its
        // epoch, which is not the same from one day to the next, and its
        // CPU model and clock, which a command without them resolves per
        // machine.
        let drv = "/nix/store/0000000000000000000000000000000a-mylib-0.3.0.drv";
        let m = manifest(
            "mylib-0.3.0",
            Source::Nix {
                drv: drv.into(),
                outputs: vec![],
            },
            spec(job(&["bash"], &[], "/build")),
        );
        let mut want = vec!["nix".to_string(), drv.to_string()];
        want.extend(words(
            "--epoch 1791072000 --cpu v3 --clock exits --name mylib-0.3.0",
        ));
        assert_eq!(command(&m).unwrap(), want);
    }

    #[test]
    fn a_described_build_is_made_again_from_its_description() {
        // A build nix-daemon described holds its machine in the
        // description, so the command is the description alone, with none
        // of the options a run or a Nix build names.
        let m = manifest(
            "greet",
            Source::Build {
                description: "/tmp/nix-build-greet.drv-0/build.json".into(),
                outputs: vec![],
            },
            spec(job(&["bash"], &[], "/build")),
        );
        assert_eq!(
            command(&m).unwrap(),
            words("build /tmp/nix-build-greet.drv-0/build.json")
        );
    }

    #[test]
    fn every_cpu_model_is_named() {
        // A run of each model, made again: the command names the model,
        // since a command without --cpu picks v3 or v3-compat by what the
        // host has, and the two make different runs.
        for (model, name) in [
            (rewind_vmm::CpuModel::V3, "v3"),
            (rewind_vmm::CpuModel::V3Compat, "v3-compat"),
            (rewind_vmm::CpuModel::Host, "host"),
        ] {
            let mut s = spec(job(&["sh"], &[], "/"));
            s.cpu = model;
            let m = manifest("sh", Source::Image { root: "/r".into() }, s);
            let args = command(&m).unwrap();
            assert!(args.windows(2).any(|w| w == ["--cpu", name]), "{args:?}");
        }
    }

    #[test]
    fn a_fork_is_its_parent_at_its_step_under_its_schedule() {
        // A fork of a fork: its own step, seed and longest stall, whatever
        // it inherited.
        let mut s = spec(job(&["sh"], &[], "/"));
        s.schedule = 5;
        s.schedule_from = 400;
        s.inherited_schedules = vec![ScheduleSegment {
            seed: 3,
            from: 300,
            until: 400,
            stall_max: "100ms".parse().unwrap(),
        }];
        let mut m = manifest("x (fork)", Source::Image { root: "/r".into() }, s);
        m.parent = Some(Parent {
            run: rewind_core::run::RunId::parse("fedcba9876543210").unwrap(),
            step: 400,
        });
        assert_eq!(
            command(&m).unwrap(),
            words("fork fedcba9876543210 400 --schedule 5")
        );

        // A longest stall of its own is spelled out; the inherited one is
        // the parent's.
        m.spec.stall_max = "50ms".parse().unwrap();
        assert_eq!(
            command(&m).unwrap(),
            words("fork fedcba9876543210 400 --schedule 5 --stall-max 50ms")
        );
    }

    /// A fork of run fedcba9876543210 at step 400 under `spec`.
    fn fork_at_400(spec: Spec) -> Manifest {
        let mut m = manifest("x (fork)", Source::Image { root: "/r".into() }, spec);
        m.parent = Some(Parent {
            run: rewind_core::run::RunId::parse("fedcba9876543210").unwrap(),
            step: 400,
        });
        m
    }

    #[test]
    fn a_fork_given_a_seed_names_the_seed() {
        // A fork whose RNG is reseeded at its own step, as `rewind fork
        // --seed` makes one, under schedule 0 and under schedule 5. A
        // reseed before the step is the parent's and takes no flag.
        let mut s = spec(job(&["sh"], &[], "/")).fork(400, 0).reseeded(400, 7);
        assert_eq!(
            command(&fork_at_400(s.clone())).unwrap(),
            words("fork fedcba9876543210 400 --schedule 0 --seed 7")
        );
        s.schedule = 5;
        s = s.reseeded(300, 9);
        assert_eq!(
            command(&fork_at_400(s)).unwrap(),
            words("fork fedcba9876543210 400 --schedule 5 --seed 7")
        );
    }

    #[test]
    fn a_fork_no_one_command_makes_is_named() {
        // Forks as `rewind check` makes them and `rewind fork` cannot: a
        // schedule that stops before the end of the run, a new seed that
        // takes effect after the fork's step, and schedule 0 with no new
        // seed. Each is named instead of a command that would make
        // another run.
        let forked = spec(job(&["sh"], &[], "/")).fork(400, 5);
        let mut bounded = forked.clone();
        bounded.schedule_until = 900;
        let unset = command(&fork_at_400(bounded)).unwrap_err().0;
        assert!(unset.contains("stops at step 900"), "{unset}");

        let later = forked.reseeded(450, 7);
        let unset = command(&fork_at_400(later)).unwrap_err().0;
        assert!(unset.contains("seed 7 from step 450"), "{unset}");

        let mut unperturbed = forked.clone();
        unperturbed.schedule = 0;
        let unset = command(&fork_at_400(unperturbed)).unwrap_err().0;
        assert!(unset.contains("schedule 0"), "{unset}");
    }

    #[test]
    fn an_input_no_flag_sets_is_named() {
        // The quantum has no command-line option.
        let mut s = spec(job(&["sh"], &[], "/"));
        s.quantum = DEFAULT_QUANTUM + 1;
        let m = manifest("sh", Source::Image { root: "/r".into() }, s);
        assert!(command(&m).unwrap_err().0.contains("quantum"));
    }

    #[test]
    fn a_host_model_run_without_the_extras_slot_is_made_again() {
        // A host-model run recorded where the CPU's address bits cannot
        // reach the extras slot: --cpu host on that host leaves the slot
        // out again. A v3 run always has the slot, so one without it
        // cannot be made again.
        let mut s = spec(job(&["sh"], &[], "/"));
        s.cpu = rewind_vmm::CpuModel::Host;
        s.extras = rewind_vmm::Extras::Absent;
        let m = manifest("sh", Source::Image { root: "/r".into() }, s.clone());
        let args = command(&m).unwrap();
        assert!(args.windows(2).any(|w| w == ["--cpu", "host"]), "{args:?}");

        s.cpu = rewind_vmm::CpuModel::V3;
        let m = manifest("sh", Source::Image { root: "/r".into() }, s);
        assert!(command(&m).unwrap_err().0.contains("shell --with"));
    }
}
