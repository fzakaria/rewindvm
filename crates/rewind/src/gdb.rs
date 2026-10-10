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
use rewind_core::debuginfod_cache;
use rewind_core::inspect::Inspection;
use rewind_core::link_map;
use rewind_core::maps::{ImageMount, Origin, Running, SymbolFile};
use rewind_core::source_cache::{self, Entry, SourceCache, Version};
use rewind_core::threads::{OnTheCpu, Tasks};
use rewind_core::{Home, Run};

use crate::downloads::{self, Downloads, FileNames};
use crate::locate::Pick;

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

/// Turning gdb's debuginfod client off and on again around a file no
/// server has debug info for.
const DEBUGINFOD_OFF: &str = "set debuginfod enabled off";
const DEBUGINFOD_ON: &str = "set debuginfod enabled on";

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

/// The most bytes of vDSO read: a few pages in any kernel, so a map that
/// says more is not believed.
const MAX_VDSO: u64 = 1 << 20;

/// Where gdb sessions keep the files only the VM has, under Rewind's
/// data directory, one directory per session.
const SESSIONS_DIR: &str = "gdb";

/// The servers gdb asks besides the session's own.
const ENV_DEBUGINFOD_URLS: &str = "DEBUGINFOD_URLS";

/// The first descriptor systemd's socket activation hands a server, the
/// way the session's server gets its listening socket.
const LISTEN_FD: i32 = 3;

/// Where gdb starts on a fork: in thread `tid` of process `pid`, or when
/// both are None the thread `rewind where` would look at, and in frame
/// `frame` of it, else its innermost.
pub struct Start {
    pub pid: Option<u32>,
    pub tid: Option<u32>,
    pub frame: Option<u32>,
}

/// Serves gdb on a fork of `run` at `step`, debugging the process `start`
/// names: on `listen` if given, else on a free local port with the host's
/// gdb started against it, with `extra` after the arguments this makes.
/// Ctrl-C belongs to gdb, which turns it into an interrupt for the fork,
/// so this process ignores it meanwhile.
pub fn gdb(
    home: &Home,
    run: &Run,
    step: u64,
    start: Start,
    listen: Option<&str>,
    extra: &[String],
) -> Result<ExitCode> {
    // The fork, then the thread: the one given, else the step's event's,
    // else the one on the CPU. Finding a process that is not on the CPU
    // takes the kernel's task list, which kernels before it do not
    // publish.
    let pick = crate::locate::thread_at(run.trace()?, step, start.pid, start.tid)?;
    let needs = match (start.pid, start.tid) {
        (None, None) => Needs::Nothing,
        _ => Needs::Tasks("--pid and --tid cannot find threads".into()),
    };
    let machine = fork(home, run, step, needs)?;
    let thread = match pick {
        Pick::Thread { pid, tid } => Some((pid, tid)),
        Pick::OnTheCpu => match on_the_cpu(&machine)? {
            Some(OnTheCpu::Thread { pid, tid }) => Some((pid, tid)),
            Some(OnTheCpu::WithoutMemory { .. } | OnTheCpu::Idle) | None => None,
        },
    };

    // What gdb is told about the process, which takes inspections on forks
    // of their own: the one given or the thread's, else the one the
    // inspection finds running. A process it does not find leaves gdb
    // debugging the kernel alone, and starting in the CPU's thread.
    let pid = match (start.pid, start.tid) {
        (None, None) => None,
        _ => thread.map(|(pid, _)| pid),
    };
    let mut symbols = Symbols::load(home, run, step, pid, Kernel::Load, Say::Aloud)?;
    let mut debuggee = debuggee(run, step, machine, symbols.process.scope)?;
    symbols.add_vdso(&debuggee, Say::Aloud);
    symbols.add_libraries(&mut debuggee, Say::Aloud);
    let listener = TcpListener::bind(listen.unwrap_or(GDB_LOCAL)).context("listening for gdb")?;
    let mut args = symbols.arguments(Some(listener.local_addr()?));

    // gdb starts in the thread, whose registers are the user registers it
    // saved entering the kernel, rather than in the CPU's, which at a step
    // are in the kernel's hypercall; then in the frame asked for.
    if let Some(number) = thread.and_then(|(_, tid)| debuggee.thread_number(tid)) {
        args.extend(["-ex".to_string(), format!("thread {number}")]);
    }
    if let Some(frame) = start.frame {
        args.extend(["-ex".to_string(), format!("frame {frame}")]);
    }
    args.extend(extra.iter().cloned());

    // Only serving: say how to connect, then wait for gdb. The debuginfod
    // server runs for as long as this does.
    if listen.is_some() {
        let shown: Vec<String> = args.iter().map(|a| crate::show::quote(a)).collect();
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
    warn_if_too_old(Say::Aloud);
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

/// A fork of `run` at `step` for gdb, refused when the run's kernel lacks
/// what `needs` names.
pub fn fork(home: &Home, run: &Run, step: u64, needs: Needs) -> Result<rewind_vmm::Machine> {
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
            "run {}'s kernel has not said where its tasks are by step {step}, so {what}",
            run.manifest.id
        );
    }
    Ok(machine)
}

