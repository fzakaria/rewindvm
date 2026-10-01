//! `rewind gdb`: gdb on a fork of a run at a step, with every symbol this
//! machine can find for what was running there.
//!
//! The kernel's symbols come from its package: vmlinux with its symbol
//! table next to the bzImage, and its DWARF in the package's `debug`
//! output, fetched the first time someone debugs. The process that was
//! running at the step is found by an inspection on a second fork, and
//! each program and library it had mapped from the store is loaded at the
//! address it was loaded at in the VM.
//!
//! DWARF and source files for all of them come from a debuginfod server
//! started for the session, nixseparatedebuginfod2, which serves the
//! `debug` outputs in the local store and on cache.nixos.org, with the
//! source files they were built from.

use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

use anyhow::{Context, Result};
use rewind_core::inspect::Inspection;
use rewind_core::maps::{Running, SymbolFile};
use rewind_core::{Home, Run};

/// The address `rewind gdb` serves on when it starts gdb itself: any free
/// port on the loopback interface.
const GDB_LOCAL: &str = "127.0.0.1:0";

/// Where Nix keeps store paths, which `nix-store --realise` can fetch.
const NIX_STORE: &str = "/nix/store";

/// The binary cache Rewind's own builds are in, the kernel's `debug`
/// output among them. Cachix keeps no index of DWARF by build ID, so a
/// debuginfod server cannot find the kernel's there; `rewind gdb` fetches
/// the output itself. Nix uses a cache named on the command line only for
/// a user it trusts, or one already in its settings, as the flake's
/// nixConfig puts it.
const REWIND_CACHE: &str = "https://rewindvm.cachix.org";
const REWIND_CACHE_KEY: &str = "rewindvm.cachix.org-1:N5gL5fQTxBxim2HQNlYVWNYLK1ttOmMU1GP43eo4V5g=";

/// The kernel package's files: vmlinux with its symbol table and the
/// kernel's gdb scripts, next to the bzImage; in the `debug` output,
/// vmlinux's DWARF, linked by name as well as by build ID.
const VMLINUX: &str = "vmlinux";
const GDB_SCRIPTS: &str = "vmlinux-gdb.py";
const DEBUG_VMLINUX: &str = "lib/debug/vmlinux";

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

