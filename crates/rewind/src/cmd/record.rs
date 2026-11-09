//! The `rewind` subcommands that record runs: a command in a root
//! filesystem, a Nix build, a search for a schedule that changes how one
//! ends, and a fork of a run at a step. A build nix-daemon described is
//! recorded here too, for `rewind build` (see [`super::build`]).

use std::path::{Path, PathBuf};
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
use rewind_trace::located::Located;
use rewind_trace::schedule::{Reseed, StallMax};

use crate::color::{self, Paint, Same, Style};
use crate::property::{self, Coverage, Judge, Property, Violation};
use crate::{
    RUN_HELP, RUN_LONG_HELP, STEP_HELP, STEP_LONG_HELP, Terminal, clear_status, json, locate, show,
    status,
};

/// The arguments of `rewind run`.
#[derive(clap::Args)]
pub(crate) struct RunArgs {
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

pub(crate) fn run_command(home: &Home, args: RunArgs) -> Result<ExitCode> {
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
pub(crate) struct NixArgs {
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

pub(crate) fn nix(home: &Home, args: NixArgs) -> Result<ExitCode> {
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
pub(crate) struct CheckArgs {
    /// A .drv or installable; leave it out and give --root and a
    /// command to check a command instead.
    pub(crate) installable: Option<String>,
    #[command(flatten)]
    pub(crate) image: ImageArgs,
    /// Check a run recorded already instead of recording a job: each
    /// schedule is a fork of the run at --schedule-from, as `rewind
    /// fork` makes one, and the run stands for the unperturbed schedule.
    ///
    /// The run's own inputs and machine stand, so the options that
    /// would make another run are refused with it.
    #[arg(long, value_name = "RUN", help = RUN_HELP, conflicts_with_all = RUN_FIXES)]
    pub(crate) run: Option<String>,
    /// How many perturbed schedules to try besides the unperturbed one.
    #[arg(long, default_value_t = 64)]
    pub(crate) schedules: u64,
    /// How many seeds of the VM's randomness to try, from --seed on. Each
    /// seed runs unperturbed and under every perturbed schedule, so the
    /// search makes seeds times (schedules + 1) runs.
    ///
    /// The seed decides everything programs draw from the kernel's
    /// randomness: hash map order, which branch a select tries first,
    /// temporary names, and the inputs a property-testing library
    /// generates when it seeds itself from the system. A seed whose
    /// unperturbed run already differs from the first seed's is not
    /// perturbed further. With --run, the run's own seed comes first, and
    /// each other one reseeds the run's RNG at --schedule-from.
    #[arg(long, default_value_t = 1)]
    pub(crate) seeds: u64,
    /// How many directory orders to try besides the stock one: the first
    /// seed's unperturbed run again under each key from 1 to N, as
    /// `--kernel-args rewind.dirents=KEY` sets one, so the search makes N
    /// more runs, each from boot.
    ///
    /// A key lists every directory in another order, as another ext4 disk
    /// would. Each key's run is judged against schedule 0 as a seed's is,
    /// and one that ends differently is reported without narrowing, since
    /// its directories list differently from boot. No key's run is
    /// perturbed, and with --seeds only the first seed runs under the
    /// keys. Refused beside a --kernel-args that sets rewind.dirents.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) dirents: u64,
    /// What a correct run is. The search looks for a schedule that
    /// violates a property schedule 0 holds, or that holds every property
    /// where schedule 0 violates one, and narrows it to the step that
    /// decides it. Repeat for several.
    ///
    /// same-outcome: the run ends as schedule 0 did, with the same exit
    /// status and outputs; the default. exit-zero: the job exits 0 having
    /// made every output. no-crash: no process of the job died of a
    /// signal that dumps core, such as SIGSEGV, even one the job's exit
    /// status hides. no-orphans: every process the job started had exited
    /// when its main process did. single-writer=GLOB: no two of the job's
    /// processes had a matching file open for writing at once, from its
    /// open until its close.
    /// atomic-replace=GLOB: no matching file the run had written was
    /// truncated, or removed and written again, in place, where a reader
    /// can see it missing or half written.
    /// no-read-while-written=GLOB: no process opened a matching file for
    /// reading while another had it open for writing.
    /// writes-within=DIR:DIR: the job wrote, renamed and removed files
    /// only under these directories, besides /dev, /proc and /sys.
    ///
    /// sdk: what the job asserted with the Rewind SDK held, and every
    /// test it marked that passed under schedule 0 passed. On by itself
    /// when the job writes SDK marks. junit=GLOB: every test that passed
    /// under schedule 0 passed, by the JUnit XML reports the job wrote to
    /// files matching GLOB, which check reads at the job's exit.
    ///
    /// script=PATH: a program on this machine judges each run, exiting 0
    /// when the property holds and 1 when it is violated, with the reason
    /// on its first line, which may start with `step N:`. It finds the
    /// run in $REWIND_RUN, schedule 0 in $REWIND_BASE and the step the
    /// job exited at in $REWIND_END, and can read the run with `rewind
    /// cat`, `rewind events` and `rewind log`. The runs of one batch are
    /// judged at once, so a script runs beside other copies of itself.
    #[arg(long = "property", value_name = "PROPERTY")]
    pub(crate) properties: Vec<Property>,
    /// A file of properties, one a line, as --property takes them; lines
    /// starting with # are comments, and a script's path is taken from
    /// the file's directory. Adds to --property.
    #[arg(long = "properties", value_name = "FILE")]
    pub(crate) properties_file: Option<PathBuf>,
    /// Try every schedule and report how many end differently, instead
    /// of stopping at the first.
    #[arg(long)]
    pub(crate) all: bool,
    /// Stop once the schedules are tried, without narrowing the first
    /// that ends differently to the step that decides it. With --all,
    /// only the count.
    #[arg(long)]
    pub(crate) no_narrow: bool,
    /// How many machines to run at once; one per CPU by default.
    #[arg(long, short)]
    pub(crate) jobs: Option<usize>,
    /// Print one JSON object on standard output once the search ends,
    /// for programs: each schedule tried, as `rewind ls --json` and
    /// `fork --json` describe runs, and the window, the two runs and
    /// where they part when one ended differently.
    #[arg(long)]
    pub(crate) json: bool,
    /// When a schedule ends differently, also show where its threads
    /// were, as `rewind where` finds them: the line of the program's own
    /// code the thread on the CPU at the deciding step was on, and the one
    /// the thread of the failing run's first differing event was on.
    ///
    /// Each takes a fork and gdb, a few seconds, and the first time for a
    /// build gdb may wait for its debug info to download.
    #[arg(long = "where")]
    pub(crate) show_where: bool,
    #[command(flatten)]
    pub(crate) machine: MachineArgs,
}

/// The options of `rewind check` that say what job to record and how,
/// which a run recorded already has fixed: refused beside --run.
const RUN_FIXES: [&str; 18] = [
    "installable",
    "root",
    "env",
    "cwd",
    "tty",
    "argv",
    "seed",
    "schedule",
    "cpu",
    "clock",
    "experimental_preempt",
    "mem",
    "cores",
    "epoch",
    "name",
    "no_keyframes",
    "kernel_args",
    "dirents",
];

/// The guest kernel's argument that keys the order every directory lists
/// in.
const DIRENTS_ARG: &str = "rewind.dirents";

/// The directory order key that leaves each filesystem its own order.
const STOCK_DIRENTS: u64 = 0;

/// Whether kernel arguments set the directory order key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirentsArg {
    Set,
    Unset,
}

/// Whether `kernel_args` sets the directory order key, as `rewind.dirents=N`.
fn dirents_arg(kernel_args: &str) -> DirentsArg {
    let set = kernel_args.split_whitespace().any(|arg| {
        arg.split_once('=')
            .is_some_and(|(name, _)| name == DIRENTS_ARG)
    });
    if set {
        return DirentsArg::Set;
    }
    DirentsArg::Unset
}

/// `kernel_args` with the directory order key `key` added.
fn with_dirents(kernel_args: &str, key: u64) -> String {
    format!("{kernel_args} {DIRENTS_ARG}={key}")
        .trim()
        .to_string()
}

/// The directory order key a run's kernel command line sets, the last
/// one given as the kernel keeps it, or the stock order's when it sets
/// none.
fn dirents_of(cmdline: &str) -> u64 {
    cmdline
        .split_whitespace()
        .rev()
        .filter_map(|arg| arg.split_once('='))
        .find(|(name, _)| *name == DIRENTS_ARG)
        .and_then(|(_, key)| key.parse().ok())
        .unwrap_or(STOCK_DIRENTS)
}

/// What `rewind check` perturbs, and so how it makes each run it tries.
enum Subject<'a> {
    /// A job it records: each schedule is a run of the job, started at
    /// the unperturbed run's latest keyframe before the schedule's window.
    Job {
        guest: &'a Guest,
        prepared: &'a Prepared,
        start: Start,
    },
    /// A run recorded already: each schedule is a fork of it at the
    /// first step of the schedule's window, its RNG reseeded at the
    /// search's first step when the seed is another than the run's.
    Run {
        run: Box<Run>,
        reseed: Option<Reseed>,
    },
}

/// One run `rewind check` tries: a schedule seed over the steps
/// `from..until`.
#[derive(Clone, Copy, Debug)]
struct Window {
    seed: u64,
    from: u64,
    until: u64,
}

impl Subject<'_> {
    /// Runs each window on its own VM, all at once, and returns the runs
    /// in the order given. `machine` holds the time limit, and for a job
    /// every other option of its runs.
    fn try_all(&self, home: &Home, machine: &MachineArgs, windows: &[Window]) -> Result<Vec<Run>> {
        match self {
            // A job's runs, recorded as `rewind run` and `nix` record them.
            Subject::Job {
                guest,
                prepared,
                start,
            } => {
                let machines = windows
                    .iter()
                    .map(|w| MachineArgs {
                        schedule: w.seed,
                        schedule_from: w.from,
                        schedule_until: w.until,
                        ..machine.clone()
                    })
                    .collect();
                execute_all(home, guest, prepared, machines, start.clone())
            }

            // A run's forks, each on a thread of its own.
            Subject::Run { run, reseed } => std::thread::scope(|scope| {
                let handles: Vec<_> = windows
                    .iter()
                    .map(|w| {
                        scope.spawn(move || fork_window(home, run, machine, *w, reseed.as_ref()))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("a fork's thread panicked"))
                    .collect()
            }),
        }
    }
}

/// Forks `run` at the first step of `window`, perturbed by its seed until
/// its last, and its RNG reseeded as `reseed` says, with only the
/// keyframes `machine` asks of a run that skips them, as `rewind check`
/// tries it.
fn fork_window(
    home: &Home,
    run: &Run,
    machine: &MachineArgs,
    window: Window,
    reseed: Option<&Reseed>,
) -> Result<Run> {
    let Window { seed, from, until } = window;
    let m = &run.manifest;
    let mut spec = m.spec.fork(from, seed).with_stall_max(machine.stall_max);
    spec.schedule_until = until;
    let mut name = format!("{} (fork of {} at {from}, schedule {seed}", m.name, m.id);
    if until != u64::MAX {
        name.push_str(&format!(" until {until}"));
    }
    name.push_str(&stall_max_words(spec.stall_max));
    if let Some(r) = reseed {
        spec = spec.reseeded(r.step, r.seed);
        name.push_str(&format!(", seed {} from {}", r.seed, r.step));
    }
    name.push(')');
    let how = Execution {
        echo: Echo::Quiet,
        keyframes: keyframes_for(machine),
        limit: time_limit(machine.timeout),
    };
    Run::execute(
        home,
        name,
        m.source.clone(),
        spec,
        Start::Fork {
            parent: m.id.clone(),
            step: from,
        },
        how,
    )
}

pub(crate) fn check(home: &Home, args: CheckArgs) -> Result<ExitCode> {
    let CheckArgs {
        installable,
        image,
        run,
        schedules,
        seeds,
        dirents,
        mut properties,
        properties_file,
        all,
        no_narrow,
        jobs,
        json,
        show_where,
        mut machine,
    } = args;
    // Lines as the search goes: on standard output, or with --json on
    // standard error, which a program reads as progress.
    let say = |line: String| {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    let paint = if json {
        color::stderr()
    } else {
        color::stdout()
    };
    if seeds == 0 {
        bail!("--seeds needs at least one seed");
    }

    // The keys --dirents tries replace the one key a run would otherwise
    // get from --kernel-args, so the two together are refused.
    if dirents > 0 && dirents_arg(&machine.kernel_args) == DirentsArg::Set {
        bail!(
            "--kernel-args sets {DIRENTS_ARG} already; give the keys to try with --dirents or the one key with --kernel-args"
        );
    }

    if let Some(file) = &properties_file {
        properties.extend(Property::read_file(file)?);
    }
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .max(1);
    // Exploring compares runs; only the unperturbed run, which every
    // other run starts from, and the two reported get keyframes.
    machine.no_keyframes = true;
    machine.quiet = true;
    let (user_from, user_until) = (machine.schedule_from, machine.schedule_until);
    let varies = Varies { seeds, dirents };
    let label = |seed: u64, key: u64, schedule: u64| run_label(varies, seed, key, schedule);
    let name = |seed: u64, key: u64, schedule: u64| run_name(varies, seed, key, schedule);

    // The unperturbed run, and how every other run is made. A recorded
    // run is the unperturbed run, and each schedule forks it.
    let guest;
    let prepared;
    let (base, subject, inputs) = match run {
        Some(what) => {
            let base = Run::find(home, &what)?;
            if base.manifest.outcome.is_none() {
                bail!("run {} has not finished", base.manifest.id);
            }
            if user_from > 0 {
                base.check_step(user_from)?;
            }
            if seeds > 1 {
                say(format!(
                    "{} {seeds} random seeds, the run's own and {} new ones from the search's first step, each run under schedule 0 and {schedules} other schedules",
                    paint.prefix(),
                    seeds - 1
                ));
            }
            let subject = Subject::Run {
                run: Box::new(Run::open(&base.dir)?),
                reseed: None,
            };
            (base, subject, None)
        }
        None => {
            guest = Guest::from_env()?;
            // Every run in a search boots with the same wall clock, even
            // one that crosses midnight.
            machine.epoch = Some(machine.epoch.unwrap_or_else(default_epoch));
            let workload = match installable {
                Some(i) => Workload::Nix(i),
                None => Workload::Image(image),
            };
            prepared = prepare(home, &guest, &workload, &mut machine)?;
            machine.schedule = 0;
            machine.schedule_from = 0;
            if seeds > 1 {
                let plural = if schedules == 1 { "" } else { "s" };
                let keys = if dirents > 0 {
                    let key_plural = if dirents == 1 { "" } else { "s" };
                    format!(", and the first under {dirents} other directory order{key_plural}")
                } else {
                    String::new()
                };
                say(format!(
                    "{} {seeds} random seeds, each run under schedule 0 and {schedules} other schedule{plural}{keys}: {} runs",
                    paint.prefix(),
                    seeds * (schedules + 1) + dirents
                ));
            }
            let base = execute(
                home,
                &guest,
                &prepared,
                &with_keyframes(&machine),
                Start::Boot,
                Announce::No,
            )?;

            // Every other run is the unperturbed run until its schedule
            // starts, so it starts at the unperturbed run's latest keyframe
            // before then instead of at boot: a narrowed window late in a
            // long build runs only from near the window.
            let subject = Subject::Job {
                guest: &guest,
                prepared: &prepared,
                start: Start::After(base.manifest.id.clone()),
            };
            (base, subject, Some((&guest, &prepared)))
        }
    };
    // The first seed is the unperturbed run's own: --seed for a job, and
    // whatever a recorded run was recorded with.
    let first_seed = base.manifest.spec.seed;
    let base_words = match &subject {
        Subject::Run { .. } => format!("run {}", base.manifest.id),
        Subject::Job { .. } => name(first_seed, STOCK_DIRENTS, 0),
    };

    // A job that asserts with the Rewind SDK is judged by its assertions
    // and tests too, besides what was asked for or the default.
    if !properties.contains(&Property::Sdk) && property::writes_sdk_marks(&base)? {
        if properties.is_empty() {
            properties.push(Property::SameOutcome);
        }
        properties.push(Property::Sdk);
        say(format!(
            "{} the job asserts with the Rewind SDK, so each run is judged by property sdk too",
            paint.prefix()
        ));
    }

    // What each run is judged by, and schedule 0's verdict, which the
    // search looks for a run to differ from.
    let judge = Judge::new(home, &base, properties)?;
    let mut coverage = Coverage::default();
    coverage.add(&base)?;
    let base_violations = judge.violations(&base)?;
    say(format!(
        "{}: {}{}",
        label(first_seed, STOCK_DIRENTS, 0),
        show::outcome_line(&base, judge.base_key(), paint)?,
        verdict_words(&judge, &base_violations, paint)
    ));
    print_timeout(home, 0, &base, say);
    print_violations(&judge, &base_violations, say);
    let base_key = judge.base_key().clone();
    let mut tried_runs = vec![schedule_json(&base, false, &base_violations)?];

    // A schedule can make a program loop forever where schedule 0
    // did not, so unless told otherwise each other run gets a
    // generous multiple of schedule 0's time and then ends as
    // timed-out, instead of holding up the whole search.
    if machine.timeout.is_none() {
        let base_wall = base.manifest.outcome.as_ref().map_or(0, |o| o.wall_ms);
        let limit = Duration::from_millis(base_wall * CHECK_TIMEOUT_FACTOR).max(CHECK_TIMEOUT_MIN);
        machine.timeout = Some(limit.as_secs());
    }

    // When the unperturbed run is the one that fails, the schedules
    // that end differently are the ones that pass. A job that exits
    // 0 without creating every output fails, as under nix-daemon.
    let base_missing = show::missing_outputs(&base.manifest.spec.job.outputs, &base_key.outputs);
    let base_failed = !base.manifest.outcome.as_ref().is_some_and(|o| {
        rewind_trace::ending::Ending::of(&o.stop, base_key.status, &base_missing).passed()
    });

    // A run the search is after is one whose verdict differs from
    // schedule 0's. With the default property, schedule 0 always holds it
    // and the words stay those of a run that ends differently; with
    // properties given, schedule 0 is the bad run when it violates one.
    let base_holds = base_violations.is_empty();
    let same_outcome_only = judge.same_outcome_only();
    let base_bad = if same_outcome_only {
        base_failed
    } else {
        !base_holds
    };
    let differs_by = |violations: &[Violation]| violations.is_empty() != base_holds;

    // A property that reads a run's files where its job exited forks the
    // run there, so each other run takes a keyframe there, which the
    // fork restores instead of replaying the run from boot.
    if judge.reads_files_at_exit() {
        machine.exit_keyframe = ExitKeyframe::Take;
    }

    // Each seed's unperturbed run and how its perturbed schedules are
    // made, the first seed's being the unperturbed run itself.
    let first = Seeded::new(base, subject, machine.clone(), user_from)?;
    let mut seeded = vec![first];
    let mut found: Option<Found> = None;
    let mut differing = 0;
    let mut tried = 0;

    // The first seed's unperturbed run under each other directory order,
    // from boot, a machine per job at a time. A key is in the run from
    // boot, as a seed is, so a key's run that ends differently is not
    // perturbed or narrowed.
    let keys: Vec<u64> = (1..=dirents).collect();
    for batch in keys.chunks(jobs) {
        if found.is_some() && !all {
            break;
        }
        let Some((guest, prepared)) = inputs else {
            bail!("--dirents needs a job to record");
        };
        let machines = batch
            .iter()
            .map(|&key| MachineArgs {
                kernel_args: with_dirents(&machine.kernel_args, key),
                ..machine.clone()
            })
            .collect();
        status(&format!(
            "{} dirents {}..{} executing",
            color::stderr().prefix(),
            batch[0],
            batch[batch.len() - 1]
        ));
        let runs = execute_all(home, guest, prepared, machines, Start::Boot);
        let judged = runs.and_then(|runs| judge_all(&judge, runs));
        clear_status();
        for (&key, (run, violations)) in batch.iter().zip(judged?) {
            let differs = differs_by(&violations);
            say(format!(
                "{}: {}{}",
                label(first_seed, key, 0),
                show::outcome_line(&run, judge.base_key(), paint)?,
                verdict_words(&judge, &violations, paint)
            ));
            tried_runs.push(schedule_json(&run, differs, &violations)?);
            coverage.add(&run)?;
            tried += 1;
            if !differs {
                continue;
            }
            differing += 1;
            if found.is_none() {
                found = Some(Found::Key(Box::new(run)));
            }
        }
    }

    // Each seed's unperturbed run and its perturbed schedules, unless a
    // key already found a run that ends differently.
    for i in 0..seeds {
        if found.is_some() && !all {
            break;
        }
        let seed = first_seed + i;

        // Another seed's unperturbed run, from boot, judged as any other
        // run: a seed alone can decide how a run ends.
        if i > 0 {
            let seed_machine = MachineArgs {
                seed,
                ..machine.clone()
            };
            status(&format!(
                "{} seed {seed} executing",
                color::stderr().prefix()
            ));
            let (seed_base, subject) = match (&seeded[0].subject, inputs) {
                // A recorded run: a fork of it at the search's first step,
                // its RNG reseeded there, and its schedules forks of the
                // run reseeded the same way.
                (Subject::Run { run, .. }, _) => {
                    let reseed = Reseed {
                        step: seeded[0].start,
                        seed,
                    };
                    let window = Window {
                        seed: 0,
                        from: reseed.step,
                        until: u64::MAX,
                    };
                    let seed_base = fork_window(home, run, &machine, window, Some(&reseed));
                    let subject = Subject::Run {
                        run: Box::new(Run::open(&run.dir)?),
                        reseed: Some(reseed),
                    };
                    (seed_base, subject)
                }

                // A job: its run from boot with the seed, which the seed's
                // schedules start from.
                (Subject::Job { .. }, Some((guest, prepared))) => {
                    let started = if schedules > 0 {
                        with_keyframes(&seed_machine)
                    } else {
                        seed_machine.clone()
                    };
                    let seed_base =
                        execute(home, guest, prepared, &started, Start::Boot, Announce::No);
                    let subject = match &seed_base {
                        Ok(b) => Subject::Job {
                            guest,
                            prepared,
                            start: Start::After(b.manifest.id.clone()),
                        },
                        Err(_) => Subject::Job {
                            guest,
                            prepared,
                            start: Start::Boot,
                        },
                    };
                    (seed_base, subject)
                }
                (Subject::Job { .. }, None) => bail!("a job's search has no job to record"),
            };
            clear_status();
            let seed_base = seed_base?;
            let violations = judge.violations(&seed_base)?;
            let differs = differs_by(&violations);
            say(format!(
                "{}: {}{}",
                label(seed, STOCK_DIRENTS, 0),
                show::outcome_line(&seed_base, judge.base_key(), paint)?,
                verdict_words(&judge, &violations, paint)
            ));
            tried_runs.push(schedule_json(&seed_base, differs, &violations)?);
            coverage.add(&seed_base)?;
            tried += 1;
            seeded.push(Seeded::new(seed_base, subject, seed_machine, user_from)?);
            if differs {
                differing += 1;
                if found.is_none() {
                    found = Some(Found::Seed(i as usize));
                }
                if !all {
                    break;
                }
                continue;
            }
        }

        // Perturbed schedules, a machine per job at a time, in order.
        let s = &seeded[i as usize];
        let numbers: Vec<u64> = (1..=schedules).collect();
        for batch in numbers.chunks(jobs) {
            let windows: Vec<Window> = batch
                .iter()
                .map(|&schedule| Window {
                    seed: schedule,
                    from: s.start,
                    until: user_until,
                })
                .collect();
            let seed_words = if seeds > 1 {
                format!("seed {seed}, ")
            } else {
                String::new()
            };
            status(&format!(
                "{} {seed_words}schedules {}..{} executing",
                color::stderr().prefix(),
                batch[0],
                batch[batch.len() - 1]
            ));
            let runs = s.subject.try_all(home, &s.machine, &windows);
            let judged = runs.and_then(|runs| judge_all(&judge, runs));
            clear_status();
            for (run, violations) in judged? {
                let differs = differs_by(&violations);
                say(format!(
                    "{}: {}{}",
                    label(seed, STOCK_DIRENTS, run.manifest.spec.schedule),
                    show::outcome_line(&run, judge.base_key(), paint)?,
                    verdict_words(&judge, &violations, paint)
                ));
                tried_runs.push(schedule_json(&run, differs, &violations)?);
                coverage.add(&run)?;
                tried += 1;
                if differs {
                    differing += 1;
                    if found.is_none() {
                        found = Some(Found::Schedule(i as usize, Box::new(run)));
                    }
                }
            }
            if found.is_some() && !all {
                break;
            }
        }
    }

    // A search that varies an input besides the schedule counts runs, and
    // one that varies only the schedule counts schedules.
    let other_inputs = seeds > 1 || dirents > 0;
    let tried_words = if other_inputs {
        "other runs"
    } else {
        "other schedules"
    };
    // The tally reads green when every run agreed with schedule 0 and
    // red when some did not, or schedule 0 itself failed.
    let agreed = Same::from(differing == 0 && !base_bad);
    if all && same_outcome_only && base_failed {
        say(paint.verdict(
            agreed,
            format!("{base_words} failed; {differing} of {tried} {tried_words} ended differently"),
        ));
    } else if all && same_outcome_only {
        say(paint.verdict(
            agreed,
            format!("{differing} of {tried} {tried_words} ended differently"),
        ));
    } else if all && base_holds {
        say(paint.verdict(
            agreed,
            format!("{differing} of {tried} {tried_words} violated a property"),
        ));
    } else if all {
        say(paint.verdict(
            agreed,
            format!(
                "{base_words} violated a property; {differing} of {tried} {tried_words} held every one"
            ),
        ));
    }
    // The `sometimes` conditions some run made true: the search reached
    // what they describe, as often as this.
    for (id, reach) in coverage.reached() {
        say(paint.verdict(
            Same::Yes,
            format!(
                "true in {} of {} runs that checked it: sometimes \"{id}\"",
                reach.hits, reach.runs
            ),
        ));
    }

    // The `sometimes` conditions no run made true: the search never
    // reached what they describe, so a clean result says less.
    let never = coverage.never();
    if !never.is_empty() {
        say(paint.paint(
            Style::Warn,
            format!(
                "never true in any run: sometimes \"{}\"",
                never.join("\", \"")
            ),
        ));
    }
    let search = |narrowed: serde_json::Value| {
        serde_json::json!({
            "properties": judge.properties().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "sometimes_never": never,
            "sometimes_true": coverage.reached().iter().map(|(id, reach)| serde_json::json!({
                "id": id,
                "runs": reach.runs,
                "true": reach.hits,
            })).collect::<Vec<_>>(),
            "seeds": seeds,
            "dirents": dirents,
            "schedules": tried_runs,
            "tried": tried,
            "differing": differing,
            "schedule_0_failed": base_failed,
            "narrowed": narrowed,
        })
    };
    let (one_run, all_runs) = if other_inputs {
        ("run", "runs")
    } else {
        ("schedule", "schedules")
    };
    let Some(found) = found else {
        // Every run tried, or the one when only the unperturbed run was.
        let every = if tried == 0 {
            format!("the one {one_run} tried")
        } else {
            format!("all {} {all_runs}", tried + 1)
        };
        if same_outcome_only {
            say(paint.verdict(Same::Yes, format!("same result under {every}")));
        } else if base_holds {
            say(paint.verdict(Same::Yes, format!("every property held under {every}")));
        } else {
            if !all {
                say(paint.verdict(
                    Same::No,
                    format!("every {one_run} violated a property, as {base_words} did"),
                ));
            }
            // Schedule 0's violation is the bug, at the step it names.
            if let Some(step) = base_violations.iter().find_map(|v| v.step)
                && !json
            {
                let open = open_line(&seeded[0].base.manifest.id, Some(step), None);
                println!("open it at the step: {open}");
            }
        }
        if json {
            println!("{}", search(serde_json::Value::Null));
        }
        if !base_holds {
            return Ok(ExitCode::FAILURE);
        }
        return Ok(ExitCode::SUCCESS);
    };
    // The runs on both sides of the difference must each replay exactly
    // here before the search says what decides it. One that does not
    // differs for a reason no schedule, seed or directory order holds:
    // something it read came from the host. A search where every run
    // agreed never gets here, so only one that found a difference pays
    // for the two replays, a small part of what narrowing it costs.
    let (found_words, found_run, base_side) = match &found {
        Found::Seed(i) => (name(seeded[*i].seed, STOCK_DIRENTS, 0), &seeded[*i].base, 0),
        Found::Key(run) => (
            format!("dirents {}", dirents_of(&run.manifest.spec.cmdline)),
            &**run,
            0,
        ),
        Found::Schedule(i, worst) => (
            name(seeded[*i].seed, STOCK_DIRENTS, worst.manifest.spec.schedule),
            &**worst,
            *i,
        ),
    };
    let base_side_words = if seeded[base_side].seed == first_seed {
        base_words.clone()
    } else {
        name(seeded[base_side].seed, STOCK_DIRENTS, 0)
    };
    status(&format!(
        "{} replaying {base_side_words} and {found_words}, each of which must replay exactly",
        color::stderr().prefix()
    ));
    let replayed = replays_exactly(&[
        (base_side_words.as_str(), &seeded[base_side].base),
        (found_words.as_str(), found_run),
    ]);
    clear_status();
    replayed?;

    if no_narrow {
        if json {
            println!("{}", search(serde_json::Value::Null));
        }
        return Ok(ExitCode::FAILURE);
    }

    // What the run found violates, or the unperturbed run does when the
    // run found is the one that holds.
    let violated = |found_run: &Run| -> Result<String> {
        if base_holds {
            Ok(violated_names(&judge.violations(found_run)?))
        } else {
            Ok(violated_names(&base_violations))
        }
    };
    let makes = |violated: &str| match (same_outcome_only, base_bad) {
        (true, true) => "pass".to_string(),
        (true, false) => "fail".to_string(),
        (false, true) => "hold every property".to_string(),
        (false, false) => format!("violate {violated}"),
    };

    // The two runs to report, the step that decides it when a schedule
    // does, and the window narrowed to.
    let (worst, partner, found_seed, decided) = match found {
        // A seed: its unperturbed run against the first seed's, with no
        // step to narrow to, since the seed is in the run from boot.
        Found::Seed(i) => {
            let chosen = seeded.swap_remove(i);
            let first = seeded.swap_remove(0);
            let violated = violated(&chosen.base)?;
            let (good, bad) = if base_bad {
                (chosen.seed, first.seed)
            } else {
                (first.seed, chosen.seed)
            };
            let words = match (same_outcome_only, base_bad) {
                (true, _) => format!("seed {good} passes and seed {bad} fails"),
                (false, _) => {
                    format!("seed {good} holds every property and seed {bad} violates {violated}")
                }
            };
            say(format!(
                "\nseed {} decides it: under schedule 0, {words}\n",
                chosen.seed
            ));
            let seed = chosen.seed;
            (chosen.base, first.base, seed, None)
        }

        // A directory order key: its run against the first seed's
        // unperturbed run, as a seed's is, since the key too is in the
        // run from boot.
        Found::Key(chosen) => {
            let chosen = *chosen;
            let first = seeded.swap_remove(0);
            let key = dirents_of(&chosen.manifest.spec.cmdline);
            let violated = violated(&chosen)?;
            let (good, bad) = if base_bad {
                (key, STOCK_DIRENTS)
            } else {
                (STOCK_DIRENTS, key)
            };
            let words = match (same_outcome_only, base_bad) {
                (true, _) => format!("dirents {good} passes and dirents {bad} fails"),
                (false, _) => format!(
                    "dirents {good} holds every property and dirents {bad} violates {violated}"
                ),
            };
            say(format!(
                "\ndirents {key} decides it: under schedule 0, {words}\n"
            ));
            (chosen, first.base, first.seed, None)
        }

        // A schedule: narrowed within its seed to a short window of steps
        // its perturbation still decides it in (see rewind_core::check).
        Found::Schedule(i, worst) => {
            let worst = *worst;
            let s = seeded.swap_remove(i);
            let schedule = worst.manifest.spec.schedule;
            let found_name = name(s.seed, STOCK_DIRENTS, schedule);
            let end = worst
                .manifest
                .outcome
                .as_ref()
                .map_or(s.start, |o| o.step)
                .min(user_until);
            let violated = violated(&worst)?;
            let seed_words = if s.seed == first_seed {
                base_words.clone()
            } else {
                name(s.seed, STOCK_DIRENTS, 0)
            };
            if same_outcome_only && base_failed {
                say(format!(
                    "\n{found_name} passes where {seed_words} fails; finding the step that decides it"
                ));
            } else if same_outcome_only {
                say(format!(
                    "\n{found_name} ends differently from {seed_words}; finding the step that decides it"
                ));
            } else if base_holds {
                say(format!(
                    "\n{found_name} violates {violated}; finding the step that decides it"
                ));
            } else {
                say(format!(
                    "\n{found_name} holds every property where {seed_words} violates {violated}; finding the step that decides it"
                ));
            }
            print_timeout(home, schedule, &worst, say);
            let probe_all = |windows: Vec<(u64, u64)>| -> Result<Vec<Judged>> {
                let lo = windows.iter().map(|w| w.0).min().unwrap_or(0);
                let hi = windows.iter().map(|w| w.1).max().unwrap_or(0);
                status(&format!(
                    "{} narrowing, {} windows within steps {lo}..{hi}",
                    color::stderr().prefix(),
                    windows.len()
                ));
                let windows: Vec<Window> = windows
                    .into_iter()
                    .map(|(from, until)| Window {
                        seed: schedule,
                        from,
                        until,
                    })
                    .collect();
                let runs = s.subject.try_all(home, &s.machine, &windows)?;
                Ok(judge_all(&judge, runs)?
                    .into_iter()
                    .map(|(run, violations)| Judged {
                        differs: differs_by(&violations),
                        run,
                    })
                    .collect())
            };
            let whole = Judged {
                run: worst,
                differs: true,
            };
            let narrowed = rewind_core::check::narrow(
                (s.start, end),
                jobs,
                whole,
                probe_all,
                |judged: &Judged| Ok(judged.differs),
            )?;
            let (lo, until, worst) = (narrowed.from, narrowed.until, narrowed.run.run);
            clear_status();
            if same_outcome_only {
                say(format!(
                    "{found_name} limited to steps {lo}..{until} still ends differently"
                ));
            } else {
                say(format!(
                    "{found_name} limited to steps {lo}..{until} still makes the run {}",
                    makes(&violated)
                ));
            }
            // The window's last step decides how the run ends. The window
            // one step shorter ends like the seed's unperturbed run and is
            // the same run until that step, so it is the run to compare
            // with; a window of one step leaves that run itself.
            let partner = narrowed.without_last.map_or(s.base, |judged| judged.run);
            (
                worst,
                partner,
                s.seed,
                Some((schedule, lo, until, violated)),
            )
        }
    };

    let (worst, partner) = std::thread::scope(|scope| {
        let worst = scope.spawn(|| worst.add_keyframes(home));
        let partner = partner.add_keyframes(home);
        (worst.join().expect("taking keyframes panicked"), partner)
    });
    let (worst, partner) = (worst?, partner?);
    let found_key = dirents_of(&worst.manifest.spec.cmdline);

    // Whether a late timer is part of what the step does depends on
    // whether its exit armed one, which only running it shows.
    let deciding = match &decided {
        Some((_, _, until, violated)) => {
            let deciding = until - 1;
            let timer = worst.armed_timer_at(home, deciding)?;
            let words = worst.manifest.spec.schedule().words_at(deciding, timer);
            say(format!(
                "step {deciding} decides it: {words} there makes the run {}\n",
                makes(violated)
            ));
            Some(deciding)
        }
        None => None,
    };
    let (passing, failing) = if base_bad {
        (worst, partner)
    } else {
        (partner, worst)
    };
    // What the failing run violates, and where, when properties were
    // given.
    let failing_violations = if same_outcome_only {
        Vec::new()
    } else {
        judge.violations(&failing)?
    };
    // Where the failing run first behaves differently from the
    // passing one, as the app shows it.
    let (pt, ft) = (passing.trace()?, failing.trace()?);
    let comparison = Comparison::of(ft, failing.last_step()?, pt, passing.last_step()?);

    // Where the threads involved were, when asked: the one on the CPU at
    // the deciding step, and the one of the failing run's first event that
    // differs. A lookup that fails says why in its place.
    let threads: Vec<(locate::Involved, u64, Result<Located>)> = if show_where {
        let first = comparison
            .point
            .as_ref()
            .and_then(|p| p.here)
            .and_then(|side| ft.events.get(side.index));
        let mut asked = Vec::new();
        if let Some(deciding) = deciding {
            asked.push((locate::Involved::AtTheDecidingStep, deciding, (None, None)));
        }
        if let Some(event) = first {
            asked.push((
                locate::Involved::AtTheFirstDifference,
                event.step,
                (Some(event.pid), Some(event.tid)),
            ));
        }
        asked
            .into_iter()
            .map(|(involved, step, thread)| {
                (
                    involved,
                    step,
                    locate::located(home, &failing, step, thread),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    if json {
        // Where the threads were, for a person reading the log beside the
        // JSON.
        for line in where_lines(&threads) {
            eprintln!("{line}");
        }
        let (schedule, from, until) = match &decided {
            Some((schedule, lo, until, _)) => (*schedule, Some(*lo), Some(*until)),
            None => (0, None, None),
        };
        let narrowed = serde_json::json!({
            "seed": found_seed,
            "dirents": found_key,
            "schedule": schedule,
            "from": from,
            "until": until,
            "deciding_step": deciding,
            "passing": json::run(&passing)?,
            "failing": json::run(&failing)?,
            "violations": violations_json(&failing_violations),
            "program": comparison.program,
            "divergence": json::comparison(ft, pt, &comparison),
            "threads": threads.iter().map(|(involved, step, answer)| thread_json(*involved, *step, answer)).collect::<Vec<_>>(),
        });
        println!("{}", search(narrowed));
        return Ok(ExitCode::FAILURE);
    }
    let words = |run: &Run| {
        let spec = &run.manifest.spec;
        let mut words = Vec::new();
        if seeds > 1 {
            words.push(show::seed_words(spec));
        }
        if dirents > 0 {
            words.push(format!("dirents {}", dirents_of(&spec.cmdline)));
        }
        words.push(show::schedule_words(spec));
        words.join(", ")
    };
    // The failing run is amber and the passing one blue, as in the
    // comparison below and the app's Compare view.
    let paint = color::stdout();
    println!(
        "{}: run {}, {}",
        paint.paint(Style::Right, PASSING),
        passing.manifest.id,
        words(&passing)
    );
    println!(
        "{}: run {}, {}",
        paint.paint(Style::Left, FAILING),
        failing.manifest.id,
        words(&failing)
    );
    if let Some(deciding) = deciding {
        println!("the two are the same run until step {deciding}");
    }
    for v in &failing_violations {
        println!(
            "failing run {} {}: {v}",
            paint.verdict(Same::No, "violates"),
            v.property
        );
    }

    // Where they part, in words, and the failing run's step there,
    // where the app opens it beside the passing one.
    println!();
    print!(
        "{}",
        show::comparison((ft, FAILING), (pt, PASSING), &comparison, paint)
    );
    for line in where_lines(&threads) {
        println!("{line}");
    }
    let open = open_line(
        &failing.manifest.id,
        comparison.step(),
        Some(&passing.manifest.id),
    );
    println!("\nopen both in the desktop app: {open}");
    Ok(ExitCode::FAILURE)
}

/// Replays each of `runs`, named by the words a search calls it, from boot
/// on a thread each, and fails on the first that does not make its
/// recording again, naming it and saying why: another build recorded it,
/// or it is not deterministic on this host. Either way no schedule, seed
/// or directory order decides how it ends.
fn replays_exactly(runs: &[(&str, &Run)]) -> Result<()> {
    let replays: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = runs
            .iter()
            .map(|(_, run)| scope.spawn(move || run.replay()))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("replaying a run panicked"))
            .collect()
    });
    for ((words, run), replayed) in runs.iter().zip(replays) {
        let Some(d) = replayed? else {
            continue;
        };
        let why = rewind_core::run::WentAnotherWay::new(
            &run.manifest.id,
            d.left_step.min(d.right_step),
            &run.manifest.recorded_by,
        );
        return Err(anyhow::Error::new(why).context(format!(
            "no schedule decides this, since {words} does not replay exactly"
        )));
    }
    Ok(())
}

/// One seed of a search: the seed, its unperturbed run, how its perturbed
/// schedules are made, the machine they run on, and the step they start
/// perturbing at.
struct Seeded<'a> {
    seed: u64,
    base: Run,
    subject: Subject<'a>,
    machine: MachineArgs,
    start: u64,
}

impl<'a> Seeded<'a> {
    /// A seed whose unperturbed run is `base`. Perturbing starts at the
    /// step the job started on, since everything before that is boot and
    /// perturbing it would only make every run differ from the first
    /// kernel thread on, or later when the user said so.
    fn new(
        base: Run,
        subject: Subject<'a>,
        machine: MachineArgs,
        user_from: u64,
    ) -> Result<Seeded<'a>> {
        let start = show::start_step(base.trace()?).max(user_from);
        Ok(Seeded {
            seed: machine.seed,
            base,
            subject,
            machine,
            start,
        })
    }
}

/// A run a search tried, and whether its verdict differs from the
/// unperturbed run's.
struct Judged {
    run: Run,
    differs: bool,
}

/// What `judge` finds each of `runs` violates, judging them all at once on
/// a thread each, since a property that looks inside a run forks it.
/// The runs come back in the order given.
fn judge_all(judge: &Judge, runs: Vec<Run>) -> Result<Vec<(Run, Vec<Violation>)>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = runs
            .into_iter()
            .map(|run| {
                scope.spawn(move || {
                    let violations = judge.violations(&run);
                    (run, violations)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                let (run, violations) = h.join().expect("judging a run panicked");
                Ok((run, violations?))
            })
            .collect()
    })
}

/// What a search found first: a seed whose unperturbed run differs, by
/// its index among the seeds, a directory order key whose run does, or a
/// perturbed schedule of a seed that does.
enum Found {
    Seed(usize),
    Key(Box<Run>),
    Schedule(usize, Box<Run>),
}

/// What a search varies besides the schedule: how many seeds it tries,
/// and how many directory order keys besides the stock order.
#[derive(Clone, Copy, Debug)]
struct Varies {
    seeds: u64,
    dirents: u64,
}

/// How a search's lines start: a run's schedule, its seed too when the
/// search tries more than one, and its directory order key when the
/// search tries keys, padded so the lines align.
fn run_label(varies: Varies, seed: u64, key: u64, schedule: u64) -> String {
    let mut label = String::new();
    if varies.seeds > 1 {
        label.push_str(&format!("seed {seed:>2} "));
    }
    if varies.dirents > 0 {
        label.push_str(&format!("dirents {key:>2} "));
    }
    label.push_str(&format!("schedule {schedule:>3}"));
    label
}

/// How a search's sentences name a run, as [`run_label`] without the
/// padding.
fn run_name(varies: Varies, seed: u64, key: u64, schedule: u64) -> String {
    let mut name = String::new();
    if varies.seeds > 1 {
        name.push_str(&format!("seed {seed} "));
    }
    if varies.dirents > 0 {
        name.push_str(&format!("dirents {key} "));
    }
    name.push_str(&format!("schedule {schedule}"));
    name
}

/// `machine` with keyframes and progress, for an unperturbed run that
/// other runs start from.
fn with_keyframes(machine: &MachineArgs) -> MachineArgs {
    MachineArgs {
        no_keyframes: false,
        progress: Progress::Shown,
        ..machine.clone()
    }
}

/// One thread `rewind check --where` names, for its JSON: why, the step,
/// and the thread's frame in the program's own code, or why the lookup
/// failed.
fn thread_json(
    involved: locate::Involved,
    step: u64,
    answer: &Result<Located>,
) -> serde_json::Value {
    let why = match involved {
        locate::Involved::AtTheDecidingStep => "deciding_step",
        locate::Involved::AtTheFirstDifference => "first_difference",
    };
    match answer {
        Err(e) => serde_json::json!({ "why": why, "step": step, "error": format!("{e:#}") }),
        Ok(a) => {
            let frame = a.chosen.and_then(|i| a.frames.get(i));
            serde_json::json!({
                "why": why,
                "step": step,
                "pid": a.pid,
                "tid": a.tid,
                "process": a.process,
                "frame": frame,
            })
        }
    }
}

/// The arguments of `rewind fork`.
#[derive(clap::Args)]
pub(crate) struct ForkArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// The schedule seed for the new branch; 1 by default, or 0 with
    /// --seed.
    ///
    /// As with a run's --schedule, values programs draw from the kernel's
    /// randomness after the step, such as load addresses, can differ from
    /// the parent's; a run recorded with `--kernel-args norandmaps` loads
    /// programs at fixed addresses.
    #[arg(long)]
    pub(crate) schedule: Option<u64>,
    /// Reseeds the VM's randomness at the step with this seed, so the
    /// branch draws other random numbers from there on: hash map order,
    /// which branch a select tries first, temporary names.
    #[arg(long)]
    pub(crate) seed: Option<u64>,
    /// The longest stall the branch's schedule asks for, such as 50ms, as
    /// a run's --stall-max sets it. Each window the fork inherits from its
    /// parent keeps its own maximum.
    #[arg(long, value_name = "DURATION", default_value_t = StallMax::default())]
    pub(crate) stall_max: StallMax,
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

pub(crate) fn fork(home: &Home, args: ForkArgs) -> Result<ExitCode> {
    let ForkArgs {
        run,
        step,
        schedule,
        seed,
        stall_max,
        quiet,
        json,
        timeout,
    } = args;
    let parent = Run::find(home, &run)?;
    let step = parent.check_step(step)?;
    let m = &parent.manifest;
    let schedule = schedule.unwrap_or(if seed.is_some() { 0 } else { 1 });
    if schedule == 0 && seed.is_none() {
        bail!(
            "schedule 0 leaves the threads alone, so a fork under it repeats its parent; give another --schedule, or a --seed"
        );
    }
    refuse_unscheduled_stall_max(schedule, stall_max)?;
    let mut spec = m.spec.fork(step, schedule).with_stall_max(stall_max);
    let mut name = format!(
        "{} (fork of {} at {step}, schedule {schedule}",
        m.name, m.id
    );
    name.push_str(&stall_max_words(spec.stall_max));
    if let Some(seed) = seed {
        spec = spec.reseeded(step, seed);
        name.push_str(&format!(", seed {seed}"));
    }
    name.push(')');
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
    let paint = color::stderr();
    eprintln!("{}", show::finished(&child, stop.as_deref(), paint));
    match first_difference {
        None => eprintln!(
            "{} the fork {}",
            paint.prefix(),
            paint.verdict(Same::Yes, "ran the same as its parent")
        ),
        Some(step) => eprintln!(
            "{} the fork {} at step {step}",
            paint.prefix(),
            paint.verdict(Same::No, "first differs from its parent")
        ),
    }
    let open = open_line(&child.manifest.id, first_difference, Some(&m.id));
    eprintln!(
        "{} open it beside its parent in the desktop app: {open}",
        paint.prefix()
    );
    Ok(exit_status(&child))
}

/// A fork's longest stall as its name gives it: nothing at the default,
/// so forks made without --stall-max keep their names.
fn stall_max_words(max: StallMax) -> String {
    if max.is_default() {
        return String::new();
    }
    format!(
        ", stalls up to {}",
        rewind_trace::schedule::stall_words(max.ns())
    )
}

/// Refuses a longest stall other than the default for schedule 0, which
/// asks for no stalls, rather than recording a run that ignores it.
fn refuse_unscheduled_stall_max(schedule: u64, stall_max: StallMax) -> Result<()> {
    if schedule == 0 && !stall_max.is_default() {
        bail!("--stall-max needs a schedule other than 0: schedule 0 pauses no threads");
    }
    Ok(())
}

/// The clock a run will use. `exits` as asked; `branches` as asked when
/// this boot's self-test found the counter exact, refused when it did not;
/// for `auto` the branch counter when exact, with a warning when not.
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
        ClockArg::Branches => {
            let clock = branches()?;
            if !pmu::counter_usable_this_boot(home, guest, &vendor)? {
                bail!(pmu::branch_clock_refusal(&vendor));
            }
            Ok(clock)
        }
        ClockArg::Auto => {
            if vendor.event().is_some() && pmu::counter_usable_this_boot(home, guest, &vendor)? {
                return branches();
            }
            eprintln!(
                "{} {}",
                color::stderr().prefix(),
                pmu::exit_time_warning(&vendor)
            );
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
    /// A fixed x86-64-v3 CPU, the same on every host that has its features.
    V3,
    /// The v3 CPU without ERMS, for hosts that lack ERMS.
    V3Compat,
    /// This machine's own features.
    Host,
}

impl From<CpuArg> for rewind_vmm::CpuModel {
    fn from(c: CpuArg) -> Self {
        match c {
            CpuArg::V3 => rewind_vmm::CpuModel::V3,
            CpuArg::V3Compat => rewind_vmm::CpuModel::V3Compat,
            CpuArg::Host => rewind_vmm::CpuModel::Host,
        }
    }
}

/// What a run without --cpu says on a host that gets the v3-compat model.
const V3_COMPAT_NOTICE: &str = "this CPU lacks ERMS (fast REP MOVSB), which --cpu v3 needs, \
     so runs use --cpu v3-compat, which is v3 without ERMS and replays on hosts with or \
     without ERMS";

/// The CPU model a run shows the guest. A model given with --cpu is checked
/// against this host now, before a Nix closure is packed for minutes. With
/// none given, the model is v3, or v3-compat with a line saying so where
/// this host lacks only ERMS.
fn resolve_cpu(arg: Option<CpuArg>) -> Result<rewind_vmm::CpuModel> {
    let kvm = rewind_vmm::kvm::open()?;

    // A model asked for by name, which this host must be able to show.
    if let Some(arg) = arg {
        let model = arg.into();
        rewind_vmm::cpu::check(&kvm, model)?;
        return Ok(model);
    }

    // No model asked for: the one this host's features allow.
    let model = rewind_vmm::cpu::default_model(&kvm)?;
    if model == rewind_vmm::CpuModel::V3Compat {
        eprintln!("{} {V3_COMPAT_NOTICE}", color::stderr().prefix());
    }
    Ok(model)
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
    /// The longest stall a perturbed schedule asks for, such as 50ms: a
    /// race whose window holds a sleep, a timer or slow I/O may need one
    /// task held off the CPU that long.
    ///
    /// One exit in 128 still stalls, for between 10us and this maximum,
    /// each doubling of 10us as likely as the next, so the run spends more
    /// virtual time stalled. From the default up to 1s; at the default a
    /// run is the one made without the flag.
    #[arg(long, value_name = "DURATION", default_value_t = StallMax::default())]
    stall_max: StallMax,
    /// The CPU the VM sees: a fixed x86-64-v3 model that replays on any
    /// machine supporting it, the same model without ERMS, or this
    /// machine's own features.
    ///
    /// Defaults to v3, or to v3-compat on a machine that lacks only ERMS.
    #[arg(long, value_enum)]
    cpu: Option<CpuArg>,
    /// The CPU model `cpu` resolved to, once per command.
    #[arg(skip)]
    resolved_cpu: Option<rewind_vmm::CpuModel>,
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
    /// Whether a run that skips keyframes still takes one where the job
    /// exits; rewind check asks for one when a property reads the run's
    /// files there.
    #[arg(skip)]
    exit_keyframe: ExitKeyframe,
    /// Stop the run after this many seconds on this machine, however far it
    /// got.
    ///
    /// It then ends as timed-out, which depends on how fast this machine is.
    /// In rewind check, --timeout bounds every run, schedule 0 included.
    /// Without it schedule 0 has no limit, and every other run gets ten
    /// times as long as schedule 0 took, and at least a minute.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
    /// Extra kernel command line arguments; `loglevel=7` shows the kernel's
    /// messages in the trace.
    #[arg(long, default_value = "")]
    kernel_args: String,
}

impl MachineArgs {
    /// The options left to a default that this machine or this day
    /// decides: the epoch, which is the start of today, the CPU model,
    /// which is v3 or v3-compat by what this CPU has, and the clock,
    /// which `auto` picks by this machine's branch counter. A run whose
    /// inputs must all be written down, as a derivation's are, names
    /// each.
    pub(super) fn unpinned(&self) -> Vec<&'static str> {
        let mut unpinned = Vec::new();
        if self.epoch.is_none() {
            unpinned.push("--epoch");
        }
        if self.cpu.is_none() {
            unpinned.push("--cpu");
        }
        if self.clock == ClockArg::Auto {
            unpinned.push("--clock exits or --clock branches");
        }
        unpinned
    }

