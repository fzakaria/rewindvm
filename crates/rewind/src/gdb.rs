//! `rewind gdb`: gdb on a fork of a run at a step, with every symbol this
//! machine can find for what was running there.
//!
//! The kernel's symbols come from its package: vmlinux with its symbol
//! table next to the bzImage, and its DWARF in the package's `debug`
//! output, fetched the first time someone debugs. The process that was
//! running at the step is found by an inspection on a second fork, and
//! each program and library it had mapped is loaded at the address it was
//! loaded at in the VM: from this machine's store, or, for files only the
//! VM has, from copies the inspection sends, with the source files a third
//! fork reads for them, or the run's source cache keeps from an earlier
//! lookup. A store file this machine lacks, or a program too large for the
//! VM to send, is read out of the run's input image.
//!
//! DWARF and source files for all of them come from a debuginfod server
//! started for the session, nixseparatedebuginfod2, which serves the
//! `debug` outputs in the local store and on cache.nixos.org, with the
//! source files they were built from. A store program that keeps its
//! DWARF, with no `debug` output, names source files in the build's
//! directory; those are found in its derivation's src.

use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use rewind_core::inspect::Inspection;
use rewind_core::maps::{ImageMount, Origin, Running, SymbolFile};
use rewind_core::source_cache::{self, Entry, SourceCache, Version};
use rewind_core::{Home, Run};

use crate::downloads::{self, Downloads, FileNames};

/// The address `rewind gdb` serves on when it starts gdb itself: any free
/// port on the loopback interface.
pub const GDB_LOCAL: &str = "127.0.0.1:0";

/// Where Nix keeps store paths, and the program that fetches them.
const NIX_STORE: &str = "/nix/store";
const NIX_STORE_PROGRAM: &str = "nix-store";

/// The kernel package's files: vmlinux with its symbol table and the
/// kernel's gdb scripts, next to the bzImage; in the `debug` output,
/// vmlinux's DWARF, linked by name as well as by build ID.
const VMLINUX: &str = "vmlinux";
const GDB_SCRIPTS: &str = "vmlinux-gdb.py";
const DEBUG_VMLINUX: &str = "lib/debug/vmlinux";
const DEBUG_BUILD_IDS: &str = "lib/debug/.build-id";

/// The release asset with the kernel's DWARF, for hosts without Nix.
const DEBUG_RELEASE: &str = "rewind-debug-x86_64-linux.tar.gz";

/// gdb's switch for the Python scripts it loads from next to a symbol
/// file, such as vmlinux-gdb.py next to vmlinux.
const AUTO_LOAD_OFF: &str = "set auto-load python-scripts off";
const AUTO_LOAD_ON: &str = "set auto-load python-scripts on";

/// In the `debug` output, the files the Rewind patch adds or changes, in
/// a directory named for the source tree. nixseparatedebuginfod2 serves
/// the source tarball's files, but takes a patched file only in place of
/// one the tarball has, so the files the patch adds are given to gdb
/// directly, on its source path.
const DEBUG_SOURCES: &str = "src/overlay";

/// The debuginfod server, named by the Nix package without depending on
/// it, and looked for on PATH otherwise.
const ENV_DEBUGINFOD: &str = "REWIND_DEBUGINFOD";
const DEBUGINFOD_PROGRAM: &str = "nixseparatedebuginfod2";

/// Where the server looks for `debug` outputs, and how long it keeps what
/// it downloads.
const DEBUGINFOD_SUBSTITUTERS: &[&str] = &["local:", "https://cache.nixos.org"];
const DEBUGINFOD_EXPIRATION: &str = "1 day";

/// Where gdb sessions keep the files only the VM has, under Rewind's
/// data directory, one directory per session.
const SESSIONS_DIR: &str = "gdb";

/// The servers gdb asks besides the session's own.
const ENV_DEBUGINFOD_URLS: &str = "DEBUGINFOD_URLS";

/// The first descriptor systemd's socket activation hands a server, the
/// way the session's server gets its listening socket.
const LISTEN_FD: i32 = 3;

/// Serves gdb on a fork of `run` at `step`, debugging process `pid`, or
/// when None the process running at the step: on `listen` if given, else
/// on a free local port with the host's gdb started against it, with
/// `extra` after the arguments this makes. Ctrl-C belongs to gdb, which
/// turns it into an interrupt for the fork, so this process ignores it
/// meanwhile.
pub fn gdb(
    home: &Home,
    run: &Run,
    step: u64,
    pid: Option<u32>,
    listen: Option<&str>,
    extra: &[String],
) -> Result<ExitCode> {
    // What gdb is told before the fork it debugs is made: the inspection
    // that finds the process needs a fork of its own.
    let symbols = Symbols::load(home, run, step, pid, Kernel::Load, Say::Aloud)?;

    // Finding a process that is not on the CPU takes the kernel's task
    // list, which kernels before it do not publish.
    let needs = match pid {
        Some(pid) => Needs::Tasks(format!("--pid cannot find process {pid}")),
        None => Needs::Nothing,
    };
    let mut debuggee = fork(home, run, step, symbols.process.scope, needs)?;
    let listener = TcpListener::bind(listen.unwrap_or(GDB_LOCAL)).context("listening for gdb")?;
    let mut args = symbols.arguments(Some(listener.local_addr()?));
    args.extend(extra.iter().cloned());

    // Only serving: say how to connect, then wait for gdb. The debuginfod
    // server runs for as long as this does.
    if listen.is_some() {
        let shown: Vec<String> = args.iter().map(|a| quote(a)).collect();
        eprintln!(
            "rewind: gdb at step {step} of {}; connect with: gdb {}",
            run.manifest.id,
            shown.join(" ")
        );
        let (conn, _) = listener.accept()?;
        debuggee.serve(conn)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Starting gdb: the fork is served from this thread while gdb owns the
    // terminal.
    // SAFETY: ignoring SIGINT has no preconditions; gdb installs its own.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    eprintln!("rewind: gdb at step {step} of {}", run.manifest.id);
    let (status, _) = run_gdb(&args, Some((&mut debuggee, &listener)), Output::Terminal)?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Whether a session loads the VM kernel's symbols: gdb on a fork does,
/// while a question about a process's own code needs only the process's,
/// and the kernel's DWARF is slow to load and may need fetching first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kernel {
    Load,
    Skip,
}

/// Whether loading symbols says on stderr what it found: for a person
/// starting gdb, or not, when the symbols only name an address in another
/// message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Say {
    Aloud,
    Nothing,
}

