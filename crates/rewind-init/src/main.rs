//! The guest's PID 1.
//!
//! Sets up the filesystems the job expects, runs it with its output on the
//! Rewind devices, reaps every process until the job's main process has
//! exited, reports the exit status as a mark, and powers the machine off.
//! Nothing here reads the clock or any other source of variation, so the
//! init is as deterministic as the rest of the guest.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rewind_init::{
    EXIT_MARK, INSPECT_ARG, INSPECT_BEGIN_MARK, INSPECT_CAT, INSPECT_END_MARK, INSPECT_FILES,
    INSPECT_RUNNING, INSPECT_SHELL, INSPECT_WITH, InspectStatus, JOB_PATH, Job, OUTPUT_MARK,
    RESIZE_ESCAPE, RESIZE_LEN, RESIZE_TAG, RUNNING_ENV, Root, SECTION_MAPS, SECTION_PID,
    START_MARK, nar_hash, section_header,
};

/// The image the monitor maps as persistent memory.
const IMAGE_DEVICE: &str = "/dev/pmem0";

/// How long to wait for the image device, in virtual time. The nvdimm bus
/// registers its devices from an async domain the kernel does not wait on
/// before starting init, so the device can appear after init starts. The
/// wait is part of the run, and as deterministic as the rest of it.
const DEVICE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
const DEVICE_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// Where output goes: one device per stream, so each write reaches the
/// monitor tagged with the pid that made it.
const STDOUT_DEVICE: &str = "/dev/rewind-stdout";
const STDERR_DEVICE: &str = "/dev/rewind-stderr";
const MARK_DEVICE: &str = "/dev/rewind";

type Result<T> = std::result::Result<T, String>;

/// The argument that turns this binary into the counter self-test's
/// workload instead of PID 1.
const SELFTEST_ARG: &str = "--selftest";

/// Threads and atomic additions per thread in the self-test.
const SELFTEST_THREADS: usize = 4;
const SELFTEST_ADDS: u64 = 5_000_000;

/// Where an image job's root is mounted; init chroots into it, and takes
/// /dev and /proc along.
const IMAGE_ROOT: &str = "/newroot";

/// How much of a file an inspection reads at a time.
const INSPECT_CHUNK: usize = 64 * 1024;

/// Where the input image's store paths are, as init sees them, and the
/// overlay's writable layer above them, which holds every store path the
/// job wrote. A store path not in the writable layer is one the host has.
const STORE_PREFIX: &str = "/nix/store/";
const STORE_UPPER: &str = "/nix/.rw-store/upper";

/// The largest mapped file a `running` inspection sends.
const MAX_SENT_FILE: u64 = 256 * 1024 * 1024;

/// The first bytes of an ELF file.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

/// The device a shell inspection's terminal is relayed through.
const CONSOLE_DEVICE: &str = "/dev/rewind-console";

/// A static busybox and a directory of its applets, in the initramfs, for
/// shells: its ash has line editing, history and completion, and its
/// applets stand in for tools a job's PATH lacks. Inside an image job's
/// root, which cannot see the initramfs, they are bind-mounted at
/// TOOLS_IN_IMAGE for the fork.
const TOOLS_DIR: &str = "/rewind/tools";
const TOOLS_IN_IMAGE: &str = "/.rewind-tools";

/// The terminal type a shell is told it has, and its prompt when the
/// environment sets none.
const SHELL_TERM: &str = "xterm-256color";

/// The extras slot's device, filled by Rewind for `rewind shell --with`:
/// the second persistent memory device when the job has an input image,
/// the first when it has none. Mounted here, then each store path in it is
/// bind-mounted into the view's /nix/store.
const EXTRAS_DEVICE_AFTER_IMAGE: &str = "/dev/pmem1";
const EXTRAS_DEVICE_ALONE: &str = "/dev/pmem0";
const EXTRAS_MOUNT: &str = "/rewind/extras";

/// BLKFLSBUF, _IO(0x12, 97): drop a block device's cached contents.
const BLKFLSBUF: u64 = 0x1261;
const SHELL_PROMPT: &str = "[rewind] \\w \\$ ";

/// How far up the process tree to look for a live environment.
const MAX_ANCESTORS: usize = 64;

/// The files besides the affinity system calls that say how many CPUs
/// there are, and the directory init writes its own versions of them to.
const CPU_LIST_FILES: [&str; 3] = [
    "/sys/devices/system/cpu/online",
    "/sys/devices/system/cpu/possible",
    "/sys/devices/system/cpu/present",
];
const CPUINFO_FILE: &str = "/proc/cpuinfo";
const REPORTED_CPUS_DIR: &str = "/rewind/cpus";

/// The line that opens CPU 0's block in /proc/cpuinfo.
const CPUINFO_FIRST: &str = "processor\t: 0";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(SELFTEST_ARG) {
        selftest();
        return;
    }
    if args.get(1).map(String::as_str) == Some(INSPECT_ARG) {
        inspect(&args[2..]);
        return;
    }
    if let Err(e) = run() {
        eprintln!("rewind-init: {e}");
        mark(&format!("{EXIT_MARK}{}", 127 << 8));
    }
    power_off();
}