    /// The same options for a run whose files are read where its job
    /// exits, as a build's outputs are: a run that skips keyframes still
    /// takes one there, so the read restores the keyframe instead of
    /// replaying the run from boot.
    pub(super) fn read_at_exit(mut self) -> MachineArgs {
        self.exit_keyframe = ExitKeyframe::Take;
        self
    }
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
pub(super) enum Workload {
    Nix(String),
    Image(ImageArgs),
    /// A build nix-daemon described, and the file the description is in.
    Described {
        description: Box<nix::Description>,
        path: PathBuf,
    },
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

/// Whether a run that skips keyframes takes one where the job exits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ExitKeyframe {
    #[default]
    Skip,
    Take,
}

/// The keyframes a run executed with `machine` takes.
fn keyframes_for(machine: &MachineArgs) -> Keyframes {
    match (machine.no_keyframes, machine.exit_keyframe) {
        (false, _) => Keyframes::Take,
        (true, ExitKeyframe::Skip) => Keyframes::Skip,
        (true, ExitKeyframe::Take) => Keyframes::AtExit,
    }
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
pub(super) enum Report {
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
pub(super) fn run_workload(
    home: &Home,
    guest: &Guest,
    workload: &Workload,
    machine: &MachineArgs,
    report: Report,
) -> Result<Run> {
    refuse_unscheduled_stall_max(machine.schedule, machine.stall_max)?;
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

    // The CPU model and the clock, resolved once for every run the command
    // makes, so a notice about either is printed once.
    if machine.resolved_cpu.is_none() {
        machine.resolved_cpu = Some(resolve_cpu(machine.cpu)?);
    }
    if machine.resolved_clock.is_none() {
        machine.resolved_clock = Some(resolve_clock(home, guest, machine.clock)?);
    }
    let (name, source, image, job) = match workload {
        Workload::Nix(installable) => prepare_nix(home, installable, machine.cores)?,
        Workload::Described { description, path } => {
            prepare_described(home, description, path, machine.cores)?
        }
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
    let graphs = nix::reference_graphs(&drv, &closure)?;
    let build = nix::describe(&drv, closure, &graphs, cores)?;
    let source = Source::Nix {
        drv: drv_path.display().to_string(),
        outputs: drv
            .outputs
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
    };
    prepare_build(home, &build, Path::new(nix::STORE), source)
}

/// The builder of a build nix-daemon described in the file at `path`,
/// told it has `cores` cores, with the inputs the description names as
/// the image.
fn prepare_described(
    home: &Home,
    description: &nix::Description,
    path: &Path,
    cores: u32,
) -> Result<(String, Source, Option<PathBuf>, Job)> {
    let mut build = description.build.clone();
    build.set_cores(cores);
    let source = Source::Build {
        description: path.display().to_string(),
        outputs: build
            .outputs
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
    };
    prepare_build(home, &build, &description.real_store, source)
}

/// A described build's builder, with the build's inputs as the image.
/// `store` is where this machine has the inputs' files.
fn prepare_build(
    home: &Home,
    build: &nix::Build,
    store: &Path,
    source: Source,
) -> Result<(String, Source, Option<PathBuf>, Job)> {
    // Images of store paths are named by the paths, which already name
    // their contents in one store. A store kept somewhere else may hold
    // other contents under a path's name, so its images are named by
    // where the store is as well.
    let mut names: Vec<String> = build
        .inputs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if store != Path::new(nix::STORE) {
        names.push(store.to_string_lossy().into_owned());
    }
    let key = blake3::hash(names.join("\n").as_bytes()).to_hex();
    let image = home.images().join(format!("store-{}.erofs", &key[..32]));
    if !image.exists() {
        eprintln!(
            "{} packing {} store paths for {}",
            color::stderr().prefix(),
            build.inputs.len(),
            build.name
        );
        let tmp = image::temp_beside(&image);
        image::from_store_paths(store, &build.inputs, &tmp)?;
        image::place(&tmp, &image)?;
    }

    Ok((build.name.clone(), source, Some(image), build.job()))
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
    let cpu = machine
        .resolved_cpu
        .context("the CPU model was not resolved before the run")?;
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
        stall_max: StallMax::default(),
        inherited_schedules: Vec::new(),
        reseeds: Vec::new(),
        cpu,
        clock: machine.resolved_clock.unwrap_or_default(),
        preemption: if machine.experimental_preempt {
            rewind_vmm::Preemption::AtBranchCounts
        } else {
            rewind_vmm::Preemption::AtExits
        },
        extras: rewind_vmm::extras_for(cpu)?,
        cmdline: format!("{BASE_CMDLINE} {}", machine.kernel_args)
            .trim()
            .to_string(),
        job: prepared.job.clone(),
    }
    .with_stall_max(machine.stall_max);
    let loudness = if machine.quiet {
        Loudness::Quiet
    } else {
        Loudness::Output
    };
    let echo = echo_for(loudness, machine.progress, Terminal::stderr());
    let how = Execution {
        echo,
        keyframes: keyframes_for(machine),
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
        eprintln!("{}", show::finished(&run, stop.as_deref(), color::stderr()));
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

/// A run `rewind check` tried, for its JSON: the run, its seed, directory
/// order key and schedule, whether its verdict differs from the
/// unperturbed run's, and the properties it violates.
fn schedule_json(run: &Run, differs: bool, violations: &[Violation]) -> Result<serde_json::Value> {
    let mut value = json::run(run)?;
    value["seed"] = show::seed_of(&run.manifest.spec).into();
    value["dirents"] = dirents_of(&run.manifest.spec.cmdline).into();
    value["schedule"] = run.manifest.spec.schedule.into();
    value["differs"] = differs.into();
    value["violations"] = violations_json(violations);
    Ok(value)
}

/// Violations for JSON: each property, its step when known, and why.
fn violations_json(violations: &[Violation]) -> serde_json::Value {
    violations
        .iter()
        .map(|v| serde_json::json!({ "property": v.property, "step": v.step, "why": v.why }))
        .collect()
}

/// The end of a schedule's line when properties were given: whether the
/// run holds them all or which it violates. Nothing for the default,
/// whose line already says how the run ended.
fn verdict_words(judge: &Judge, violations: &[Violation], paint: Paint) -> String {
    if judge.same_outcome_only() {
        return String::new();
    }
    if violations.is_empty() {
        return format!("  {}", paint.verdict(Same::Yes, "holds"));
    }
    format!(
        "  {} {}",
        paint.verdict(Same::No, "violates"),
        violated_names(violations)
    )
}

/// The words `rewind check`'s report names its two runs by.
const FAILING: &str = "failing";
const PASSING: &str = "passing";

/// The properties `violations` name, as a report lists them.
fn violated_names(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(|v| v.property.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Each violation of an unperturbed run on a line of its own, under its
/// line, when properties were given, said through `say`.
fn print_violations(judge: &Judge, violations: &[Violation], say: impl Fn(String)) {
    if judge.same_outcome_only() {
        return;
    }
    for v in violations {
        say(format!("  {}: {v}", v.property));
    }
}

/// For a schedule of `rewind check` that hit its time limit, how and where,
/// said through `say`: a guest stuck computing without exits is often what
/// the search found. A user-space address is named by the program's
/// symbols when it can be.
fn print_timeout(home: &Home, schedule: u64, run: &Run, say: impl Fn(String)) {
    let Some(o) = run.manifest.outcome.as_ref() else {
        return;
    };
    if o.stop.timeout().is_none() {
        return;
    }
    let stop = locate::describe_stall(home, run).unwrap_or_else(|| o.stop.to_string());
    say(format!("schedule {schedule}: {stop}"));
}

/// Where the threads of a failure were, as `rewind check --where` lists
/// them after its narrowing: a heading, then a line for each thread, or
/// the reason gdb could not walk it. No lines without a thread.
fn where_lines(threads: &[(locate::Involved, u64, Result<Located>)]) -> Vec<String> {
    if threads.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["\nwhere the threads were:".to_string()];
    for (involved, step, answer) in threads {
        lines.push(match answer {
            Ok(answer) => locate::thread_line(answer, *involved),
            Err(e) => format!("  {step:>10}   {e:#}"),
        });
    }
    lines
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
            eprintln!(
                "{} comparing with your store only: {e:#}",
                color::stderr().prefix()
            );
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
    let paint = color::stdout();
    for (path, hash, comparisons) in &compared.outputs {
        println!("{}", show::verdict_line(path, hash, comparisons, paint));
    }
    let differs = compared
        .outputs
        .iter()
        .flat_map(|(_, _, comparisons)| comparisons)
        .any(|c| c.verdict == compare::Verdict::Differs);
    if differs {
        println!("{} {}", paint.prefix(), show::DIFFERS_NOTE);
    }
    if !compared.outputs.is_empty() && !compared.skipped.is_empty() {
        println!(
            "{} did not ask {}, which are not HTTP binary caches",
            paint.prefix(),
            compared.skipped.join(", ")
        );
    }
    for path in &compared.missing {
        println!(
            "{path} {}: the builder exited 0 without creating it",
            paint.verdict(Same::No, "missing")
        );
    }
}

/// Builds or reuses the erofs image for a root filesystem argument.
fn root_image(home: &Home, root: &Path) -> Result<PathBuf> {
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
pub(super) fn exit_status(run: &Run) -> ExitCode {
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

    /// The lines that say where the threads of a failure were, which check
    /// prints after the narrowing, to standard error with --json: a heading,
    /// then a line for each thread, its error when gdb could not walk it.
    /// None at all when no thread was asked about. Builds the answers by
    /// hand.
    #[test]
    fn the_threads_of_a_failure_are_listed_under_a_heading() {
        let answer = Located {
            run: "5c910df9774b38f2".into(),
            step: 5139,
            pid: 166,
            tid: 166,
            process: "test_pool_shutdown".into(),
            frames: Vec::new(),
            chosen: None,
            files: std::collections::BTreeMap::new(),
        };
        let threads = vec![
            (locate::Involved::AtTheDecidingStep, 5139, Ok(answer)),
            (
                locate::Involved::AtTheFirstDifference,
                5200,
                Err(anyhow::anyhow!("no process 40 at this step")),
            ),
        ];
        assert_eq!(
            where_lines(&threads),
            vec![
                "\nwhere the threads were:".to_string(),
                "        5139   166/166   no frame names a function, on the CPU at the deciding step"
                    .to_string(),
                "        5200   no process 40 at this step".to_string(),
            ]
        );
        assert!(where_lines(&[]).is_empty());
    }

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

    #[test]
    fn a_search_over_directory_orders_names_the_key_on_every_line() {
        // The labels of a search that varies the directory order key, with
        // one seed and with several, built for the stock order and for a
        // key: each line names its key in a column of its own, so the
        // lines of one search are as wide as each other. A search that
        // varies neither keeps its schedule-only labels.
        let keys = Varies {
            seeds: 1,
            dirents: 3,
        };
        assert_eq!(run_label(keys, 0, 0, 0), "dirents  0 schedule   0");
        assert_eq!(run_label(keys, 0, 2, 0), "dirents  2 schedule   0");
        assert_eq!(run_name(keys, 0, 2, 0), "dirents 2 schedule 0");

        let both = Varies {
            seeds: 8,
            dirents: 3,
        };
        assert_eq!(run_label(both, 0, 2, 0), "seed  0 dirents  2 schedule   0");
        assert_eq!(run_label(both, 5, 0, 0), "seed  5 dirents  0 schedule   0");
        assert_eq!(run_name(both, 5, 0, 0), "seed 5 dirents 0 schedule 0");

        let neither = Varies {
            seeds: 1,
            dirents: 0,
        };
        assert_eq!(run_label(neither, 0, 0, 12), "schedule  12");
        assert_eq!(run_name(neither, 0, 0, 12), "schedule 12");
    }

    #[test]
    fn a_key_run_adds_the_key_to_the_kernel_arguments() {
        // Kernel arguments with and without other arguments, given a key:
        // the key is appended as rewind.dirents=N, and read back from a
        // run's whole command line, 0 when the line sets none.
        assert_eq!(with_dirents("", 2), "rewind.dirents=2");
        assert_eq!(with_dirents("norandmaps", 3), "norandmaps rewind.dirents=3");
        assert_eq!(dirents_of(&format!("{BASE_CMDLINE} rewind.dirents=3")), 3);
        assert_eq!(dirents_of(&format!("{BASE_CMDLINE} norandmaps")), 0);
        assert_eq!(dirents_of(BASE_CMDLINE), 0);
    }

    #[test]
    fn kernel_arguments_that_set_the_key_are_found() {
        // Kernel arguments that set the key, alone, among others, or
        // empty, and ones that only look like it: only an argument named
        // rewind.dirents counts, which --dirents refuses beside it.
        assert_eq!(dirents_arg("rewind.dirents=1"), DirentsArg::Set);
        assert_eq!(
            dirents_arg("loglevel=7 rewind.dirents=0 norandmaps"),
            DirentsArg::Set
        );
        assert_eq!(dirents_arg(""), DirentsArg::Unset);
        assert_eq!(dirents_arg("norandmaps"), DirentsArg::Unset);
        assert_eq!(dirents_arg("rewind.direntsx=1"), DirentsArg::Unset);
    }
}