impl Say {
    /// Prints `text` on its own line after rewind's prefix, when aloud.
    fn line(self, text: impl std::fmt::Display) {
        if self == Say::Aloud {
            eprintln!("rewind: {text}");
        }
    }
}

/// What a fork for gdb needs of the run's kernel.
pub enum Needs {
    /// Nothing more than any kernel gives.
    Nothing,
    /// Its task list, without which the thing named cannot be done:
    /// kernels before the task layout was published do not give it.
    Tasks(String),
}

/// A fork of `run` at `step` for gdb, seeing `scope`'s threads, refused
/// when the run's kernel lacks what `needs` names.
pub fn fork(
    home: &Home,
    run: &Run,
    step: u64,
    scope: rewind_core::debug::Scope,
    needs: Needs,
) -> Result<rewind_core::debug::Debuggee> {
    let machine = run.machine_at(
        home,
        step,
        rewind_core::run::Keep::Keyframe,
        &mut rewind_vmm::Ignore,
    )?;
    if let Needs::Tasks(what) = needs
        && machine.task_layout()?.is_none()
    {
        bail!(
            "run {}'s kernel does not say where its tasks are, so {what}; record the run again",
            run.manifest.id
        );
    }
    let made = run.records_after(step)?;
    rewind_core::debug::Debuggee::new(machine, made, scope)
}

/// Where gdb's output goes: to the terminal, or back to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output<'a> {
    Terminal,
    Captured,
    /// Back to the caller, with each download gdb's debuginfod client
    /// starts said as it starts, the programs and libraries named by their
    /// build IDs here.
    CapturedSayingDownloads(&'a FileNames),
}

/// What gdb printed when its output was captured.
#[derive(Default)]
pub struct Printed {
    pub stdout: String,
    pub stderr: String,
}

/// Runs the host's gdb with `args` and waits for it, serving `fork` to it
/// from this thread when given, on the listener its arguments connect to.
/// Returns how gdb exited and, when captured, what it printed.
pub fn run_gdb(
    args: &[String],
    fork: Option<(&mut rewind_core::debug::Debuggee, &TcpListener)>,
    output: Output,
) -> Result<(std::process::ExitStatus, Printed)> {
    let mut command = Command::new(GDB_PROGRAM);
    command.args(args);
    if output != Output::Terminal {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }

    // The debuginfod client's log on gdb's standard error, for the
    // downloads to be said as they start.
    let names = match output {
        Output::CapturedSayingDownloads(names) => {
            command.env(downloads::VERBOSE_ENV, downloads::VERBOSE_ON);
            Some(names.clone())
        }
        Output::Terminal | Output::Captured => None,
    };
    let mut child = command.spawn().context(
        "starting gdb; is it on PATH? `rewind gdb --listen 127.0.0.1:1234` serves without it",
    )?;

    // gdb's output is read beside the serving, so a full pipe cannot stop
    // gdb while the fork waits for it. The client's downloads are said as
    // they start, since a library's DWARF or sources can take a minute to
    // download the first time, and its log is left out of what gdb said.
    let stdout = child
        .stdout
        .take()
        .map(|pipe| std::thread::spawn(move || read_lines(pipe, |_| {})));
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            let Some(names) = names else {
                return read_lines(pipe, |_| {});
            };
            let mut downloads = Downloads::new(&names);
            let text = read_lines(pipe, |line| {
                if let Some(message) = downloads.message(line) {
                    Say::Aloud.line(message);
                }
            });
            text.lines()
                .filter(|line| !downloads::is_client_log(line))
                .map(|line| format!("{line}\n"))
                .collect()
        })
    });
    let served = match fork {
        Some((debuggee, listener)) => {
            let (conn, _) = listener.accept()?;
            debuggee.serve(conn).map(|_| ())
        }
        None => Ok(()),
    };
    let status = child.wait()?;
    let text = |reader: Option<std::thread::JoinHandle<String>>| {
        reader
            .map(|r| r.join().unwrap_or_default())
            .unwrap_or_default()
    };
    let printed = Printed {
        stdout: text(stdout),
        stderr: text(stderr),
    };
    served?;
    Ok((status, printed))
}

/// The host's gdb.
const GDB_PROGRAM: &str = "gdb";

/// Reads `pipe` to its end a line at a time, handing each line to `each`,
/// without its newline, as it is read. Returns everything read.
fn read_lines(pipe: impl std::io::Read, mut each: impl FnMut(&str)) -> String {
    let mut reader = std::io::BufReader::new(pipe);
    let mut text = String::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&line);
        each(line.trim_end_matches('\n'));
        text.push_str(&line);
    }
    text
}

/// A shell command for gdb's pipe command that passes on only gdb's lines
/// about downloads, such as "Downloading 4.35 M separate debug info for
/// /nix/store/...-glibc-2.44/lib/libc.so.6...", to gdb's own standard
/// output. gdb runs the command with /bin/sh, whatever the person's shell.
const DOWNLOADS_ONLY: &str =
    r#"while IFS= read -r line; do case $line in Downloading*) printf '%s\n' "$line";; esac; done"#;

/// What gdb is told about a run at a step: the kernel's symbols, the
/// process's programs and libraries with their sources, and a debuginfod
/// server for their DWARF, with the session directory the files only the
/// VM had are written to.
pub struct Symbols {
    session: Session,
    kernel: KernelSymbols,
    process: Process,
    debuginfod: Option<Debuginfod>,
}

impl Symbols {
    /// The symbols for process `pid` at `step` of `run`, or when None the
    /// process running there, with the kernel's when `kernel` says so,
    /// saying what they are as `say` says.
    pub fn load(
        home: &Home,
        run: &Run,
        step: u64,
        pid: Option<u32>,
        kernel: Kernel,
        say: Say,
    ) -> Result<Symbols> {
        let kernel = match kernel {
            Kernel::Load => KernelSymbols::find(run),
            Kernel::Skip => KernelSymbols::none(),
        };
        let session = Session::new(home)?;
        let process = debugged_process(home, run, step, pid, &session.dir, say);
        let debuginfod = Debuginfod::start();
        Ok(Symbols {
            session,
            kernel,
            process,
            debuginfod,
        })
    }

    /// The directory the session's files are written to, which goes when
    /// the symbols do.
    pub fn dir(&self) -> &Path {
        &self.session.dir
    }

    /// The process's programs and libraries by their build IDs, which the
    /// debuginfod client asks for them by. A file with no build ID is left
    /// out.
    pub fn file_names(&self) -> FileNames {
        let mut names = FileNames::new();
        for file in &self.process.files {
            let Ok(mut opened) = std::fs::File::open(&file.path) else {
                continue;
            };
            let Some(id) = rewind_core::maps::build_id(&mut opened) else {
                continue;
            };
            let Some(name) = file.path.file_name() else {
                continue;
            };
            names.insert(id, name.to_string_lossy().into_owned());
        }
        names
    }

