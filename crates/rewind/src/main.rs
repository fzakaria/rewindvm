//! The `rewind` command.

mod doctor;
mod downloads;
mod gdb;
mod json;
mod list;
mod locate;
mod reproduce;
mod show;
mod terminal;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use rewind_core::inspect::Inspection;
use rewind_core::run::{
    BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM, MAX_CORES, Start, default_epoch,
};
use rewind_core::{Echo, Execution, Guest, Home, Keyframes, Run, Source, Spec, TimeLimit};
use rewind_core::{compare, export, image, nix};
use rewind_init::{Job, Root};

/// The help of every argument that names a run.
const RUN_HELP: &str = "The run: its id or the start of one, its name, @ for the newest";
const RUN_LONG_HELP: &str = "The run: its id or the start of one, its name for the newest run \
     with that name, @ for the newest run and @2, @3 and on for the ones before it, or its \
     directory.";

/// The help of every argument that names a step of a run.
const STEP_HELP: &str = "A step of the run, as `rewind events` numbers them";
const STEP_LONG_HELP: &str = "A step of the run: how many times the VM had stopped for the \
     host by then, as `rewind events` and `rewind log --steps` number them, from 0 to the \
     step the run ended at.";

/// `rewind cat`'s exit status when the file did not exist at the step:
/// apart from 1, which any failure exits with, and 2, clap's for a
/// command line it refused.
const CAT_NOT_FOUND: u8 = 3;

/// The VM's memory in MiB unless --mem says otherwise.
const DEFAULT_MEM_MIB: u64 = 1024;

/// The PATH a command in a root filesystem gets unless --env sets one.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The desktop app, looked up on PATH unless REWIND_APP names it.
const APP_PROGRAM: &str = "rewind-app";
const APP_ENV: &str = "REWIND_APP";

/// How many callers of the chosen frame `rewind where` shows.
const DEFAULT_CALLERS: usize = 2;

#[derive(Parser)]
#[command(
    version = rewind_core::VERSION,
    about = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The machine options every way of starting a run shares.
