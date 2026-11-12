//! `rewind time`: where a run's time went, and what would make it finish
//! sooner, as [`rewind_trace::time_report`] works it out; the desktop
//! app's Time tab shows the same report.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rewind_core::{Home, Run};
use rewind_trace::critical::TimeError;
use rewind_trace::machine::ClockSource;
use rewind_trace::time_report::{
    Budget, PathNode, PathTree, Row, RowKind, State, TREE_SHARE, Timing, share,
};

use crate::{RUN_HELP, RUN_LONG_HELP};

/// The arguments of `rewind time`.
#[derive(clap::Args)]
pub(crate) struct TimeArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// Look inside one process: its pid, a program's name, or a word of
    /// its command line, which picks the process of that name the
    /// critical path spends the most in.
    #[arg(long, value_name = "PID|NAME")]
    pub(crate) r#in: Option<String>,
    /// Print the chain itself, in order, one line per stretch of one
    /// process.
    #[arg(long)]
    pub(crate) chain: bool,
    /// How many rows "What would shorten it" shows.
    #[arg(long, default_value_t = DEFAULT_LINES)]
    pub(crate) lines: usize,
    /// Write the run as a Perfetto trace to this file, for
    /// ui.perfetto.dev: the scheduler's switches, wakes, forks, execs and
    /// exits, with the longest chain of work and the Nix phases as tracks
    /// of their own.
    #[arg(long, value_name = "FILE")]
    pub(crate) perfetto: Option<std::path::PathBuf>,
    /// Print the chain as folded stacks, one line per stack with its
    /// microseconds on the chain, for flamegraph.pl or speedscope.app.
    #[arg(long)]
    pub(crate) folded: bool,
}

/// Nanoseconds in a microsecond, the unit folded stacks count in.
const NS_PER_US: u64 = 1_000;

/// How many lines the table shows by default.
const DEFAULT_LINES: usize = 10;

/// The share of the path a row must shorten it by to be shown.
const LEVER_SHARE: f64 = 0.01;

/// The width of a command in "Look closer".
const COMMAND_WIDTH: usize = 40;

/// How many wrappers sharing a line are named before the rest are cut.
const WRAPPERS_SHOWN: usize = 3;

/// How many states the path's line names.
const STATES_SHOWN: usize = 3;

/// How many of a process's waits on other processes are shown.
const WAITS_SHOWN: usize = 5;

/// The width a line's name is cut to, and a phase bar's at its longest.
const NAME_WIDTH: usize = 64;
const BAR_WIDTH: usize = 24;

/// A share of the path a process's own computing must be under to go
/// unsaid on its line: a leaf that only computes says so by its time.
const ALL_COMPUTING: f64 = 0.98;