    /// gdb's arguments for these symbols, connecting to `target` when
    /// given. The session's debuginfod server is asked first, then any the
    /// person already uses.
    pub fn arguments(&self, target: Option<std::net::SocketAddr>) -> Vec<String> {
        let others = std::env::var(ENV_DEBUGINFOD_URLS).unwrap_or_default();
        let urls: Vec<&str> = self
            .debuginfod
            .as_ref()
            .map(|d| d.url.as_str())
            .into_iter()
            .chain(others.split_whitespace())
            .collect();
        arguments(&self.kernel, &self.process, &urls, target)
    }
}

/// gdb's arguments: the debuginfod servers to ask for DWARF, the kernel's symbols
/// and scripts, the running process's files, then the connection when
/// there is one. Each is its own -ex, so one gdb cannot run, such as a
/// debuginfod setting in a gdb built without debuginfod, does not stop the
/// rest.
fn arguments(
    kernel: &KernelSymbols,
    process: &Process,
    urls: &[&str],
    target: Option<std::net::SocketAddr>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-q".into()];
    let mut ex = |when: &str, command: String| {
        args.push(when.into());
        args.push(command);
    };

    if !urls.is_empty() {
        ex("-iex", "set debuginfod enabled on".into());
        ex("-iex", format!("set debuginfod urls {}", urls.join(" ")));
    }

    // The kernel: its DWARF when it is here, else its symbol table. The
    // scripts need the DWARF, which no debuginfod server has: the session's
    // serves only what is in the store, and the kernel's is not until
    // `realise` fetches it. gdb would also auto-load the scripts from next
    // to a vmlinux with only its symbol table, so auto-loading is off while
    // that one loads, and on again for the process's libraries.
    if let Some(file) = &kernel.file {
        if !kernel.dwarf {
            ex("-ex", AUTO_LOAD_OFF.into());
        }
        ex("-ex", format!("file {}", file.display()));
        if !kernel.dwarf {
            ex("-ex", AUTO_LOAD_ON.into());
        }
        if let Some(sources) = &kernel.sources {
            ex("-ex", format!("directory {}", sources.display()));
        }
        if let Some(scripts) = &kernel.scripts
            && kernel.dwarf
        {
            ex("-ex", format!("source {}", scripts.display()));
        }
    }

    // The running process's source files that only the VM had, where
    // they were written, and its programs and libraries, where it had
    // them.
    for (from, to) in &process.source_dirs {
        ex(
            "-ex",
            format!("set substitute-path {} {}", from.display(), to.display()),
        );
    }
    // Each is loaded through gdb's pipe command into a filter that keeps
    // add-symbol-file's line about the file and its offset to itself and
    // passes on gdb's line about downloading the file's debug info, which
    // can take a minute the first time.
    for file in &process.files {
        ex(
            "-ex",
            format!(
                "pipe with confirm off -- add-symbol-file {} -o {:#x} | {DOWNLOADS_ONLY}",
                file.path.display(),
                file.offset
            ),
        );
    }

    if let Some(address) = target {
        ex("-ex", format!("target remote {address}"));
    }
    args
}

/// The kernel's symbol file for gdb, the gdb scripts to load with it, and
/// the patched source files.
struct KernelSymbols {
    file: Option<PathBuf>,
    scripts: Option<PathBuf>,
    sources: Option<PathBuf>,
    /// Whether `file` has the DWARF as well as the symbol table.
    dwarf: bool,
}

impl KernelSymbols {
    /// No symbols for the kernel.
    fn none() -> KernelSymbols {
        KernelSymbols {
            file: None,
            scripts: None,
            sources: None,
            dwarf: false,
        }
    }

    /// The best symbols for the kernel `run` booted: its DWARF, else its
    /// symbol table. The DWARF is looked for where the run was recorded
    /// with it, then where this rewind would record it, which on a host
    /// without Nix is the debug release unpacked after the run was made.
    fn find(run: &Run) -> KernelSymbols {
        let dir = run.manifest.spec.kernel.parent().unwrap_or(Path::new("/"));
        let scripts = Some(dir.join(GDB_SCRIPTS)).filter(|p| p.exists());
        let build_id = std::fs::File::open(dir.join(VMLINUX))
            .ok()
            .and_then(|mut f| rewind_core::maps::build_id(&mut f));
        let candidates: Vec<PathBuf> = run
            .manifest
            .spec
            .kernel_debug
            .clone()
            .into_iter()
            .chain(std::env::var_os(rewind_core::home::ENV_KERNEL_DEBUG).map(PathBuf::from))
            .collect();

        if let Some(debug) = debug_dir(&candidates, build_id.as_deref()) {
            // The one directory in the overlay, named like the source tree.
            let sources = std::fs::read_dir(debug.join(DEBUG_SOURCES))
                .ok()
                .and_then(|mut entries| entries.next()?.ok())
                .map(|entry| entry.path());
            return KernelSymbols {
                file: Some(debug.join(DEBUG_VMLINUX)),
                scripts,
                sources,
                dwarf: true,
            };
        }

        // Without Nix there is no binary cache to fetch the DWARF from, so
        // say where it is.
        if on_path(NIX_STORE_PROGRAM, std::env::var_os("PATH").as_deref()).is_none() {
            eprintln!(
                "rewind: the kernel has its symbol table only; unpack {DEBUG_RELEASE} \
                 next to rewind-x86_64-linux for its DWARF and source files"
            );
        }
        KernelSymbols {
            file: Some(dir.join(VMLINUX)).filter(|p| p.exists()),
            scripts,
            sources: None,
            dwarf: false,
        }
    }
}

/// The first of `candidates` with the kernel's DWARF: for `build_id` when
/// it is known, so a debug directory updated in place for a newer kernel
/// is passed over. A candidate in the store is fetched when it is not
/// here.
fn debug_dir(candidates: &[PathBuf], build_id: Option<&str>) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|debug| {
            if !realise(&debug.join(DEBUG_VMLINUX), "the kernel's debug symbols") {
                return false;
            }
            let Some(id) = build_id.filter(|id| id.len() > 2) else {
                return true;
            };
            let (dir, rest) = id.split_at(2);
            debug
                .join(DEBUG_BUILD_IDS)
                .join(dir)
                .join(format!("{rest}.debug"))
                .exists()
        })
        .cloned()
}

