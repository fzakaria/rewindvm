//! `rewind show`: the commands that make a run again.
//!
//! A run's id is the hash of its inputs, so the same command on a machine
//! with the same CPU vendor and the same guest makes the same run. Every
//! input a command line sets is spelled out, the epoch above all: by
//! default it is the start of the day the run was made, so the same
//! command a day later makes another run.

use rewind_core::run::{BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM};
use rewind_core::{Manifest, Source};
use rewind_init::Output;

/// The arguments after `rewind` that make the run `m` describes again: a
/// fork of its parent, or a run or Nix build from boot. Inputs no
/// command line sets, such as the quantum, are named in `Unset`.
pub fn command(m: &Manifest) -> Result<Vec<String>, Unset> {
    let spec = &m.spec;
    let mut args: Vec<String> = Vec::new();

    // A fork is its parent's run up to its step, then its own schedule.
    if let Some((parent, step)) = &m.parent {
        let mut args = vec![
            "fork".to_string(),
            parent.clone(),
            step.to_string(),
            "--schedule".to_string(),
            spec.schedule.to_string(),
        ];
        if let Some(seconds) = timed_out_after(m) {
            args.extend(["--timeout".to_string(), seconds.to_string()]);
        }
        return Ok(args);
    }

    // Inputs this rewind always sets one way.
    if spec.quantum != DEFAULT_QUANTUM {
        return Err(Unset(format!(
            "its quantum of {} ns, where this rewind takes {DEFAULT_QUANTUM}",
            spec.quantum
        )));
    }
    if spec.extras != rewind_vmm::Extras::Reserved {
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
        Source::Image { root } => {
            let job = &spec.job;
            flag(&mut args, "root", root.clone());
            if job.cwd != "/" {
                flag(&mut args, "cwd", job.cwd.clone());
            }
            let default_path = ("PATH".to_string(), crate::DEFAULT_PATH.to_string());
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

    // The machine: the epoch and the clock always, since neither is the
    // same everywhere by default, and every other option away from its
    // default.
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
    if spec.cores != DEFAULT_CORES {
        flag(&mut args, "cores", spec.cores.to_string());
    }
    if spec.mem_mib != crate::DEFAULT_MEM_MIB {
        flag(&mut args, "mem", spec.mem_mib.to_string());
    }
    if spec.cpu == rewind_vmm::CpuModel::Host {
        flag(&mut args, "cpu", "host".into());
    }
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
    if !rewind_trace::stop::timed_out(&o.stop) {
        return None;
    }
    Some(o.wall_ms.div_ceil(1000))
}

/// An input of a run that no command line of this rewind sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unset(pub String);

#[cfg(test)]
mod tests {
    // Commands for manifests built by hand: a command in a root
    // filesystem with every machine option away from its default, a Nix
    // build with them all at their defaults, and a fork.
    use super::*;
    use rewind_core::Spec;
    use rewind_core::run::{RunOutcome, ScheduleSegment};
    use rewind_init::{Job, Root};

    fn spec(job: Job) -> Spec {
        Spec {
            kernel: "/k".into(),
            initrd: "/i".into(),
            kernel_debug: None,
            image: Some("/images/x.erofs".into()),
            image_hash: Some("ab".into()),
            mem_mib: crate::DEFAULT_MEM_MIB,
            cores: DEFAULT_CORES,
            seed: 0,
            epoch: 1_791_072_000,
            quantum: DEFAULT_QUANTUM,
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            inherited_schedules: Vec::new(),
            cpu: rewind_vmm::CpuModel::V3,
            clock: rewind_vmm::ClockSource::Exits,
            preemption: rewind_vmm::Preemption::AtExits,
            extras: rewind_vmm::Extras::Reserved,
            cmdline: BASE_CMDLINE.into(),
            job,
        }
    }

    fn job(argv: &[&str], env: &[(&str, &str)], cwd: &str) -> Job {
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
            "version": 1,
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
        // terminal, extra kernel arguments, and a run that timed out after
        // 2.5 s.
        let mut s = spec(job(
            &["make", "check"],
            &[("PATH", crate::DEFAULT_PATH), ("CC", "gcc -O1")],
            "/src",
        ));
        s.job.output = Output::Terminal;
        s.seed = 3;
        s.schedule = 4;
        s.schedule_from = 100;
        s.schedule_until = 200;
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
            stop: rewind_core::run::TIMED_OUT.into(),
            step: 9,
            virtual_ns: 0,
            status: None,
            wall_ms: 2_500,
        });
        let mut want = words(
            "run --root mylib.tar --cwd /src --env CC=gcc_-O1 --tty --epoch 1791072000 --seed 3 \
             --schedule 4 --schedule-from 100 --schedule-until 200 --cores 4 --mem 2048 \
             --cpu host --clock branches --kernel-args norandmaps --timeout 3 --name",
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
    fn a_nix_build_names_its_derivation_epoch_clock_and_name() {
        // A Nix build with every option at its default still names its
        // epoch, which is not the same from one day to the next, and its
        // clock, which `auto` resolves per machine.
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
        want.extend(words("--epoch 1791072000 --clock exits --name mylib-0.3.0"));
        assert_eq!(command(&m).unwrap(), want);
    }

    #[test]
    fn a_fork_is_its_parent_at_its_step_under_its_schedule() {
        // A fork of a fork: its own step and seed, whatever it inherited.
        let mut s = spec(job(&["sh"], &[], "/"));
        s.schedule = 5;
        s.schedule_from = 400;
        s.inherited_schedules = vec![ScheduleSegment {
            seed: 3,
            from: 300,
            until: 400,
        }];
        let mut m = manifest("x (fork)", Source::Image { root: "/r".into() }, s);
        m.parent = Some(("fedcba9876543210".into(), 400));
        assert_eq!(
            command(&m).unwrap(),
            words("fork fedcba9876543210 400 --schedule 5")
        );
    }

    #[test]
    fn an_input_no_flag_sets_is_named() {
        // The quantum has no command-line option.
        let mut s = spec(job(&["sh"], &[], "/"));
        s.quantum = DEFAULT_QUANTUM + 1;
        let m = manifest("sh", Source::Image { root: "/r".into() }, s);
        assert!(command(&m).unwrap_err().0.contains("quantum"));
    }
}