pub(crate) fn time(home: &Home, args: TimeArgs) -> Result<ExitCode> {
    let run = Run::find(home, &args.run)?;
    if run.manifest.spec.clock == ClockSource::Exits {
        bail!("{}", TimeError::ExitClock);
    }
    let trace = run.trace()?;
    let io = run.io()?;
    let end_ns = run
        .manifest
        .outcome
        .as_ref()
        .map(|o| o.virtual_ns)
        .context("the run has not finished")?;
    let timing = Timing::new(trace, &io, end_ns)?;
    let id = run.manifest.id.to_string();
    let short = &id[..id.len().min(8)];

    if let Some(path) = &args.perfetto {
        let bytes = rewind_trace::perfetto::trace(trace, &timing);
        std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "wrote {} ({} KiB); open it at https://ui.perfetto.dev",
            path.display(),
            bytes.len() / 1024
        );
        return Ok(ExitCode::SUCCESS);
    }
    if args.folded {
        for (stack, ns) in timing.folded() {
            let us = ns / NS_PER_US;
            if us > 0 {
                println!("{stack} {us}");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.chain {
        print_chain(&timing);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(which) = &args.r#in {
        let life = timing
            .pick(which)
            .with_context(|| format!("no process {which} in the run"))?;
        print_inside(&timing, life, short, args.lines);
        return Ok(ExitCode::SUCCESS);
    }
    print_job(&timing, &run.manifest.name, args.lines, short);
    Ok(ExitCode::SUCCESS)
}

/// The run: its two times, the critical path by phase and by process,
/// what would shorten it most, and where to look closer. Nothing in it
/// knows what kind of run it is but the phases, which only a Nix build
/// prints.
fn print_job(timing: &Timing, name: &str, limit: usize, run: &str) {
    let report = timing.report();
    let tree = timing.path_tree(None, None, Budget::Lines);
    let chain = tree.ns;
    println!("{name}");
    println!(
        "  {:>8}  elapsed on the VM's one CPU",
        fmt_ns(report.took_ns)
    );
    println!(
        "  {:>8}  critical path: the least it can take on any number of CPUs",
        fmt_ns(chain)
    );
    println!("  {:>8}  {}", "", states_line(&tree.states, chain));

    // The path by phase, for a run that prints them.
    if !report.phases.is_empty() {
        println!();
        println!("The critical path by phase");
        let min_ns = (chain as f64 * TREE_SHARE) as u64;
        let (big, small): (Vec<_>, Vec<_>) =
            report.phases.iter().partition(|p| p.on_chain_ns >= min_ns);
        let width = big.iter().map(|p| p.name.len()).max().unwrap_or(0);
        for p in &big {
            println!(
                "  {:>8}  {:<width$}  {}",
                fmt_ns(p.on_chain_ns),
                p.name,
                bar(p.on_chain_ns, chain)
            );
        }
        if !small.is_empty() {
            let ns: u64 = small.iter().map(|p| p.on_chain_ns).sum();
            println!("  {:>8}  {} other phases", fmt_ns(ns), small.len());
        }
    }

    println!();
    println!("The critical path by process");
    print_tree(&tree);

    println!();
    println!("What would shorten it");
    let rows: Vec<&Row> = report.rows.iter().collect();
    print_levers(&rows, chain, limit);

    println!();
    println!("Look closer");
    closer(
        &format!("rewind time {run} --in PID"),
        "one process: its threads, sleeps and waits",
    );
    closer(
        &format!("rewind time {run} --folded"),
        "the path for flamegraph.pl or speedscope.app",
    );
    closer(
        &format!("rewind time {run} --perfetto F"),
        "every process on a timeline, for ui.perfetto.dev",
    );
}

/// One process: its time on the path, the tree of what it started, what
/// would shorten it, and what outside it held it up.
fn print_inside(timing: &Timing, life: rewind_trace::Life, run: &str, limit: usize) {
    let inside = timing.inside(life);
    let tree = timing.path_tree(Some(life), None, Budget::Lines);
    let chain_ns = timing.chain.ns;
    println!("{}", clip(&inside.label, NAME_WIDTH + 16));
    println!(
        "  {:>8}  on the critical path with what it started, {} of the {} path",
        fmt_ns(tree.ns),
        share(tree.ns, chain_ns),
        fmt_ns(chain_ns)
    );
    if tree.ns == 0 {
        return;
    }
    println!("  {:>8}  {}", "", states_line(&tree.states, tree.ns));

    // Its tree, when it started anything on the path.
    if tree
        .roots
        .iter()
        .any(|r| !r.children.is_empty() || r.hidden > 0)
    {
        println!();
        println!("Its part of the critical path by process");
        print_tree(&tree);
    }

    // Its threads' computing and sleeps, and what it started, by how
    // much sooner the job would finish without them.
    let (waits, own): (Vec<&Row>, Vec<&Row>) =
        inside.rows.iter().partition(|r| r.without_ns.is_none());
    let started = timing.started(life);
    let mut levers: Vec<&Row> = own;
    levers.extend(
        started
            .iter()
            .filter(|r| !matches!(r.kind, RowKind::Tree { .. })),
    );
    levers.sort_by_key(|r| (std::cmp::Reverse(r.gain_ns(chain_ns)), r.label.clone()));
    println!();
    println!("What would shorten it");
    print_levers(&levers, chain_ns, limit);

    // What held it up from outside: another process, not one it started,
    // whose wake let it go on.
    let waits: Vec<&Row> = waits.into_iter().filter(|r| r.life.is_some()).collect();
    if !waits.is_empty() {
        println!();
        println!("What it waited for, outside it");
        for r in waits.iter().take(WAITS_SHOWN) {
            println!(
                "  {:>8}  {}",
                fmt_ns(r.took_ns),
                clip(&r.label, NAME_WIDTH + 16)
            );
        }
        if waits.len() > WAITS_SHOWN {
            println!("  {:>8}  and {} more", "", waits.len() - WAITS_SHOWN);
        }
    }

    // The line of code a lever's time starts at: where its thread fell
    // asleep, or where it is first on the path.
    println!();
    println!("Look closer");
    let first = levers
        .iter()
        .find(|r| r.step.is_some() && r.gain_ns(chain_ns) > 0);
    if let Some(r) = first
        && let Some(step) = r.step
    {
        let pid = r.life.map_or(life.pid, |l| l.pid);
        closer(
            &format!("rewind where {run} {step} --pid {pid}"),
            "the source line the first row's time starts at",
        );
    }
    if let Some(w) = waits.iter().find_map(|r| r.life) {
        closer(
            &format!("rewind time {run} --in {}", w.pid),
            "what it waited for longest",
        );
    }
    closer(
        &format!("rewind time {run} --perfetto F"),
        "every process on a timeline",
    );
}

/// A command to look closer with, and what it shows.
fn closer(command: &str, what: &str) {
    println!("  {command:<COMMAND_WIDTH$}  {what}");
}

/// The path by state, as `67% asleep (epoll_pwait 23.1 s, ...), 33% computing`.
fn states_line(states: &[(State, u64)], whole: u64) -> String {
    let computing: u64 = states
        .iter()
        .filter(|(s, _)| *s == State::Computing)
        .map(|(_, ns)| ns)
        .sum();
    let asleep = whole.saturating_sub(computing);
    let calls: Vec<String> = states
        .iter()
        .filter_map(|(s, ns)| match s {
            State::Asleep(call) => Some(format!("{call} {}", fmt_ns(*ns))),
            State::Computing => None,
        })
        .take(STATES_SHOWN)
        .collect();
    let asleep_part = if calls.is_empty() {
        format!("{} asleep", share(asleep, whole))
    } else {
        format!("{} asleep ({})", share(asleep, whole), calls.join(", "))
    };
    let computing_part = format!("{} computing", share(computing, whole));
    if asleep == 0 {
        return "all computing".to_string();
    }
    if computing == 0 {
        return asleep_part;
    }
    if computing >= asleep {
        return format!("{computing_part}, {asleep_part}");
    }
    format!("{asleep_part}, {computing_part}")
}

/// The path as a tree of processes, each with its time on the path.
fn print_tree(tree: &PathTree) {
    let min_ns = (tree.ns as f64 * TREE_SHARE) as u64;
    println!(
        "  {:>8}  {:>7}  process, with what it started",
        "on path", "pid"
    );
    for root in &tree.roots {
        print_node(root, "", None, min_ns);
    }
    if tree.kernel_ns >= min_ns && tree.kernel_ns > 0 {
        println!("  {:>8}  {:>7}  kernel threads", fmt_ns(tree.kernel_ns), "");
    }
}

/// Where a line sits among its siblings, for the tree's branches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sibling {
    Last,
    More,
}

/// One process of the tree and its children: `indent` is the branches
/// above it, `place` none for a root.
fn print_node(node: &PathNode, indent: &str, place: Option<Sibling>, min_ns: u64) {
    let branch = match place {
        None => "",
        Some(Sibling::Last) => "└─ ",
        Some(Sibling::More) => "├─ ",
    };
    let name = format!("{}{}", wrappers(&node.through), node.label);
    let room = NAME_WIDTH.saturating_sub(indent.chars().count());
    let own = own_line(node, min_ns);
    let own = if own.is_empty() {
        own
    } else {
        format!("  {own}")
    };
    println!(
        "  {:>8}  {:>7}  {indent}{branch}{}{own}",
        fmt_ns(node.total_ns),
        node.life.pid,
        clip(&name, room)
    );

    // Its children, and a line for those too small for their own.
    let below = match place {
        None => indent.to_string(),
        Some(Sibling::Last) => format!("{indent}   "),
        Some(Sibling::More) => format!("{indent}│  "),
    };
    let count = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let last = i + 1 == count && (node.hidden == 0 || node.hidden_ns < min_ns);
        let place = if last { Sibling::Last } else { Sibling::More };
        print_node(child, &below, Some(place), min_ns);
    }
    if node.hidden > 0 && node.hidden_ns >= min_ns {
        println!(
            "  {:>8}  {:>7}  {below}└─ {} more",
            fmt_ns(node.hidden_ns),
            "",
            node.hidden
        );
    }
}