/// gdb's view of `machine`, a fork of `run` at `step`, seeing `scope`'s
/// threads.
pub fn debuggee(
    run: &Run,
    step: u64,
    machine: rewind_vmm::Machine,
    scope: rewind_core::debug::Scope,
) -> Result<rewind_core::debug::Debuggee> {
    let made = run.records_after(step)?;
    rewind_core::debug::Debuggee::new(machine, made, scope)
}

/// What the CPU ran at the fork's step, or None from a kernel that does
/// not say where its tasks are.
pub fn on_the_cpu(machine: &rewind_vmm::Machine) -> Result<Option<OnTheCpu>> {
    let Some(layout) = machine.task_layout()? else {
        return Ok(None);
    };
    Ok(Some(Tasks::new(machine, layout).on_the_cpu()?))
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
    let mut command = Command::new(gdb_program());
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
        Some((debuggee, listener)) => match accept_while_running(listener, &mut child)? {
            Some(conn) => debuggee.serve(conn).map(|_| ()),
            None => Ok(()),
        },
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

/// The host's gdb, unless the variable names one, as the Nix package's
/// wrapper names the gdb it ships.
const GDB_PROGRAM: &str = "gdb";
const ENV_GDB: &str = "REWIND_GDB";

/// The first gdb version that finds thread-local variables, such as
/// errno, itself, from the FS base and the shared library list the stub
/// gives it: earlier ones ask the stub, which cannot say.
pub const GDB_THREAD_LOCALS: u32 = 17;

/// The gdb `rewind gdb` starts.
pub fn gdb_program() -> std::ffi::OsString {
    std::env::var_os(ENV_GDB)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| GDB_PROGRAM.into())
}

