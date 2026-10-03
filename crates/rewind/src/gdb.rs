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
//! fork reads for them.
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

/// Serves gdb on a fork of `run` at `step`: on `listen` if given, else on
/// a free local port with the host's gdb started against it, with `extra`
/// after the arguments this makes. Ctrl-C belongs to gdb, which turns it
/// into an interrupt for the fork, so this process ignores it meanwhile.
pub fn gdb(
    home: &Home,
    run: &Run,
    step: u64,
    listen: Option<&str>,
    extra: &[String],
) -> Result<ExitCode> {
    // What gdb is told before the fork it debugs is made: the inspection
    // that finds the running process needs a fork of its own.
    let kernel = KernelSymbols::find(run);
    let session = Session::new(home)?;
    let process = running_process(home, run, step, &session.dir);
    let debuginfod = Debuginfod::start();

    let machine = run.machine_at(home, step, &mut rewind_vmm::Ignore)?;
    let made = run.records_after(step)?;
    let mut debuggee = rewind_core::debug::Debuggee::new(machine, made, process.scope)?;
    let listener = TcpListener::bind(listen.unwrap_or(GDB_LOCAL)).context("listening for gdb")?;
    let address = listener.local_addr()?;
    // The session's server first, then any the person already uses.
    let others = std::env::var(ENV_DEBUGINFOD_URLS).unwrap_or_default();
    let urls: Vec<&str> = debuginfod
        .as_ref()
        .map(|d| d.url.as_str())
        .into_iter()
        .chain(others.split_whitespace())
        .collect();
    let mut args = arguments(&kernel, &process, &urls, address);
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

/// gdb's arguments: the debuginfod servers to ask for DWARF, the kernel's symbols
/// and scripts, the running process's files, then the connection. Each is
/// its own -ex, so one gdb cannot run, such as a debuginfod setting in a
/// gdb built without debuginfod, does not stop the rest.
fn arguments(
    kernel: &KernelSymbols,
    process: &Process,
    urls: &[&str],
    address: std::net::SocketAddr,
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
    // Each is loaded through gdb's Python, which keeps add-symbol-file's
    // line about the file and its offset to itself.
    for file in &process.files {
        let command = format!(
            "with confirm off -- add-symbol-file {} -o {:#x}",
            file.path.display(),
            file.offset
        );
        ex(
            "-ex",
            format!(
                "python gdb.execute({}, to_string=True)",
                python_string(&command)
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
    /// Whether `file` has the DWARF as well as the symbol table.
    dwarf: bool,
}

impl KernelSymbols {
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
    Process {
        scope: rewind_core::debug::Scope::Process,
        files,
        source_dirs,
    }
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

/// A Python string literal holding `text`.
fn python_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
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

    #[test]
    fn a_python_string_escapes_quotes_and_backslashes() {
        assert_eq!(python_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
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

        let without = arguments(&kernel(false), &Process::default(), &urls, address);
        assert!(!sourced(&without));
        let at = |command: &str| without.iter().position(|a| a == command);
        let off = at("set auto-load python-scripts off").expect("auto-load is turned off");
        let file = at("file /opt/rewind/vmlinux").expect("the kernel is loaded");
        let on = at("set auto-load python-scripts on").expect("auto-load is turned back on");
        assert!(off < file && file < on);
        assert!(without.contains(&"set debuginfod urls https://debuginfod.debian.net".to_string()));

        let with = arguments(&kernel(true), &Process::default(), &urls, address);
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
