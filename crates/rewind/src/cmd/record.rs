//! The `rewind` subcommands that record runs: a command in a root
//! filesystem, a Nix build, a search for a schedule that changes how one
//! ends, and a fork of a run at a step.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rewind_core::run::{
    BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM, MAX_CORES, Start, default_epoch,
};
use rewind_core::{
    Echo, Execution, Guest, Home, Keyframes, Run, Source, Spec, TimeLimit, compare, image, nix,
};
use rewind_init::{Job, Output, Root};
use rewind_trace::compare::Comparison;

use crate::{
    RUN_HELP, RUN_LONG_HELP, STEP_HELP, STEP_LONG_HELP, Terminal, clear_status, json, locate, show,
    status,
};

/// The arguments of `rewind run`.
#[derive(clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub(crate) image: ImageArgs,
    /// Print the run as one JSON object on standard output once it
    /// ends, for programs, instead of its output and summary: its id,
    /// name, directory, how it ended and the outputs its job hashed.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) machine: MachineArgs,
}

pub fn run_command(home: &Home, args: RunArgs) -> Result<ExitCode> {
    let RunArgs {
        image,
        json,
        machine,
    } = args;
    let guest = Guest::from_env()?;
    let workload = Workload::Image(image);
    let run = run_workload(home, &guest, &workload, &machine, Report::of(json))?;
    if json {
        println!("{}", json::run(&run)?);
    }
    Ok(exit_status(&run))
}

/// The arguments of `rewind nix`.
#[derive(clap::Args)]
pub struct NixArgs {
    /// A .drv path or an installable such as `nixpkgs#hello`.
    pub(crate) installable: String,
    /// Also compare the outputs with this binary cache's builds of them,
    /// besides the substituters Nix is configured with.
    ///
    /// Repeatable.
    #[arg(long = "compare-with", value_name = "URL")]
    pub(crate) compare_with: Vec<String>,
    /// Compare the outputs with this machine's store only, asking no
    /// binary cache.
    #[arg(long)]
    pub(crate) no_compare: bool,
    /// Print the run as one JSON object on standard output once it
    /// ends, for programs, instead of its output and summary: as `run
    /// --json` does, with what each store and cache said of each output.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) machine: MachineArgs,
}

pub fn nix(home: &Home, args: NixArgs) -> Result<ExitCode> {
    let NixArgs {
        installable,
        compare_with,
        no_compare,
        json,
        machine,
    } = args;
    let guest = Guest::from_env()?;
    let workload = Workload::Nix(installable);
    let run = run_workload(home, &guest, &workload, &machine, Report::of(json))?;
    let lookup = if no_compare {
        compare::Lookup::Off
    } else {
        compare::Lookup::Configured {
            extra: compare_with,
        }
    };
    let compared = compare_outputs(&run, &lookup)?;
    if json {
        let mut value = json::run(&run)?;
        value["outputs"] = compared
            .outputs
            .iter()
            .map(|(path, hash, comparisons)| {
                serde_json::json!({
                    "path": path,
                    "hash": hash,
                    "comparisons": json::comparisons(comparisons),
                })
            })
            .collect();
        value["not_asked"] = compared.skipped.clone().into();
        println!("{value}");
    } else {
        print_compared(&compared);
    }
    if !compared.missing.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
    Ok(exit_status(&run))
}

/// The arguments of `rewind check`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// A .drv or installable; leave it out and give --root and a
    /// command to check a command instead.
    pub(crate) installable: Option<String>,
    #[command(flatten)]
    pub(crate) image: ImageArgs,
    /// How many perturbed schedules to try besides the unperturbed one.
    #[arg(long, default_value_t = 64)]
    pub(crate) schedules: u64,
    /// Try every schedule and report how many end differently, instead
    /// of stopping at the first.
    #[arg(long)]
    pub(crate) all: bool,
    /// How many machines to run at once; one per CPU by default.
    #[arg(long, short)]
    pub(crate) jobs: Option<usize>,
    /// Print one JSON object on standard output once the search ends,
    /// for programs: each schedule tried, as `rewind ls --json` and
    /// `fork --json` describe runs, and the window, the two runs and
    /// where they part when one ended differently.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) machine: MachineArgs,
}

