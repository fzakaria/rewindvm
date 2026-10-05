//! The `rewind` subcommands that read recorded runs: listing them, the
//! commands that make them, their output, processes and events, comparing
//! and replaying them, and opening them in the desktop app.

use std::process::ExitCode;

use anyhow::{Result, bail};
use rewind_core::{Guest, Home, Run, image};

use crate::{RUN_HELP, RUN_LONG_HELP, json, list, reproduce, show};

/// The arguments of `rewind open`.
#[derive(clap::Args)]
pub struct OpenArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The step the playhead starts at; by default the app's choice.
    pub(crate) step: Option<u64>,
    /// Show this run beside it, lined up with it, as the passing run
    /// beside a failing one.
    #[arg(long, value_name = "RUN")]
    pub(crate) compare: Option<String>,
}

pub fn open(home: &Home, args: OpenArgs) -> Result<ExitCode> {
    let OpenArgs { run, step, compare } = args;
    use std::os::unix::process::CommandExt;
    let run = Run::find(home, &run)?;
    let step = step.map(|s| run.check_step(s)).transpose()?;
    let compare = compare.map(|c| Run::find(home, &c)).transpose()?;
    let program = std::env::var_os(APP_ENV).unwrap_or_else(|| APP_PROGRAM.into());
    let args = app_args(&run.dir, step, compare.as_ref().map(|c| c.dir.as_path()));

    // exec only returns when the app did not start.
    let e = std::process::Command::new(&program).args(args).exec();
    if e.kind() == std::io::ErrorKind::NotFound {
        bail!(
            "the desktop app, {}, is not on PATH; install it from \
                 https://rewindvm.dev, or set {APP_ENV} to it",
            program.to_string_lossy()
        );
    }
    Err(anyhow::Error::new(e).context(format!("starting {}", program.to_string_lossy())))
}

/// The arguments of `rewind ls`.
#[derive(clap::Args)]
pub struct LsArgs {
    /// List only the newest N of the runs the other options pick.
    #[arg(short = 'n', long = "limit", value_name = "N")]
    pub(crate) limit: Option<usize>,
    /// Only runs whose name contains TEXT.
    #[arg(long, value_name = "TEXT")]
    pub(crate) name: Option<String>,
    /// Only runs that stand so.
    #[arg(long, value_enum)]
    pub(crate) status: Option<list::Status>,
    /// Only the runs forked from RUN, and forks of those.
    #[arg(long, value_name = "RUN")]
    pub(crate) forks_of: Option<String>,
    /// Only runs made since WHEN: an amount ago, such as 30m, 2h, 7d or
    /// 2w, or a day, such as 2026-10-01, from its start in UTC.
    #[arg(long, value_name = "WHEN")]
    pub(crate) since: Option<String>,
    /// Print one JSON object a line per run, for programs: its id,
    /// name, directory, when it was made, its parent, how it stands,
    /// its wait status, ending and steps.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn ls(home: &Home, args: LsArgs) -> Result<ExitCode> {
    let LsArgs {
        limit,
        name,
        status,
        forks_of,
        since,
        json,
    } = args;
    // Every run and every unreadable one, in one list, newest first.
    let listing = Run::list_all(home)?;
    let mut rows: Vec<(list::Entry, list::Row)> = listing
        .runs
        .iter()
        .map(|r| (list::Entry::of(r), list::Row::Run(r)))
        .chain(
            listing
                .unreadable
                .iter()
                .map(|u| (list::Entry::unreadable(u), list::Row::Unreadable(u))),
        )
        .collect();
    rows.sort_by_key(|(entry, _)| std::cmp::Reverse(entry.created));