fn run() -> Result<()> {
    // The kernel does not mount devtmpfs over an initramfs, so the first
    // thing is somewhere to find the Rewind devices.
    mount("devtmpfs", "/dev", "devtmpfs", 0, "")?;
    redirect_own_output();

    mount("proc", "/proc", "proc", 0, "")?;
    mount("sysfs", "/sys", "sysfs", 0, "")?;
    report_cpus()?;
    mkdir("/dev/pts")?;
    mount("devpts", "/dev/pts", "devpts", 0, "ptmxmode=0666,mode=0620")?;
    mkdir("/dev/shm")?;
    mount("tmpfs", "/dev/shm", "tmpfs", 0, "mode=1777")?;
    dev_links()?;

    let job: Job = serde_json::from_slice(
        &fs::read(JOB_PATH).map_err(|e| format!("reading {JOB_PATH}: {e}"))?,
    )
    .map_err(|e| format!("parsing {JOB_PATH}: {e}"))?;

    set_hostname(&job.hostname)?;
    loopback_up()?;

    if job.root != Root::Initramfs {
        wait_for(IMAGE_DEVICE)?;
    }
    // The job's root. Only the job is chrooted into an image, never init:
    // init shares its root with the kernel's threads, and the kernel starts
    // inspections from this root.
    let root = match job.root {
        Root::Store => {
            store_root(&job)?;
            "/"
        }
        Root::Image => {
            image_root()?;
            IMAGE_ROOT
        }
        Root::Initramfs => "/",
    };

    for f in &job.files {
        let path = within(root, &f.path);
        fs::write(&path, &f.contents).map_err(|e| format!("writing {}: {e}", f.path))?;
        chown(&path.to_string_lossy(), job.uid, job.gid)?;
    }

    mark(START_MARK);
    let status = spawn_and_reap(&job, root)?;
    if status == 0 {
        for output in &job.outputs {
            match nar_hash(&within(root, output)) {
                Ok(hash) => mark(&format!("{OUTPUT_MARK}{output} {hash}")),
                Err(e) => eprintln!("rewind-init: hashing {output}: {e}"),
            }
        }
    }
    mark(&format!("{EXIT_MARK}{status}"));
    // SAFETY: sync has no preconditions.
    unsafe { libc::sync() };
    Ok(())
}

/// Nix mode: the store image read-only under a writable overlay at
/// /nix/store, and /build owned by the builder, as in the build sandbox.
fn store_root(job: &Job) -> Result<()> {
    mkdir("/nix/.ro-store")?;
    mount(IMAGE_DEVICE, "/nix/.ro-store", "erofs", libc::MS_RDONLY, "")?;
    mkdir("/nix/.rw-store")?;
    mount("tmpfs", "/nix/.rw-store", "tmpfs", 0, "mode=0755")?;
    mkdir("/nix/.rw-store/upper")?;
    mkdir("/nix/.rw-store/work")?;
    mkdir("/nix/store")?;
    mount(
        "overlay",
        "/nix/store",
        "overlay",
        0,
        "lowerdir=/nix/.ro-store,upperdir=/nix/.rw-store/upper,workdir=/nix/.rw-store/work",
    )?;

    // The builder writes its outputs straight into the store, as it does
    // in the sandbox, where the store is group-writable by nixbld.
    chown("/nix/store", 0, job.gid)?;
    chmod("/nix/store", 0o1775)?;
    mkdir(&job.cwd)?;
    chown(&job.cwd, job.uid, job.gid)?;
    chmod(&job.cwd, 0o700)?;
    Ok(())
}

/// A path inside the job's root, as init sees it.
fn within(root: &str, path: &str) -> PathBuf {
    Path::new(root).join(path.trim_start_matches('/'))
}

/// Image mode: the image is a root filesystem; overlay it writable at
/// IMAGE_ROOT, with /dev, /proc and /sys bound into it, for the job to be
/// chrooted into.
fn image_root() -> Result<()> {
    mkdir("/lower")?;
    mount(IMAGE_DEVICE, "/lower", "erofs", libc::MS_RDONLY, "")?;
    mkdir("/rw")?;
    mount("tmpfs", "/rw", "tmpfs", 0, "mode=0755")?;
    mkdir("/rw/upper")?;
    mkdir("/rw/work")?;
    mkdir("/newroot")?;
    mount(
        "overlay",
        "/newroot",
        "overlay",
        0,
        "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work",
    )?;
    for dir in ["/dev", "/proc", "/sys"] {
        let target = format!("{IMAGE_ROOT}{dir}");
        mkdir(&target)?;
        mount(dir, &target, "", libc::MS_BIND | libc::MS_REC, "")?;
    }
    for dir in ["/tmp", "/build"] {
        let dir = format!("{IMAGE_ROOT}{dir}");
        mkdir(&dir)?;
        chmod(&dir, 0o1777)?;
    }
    Ok(())
}