pub fn check(home: &Home, args: CheckArgs) -> Result<ExitCode> {
    let CheckArgs {
        installable,
        image,
        schedules,
        all,
        jobs,
        json,
        mut machine,
    } = args;
    // Lines as the search goes, unless it ends in one JSON object.
    let say = |line: String| {
        if !json {
            println!("{line}");
        }
    };
    let guest = Guest::from_env()?;
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .max(1);
    // Exploring compares runs; only the unperturbed run, which every
    // other run starts from, and the two reported get keyframes.
    machine.no_keyframes = true;
    machine.quiet = true;
    // Every run in a search boots with the same wall clock, even one
    // that crosses midnight.
    machine.epoch = Some(machine.epoch.unwrap_or_else(default_epoch));
    let workload = match installable {
        Some(i) => Workload::Nix(i),
        None => Workload::Image(image),
    };
    let prepared = prepare(home, &guest, &workload, &mut machine)?;

    // The unperturbed run, and the step its job started on:
    // everything before that is boot, and perturbing it would only
    // make every run differ from the first kernel thread on. The
    // window the user gave narrows that further.
    let (user_from, user_until) = (machine.schedule_from, machine.schedule_until);
    machine.schedule = 0;
    machine.schedule_from = 0;
    let with_keyframes = MachineArgs {
        no_keyframes: false,
        progress: Progress::Shown,
        ..machine.clone()
    };
    let base = execute(
        home,
        &guest,
        &prepared,
        &with_keyframes,
        Start::Boot,
        Announce::No,
    )?;

    // Every other run is the unperturbed run until its schedule
    // starts, so it starts at the unperturbed run's latest keyframe
    // before then instead of at boot: a narrowed window late in a
    // long build runs only from near the window.
    let from_base = Start::After(base.manifest.id.clone());
    say(format!("schedule   0: {}", show::outcome_line(&base)?));
    if !json {
        print_timeout(home, 0, &base);
    }
    let mut tried_runs = vec![schedule_json(&base)?];

    // A schedule can make a program loop forever where schedule 0
    // did not, so unless told otherwise each other run gets a
    // generous multiple of schedule 0's time and then ends as
    // timed-out, instead of holding up the whole search.
    if machine.timeout.is_none() {
        let base_wall = base.manifest.outcome.as_ref().map_or(0, |o| o.wall_ms);
        let limit = Duration::from_millis(base_wall * CHECK_TIMEOUT_FACTOR).max(CHECK_TIMEOUT_MIN);
        machine.timeout = Some(limit.as_secs());
    }
    let base_trace = base.trace()?;
    let start = show::start_step(base_trace).max(user_from);
    let base_key = show::outcome_key(&base)?;
    // When the unperturbed run is the one that fails, the schedules
    // that end differently are the ones that pass. A job that exits
    // 0 without creating every output fails, as under nix-daemon.
    let base_missing = show::missing_outputs(&base.manifest.spec.job.outputs, &base_key.outputs);
    let base_failed = !base.manifest.outcome.as_ref().is_some_and(|o| {
        rewind_trace::ending::Ending::of(&o.stop, base_key.status, &base_missing).passed()
    });
    let differs = |run: &Run| -> Result<bool> { Ok(show::outcome_key(run)? != base_key) };

    // Perturbed schedules, a machine per job at a time, in order.
    let mut failing: Option<Run> = None;
    let mut differing = 0;
    let mut tried = 0;
    let seeds: Vec<u64> = (1..=schedules).collect();
    for batch in seeds.chunks(jobs) {
        let machines = batch
            .iter()
            .map(|&seed| MachineArgs {
                schedule: seed,
                schedule_from: start,
                schedule_until: user_until,
                ..machine.clone()
            })
            .collect();
        status(&format!(
            "rewind: schedules {}..{} executing",
            batch[0],
            batch[batch.len() - 1]
        ));
        let runs = execute_all(home, &guest, &prepared, machines, from_base.clone());
        clear_status();
        for run in runs? {
            say(format!(
                "schedule {:>3}: {}",
                run.manifest.spec.schedule,
                show::outcome_line(&run)?
            ));
            tried_runs.push(schedule_json(&run)?);
            tried += 1;
            if differs(&run)? {
                differing += 1;
                if failing.is_none() {
                    failing = Some(run);
                }
            }
        }
        if failing.is_some() && !all {
            break;
        }
    }
    if all && base_failed {
        say(format!(
            "schedule 0 failed; {differing} of {tried} perturbed schedules ended differently"
        ));
    } else if all {
        say(format!(
            "{differing} of {tried} perturbed schedules ended differently"
        ));
    }
    let search = |narrowed: serde_json::Value| {
        serde_json::json!({
            "schedules": tried_runs,
            "tried": tried,
            "differing": differing,
            "schedule_0_failed": base_failed,
            "narrowed": narrowed,
        })
    };
    let Some(worst) = failing else {
        say(format!("same result under all {} schedules", tried + 1));
        if json {
            println!("{}", search(serde_json::Value::Null));
        }
        return Ok(ExitCode::SUCCESS);
    };

    // Narrow it to the smallest window of steps its perturbation
    // still ends differently from (see rewind_core::check).
    machine.schedule = worst.manifest.spec.schedule;
    let end = worst
        .manifest
        .outcome
        .as_ref()
        .map_or(start, |o| o.step)
        .min(user_until);
    if base_failed {
        say(format!(
            "\nschedule {} passes where schedule 0 fails; narrowing the steps it perturbs",
            machine.schedule
        ));
    } else {
        say(format!(
            "\nschedule {} ends differently; narrowing the steps it perturbs",
            machine.schedule
        ));
    }
    if !json {
        print_timeout(home, machine.schedule, &worst);
    }
    let probe_all = |windows: Vec<(u64, u64)>| -> Result<Vec<Run>> {
        let lo = windows.iter().map(|w| w.0).min().unwrap_or(0);
        let hi = windows.iter().map(|w| w.1).max().unwrap_or(0);
        status(&format!(
            "rewind: narrowing, {} windows within steps {lo}..{hi}",
            windows.len()
        ));
        let machines = windows
            .into_iter()
            .map(|(from, until)| MachineArgs {
                schedule_from: from,
                schedule_until: until,
                ..machine.clone()
            })
            .collect();
        execute_all(home, &guest, &prepared, machines, from_base.clone())
    };
    let narrowed = rewind_core::check::narrow((start, end), jobs, worst, probe_all, differs)?;
    let (lo, until, worst) = (narrowed.from, narrowed.until, narrowed.run);
    clear_status();
    say(format!(
        "perturbing only steps {lo}..{until} still ends differently\n"
    ));
    let base = base.add_keyframes(home)?;
    let worst = worst.add_keyframes(home)?;
    let (passing, failing) = if base_failed {
        (worst, base)
    } else {
        (base, worst)
    };
    // Where the failing run first behaves differently from the
    // passing one, as the app shows it.
    let (pt, ft) = (passing.trace()?, failing.trace()?);
    let comparison = Comparison::of(ft, failing.last_step()?, pt, passing.last_step()?);
    if json {
        let narrowed = serde_json::json!({
            "schedule": machine.schedule,
            "from": lo,
            "until": until,
            "passing": json::run(&passing)?,
            "failing": json::run(&failing)?,
            "program": comparison.program,
            "divergence": json::comparison(ft, pt, &comparison),
        });
        println!("{}", search(narrowed));
        return Ok(ExitCode::FAILURE);
    }
    println!("passing: run {}", passing.manifest.id);
    println!("failing: run {}", failing.manifest.id);

    // Where they part, in words, and the failing run's step there,
    // where the app opens it beside the passing one.
    println!();
    print!(
        "{}",
        show::comparison((ft, "failing"), (pt, "passing"), &comparison)
    );
    let open = open_line(
        &failing.manifest.id,
        comparison.step(),
        Some(&passing.manifest.id),
    );
    println!("\nopen both in the desktop app: {open}");
    Ok(ExitCode::FAILURE)
}