#[derive(clap::Args, Clone)]
struct MachineArgs {
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

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ClockArg {
    Auto,
    Exits,
    Branches,
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

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CpuArg {
    V3,
    Host,
}

impl From<CpuArg> for rewind_vmm::cpu::Model {
    fn from(c: CpuArg) -> Self {
        match c {
            CpuArg::V3 => rewind_vmm::cpu::Model::V3,
            CpuArg::Host => rewind_vmm::cpu::Model::Host,
        }
    }
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
    /// The working directory inside the VM.
    #[arg(long, default_value = "/")]
    cwd: String,
    /// The command and its arguments.
    #[arg(last = true)]
    argv: Vec<String>,
}

#[derive(Subcommand)]
enum PmuAction {
    /// Show the CPU, the workaround's state, and a self-test of the counter.
    Status,
    /// Apply rr's workaround for AMD Zen's branch counter on every CPU, until
    /// reboot.
    ///
    /// Run it as root: sudo rewind pmu enable.
    Enable,
}

/// What `rewind generate` writes, for packagers.
#[derive(Subcommand)]
enum Generate {
    /// A shell's completions, on standard output.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// A man page for rewind and one for each of its commands, in DIR.
    Man { dir: PathBuf },
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
        /// Print the run as one JSON object on standard output once it
        /// ends, for programs, instead of its output and summary: its id,
        /// name, directory, how it ended and the outputs its job hashed.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Build a Nix derivation in the deterministic VM.
    Nix {
        /// A .drv path or an installable such as `nixpkgs#hello`.
        installable: String,
        /// Also compare the outputs with this binary cache's builds of them,
        /// besides the substituters Nix is configured with.
        ///
        /// Repeatable.
        #[arg(long = "compare-with", value_name = "URL")]
        compare_with: Vec<String>,
        /// Compare the outputs with this machine's store only, asking no
        /// binary cache.
        #[arg(long)]
        no_compare: bool,
        /// Print the run as one JSON object on standard output once it
        /// ends, for programs, instead of its output and summary: as `run
        /// --json` does, with what each store and cache said of each output.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Find a schedule that changes how a build or command ends, and where
    /// it went its own way.
    ///
    /// Runs a Nix derivation, or a command with --root, under several
    /// schedules, narrows the steps the first one that ends differently
    /// perturbs, and shows where it parts from the run that did not.
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
        /// How many machines to run at once; one per CPU by default.
        #[arg(long, short)]
        jobs: Option<usize>,
        /// Print one JSON object on standard output once the search ends,
        /// for programs: each schedule tried, as `rewind ls --json` and
        /// `fork --json` describe runs, and the window, the two runs and
        /// where they part when one ended differently.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        machine: MachineArgs,
    },
    /// Branch a run at a step: the same run up to the step, then another
    /// interleaving from there.
    Fork {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
        step: u64,
        /// The schedule seed for the new branch.
        ///
        /// As with a run's --schedule, values programs draw from the kernel's
        /// randomness after the step, such as load addresses, can differ from
        /// the parent's; a run recorded with `--kernel-args norandmaps` loads
        /// programs at fixed addresses.
        #[arg(long, default_value_t = 1)]
        schedule: u64,
        /// Print nothing while the fork executes.
        #[arg(long, short)]
        quiet: bool,
        /// Print the result as one JSON object on standard output, for
        /// programs such as the desktop app.
        #[arg(long)]
        json: bool,
        /// Stop the fork after this many seconds on this machine, however
        /// far it got, as `--timeout` does for a run.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// Remove forks of a run, and forks of those, that ran exactly as an
    /// older one did.
    ///
    /// The run itself stays, and so does any run another run here was forked
    /// from or reads keyframes from.
    Prune {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// Remove forks whose trace is the same as an older fork's in the
        /// family, or the run's own.
        #[arg(long)]
        identical: bool,
        /// Show what would be removed and remove nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print the ids removed as one JSON array on standard output, for
        /// programs such as the desktop app.
        #[arg(long)]
        json: bool,
    },
    /// Remove runs, with every run forked from them and from those.
    ///
    /// Their imported inputs go with them. Refused, removing nothing, while one of them has not finished or a run
    /// that stays reads keyframes from one of them. The images and pages they
    /// used stay until `rewind gc`. Many runs at once take one call: `xargs
    /// rewind remove < ids`. A run `rewind ls` lists as unreadable is named
    /// by its id.
    Remove {
        #[arg(value_name = "RUN", required = true, help = RUN_HELP, long_help = RUN_LONG_HELP)]
        runs: Vec<String>,
        /// Show what would be removed and remove nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print {"removed": [ids]} on standard output, each run named
        /// before its forks, for programs such as the desktop app.
        #[arg(long)]
        json: bool,
    },
    /// Remove the images, pages and source files no run uses any more.
    ///
    /// These are the cached images no run names and the pages in the page
    /// store no keyframe names, which `remove` and `prune` leave behind, and
    /// the source files cached for runs that are gone. Refused, removing nothing, while another rewind process is packing an
    /// image, executing a run, has a shell open or has the page store open.
    Gc {
        /// Show what would be removed and remove nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print what was removed as one JSON object on standard output:
        /// {"images": [{"path", "bytes"}], "source_caches": [paths], "pages",
        /// "page_bytes", "bytes"}.
        #[arg(long)]
        json: bool,
    },
    /// Print a file as it was at a step of a run.
    ///
    /// Rewind forks the run at the step and reads the file inside the VM, so
    /// this takes about as long as seeking there. Exits 3 when the file did
    /// not exist then.
    Cat {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
        step: u64,
        /// The path, absolute or relative to the process's working
        /// directory.
        path: String,
        /// Resolve the path as this process saw it, in its root and working
        /// directory.
        ///
        /// By default, and once it has exited, the job's.
        #[arg(long)]
        pid: Option<u32>,
    },
    /// A shell inside the VM at a step of a run.
    ///
    /// It has the job's environment and starts in a process's root and
    /// working directory, while everything else in the VM stays stopped
    /// where it was. Nothing done in it changes the run: it happens in a
    /// throwaway fork.
    Shell {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
        step: u64,
        /// The process whose root and working directory the shell starts in.
        ///
        /// By default, and once it has exited, the job's.
        #[arg(long)]
        pid: Option<u32>,
        /// More packages in the shell, from Nix: an installable such as
        /// nixpkgs#strace, built or fetched here, its closure visible in the
        /// VM's /nix/store and its bin directory first on PATH.
        ///
        /// Repeat for several.
        #[arg(long = "with", value_name = "INSTALLABLE")]
        with: Vec<String>,
    },
    /// gdb on a fork of a run at a step.
    ///
    /// It debugs one x86-64 CPU and every thread of the process, the VM's
    /// memory as its page tables map it, with breakpoints and single steps.
    /// Starts the host's gdb with the symbols of the VM's kernel and of the
    /// process running at the step, or of --pid's, and a debuginfod server
    /// for their DWARF and sources; with --listen, only serves the GDB remote
    /// protocol for a gdb started some other way. gdb starts in the thread
    /// `rewind where` looks at, in the registers it entered the kernel with,
    /// rather than in the CPU's, which at a step are in the kernel.
    Gdb {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
        step: u64,
        /// Debug this process, whether or not it was running at the step:
        /// its symbols, its breakpoints and every one of its threads.
        #[arg(long)]
        pid: Option<u32>,
        /// Start in this thread; by default the step's event's, or the
        /// main thread of --pid.
        #[arg(long)]
        tid: Option<u32>,
        /// Start in this frame of the thread, as `rewind where --json`
        /// numbers them; by default the innermost.
        #[arg(long)]
        frame: Option<u32>,
        /// Serve on this address, such as 127.0.0.1:1234, and start no gdb.
        #[arg(long)]
        listen: Option<String>,
        /// More arguments for gdb, after `--`, such as -batch -ex bt.
        ///
        /// They come after the ones that load the symbols and connect.
        #[arg(last = true)]
        gdb_args: Vec<String>,
    },
    /// Where in the program's own code a thread was at a step.
    ///
    /// Prints its innermost frame outside the kernel, the C library, Rust's
    /// standard library and dependencies, with its source, and the frames
    /// that called it. By default the thread of the step's own event, the pid and tid `rewind
    /// events` prints, and at a step with no event or the kernel's own, such
    /// as a console line, the thread on the CPU. Takes gdb with Python on
    /// PATH, and a run recorded with a kernel that lists its tasks.
    Where {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
        step: u64,
        /// Look at this process; by default the step's event's.
        #[arg(long)]
        pid: Option<u32>,
        /// Look at this thread; by default the step's event's, or the
        /// main thread of --pid.
        #[arg(long)]
        tid: Option<u32>,
        /// How many frames that called the chosen one to show.
        #[arg(long, default_value_t = DEFAULT_CALLERS)]
        frames: usize,
        /// Print every frame, the chosen one's index and each source file
        /// the frames are in, whole up to a mebibyte and the lines around
        /// the frames' lines past that, as one JSON object on standard
        /// output, for programs such as the desktop app.
        #[arg(long)]
        json: bool,
    },
    /// Open a run in the desktop app, at a step and beside another run.
    ///
    /// Starts rewind-app, from PATH or REWIND_APP, on the run's directory.
    /// `rewind check` and `rewind fork` print the command that opens what
    /// they found.
    Open {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// The step the playhead starts at; by default the app's choice.
        step: Option<u64>,
        /// Show this run beside it, lined up with it, as the passing run
        /// beside a failing one.
        #[arg(long, value_name = "RUN")]
        compare: Option<String>,
    },
    /// Whether this machine can record runs and look inside them.
    ///
    /// Checks KVM, the guest, which clock runs get, gdb and its Python, the
    /// debuginfod server, nix and mkfs.erofs, the disk the home takes, and
    /// runs whose manifests do not read, saying what to do about each
    /// problem. Exits 1 when runs cannot be recorded here.
    Doctor,
    /// Whether this host's performance counters can drive virtual time.
    Pmu {
        #[command(subcommand)]
        action: PmuAction,
    },
    /// Write a run to a single .rwd file.
    Export {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// Where to write it; <id>.rwd by default.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Include keyframes, their pages, the input image and the VM's
        /// kernel, so another machine with a compatible CPU can replay the
        /// run.
        #[arg(long)]
        replayable: bool,
    },
    /// Read a .rwd file into the local runs.
    Import {
        /// A .rwd file, or an http or https URL of one, which is unpacked
        /// as it downloads.
        file: String,
        /// Print the run as one JSON object on standard output, for
        /// programs such as the desktop app: its id, its directory, and
        /// whether it has the keyframes and inputs to replay.
        #[arg(long)]
        json: bool,
    },
    /// List runs, newest first: each one's id, how it ended, its steps and
    /// its name.
    ///
    /// A run whose manifest another build of rewind wrote is listed as
    /// unreadable, with why; `rewind remove` takes it by id.
    Ls {
        /// List only the newest N of the runs the other options pick.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
        /// Only runs whose name contains TEXT.
        #[arg(long, value_name = "TEXT")]
        name: Option<String>,
        /// Only runs that stand so.
        #[arg(long, value_enum)]
        status: Option<list::Status>,
        /// Only the runs forked from RUN, and forks of those.
        #[arg(long, value_name = "RUN")]
        forks_of: Option<String>,
        /// Only runs made since WHEN: an amount ago, such as 30m, 2h, 7d or
        /// 2w, or a day, such as 2026-10-01, from its start in UTC.
        #[arg(long, value_name = "WHEN")]
        since: Option<String>,
        /// Print one JSON object a line per run, for programs: its id,
        /// name, directory, when it was made, its parent, how it stands,
        /// its wait status, ending and steps.
        #[arg(long)]
        json: bool,
    },
    /// The command that makes a run again, and for a fork its parents'.
    ///
    /// A run's id is the hash of its inputs, so the same command on a
    /// machine with the same CPU vendor and guest makes the same run. The
    /// commands name the epoch, which by default is the start of the day a
    /// run is made, so the same command a day later makes another run.
    Show {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// Print one JSON object on standard output, for programs: the
        /// run's id, whether this rewind boots the guest the run booted, and
        /// oldest first each command with the id of the run it makes, as
        /// arguments after `rewind` and as a line for a shell.
        #[arg(long)]
        json: bool,
    },
    /// Print a run's output, up to a step.
    Log {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// The step to print up to; by default the run's end.
        step: Option<u64>,
        /// Prefix each line with its step and pid.
        #[arg(long, short)]
        steps: bool,
    },
    /// Show the processes alive at a step.
    Ps {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// The step; by default the run's end.
        step: Option<u64>,
        /// Show the kernel's own threads too.
        #[arg(long)]
        all: bool,
        /// Print one JSON object on standard output, for programs: the step
        /// and each process alive then, with its parent, command line and
        /// threads.
        #[arg(long)]
        json: bool,
    },
    /// Print a run's events.
    Events {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// The first step whose events to print; by default 0.
        #[arg(long, value_name = "STEP")]
        from: Option<u64>,
        /// The last step whose events to print; by default the run's end.
        #[arg(long, value_name = "STEP")]
        to: Option<u64>,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Run a run's inputs again and check the trace comes out identical.
    Replay {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        run: String,
        /// Start from the keyframe at or before this step instead of boot.
        #[arg(long, value_name = "STEP")]
        from: Option<u64>,
        /// Print one JSON object on standard output, for programs: whether
        /// the trace came out identical, the keyframe it started from, and
        /// where it first differed.
        #[arg(long)]
        json: bool,
    },
    /// Shell completions and man pages, which the packages install.
    #[command(hide = true)]
    Generate {
        #[command(subcommand)]
        what: Generate,
    },
    /// Compare two runs and show where they first differ.
    ///
    /// Exits 0 when the runs are identical and 1 when they differ.
    Diff {
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        left: String,
        #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
        right: String,
        /// Print one JSON object on standard output, for programs: the two
        /// runs' ids and where they first differ, null when identical.
        #[arg(long)]
        json: bool,
    },
}

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

/// Whether standard error is a terminal, where a status line can be drawn
/// over itself; in a pipe or a log it would only add lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Yes,
    No,
}

impl Terminal {
    fn stderr() -> Terminal {
        use std::io::IsTerminal;
        if std::io::stderr().is_terminal() {
            Terminal::Yes
        } else {
            Terminal::No
        }
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

/// Draws `text` as the status line on standard error, over the last one,
/// when it is a terminal.
fn status(text: &str) {
    if Terminal::stderr() == Terminal::Yes {
        eprint!("{}{text}", rewind_core::run::CLEAR_LINE);
    }
}

/// Takes the status line away, before anything else is printed.
fn clear_status() {
    if Terminal::stderr() == Terminal::Yes {
        eprint!("{}", rewind_core::run::CLEAR_LINE);
    }
}

/// Whether a command holds the home in use while it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeHold {
    /// It packs images, executes runs, boots a run's image or opens the
    /// page store, so `rewind gc` must not remove an image between its
    /// packing and the manifest of the run that boots it, nor pages under
    /// a machine that reads them.
    InUse,
    /// It reads only manifests and traces, or, as gc does, takes the home
    /// its own way.
    Unheld,
}

impl Command {
    /// Every command says, so a new one cannot forget to.
    fn home_hold(&self) -> HomeHold {
        match self {
            Command::Run { .. }
            | Command::Nix { .. }
            | Command::Check { .. }
            | Command::Fork { .. }
            | Command::Cat { .. }
            | Command::Shell { .. }
            | Command::Gdb { .. }
            | Command::Where { .. }
            | Command::Replay { .. }
            | Command::Export { .. }
            | Command::Import { .. } => HomeHold::InUse,
            Command::Prune { .. }
            | Command::Remove { .. }
            | Command::Gc { .. }
            | Command::Pmu { .. }
            | Command::Open { .. }
            | Command::Doctor
            | Command::Ls { .. }
            | Command::Show { .. }
            | Command::Log { .. }
            | Command::Ps { .. }
            | Command::Events { .. }
            | Command::Diff { .. }
            | Command::Generate { .. } => HomeHold::Unheld,
        }
    }
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
    // Setting the AMD workaround needs no runs, and the NixOS module's boot
    // service runs it with no HOME to find them under.
    if let Command::Pmu {
        action: PmuAction::Enable,
    } = cli.command
    {
        return pmu_enable();
    }

    // Completions and man pages need no runs, and a package's build has no
    // home to find them under.
    let command = match cli.command {
        Command::Generate { what } => return generate(what),
        command => command,
    };
    let home = Home::open()?;

    // Commands that pack images, boot a run or read pages hold the home in
    // use until they exit (see `HomeHold`).
    let _in_use = match command.home_hold() {
        HomeHold::InUse => Some(home.in_use()?),
        HomeHold::Unheld => None,
    };
    match command {
        Command::Run {
            image,
            json,
            machine,
        } => {
            let guest = Guest::from_env()?;
            let workload = Workload::Image(image);
            let run = run_workload(&home, &guest, &workload, &machine, Report::of(json))?;
            if json {
                println!("{}", json::run(&run)?);
            }
            Ok(exit_status(&run))
        }
        Command::Nix {
            installable,
            compare_with,
            no_compare,
            json,
            machine,
        } => {
            let guest = Guest::from_env()?;
            let workload = Workload::Nix(installable);
            let run = run_workload(&home, &guest, &workload, &machine, Report::of(json))?;
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
        Command::Check {
            installable,
            image,
            schedules,
            all,
            jobs,
            json,
            mut machine,
        } => {
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
            let prepared = prepare(&home, &guest, &workload, &mut machine)?;

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
                &home,
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
                print_timeout(&home, 0, &base);
            }
            let mut tried_runs = vec![schedule_json(&base)?];

            // A schedule can make a program loop forever where schedule 0
            // did not, so unless told otherwise each other run gets a
            // generous multiple of schedule 0's time and then ends as
            // timed-out, instead of holding up the whole search.
            if machine.timeout.is_none() {
                let base_wall = base.manifest.outcome.as_ref().map_or(0, |o| o.wall_ms);
                let limit =
                    Duration::from_millis(base_wall * CHECK_TIMEOUT_FACTOR).max(CHECK_TIMEOUT_MIN);
                machine.timeout = Some(limit.as_secs());
            }
            let base_trace = base.trace()?;
            let start = show::start_step(base_trace).max(user_from);
            let base_key = show::outcome_key(&base)?;
            // When the unperturbed run is the one that fails, the schedules
            // that end differently are the ones that pass. A job that exits
            // 0 without creating every output fails, as under nix-daemon.
            let base_missing = show::missing_outputs(&base.manifest.spec.job.outputs, &base_key.1);
            let base_failed = !show::passed(base_key.0, &base_missing);
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
                let runs = execute_all(&home, &guest, &prepared, machines, from_base.clone());
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
            let Some(mut worst) = failing else {
                say(format!("same result under all {} schedules", tried + 1));
                if json {
                    println!("{}", search(serde_json::Value::Null));
                }
                return Ok(ExitCode::SUCCESS);
            };

            // Narrow it to a window of steps: first the latest start that
            // still changes the outcome, then the earliest end. Each step's
            // perturbation depends only on the seed and the step, so a
            // smaller window perturbs a subset of the same steps. The two
            // runs are identical up to the window, so where they part is
            // inside it, next to the interleaving that matters. The start
            // comes first because in a long run a perturbation anywhere
            // early changes everything after it; the latest start keeps the
            // window near the end that differs. Each round tries a point per
            // job, so a round divides the range by jobs + 1.
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
                print_timeout(&home, machine.schedule, &worst);
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
                execute_all(&home, &guest, &prepared, machines, from_base.clone())
            };
            let points = |lo: u64, hi: u64| -> Vec<u64> {
                let n = (jobs as u64).min(hi - lo - 1).max(1);
                (1..=n).map(|i| lo + (hi - lo) * i / (n + 1)).collect()
            };

            // The latest start: `lo` differs, `hi` (an empty window) does not.
            let (mut lo, mut hi) = (start, end);
            while hi - lo > 1 {
                let starts = points(lo, hi);
                let runs = probe_all(starts.iter().map(|&f| (f, end)).collect())?;
                let mut found = None;
                for (f, run) in starts.iter().zip(runs).rev() {
                    if differs(&run)? {
                        found = Some((*f, run));
                        break;
                    }
                }
                match found {
                    Some((f, run)) => {
                        lo = f;
                        hi = starts
                            .iter()
                            .copied()
                            .filter(|x| *x > f)
                            .min()
                            .unwrap_or(hi);
                        worst = run;
                    }
                    None => hi = starts[0],
                }
            }
            let from = lo;

            // The earliest end: `hi` differs, `lo` (an empty window) does not.
            let (mut lo, mut hi) = (from, end);
            while hi - lo > 1 {
                let ends = points(lo, hi);
                let runs = probe_all(ends.iter().map(|&e| (from, e)).collect())?;
                let mut next_lo = *ends.last().unwrap();
                let mut found = None;
                for (e, run) in ends.iter().zip(runs) {
                    if differs(&run)? {
                        found = Some((*e, run));
                        break;
                    }
                    next_lo = *e;
                }
                match found {
                    Some((e, run)) => {
                        hi = e;
                        lo = ends.iter().copied().filter(|x| *x < e).max().unwrap_or(lo);
                        worst = run;
                    }
                    None => lo = next_lo,
                }
            }
            let (lo, until) = (from, hi);
            clear_status();
            say(format!(
                "perturbing only steps {lo}..{until} still ends differently\n"
            ));
            let base = base.add_keyframes(&home)?;
            let worst = worst.add_keyframes(&home)?;
            let (passing, failing) = if base_failed {
                (worst, base)
            } else {
                (base, worst)
            };
            let (pt, ft) = (passing.trace()?, failing.trace()?);
            let culprit = ft.culprit_against(pt);
            if json {
                let divergence = match &culprit {
                    Some(argv) => json::program_divergence(pt, ft, argv),
                    None => json::divergence(pt, ft),
                };
                let narrowed = serde_json::json!({
                    "schedule": machine.schedule,
                    "from": lo,
                    "until": until,
                    "passing": json::run(&passing)?,
                    "failing": json::run(&failing)?,
                    "program": culprit,
                    "divergence": divergence,
                });
                println!("{}", search(narrowed));
                return Ok(ExitCode::FAILURE);
            }
            println!("passing: run {}", passing.manifest.id);
            println!("failing: run {}", failing.manifest.id);

            // Where they part, in words, and the failing run's step there,
            // where the app opens it beside the passing one.
            let parts_at = match &culprit {
                Some(argv) => {
                    println!("\nwhere {} first behaves differently:", argv.join(" "));
                    print!("{}", show::divergence_in(pt, ft, argv));
                    pt.divergence_in(ft, argv)
                        .and_then(|d| d.right_event())
                        .map(|(i, _)| ft.events[i].step)
                }
                None => {
                    print!("{}", show::divergence(pt, ft));
                    pt.divergence(ft).map(|d| d.right_step)
                }
            };
            let open = open_line(&failing.manifest.id, parts_at, Some(&passing.manifest.id));
            println!("\nopen both in the desktop app: {open}");
            Ok(ExitCode::FAILURE)
        }
        Command::Fork {
            run,
            step,
            schedule,
            quiet,
            json,
            timeout,
        } => {
            let parent = Run::find(&home, &run)?;
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
                &home,
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
            let stop = locate::describe_stall(&home, &child);
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
        Command::Prune {
            run,
            identical,
            dry_run,
            json,
        } => {
            if !identical {
                bail!("say what to prune: --identical removes forks that ran the same");
            }
            let root = Run::find(&home, &run)?;
            let removals = rewind_core::prune::plan_identical(&home, &root)?;
            if !dry_run {
                rewind_core::prune::remove(&home, &removals)?;
            }
            if json {
                let ids: Vec<&str> = removals.iter().map(|r| r.id.as_str()).collect();
                println!("{}", serde_json::json!(ids));
                return Ok(ExitCode::SUCCESS);
            }
            let verb = if dry_run { "would remove" } else { "removed" };
            for r in &removals {
                println!("{verb} {}: the same trace as {}", r.id, r.same_as);
            }
            if removals.is_empty() {
                eprintln!("rewind: no fork of {} needs pruning", root.manifest.id);
                return Ok(ExitCode::SUCCESS);
            }
            if !dry_run {
                print_gc_hint(&home);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Remove {
            runs,
            dry_run,
            json,
        } => {
            // Every run named must resolve before any is removed. One whose
            // manifest does not read is named by its id or a prefix of it.
            let runs = runs
                .iter()
                .map(|run| match Run::find(&home, run) {
                    Ok(found) => Ok(found.manifest.id),
                    Err(e) => Run::find_unreadable(&home, run).map(|u| u.id).ok_or(e),
                })
                .collect::<Result<Vec<String>>>()?;
            let act = if dry_run {
                rewind_core::prune::Act::DryRun
            } else {
                rewind_core::prune::Act::Remove
            };
            let removed = rewind_core::prune::remove_with_forks(&home, &runs, act)?;
            if json {
                println!("{}", serde_json::json!({ "removed": removed }));
                return Ok(ExitCode::SUCCESS);
            }
            let verb = if dry_run { "would remove" } else { "removed" };
            for id in &removed {
                println!("{verb} {id}");
            }
            if !dry_run {
                print_gc_hint(&home);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Gc { dry_run, json } => {
            let act = if dry_run {
                rewind_core::gc::Act::DryRun
            } else {
                rewind_core::gc::Act::Remove
            };
            let garbage = rewind_core::gc::collect(&home, act)?;
            if json {
                let images: Vec<serde_json::Value> = garbage
                    .images
                    .iter()
                    .map(|i| serde_json::json!({ "path": i.path, "bytes": i.bytes }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "images": images,
                        "source_caches": garbage.source_caches,
                        "pages": garbage.pages.pages,
                        "page_bytes": garbage.pages.bytes,
                        "bytes": garbage.bytes(),
                    })
                );
                return Ok(ExitCode::SUCCESS);
            }
            let (verb, freed) = if dry_run {
                ("would remove", "would free")
            } else {
                ("removed", "freed")
            };
            for image in &garbage.images {
                println!(
                    "{verb} {} ({})",
                    image.path.display(),
                    show::size(image.bytes)
                );
            }
            for cache in &garbage.source_caches {
                println!("{verb} {}", cache.display());
            }
            println!(
                "{verb} {} pages ({})",
                garbage.pages.pages,
                show::size(garbage.pages.bytes)
            );
            if garbage.unreadable_keyframes > 0 {
                eprintln!(
                    "rewind: {} keyframes do not read back, as from another build of rewind; \
                     the pages only they name were not kept",
                    garbage.unreadable_keyframes
                );
            }
            println!("{freed} {}", show::size(garbage.bytes()));
            Ok(ExitCode::SUCCESS)
        }
        Command::Cat {
            run,
            step,
            path,
            pid,
        } => {
            let run = Run::find(&home, &run)?;
            let step = run.check_step(step)?;
            match rewind_core::inspect::cat(&home, &run, step, pid, &path)? {
                Inspection::Contents(bytes) => {
                    use std::io::Write;
                    std::io::stdout().write_all(&bytes)?;
                    Ok(ExitCode::SUCCESS)
                }
                Inspection::NotFound(message) => {
                    eprintln!("rewind: {message}");
                    Ok(ExitCode::from(CAT_NOT_FOUND))
                }
                Inspection::Failed(message) => bail!("reading {path} at step {step}: {message}"),
            }
        }
        Command::Shell {
            run,
            step,
            pid,
            with,
        } => {
            use std::io::Write;
            use std::os::fd::AsRawFd;

            let run = Run::find(&home, &run)?;
            let step = run.check_step(step)?;

            // A pid is checked against the run: one it never had is a
            // mistake, and one gone by the step falls back to its parent.
            if let Some(pid) = pid {
                let procs = run.trace()?.processes();
                let Some(p) = procs.iter().rev().find(|p| p.pid == pid) else {
                    bail!(
                        "run {} has no process {pid}; see `rewind ps`",
                        run.manifest.id
                    );
                };
                if !p.alive_at(step) {
                    eprintln!(
                        "rewind: pid {pid} is not running at step {step}; the shell takes \
                         the view of its nearest running ancestor"
                    );
                }
            }
            // KVM, then the packages, while Ctrl-C still stops a slow Nix
            // build, then the terminal raw for the shell.
            rewind_vmm::kvm::open()?;
            let extras = if with.is_empty() {
                None
            } else {
                Some(rewind_core::inspect::Extras::build(&home, &with)?)
            };
            let size =
                terminal::size(std::io::stdout().as_raw_fd()).unwrap_or(terminal::DEFAULT_SIZE);
            eprintln!(
                "rewind: a shell at step {step} of {}; exit it to leave",
                run.manifest.id
            );
            terminal::wake_on_resize();
            let raw = terminal::RawMode::enter();
            let result = rewind_core::inspect::shell(
                &home,
                &run,
                step,
                rewind_core::inspect::Session { pid, size, extras },
                Box::new(terminal::Keyboard::new(size)),
                Box::new({
                    // SAFETY: isatty only inspects the descriptor.
                    let tty = unsafe { libc::isatty(std::io::stdout().as_raw_fd()) } == 1;
                    let mut plain = terminal::Unterminal::default();
                    move |bytes: &[u8]| {
                        let mut out = std::io::stdout().lock();
                        let _ = if tty {
                            out.write_all(bytes)
                        } else {
                            out.write_all(&plain.convert(bytes))
                        };
                        let _ = out.flush();
                    }
                }),
            );
            drop(raw);
            result?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Gdb {
            run,
            step,
            pid,
            tid,
            frame,
            listen,
            gdb_args,
        } => {
            let run = Run::find(&home, &run)?;
            let step = run.check_step(step)?;
            let start = gdb::Start { pid, tid, frame };
            gdb::gdb(&home, &run, step, start, listen.as_deref(), &gdb_args)
        }
        Command::Where {
            run,
            step,
            pid,
            tid,
            frames,
            json,
        } => {
            let run = Run::find(&home, &run)?;
            let step = run.check_step(step)?;
            let format = if json {
                locate::Format::Json
            } else {
                locate::Format::Text
            };
            locate::locate(&home, &run, step, (pid, tid), frames, format)
        }
        Command::Pmu { action } => {
            let vendor = rewind_core::pmu::Vendor::detect()?;
            match action {
                PmuAction::Enable => pmu_enable(),
                PmuAction::Status => {
                    println!("cpu: {vendor}");
                    let needs = vendor.needs_workaround();
                    let known = needs && rewind_core::pmu::workaround_known();
                    if needs {
                        let state = match (rewind_core::pmu::workaround_set(), known) {
                            (Some(true), _) => "set",
                            (Some(false), _) => "not set",
                            (None, true) => "set by `rewind pmu enable` this boot",
                            (None, false) => "not known to be set this boot",
                        };
                        println!("amd workaround (MSR 0xc0011020 bit 54): {state}");
                    }
                    if let Some(p) = rewind_core::pmu::perf_event_paranoid() {
                        println!("perf_event_paranoid: {p}");
                    }
                    let guest = Guest::from_env()?;
                    let t = rewind_core::pmu::selftest(&guest)?;
                    rewind_core::pmu::remember(&home, known, t.exact)?;
                    let usable = rewind_core::pmu::counter_usable(needs, known, t.exact);
                    println!(
                        "self-test: {} and {} branches, {}",
                        t.counts[0],
                        t.counts[1],
                        if t.exact {
                            "exact at every exit"
                        } else {
                            "NOT exact"
                        }
                    );
                    // On AMD an exact self-test without the workaround is
                    // luck, and says so.
                    if t.exact && !usable {
                        println!(
                            "the self-test agreed this time, but without the workaround this \
                             CPU's counter is not reliably exact; set it with `sudo rewind pmu enable`"
                        );
                    }
                    if usable {
                        println!("runs will use counter time");
                    } else {
                        println!(
                            "runs will use exit time; see {}",
                            rewind_core::pmu::DOCS_URL
                        );
                    }
                    Ok(if usable {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    })
                }
            }
        }
        Command::Export {
            run,
            output,
            replayable,
        } => {
            let run = Run::find(&home, &run)?;
            let out = output.unwrap_or_else(|| PathBuf::from(format!("{}.rwd", run.manifest.id)));
            let contents = if replayable {
                export::Contents::Replayable
            } else {
                export::Contents::View
            };
            export::export(&home, &run, contents, &out)?;
            let size = std::fs::metadata(&out)?.len();
            eprintln!("rewind: wrote {} ({})", out.display(), show::size(size));
            Ok(ExitCode::SUCCESS)
        }
        Command::Import { file, json } => {
            let run = if export::is_url(&file) {
                eprintln!("rewind: downloading {file}");
                export::import_url(&home, &file)?
            } else {
                export::import(&home, std::path::Path::new(&file))?
            };
            if json {
                let replayable = run.has_keyframes();
                println!(
                    "{}",
                    serde_json::json!({
                        "id": run.manifest.id,
                        "dir": run.dir,
                        "replayable": replayable,
                    })
                );
                return Ok(ExitCode::SUCCESS);
            }
            println!("{}", show::summary(&run));
            Ok(ExitCode::SUCCESS)
        }
        Command::Ls {
            limit,
            name,
            status,
            forks_of,
            since,
            json,
        } => {
            // Every run and every unreadable one, in one list, newest first.
            let listing = Run::list_all(&home)?;
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
                Some(run) => Some(list::forks_of(
                    &entries,
                    &Run::find(&home, run)?.manifest.id,
                )),
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
        Command::Show { run, json } => {
            let run = Run::find(&home, &run)?;

            // The run and the runs it was forked from, oldest first, as far
            // back as this home has them.
            let mut chain = vec![run.manifest.clone()];
            let mut gone = None;
            while let Some((parent, _)) = chain.last().and_then(|m| m.parent.clone()) {
                match Run::find(&home, &parent) {
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
                eprintln!(
                    "rewind: run {parent}, which the first of these forks, is not in this home"
                );
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
        Command::Log { run, step, steps } => {
            let run = Run::find(&home, &run)?;
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
        Command::Ps {
            run,
            step,
            all,
            json,
        } => {
            let run = Run::find(&home, &run)?;
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
        Command::Events {
            run,
            from,
            to,
            json,
        } => {
            let run = Run::find(&home, &run)?;
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
        Command::Replay {
            run,
            from: Some(step),
            json,
        } => {
            let run = Run::find(&home, &run)?;
            let step = run.check_step(step)?;
            let started = std::time::Instant::now();
            let (kf, original, again) = run.replay_from(&home, step)?;
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
        Command::Replay {
            run,
            from: None,
            json,
        } => {
            let run = Run::find(&home, &run)?;
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
        Command::Generate { what } => generate(what),
        Command::Open { run, step, compare } => {
            use std::os::unix::process::CommandExt;
            let run = Run::find(&home, &run)?;
            let step = step.map(|s| run.check_step(s)).transpose()?;
            let compare = compare.map(|c| Run::find(&home, &c)).transpose()?;
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
        Command::Doctor => {
            println!("rewind {}", rewind_core::VERSION);
            let found = doctor::findings(&home);
            for finding in &found {
                println!("{}", finding.line());
            }
            let failed = found.iter().any(|f| f.level == doctor::Level::Fail);
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Diff { left, right, json } => {
            let left = Run::find(&home, &left)?;
            let right = Run::find(&home, &right)?;
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

/// `rewind generate`: completions on standard output, or man pages in a
/// directory.
fn generate(what: Generate) -> Result<ExitCode> {
    match what {
        Generate::Completions { shell } => completions(shell, &mut std::io::stdout()),
        Generate::Man { dir } => {
            std::fs::create_dir_all(&dir)?;
            clap_mangen::generate_to(Cli::command(), &dir)
                .with_context(|| format!("writing man pages to {}", dir.display()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shell`'s completions for every command and option, written to `out`.
fn completions(shell: clap_complete::Shell, out: &mut dyn std::io::Write) {
    clap_complete::generate(shell, &mut Cli::command(), "rewind", out);
}

/// After runs are removed, says how much of the image cache `rewind gc`
/// would free, which takes reading every manifest. Counting the pages too
/// would take reading every keyframe and the whole page index, which is
/// too slow to do after each removal, so the line only mentions them. A
/// failure to count is not the removal's, and prints nothing.
fn print_gc_hint(home: &Home) {
    let Ok(images) = rewind_core::gc::unused_images(home) else {
        return;
    };
    let bytes: u64 = images.iter().map(|i| i.bytes).sum();
    if bytes == 0 {
        eprintln!("rewind: `rewind gc` removes the pages no run uses any more");
        return;
    }
    eprintln!(
        "rewind: images no run uses take {}; `rewind gc` removes them and the pages no run uses",
        show::size(bytes)
    );
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
    if !o.stop.starts_with(rewind_core::run::TIMED_OUT) {
        return;
    }
    let stop = locate::describe_stall(home, run).unwrap_or_else(|| o.stop.clone());
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
    let (status, hashed) = show::outcome_key(run)?;
    let outputs: Vec<(String, String)> = hashed
        .iter()
        .filter_map(|h| h.split_once(' '))
        .map(|(path, hash)| (path.to_string(), hash.to_string()))
        .collect();

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
        show::missing_outputs(&run.manifest.spec.job.outputs, &hashed)
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

/// `rewind pmu enable`: sets the AMD branch counter workaround on every
/// CPU, until reboot, where the CPU needs it.
fn pmu_enable() -> Result<ExitCode> {
    let vendor = rewind_core::pmu::Vendor::detect()?;
    if !vendor.needs_workaround() {
        println!("{vendor}: no workaround needed");
        return Ok(ExitCode::SUCCESS);
    }
    let n = rewind_core::pmu::enable_workaround()?;
    println!("set the branch counter workaround on {n} CPUs, until reboot");
    Ok(ExitCode::SUCCESS)
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

#[cfg(test)]
mod tests {
    // Command lines parsed the way the shell would hand them over, without
    // running anything.
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

    #[test]
    fn completions_and_man_pages_cover_every_command() {
        // The man pages are one for rewind and one for each command it
        // shows; bash's completions name a command.
        let dir = std::env::temp_dir().join(format!("rewind-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        generate(Generate::Man { dir: dir.clone() }).unwrap();
        for page in ["rewind.1", "rewind-fork.1", "rewind-show.1"] {
            assert!(dir.join(page).exists(), "{page}");
        }
        assert!(!dir.join("rewind-generate.1").exists());
        std::fs::remove_dir_all(&dir).unwrap();

        let mut bash = Vec::new();
        completions(clap_complete::Shell::Bash, &mut bash);
        assert!(String::from_utf8(bash).unwrap().contains("fork"));
    }

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

    #[test]
    fn a_step_follows_the_run_in_every_command() {
        // Each command that looks at one step takes it as the argument
        // after the run, optional where the run's end is a default; the
        // range of `events` and the start of `replay` stay options.
        let parses = |line: &str| Cli::try_parse_from(line.split_whitespace()).is_ok();
        for line in [
            "rewind fork abc 5",
            "rewind cat abc 5 /etc/hosts",
            "rewind shell abc 5",
            "rewind gdb abc 5",
            "rewind where abc 5",
            "rewind log abc 5",
            "rewind log abc",
            "rewind ps abc 5",
            "rewind ps abc",
            "rewind events abc --from 1 --to 5",
            "rewind replay abc --from 5",
        ] {
            assert!(parses(line), "{line}");
        }
        assert!(!parses("rewind ps abc --at 5"));
        assert!(!parses("rewind log abc --at 5"));
    }

    #[test]
    fn commands_that_boot_a_run_or_read_pages_hold_the_home() {
        // Every command that packs an image, boots a run's image or opens
        // the page store holds the home in use, so `rewind gc` waits for
        // it; the ones that read only manifests and traces, and gc itself,
        // do not.
        let hold = |line: &str| {
            let cli = Cli::try_parse_from(line.split_whitespace()).unwrap();
            cli.command.home_hold()
        };
        for line in [
            "rewind run --root r -- true",
            "rewind nix nixpkgs#hello",
            "rewind check nixpkgs#hello",
            "rewind fork abc 5 --schedule 2",
            "rewind cat abc 5 /etc/hosts",
            "rewind shell abc 5",
            "rewind gdb abc 5",
            "rewind where abc 5",
            "rewind replay abc",
            "rewind export abc",
            "rewind import a.rwd",
        ] {
            assert_eq!(hold(line), HomeHold::InUse, "{line}");
        }
        for line in [
            "rewind ls",
            "rewind log abc",
            "rewind ps abc",
            "rewind events abc",
            "rewind diff abc def",
            "rewind prune abc --identical",
            "rewind remove abc",
            "rewind gc",
            "rewind pmu status",
            "rewind doctor",
        ] {
            assert_eq!(hold(line), HomeHold::Unheld, "{line}");
        }
    }
}
