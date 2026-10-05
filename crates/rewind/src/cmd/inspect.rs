//! The `rewind` subcommands that look inside a run at a step: a file, a
//! shell, gdb, and the line a thread was on.

use std::process::ExitCode;

use anyhow::{Result, bail};
use rewind_core::inspect::Inspection;
use rewind_core::{Home, Run};

use crate::{RUN_HELP, RUN_LONG_HELP, STEP_HELP, STEP_LONG_HELP, gdb, locate, terminal};

/// The arguments of `rewind cat`.
#[derive(clap::Args)]
pub struct CatArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// The path, absolute or relative to the process's working
    /// directory.
    pub(crate) path: String,
    /// Resolve the path as this process saw it, in its root and working
    /// directory.
    ///
    /// By default, and once it has exited, the job's.
    #[arg(long)]
    pub(crate) pid: Option<u32>,
}

pub fn cat(home: &Home, args: CatArgs) -> Result<ExitCode> {
    let CatArgs {
        run,
        step,
        path,
        pid,
    } = args;
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    match rewind_core::inspect::cat(home, &run, step, pid, &path)? {
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

/// The arguments of `rewind shell`.
#[derive(clap::Args)]
pub struct ShellArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// The process whose root and working directory the shell starts in.
    ///
    /// By default, and once it has exited, the job's.
    #[arg(long)]
    pub(crate) pid: Option<u32>,
    /// More packages in the shell, from Nix: an installable such as
    /// nixpkgs#strace, built or fetched here, its closure visible in the
    /// VM's /nix/store and its bin directory first on PATH.
    ///
    /// Repeat for several.
    #[arg(long = "with", value_name = "INSTALLABLE")]
    pub(crate) with: Vec<String>,
}

pub fn shell(home: &Home, args: ShellArgs) -> Result<ExitCode> {
    let ShellArgs {
        run,
        step,
        pid,
        with,
    } = args;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let run = Run::find(home, &run)?;
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
        Some(rewind_core::inspect::Extras::build(home, &with)?)
    };
    let size = terminal::size(std::io::stdout().as_raw_fd()).unwrap_or(terminal::DEFAULT_SIZE);
    eprintln!(
        "rewind: a shell at step {step} of {}; exit it to leave",
        run.manifest.id
    );
    terminal::wake_on_resize();
    let raw = terminal::RawMode::enter();
    let result = rewind_core::inspect::shell(
        home,
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

/// The arguments of `rewind gdb`.
#[derive(clap::Args)]
pub struct GdbArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// Debug this process, whether or not it was running at the step:
    /// its symbols, its breakpoints and every one of its threads.
    #[arg(long)]
    pub(crate) pid: Option<u32>,
    /// Start in this thread; by default the step's event's, or the
    /// main thread of --pid.
    #[arg(long)]
    pub(crate) tid: Option<u32>,
    /// Start in this frame of the thread, as `rewind where --json`
    /// numbers them; by default the innermost.
    #[arg(long)]
    pub(crate) frame: Option<u32>,
    /// Serve on this address, such as 127.0.0.1:1234, and start no gdb.
    #[arg(long)]
    pub(crate) listen: Option<String>,
    /// More arguments for gdb, after `--`, such as -batch -ex bt.
    ///
    /// They come after the ones that load the symbols and connect.
    #[arg(last = true)]
    pub(crate) gdb_args: Vec<String>,
}

pub fn gdb(home: &Home, args: GdbArgs) -> Result<ExitCode> {
    let GdbArgs {
        run,
        step,
        pid,
        tid,
        frame,
        listen,
        gdb_args,
    } = args;
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    let start = gdb::Start { pid, tid, frame };
    gdb::gdb(home, &run, step, start, listen.as_deref(), &gdb_args)
}

/// The arguments of `rewind where`.
#[derive(clap::Args)]
pub struct WhereArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// Look at this process; by default the step's event's.
    #[arg(long)]
    pub(crate) pid: Option<u32>,
    /// Look at this thread; by default the step's event's, or the
    /// main thread of --pid.
    #[arg(long)]
    pub(crate) tid: Option<u32>,
    /// How many frames that called the chosen one to show.
    #[arg(long, default_value_t = DEFAULT_CALLERS)]
    pub(crate) frames: usize,
    /// Print every frame, the chosen one's index and each source file
    /// the frames are in, whole up to a mebibyte and the lines around
    /// the frames' lines past that, as one JSON object on standard
    /// output, for programs such as the desktop app.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn r#where(home: &Home, args: WhereArgs) -> Result<ExitCode> {
    let WhereArgs {
        run,
        step,
        pid,
        tid,
        frames,
        json,
    } = args;
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    let format = if json {
        locate::Format::Json
    } else {
        locate::Format::Text
    };
    locate::locate(home, &run, step, (pid, tid), frames, format)
}

/// `rewind cat`'s exit status when the file did not exist at the step:
/// apart from 1, which any failure exits with, and 2, clap's for a
/// command line it refused.
const CAT_NOT_FOUND: u8 = 3;

/// How many callers of the chosen frame `rewind where` shows.
const DEFAULT_CALLERS: usize = 2;