/// The arguments of `rewind fork`.
#[derive(clap::Args)]
pub struct ForkArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// The schedule seed for the new branch.
    ///
    /// As with a run's --schedule, values programs draw from the kernel's
    /// randomness after the step, such as load addresses, can differ from
    /// the parent's; a run recorded with `--kernel-args norandmaps` loads
    /// programs at fixed addresses.
    #[arg(long, default_value_t = 1)]
    pub(crate) schedule: u64,
    /// Print nothing while the fork executes.
    #[arg(long, short)]
    pub(crate) quiet: bool,
    /// Print the result as one JSON object on standard output, for
    /// programs such as the desktop app.
    #[arg(long)]
    pub(crate) json: bool,
    /// Stop the fork after this many seconds on this machine, however
    /// far it got, as `--timeout` does for a run.
    #[arg(long, value_name = "SECONDS")]
    pub(crate) timeout: Option<u64>,
}

pub fn fork(home: &Home, args: ForkArgs) -> Result<ExitCode> {
    let ForkArgs {
        run,
        step,
        schedule,
        quiet,
        json,
        timeout,
    } = args;
    let parent = Run::find(home, &run)?;
    let step = parent.check_step(step)?;
    let m = &parent.manifest;
    if schedule == 0 {
        bail!("schedule 0 is the unperturbed run; a fork needs another seed");
    }
    let spec = m.spec.fork(step, schedule);
    let name = format!(
        "{} (fork of {} at {step}, schedule {schedule})",
        m.name, m.id
    );
    let echo = if quiet || json {
        Echo::Quiet
    } else {
        Echo::Output
    };
    let how = Execution {
        echo,
        keyframes: Keyframes::Take,
        limit: time_limit(timeout),
    };
    let child = Run::execute(
        home,
        name,
        m.source.clone(),
        spec,
        Start::Fork {
            parent: m.id.clone(),
            step,
        },
        how,
    )?;
    let first_difference = child.manifest.first_difference;
    if json {
        println!("{}", json::run(&child)?);
        return Ok(exit_status(&child));
    }
    let stop = locate::describe_stall(home, &child);
    eprintln!("{}", show::finished(&child, stop.as_deref()));
    match first_difference {
        None => eprintln!("rewind: the fork ran the same as its parent"),
        Some(step) => {
            eprintln!("rewind: the fork first differs from its parent at step {step}")
        }
    }
    let open = open_line(&child.manifest.id, first_difference, Some(&m.id));
    eprintln!("rewind: open it beside its parent in the desktop app: {open}");
    Ok(exit_status(&child))
}