/// gdb's major version, from the last word of `banner`, the first line
/// `gdb --version` prints, such as "GNU gdb (GDB) 17.2".
pub fn major_version(banner: &str) -> Option<u32> {
    banner
        .split_whitespace()
        .last()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// What a gdb older than [`GDB_THREAD_LOCALS`] cannot do, said by
/// `rewind gdb` and `rewind doctor`.
pub fn too_old(major: u32) -> String {
    format!(
        "gdb {major} cannot read thread-local variables, such as errno, at a step; \
         gdb {GDB_THREAD_LOCALS} or later can"
    )
}

/// Says when the gdb `rewind gdb` starts is too old to read thread-local
/// variables. A gdb that does not run is left to fail when started.
fn warn_if_too_old(say: Say) {
    let Ok(output) = Command::new(gdb_program()).arg("--version").output() else {
        return;
    };
    let banner = String::from_utf8_lossy(&output.stdout);
    let Some(major) = banner.lines().next().and_then(major_version) else {
        return;
    };
    if major < GDB_THREAD_LOCALS {
        say.line(too_old(major));
    }
}

/// How often a wait for gdb to connect looks whether gdb has exited.
const CONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// The connection `gdb` makes to `listener`, or None once gdb exits
/// without making one, as `gdb --version` does. gdb loading large symbol
/// files can take minutes to connect, so there is no time limit.
fn accept_while_running(
    listener: &TcpListener,
    gdb: &mut Child,
) -> Result<Option<std::net::TcpStream>> {
    listener.set_nonblocking(true)?;
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false)?;
                return Ok(Some(conn));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if gdb.try_wait()?.is_some() {
            return Ok(None);
        }
        std::thread::sleep(CONNECT_POLL);
    }
}

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
        let debuginfod = Debuginfod::start(home);
        Ok(Symbols {
            session,
            kernel,
            process,
            debuginfod,
        })
    }

    /// Adds the process's vDSO, where clock_gettime and the like run, to
    /// the files gdb loads. No file holds it, so it is read out of
    /// `debuggee`, the fork, in the process's address space; without it gdb
    /// can neither name a frame in it nor unwind past one. A vDSO that does
    /// not read is said, and left out, as is one the process has not
    /// called into yet, which no frame can be in.
    pub fn add_vdso(&mut self, debuggee: &rewind_core::debug::Debuggee, say: Say) {
        let Some(range) = self.process.vdso.clone() else {
            return;
        };
        let len = range.end.saturating_sub(range.start);
        if len > MAX_VDSO {
            say.line(format_args!(
                "no symbols for the vDSO: its {len} bytes are past {MAX_VDSO}"
            ));
            return;
        }
        let bytes = match debuggee.read_vdso(range.clone()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(e) => {
                say.line(format_args!("no symbols for the vDSO: {e:#}"));
                return;
            }
        };
        match rewind_core::maps::vdso_file(&self.session.dir, range.start, &bytes) {
            Ok(Some(file)) => self.process.files.push(file),
            Ok(None) => say.line("no symbols for the vDSO: it is not ELF"),
            Err(e) => say.line(format_args!("no symbols for the vDSO: {e}")),
        }
    }

    /// Tells gdb, through `debuggee`, of the shared libraries the process's
    /// dynamic loader listed in its memory, so gdb loads them as shared
    /// libraries and knows each one's link map, which it finds their
    /// thread-local variables through. Each link map is matched to the
    /// file whose dynamic section is where the link map says. A static
    /// program, a process whose loader has not run yet, or a list whose
    /// main program is not among the files leaves every file loaded one by
    /// one, as before.
    pub fn add_libraries(&mut self, debuggee: &mut rewind_core::debug::Debuggee, say: Say) {
        // Where each file's dynamic section is in the process. The vDSO,
        // which no file here holds, stays loaded on its own.
        let dynamics: Vec<Option<u64>> = self
            .process
            .files
            .iter()
            .map(|f| {
                if f.origin == Origin::Memory {
                    return None;
                }
                let mut file = std::fs::File::open(&f.path).ok()?;
                let linked = rewind_core::maps::dynamic_vaddr(&mut file)?;
                Some(f.offset.wrapping_add(linked))
            })
            .collect();
        let read = |at: u64, buf: &mut [u8]| debuggee.read_process(at, buf);
        let found: Vec<u64> = dynamics.iter().flatten().copied().collect();
        let Some(r_debug) = link_map::find_r_debug(&read, &found) else {
            return;
        };
        let maps = match link_map::entries(&read, r_debug) {
            Ok(maps) => maps,
            Err(e) => {
                say.line(format_args!("no shared library list: {e:#}"));
                return;
            }
        };

        // The main program first, then each library a file here holds.
        let file_of = |map: &link_map::LinkMap| dynamics.iter().position(|d| *d == Some(map.l_ld));
        let Some((first, rest)) = maps.split_first() else {
            return;
        };
        let Some(main) = file_of(first) else {
            return;
        };
        let mut libraries = Vec::new();
        let mut named = Vec::new();
        for map in rest {
            let Some(i) = file_of(map).filter(|i| *i != main) else {
                continue;
            };
            libraries.push(i);
            named.push((map, self.process.files[i].path.as_path()));
        }
        debuggee.set_libraries(link_map::svr4_xml(first.lm, &named));
        self.process.listed = Some(Listed { main, libraries });
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
        ex("-iex", DEBUGINFOD_ON.into());
        ex("-iex", format!("set debuginfod urls {}", urls.join(" ")));
    }

    // A program whose loader listed its libraries is gdb's executable and
    // symbol file, at its load offset, so the list's first link map is the
    // program's, as gdb assumes when it finds thread-local variables. The
    // kernel is then added beside it rather than taking its place.
    let main = process.listed.as_ref().map(|l| &process.files[l.main]);
    if let Some(main) = main {
        ex("-ex", format!("exec-file {}", main.path.display()));
        ex(
            "-ex",
            format!(
                "pipe with confirm off -- symbol-file -o {:#x} {} | {DOWNLOADS_ONLY}",
                main.offset,
                main.path.display()
            ),
        );
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
        match main {
            Some(_) => ex(
                "-ex",
                format!(
                    "pipe with confirm off -- add-symbol-file {} | {DOWNLOADS_ONLY}",
                    file.display()
                ),
            ),
            None => ex("-ex", format!("file {}", file.display())),
        }
        if !kernel.dwarf {
            ex("-ex", AUTO_LOAD_ON.into());
        }
        if let Some(sources) = &kernel.sources {
            ex("-ex", format!("directory {}", sources.display()));
        }
        if let Some(scripts) = &kernel.scripts
            && kernel.dwarf
        {
            // The scripts evaluate C as they load, which the program's own
            // language, gdb's choice once the program is its symbol file,
            // may not parse.
            ex(
                "-ex",
                format!("with language c -- source {}", scripts.display()),
            );
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
    // The vDSO, read out of the VM's memory, has debug info on no server,
    // so it loads with debuginfod off: asked, the client would announce a
    // download that never comes.
    // The files the loader listed are left to gdb's shared library list.
    let listed = |i: usize| {
        process
            .listed
            .as_ref()
            .is_some_and(|l| l.main == i || l.libraries.contains(&i))
    };
    for (i, file) in process.files.iter().enumerate() {
        if listed(i) {
            continue;
        }
        let quiet = file.origin == Origin::Memory && !urls.is_empty();
        if quiet {
            ex("-ex", DEBUGINFOD_OFF.into());
        }
        ex(
            "-ex",
            format!(
                "pipe with confirm off -- add-symbol-file {} -o {:#x} | {DOWNLOADS_ONLY}",
                file.path.display(),
                file.offset
            ),
        );
        if quiet {
            ex("-ex", DEBUGINFOD_ON.into());
        }
    }

    // The languages' helpers, each after the directory its modules are in.
    for helper in &process.helpers {
        if let Some(dir) = &helper.import_from {
            ex(
                "-ex",
                format!(
                    "python import sys; sys.path.insert(0, {:?})",
                    dir.display().to_string()
                ),
            );
        }
        ex("-ex", format!("source {}", helper.script.display()));
    }

    // The listed libraries' paths are this machine's, so gdb reads them
    // here rather than asking the stub for them, and loads them once it
    // has the list from the stub.
    if process.listed.is_some() {
        ex("-ex", SYSROOT_HERE.into());
    }
    if let Some(address) = target {
        ex("-ex", format!("target remote {address}"));
        if process.listed.is_some() {
            ex(
                "-ex",
                format!("pipe with confirm off -- sharedlibrary | {DOWNLOADS_ONLY}"),
            );
        }
    }
    args
}

/// Where gdb finds the files a shared library list names: here, under the
/// root, since the stub names this machine's copies.
const SYSROOT_HERE: &str = "set sysroot /";

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
    /// Where its vDSO is mapped, which [`Symbols::add_vdso`] reads.
    vdso: Option<std::ops::Range<u64>>,
    /// Which of `files` its loader listed, when [`Symbols::add_libraries`]
    /// found the list.
    listed: Option<Listed>,
    /// gdb support for the languages it runs that its toolchain ships but
    /// gdb does not load itself.
    helpers: Vec<Helper>,
}

