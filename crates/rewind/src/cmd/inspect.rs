//! The `rewind` subcommands that look inside a run at a step: a file, a
//! shell, gdb, the line a thread was on, and the threads that held the
//! CPU over a window of steps.

use std::process::ExitCode;

use anyhow::{Result, bail};
use rewind_core::inspect::{Inspection, Sent};
use rewind_core::threads::{OnTheCpu, Slice};
use rewind_core::tree::{Nar, Tar, UNNAMED_ROOT};
use rewind_core::{Home, Run};
use rewind_init::{Listed, ListedAll, ListedKind, Listing};

use crate::{RUN_HELP, RUN_LONG_HELP, STEP_HELP, STEP_LONG_HELP, gdb, locate, terminal};

/// The arguments of `rewind cat`.
#[derive(clap::Args)]
pub(crate) struct CatArgs {
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
    /// Write the path whole as an archive on standard output, in place
    /// of a file's bytes: the file, the link or the directory with
    /// everything below it, however deep.
    ///
    /// A tar holds the tree under the path's last name, as `tar -c -C
    /// /nix/store name` would, each entry with the permissions, the
    /// numeric owner and group and the modification time the VM held, so
    /// `tar -x` puts the tree back. A nar is Nix's archive of the tree:
    /// names, contents, executable bits and links only, and the bytes
    /// whose SHA-256 is a store path's NAR hash. Either is the same bytes
    /// each time, on any machine. Links are written, not followed, a
    /// directory of /proc or /sys below the path is written empty, as
    /// `rewind files` lists one, and a device, a socket or a pipe is left
    /// out of a tar and refused by a nar.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub(crate) archive: Option<ArchiveFormat>,
}

/// The archives `rewind cat --archive` writes.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveFormat {
    /// A GNU tar, with permissions, owners and times.
    Tar,
    /// A Nix archive, with executable bits only.
    Nar,
}

pub(crate) fn cat(home: &Home, args: CatArgs) -> Result<ExitCode> {
    let CatArgs {
        run,
        step,
        path,
        pid,
        archive,
    } = args;
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    warn_of_machine(&run, step);
    if let Some(format) = archive {
        return cat_archive(home, &run, step, pid, &path, format);
    }
    match rewind_core::inspect::cat(home, &run, step, pid, &path)? {
        Inspection::Contents(bytes) => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
            Ok(ExitCode::SUCCESS)
        }
        Inspection::NotFound(message) => {
            eprintln!("{} {message}", crate::color::stderr().prefix());
            Ok(ExitCode::from(CAT_NOT_FOUND))
        }
        Inspection::Failed(message) if names_a_directory(&message) => {
            eprintln!(
                "{} {path} is a directory at step {step}; `rewind files` lists it, and \
                 --archive writes it whole",
                crate::color::stderr().prefix()
            );
            Ok(ExitCode::from(CAT_DIRECTORY))
        }
        Inspection::Failed(message) => bail!("reading {path} at step {step}: {message}"),
    }
}