/// The clock a run will use: an explicit choice, or for `auto` the branch
/// counter when this boot's self-test found it exact, with a warning when
/// it did not.
fn resolve_clock(home: &Home, guest: &Guest, arg: ClockArg) -> Result<rewind_vmm::ClockSource> {
    use rewind_core::pmu::{self, Vendor};
    use rewind_vmm::ClockSource;
    let vendor = Vendor::detect()?;
    let branches = || {
        vendor
            .event()
            .map(ClockSource::Branches)
            .context("this CPU has no branch counter rewind knows")
    };
    match arg {
        ClockArg::Exits => Ok(ClockSource::Exits),
        ClockArg::Branches => branches(),
        ClockArg::Auto => {
            if vendor.event().is_some() && pmu::counter_usable_this_boot(home, guest, &vendor)? {
                return branches();
            }
            eprintln!("rewind: {}", pmu::exit_time_warning(&vendor));
            Ok(ClockSource::Exits)
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ClockArg {
    Auto,
    Exits,
    Branches,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CpuArg {
    V3,
    Host,
}

impl From<CpuArg> for rewind_vmm::CpuModel {
    fn from(c: CpuArg) -> Self {
        match c {
            CpuArg::V3 => rewind_vmm::CpuModel::V3,
            CpuArg::Host => rewind_vmm::CpuModel::Host,
        }
    }
}

/// The machine options every way of starting a run shares.
#[derive(clap::Args, Clone)]
pub(crate) struct MachineArgs {
    /// Seeds the VM's randomness.
    ///
    /// Runs with the same inputs and seed are identical; a different seed
    /// explores a different run.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Asks the VM to reschedule at steps this seed picks, to explore other
    /// thread interleavings.
    ///
    /// The inputs, --seed and --epoch stay as they are, but values programs
    /// draw from the kernel's randomness, such as ephemeral port numbers and
    /// where programs are loaded, can differ: the schedule decides which
    /// process draws first. `--kernel-args norandmaps` turns address
    /// randomization off, to tell an interleaving apart from a layout change.
    /// 0 is the unperturbed schedule.
    #[arg(long, default_value_t = 0)]
    schedule: u64,
    /// The first step a reschedule may be asked at; before it the run is the
    /// unperturbed one.
    #[arg(long, default_value_t = 0)]
    schedule_from: u64,
    /// The CPU the VM sees: a fixed x86-64-v3 model that replays on any
    /// machine supporting it, or this machine's own features.
    #[arg(long, value_enum, default_value_t = CpuArg::V3)]
    cpu: CpuArg,
    /// What moves the VM's clock besides exits: the work done inside it,
    /// counted by this machine's branch counter (`branches`), or nothing
    /// (`exits`).
    ///
    /// `auto` uses the counter when `rewind pmu status` finds it exact.
    #[arg(long, value_enum, default_value_t = ClockArg::Auto)]
    clock: ClockArg,
    /// Experimental: with counter time, also interrupt a VM computing without
    /// exits at the timer's branch count.
    ///
    /// See docs/pmu.md.
    #[arg(long, hide = true)]
    experimental_preempt: bool,
    /// The clock `clock` resolved to, once per command.
    #[arg(skip)]
    resolved_clock: Option<rewind_vmm::ClockSource>,
    /// The step after which no more reschedules are asked; by default none
    /// is.
    #[arg(long, default_value_t = u64::MAX, hide_default_value = true)]
    schedule_until: u64,
    /// The VM's memory in MiB.
    #[arg(long, default_value_t = DEFAULT_MEM_MIB)]
    mem: u64,
    /// The CPUs programs in the VM are told it has, and for a Nix build its
    /// NIX_BUILD_CORES, which stdenv passes to make, ninja and test runners
    /// as their job count.
    ///
    /// The VM still has one vCPU: the threads and jobs sized by the count
    /// interleave on it, so schedules can reorder them.
    #[arg(long, default_value_t = DEFAULT_CORES,
          value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_CORES)))]
    cores: u32,
    /// The VM's wall clock at boot, in seconds since the Unix epoch.
    ///
    /// Defaults to the start of today, UTC.
    #[arg(long)]
    epoch: Option<u64>,
    /// A name to find the run by later.
    #[arg(long)]
    name: Option<String>,
    /// Print nothing while the run executes.
    #[arg(long, short)]
    quiet: bool,
    /// Whether a quiet run shows a status line on a terminal; rewind check
    /// asks for one on the run every other starts from.
    #[arg(skip)]
    progress: Progress,
    /// Skip keyframes: faster, but seeking into the run starts from boot.
    #[arg(long)]
    no_keyframes: bool,
    /// Stop the run after this many seconds on this machine, however far it
    /// got.
    ///
    /// It then ends as timed-out, which depends on how fast this machine is.
    /// Unless this is set, rewind check gives each perturbed schedule ten
    /// times as long as schedule 0 took, and at least a minute.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
    /// Extra kernel command line arguments; `loglevel=7` shows the kernel's
    /// messages in the trace.
    #[arg(long, default_value = "")]
    kernel_args: String,
}