    // The runs the options pick, the newest `limit` of them.
    let entries: Vec<list::Entry> = rows.iter().map(|(e, _)| e.clone()).collect();
    let among = match &forks_of {
        Some(run) => Some(list::forks_of(&entries, &Run::find(home, run)?.manifest.id)),
        None => None,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filter = list::Filter {
        name,
        status,
        among,
        since: since.map(|s| list::since(&s, now)).transpose()?,
    };
    let picked = rows
        .iter()
        .filter(|(entry, _)| filter.keeps(entry))
        .take(limit.unwrap_or(usize::MAX));
    for (entry, row) in picked {
        if json {
            println!("{}", list::json(entry, row));
        } else {
            println!("{}", list::line(row));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind show`.
#[derive(clap::Args)]
pub struct ShowArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// Print one JSON object on standard output, for programs: the
    /// run's id, whether this rewind boots the guest the run booted, and
    /// oldest first each command with the id of the run it makes, as
    /// arguments after `rewind` and as a line for a shell.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn show(home: &Home, args: ShowArgs) -> Result<ExitCode> {
    let ShowArgs { run, json } = args;
    let run = Run::find(home, &run)?;

    // The run and the runs it was forked from, oldest first, as far
    // back as this home has them. Imported manifests can name each
    // other as parents in a loop, which ends at the first repeat.
    let mut chain = vec![run.manifest.clone()];
    let mut gone = None;
    while let Some(parent) = chain.last().and_then(|m| m.parent.clone()).map(|p| p.run) {
        if chain.iter().any(|m| m.id == parent) {
            break;
        }
        match Run::find(home, &parent) {
            Ok(p) => chain.push(p.manifest),
            Err(_) => {
                gone = Some(parent);
                break;
            }
        }
    }
    chain.reverse();
    let commands = chain
        .iter()
        .map(|m| {
            let args = reproduce::command(m).map_err(|unset| {
                anyhow::anyhow!(
                    "no command line makes run {} again: it has {}",
                    m.id,
                    unset.0
                )
            })?;
            let line = std::iter::once("rewind".to_string())
                .chain(args.iter().map(|a| show::quote(a)))
                .collect::<Vec<_>>()
                .join(" ");
            Ok((m.id.clone(), args, line))
        })
        .collect::<Result<Vec<_>>>()?;

    // The guest is an input too, by its contents.
    let same_guest = Guest::from_env().ok().map(|guest| {
        let same = |recorded: &std::path::Path, now: &std::path::Path| {
            recorded == now || image::hash_file(recorded).ok() == image::hash_file(now).ok()
        };
        let spec = &run.manifest.spec;
        same(&spec.kernel, &guest.kernel) && same(&spec.initrd, &guest.initrd)
    });

    if json {
        let commands: Vec<serde_json::Value> = commands
            .iter()
            .map(|(id, args, line)| serde_json::json!({ "id": id, "args": args, "line": line }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "id": run.manifest.id,
                "recorded_by": run.manifest.recorded_by,
                "same_guest": same_guest,
                "parent_gone": gone,
                "commands": commands,
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", show::summary(&run));
    println!("recorded by rewind {}", run.manifest.recorded_by);
    if let Some(parent) = gone {
        eprintln!("rewind: run {parent}, which the first of these forks, is not in this home");
    }
    if same_guest == Some(false) {
        eprintln!(
            "rewind: this rewind boots another kernel or initramfs than the run did, so \
                 these commands make other runs"
        );
    }
    for (id, _, line) in &commands {
        println!("{line}  # {id}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind log`.
#[derive(clap::Args)]
pub struct LogArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The step to print up to; by default the run's end.
    pub(crate) step: Option<u64>,
    /// Prefix each line with its step and pid.
    #[arg(long, short)]
    pub(crate) steps: bool,
}

pub fn log(home: &Home, args: LogArgs) -> Result<ExitCode> {
    let LogArgs { run, step, steps } = args;
    let run = Run::find(home, &run)?;
    let until = step.map(|s| run.check_step(s)).transpose()?;
    let trace = run.trace()?;
    for line in trace.lines_until(until.unwrap_or(u64::MAX)) {
        if steps {
            println!("{:>10} {:>5}  {}", line.step, line.pid, line.text);
        } else {
            println!("{}", line.text);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind ps`.
#[derive(clap::Args)]
pub struct PsArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The step; by default the run's end.
    pub(crate) step: Option<u64>,
    /// Show the kernel's own threads too.
    #[arg(long)]
    pub(crate) all: bool,
    /// Print one JSON object on standard output, for programs: the step
    /// and each process alive then, with its parent, command line and
    /// threads.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn ps(home: &Home, args: PsArgs) -> Result<ExitCode> {
    let PsArgs {
        run,
        step,
        all,
        json,
    } = args;
    let run = Run::find(home, &run)?;
    let at = step.map(|s| run.check_step(s)).transpose()?;
    let trace = run.trace()?;
    let at = at.unwrap_or(trace.last_step());
    let threads = if all {
        show::KernelThreads::Show
    } else {
        show::KernelThreads::Hide
    };
    if json {
        println!("{}", json::processes(trace, at, threads));
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", show::process_tree(trace, at, threads));
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind events`.
#[derive(clap::Args)]
pub struct EventsArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The first step whose events to print; by default 0.
    #[arg(long, value_name = "STEP")]
    pub(crate) from: Option<u64>,
    /// The last step whose events to print; by default the run's end.
    #[arg(long, value_name = "STEP")]
    pub(crate) to: Option<u64>,
    /// One JSON object per line.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn events(home: &Home, args: EventsArgs) -> Result<ExitCode> {
    let EventsArgs {
        run,
        from,
        to,
        json,
    } = args;
    let run = Run::find(home, &run)?;
    let from = from.map(|s| run.check_step(s)).transpose()?;
    let to = to.map(|s| run.check_step(s)).transpose()?;
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

/// The arguments of `rewind replay`.
#[derive(clap::Args)]
pub struct ReplayArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// Start from the keyframe at or before this step instead of boot.
    #[arg(long, value_name = "STEP")]
    pub(crate) from: Option<u64>,
    /// Print one JSON object on standard output, for programs: whether
    /// the trace came out identical, the keyframe it started from, and
    /// where it first differed.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn replay(home: &Home, args: ReplayArgs) -> Result<ExitCode> {
    match args {
        ReplayArgs {
            run,
            from: Some(step),
            json,
        } => {
            let run = Run::find(home, &run)?;
            let step = run.check_step(step)?;
            let started = std::time::Instant::now();
            let (kf, original, again) = run.replay_from(home, step)?;
            let identical = original.divergence(&again).is_none();
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "identical": identical,
                        "keyframe": kf,
                        "divergence": json::divergence(&original, &again),
                    })
                );
                return Ok(if identical {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                });
            }
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
        ReplayArgs {
            run,
            from: None,
            json,
        } => {
            let run = Run::find(home, &run)?;
            let replayed = run.replay()?;
            if json {
                let divergence = replayed.as_ref().map(|d| {
                    serde_json::json!({
                        "index": d.index,
                        "left_step": d.left_step,
                        "right_step": d.right_step,
                    })
                });
                println!(
                    "{}",
                    serde_json::json!({
                        "identical": replayed.is_none(),
                        "keyframe": null,
                        "divergence": divergence,
                    })
                );
                return Ok(if replayed.is_none() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                });
            }
            match replayed {
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
    }
}

/// The arguments of `rewind diff`.
#[derive(clap::Args)]
pub struct DiffArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) left: String,
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) right: String,
    /// Print one JSON object on standard output, for programs: the two
    /// runs' ids and where they first differ, null when identical.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn diff(home: &Home, args: DiffArgs) -> Result<ExitCode> {
    let DiffArgs { left, right, json } = args;
    let left = Run::find(home, &left)?;
    let right = Run::find(home, &right)?;
    let (lt, rt) = (left.trace()?, right.trace()?);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "left": left.manifest.id,
                "right": right.manifest.id,
                "divergence": json::divergence(lt, rt),
            })
        );
    } else {
        print!("{}", show::divergence(lt, rt));
    }
    match lt.divergence(rt) {
        None => Ok(ExitCode::SUCCESS),
        Some(_) => Ok(ExitCode::FAILURE),
    }
}

