//! The `rewind` command.

mod cmd;
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

use anyhow::Result;
use clap::{Parser, Subcommand};
use rewind_core::Home;

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

#[derive(Parser)]
#[command(
    version = rewind_core::VERSION,
    about = "Run Linux workloads in a deterministic VM, then scrub, rewind and fork them"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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

#[derive(Subcommand)]
enum Command {
    /// Run a command in a root filesystem.
    Run(cmd::record::RunArgs),
    /// Build a Nix derivation in the deterministic VM.
    Nix(cmd::record::NixArgs),
    /// Find a schedule that changes how a build or command ends, and where
    /// it went its own way.
    ///
    /// Runs a Nix derivation, or a command with --root, under several
    /// schedules, narrows the steps the first one that ends differently
    /// perturbs, and shows where it parts from the run that did not.
    Check(cmd::record::CheckArgs),
    /// Branch a run at a step: the same run up to the step, then another
    /// interleaving from there.
    Fork(cmd::record::ForkArgs),
    /// Remove forks of a run, and forks of those, that ran exactly as an
    /// older one did.
    ///
    /// The run itself stays, and so does any run another run here was forked
    /// from or reads keyframes from.
    Prune(cmd::remove::PruneArgs),
    /// Remove runs, with every run forked from them and from those.
    ///
    /// Their imported inputs go with them. Refused, removing nothing, while one of them has not finished or a run
    /// that stays reads keyframes from one of them. The images and pages they
    /// used stay until `rewind gc`. Many runs at once take one call: `xargs
    /// rewind remove < ids`. A run `rewind ls` lists as unreadable is named
    /// by its id.
    Remove(cmd::remove::RemoveArgs),
    /// Remove the images, pages and source files no run uses any more.
    ///
    /// These are the cached images no run names and the pages in the page
    /// store no keyframe names, which `remove` and `prune` leave behind, the
    /// source files cached for runs that are gone, and the debug info and
    /// sources `rewind gdb` sessions downloaded, which the next session
    /// fetches again as it needs. Refused, removing nothing, while another
    /// rewind process is packing an image, executing a run, has a shell
    /// open or has the page store open.
    Gc(cmd::remove::GcArgs),
    /// Print a file as it was at a step of a run.
    ///
    /// Rewind forks the run at the step and reads the file inside the VM, so
    /// this takes about as long as seeking there. Exits 3 when the file did
    /// not exist then.
    Cat(cmd::inspect::CatArgs),
    /// A shell inside the VM at a step of a run.
    ///
    /// It has the job's environment and starts in a process's root and
    /// working directory, while everything else in the VM stays stopped
    /// where it was. Nothing done in it changes the run: it happens in a
    /// throwaway fork.
    Shell(cmd::inspect::ShellArgs),
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
    Gdb(cmd::inspect::GdbArgs),
    /// Where in the program's own code a thread was at a step.
    ///
    /// Prints its innermost frame outside the kernel, the C library, Rust's
    /// standard library and dependencies, with its source, and the frames
    /// that called it. By default the thread of the step's own event, the pid and tid `rewind
    /// events` prints, and at a step with no event or the kernel's own, such
    /// as a console line, the thread on the CPU. Takes gdb with Python on
    /// PATH, and a run recorded with a kernel that lists its tasks.
    Where(cmd::inspect::WhereArgs),
    /// Which thread held the CPU at each step of a window of a run.
    ///
    /// Prints a line per slice of steps one task held: a thread of a
    /// process, a kernel thread, or the idle task. Rewind brings the run to
    /// the window's first step and takes it one step at a time, reading the
    /// task the VM's kernel has on the CPU, so a wide window takes a while.
    /// A step is an exit, so a thread that ran between two exits and gave
    /// the CPU back before the next is not seen.
    Threads(cmd::inspect::ThreadsArgs),
    /// Open a run in the desktop app, at a step and beside another run.
    ///
    /// Starts rewind-app, from PATH or REWIND_APP, on the run's directory.
    /// `rewind check` and `rewind fork` print the command that opens what
    /// they found.
    Open(cmd::view::OpenArgs),
    /// Whether this machine can record runs and look inside them.
    ///
    /// Checks KVM, the guest, which clock runs get, gdb and its Python, the
    /// debuginfod server, nix and mkfs.erofs, the disk the home takes, and
    /// runs whose manifests do not read, saying what to do about each
    /// problem. Exits 1 when runs cannot be recorded here.
    Doctor,
    /// Whether this host's performance counters can drive virtual time.
    Pmu(cmd::setup::PmuArgs),
    /// Write a run to a single .rwd file.
    Export(cmd::transfer::ExportArgs),
    /// Read a .rwd file into the local runs.
    Import(cmd::transfer::ImportArgs),
    /// List runs, newest first: each one's id, how it ended, its steps and
    /// its name.
    ///
    /// A run whose manifest another build of rewind wrote is listed as
    /// unreadable, with why; `rewind remove` takes it by id.
    Ls(cmd::view::LsArgs),
    /// The command that makes a run again, and for a fork its parents'.
    ///
    /// A run's id is the hash of its inputs, so the same command on a
    /// machine with the same CPU vendor and guest makes the same run. The
    /// commands name the epoch, which by default is the start of the day a
    /// run is made, so the same command a day later makes another run.
    Show(cmd::view::ShowArgs),
    /// Print a run's output, up to a step.
    Log(cmd::view::LogArgs),
    /// Show the processes alive at a step.
    Ps(cmd::view::PsArgs),
    /// Print a run's events.
    Events(cmd::view::EventsArgs),
    /// Run a run's inputs again and check the trace comes out identical.
    Replay(cmd::view::ReplayArgs),
    /// Shell completions and man pages, which the packages install.
    #[command(hide = true)]
    Generate(cmd::setup::GenerateArgs),
    /// Compare two runs and show where they first differ.
    ///
    /// Exits 0 when the runs are identical and 1 when they differ.
    Diff(cmd::view::DiffArgs),
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
            Command::Run(_)
            | Command::Nix(_)
            | Command::Check(_)
            | Command::Fork(_)
            | Command::Cat(_)
            | Command::Shell(_)
            | Command::Gdb(_)
            | Command::Where(_)
            | Command::Threads(_)
            | Command::Replay(_)
            | Command::Export(_)
            | Command::Import(_) => HomeHold::InUse,
            Command::Prune(_)
            | Command::Remove(_)
            | Command::Gc(_)
            | Command::Pmu(_)
            | Command::Open(_)
            | Command::Doctor
            | Command::Ls(_)
            | Command::Show(_)
            | Command::Log(_)
            | Command::Ps(_)
            | Command::Events(_)
            | Command::Diff(_)
            | Command::Generate(_) => HomeHold::Unheld,
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
    if let Command::Pmu(cmd::setup::PmuArgs {
        action: PmuAction::Enable,
    }) = cli.command
    {
        return cmd::setup::pmu_enable();
    }

