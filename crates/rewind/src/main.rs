//! The `rewind` command.

mod show;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rewind_core::image;
use rewind_core::run::{BASE_CMDLINE, DEFAULT_EPOCH, DEFAULT_QUANTUM};
use rewind_core::{Echo, Guest, Home, Run, Source, Spec};
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
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 1024)]
    mem: u64,
    /// The guest's wall clock at boot, in seconds since the Unix epoch.
    #[arg(long, env = "SOURCE_DATE_EPOCH", default_value_t = DEFAULT_EPOCH)]
    epoch: u64,
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
            };
            let name = machine.name.clone().unwrap_or_else(|| argv.join(" "));
            let source = Source::Image {
                root: root.display().to_string(),
            };
            execute(&home, &guest, name, source, Some(image), job, &machine)
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
        epoch: machine.epoch,
        quantum: DEFAULT_QUANTUM,
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
    let status = run.manifest.outcome.as_ref().and_then(|o| o.status);
    Ok(match status {
        Some(0) => ExitCode::SUCCESS,
        Some(s) => ExitCode::from(show::exit_code(s)),
        None => ExitCode::FAILURE,
    })
}
