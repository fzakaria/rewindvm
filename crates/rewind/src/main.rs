//! The `rewind` command.

mod show;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rewind_core::run::{BASE_CMDLINE, DEFAULT_QUANTUM, default_epoch};
use rewind_core::{Echo, Guest, Home, Run, Source, Spec};
use rewind_core::{image, nix};
use rewind_init::{Job, Root};

#[derive(Parser)]
#[command(
    version,
    about = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The machine options every way of starting a run shares.
#[derive(clap::Args)]
struct MachineArgs {
    /// Seeds the guest's randomness. Runs with the same inputs and seed are
    /// identical; a different seed explores a different run.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Asks the guest to reschedule at steps this seed picks, to explore
    /// other thread interleavings with everything else, time included,
    /// unchanged. 0 is the unperturbed schedule.
    #[arg(long, default_value_t = 0)]
    schedule: u64,
    /// The first step a reschedule may be asked at; before it the run is the
    /// unperturbed one.
    #[arg(long, default_value_t = 0)]
    schedule_from: u64,
    /// The step after which no more reschedules are asked.
    #[arg(long, default_value_t = u64::MAX)]
    schedule_until: u64,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 1024)]
    mem: u64,
    /// The guest's wall clock at boot, in seconds since the Unix epoch.
    /// Defaults to the start of today, UTC.
    #[arg(long)]
    epoch: Option<u64>,
    /// A name to find the run by later.
    #[arg(long)]
    name: Option<String>,
    /// Print nothing while the run executes.
    #[arg(long, short)]
    quiet: bool,
    /// Extra kernel command line arguments; `loglevel=7` shows the kernel's
    /// messages in the trace.
    #[arg(long, default_value = "")]
    kernel_args: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run a command in a root filesystem.
    Run {
        /// The root filesystem: a directory, an erofs image, or a tarball
        /// such as `docker export` writes.
        #[arg(long)]
        root: PathBuf,
        /// Environment variables for the command, as KEY=VALUE.
        #[arg(long = "env", short = 'e')]
        env: Vec<String>,
        /// The working directory in the guest.
        #[arg(long, default_value = "/")]
        cwd: String,
        #[command(flatten)]
        machine: MachineArgs,
        /// The command and its arguments.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Build a Nix derivation in the deterministic VM.
    Nix {
        /// A .drv path or an installable such as `nixpkgs#hello`.
        installable: String,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Build a Nix derivation under several schedules and show where the
    /// first build that ends differently went its own way.
    Check {
        installable: String,
        /// How many perturbed schedules to try besides the unperturbed one.
        #[arg(long, default_value_t = 8)]
        schedules: u64,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Branch a run at a step: the same run up to the step, then another
    /// interleaving from there.
    Fork {
        run: String,
        step: u64,
        /// The schedule seed for the new branch.
        #[arg(long, default_value_t = 1)]
        schedule: u64,
        #[arg(long, short)]
        quiet: bool,
    },
    /// List runs, newest first.
    Ls,
    /// Print a run's output, up to a step.
    Log {
        run: String,
        #[arg(long)]
        at: Option<u64>,
        /// Prefix each line with its step and pid.
        #[arg(long, short)]
        steps: bool,
    },
    /// Show the processes alive at a step.
    Ps {
        run: String,
        #[arg(long)]
        at: Option<u64>,
    },
    /// Print a run's events.
    Events {
        run: String,
        #[arg(long)]
        from: Option<u64>,
        #[arg(long)]
        to: Option<u64>,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Run a run's inputs again and check the trace comes out identical.
    Replay { run: String },
    /// Compare two runs and show where they first differ.
    Diff { left: String, right: String },
}

fn main() -> ExitCode {
    // Printing into a closed pipe, as `rewind ls | head` does, should end
    // the program quietly the way it ends any other Unix tool.
    // SAFETY: restoring a default signal disposition.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rewind: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let home = Home::open()?;
    match cli.command {
        Command::Run {
            root,
            env,
            cwd,
            machine,
            argv,
        } => {
            let guest = Guest::from_env()?;
            let image = root_image(&home, &root)?;
            let job = Job {
                argv: argv.clone(),
                env: parse_env(&env)?,
                cwd,
                uid: 0,
                gid: 0,
                hostname: "localhost".into(),
                root: Root::Image,
                files: Vec::new(),
                outputs: Vec::new(),
            };
            let name = machine.name.clone().unwrap_or_else(|| argv.join(" "));
            let source = Source::Image {
                root: root.display().to_string(),
            };
            execute(&home, &guest, name, source, Some(image), job, &machine)
        }
        Command::Nix {
            installable,
            machine,
        } => {
            let guest = Guest::from_env()?;
            let run = nix_run(&home, &guest, &installable, &machine)?;
            report_outputs(&run)?;
            Ok(exit_status(&run))
        }
        Command::Check {
            installable,
            schedules,
            mut machine,
        } => {
            let guest = Guest::from_env()?;
            machine.quiet = true;

            // The unperturbed build, and the step its job started on:
            // everything before that is boot, and perturbing it would only
            // make every run differ from the first kernel thread on.
            machine.schedule = 0;
            machine.schedule_from = 0;
            let base = nix_run(&home, &guest, &installable, &machine)?;
            println!("schedule   0: {}", show::outcome_line(&base)?);
            let base_trace = base.trace()?;
            let start = show::start_step(&base_trace);
            let base_key = show::outcome_key(&base)?;

            let mut failing = None;
            for schedule in 1..=schedules {
                machine.schedule = schedule;
                machine.schedule_from = start;
                let run = nix_run(&home, &guest, &installable, &machine)?;
                println!("schedule {schedule:>3}: {}", show::outcome_line(&run)?);
                if failing.is_none() && show::outcome_key(&run)? != base_key {
                    failing = Some(run);
                }
            }
            let Some(mut worst) = failing else {
                println!("same result under all {} schedules", schedules + 1);
                return Ok(ExitCode::SUCCESS);
            };

            // Perturbing from the job's start finds failures, but a timer
            // delayed while the compiler runs shifts everything after it,
            // so the two runs part ways long before anything interesting.
            // Look again with perturbation starting where the failing
            // program was exec'd in the baseline: a failure found that way
            // parts from the baseline inside the program itself.
            let mut from = start;
            if let Some(culprit) = show::culprit(&worst.trace()?) {
                if let Some(exec) = show::exec_step(&base_trace, &culprit) {
                    println!(
                        "\n{} failed under schedule {}; searching again from its exec at step {exec}",
                        culprit.join(" "),
                        worst.manifest.spec.schedule
                    );
                    for schedule in 1..=schedules * SEARCH_FACTOR {
                        machine.schedule = schedule;
                        machine.schedule_from = exec;
                        let run = nix_run(&home, &guest, &installable, &machine)?;
                        if show::outcome_key(&run)? != base_key {
                            println!("schedule {schedule:>3}: {}", show::outcome_line(&run)?);
                            worst = run;
                            from = exec;
                            break;
                        }
                    }
                }
            }
            let start = from;

            // Narrow it to a window of steps: first the earliest end that
            // still changes the outcome, then the latest start. Each step's
            // perturbation depends only on the seed and the step, so a
            // smaller window perturbs a subset of the same steps. The two
            // runs are identical up to the window, so where they part is
            // inside it, next to the interleaving that matters.
            machine.schedule = worst.manifest.spec.schedule;
            machine.schedule_from = start;
            let end = worst.manifest.outcome.as_ref().map_or(start, |o| o.step);
            println!(
                "\nschedule {} ends differently; narrowing the steps it perturbs",
                machine.schedule
            );
            let mut probe = |from: u64, until: u64| -> Result<Option<Run>> {
                machine.schedule_from = from;
                machine.schedule_until = until;
                let run = nix_run(&home, &guest, &installable, &machine)?;
                Ok((show::outcome_key(&run)? != base_key).then_some(run))
            };
            let (mut lo, mut hi) = (start, end);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                match probe(start, mid)? {
                    Some(run) => {
                        hi = mid;
                        worst = run;
                    }
                    None => lo = mid,
                }
            }
            let until = hi;
            let (mut lo, mut hi) = (start, until);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                match probe(mid, until)? {
                    Some(run) => {
                        lo = mid;
                        worst = run;
                    }
                    None => hi = mid,
                }
            }
            println!("perturbing only steps {lo}..{until} still ends differently\n");
            println!("passing: run {}", base.manifest.id);
            println!("failing: run {}", worst.manifest.id);
            print!("{}", show::divergence(&base_trace, &worst.trace()?));
            Ok(ExitCode::FAILURE)
        }
        Command::Fork {
            run,
            step,
            schedule,
            quiet,
        } => {
            let parent = Run::find(&home, &run)?;
            let m = &parent.manifest;
            if schedule == 0 {
                bail!("schedule 0 is the unperturbed run; a fork needs another seed");
            }
            let mut spec = m.spec.clone();
            spec.schedule = schedule;
            spec.schedule_from = step;
            spec.schedule_until = u64::MAX;
            let name = format!(
                "{} (fork of {} at {step}, schedule {schedule})",
                m.name, m.id
            );
            let echo = if quiet { Echo::Quiet } else { Echo::Output };
            let child = Run::execute(
                &home,
                name,
                m.source.clone(),
                spec,
                Some((m.id.clone(), step)),
                echo,
            )?;
            eprintln!("{}", show::finished(&child));
            let (pt, ct) = (parent.trace()?, child.trace()?);
            match pt.divergence(&ct) {
                None => eprintln!("rewind: the fork ran the same as its parent"),
                Some(d) => eprintln!(
                    "rewind: the fork first differs from its parent at step {}",
                    d.right_step
                ),
            }
            Ok(exit_status(&child))
        }
        Command::Ls => {
            for r in Run::list(&home)? {
                println!("{}", show::summary(&r));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Log { run, at, steps } => {
            let run = Run::find(&home, &run)?;
            let trace = run.trace()?;
            for line in trace.lines_until(at.unwrap_or(u64::MAX)) {
                if steps {
                    println!("{:>10} {:>5}  {}", line.step, line.pid, line.text);
                } else {
                    println!("{}", line.text);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Ps { run, at } => {
            let run = Run::find(&home, &run)?;
            let trace = run.trace()?;
            let at = at.unwrap_or(trace.last_step());
            print!("{}", show::process_tree(&trace, at));
            Ok(ExitCode::SUCCESS)
        }
        Command::Events {
            run,
            from,
            to,
            json,
        } => {
            let run = Run::find(&home, &run)?;
            let trace = run.trace()?;
            let (from, to) = (from.unwrap_or(0), to.unwrap_or(u64::MAX));
            for e in trace
                .events
                .iter()
                .filter(|e| (from..=to).contains(&e.step))
            {
                if json {
                    println!("{}", serde_json::to_string(e)?);
                } else {
                    println!("{}", show::event(e));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Replay { run } => {
            let run = Run::find(&home, &run)?;
            match run.replay()? {
                None => {
                    println!(
                        "identical: {} events over {} steps",
                        run.trace()?.events.len(),
                        run.manifest.outcome.as_ref().map_or(0, |o| o.step)
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Some(d) => {
                    println!(
                        "DIFFERENT at event {} (step {} vs {}): the run was not deterministic",
                        d.index, d.left_step, d.right_step
                    );
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::Diff { left, right } => {
            let left = Run::find(&home, &left)?;
            let right = Run::find(&home, &right)?;
            let (lt, rt) = (left.trace()?, right.trace()?);
            print!("{}", show::divergence(&lt, &rt));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// How many more schedules `check` tries when searching from the failing
/// program's exec, as a multiple of the schedules asked for.
const SEARCH_FACTOR: u64 = 4;

/// Runs a derivation's builder in the guest.
fn nix_run(home: &Home, guest: &Guest, installable: &str, machine: &MachineArgs) -> Result<Run> {
    let drv_path = nix::resolve(installable)?;
    let drv = nix::show(&drv_path)?;
    let extra: Vec<PathBuf> = guest.sandbox_shell.iter().cloned().collect();
    let closure = nix::input_closure(&drv, &extra)?;

    // Images of store paths are named by the paths, which already name
    // their contents.
    let key = blake3::hash(
        closure
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    )
    .to_hex();
    let image = home.images().join(format!("store-{}.erofs", &key[..32]));
    if !image.exists() {
        eprintln!(
            "rewind: packing {} store paths for {}",
            closure.len(),
            drv.name
        );
        let tmp = image.with_extension("building");
        image::from_store_paths(&closure, &tmp)?;
        std::fs::rename(&tmp, &image)?;
    }

    let job = nix::job(&drv)?;
    let name = machine.name.clone().unwrap_or_else(|| drv.name.clone());
    let source = Source::Nix {
        drv: drv_path.display().to_string(),
        outputs: drv
            .outputs
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
    };
    let run = execute_spec(home, guest, name, source, Some(image), job, machine)?;
    Ok(run)
}

/// Prints each output's hash from the guest, and whether it matches the
/// copy of that output the host already has, if it has one.
fn report_outputs(run: &Run) -> Result<()> {
    for e in run.trace()?.events {
        let rewind_trace::EventKind::Mark { text } = e.kind else {
            continue;
        };
        let Some(rest) = text.strip_prefix(rewind_init::OUTPUT_MARK) else {
            continue;
        };
        let Some((path, hash)) = rest.split_once(' ') else {
            continue;
        };
        let host = std::path::Path::new(path);
        let verdict = if host.exists() {
            match rewind_init::tree_hash(host) {
                Ok(h) if h.to_hex().as_str() == hash => "same as the host's build",
                Ok(_) => "DIFFERENT from the host's build",
                Err(_) => "host copy unreadable",
            }
        } else {
            "not built on the host"
        };
        println!("{path} {} ({verdict})", &hash[..16]);
    }
    Ok(())
}

/// Builds or reuses the erofs image for a root filesystem argument.
fn root_image(home: &Home, root: &std::path::Path) -> Result<PathBuf> {
    if root.extension().is_some_and(|e| e == "erofs") {
        return Ok(root.to_path_buf());
    }
    let tmp = home
        .images()
        .join(format!("building-{}.erofs", std::process::id()));
    if root.is_dir() {
        image::from_dir(root, &tmp)?;
    } else if root.is_file() {
        image::from_tar(root, &tmp)?;
    } else {
        bail!("{} is neither a directory nor a file", root.display());
    }
    // Images are named by their contents, so the same root twice is one
    // file on disk.
    let hash = image::hash_file(&tmp)?;
    let path = home.images().join(format!("{}.erofs", &hash[..32]));
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut env = vec![(
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    )];
    for pair in pairs {
        let (k, v) = pair
            .split_once('=')
            .with_context(|| format!("--env {pair:?} is not KEY=VALUE"))?;
        env.retain(|(key, _)| key != k);
        env.push((k.to_string(), v.to_string()));
    }
    Ok(env)
}

fn execute(
    home: &Home,
    guest: &Guest,
    name: String,
    source: Source,
    image: Option<PathBuf>,
    job: Job,
    machine: &MachineArgs,
) -> Result<ExitCode> {
    let run = execute_spec(home, guest, name, source, image, job, machine)?;
    Ok(exit_status(&run))
}

/// The job's wait status as the process exit code, the way a shell reports
/// a child.
fn exit_status(run: &Run) -> ExitCode {
    match run.manifest.outcome.as_ref().and_then(|o| o.status) {
        Some(0) => ExitCode::SUCCESS,
        Some(s) => ExitCode::from(show::exit_code(s)),
        None => ExitCode::FAILURE,
    }
}

fn execute_spec(
    home: &Home,
    guest: &Guest,
    name: String,
    source: Source,
    image: Option<PathBuf>,
    job: Job,
    machine: &MachineArgs,
) -> Result<Run> {
    let image_hash = match &image {
        Some(path) => Some(image::hash_file(path)?),
        None => None,
    };
    let spec = Spec {
        kernel: guest.kernel.clone(),
        initrd: guest.initrd.clone(),
        image,
        image_hash,
        mem_mib: machine.mem,
        seed: machine.seed,
        epoch: machine.epoch.unwrap_or_else(default_epoch),
        quantum: DEFAULT_QUANTUM,
        schedule: machine.schedule,
        schedule_from: machine.schedule_from,
        schedule_until: machine.schedule_until,
        cmdline: format!("{BASE_CMDLINE} {}", machine.kernel_args)
            .trim()
            .to_string(),
        job,
    };
    let echo = if machine.quiet {
        Echo::Quiet
    } else {
        Echo::Output
    };
    let run = Run::execute(home, name, source, spec, None, echo)?;
    eprintln!("{}", show::finished(&run));
    Ok(run)
}