/// `rewind cat --archive`: the tree at `path` on standard output as
/// `format` says, written as the VM sends it.
fn cat_archive(
    home: &Home,
    run: &Run,
    step: u64,
    pid: Option<u32>,
    path: &str,
    format: ArchiveFormat,
) -> Result<ExitCode> {
    use rewind_core::inspect::tree;

    let paint = crate::color::stderr();
    let out = std::io::BufWriter::new(std::io::stdout().lock());

    // Each format's archive, ended only when the VM sent a whole tree: a
    // tar cut short has no end mark, and tar says so when it unpacks one.
    let sent = match format {
        ArchiveFormat::Tar => {
            let mut tar = Tar::new(out, root_name(path).as_bytes());
            let sent = tree(home, run, step, pid, path, &mut tar)?;
            if sent == Sent::Whole
                && let Some(left_out) = tar.finish()?
            {
                eprintln!(
                    "{} left out {} that tar cannot hold, a device, a socket or a pipe: {}",
                    paint.prefix(),
                    entries(left_out.count),
                    std::path::Path::new(path)
                        .join(String::from_utf8_lossy(&left_out.first).as_ref())
                        .display()
                );
            }
            sent
        }
        ArchiveFormat::Nar => {
            let mut nar = Nar::new(out)?;
            let sent = tree(home, run, step, pid, path, &mut nar)?;
            if sent == Sent::Whole {
                nar.finish()?;
            }
            sent
        }
    };

    match sent {
        Sent::Whole => Ok(ExitCode::SUCCESS),
        Sent::NotFound(message) => {
            eprintln!("{} {message}", paint.prefix());
            Ok(ExitCode::from(CAT_NOT_FOUND))
        }
        Sent::Failed(message) if message.starts_with(UNKNOWN_REQUEST) => bail!(
            "run {} was recorded with a guest from before `rewind cat --archive`; record it \
             again to archive its files",
            run.manifest.id
        ),
        Sent::Failed(message) => bail!("archiving {path} at step {step}: {message}"),
    }
}

/// The name a tar gives the tree at `path`: the path's last name, or
/// UNNAMED_ROOT for a path that ends in none, as `/` and `..` do.
fn root_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or(UNNAMED_ROOT.to_string(), |name| {
            name.to_string_lossy().into_owned()
        })
}

/// A count of entries, in words.
fn entries(count: usize) -> String {
    match count {
        1 => "1 entry".to_string(),
        n => format!("{n} entries"),
    }
}

/// Whether `message`, the VM's reason a file could not be read, says the
/// path names a directory. The guest opens the path and reads it, and
/// reports a failed read as the path and the operating system's error.
fn names_a_directory(message: &str) -> bool {
    ends_with_os_error(message, libc::EISDIR)
}

/// Whether `message`, the VM's reason a directory could not be listed,
/// says the path names something other than a directory.
fn names_no_directory(message: &str) -> bool {
    ends_with_os_error(message, libc::ENOTDIR)
}

/// Whether `message` is the guest's report of the operating system's
/// error number `errno`: the guest ends such a report with the number.
fn ends_with_os_error(message: &str, errno: i32) -> bool {
    let number = format!("(os error {errno})");
    message.trim_end().ends_with(&number)
}

/// The arguments of `rewind files`.
#[derive(clap::Args)]
pub(crate) struct FilesArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    #[arg(help = STEP_HELP, long_help = STEP_LONG_HELP)]
    pub(crate) step: u64,
    /// The directory's path, absolute or relative to the process's
    /// working directory.
    pub(crate) path: String,
    /// Resolve the path as this process saw it, in its root and working
    /// directory.
    ///
    /// By default, and once it has exited, the job's.
    #[arg(long)]
    pub(crate) pid: Option<u32>,
    /// Print one JSON object a line per entry, for programs such as the
    /// desktop app: its `path` below the directory, its `kind` (`file`,
    /// `directory`, `symlink` or `other`), its `mode`, its `size` in
    /// bytes, and a link's `target`. A listing that was cut ends with a
    /// line `{"cut":true}`.
    #[arg(long)]
    pub(crate) json: bool,
}