/// A command in a root filesystem, for `run` and `check`.
#[derive(clap::Args, Clone)]
pub(crate) struct ImageArgs {
    /// The root filesystem: a directory, an erofs image, or a tarball
    /// such as `docker export` writes.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Environment variables for the command, as KEY=VALUE.
    #[arg(long = "env", short = 'e')]
    env: Vec<String>,
    /// The working directory inside the VM.
    #[arg(long, default_value = "/")]
    cwd: String,
    /// Give the command a terminal for its output, as `docker run -t` does.
    ///
    /// A program whose output is a terminal writes it a line at a time and
    /// may color it; without one, it buffers its output and writes it in
    /// blocks, as under `docker run` without -t. A Nix build always gets
    /// one, as nix-daemon gives a builder.
    #[arg(long, short = 't')]
    tty: bool,
    /// The command and its arguments.
    #[arg(last = true)]
    argv: Vec<String>,
}

/// What a run runs.
enum Workload {
    Nix(String),
    Image(ImageArgs),
}

/// The VM's memory in MiB unless --mem says otherwise.
pub(crate) const DEFAULT_MEM_MIB: u64 = 1024;

/// The PATH a command in a root filesystem gets unless --env sets one.
pub(crate) const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Whether a run echoes its program's output while it executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loudness {
    Output,
    Quiet,
}