/// The wrappers sharing a line, as `make › bash › `; of a long run of
/// them, the outermost and the innermost.
fn wrappers(through: &[String]) -> String {
    if through.len() > WRAPPERS_SHOWN {
        let (first, last) = (&through[0], &through[through.len() - 1]);
        return format!("{first} › … › {last} › ");
    }
    through.iter().map(|w| format!("{w} › ")).collect()
}

/// A process's own time on the path, by state, where it says more than
/// the line's time: `asleep in clock_nanosleep 2.00 s, computing 1.10 s`.
fn own_line(node: &PathNode, min_ns: u64) -> String {
    let own_ns: u64 = node.own.iter().map(|p| p.ns).sum();
    let leaf = node.children.is_empty() && node.hidden == 0;
    let computing_only = node.own.iter().all(|p| {
        p.state == State::Computing || (p.ns as f64) < own_ns as f64 * (1.0 - ALL_COMPUTING)
    });
    if leaf && computing_only {
        return String::new();
    }
    node.own
        .iter()
        .filter(|p| p.ns >= min_ns && p.ns > 0)
        .map(|p| format!("{} {}", p.state.name(), fmt_ns(p.ns)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rows by how much sooner the job would finish if each took no time:
/// one number each.
fn print_levers(rows: &[&Row], chain_ns: u64, limit: usize) {
    let min_ns = (chain_ns as f64 * LEVER_SHARE) as u64;
    let shown: Vec<&&Row> = rows
        .iter()
        .filter(|r| r.without_ns.is_some() && r.gain_ns(chain_ns) >= min_ns.max(1))
        .take(limit)
        .collect();
    if shown.is_empty() {
        println!("  nothing on its own would make it noticeably sooner");
        return;
    }
    println!("  {:>8}  {:>7}  if this took no time", "sooner", "pid");
    for r in shown {
        let pid = r.life.map_or_else(String::new, |l| l.pid.to_string());
        println!(
            "  {:>8}  {pid:>7}  {}",
            fmt_ns(r.gain_ns(chain_ns)),
            clip(&strip_pid(&r.label), NAME_WIDTH + 8)
        );
    }
}

/// A row's label without the `(pid N)` its pid column shows.
fn strip_pid(label: &str) -> String {
    match label.rfind(" (pid ") {
        Some(at) if label.ends_with(')') => label[..at].to_string(),
        _ => label.to_string(),
    }
}

/// The chain in order: a line per stretch of one process.
fn print_chain(timing: &Timing) {
    println!("{:>9}  {:>9}  {:>8}", "at", "took", "step");
    for s in &timing.stretches() {
        println!(
            "{:>9}  {:>9}  {:>8}  {}",
            fmt_ns(s.at_ns),
            fmt_ns(s.ns),
            s.step,
            clip(&s.label, NAME_WIDTH)
        );
    }
    println!("{:>9}  {:>9}", "", fmt_ns(timing.chain.ns));
}

/// A bar for `ns` of `whole`, up to [`BAR_WIDTH`] characters.
fn bar(ns: u64, whole: u64) -> String {
    if whole == 0 {
        return String::new();
    }
    let n = ((ns as f64 / whole as f64) * BAR_WIDTH as f64).round() as usize;
    "█".repeat(n.min(BAR_WIDTH))
}

/// A duration on the VM's clock, at a precision that suits it.
fn fmt_ns(ns: u64) -> String {
    const US: u64 = 1_000;
    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    match ns {
        n if n >= 10 * S => format!("{:.1} s", n as f64 / S as f64),
        n if n >= S => format!("{:.2} s", n as f64 / S as f64),
        n if n >= 10 * MS => format!("{:.0} ms", n as f64 / MS as f64),
        n if n >= MS => format!("{:.1} ms", n as f64 / MS as f64),
        n if n >= US => format!("{:.0} us", n as f64 / US as f64),
        n => format!("{n} ns"),
    }
}

/// `text` cut to `width` characters, with an ellipsis where it was cut.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    // The pieces of a line of output: durations at their precision, and
    // bars to scale.
    use super::*;

    #[test]
    fn durations_read_at_a_precision_that_suits_them() {
        assert_eq!(fmt_ns(16_600_000_000), "16.6 s");
        assert_eq!(fmt_ns(2_030_000_000), "2.03 s");
        assert_eq!(fmt_ns(36_000_000), "36 ms");
        assert_eq!(fmt_ns(1_500_000), "1.5 ms");
        assert_eq!(fmt_ns(45_000), "45 us");
    }

    #[test]
    fn a_bar_is_to_scale_and_never_longer_than_its_width() {
        assert_eq!(bar(50, 100).chars().count(), BAR_WIDTH / 2);
        assert_eq!(bar(200, 100).chars().count(), BAR_WIDTH);
        assert_eq!(bar(1, 0), "");
    }
}