/// What gdb is told about the process running at a step.
#[derive(Default)]
struct Process {
    /// Whose breakpoints and watchpoints gdb sees: this process's, when
    /// one was running.
    scope: rewind_core::debug::Scope,
    /// Its programs and libraries, with their load offsets.
    files: Vec<SymbolFile>,
    /// Directories of source files as the programs' DWARF names them, each
    /// with where its files are here: written from the VM, or a
    /// derivation's src in the store.
    source_dirs: Vec<(PathBuf, PathBuf)>,
}

/// Process `pid` at `step`, or when None the process running there: its
/// programs and libraries, from this machine's store or, when only the VM
/// or the run's image has them, written under `dir`, with the source files
/// those name. Empty when the kernel was running, there was no such
/// process, or the map could not be read.
fn debugged_process(
    home: &Home,
    run: &Run,
    step: u64,
    pid: Option<u32>,
    dir: &Path,
    say: Say,
) -> Process {
    let answer = match rewind_core::inspect::running(home, run, step, pid) {
        Ok(Inspection::Contents(bytes)) => bytes,
        Ok(Inspection::NotFound(message)) if pid.is_some() => {
            say.line(message.trim());
            return Process::default();
        }
        Ok(Inspection::NotFound(_)) => {
            say.line(format_args!("step {step} ran in the kernel, in no process"));
            return Process::default();
        }
        Ok(Inspection::Failed(message)) => {
            say.line(format_args!(
                "no symbols for the running process: {message}"
            ));
            return Process::default();
        }
        Err(e) => {
            say.line(format_args!("no symbols for the running process: {e:#}"));
            return Process::default();
        }
    };
    let Some(running) = Running::parse(&answer) else {
        say.line(format_args!(
            "no symbols for the running process: its answer was cut short"
        ));
        return Process::default();
    };
    // A Nix run's store, or a run's root filesystem, was mounted from its
    // input image, so a file this machine lacks is read from there.
    let mount = run
        .manifest
        .spec
        .image
        .as_deref()
        .and_then(|image| ImageMount::of(image, run.manifest.spec.job.root));
    let files = match running.symbol_files(dir, mount) {
        Ok(files) => files,
        Err(e) => {
            say.line(format_args!("no symbols for the running process: {e}"));
            return Process::default();
        }
    };

    // Say what gdb will know about, and what it cannot.
    let whose = match pid {
        Some(_) => format!("process {} at step {step}", running.pid),
        None => format!("step {step} ran in process {}", running.pid),
    };
    say.line(format_args!(
        "{whose}; loading symbols for {} of its files",
        files.len()
    ));
    let from_image = files.iter().filter(|f| f.origin == Origin::Image).count();
    if from_image > 0 {
        say.line(format_args!(
            "read {from_image} of them from the run's image"
        ));
    }
    let missing = running.missing(dir);
    if !missing.is_empty() {
        say.line(format_args!("no symbols for {}", missing.join(", ")));
    }

    // Source files outside the store, which debuginfod does not serve.
    // The VM has them for a program it built: one it sent, or one of the
    // run's outputs. Any other store program was built elsewhere, and the
    // VM's /build, if it has one, is another build's, so its source files
    // are its derivation's src on this machine. The VM's trees go first:
    // gdb takes the first substitute-path that matches.
    let outputs = &run.manifest.spec.job.outputs;
    let mut from_vm: Vec<String> = Vec::new();
    let mut from_src: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for (file, sources) in files.iter().zip(sources_outside_store(&files)) {
        if sources.is_empty() {
            continue;
        }
        let store_path = path_in_vm(&file.path, dir);
        let built_by_run = store_root(&store_path)
            .is_some_and(|root| outputs.iter().any(|o| Path::new(o) == root));
        if file.origin == Origin::Sent || built_by_run {
            from_vm.extend(sources.iter().cloned());
        }
        if file.origin != Origin::Sent {
            from_src.push((store_path, sources));
        }
    }
    let mut source_dirs = fetch_sources(home, run, step, running.pid, from_vm, dir, say);

    // The trees the VM did not have, from each store program's derivation.
    for (store_path, sources) in &from_src {
        let unmapped: Vec<String> = sources
            .iter()
            .filter(|p| {
                let tree = source_tree(Path::new(p));
                !source_dirs.iter().any(|(from, _)| *from == tree)
            })
            .cloned()
            .collect();
        if unmapped.is_empty() {
            continue;
        }
        source_dirs.extend(derivation_sources(store_path, &unmapped, say));
    }

    Process {
        scope: rewind_core::debug::Scope::Process(running.pid),
        files,
        source_dirs,
    }
}

/// Fetches the source files at `paths`, named by programs the VM built,
/// and writes them under `dir`: from the run's source cache when it holds
/// them as they were at `step`, else from a fork, keeping them in the
/// cache for the next lookup. Returns the tree each was in, with where it
/// is here, for gdb's substitute-path.
fn fetch_sources(
    home: &Home,
    run: &Run,
    step: u64,
    pid: u32,
    paths: Vec<String>,
    dir: &Path,
    say: Say,
) -> Vec<(PathBuf, PathBuf)> {
    let paths: Vec<String> = paths
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if paths.is_empty() {
        return Vec::new();
    }

    // What the cache holds of each file as it was at the step. Without a
    // trace to say which version that is, every file is fetched.
    let trace = run.trace().ok();
    let cache = SourceCache::of(home, &run.manifest.id);
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    let mut missing: Vec<(String, Version)> = Vec::new();
    for path in paths {
        let version = trace.map_or(Version::Changing, |t| source_cache::version(t, &path, step));
        match cache.get(version, &path) {
            Some(Entry::File(bytes)) => found.push((path, bytes)),
            Some(Entry::Absent) => {}
            None => missing.push((path, version)),
        }
    }
    let cached = found.len();

    // The rest from a fork, each kept whether the VM had it or not. A
    // cache that cannot be written only means fetching again next time.
    let mut fetched = None;
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|(path, _)| path.clone()).collect();
        match files_in_vm(home, run, step, pid, &names) {
            Ok(mut sections) => {
                for (path, version) in &missing {
                    let entry = match sections.iter().find(|(name, _)| name == path) {
                        Some((_, bytes)) => Entry::File(bytes.clone()),
                        None => Entry::Absent,
                    };
                    let _ = cache.put(*version, path, &entry);
                }
                fetched = Some(sections.len());
                found.append(&mut sections);
            }
            Err(message) => say.line(format_args!("no source files: {message}")),
        }
    }

    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (path, bytes) in found {
        let to = dir.join(path.trim_start_matches('/'));
        // Dated at the epoch, older than the programs written before them,
        // or gdb warns that each source is newer than its program.
        let written = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&to, bytes))
            .and_then(|()| std::fs::File::options().write(true).open(&to))
            .and_then(|f| f.set_modified(std::time::UNIX_EPOCH));
        if written.is_err() {
            continue;
        }
        let from = source_tree(Path::new(&path));
        if !dirs.iter().any(|(f, _)| *f == from) {
            let here = dir.join(from.strip_prefix("/").unwrap_or(&from));
            dirs.push((from, here));
        }
    }
    if let Some(fetched) = fetched {
        say.line(format_args!("fetched {fetched} source files from the VM"));
    }
    if cached > 0 {
        say.line(format_args!(
            "read {cached} source files fetched from the VM earlier"
        ));
    }
    dirs
}