/// rewind-app's arguments for a run's directory, the step to start at and
/// the directory of the run to show beside it.
fn app_args(
    dir: &std::path::Path,
    step: Option<u64>,
    compare: Option<&std::path::Path>,
) -> Vec<std::ffi::OsString> {
    let mut args = vec![dir.as_os_str().to_owned()];
    if let Some(step) = step {
        args.extend(["--step".into(), step.to_string().into()]);
    }
    if let Some(compare) = compare {
        args.extend(["--compare".into(), compare.as_os_str().to_owned()]);
    }
    args
}

/// The desktop app, looked up on PATH unless REWIND_APP names it.
const APP_PROGRAM: &str = "rewind-app";

const APP_ENV: &str = "REWIND_APP";

#[cfg(test)]
mod tests {
    // The arguments `rewind open` starts the app with.
    use super::*;
    use crate::Cli;
    use clap::Parser;

    #[test]
    fn the_app_is_given_the_run_its_step_and_the_run_beside_it() {
        // rewind-app takes a run's directory, then --step and --compare
        // when asked for; a run alone opens alone.
        let args = |step, compare: Option<&str>| -> Vec<String> {
            app_args(
                std::path::Path::new("/runs/a"),
                step,
                compare.map(std::path::Path::new),
            )
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
        };
        assert_eq!(args(None, None), vec!["/runs/a"]);
        assert_eq!(
            args(Some(5), Some("/runs/b")),
            vec!["/runs/a", "--step", "5", "--compare", "/runs/b"]
        );
        assert!(Cli::try_parse_from("rewind open abc 5 --compare def".split_whitespace()).is_ok());
    }
}