pub(crate) fn files(home: &Home, args: FilesArgs) -> Result<ExitCode> {
    let FilesArgs {
        run,
        step,
        path,
        pid,
        json,
    } = args;
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    warn_of_machine(&run, step);
    let paint = crate::color::stderr();
    let bytes = match rewind_core::inspect::list(home, &run, step, pid, &path)? {
        Inspection::Contents(bytes) => bytes,
        Inspection::NotFound(message) => {
            eprintln!("{} {message}", paint.prefix());
            return Ok(ExitCode::from(CAT_NOT_FOUND));
        }
        Inspection::Failed(message) if names_no_directory(&message) => {
            eprintln!(
                "{} {path} is not a directory at step {step}",
                paint.prefix()
            );
            return Ok(ExitCode::from(FILES_NOT_A_DIRECTORY));
        }
        Inspection::Failed(message) if message.starts_with(UNKNOWN_REQUEST) => bail!(
            "run {} was recorded with a guest from before `rewind files`; record it again to \
             list its directories",
            run.manifest.id
        ),
        Inspection::Failed(message) => bail!("listing {path} at step {step}: {message}"),
    };
    let Some(mut listing) = Listing::parse(&bytes) else {
        bail!("listing {path} at step {step}: the VM's answer is not a listing");
    };

    // By path, as `find` piped through `sort` prints a tree.
    listing.entries.sort_by(|a, b| a.path.cmp(&b.path));
    if json {
        print!("{}", listing.lines());
        return Ok(ExitCode::SUCCESS);
    }
    for entry in &listing.entries {
        println!("{}", listed_line(entry));
    }
    if listing.complete == ListedAll::No {
        eprintln!(
            "{} {path} holds more; these are the {} entries nearest it",
            paint.prefix(),
            listing.entries.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// How the guest's init starts its reason for refusing a request it does
/// not know, as a guest from before the request does.
const UNKNOWN_REQUEST: &str = "inspect: unknown request";

/// An entry as `rewind files` prints it: its kind and permissions as `ls
/// -l` writes them, its size, its path, and where a link points.
fn listed_line(entry: &Listed) -> String {
    /// The width sizes are padded to, which holds a file of gigabytes.
    const SIZE_WIDTH: usize = 11;

    let mode = mode_letters(entry.kind, entry.mode);
    let target = match &entry.target {
        Some(target) => format!(" -> {target}"),
        None => String::new(),
    };
    format!("{mode} {:>SIZE_WIDTH$}  {}{target}", entry.size, entry.path)
}

/// An entry's kind and permissions as `ls -l` writes them: a letter for
/// the kind, then read, write and execute for the owner, the group and
/// everyone else. Set-id and sticky bits are not shown.
fn mode_letters(kind: ListedKind, mode: u32) -> String {
    /// The letters of one class's permissions, and how many classes.
    const LETTERS: [char; 3] = ['r', 'w', 'x'];
    const CLASSES: u32 = 3;
    const ABSENT: char = '-';

    let kind = match kind {
        ListedKind::File => '-',
        ListedKind::Directory => 'd',
        ListedKind::Symlink => 'l',
        ListedKind::Other => '?',
    };
    let bits = CLASSES * LETTERS.len() as u32;
    let permissions = (0..bits).map(|i| {
        let set = mode & (1 << (bits - 1 - i)) != 0;
        if set {
            LETTERS[i as usize % LETTERS.len()]
        } else {
            ABSENT
        }
    });
    std::iter::once(kind).chain(permissions).collect()
}

/// The arguments of `rewind shell`.
#[derive(clap::Args)]
pub(crate) struct ShellArgs {
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

pub(crate) fn shell(home: &Home, args: ShellArgs) -> Result<ExitCode> {
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
    warn_of_machine(&run, step);

    // A pid is checked against the run: one it never had is a
    // mistake, and one gone by the step falls back to its parent.
    if let Some(pid) = pid {
        let Some(p) = run.trace()?.process_at(pid, step) else {
            bail!(
                "run {} has no process {pid}; see `rewind ps`",
                run.manifest.id
            );
        };
        if !p.alive_at(step) {
            eprintln!(
                "{} pid {pid} is not running at step {step}; the shell takes \
                     the view of its nearest running ancestor",
                crate::color::stderr().prefix()
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
        "{} a shell at step {step} of {}; exit it to leave",
        crate::color::stderr().prefix(),
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
pub(crate) struct GdbArgs {
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

pub(crate) fn gdb(home: &Home, args: GdbArgs) -> Result<ExitCode> {
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
    warn_of_machine(&run, step);
    let start = gdb::Start { pid, tid, frame };
    gdb::gdb(home, &run, step, start, listen.as_deref(), &gdb_args)
}

/// The arguments of `rewind where`.
#[derive(clap::Args)]
pub(crate) struct WhereArgs {
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
    /// Look just after the process writes the --size bytes at this
    /// address in the step, its --hit'th write of them there, rather than
    /// at the step's start, which can be lines of the program before: the
    /// write `rewind blame --at` names. The chosen frame's line is the
    /// writing instruction's.
    #[arg(
        long,
        value_name = "ADDRESS",
        requires = "size",
        conflicts_with = "after_access"
    )]
    pub(crate) after_write: Option<String>,
    /// As --after-write, after any access, read or write: the --hit'th
    /// in the step.
    #[arg(long, value_name = "ADDRESS", requires = "size")]
    pub(crate) after_access: Option<String>,
    /// How many bytes --after-write or --after-access names.
    #[arg(long)]
    pub(crate) size: Option<u64>,
    /// Which of the step's writes, or accesses, to look after, from 1,
    /// as `rewind blame --json` numbers them.
    #[arg(long, default_value_t = 1)]
    pub(crate) hit: u32,
}

pub(crate) fn r#where(home: &Home, args: WhereArgs) -> Result<ExitCode> {
    let WhereArgs {
        run,
        step,
        pid,
        tid,
        frames,
        json,
        after_write,
        after_access,
        size,
        hit,
    } = args;

    // The access to look just after, if one is named.
    let after = match (after_write, after_access) {
        (Some(address), _) => Some((address, rewind_core::accesses::Kind::Write)),
        (None, Some(address)) => Some((address, rewind_core::accesses::Kind::Read)),
        (None, None) => None,
    };
    let after = match after {
        Some((address, kind)) => {
            let Some(address) = super::blame_memory::parse_hex(&address) else {
                anyhow::bail!("not an address in hex: {address}");
            };
            let Some(len) = size else {
                anyhow::bail!("--size names how many bytes");
            };
            Some(rewind_core::accesses::Sought {
                value: rewind_core::accesses::Value { address, len },
                kind,
                hit,
            })
        }
        None => None,
    };
    let run = Run::find(home, &run)?;
    let step = run.check_step(step)?;
    warn_of_machine(&run, step);
    let format = if json {
        locate::Format::Json
    } else {
        locate::Format::Text
    };
    locate::locate(home, &run, step, (pid, tid), after, frames, format)
}

/// `rewind cat`'s exit status when the file did not exist at the step:
/// apart from 1, which any failure exits with, and 2, clap's for a
/// command line it refused.
const CAT_NOT_FOUND: u8 = 3;

/// `rewind cat`'s exit status when the path named a directory at the
/// step, which has no bytes to print.
const CAT_DIRECTORY: u8 = 4;

/// `rewind files`' exit status when the path named something other than
/// a directory at the step. A missing directory exits as a missing file
/// does from `rewind cat`.
const FILES_NOT_A_DIRECTORY: u8 = 4;

/// How many callers of the chosen frame `rewind where` shows.
const DEFAULT_CALLERS: usize = 2;

/// The arguments of `rewind threads`.
#[derive(clap::Args)]
pub(crate) struct ThreadsArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// The first step to look at; by default the step the job started on.
    #[arg(long)]
    pub(crate) from: Option<u64>,
    /// The last step to look at; by default the run's end.
    #[arg(long)]
    pub(crate) to: Option<u64>,
    /// Print one JSON object a line per slice, for programs such as the
    /// desktop app: its first and last step, what held the CPU (`thread`,
    /// `kernel` or `idle`), its pid and tid, and its name.
    #[arg(long)]
    pub(crate) json: bool,
}

pub(crate) fn threads(home: &Home, args: ThreadsArgs) -> Result<ExitCode> {
    let ThreadsArgs {
        run,
        from,
        to,
        json,
    } = args;
    let run = Run::find(home, &run)?;
    let from = match from {
        Some(step) => run.check_step(step)?,
        None => crate::show::start_step(run.trace()?),
    };
    let to = to.unwrap_or(u64::MAX);
    let slices = run.threads_through(from, to)?;

    // One line a slice: as JSON for programs, or as a table.
    for slice in &slices {
        if json {
            println!("{}", crate::json::slice(slice));
            continue;
        }
        println!("{}", slice_line(slice));
    }
    Ok(ExitCode::SUCCESS)
}

/// A slice as `rewind threads` prints it: its steps, then pid/tid and the
/// task's name, laid out as `rewind events` lays out an event's thread.
fn slice_line(slice: &Slice) -> String {
    let who = match &slice.on {
        OnTheCpu::Thread { pid, tid } => format!("{pid}/{tid}"),
        OnTheCpu::WithoutMemory { tid, .. } => format!("kernel {tid}"),
        OnTheCpu::Idle => "idle".to_string(),
    };
    format!(
        "{:>10} {:>10}  {who:>12}  {}",
        slice.from, slice.to, slice.name
    )
}

/// Says, before a look inside `run` at `step`, when a replay found the
/// machine different from the recording at a keyframe after the step and
/// the look replays the stretch where it changed.
pub(crate) fn warn_of_machine(run: &Run, step: u64) {
    let Some(warning) = run.machine_warning(step) else {
        return;
    };
    let paint = crate::color::stderr();
    eprintln!(
        "{} {}",
        paint.prefix(),
        paint.paint(crate::color::Style::Warn, warning)
    );
}

#[cfg(test)]
mod tests {
    // What `rewind cat` makes of the VM's answers, with messages as the
    // guest's init writes them and no VM.
    use super::*;

    #[test]
    fn a_read_that_failed_on_a_directory_is_told_apart() {
        // The guest's message for a path that names a directory ends with
        // the operating system's error for one; a failure for any other
        // reason does not.
        assert!(names_a_directory(
            "/nix/store/3ynx-shuffle-pgo: Is a directory (os error 21)"
        ));
        assert!(names_a_directory("/out: Is a directory (os error 21)\n"));
        assert!(!names_a_directory(
            "/build/secret: Permission denied (os error 13)"
        ));
        assert!(!names_a_directory("inspect: no process 21"));

        // A listing of a file fails with the error for no directory.
        assert!(names_no_directory(
            "/out/bin/sh: Not a directory (os error 20)"
        ));
        assert!(!names_no_directory("/out: Is a directory (os error 21)"));
    }

    #[test]
    fn an_entry_prints_as_ls_writes_it() {
        // An executable, a link and a directory: each line has the kind
        // and permissions as letters, the size, the path, and a link's
        // target.
        let entry = |path: &str, kind, mode, size, target: Option<&str>| Listed {
            path: path.into(),
            kind,
            mode,
            size,
            target: target.map(str::to_string),
        };
        assert_eq!(
            listed_line(&entry("bin/shuffle", ListedKind::File, 0o755, 18_504, None)),
            "-rwxr-xr-x       18504  bin/shuffle"
        );
        assert_eq!(
            listed_line(&entry(
                "bin/sh",
                ListedKind::Symlink,
                0o777,
                0,
                Some("shuffle")
            )),
            "lrwxrwxrwx           0  bin/sh -> shuffle"
        );
        assert_eq!(
            listed_line(&entry("share", ListedKind::Directory, 0o555, 0, None)),
            "dr-xr-xr-x           0  share"
        );
        assert_eq!(mode_letters(ListedKind::File, 0o640), "-rw-r-----");
    }

    #[test]
    fn a_tar_names_its_tree_by_the_path_s_last_name() {
        // An output's tar unpacks to the output's name, a file's to the
        // file's, and a path with no last name, as the root has none, to
        // the directory it is unpacked in.
        assert_eq!(root_name("/nix/store/3ynx-shuffle"), "3ynx-shuffle");
        assert_eq!(root_name("/out/"), "out");
        assert_eq!(root_name("notes.txt"), "notes.txt");
        assert_eq!(root_name("/"), UNNAMED_ROOT);
        assert_eq!(root_name(".."), UNNAMED_ROOT);
        assert_eq!(entries(1), "1 entry");
        assert_eq!(entries(3), "3 entries");
    }
}