/// The files at `paths` in a fork of `run` at `step`, as process `pid`
/// would open them, each by its path. Files the VM did not have are left
/// out. Fails with a message when the fork could not answer.
fn files_in_vm(
    home: &Home,
    run: &Run,
    step: u64,
    pid: u32,
    paths: &[String],
) -> std::result::Result<Vec<(String, Vec<u8>)>, String> {
    let answer = match rewind_core::inspect::files(home, run, step, Some(pid), paths) {
        Ok(Inspection::Contents(bytes)) => bytes,
        Ok(Inspection::NotFound(message) | Inspection::Failed(message)) => return Err(message),
        Err(e) => return Err(format!("{e:#}")),
    };
    let Some(sections) = rewind_init::sections(&answer) else {
        return Err("the answer was cut short".into());
    };
    Ok(sections
        .into_iter()
        .map(|(path, bytes)| (path, bytes.to_vec()))
        .collect())
}

/// How many directories down a source tree starts, as in /build/<name>
/// where Nix builds and /src in a container.
const SOURCE_TREE_DEPTH: usize = 2;

/// The tree a source file is in: the directory SOURCE_TREE_DEPTH down
/// from the root, or the file's own directory when it is not that deep.
/// gdb's substitute-path rewrites a program's compilation directory, the
/// directory its relative source paths start from, and that is the top
/// of the tree or somewhere in it. The kernel's tree, also under /build,
/// has a name of its own, so its sources still come from debuginfod.
fn source_tree(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("/"));
    let mut tree = PathBuf::from("/");
    for part in parent.components().skip(1).take(SOURCE_TREE_DEPTH) {
        tree.push(part);
    }
    tree
}

/// The start of the line gdb echoes before listing a file's sources,
/// followed by the file's index.
const SOURCES_MARKER: &str = "rewind-sources ";

/// The source files `info sources` listed for each of `count` files, in
/// gdb's `output` with each file's listing after its marker: the paths
/// after the objfile's name and a colon, separated by commas, without
/// gdb's notes. A marker for no such file starts nothing.
fn listed_sources(output: &str, count: usize) -> Vec<Vec<String>> {
    let mut listed = vec![Vec::new(); count];
    let mut current: Option<usize> = None;
    for line in output.lines() {
        if let Some(index) = line.strip_prefix(SOURCES_MARKER) {
            current = index.trim().parse().ok().filter(|i| *i < count);
            continue;
        }
        let Some(i) = current else {
            continue;
        };
        let paths = line
            .split(',')
            .map(str::trim)
            .filter(|p| p.starts_with('/') && !p.ends_with(':'))
            .map(String::from);
        listed[i].extend(paths);
    }
    listed
}

/// The characters a gdb command that splits its arguments like a shell,
/// such as `symbol-file`, needs escaped in a path.
const GDB_SPECIAL: &[char] = &[' ', '\t', '\'', '"', '\\'];

/// `path` as one argument of a gdb command that splits its arguments like
/// a shell: each special character after a backslash.
fn gdb_word(path: &Path) -> String {
    let mut word = String::new();
    for c in path.display().to_string().chars() {
        if GDB_SPECIAL.contains(&c) {
            word.push('\\');
        }
        word.push(c);
    }
    word
}

/// For each of `files`, the source files its DWARF names outside the
/// store, all listed by one gdb. It loads each file's symbols in turn,
/// after echoing the file's marker and clearing the last file's, so a
/// file gdb cannot read lists nothing rather than the files of the one
/// before it. Every list is empty when gdb cannot start.
fn sources_outside_store(files: &[SymbolFile]) -> Vec<Vec<String>> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut command = Command::new(GDB_PROGRAM);
    command.args(["-batch", "-nx"]);
    for (i, file) in files.iter().enumerate() {
        command
            .arg("-ex")
            .arg(format!("echo {SOURCES_MARKER}{i}\\n"));
        command.args(["-ex", "symbol-file"]);
        command
            .arg("-ex")
            .arg(format!("symbol-file {}", gdb_word(&file.path)));
        command.args(["-ex", "info sources"]);
    }
    let Ok(output) = command.stderr(Stdio::null()).output() else {
        return vec![Vec::new(); files.len()];
    };
    listed_sources(&String::from_utf8_lossy(&output.stdout), files.len())
        .into_iter()
        .map(|sources| {
            sources
                .into_iter()
                .filter(|p| !p.starts_with(NIX_STORE))
                .collect()
        })
        .collect()
}

/// The path a symbol file had in the VM: its path under `dir` when it was
/// written there, else its own path.
pub fn path_in_vm(file: &Path, dir: &Path) -> PathBuf {
    match file.strip_prefix(dir) {
        Ok(inside) => Path::new("/").join(inside),
        Err(_) => file.to_path_buf(),
    }
}

/// How Nix answers for a store path it does not know the derivation of.
const UNKNOWN_DERIVER: &str = "unknown-deriver";

/// The nix command, with the feature `nix derivation show` needs, and the
/// key recent versions list the derivations it shows under.
const NIX_PROGRAM: &str = "nix";
const NIX_COMMAND: &[&str] = &["--extra-experimental-features", "nix-command"];
const DERIVATIONS: &str = "derivations";