/// A language's gdb support, shipped by the toolchain or interpreter a
/// program came from: a script to source, and a directory gdb's Python
/// imports the script's own modules from when it has some.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Helper {
    import_from: Option<PathBuf>,
    script: PathBuf,
}

/// Rust's pretty printers, which a Rust program's .debug_gdb_scripts names
/// without a directory, and where a rustc keeps them.
const RUST_PRINTERS: &str = "gdb_load_rust_pretty_printers.py";
const RUSTLIB_ETC: &str = "lib/rustlib/etc";

/// CPython's gdb commands, py-bt and the like, which nixpkgs' python ships
/// beside its libpython rather than where gdb would load them itself.
const LIBPYTHON: &str = "libpython";
const LIBPYTHON_GDB: &str = "share/gdb/libpython.py";

/// The files of a process its dynamic loader listed: gdb is told of them
/// through the stub's shared library list rather than loaded one by one,
/// so it knows each one's link map, which it finds thread-local variables
/// through. Each is an index into [`Process::files`].
struct Listed {
    /// The main program, which gdb takes as its symbol file, so the
    /// list's first link map is this program's.
    main: usize,
    /// The shared libraries.
    libraries: Vec<usize>,
}

/// The step of process `pid`'s latest event before `step`, if it made one.
fn last_event_before(trace: &rewind_trace::Trace, pid: u32, step: u64) -> Option<u64> {
    trace
        .until(step.saturating_sub(1))
        .iter()
        .rev()
        .find(|e| e.pid == pid)
        .map(|e| e.step)
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

    // A process with nothing mapped died in the inspection's own fork, as
    // one with a fatal signal pending does once the inspection's stop
    // wakes it. Its files are read at its last event before the step,
    // where it was alive; what it maps rarely changes after it starts.
    let earlier = run
        .trace()
        .ok()
        .and_then(|trace| last_event_before(trace, running.pid, step));
    if running.mappings.is_empty()
        && let Some(earlier) = earlier
    {
        say.line(format_args!(
            "process {} had died by the time its map was read at step {step}; \
             reading its files at step {earlier}, its last event before",
            running.pid
        ));
        return debugged_process(home, run, earlier, Some(running.pid), dir, say);
    }
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

    let helpers = helpers(&files, dir, say);
    Process {
        scope: rewind_core::debug::Scope::Process(running.pid),
        files,
        source_dirs,
        vdso: running.vdso,
        listed: None,
        helpers,
    }
}