/// Runs the job and reaps every process until its main process exits,
/// then stops the rest. Returns the main process's wait status.
fn spawn_and_reap(job: &Job, root: &str) -> Result<i32> {
    let program = resolve(job, root)?;
    let stdout = open_device(STDOUT_DEVICE)?;
    let stderr = open_device(STDERR_DEVICE)?;

    let mut cmd = Command::new(&program);
    cmd.arg0(&job.argv[0])
        .args(&job.argv[1..])
        .env_clear()
        .envs(job.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);

    // In the child: into the root, then the working directory, then the
    // job's identity, in that order, since a chroot needs root and the
    // working directory is inside it. The strings are made before the fork.
    let root_c = CString::new(root).map_err(|e| e.to_string())?;
    let cwd_c = CString::new(job.cwd.as_str()).map_err(|e| e.to_string())?;
    let (uid, gid) = (job.uid, job.gid);
    // SAFETY: chroot, chdir, setgroups, setgid, setuid and setsid are
    // async-signal-safe, and the pointers outlive the call.
    unsafe {
        cmd.pre_exec(move || {
            let check = |rc: libc::c_int| {
                if rc == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            check(libc::chroot(root_c.as_ptr()))?;
            check(libc::chdir(cwd_c.as_ptr()))?;
            check(libc::setgroups(0, std::ptr::null()))?;
            check(libc::setgid(gid))?;
            check(libc::setuid(uid))?;
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("starting {}: {e}", program.display()))?;
    let main_pid = child.id() as i32;

    // PID 1 reaps orphans too, so wait for anything and pick out the job.
    let mut main_status = None;
    loop {
        let mut status = 0;
        // SAFETY: a valid out pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            break;
        }
        if pid == main_pid {
            main_status = Some(status);
            // Whatever the job left behind stops with it, as it would when
            // Nix tears down a build sandbox.
            // SAFETY: signalling every other process is what PID 1 may do.
            unsafe { libc::kill(-1, libc::SIGKILL) };
        }
    }
    main_status.ok_or_else(|| "the job's main process was never reaped".to_string())
}

/// The counter self-test's workload: threads adding to one atomic with
/// lock-prefixed instructions, the pattern AMD's branch counter overcounts
/// on, interleaved with futex handoffs. It prints the total so a broken
/// run shows.
fn selftest() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    let total = Arc::new(AtomicU64::new(0));
    let handoff = Arc::new(Mutex::new(0u64));
    let threads: Vec<_> = (0..SELFTEST_THREADS)
        .map(|_| {
            let total = Arc::clone(&total);
            let handoff = Arc::clone(&handoff);
            std::thread::spawn(move || {
                for i in 0..SELFTEST_ADDS {
                    total.fetch_add(i & 7, Ordering::SeqCst);
                    if i % 4096 == 0 {
                        *handoff.lock().unwrap() += 1;
                    }
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    println!("selftest {}", total.load(Ordering::SeqCst));
}

/// An inspection, started by the kernel in a forked run at Rewind's
/// request, outside the job and with init's root. The kernel has stopped
/// every other process, so what it reads is the machine at the step asked
/// about.
fn inspect(request: &[String]) {
    // The devices are opened now, before entering the job's view, which
    // may not have them.
    let answer = |status: InspectStatus| {
        mark(&format!("{INSPECT_END_MARK}{}", status.code()));
    };
    mark(INSPECT_BEGIN_MARK);

    // Errors go to standard error, where Rewind shows them.
    let (Ok(mut out), Ok(mut err)) = (open_device(STDOUT_DEVICE), open_device(STDERR_DEVICE))
    else {
        answer(InspectStatus::Failed);
        return;
    };

    let status = match request {
        [op, pid, path] if op == INSPECT_CAT => {
            cat(pid.parse().unwrap_or(0), path, &mut out, &mut err)
        }
        [op] if op == INSPECT_RUNNING => running(&mut out, &mut err),
        [op, pid] if op == INSPECT_FILES => files(pid.parse().unwrap_or(0), &mut out, &mut err),
        [op, pid, cols, rows, extras @ ..] if op == INSPECT_SHELL => {
            let size = (cols.parse().unwrap_or(80), rows.parse().unwrap_or(24));
            // `--with <bin dir>...`: the extras slot holds more packages.
            let with = match extras {
                [] => None,
                [flag, bins @ ..] if flag == INSPECT_WITH => Some(bins.to_vec()),
                _ => None,
            };
            match shell(pid.parse().unwrap_or(0), size, with) {
                Ok(()) => InspectStatus::Done,
                Err(e) => {
                    let _ = writeln!(err, "inspect: {e}");
                    InspectStatus::Failed
                }
            }
        }
        _ => {
            let _ = writeln!(err, "inspect: unknown request {request:?}");
            InspectStatus::Failed
        }
    };
    answer(status);
}

/// A file's bytes on `out`, as process `pid` sees it.
fn cat(pid: i32, path: &str, out: &mut File, err: &mut File) -> InspectStatus {
    // See the file as the process did: its root and working directory
    // when it is still alive, else the job's.
    if let Err(e) = enter_view_of(pid) {
        let _ = writeln!(err, "inspect: {e}");
        return InspectStatus::Failed;
    }
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = writeln!(err, "{path}: no such file at this step");
            return InspectStatus::NotFound;
        }
        Err(e) => {
            let _ = writeln!(err, "{path}: {e}");
            return InspectStatus::Failed;
        }
    };

    // The bytes, in chunks; the device splits them into records.
    let mut buf = vec![0u8; INSPECT_CHUNK];
    loop {
        match std::io::Read::read(&mut file, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    return InspectStatus::Failed;
                }
            }
            Err(e) => {
                let _ = writeln!(err, "{path}: {e}");
                return InspectStatus::Failed;
            }
        }
    }
    InspectStatus::Done
}

/// The process that was running at the step, which the kernel names in
/// REWIND_RUNNING, in sections: its pid, its memory map, and the ELF files
/// it had mapped that the host does not have. The map is read with init's
/// root, so its paths are whole paths in the VM, a job's root included.
fn running(out: &mut File, err: &mut File) -> InspectStatus {
    let pid = std::env::var(RUNNING_ENV)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    if pid <= 0 {
        let _ = writeln!(err, "no process was running at this step, only the kernel");
        return InspectStatus::NotFound;
    }

    let maps = match fs::read_to_string(format!("/proc/{pid}/maps")) {
        Ok(maps) => maps,
        Err(e) => {
            let _ = writeln!(err, "/proc/{pid}/maps: {e}");
            return InspectStatus::Failed;
        }
    };
    let mut send = |name: &str, bytes: &[u8]| {
        out.write_all(section_header(name, bytes.len()).as_bytes())
            .and_then(|()| out.write_all(bytes))
    };
    if send(SECTION_PID, pid.to_string().as_bytes()).is_err()
        || send(SECTION_MAPS, maps.as_bytes()).is_err()
    {
        return InspectStatus::Failed;
    }

    // Each file is read through /proc/<pid>/map_files, which reaches it
    // even when it has since been deleted or replaced.
    for (range, path) in files_only_here(&maps) {
        let Ok(bytes) = read_elf(&format!("/proc/{pid}/map_files/{range}")) else {
            continue;
        };
        if send(&path, &bytes).is_err() {
            return InspectStatus::Failed;
        }
    }
    InspectStatus::Done
}

/// The files Rewind lists on the console, in sections, as process `pid`
/// sees them.
fn files(pid: i32, out: &mut File, err: &mut File) -> InspectStatus {
    // The list is read before entering the process's view, which may not
    // have the console.
    let list = match read_list() {
        Ok(list) => list,
        Err(e) => {
            let _ = writeln!(err, "{CONSOLE_DEVICE}: {e}");
            return InspectStatus::Failed;
        }
    };
    if let Err(e) = enter_view_of(pid) {
        let _ = writeln!(err, "inspect: {e}");
        return InspectStatus::Failed;
    }

    for path in list.lines().filter(|l| !l.is_empty()) {
        let fits = fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= MAX_SENT_FILE);
        if !fits {
            continue;
        }
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let header = section_header(path, bytes.len());
        if out
            .write_all(header.as_bytes())
            .and_then(|()| out.write_all(&bytes))
            .is_err()
        {
            return InspectStatus::Failed;
        }
    }
    InspectStatus::Done
}

/// Lines typed on the console up to an empty one.
fn read_list() -> std::io::Result<String> {
    use std::io::Read;

    let mut console = File::open(CONSOLE_DEVICE)?;
    let mut list = Vec::new();
    let mut buf = [0u8; 4096];
    while !list.ends_with(b"\n\n") {
        let n = console.read(&mut buf)?;
        if n == 0 {
            break;
        }
        list.extend_from_slice(&buf[..n]);
    }
    Ok(String::from_utf8_lossy(&list).into_owned())
}

/// The files a memory map maps that did not come from the input image's
/// store, each with the address range of its first mapping, in the order
/// they first appear.
fn files_only_here(maps: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else {
            continue;
        };

        // The path is what follows the fifth field, padded to a column.
        let Some(path) = line.splitn(6, char::is_whitespace).nth(5) else {
            continue;
        };
        let path = path.trim_start();
        let path = path.strip_suffix(" (deleted)").unwrap_or(path);
        if !path.starts_with('/') || files.iter().any(|(_, p)| p == path) {
            continue;
        }
        if let Some(rest) = path.strip_prefix(STORE_PREFIX)
            && !Path::new(STORE_UPPER).join(rest).exists()
        {
            continue;
        }
        let Some(range) = map_files_name(range) else {
            continue;
        };
        files.push((range, path.to_string()));
    }
    files
}