/// Whether a quiet run shows a status line while it executes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Progress {
    #[default]
    Hidden,
    Shown,
}

/// What a run prints while it executes.
fn echo_for(loudness: Loudness, progress: Progress, terminal: Terminal) -> Echo {
    match (loudness, progress, terminal) {
        (Loudness::Output, _, _) => Echo::Output,
        (Loudness::Quiet, Progress::Shown, Terminal::Yes) => Echo::Progress,
        (Loudness::Quiet, _, _) => Echo::Quiet,
    }
}

/// The command that opens `run` in the desktop app at `step`, beside
/// `compare`, as `rewind check` and `rewind fork` print it.
fn open_line(run: &str, step: Option<u64>, compare: Option<&str>) -> String {
    let mut line = format!("rewind open {run}");
    if let Some(step) = step {
        line.push_str(&format!(" {step}"));
    }
    if let Some(compare) = compare {
        line.push_str(&format!(" --compare {compare}"));
    }
    line
}

/// A workload resolved to what a run needs: done once, then run under as
/// many machine options as a search wants.
struct Prepared {
    name: String,
    source: Source,
    image: Option<PathBuf>,
    image_hash: Option<String>,
    job: Job,
}

/// How `run` and `nix` report a run: its output as it executes and a
/// summary when it ends, or nothing until one JSON object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    Text,
    Json,
}

impl Report {
    fn of(json: bool) -> Report {
        if json { Report::Json } else { Report::Text }
    }
}

/// Runs a workload with the given machine options, reported as `report`
/// says.
fn run_workload(
    home: &Home,
    guest: &Guest,
    workload: &Workload,
    machine: &MachineArgs,
    report: Report,
) -> Result<Run> {
    let mut machine = machine.clone();
    let prepared = prepare(home, guest, workload, &mut machine)?;
    let announce = match report {
        Report::Text => Announce::Yes,
        Report::Json => {
            machine.quiet = true;
            Announce::No
        }
    };
    execute(home, guest, &prepared, &machine, Start::Boot, announce)
}

fn prepare(
    home: &Home,
    guest: &Guest,
    workload: &Workload,
    machine: &mut MachineArgs,
) -> Result<Prepared> {
    // KVM first: packing a Nix closure into an image can take minutes, and
    // a machine without KVM would only say so after them.
    rewind_vmm::kvm::open()?;
    if machine.resolved_clock.is_none() {
        machine.resolved_clock = Some(resolve_clock(home, guest, machine.clock)?);
    }
    let (name, source, image, job) = match workload {
        Workload::Nix(installable) => prepare_nix(home, installable, machine.cores)?,
        Workload::Image(args) => prepare_image(home, args)?,
    };
    let image_hash = match &image {
        Some(path) => Some(image::hash_file(path)?),
        None => None,
    };
    Ok(Prepared {
        name: machine.name.clone().unwrap_or(name),
        source,
        image,
        image_hash,
        job,
    })
}

/// A command in a root filesystem.
fn prepare_image(home: &Home, args: &ImageArgs) -> Result<(String, Source, Option<PathBuf>, Job)> {
    let root = args
        .root
        .as_ref()
        .context("give --root with the root filesystem to run in")?;
    if args.argv.is_empty() {
        bail!("give the command to run after --");
    }
    let image = root_image(home, root)?;
    let job = Job {
        program: None,
        argv: args.argv.clone(),
        env: parse_env(&args.env)?,
        cwd: args.cwd.clone(),
        uid: 0,
        gid: 0,
        hostname: "localhost".into(),
        root: Root::Image,
        files: Vec::new(),
        outputs: Vec::new(),
        output: if args.tty {
            Output::Terminal
        } else {
            Output::Plain
        },
    };
    let source = Source::Image {
        root: root.display().to_string(),
    };
    Ok((args.argv.join(" "), source, Some(image), job))
}