/// Serves gdb on a fork of `run` at `step`: on `listen` if given, else on
/// a free local port with the host's gdb started against it. Ctrl-C
/// belongs to gdb, which turns it into an interrupt for the fork, so this
/// process ignores it meanwhile.
pub fn gdb(home: &Home, run: &Run, step: u64, listen: Option<&str>) -> Result<ExitCode> {
    // What gdb is told before the fork it debugs is made: the inspection
    // that finds the running process needs a fork of its own.
    let kernel = KernelSymbols::find(run);
    let session = Session::new(home)?;
    let process = running_process(home, run, step, &session.dir);
    let debuginfod = Debuginfod::start();

    let machine = run.machine_at(home, step, &mut rewind_vmm::Ignore)?;
    let mut debuggee = rewind_core::debug::Debuggee::new(machine);
    let listener = TcpListener::bind(listen.unwrap_or(GDB_LOCAL)).context("listening for gdb")?;
    let address = listener.local_addr()?;
    let args = arguments(&kernel, &process, debuginfod.as_ref(), address);

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
    let mut child = Command::new("gdb").args(&args).spawn().context(
        "starting gdb; is it on PATH? `rewind gdb --listen 127.0.0.1:1234` serves without it",
    )?;
    eprintln!("rewind: gdb at step {step} of {}", run.manifest.id);
    let (conn, _) = listener.accept()?;
    let served = debuggee.serve(conn);
    let status = child.wait()?;
    served?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// gdb's arguments: the servers to ask for DWARF, the kernel's symbols
/// and scripts, the running process's files, then the connection. Each is
/// its own -ex, so one gdb cannot run, such as a debuginfod setting in a
/// gdb built without debuginfod, does not stop the rest.
fn arguments(
    kernel: &KernelSymbols,
    process: &Process,
    debuginfod: Option<&Debuginfod>,
    address: std::net::SocketAddr,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-q".into()];
    let mut ex = |when: &str, command: String| {
        args.push(when.into());
        args.push(command);
    };

    // The session's server first, then any the person already uses.
    let others = std::env::var(ENV_DEBUGINFOD_URLS).unwrap_or_default();
    let urls: Vec<&str> = debuginfod
        .map(|d| d.url.as_str())
        .into_iter()
        .chain(others.split_whitespace())
        .collect();
    if !urls.is_empty() {
        ex("-iex", "set debuginfod enabled on".into());
        ex("-iex", format!("set debuginfod urls {}", urls.join(" ")));
    }

    // The kernel: its DWARF when it is here, else its symbol table, whose
    // DWARF a debuginfod server may have. The scripts need the DWARF.
    if let Some(file) = &kernel.file {
        ex("-ex", format!("file {}", file.display()));
        if let Some(sources) = &kernel.sources {
            ex("-ex", format!("directory {}", sources.display()));
        }
        if let Some(scripts) = &kernel.scripts
            && (kernel.dwarf || !urls.is_empty())
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
    for file in &process.files {
        ex(
            "-ex",
            format!(
                "with confirm off -- add-symbol-file {} -o {:#x}",
                file.path.display(),
                file.offset
            ),
        );
    }

    ex("-ex", format!("target remote {address}"));
    args
}

/// The kernel's symbol file for gdb, the gdb scripts to load with it, and
/// the patched source files.
struct KernelSymbols {
    file: Option<PathBuf>,
    scripts: Option<PathBuf>,
    sources: Option<PathBuf>,
    /// Whether `file` has the DWARF, not only the symbol table.
    dwarf: bool,
}

impl KernelSymbols {
    /// The best symbols for the kernel `run` booted: its DWARF, fetched
    /// from a binary cache when it is not on this machine, else its symbol
    /// table.
    fn find(run: &Run) -> KernelSymbols {
        let dir = run.manifest.spec.kernel.parent().unwrap_or(Path::new("/"));
        let scripts = Some(dir.join(GDB_SCRIPTS)).filter(|p| p.exists());

        if let Some(debug) = &run.manifest.spec.kernel_debug
            && realise(&debug.join(DEBUG_VMLINUX), "the kernel's debug symbols")
        {
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
        KernelSymbols {
            file: Some(dir.join(VMLINUX)).filter(|p| p.exists()),
            scripts,
            sources: None,
            dwarf: false,
        }
    }
}

/// What gdb is told about the process running at a step.
#[derive(Default)]
struct Process {
    /// Its programs and libraries, with their load offsets.
    files: Vec<SymbolFile>,
    /// Directories of source files only the VM had, as the VM names them,
    /// each with where its files were written here.
    source_dirs: Vec<(PathBuf, PathBuf)>,
}

/// The process running at `step`: its programs and libraries, from this
/// machine's store or, when only the VM has them, written under `dir`,
/// with the source files those name. Empty when the kernel was running
/// or the map could not be read.
fn running_process(home: &Home, run: &Run, step: u64, dir: &Path) -> Process {
    let answer = match rewind_core::inspect::running(home, run, step) {
        Ok(Inspection::Contents(bytes)) => bytes,
        Ok(Inspection::NotFound(_)) => {
            eprintln!("rewind: step {step} ran in the kernel, in no process");
            return Process::default();
        }
        Ok(Inspection::Failed(message)) => {
            eprintln!("rewind: no symbols for the running process: {message}");
            return Process::default();
        }
        Err(e) => {
            eprintln!("rewind: no symbols for the running process: {e:#}");
            return Process::default();
        }
    };
    let Some(running) = Running::parse(&answer) else {
        eprintln!("rewind: no symbols for the running process: its answer was cut short");
        return Process::default();
    };
    let files = match running.symbol_files(dir) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("rewind: no symbols for the running process: {e}");
            return Process::default();
        }
    };

    // Say what gdb will know about, and what it cannot.
    eprintln!(
        "rewind: step {step} ran in process {}; loading symbols for {} of its files",
        running.pid,
        files.len()
    );
    let missing = running.missing(dir);
    if !missing.is_empty() {
        eprintln!("rewind: no symbols for {}", missing.join(", "));
    }

    let sent: Vec<&Path> = files
        .iter()
        .map(|f| f.path.as_path())
        .filter(|p| p.starts_with(dir))
        .collect();
    let source_dirs = fetch_sources(home, run, step, running.pid, &sent, dir);
    Process { files, source_dirs }
}

/// Fetches from a fork the source files that `programs`, which only the
/// VM had, were built from, and writes them under `dir`. Returns the tree
/// each was in, with where it is here, for gdb's substitute-path. Sources
/// in the store are left to debuginfod.
fn fetch_sources(
    home: &Home,
    run: &Run,
    step: u64,
    pid: u32,
    programs: &[&Path],
    dir: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let paths: Vec<String> = programs
        .iter()
        .flat_map(|program| source_files(program))
        .filter(|p| !p.starts_with(NIX_STORE))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if paths.is_empty() {
        return Vec::new();
    }

    let answer = match rewind_core::inspect::files(home, run, step, Some(pid), &paths) {
        Ok(Inspection::Contents(bytes)) => bytes,
        Ok(Inspection::NotFound(message) | Inspection::Failed(message)) => {
            eprintln!("rewind: no source files: {message}");
            return Vec::new();
        }
        Err(e) => {
            eprintln!("rewind: no source files: {e:#}");
            return Vec::new();
        }
    };
    let Some(sections) = rewind_init::sections(&answer) else {
        eprintln!("rewind: no source files: the answer was cut short");
        return Vec::new();
    };

    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut fetched = 0;
    for (path, bytes) in sections {
        let to = dir.join(path.trim_start_matches('/'));
        let written = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&to, bytes));
        if written.is_err() {
            continue;
        }
        fetched += 1;
        let from = source_tree(Path::new(&path));
        if !dirs.iter().any(|(f, _)| *f == from) {
            let here = dir.join(from.strip_prefix("/").unwrap_or(&from));
            dirs.push((from, here));
        }
    }
    eprintln!("rewind: fetched {fetched} source files from the VM");
    dirs
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