/// The name /proc/<pid>/map_files gives a mapping: its address range as
/// the map shows it, but without the map's zero padding.
fn map_files_name(range: &str) -> Option<String> {
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    Some(format!("{start:x}-{end:x}"))
}

/// A file's bytes when it is an ELF file no larger than MAX_SENT_FILE.
fn read_elf(path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut file = File::open(path)?;
    let mut magic = [0u8; ELF_MAGIC.len()];
    file.read_exact(&mut magic)?;
    if &magic != ELF_MAGIC || file.metadata()?.len() > MAX_SENT_FILE {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = magic.to_vec();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// An interactive shell for Rewind, on a pty relayed through the console
/// device, in the root and working directory of process `pid`, with that
/// process's environment as it was at this step (the job's when the
/// process is gone), and the shell that environment names. For a Nix build
/// that is the builder's bash with stdenv's PATH. Returns when the shell
/// exits.
fn shell(pid: i32, (cols, rows): (u16, u16), with: Option<Vec<String>>) -> Result<()> {
    use std::os::fd::AsRawFd;

    // Everything that lives outside the job's root is opened first: the
    // console, the pty and the environment.
    let console = OpenOptions::new()
        .read(true)
        .write(true)
        .open(CONSOLE_DEVICE)
        .map_err(|e| format!("opening {CONSOLE_DEVICE}: {e}"))?;
    let job: Job = serde_json::from_slice(
        &fs::read(JOB_PATH).map_err(|e| format!("reading {JOB_PATH}: {e}"))?,
    )
    .map_err(|e| format!("parsing {JOB_PATH}: {e}"))?;
    let (master, slave) = open_pty(cols, rows)?;
    let source = nearest_live(pid);
    let mut env = source
        .and_then(live_environment)
        .unwrap_or_else(|| job.env.clone());

    // The tools, where the shell will see them once inside the view.
    let (root, cwd) = view_of(source.unwrap_or(0))?;
    let tools = if fs::canonicalize(&root).is_ok_and(|r| r == Path::new("/")) {
        TOOLS_DIR.to_string()
    } else {
        let target = format!("{root}{TOOLS_IN_IMAGE}");
        mkdir(&target)?;
        mount(TOOLS_DIR, &target, "", libc::MS_BIND, "")?;
        TOOLS_IN_IMAGE.to_string()
    };
    if with.is_some() {
        mount_extras(&root, &job)?;
    }
    enter(&root, &cwd)?;

    // The packages asked for with --with first, then the environment's
    // PATH, so the job's own tools win over the applets that come last;
    // and a prompt when the environment has none.
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| format!("{v}:{tools}/bin"))
        .unwrap_or_else(|| format!("{tools}/bin"));
    let path = match &with {
        Some(bins) if !bins.is_empty() => format!("{}:{path}", bins.join(":")),
        _ => path,
    };
    env.retain(|(k, _)| k != "PATH");
    env.push(("PATH".to_string(), path));
    if !env.iter().any(|(k, _)| k == "PS1") {
        env.push(("PS1".to_string(), SHELL_PROMPT.to_string()));
    }
    let shell = format!("{tools}/busybox");

    let slave_fd = slave.as_raw_fd();
    let mut cmd = Command::new(&shell);
    cmd.args(["ash", "-i"])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("TERM", SHELL_TERM)
        .stdin(slave.try_clone().map_err(|e| e.to_string())?)
        .stdout(slave.try_clone().map_err(|e| e.to_string())?)
        .stderr(slave);
    // SAFETY: setsid and the controlling-terminal ioctl are
    // async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::Ioctl, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("starting {shell}: {e}"))?;
    // The command holds copies of the pty's slave end; while any is open
    // here, the master never sees the shell hang up.
    drop(cmd);

    relay(&console, &master);
    let _ = child.wait();
    Ok(())
}

/// The process a look inside should take its view from: `pid` while it is
/// alive, else its nearest live ancestor, so a process that has just
/// crashed gives the view of the program that ran it. With no pid, or no
/// live ancestor short of init, the job's main process. None when there
/// is none of those either.
fn nearest_live(pid: i32) -> Option<i32> {
    let alive = |p: i32| fs::read_link(format!("/proc/{p}/cwd")).is_ok();
    let mut p = pid;
    for _ in 0..MAX_ANCESTORS {
        if p <= 1 {
            break;
        }
        if alive(p) {
            return Some(p);
        }
        p = parent_of(p)?;
    }
    main_process().filter(|p| alive(*p))
}

/// A process's parent, from /proc/<pid>/status.
fn parent_of(pid: i32) -> Option<i32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The job's main process: init's child with the lowest pid. An
/// inspection's own process is a child of kthreadd, not of init.
fn main_process() -> Option<i32> {
    fs::read_dir("/proc")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|p| *p > 1 && parent_of(*p) == Some(1))
        .min()
}