/// A derivation's builder, with its input closure as the image.
fn prepare_nix(
    home: &Home,
    installable: &str,
    cores: u32,
) -> Result<(String, Source, Option<PathBuf>, Job)> {
    let drv_path = nix::resolve(installable)?;
    let drv = nix::show(&drv_path)?;
    nix::runnable(&drv)?;
    let closure = nix::input_closure(&drv)?;

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
        let tmp = image::temp_beside(&image);
        image::from_store_paths(&closure, &tmp)?;
        image::place(&tmp, &image)?;
    }

    let graphs = nix::reference_graphs(&drv, &closure)?;
    let job = nix::job(&drv, &graphs, cores)?;
    let source = Source::Nix {
        drv: drv_path.display().to_string(),
        outputs: drv
            .outputs
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
    };
    Ok((drv.name.clone(), source, Some(image), job))
}

/// Whether a run prints its one-line summary when it ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Announce {
    Yes,
    No,
}

fn execute(
    home: &Home,
    guest: &Guest,
    prepared: &Prepared,
    machine: &MachineArgs,
    start: Start,
    announce: Announce,
) -> Result<Run> {
    let spec = Spec {
        kernel: guest.kernel.clone(),
        initrd: guest.initrd.clone(),
        kernel_debug: guest.kernel_debug.clone(),
        image: prepared.image.clone(),
        image_hash: prepared.image_hash.clone(),
        mem_mib: machine.mem,
        cores: machine.cores,
        seed: machine.seed,
        epoch: machine.epoch.unwrap_or_else(default_epoch),
        quantum: DEFAULT_QUANTUM,
        schedule: machine.schedule,
        schedule_from: machine.schedule_from,
        schedule_until: machine.schedule_until,
        inherited_schedules: Vec::new(),
        cpu: machine.cpu.into(),
        clock: machine.resolved_clock.unwrap_or_default(),
        preemption: if machine.experimental_preempt {
            rewind_vmm::Preemption::AtBranchCounts
        } else {
            rewind_vmm::Preemption::AtExits
        },
        extras: rewind_vmm::Extras::Reserved,
        cmdline: format!("{BASE_CMDLINE} {}", machine.kernel_args)
            .trim()
            .to_string(),
        job: prepared.job.clone(),
    };
    let loudness = if machine.quiet {
        Loudness::Quiet
    } else {
        Loudness::Output
    };
    let echo = echo_for(loudness, machine.progress, Terminal::stderr());
    let keyframes = if machine.no_keyframes {
        Keyframes::Skip
    } else {
        Keyframes::Take
    };
    let how = Execution {
        echo,
        keyframes,
        limit: time_limit(machine.timeout),
    };
    let run = Run::execute(
        home,
        prepared.name.clone(),
        prepared.source.clone(),
        spec,
        start,
        how,
    )?;
    if announce == Announce::Yes {
        let stop = locate::describe_stall(home, &run);
        eprintln!("{}", show::finished(&run, stop.as_deref()));
    }
    Ok(run)
}

/// Runs each machine option set on its own VM, `jobs` at a time, and
/// returns the runs in the order given.
fn execute_all(
    home: &Home,
    guest: &Guest,
    prepared: &Prepared,
    machines: Vec<MachineArgs>,
    start: Start,
) -> Result<Vec<Run>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .into_iter()
            .map(|m| {
                let start = start.clone();
                scope.spawn(move || execute(home, guest, prepared, &m, start, Announce::No))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a run's thread panicked"))
            .collect()
    })
}

/// A run `rewind check` tried, for its JSON: the run, and its schedule.
fn schedule_json(run: &Run) -> Result<serde_json::Value> {
    let mut value = json::run(run)?;
    value["schedule"] = run.manifest.spec.schedule.into();
    Ok(value)
}

/// For a schedule of `rewind check` that hit its time limit, how and where:
/// a guest stuck computing without exits is often what the search found.
/// A user-space address is named by the program's symbols when it can be.
fn print_timeout(home: &Home, schedule: u64, run: &Run) {
    let Some(o) = run.manifest.outcome.as_ref() else {
        return;
    };
    if o.stop.timeout().is_none() {
        return;
    }
    let stop = locate::describe_stall(home, run).unwrap_or_else(|| o.stop.to_string());
    println!("schedule {schedule}: {stop}");
}

/// The time limit `--timeout` sets, in seconds, if given.
fn time_limit(timeout: Option<u64>) -> TimeLimit {
    match timeout {
        Some(secs) => TimeLimit::Wall(Duration::from_secs(secs)),
        None => TimeLimit::None,
    }
}