/// The language helpers gdb needs for `files` that their toolchains ship
/// on this machine: CPython's commands beside a store libpython, and the
/// pretty printers of the rustc that built a store program that asks for
/// them, each once.
fn helpers(files: &[SymbolFile], dir: &Path, say: Say) -> Vec<Helper> {
    let mut helpers: Vec<Helper> = Vec::new();
    for file in files {
        if file.origin != Origin::Store {
            continue;
        }
        let store_path = path_in_vm(&file.path, dir);
        let Some(root) = store_root(&store_path) else {
            continue;
        };

        // CPython's commands, beside the libpython the process has.
        let is_libpython = file
            .path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(LIBPYTHON));
        let libpython_gdb = root.join(LIBPYTHON_GDB);
        if is_libpython && libpython_gdb.exists() {
            let helper = Helper {
                import_from: None,
                script: libpython_gdb,
            };
            if !helpers.contains(&helper) {
                helpers.push(helper);
            }
        }

        // Rust's printers, which the program names without a directory.
        let wants_rust = std::fs::File::open(&file.path)
            .map(|mut f| rewind_core::maps::gdb_scripts(&mut f))
            .is_ok_and(|names| names.iter().any(|n| n == RUST_PRINTERS));
        if !wants_rust {
            continue;
        }
        let Some(etc) = rust_printers(&root, say) else {
            continue;
        };
        let helper = Helper {
            import_from: Some(etc.clone()),
            script: etc.join(RUST_PRINTERS),
        };
        if !helpers.contains(&helper) {
            helpers.push(helper);
        }
    }
    helpers
}

/// The directory of Rust's pretty printers for the store path `root`:
/// the ones of the rustc that built it, from its derivation, else those of
/// the rustc on PATH, as rust-gdb finds them. Says why there are none.
fn rust_printers(root: &Path, say: Say) -> Option<PathBuf> {
    let found = derivation_rust_printers(root).or_else(path_rust_printers);
    if found.is_none() {
        say.line(format_args!(
            "no Rust pretty printers for {}: neither the rustc that built it nor one on PATH is here",
            root.display()
        ));
    }
    found
}