/// Mounts the extras slot's image, filled by Rewind for `--with`, and makes
/// each store path in it appear in the store of the view at `root`. The
/// slot is the second persistent memory device when the job has an input
/// image, the first otherwise. The boot's partition scan read the empty
/// slot through the block device's cache, so that cache is dropped first.
fn mount_extras(root: &str, job: &Job) -> Result<()> {
    use std::os::fd::AsRawFd;

    let device = if job.root == Root::Initramfs {
        EXTRAS_DEVICE_ALONE
    } else {
        EXTRAS_DEVICE_AFTER_IMAGE
    };
    let dev = File::open(device).map_err(|e| format!("opening {device}: {e}"))?;
    // SAFETY: an ioctl without arguments on an open block device.
    unsafe { libc::ioctl(dev.as_raw_fd(), BLKFLSBUF as libc::Ioctl, 0) };
    drop(dev);

    mkdir(EXTRAS_MOUNT)?;
    mount(device, EXTRAS_MOUNT, "erofs", libc::MS_RDONLY, "")?;
    let store = within(root, "/nix/store");
    for entry in fs::read_dir(EXTRAS_MOUNT).map_err(|e| format!("reading {EXTRAS_MOUNT}: {e}"))? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let target = store.join(&name);
        // A path already in the view's store is the same path.
        if target.exists() {
            continue;
        }
        let source = Path::new(EXTRAS_MOUNT).join(&name);
        let (source, target) = (source.display().to_string(), target.display().to_string());
        // A store path can be a single file; a bind mount needs a file to
        // cover it then.
        if Path::new(&source).is_dir() {
            mkdir(&target)?;
        } else {
            fs::write(&target, b"").map_err(|e| format!("creating {target}: {e}"))?;
        }
        mount(&source, &target, "", libc::MS_BIND, "")?;
    }
    Ok(())
}