/// How many times schedule 0's wall-clock time `rewind check` gives each
/// other schedule, unless `--timeout` says otherwise.
const CHECK_TIMEOUT_FACTOR: u64 = 10;

/// The least time `rewind check` gives each schedule after schedule 0.
const CHECK_TIMEOUT_MIN: Duration = Duration::from_secs(60);

/// What this machine's store and the binary caches said about a Nix run's
/// outputs.
struct Compared {
    /// Each output's path and hash from the guest, and what each source
    /// said about its build of it.
    outputs: Vec<(String, String, Vec<compare::Comparison>)>,
    /// Substituters that are not HTTP binary caches, which were not asked.
    skipped: Vec<String>,
    /// For a job that exited 0, the outputs it did not create: it failed,
    /// as nix-daemon judges it.
    missing: Vec<String>,
}

/// Asks this machine's store and the binary caches `lookup` names about
/// each output the run's job hashed.
fn compare_outputs(run: &Run, lookup: &compare::Lookup) -> Result<Compared> {
    let show::OutcomeKey { status, outputs } = show::outcome_key(run)?;

    // The caches to ask; without Nix's configuration, only the store.
    let caches = match compare::caches(lookup) {
        Ok(caches) => caches,
        Err(e) => {
            eprintln!("rewind: comparing with your store only: {e:#}");
            compare::Caches::default()
        }
    };
    let netrc = if caches.http.is_empty() {
        compare::Netrc::default()
    } else {
        compare::Netrc::load()
    };
    let results = compare::compare(&outputs, &caches, &netrc);
    let missing = if status == Some(0) {
        show::missing_outputs(&run.manifest.spec.job.outputs, &outputs)
    } else {
        Vec::new()
    };
    Ok(Compared {
        outputs: outputs
            .into_iter()
            .zip(results)
            .map(|((path, hash), result)| (path, hash, result.comparisons))
            .collect(),
        skipped: caches.skipped,
        missing,
    })
}

/// Prints each output's hash from the guest and what each source said
/// about it, then the outputs a job that exited 0 did not create.
fn print_compared(compared: &Compared) {
    for (path, hash, comparisons) in &compared.outputs {
        println!("{}", show::verdict_line(path, hash, comparisons));
    }
    let differs = compared
        .outputs
        .iter()
        .flat_map(|(_, _, comparisons)| comparisons)
        .any(|c| c.verdict == compare::Verdict::Differs);
    if differs {
        println!("{}", show::DIFFERS_NOTE);
    }
    if !compared.outputs.is_empty() && !compared.skipped.is_empty() {
        println!(
            "rewind: did not ask {}, which are not HTTP binary caches",
            compared.skipped.join(", ")
        );
    }
    for path in &compared.missing {
        println!("{path} missing: the builder exited 0 without creating it");
    }
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

    let tmp = image::temp_beside(&home.images().join("root.erofs"));
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
    image::place(&tmp, &path)?;
    Ok(path)
}

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut env = vec![("PATH".to_string(), DEFAULT_PATH.to_string())];
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
        Some(s) => ExitCode::from(rewind_trace::ending::ExitStatus::from_wait(s).shell_code()),
        None => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    // How a run echoes, from its flags and where its output goes.
    use super::*;

    #[test]
    fn a_quiet_run_shows_a_status_line_only_when_asked_on_a_terminal() {
        // A run that is not quiet echoes its output wherever it goes. A
        // quiet one draws a status line only when asked to and standard
        // error is a terminal, and is silent otherwise, as in a pipe or a
        // log.
        assert_eq!(
            echo_for(Loudness::Output, Progress::Hidden, Terminal::Yes),
            Echo::Output
        );
        assert_eq!(
            echo_for(Loudness::Quiet, Progress::Shown, Terminal::Yes),
            Echo::Progress
        );
        assert_eq!(
            echo_for(Loudness::Quiet, Progress::Shown, Terminal::No),
            Echo::Quiet
        );
        assert_eq!(
            echo_for(Loudness::Quiet, Progress::Hidden, Terminal::Yes),
            Echo::Quiet
        );
    }
}
