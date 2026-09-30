//! The `rewind` command.

mod show;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rewind_core::run::{BASE_CMDLINE, DEFAULT_QUANTUM, default_epoch};
use rewind_core::{Echo, Guest, Home, Keyframes, Run, Source, Spec};
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
    /// Skip keyframes: faster, but seeking into the run starts from boot.
    #[arg(long)]
    no_keyframes: bool,
    /// Extra kernel command line arguments; `loglevel=7` shows the kernel's
    /// messages in the trace.
    #[arg(long, default_value = "")]
    kernel_args: String,
}

/// A command in a root filesystem, for `run` and `check`.
#[derive(clap::Args, Clone)]
struct ImageArgs {
    /// The root filesystem: a directory, an erofs image, or a tarball
    /// such as `docker export` writes.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Environment variables for the command, as KEY=VALUE.
    #[arg(long = "env", short = 'e')]
    env: Vec<String>,
    /// The working directory in the guest.
    #[arg(long, default_value = "/")]
    cwd: String,
    /// The command and its arguments.
    #[arg(last = true)]
    argv: Vec<String>,
}

/// What a run runs.
enum Workload {
    Nix(String),
    Image(ImageArgs),
}

#[derive(Subcommand)]
enum Command {
    /// Run a command in a root filesystem.
    Run {
        #[command(flatten)]
        image: ImageArgs,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Build a Nix derivation in the deterministic VM.
    Nix {
        /// A .drv path or an installable such as `nixpkgs#hello`.
        installable: String,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Run a Nix derivation, or a command with --root, under several
    /// schedules and show where the first run that ends differently went
    /// its own way.
    Check {
        /// A .drv or installable; leave it out and give --root and a
        /// command to check a command instead.
        installable: Option<String>,
        #[command(flatten)]
        image: ImageArgs,
        /// How many perturbed schedules to try besides the unperturbed one.
        #[arg(long, default_value_t = 64)]
        schedules: u64,
        /// Try every schedule and report how many end differently, instead
        /// of stopping at the first.
        #[arg(long)]
        all: bool,
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
        /// Show the kernel's own threads too.
        #[arg(long)]
        all: bool,
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
    Replay {
        run: String,
        /// Start from the keyframe at or before this step instead of boot.
        #[arg(long)]
        from: Option<u64>,
    },
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
        Command::Run { image, machine } => {
            let guest = Guest::from_env()?;
            let run = run_workload(&home, &guest, &Workload::Image(image), &machine)?;
            Ok(exit_status(&run))
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
            image,
            schedules,
            all,
            mut machine,
        } => {
            let guest = Guest::from_env()?;
            // Exploring compares runs; only the two reported get keyframes.
            machine.no_keyframes = true;
            let workload = match installable {
                Some(i) => Workload::Nix(i),
                None => Workload::Image(image),
            };
            machine.quiet = true;

            // The unperturbed build, and the step its job started on:
            // everything before that is boot, and perturbing it would only
            // make every run differ from the first kernel thread on.
            machine.schedule = 0;
            machine.schedule_from = 0;
            let base = run_workload(&home, &guest, &workload, &machine)?;
            println!("schedule   0: {}", show::outcome_line(&base)?);
            let base_trace = base.trace()?;
            let start = show::start_step(&base_trace);
            let base_key = show::outcome_key(&base)?;

            let mut failing = None;
            let mut differing = 0;
            let mut tried = 0;
            for schedule in 1..=schedules {
                machine.schedule = schedule;
                machine.schedule_from = start;
                let run = run_workload(&home, &guest, &workload, &machine)?;
                println!("schedule {schedule:>3}: {}", show::outcome_line(&run)?);
                tried += 1;
                if show::outcome_key(&run)? != base_key {
                    differing += 1;
                    failing.get_or_insert(run);
                    if !all {
                        break;
                    }
                }
            }
            if all {
                println!("{differing} of {tried} perturbed schedules ended differently");
            }
            let Some(mut worst) = failing else {
                println!("same result under all {} schedules", schedules + 1);
                return Ok(ExitCode::SUCCESS);
            };

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
                let run = run_workload(&home, &guest, &workload, &machine)?;
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
            let base = base.add_keyframes(&home)?;
            let worst = worst.add_keyframes(&home)?;
            println!("passing: run {}", base.manifest.id);
            println!("failing: run {}", worst.manifest.id);
            let (bt, wt) = (base.trace()?, worst.trace()?);
            match show::culprit(&wt) {
                Some(argv) => {
                    println!("\nwhere {} first behaves differently:", argv.join(" "));
                    print!("{}", show::divergence_in(&bt, &wt, &argv));
                }
                None => print!("{}", show::divergence(&bt, &wt)),
            }
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
                Keyframes::Take,
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
        Command::Ps { run, at, all } => {
            let run = Run::find(&home, &run)?;
            let trace = run.trace()?;
            let at = at.unwrap_or(trace.last_step());
            let threads = if all {
                show::KernelThreads::Show
            } else {
                show::KernelThreads::Hide
            };
            print!("{}", show::process_tree(&trace, at, threads));
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
        Command::Replay {
            run,
            from: Some(step),
        } => {
            let run = Run::find(&home, &run)?;
            let started = std::time::Instant::now();
            let (kf, original, again) = run.replay_from(&home, step)?;
            match original.divergence(&again) {
                None => {
                    println!(
                        "identical from the keyframe at step {kf} to the end ({:.2}s)",
                        started.elapsed().as_secs_f64()
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Some(_) => {
                    println!("DIFFERENT after the keyframe at step {kf}:");
                    print!("{}", show::divergence(&original, &again));
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::Replay { run, from: None } => {
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

/// Runs a workload with the given machine options.
fn run_workload(
    home: &Home,
    guest: &Guest,
    workload: &Workload,
    machine: &MachineArgs,
) -> Result<Run> {
    match workload {
        Workload::Nix(installable) => nix_run(home, guest, installable, machine),
        Workload::Image(image) => image_run(home, guest, image, machine),
    }
}

/// Runs a command in a root filesystem in the guest.
fn image_run(home: &Home, guest: &Guest, args: &ImageArgs, machine: &MachineArgs) -> Result<Run> {
    let root = args
        .root
        .as_ref()
        .context("give --root with the root filesystem to run in")?;
    if args.argv.is_empty() {
        bail!("give the command to run after --");
    }
    let image = root_image(home, root)?;
    let job = Job {
        argv: args.argv.clone(),
        env: parse_env(&args.env)?,
        cwd: args.cwd.clone(),
        uid: 0,
        gid: 0,
        hostname: "localhost".into(),
        root: Root::Image,
        files: Vec::new(),
        outputs: Vec::new(),
    };
    let name = machine.name.clone().unwrap_or_else(|| args.argv.join(" "));
    let source = Source::Image {
        root: root.display().to_string(),
    };
    execute_spec(home, guest, name, source, Some(image), job, machine)
}

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

    // A tarball's image is named after the tarball's hash, so checking a
    // command under many schedules converts it once.
    let cached = if root.is_file() {
        let key = image::hash_file(root)?;
        let path = home.images().join(format!("tar-{}.erofs", &key[..32]));
        if path.exists() {
            return Ok(path);
        }
        Some(path)
    } else {
        None
    };

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

    // Directory images are named by their own contents, so the same tree
    // twice is one file on disk.
    let path = match cached {
        Some(path) => path,
        None => {
            let hash = image::hash_file(&tmp)?;
            home.images().join(format!("{}.erofs", &hash[..32]))
        }
    };
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
    let keyframes = if machine.no_keyframes {
        Keyframes::Skip
    } else {
        Keyframes::Take
    };
    let run = Run::execute(home, name, source, spec, None, echo, keyframes)?;
    eprintln!("{}", show::finished(&run));
    Ok(run)
}