/// Process `pid`'s environment as it is now, when the process is alive
/// and has one.
fn live_environment(pid: i32) -> Option<Vec<(String, String)>> {
    if pid <= 0 {
        return None;
    }
    let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|b| *b == 0)
            .filter_map(|entry| {
                let entry = String::from_utf8_lossy(entry);
                let (k, v) = entry.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect(),
    )
}

/// A pty pair of the given size: the master, and the slave opened by path.
fn open_pty(cols: u16, rows: u16) -> Result<(File, File)> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    // SAFETY: plain calls on a descriptor checked at each step.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 || libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            return Err(format!(
                "opening a pty: {}",
                std::io::Error::last_os_error()
            ));
        }
        let master = File::from_raw_fd(master);
        set_size(&master, cols, rows);
        let name = libc::ptsname(std::os::fd::AsRawFd::as_raw_fd(&master));
        if name.is_null() {
            return Err("ptsname failed".to_string());
        }
        let path = std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
            .map_err(|e| format!("opening {path}: {e}"))?;
        Ok((master, slave))
    }
}

/// Sets a pty's window size, which signals the shell to redraw.
fn set_size(master: &File, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a valid descriptor and winsize.
    unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(master),
            libc::TIOCSWINSZ as libc::Ioctl,
            &size,
        )
    };
}