/// The printers of the rustc on PATH, in its sysroot.
fn path_rust_printers() -> Option<PathBuf> {
    let output = Command::new(RUSTC_PROGRAM)
        .args(["--print", "sysroot"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let etc = Path::new(&sysroot).join(RUSTLIB_ETC);
    etc.join(RUST_PRINTERS).exists().then_some(etc)
}

/// The rustc rust-gdb asks for its sysroot.
const RUSTC_PROGRAM: &str = "rustc";

/// The printers of the rustc that built the store path `root`: among its
/// derivation's inputs, a Rust toolchain's output, or a path that output
/// refers to, as nixpkgs' rustc wrapper refers to the rustc it wraps.
/// None when the derivation or its rustc is not on this machine.
fn derivation_rust_printers(root: &Path) -> Option<PathBuf> {
    let nix_store = on_path(NIX_STORE_PROGRAM, std::env::var_os("PATH").as_deref())?;
    let query = |args: &[&str], path: &Path| -> Vec<PathBuf> {
        Command::new(&nix_store)
            .args(args)
            .arg(path)
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    };

    // The derivation, which must be on this machine.
    let drv = query(&["--query", "--deriver"], root)
        .into_iter()
        .next()
        .filter(|d| d.exists())?;
    let show = Command::new(NIX_PROGRAM)
        .args(NIX_COMMAND)
        .args(["derivation", "show"])
        .arg(&drv)
        .stderr(Stdio::null())
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();

    // Each Rust input's outputs, and what each refers to.
    for input in derivation_inputs(&show) {
        let is_rust = input
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains(RUST));
        if !is_rust {
            continue;
        }
        for output in query(&["--query", "--outputs"], &input) {
            let mut candidates = vec![output.clone()];
            if output.exists() {
                candidates.extend(query(&["--query", "--references"], &output));
            }
            let found = candidates
                .into_iter()
                .map(|c| c.join(RUSTLIB_ETC))
                .find(|etc| etc.join(RUST_PRINTERS).exists());
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

/// How a Rust toolchain's derivations are named, among a derivation's
/// inputs: rustc, rustc-wrapper, rust-default and the like.
const RUST: &str = "rust";

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
        let Some(to) = rewind_core::guest_path::under(dir, &path) else {
            continue;
        };
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
    let mut command = Command::new(gdb_program());
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
/// written there, else its own path. The vDSO is no file in the VM, and
/// keeps the name the VM's map gives it.
pub fn path_in_vm(file: &Path, dir: &Path) -> PathBuf {
    match file.strip_prefix(dir) {
        Ok(inside) if inside == Path::new(rewind_core::maps::VDSO) => inside.to_path_buf(),
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
/// The input derivations of the derivation `nix derivation show` printed
/// as `show`: by name under `inputs.drvs` in recent versions of Nix, by
/// path under `inputDrvs` in older ones.
fn derivation_inputs(show: &[u8]) -> Vec<PathBuf> {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(show) else {
        return Vec::new();
    };
    let derivations = json.get(DERIVATIONS).unwrap_or(&json);
    let Some(derivation) = derivations.as_object().and_then(|d| d.values().next()) else {
        return Vec::new();
    };
    let newer = derivation.get(INPUTS).and_then(|i| i.get(INPUT_DRVS_NEWER));
    let older = derivation.get(INPUT_DRVS_OLDER);
    let Some(drvs) = newer.or(older).and_then(|d| d.as_object()) else {
        return Vec::new();
    };
    drvs.keys()
        .map(|key| Path::new(NIX_STORE).join(key))
        .collect()
}

/// Where `nix derivation show` lists a derivation's input derivations.
const INPUTS: &str = "inputs";
const INPUT_DRVS_NEWER: &str = "drvs";
const INPUT_DRVS_OLDER: &str = "inputDrvs";

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

/// A debuginfod server for one gdb session, stopped when dropped.
struct Debuginfod {
    child: Child,
    url: String,
    /// The server's cache directory, let go once the server is stopped.
    _cache: debuginfod_cache::Slot,
}

impl Debuginfod {
    /// Starts nixseparatedebuginfod2 on a free local port, or None when it
    /// is not to be had. The socket is bound here and handed over the way
    /// systemd's socket activation does, so gdb can connect before the
    /// server is ready and nothing races for the port. The server runs in
    /// a process group of its own, so the Ctrl-C meant for gdb does not
    /// stop it. Its cache is a directory under `home` that no other
    /// session's server is using.
    fn start(home: &Home) -> Option<Debuginfod> {
        let program = debuginfod_program()?;
        let cache = debuginfod_cache::take(&home.debuginfod_cache()).ok()?;
        let listener = TcpListener::bind(GDB_LOCAL).ok()?;
        let url = format!("http://{}", listener.local_addr().ok()?);
        let fd = listener.as_raw_fd();
        let lock_fd = cache.lock().as_raw_fd();

        // A shell sets LISTEN_PID to its own pid and execs the server,
        // which keeps the pid, as the protocol needs.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#"export LISTEN_PID=$$ LISTEN_FDS=1; exec "$0" "$@""#)
            .arg(&program);
        for substituter in DEBUGINFOD_SUBSTITUTERS {
            cmd.args(["--substituter", substituter]);
        }
        cmd.arg("--cache-dir")
            .arg(cache.dir())
            .args(["--expiration", DEBUGINFOD_EXPIRATION])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);

        // The server ends with rewind however rewind ends, even by a signal
        // that skips Drop, as when the app cancels a lookup by stopping
        // its process group, which the server is not in. Starting it on a
        // thread that ends sooner would end it then too; `Symbols::load`
        // runs on the thread the command runs on.
        let parent = std::process::id();
        // SAFETY: prctl, getppid, dup2 and fcntl are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if u32::try_from(libc::getppid()).ok() != Some(parent) {
                    return Err(std::io::Error::other(
                        "rewind ended before the server started",
                    ));
                }

                // The cache's lock goes to the server open, so the
                // directory stays taken until the server ends. A lock on
                // the listening socket's number is copied off it first.
                let kept = if lock_fd == LISTEN_FD {
                    libc::fcntl(lock_fd, libc::F_DUPFD, LISTEN_FD + 1)
                } else {
                    libc::fcntl(lock_fd, libc::F_SETFD, 0)
                };
                if kept < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                // The listening socket goes to the descriptor socket
                // activation names.
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
        Some(Debuginfod {
            child,
            url,
            _cache: cache,
        })
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

    /// A file written into the session is named by its path in the VM,
    /// and the vDSO, which is no file there, as the VM's map names it.
    #[test]
    fn a_dead_process_s_files_are_read_at_its_last_event_before() {
        // The step of process 140's latest event before the one asked
        // about; another process's events and its own later ones do not
        // count, and with none before there is no step to read them at.
        let event = |step, pid| rewind_trace::Event {
            step,
            pid,
            tid: pid,
            kind: rewind_trace::EventKind::Output {
                fd: 1,
                bytes: b"x".to_vec(),
            },
        };
        let trace = rewind_trace::Trace {
            events: vec![event(3788, 140), event(3795, 147), event(6299, 140)],
        };
        assert_eq!(last_event_before(&trace, 140, 6298), Some(3788));
        assert_eq!(last_event_before(&trace, 140, 6300), Some(6299));
        assert_eq!(last_event_before(&trace, 140, 3788), None);
        assert_eq!(last_event_before(&trace, 147, 3795), None);
    }

    #[test]
    fn a_session_file_is_named_as_the_vm_had_it() {
        let dir = Path::new("/home/u/.local/share/rewind/gdb/42");
        assert_eq!(
            path_in_vm(&dir.join("build/source/helper"), dir),
            PathBuf::from("/build/source/helper")
        );
        assert_eq!(
            path_in_vm(&dir.join("[vdso]"), dir),
            PathBuf::from("[vdso]")
        );
        assert_eq!(
            path_in_vm(Path::new("/nix/store/abc-glibc/lib/libc.so.6"), dir),
            PathBuf::from("/nix/store/abc-glibc/lib/libc.so.6")
        );
    }

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

    /// A derivation's input derivations, from `nix derivation show` as
    /// recent Nix prints them, by name under `inputs.drvs`, and as older
    /// versions print them, by path under `inputDrvs`; none from output
    /// that is neither.
    #[test]
    fn a_derivation_s_inputs_are_read_from_either_shape() {
        let wrapper = PathBuf::from("/nix/store/f6q-rustc-wrapper-1.91.1.drv");
        let newer = br#"{"derivations":{"yfd-polyglot-rust.drv":{"inputs":{"drvs":{"f6q-rustc-wrapper-1.91.1.drv":{"outputs":["out"]}},"srcs":[]}}},"version":4}"#;
        assert_eq!(derivation_inputs(newer), std::slice::from_ref(&wrapper));
        let older = br#"{"/nix/store/yfd-polyglot-rust.drv":{"inputDrvs":{"/nix/store/f6q-rustc-wrapper-1.91.1.drv":["out"]}}}"#;
        assert_eq!(derivation_inputs(older), [wrapper]);
        assert!(derivation_inputs(b"{}").is_empty());
    }

    /// A process's language helpers are sourced before gdb connects, each
    /// after its directory is put where gdb's Python imports from when it
    /// has one. Builds the arguments for Rust's printers and CPython's
    /// commands.
    #[test]
    fn helpers_are_sourced_with_their_imports() {
        let etc = PathBuf::from("/nix/store/abc-rustc/lib/rustlib/etc");
        let process = Process {
            helpers: vec![
                Helper {
                    import_from: Some(etc.clone()),
                    script: etc.join(RUST_PRINTERS),
                },
                Helper {
                    import_from: None,
                    script: PathBuf::from("/nix/store/def-python3/share/gdb/libpython.py"),
                },
            ],
            ..Process::default()
        };
        let address = "127.0.0.1:1234".parse().unwrap();
        let args = arguments(&KernelSymbols::none(), &process, &[], Some(address));
        let at = |needle: &str| {
            args.iter()
                .position(|a| a == needle)
                .unwrap_or_else(|| panic!("{needle} in {args:?}"))
        };
        let import =
            at("python import sys; sys.path.insert(0, \"/nix/store/abc-rustc/lib/rustlib/etc\")");
        let rust =
            at("source /nix/store/abc-rustc/lib/rustlib/etc/gdb_load_rust_pretty_printers.py");
        let python = at("source /nix/store/def-python3/share/gdb/libpython.py");
        assert!(import < rust && python < at("target remote 127.0.0.1:1234"));
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

    /// A process whose loader listed its libraries has the main program as
    /// gdb's symbol file and executable, at its load offset, the kernel
    /// added beside it, and the listed libraries left to gdb's shared
    /// library list, read once gdb connects and with the root as the
    /// place their paths are found. A file the list lacks, such as the
    /// vDSO, is still added. Builds the arguments for a program, libc and
    /// the vDSO, the first two listed.
    #[test]
    fn listed_libraries_are_left_to_gdb_s_shared_library_list() {
        let file = |path: &str, offset, origin| SymbolFile {
            path: PathBuf::from(path),
            offset,
            origin,
        };
        let process = Process {
            files: vec![
                file(
                    "/nix/store/abc-python3/bin/python3",
                    0x5555_0000_0000,
                    Origin::Store,
                ),
                file(
                    "/nix/store/abc-glibc/lib/libc.so.6",
                    0x7f00_0000_0000,
                    Origin::Store,
                ),
                file("/session/[vdso]", 0x7ffd_0000_0000, Origin::Memory),
            ],
            listed: Some(Listed {
                main: 0,
                libraries: vec![1],
            }),
            ..Process::default()
        };
        let kernel = KernelSymbols {
            file: Some(PathBuf::from("/opt/rewind/vmlinux")),
            scripts: None,
            sources: None,
            dwarf: true,
        };
        let address = "127.0.0.1:1234".parse().unwrap();
        let args = arguments(&kernel, &process, &[], Some(address));
        let at = |needle: &str| {
            args.iter()
                .position(|a| a.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {args:?}"))
        };

        let main = "/nix/store/abc-python3/bin/python3";
        assert!(at(&format!("exec-file {main}")) < at("symbol-file -o 0x555500000000"));
        assert!(at("symbol-file -o") < at("add-symbol-file /opt/rewind/vmlinux"));
        assert!(
            !args.iter().any(|a| a == "file /opt/rewind/vmlinux"),
            "{args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains("add-symbol-file /nix/store/abc-glibc"))
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains(&format!("add-symbol-file {main}")))
        );
        at("add-symbol-file /session/[vdso]");
        assert!(at("set sysroot /") < at("target remote"));
        assert!(at("target remote") < at("sharedlibrary"));
    }

    /// The vDSO, read out of the VM's memory, has debug info on no server,
    /// so gdb loads it with debuginfod off and asks for none, and says
    /// nothing of a download that would never come; a library loads with
    /// debuginfod on.
    #[test]
    fn the_vdso_is_loaded_without_asking_debuginfod() {
        let file = |path: &str, origin| SymbolFile {
            path: PathBuf::from(path),
            offset: 0x7f00_0000_0000,
            origin,
        };
        let process = Process {
            files: vec![
                file("/nix/store/abc-glibc/lib/libc.so.6", Origin::Store),
                file("/session/[vdso]", Origin::Memory),
            ],
            ..Process::default()
        };
        let urls = ["http://127.0.0.1:1"];
        let args = arguments(&KernelSymbols::none(), &process, &urls, None);
        let at = |needle: &str| {
            args.iter()
                .position(|a| a.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {args:?}"))
        };
        let on = args.iter().rposition(|a| a == DEBUGINFOD_ON).unwrap();
        let (off, vdso) = (at(DEBUGINFOD_OFF), at("[vdso]"));
        assert!(off < vdso && vdso < on, "{args:?}");
        assert!(at("libc.so.6") < off, "{args:?}");

        let quiet = arguments(&KernelSymbols::none(), &process, &[], None);
        assert!(
            !quiet.iter().any(|a| a.contains(DEBUGINFOD_OFF)),
            "{quiet:?}"
        );
    }

    /// gdb's major version is the start of the last word of the first
    /// line `gdb --version` prints, as upstream, Ubuntu and Fedora builds
    /// print it; a line without one has none.
    #[test]
    fn gdb_s_major_version_is_read_from_its_banner() {
        assert_eq!(major_version("GNU gdb (GDB) 17.2"), Some(17));
        assert_eq!(
            major_version("GNU gdb (Ubuntu 15.0.50.20240403-0ubuntu1) 15.0.50.20240403-git"),
            Some(15)
        );
        assert_eq!(
            major_version("GNU gdb (Fedora Linux) 16.3-1.fc42"),
            Some(16)
        );
        assert_eq!(major_version("not gdb"), None);
    }

    /// A gdb that exits without connecting, as `gdb --version` does, is
    /// not waited for; one that connects is served. `true` stands in for
    /// the first, a thread connecting while `sleep` runs for the second.
    #[test]
    fn a_gdb_that_never_connects_is_not_waited_for() {
        let listener = TcpListener::bind(GDB_LOCAL).unwrap();
        let mut gdb = Command::new("true").spawn().unwrap();
        assert!(accept_while_running(&listener, &mut gdb).unwrap().is_none());

        let mut gdb = Command::new("sleep").arg("5").spawn().unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || std::net::TcpStream::connect(address).unwrap());
        assert!(accept_while_running(&listener, &mut gdb).unwrap().is_some());
        client.join().unwrap();
        gdb.kill().unwrap();
        gdb.wait().unwrap();
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
        let sourced = |args: &[String]| {
            args.iter()
                .any(|a| a.ends_with("source /opt/rewind/vmlinux-gdb.py"))
        };

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