/// The absolute paths of the source files a program's DWARF names, as gdb
/// lists them: `info sources` prints the program's name and a colon, then
/// the files, separated by commas.
fn source_files(program: &Path) -> Vec<String> {
    let Ok(output) = Command::new("gdb")
        .args(["-batch", "-nx", "-ex", "info sources"])
        .arg(program)
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .split([',', '\n'])
        .map(str::trim)
        .filter(|p| p.starts_with('/') && !p.ends_with(':'))
        .map(str::to_string)
        .collect()
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

/// Fetches `path` from a binary cache with `nix-store --realise` when it
/// is a store path not on this machine. Whether it is here now.
fn realise(path: &Path, what: &str) -> bool {
    if path.exists() {
        return true;
    }
    let Some(root) = store_root(path) else {
        return false;
    };
    eprintln!("rewind: fetching {what}, {}", root.display());
    let _ = Command::new("nix-store")
        .arg("--realise")
        .arg(&root)
        .args(["--option", "extra-substituters", REWIND_CACHE])
        .args(["--option", "extra-trusted-public-keys", REWIND_CACHE_KEY])
        .stdout(Stdio::null())
        .status();
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
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(DEBUGINFOD_PROGRAM))
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

    #[test]
    fn arguments_with_spaces_or_quotes_are_quoted() {
        assert_eq!(quote("-q"), "-q");
        assert_eq!(quote("target remote x"), "'target remote x'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