/// Copies between the console and the pty until the shell's side closes:
/// console input to the shell, with resize messages applied to the pty,
/// and the shell's output to the console.
fn relay(console: &File, master: &File) {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let mut fds = [
        libc::pollfd {
            fd: console.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buf = vec![0u8; INSPECT_CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    let (mut console_w, mut master_w) = (console, master);
    loop {
        // SAFETY: a valid array of pollfds.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        // The shell's output; EOF or EIO means it has exited.
        if fds[1].revents != 0 {
            match (&*master).read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if console_w.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }

        // What Rewind sends, with resize messages taken out.
        if fds[0].revents & libc::POLLIN != 0 {
            let Ok(n) = (&*console).read(&mut buf) else {
                return;
            };
            pending.extend_from_slice(&buf[..n]);
            let typed = take_typed(&mut pending, |cols, rows| set_size(master, cols, rows));
            if master_w.write_all(&typed).is_err() {
                return;
            }
        }
    }
}

/// Splits console input into the bytes to type and resize messages, which
/// go to `resize`. A message cut off at the end stays in `pending`.
fn take_typed(pending: &mut Vec<u8>, mut resize: impl FnMut(u16, u16)) -> Vec<u8> {
    let mut typed = Vec::with_capacity(pending.len());
    let mut i = 0;
    while i < pending.len() {
        if pending[i] != RESIZE_ESCAPE {
            typed.push(pending[i]);
            i += 1;
            continue;
        }
        if pending.len() - i < RESIZE_LEN {
            break;
        }
        if pending[i + 1] == RESIZE_TAG {
            let cols = u16::from_le_bytes([pending[i + 2], pending[i + 3]]);
            let rows = u16::from_le_bytes([pending[i + 4], pending[i + 5]]);
            resize(cols, rows);
        }
        i += RESIZE_LEN;
    }
    pending.drain(..i);
    typed
}

/// Moves this process into the root and working directory of `pid`, or of
/// its nearest live ancestor, or of the job.
fn enter_view_of(pid: i32) -> Result<()> {
    let (root, cwd) = view_of(nearest_live(pid).unwrap_or(0))?;
    enter(&root, &cwd)
}

/// The root and working directory of process `pid`, or of the job when
/// `pid` is 0.
fn view_of(pid: i32) -> Result<(String, String)> {
    if pid > 0 {
        return Ok((format!("/proc/{pid}/root"), format!("/proc/{pid}/cwd")));
    }
    let root = if Path::new(IMAGE_ROOT).exists() {
        IMAGE_ROOT
    } else {
        "/"
    };
    let job: Job = serde_json::from_slice(
        &fs::read(JOB_PATH).map_err(|e| format!("reading {JOB_PATH}: {e}"))?,
    )
    .map_err(|e| format!("parsing {JOB_PATH}: {e}"))?;
    Ok((
        root.to_string(),
        within(root, &job.cwd).display().to_string(),
    ))
}

/// Chroots into `root` and enters `cwd`.
fn enter(root: &str, cwd: &str) -> Result<()> {
    // The working directory is opened before the chroot, which would hide
    // it, and entered after.
    let dir = File::open(cwd).map_err(|e| format!("opening {cwd}: {e}"))?;
    let root_c = CString::new(root).unwrap();
    // SAFETY: a valid path and a valid descriptor.
    unsafe {
        if libc::chroot(root_c.as_ptr()) != 0 {
            return Err(format!(
                "chroot {root}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::fchdir(std::os::fd::AsRawFd::as_raw_fd(&dir)) != 0 {
            return Err(format!(
                "entering {cwd}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// The program to exec, as a path inside the job's root: the job's program
/// if it names one, else argv[0], as given if it contains a slash, else
/// the first match on the job's PATH.
fn resolve(job: &Job, root: &str) -> Result<PathBuf> {
    if let Some(program) = &job.program {
        return Ok(PathBuf::from(program));
    }
    let name = job.argv.first().ok_or("the job has no program")?;
    if name.contains('/') {
        return Ok(PathBuf::from(name));
    }
    let path = job
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map_or("/usr/local/bin:/usr/bin:/bin", |(_, v)| v.as_str());
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(name);
        let executable = fs::metadata(within(root, &candidate.to_string_lossy()))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if executable {
            return Ok(candidate);
        }
    }
    Err(format!("{name}: not found on PATH {path}"))
}

fn wait_for(path: &str) -> Result<()> {
    let mut waited = std::time::Duration::ZERO;
    while !Path::new(path).exists() {
        if waited >= DEVICE_WAIT {
            return Err(format!(
                "{path} did not appear within {DEVICE_WAIT:?}\n{}",
                device_report()
            ));
        }
        std::thread::sleep(DEVICE_POLL);
        waited += DEVICE_POLL;
    }
    Ok(())
}

/// What the kernel knows about persistent memory, for the error when the
/// image device is missing.
fn device_report() -> String {
    let nd: Vec<String> = fs::read_dir("/sys/bus/nd/devices")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let iomem: Vec<String> = fs::read_to_string("/proc/iomem")
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    let platform = Path::new("/sys/bus/platform/devices/e820_pmem").exists();
    format!("nd devices: {nd:?}\ne820_pmem platform device: {platform}\niomem: {iomem:?}")
}

/// The links the Nix sandbox and most programs expect under /dev.
fn dev_links() -> Result<()> {
    for (link, target) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        symlink(target, link).map_err(|e| format!("linking {link}: {e}"))?;
    }
    Ok(())
}

/// Makes sysfs and /proc/cpuinfo report as many CPUs as the kernel's
/// affinity system calls do (rewind.cpus=), by bind-mounting files init
/// writes over them. Runs before the job's root is made, so an image root's
/// recursive binds of /proc and /sys carry these mounts along.
fn report_cpus() -> Result<()> {
    let cpus = reported_cpus()?;
    if cpus == 1 {
        return Ok(());
    }

    // CPU 0's block of /proc/cpuinfo, read before anything covers it.
    let one =
        fs::read_to_string(CPUINFO_FILE).map_err(|e| format!("reading {CPUINFO_FILE}: {e}"))?;

    // Each file's replacement, written beside the others and bound over it.
    mkdir(REPORTED_CPUS_DIR)?;
    let files = CPU_LIST_FILES
        .iter()
        .map(|target| (*target, cpu_list(cpus)))
        .chain(std::iter::once((CPUINFO_FILE, cpuinfo(&one, cpus))));
    for (target, contents) in files {
        let name = Path::new(target)
            .file_name()
            .expect("every reported file has a name")
            .to_string_lossy();
        let source = format!("{REPORTED_CPUS_DIR}/{name}");
        fs::write(&source, contents).map_err(|e| format!("writing {source}: {e}"))?;
        mount(&source, target, "", libc::MS_BIND, "")?;
    }
    Ok(())
}

/// How many CPUs the kernel's sched_getaffinity reports.
fn reported_cpus() -> Result<usize> {
    // SAFETY: an all-zero cpu_set_t is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the kernel writes at most the size it is given into `set`.
    let rc = unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) };
    if rc != 0 {
        return Err(format!(
            "sched_getaffinity: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `set` is a cpu_set_t the kernel filled.
    Ok(unsafe { libc::CPU_COUNT(&set) } as usize)
}

/// A sysfs CPU list naming CPUs 0 to `cpus` - 1.
fn cpu_list(cpus: usize) -> String {
    if cpus == 1 {
        return "0\n".into();
    }
    format!("0-{}\n", cpus - 1)
}

/// /proc/cpuinfo for `cpus` CPUs: CPU 0's block once per CPU, each with
/// its own processor number.
fn cpuinfo(one: &str, cpus: usize) -> String {
    (0..cpus)
        .map(|cpu| one.replacen(CPUINFO_FIRST, &format!("processor\t: {cpu}"), 1))
        .collect()
}

/// Points init's own output at the Rewind devices, so its errors reach the
/// trace. Best effort: nowhere else to report a failure here.
fn redirect_own_output() {
    for (device, fd) in [(STDOUT_DEVICE, 1), (STDERR_DEVICE, 2)] {
        if let Ok(f) = open_device(device) {
            // SAFETY: both descriptors are valid.
            unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&f), fd) };
        }
    }
}

fn open_device(path: &str) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("opening {path}: {e}"))
}

/// Writes a mark to the timeline. Best effort.
fn mark(text: &str) {
    if let Ok(mut f) = open_device(MARK_DEVICE) {
        let _ = writeln!(f, "{text}");
    }
}

fn set_hostname(name: &str) -> Result<()> {
    // SAFETY: a valid buffer and length.
    let rc = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if rc != 0 {
        return Err(format!("sethostname: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Brings up the loopback interface: builds and tests may listen on
/// localhost even with no network.
fn loopback_up() -> Result<()> {
    // SAFETY: plain socket and ioctl calls on a zeroed ifreq.
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return Err(format!("socket: {}", std::io::Error::last_os_error()));
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        let ok = libc::ioctl(sock, libc::SIOCGIFFLAGS as libc::Ioctl, &mut req) == 0 && {
            req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(sock, libc::SIOCSIFFLAGS as libc::Ioctl, &req) == 0
        };
        libc::close(sock);
        if !ok {
            return Err(format!(
                "bringing up lo: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<()> {
    let c = |s: &str| CString::new(s).unwrap();
    let (source_c, target_c, fstype_c, data_c) = (c(source), c(target), c(fstype), c(data));
    // SAFETY: valid NUL-terminated strings for the duration of the call.
    let rc = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            if fstype.is_empty() {
                std::ptr::null()
            } else {
                fstype_c.as_ptr()
            },
            flags,
            data_c.as_ptr().cast(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "mounting {source} on {target}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn mkdir(path: &str) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| format!("creating {path}: {e}"))
}

fn chmod(path: &str, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {path}: {e}"))
}

fn chown(path: &str, uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|e| format!("chown {path}: {e}"))
}

fn power_off() -> ! {
    // SAFETY: rebooting is what PID 1 is for; the kernel's power off is a
    // port write the monitor turns into the end of the run.
    unsafe {
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    // Console input for a shell: typed bytes pass through, resize messages
    // are taken out and applied, and a message split across reads waits
    // for the rest.
    use super::*;

    #[test]
    fn a_job_runs_its_program_whatever_argv0_says() {
        // A Nix builder is executed by its path with only its file name as
        // argv[0]; a job without a program runs argv[0], as given when it
        // has a slash.
        let mut job = Job {
            program: Some("/nix/store/aaa-bash/bin/bash".into()),
            argv: vec!["bash".into(), "-e".into()],
            env: vec![],
            cwd: "/build".into(),
            uid: 1000,
            gid: 100,
            hostname: "localhost".into(),
            root: Root::Store,
            files: vec![],
            outputs: vec![],
        };
        assert_eq!(
            resolve(&job, "/").unwrap(),
            PathBuf::from("/nix/store/aaa-bash/bin/bash")
        );
        job.program = None;
        job.argv[0] = "/bin/sh".into();
        assert_eq!(resolve(&job, "/").unwrap(), PathBuf::from("/bin/sh"));
    }

    #[test]
    fn a_cpu_list_names_every_cpu_reported() {
        // sysfs's online, possible and present files: a range from CPU 0.
        assert_eq!(cpu_list(1), "0\n");
        assert_eq!(cpu_list(4), "0-3\n");
    }

    #[test]
    fn cpuinfo_repeats_the_one_cpu_for_each_cpu_reported() {
        // Each copy of CPU 0's block carries its own processor number, so
        // `grep -c ^processor /proc/cpuinfo` counts the CPUs reported.
        let one = "processor\t: 0\nvendor_id\t: AuthenticAMD\n\n";
        assert_eq!(
            cpuinfo(one, 3),
            "processor\t: 0\nvendor_id\t: AuthenticAMD\n\n\
             processor\t: 1\nvendor_id\t: AuthenticAMD\n\n\
             processor\t: 2\nvendor_id\t: AuthenticAMD\n\n"
        );
    }

    #[test]
    fn a_map_range_is_named_without_padding_in_map_files() {
        assert_eq!(
            map_files_name("00400000-00401000").as_deref(),
            Some("400000-401000")
        );
        assert_eq!(
            map_files_name("7f0000000000-7f0000028000").as_deref(),
            Some("7f0000000000-7f0000028000")
        );
    }

    #[test]
    fn resize_messages_are_taken_out_of_typed_input() {
        let mut pending = b"ls".to_vec();
        pending.extend_from_slice(&rewind_init::resize_message(120, 40));
        pending.extend_from_slice(b"\r");
        let mut sizes = Vec::new();
        let typed = take_typed(&mut pending, |c, r| sizes.push((c, r)));
        assert_eq!(typed, b"ls\r");
        assert_eq!(sizes, [(120, 40)]);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_split_resize_message_waits_for_the_rest() {
        let message = rewind_init::resize_message(80, 24);
        let mut pending = b"a".to_vec();
        pending.extend_from_slice(&message[..3]);
        let mut sizes = Vec::new();
        assert_eq!(take_typed(&mut pending, |c, r| sizes.push((c, r))), b"a");
        assert_eq!(pending, message[..3]);

        pending.extend_from_slice(&message[3..]);
        assert!(take_typed(&mut pending, |c, r| sizes.push((c, r))).is_empty());
        assert_eq!(sizes, [(80, 24)]);
    }
}