/// The trees `sources` are in, named by the DWARF of `store_path`, each
/// with the src of the derivation that built it: a program built in
/// the Nix sandbox names its files under the build's directory, such as
/// /build/source, which held src unpacked. Says on stderr why there are
/// none when the derivation or its src is not on this machine, or src is
/// not a directory, such as a tarball.
fn derivation_sources(store_path: &Path, sources: &[String], say: Say) -> Vec<(PathBuf, PathBuf)> {
    // Without Nix there is no derivation to ask, and nothing to say.
    let Some(root) = store_root(store_path) else {
        return Vec::new();
    };
    let Some(nix_store) = on_path(NIX_STORE_PROGRAM, std::env::var_os("PATH").as_deref()) else {
        return Vec::new();
    };
    let skip = |why: String| {
        say.line(format_args!(
            "no source files for {}: {why}",
            root.display()
        ));
        Vec::new()
    };

    // The derivation, which must be on this machine.
    let deriver = Command::new(nix_store)
        .args(["--query", "--deriver"])
        .arg(&root)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if deriver.is_empty() || deriver == UNKNOWN_DERIVER {
        return skip("this machine does not know its derivation".into());
    }
    let drv = PathBuf::from(deriver);
    if !drv.exists() {
        return skip(format!(
            "its derivation {} is not on this machine",
            drv.display()
        ));
    }

    // The derivation's src, which must be a directory here.
    let show = Command::new(NIX_PROGRAM)
        .args(NIX_COMMAND)
        .args(["derivation", "show"])
        .arg(&drv)
        .stderr(Stdio::null())
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let Some(src) = derivation_src(&show) else {
        return skip(format!("its derivation {} names no src", drv.display()));
    };
    if !src.is_dir() {
        return skip(format!("its src {} is not a directory here", src.display()));
    }

    let trees = source_trees_in(&src, sources);
    if trees.is_empty() {
        return skip(format!("none of its source files are in {}", src.display()));
    }
    say.line(format_args!(
        "source files for {} from {}",
        root.display(),
        src.display()
    ));
    trees.into_iter().map(|tree| (tree, src.clone())).collect()
}

/// Where a derivation's attributes are in `nix derivation show`: its
/// environment, and for a derivation with `__structuredAttrs = true`
/// either an object of their own, in Nix, or the environment's JSON
/// string, in Lix and older versions of Nix.
const DERIVATION_ENV: &str = "env";
const STRUCTURED_ATTRS: &str = "structuredAttrs";
const STRUCTURED_ATTRS_JSON: &str = "__json";
const SRC: &str = "src";

/// The `src` of the derivation `nix derivation show` printed as `show`:
/// under `derivations` by name in recent versions of Nix, by path at the
/// top in older ones, and in its environment or, with structured
/// attributes, among those.
fn derivation_src(show: &[u8]) -> Option<PathBuf> {
    let json: serde_json::Value = serde_json::from_slice(show).ok()?;
    let derivations = json.get(DERIVATIONS).unwrap_or(&json);
    let derivation = derivations.as_object()?.values().next()?;
    let env = derivation.get(DERIVATION_ENV);

    // The attributes as an object, then as a JSON string, then plain.
    let encoded: Option<serde_json::Value> = env
        .and_then(|e| e.get(STRUCTURED_ATTRS_JSON)?.as_str())
        .and_then(|text| serde_json::from_str(text).ok());
    let attributes = [derivation.get(STRUCTURED_ATTRS), encoded.as_ref(), env];
    attributes
        .into_iter()
        .flatten()
        .find_map(|a| a.get(SRC)?.as_str())
        .map(PathBuf::from)
}

/// The trees `sources` are in whose files are in `src`: a tree is the
/// directory src was unpacked to when one of its files, by its path
/// inside the tree, is in src. Files a build generated, under target/ for
/// one, are not in src, so one file found is enough.
fn source_trees_in(src: &Path, sources: &[String]) -> Vec<PathBuf> {
    let mut trees: Vec<PathBuf> = Vec::new();
    for source in sources {
        let source = Path::new(source);
        let tree = source_tree(source);
        if trees.contains(&tree) {
            continue;
        }
        let Ok(inside) = source.strip_prefix(&tree) else {
            continue;
        };
        if src.join(inside).exists() {
            trees.push(tree);
        }
    }
    trees
}

/// A directory for one gdb session's files, removed when dropped.
struct Session {
    dir: PathBuf,
}

impl Session {
    fn new(home: &Home) -> Result<Session> {
        let dir = home
            .root()
            .join(SESSIONS_DIR)
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir)?;
        Ok(Session { dir })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Fetches `path` with `nix-store --realise` when it is a store path not
/// on this machine, from the substituters in Nix's own settings. The
/// kernel's debug output is only in Rewind's binary cache, which the
/// NixOS module adds to them. Whether the path is here now.
/// On a host without Nix there is nothing to fetch with, and nothing to
/// say about it.
fn realise(path: &Path, what: &str) -> bool {
    if path.exists() {
        return true;
    }
    let Some(root) = store_root(path) else {
        return false;
    };
    let Some(nix_store) = on_path(NIX_STORE_PROGRAM, std::env::var_os("PATH").as_deref()) else {
        return false;
    };
    eprintln!("rewind: fetching {what}, {}", root.display());
    let _ = Command::new(nix_store)
        .arg("--realise")
        .arg(&root)
        .stdout(Stdio::null())
        .status();
    if !path.exists() {
        eprintln!(
            "rewind: could not fetch {what}; is Rewind's binary cache among Nix's substituters?"
        );
    }
    path.exists()
}

/// The store path a path is in: /nix/store/<hash>-<name>.
fn store_root(path: &Path) -> Option<PathBuf> {
    let inside = path.strip_prefix(NIX_STORE).ok()?;
    let first = inside.components().next()?;
    Some(Path::new(NIX_STORE).join(first))
}

/// An argument as a shell would need it typed.
fn quote(arg: &str) -> String {
    if arg.contains([' ', '\'', '"', '$', '\\']) {
        format!("'{}'", arg.replace('\'', r"'\''"))
    } else {
        arg.to_string()
    }
}

/// A debuginfod server for one gdb session, stopped when dropped.
struct Debuginfod {
    child: Child,
    url: String,
}

impl Debuginfod {
    /// Starts nixseparatedebuginfod2 on a free local port, or None when it
    /// is not to be had. The socket is bound here and handed over the way
    /// systemd's socket activation does, so gdb can connect before the
    /// server is ready and nothing races for the port. The server runs in
    /// a process group of its own, so the Ctrl-C meant for gdb does not
    /// stop it.
    fn start() -> Option<Debuginfod> {
        let program = debuginfod_program()?;
        let listener = TcpListener::bind(GDB_LOCAL).ok()?;
        let url = format!("http://{}", listener.local_addr().ok()?);
        let fd = listener.as_raw_fd();

        // A shell sets LISTEN_PID to its own pid and execs the server,
        // which keeps the pid, as the protocol needs.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#"export LISTEN_PID=$$ LISTEN_FDS=1; exec "$0" "$@""#)
            .arg(&program);
        for substituter in DEBUGINFOD_SUBSTITUTERS {
            cmd.args(["--substituter", substituter]);
        }
        cmd.args(["--expiration", DEBUGINFOD_EXPIRATION])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // SAFETY: dup2 and fcntl are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                let moved = if fd == LISTEN_FD {
                    libc::fcntl(fd, libc::F_SETFD, 0)
                } else {
                    libc::dup2(fd, LISTEN_FD)
                };
                if moved < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().ok()?;
        Some(Debuginfod { child, url })
    }
}