    // Completions and man pages need no runs, and a package's build has no
    // home to find them under.
    let command = match cli.command {
        Command::Generate(args) => return cmd::setup::generate(args),
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
        Command::Run(args) => cmd::record::run_command(&home, args),
        Command::Nix(args) => cmd::record::nix(&home, args),
        Command::Check(args) => cmd::record::check(&home, args),
        Command::Fork(args) => cmd::record::fork(&home, args),
        Command::Prune(args) => cmd::remove::prune(&home, args),
        Command::Remove(args) => cmd::remove::remove(&home, args),
        Command::Gc(args) => cmd::remove::gc(&home, args),
        Command::Cat(args) => cmd::inspect::cat(&home, args),
        Command::Shell(args) => cmd::inspect::shell(&home, args),
        Command::Gdb(args) => cmd::inspect::gdb(&home, args),
        Command::Where(args) => cmd::inspect::r#where(&home, args),
        Command::Threads(args) => cmd::inspect::threads(&home, args),
        Command::Open(args) => cmd::view::open(&home, args),
        Command::Doctor => cmd::setup::doctor(&home),
        Command::Pmu(args) => cmd::setup::pmu(&home, args),
        Command::Export(args) => cmd::transfer::export(&home, args),
        Command::Import(args) => cmd::transfer::import(&home, args),
        Command::Ls(args) => cmd::view::ls(&home, args),
        Command::Show(args) => cmd::view::show(&home, args),
        Command::Log(args) => cmd::view::log(&home, args),
        Command::Ps(args) => cmd::view::ps(&home, args),
        Command::Events(args) => cmd::view::events(&home, args),
        Command::Replay(args) => cmd::view::replay(&home, args),
        Command::Generate(args) => cmd::setup::generate(args),
        Command::Diff(args) => cmd::view::diff(&home, args),
    }
}

#[cfg(test)]
mod tests {
    // Command lines parsed the way the shell would hand them over, without
    // running anything.
    use super::*;

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