impl Drop for Debuginfod {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The server's program: the one the Nix package names, fetched the first
/// time, else one on PATH.
fn debuginfod_program() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os(ENV_DEBUGINFOD).map(PathBuf::from)
        && realise(&named, "the debuginfod server")
    {
        return Some(named);
    }
    on_path(DEBUGINFOD_PROGRAM, std::env::var_os("PATH").as_deref())
}

/// Where `program` is in `path`, a list of directories like PATH's.
fn on_path(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    // Store paths cut to the path Nix fetches, and arguments quoted for a
    // shell.
    use super::*;

    #[test]
    fn a_file_in_a_store_path_is_fetched_as_the_whole_path() {
        assert_eq!(
            store_root(Path::new("/nix/store/abc-kernel-debug/lib/debug/vmlinux")),
            Some(PathBuf::from("/nix/store/abc-kernel-debug"))
        );
        assert_eq!(store_root(Path::new("/opt/rewind/vmlinux")), None);
    }

    #[test]
    fn a_source_tree_is_two_directories_down_or_the_file_s_own() {
        assert_eq!(
            source_tree(Path::new("/build/mylib/src/pool.c")),
            PathBuf::from("/build/mylib")
        );
        assert_eq!(source_tree(Path::new("/src/main.c")), PathBuf::from("/src"));
    }

    /// A store program's source tree is the one its derivation's src
    /// holds. Makes a src directory with one crate's test file, then checks
    /// that of the trees gdb names, the build's /build/source is found in
    /// it, and the vendored crates and the Rust library, which src lacks,
    /// are not.
    #[test]
    fn a_source_tree_is_found_in_src_by_its_files() {
        let src = std::env::temp_dir().join(format!("rewind-src-tree-{}", std::process::id()));
        let test = src.join("crates/casita/tests/retained_process_pins.rs");
        std::fs::create_dir_all(test.parent().unwrap()).unwrap();
        std::fs::write(&test, "").unwrap();

        let sources = [
            "/build/source/crates/casita/tests/retained_process_pins.rs",
            "/build/source/target/release/build/casita/out/generated.rs",
            "/build/cargo-vendor-dir/libc-0.2.177/src/lib.rs",
            "/rustc/48a229ce/library/std/src/fs.rs",
        ]
        .map(String::from);
        assert_eq!(
            source_trees_in(&src, &sources),
            vec![PathBuf::from("/build/source")]
        );
        std::fs::remove_dir_all(&src).unwrap();
    }

    /// A derivation's src, from `nix derivation show` as Nix 2.35 prints
    /// it, under `derivations` by name, and as older versions print it, by
    /// path at the top; and none when the derivation has no src.
    #[test]
    fn a_derivation_s_src_is_read_from_either_shape() {
        let src = Some(PathBuf::from("/nix/store/jyz-source"));
        let newer = br#"{"derivations":{"h9v-casita-tests-0.1.0.drv":{"env":{"src":"/nix/store/jyz-source"}}},"version":4}"#;
        assert_eq!(derivation_src(newer), src);
        let older =
            br#"{"/nix/store/h9v-casita-tests-0.1.0.drv":{"env":{"src":"/nix/store/jyz-source"}}}"#;
        assert_eq!(derivation_src(older), src);
        let none = br#"{"/nix/store/h9v-run.drv":{"env":{"buildCommand":"true"}}}"#;
        assert_eq!(derivation_src(none), None);
    }

    /// A derivation with `__structuredAttrs = true` keeps src among its
    /// structured attributes, not in env. Reads src from `nix derivation
    /// show` of one such derivation as three versions print it, cut to a
    /// few attributes: Nix 2.35, under `derivations` with a
    /// `structuredAttrs` object; Nix 2.31, by path with the same object;
    /// and Lix 2.95, by path with the attributes as a JSON string in
    /// env's `__json`.
    #[test]
    fn a_structured_derivation_s_src_is_read_from_its_attributes() {
        let src = Some(PathBuf::from(
            "/nix/store/cwjyzq3122bisr9wdff7a683axzp7m84-src",
        ));
        let nix_2_35 = br#"{"derivations":{"813zvgg0x0ax42v6dm2jf19p9k8kx5lv-structured-0.1.drv":{"env":{"out":"/nix/store/i5sh3ajs0cb1mhxzvs8pg8sz92nzjpc6-structured-0.1"},"name":"structured-0.1","structuredAttrs":{"__structuredAttrs":true,"pname":"structured","src":"/nix/store/cwjyzq3122bisr9wdff7a683axzp7m84-src","version":"0.1"}}},"version":4}"#;
        assert_eq!(derivation_src(nix_2_35), src);
        let nix_2_31 = br#"{"/nix/store/813zvgg0x0ax42v6dm2jf19p9k8kx5lv-structured-0.1.drv":{"env":{"out":"/nix/store/i5sh3ajs0cb1mhxzvs8pg8sz92nzjpc6-structured-0.1"},"name":"structured-0.1","structuredAttrs":{"pname":"structured","src":"/nix/store/cwjyzq3122bisr9wdff7a683axzp7m84-src","version":"0.1"}}}"#;
        assert_eq!(derivation_src(nix_2_31), src);
        let lix_2_95 = br#"{"/nix/store/813zvgg0x0ax42v6dm2jf19p9k8kx5lv-structured-0.1.drv":{"env":{"__json":"{\"pname\":\"structured\",\"src\":\"/nix/store/cwjyzq3122bisr9wdff7a683axzp7m84-src\",\"version\":\"0.1\"}","out":"/nix/store/i5sh3ajs0cb1mhxzvs8pg8sz92nzjpc6-structured-0.1"},"name":"structured-0.1"}}"#;
        assert_eq!(derivation_src(lix_2_95), src);
    }

    /// One gdb lists every file's sources, each after a marker it echoes
    /// before loading the file. Output as gdb 16 prints it for a program,
    /// a path it cannot open and a library, cut to two source files: each
    /// file gets the paths after its own marker, the objfile's header and
    /// gdb's notes left out, and the missing file none, not the files of
    /// the one before it.
    #[test]
    fn one_gdb_lists_each_file_s_sources_after_its_marker() {
        let output = "rewind-sources 0\n\
            /tmp/s/prog:\n\
            (Full debug information has not yet been read for this file.)\n\
            \n\
            /build/mylib/src/pool.c, /build/mylib/src/pool.h\n\
            \n\
            rewind-sources 1\n\
            rewind-sources 2\n\
            /tmp/s/libb.so:\n\
            (Full debug information has not yet been read for this file.)\n\
            \n\
            /build/b/b.c\n";
        assert_eq!(
            listed_sources(output, 3),
            vec![
                vec![
                    "/build/mylib/src/pool.c".to_string(),
                    "/build/mylib/src/pool.h".to_string()
                ],
                vec![],
                vec!["/build/b/b.c".to_string()],
            ]
        );
        assert_eq!(
            listed_sources("rewind-sources 7\n/x.c\n", 1),
            vec![Vec::<String>::new()]
        );
    }

    /// A path in a gdb command that splits its arguments like a shell:
    /// spaces, quotes and backslashes are escaped, other characters left.
    #[test]
    fn a_path_is_one_gdb_argument() {
        assert_eq!(
            gdb_word(Path::new("/nix/store/abc-x/lib.so")),
            "/nix/store/abc-x/lib.so"
        );
        assert_eq!(
            gdb_word(Path::new(r#"/a b/c'd"e\f"#)),
            r#"/a\ b/c\'d\"e\\f"#
        );
    }

    /// Each line of a pipe reaches the callback as it is read, without its
    /// newline, and the whole text comes back: two lines, the last with no
    /// newline, read from a byte slice.
    #[test]
    fn lines_are_handed_on_as_they_are_read_and_kept() {
        let mut seen = Vec::new();
        let text = read_lines(&b"one\ntwo"[..], |line| seen.push(line.to_string()));
        assert_eq!(seen, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(text, "one\ntwo");
    }

    /// Each program or library is loaded through gdb's pipe command into
    /// the filter that passes on only download lines. Builds the arguments
    /// for a process with one library and checks its command.
    #[test]
    fn a_library_is_loaded_with_only_its_download_lines_shown() {
        let process = Process {
            files: vec![SymbolFile {
                path: PathBuf::from("/nix/store/abc-glibc/lib/libc.so.6"),
                offset: 0x7f00_0000_0000,
                origin: Origin::Store,
            }],
            ..Process::default()
        };
        let args = arguments(&KernelSymbols::none(), &process, &[], None);
        let expected = format!(
            "pipe with confirm off -- add-symbol-file /nix/store/abc-glibc/lib/libc.so.6 -o 0x7f0000000000 | {DOWNLOADS_ONLY}"
        );
        assert!(args.contains(&expected), "{args:?}");
    }

    #[test]
    fn arguments_with_spaces_or_quotes_are_quoted() {
        assert_eq!(quote("-q"), "-q");
        assert_eq!(quote("target remote x"), "'target remote x'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    /// The kernel's gdb scripts need its DWARF. Builds gdb's arguments for
    /// a kernel with only its symbol table while a debuginfod server is
    /// named, as on a host without Nix with DEBUGINFOD_URLS set, and checks
    /// the scripts are neither sourced nor auto-loaded with vmlinux, and
    /// the server is still named.
    #[test]
    fn the_kernel_s_scripts_are_loaded_only_with_its_dwarf() {
        let kernel = |dwarf| KernelSymbols {
            file: Some(PathBuf::from("/opt/rewind/vmlinux")),
            scripts: Some(PathBuf::from("/opt/rewind/vmlinux-gdb.py")),
            sources: None,
            dwarf,
        };
        let urls = ["https://debuginfod.debian.net"];
        let address = "127.0.0.1:1234".parse().unwrap();
        let sourced = |args: &[String]| args.iter().any(|a| a.starts_with("source "));

        let without = arguments(&kernel(false), &Process::default(), &urls, Some(address));
        assert!(!sourced(&without));
        let at = |command: &str| without.iter().position(|a| a == command);
        let off = at("set auto-load python-scripts off").expect("auto-load is turned off");
        let file = at("file /opt/rewind/vmlinux").expect("the kernel is loaded");
        let on = at("set auto-load python-scripts on").expect("auto-load is turned back on");
        assert!(off < file && file < on);
        assert!(without.contains(&"set debuginfod urls https://debuginfod.debian.net".to_string()));

        let with = arguments(&kernel(true), &Process::default(), &urls, Some(address));
        assert!(sourced(&with));
    }

    /// The kernel's DWARF comes from the first candidate directory that
    /// holds it for the run's build ID. Makes two debug directories in a
    /// temporary directory, one for another kernel and one for this, and
    /// checks the matching one is chosen whatever the order, that none is
    /// when neither matches, and that the first with any DWARF is when the
    /// build ID is not known.
    #[test]
    fn the_kernel_s_dwarf_comes_from_a_directory_with_its_build_id() {
        let root = std::env::temp_dir().join(format!("rewind-debug-dir-{}", std::process::id()));
        let debug = |name: &str, id: &str| {
            let dir = root.join(name);
            let file = dir
                .join(DEBUG_BUILD_IDS)
                .join(&id[..2])
                .join(format!("{}.debug", &id[2..]));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "").unwrap();
            std::os::unix::fs::symlink(&file, dir.join(DEBUG_VMLINUX)).unwrap();
            dir
        };
        let old = debug("old", "aa0001");
        let new = debug("new", "bb0002");

        let both = [old.clone(), new.clone()];
        assert_eq!(debug_dir(&both, Some("bb0002")), Some(new.clone()));
        assert_eq!(
            debug_dir(&[new.clone(), old.clone()], Some("aa0001")),
            Some(old.clone())
        );
        assert_eq!(debug_dir(&both, Some("cc0003")), None);
        assert_eq!(debug_dir(&both, None), Some(old.clone()));
        assert_eq!(
            debug_dir(&[root.join("missing"), new.clone()], None),
            Some(new)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Looks for a program in a PATH-like list of directories: a temporary
    /// directory holding a file of that name, then a list without it.
    #[test]
    fn a_program_is_found_on_the_path_it_is_on() {
        let dir = std::env::temp_dir().join(format!("rewind-on-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nix-store"), "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();

        assert_eq!(
            on_path("nix-store", Some(&path)),
            Some(dir.join("nix-store"))
        );
        assert_eq!(
            on_path("nix-store", Some(std::ffi::OsStr::new("/nonexistent"))),
            None
        );
        assert_eq!(on_path("nix-store", None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
