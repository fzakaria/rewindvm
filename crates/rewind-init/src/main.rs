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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rewind_init::{
    IMAGE_ROOT, INSPECT_ARG, InspectRequest, InspectStatus, JOB_PATH, Job, Listed, ListedAll,
    ListedKind, Listing, MAX_LISTED, MAX_TREE_NAME, Mark, Output, Probe, RESIZE_ESCAPE, RESIZE_LEN,
    RESIZE_TAG, RUNNING_ENV, Root, SECTION_MAPS, SECTION_PID, SELFTEST_ARG, TreeEntry, nar_hash,
    section_header,
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
/// monitor tagged with the pid that made it. The plain devices are not a
/// terminal; the guest kernel's terminal lines are, for a job that asks
/// for one.
const STDOUT_DEVICE: &str = "/dev/rewind-stdout";
const STDERR_DEVICE: &str = "/dev/rewind-stderr";
const STDOUT_TERMINAL: &str = "/dev/rewind-tty0";
const STDERR_TERMINAL: &str = "/dev/rewind-tty1";
const MARK_DEVICE: &str = "/dev/rewind";

type Result<T> = std::result::Result<T, String>;

/// Threads and atomic additions per thread in the self-test.
const SELFTEST_THREADS: usize = 4;
const SELFTEST_ADDS: u64 = 5_000_000;

/// Where an image job's image is mounted read-only, under the overlay at
/// IMAGE_ROOT that init chroots the job into, taking /dev and /proc along.
const IMAGE_LOWER: &str = "/lower";

/// A root's hosts file, and the lines an image root gets when its own is
/// missing or empty, the same the initramfs has for Nix builds. `docker
/// export` leaves /etc/hosts empty, since Docker bind-mounts its own over
/// it, and without these lines `localhost` does not resolve.
const HOSTS_FILE: &str = "etc/hosts";
const LOCALHOST_HOSTS: &str = "127.0.0.1 localhost\n::1 localhost\n";

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
        mark(&Mark::Exit { status: 127 << 8 });
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

    mark(&Mark::Start);
    let status = spawn_and_reap(&job, root)?;
    if status == 0 {
        for output in &job.outputs {
            match nar_hash(&within(root, output)) {
                Ok(hash) => mark(&Mark::Output {
                    path: output.clone(),
                    hash,
                }),
                Err(e) => eprintln!("rewind-init: hashing {output}: {e}"),
            }
        }
    }
    mark(&Mark::Exit { status });
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
    mkdir(IMAGE_LOWER)?;
    mount(IMAGE_DEVICE, IMAGE_LOWER, "erofs", libc::MS_RDONLY, "")?;
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
        &format!("lowerdir={IMAGE_LOWER},upperdir=/rw/upper,workdir=/rw/work"),
    )?;
    default_hosts(Path::new(IMAGE_LOWER), Path::new(IMAGE_ROOT))?;
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

/// Writes LOCALHOST_HOSTS to `root`'s hosts file when `image`, the image
/// `root` is an overlay of, has none or an empty one. The write lands in
/// the overlay's writable layer; the image is never changed. A hosts file
/// that is a symbolic link is the image's choice, and stays.
fn default_hosts(image: &Path, root: &Path) -> Result<()> {
    let ours = match fs::symlink_metadata(image.join(HOSTS_FILE)) {
        Ok(meta) => meta.is_file() && meta.len() == 0,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    };
    if !ours {
        return Ok(());
    }

    let hosts = root.join(HOSTS_FILE);
    if let Some(etc) = hosts.parent() {
        mkdir(&etc.to_string_lossy())?;
    }
    fs::write(&hosts, LOCALHOST_HOSTS).map_err(|e| format!("writing {}: {e}", hosts.display()))
}

/// Runs the job and reaps every process until its main process exits,
/// then stops the rest. Returns the main process's wait status.
fn spawn_and_reap(job: &Job, root: &str) -> Result<i32> {
    let program = resolve(job, root)?;
    let (stdout, stderr) = match job.output {
        Output::Plain => (open_device(STDOUT_DEVICE)?, open_device(STDERR_DEVICE)?),
        Output::Terminal => (
            open_terminal(STDOUT_TERMINAL, job)?,
            open_terminal(STDERR_TERMINAL, job)?,
        ),
    };

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
/// run shows. Then each probe, whose count the host knows in advance.
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
                        *handoff
                            .lock()
                            .expect("no thread panics holding the handoff") += 1;
                    }
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    println!("selftest {}", total.load(Ordering::SeqCst));
    probes();
}

/// Bytes in a page, which the exits probe touches one of an iteration.
const PAGE_SIZE: usize = 4096;

/// Runs each probe's loop for its iterations and for twice as many, between
/// two of its marks each time. Without /dev/rewind there are no marks, and
/// the host finds every probe's count missing.
fn probes() {
    use std::os::fd::AsRawFd;

    let Ok(device) = open_device(MARK_DEVICE) else {
        return;
    };
    let fd = device.as_raw_fd();

    // Pages no iteration of the exits probe has touched, three times its
    // iterations for its two runs.
    let len = 3 * Probe::Exits.iterations() as usize * PAGE_SIZE;
    // SAFETY: a new private mapping, which nothing else uses.
    let pages = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if pages == libc::MAP_FAILED {
        return;
    }
    let mut fresh = pages.cast::<u8>();

    for probe in Probe::ALL {
        let mark = Mark::Probe(probe).to_string();
        for iterations in [probe.iterations(), 2 * probe.iterations()] {
            measure(fd, mark.as_bytes(), probe, iterations, fresh);
            if probe == Probe::Exits {
                // SAFETY: within the mapping, which holds both runs' pages.
                fresh = unsafe { fresh.add(iterations as usize * PAGE_SIZE) };
            }
        }
    }
}

/// One run of a probe's loop between two of its marks. Not inlined, so
/// both runs of a probe take the same branches around the loop.
#[inline(never)]
fn measure(fd: i32, mark: &[u8], probe: Probe, iterations: u64, pages: *mut u8) {
    // SAFETY: a valid buffer and its length.
    unsafe { libc::write(fd, mark.as_ptr().cast(), mark.len()) };
    probe_loop(probe, iterations, fd, pages);
    // SAFETY: as above.
    unsafe { libc::write(fd, mark.as_ptr().cast(), mark.len()) };
}

/// Runs `probe`'s loop `iterations` times, at least once. Each loop counts
/// down a register and ends in `dec; jnz`, its backward branch. The
/// conditional branches an iteration retires must match
/// [`Probe::branches_per_iteration`].
fn probe_loop(probe: Probe, iterations: u64, fd: i32, pages: *mut u8) {
    use std::arch::asm;

    match probe {
        // Not taken, taken forward, and the backward branch: 3.
        // SAFETY: registers only.
        Probe::Branches => unsafe {
            asm!(
                "2:",
                "xor {t:e}, {t:e}",
                "jnz 3f",
                "3:",
                "jz 4f",
                "ud2",
                "4:",
                "dec {n}",
                "jnz 2b",
                n = inout(reg) iterations => _,
                t = out(reg) _,
                options(nostack, nomem),
            )
        },

        // A branch the locked add's result decides, never taken, since
        // the word only grows, and the backward branch: 2.
        Probe::Locked => {
            let mut word = 0u64;
            // SAFETY: `word` is a local the loop alone writes.
            unsafe {
                asm!(
                    "2:",
                    "lock inc qword ptr [{p}]",
                    "jz 3f",
                    "3:",
                    "dec {n}",
                    "jnz 2b",
                    n = inout(reg) iterations => _,
                    p = in(reg) &raw mut word,
                    options(nostack),
                )
            }
        }

        // SSE, cmov, setcc, movzx, imul and bt all start with 0x0F, and
        // only the backward branch is one: 1.
        // SAFETY: registers only.
        Probe::NotBranches => unsafe {
            asm!(
                "2:",
                "pxor xmm0, xmm0",
                "paddq xmm0, xmm1",
                "cmp {n}, 1",
                "cmovz {t}, {n}",
                "setz {t:l}",
                "movzx {t:e}, {t:l}",
                "imul {t}, {n}",
                "bt {t}, 0",
                "dec {n}",
                "jnz 2b",
                n = inout(reg) iterations => _,
                t = out(reg) _,
                out("xmm0") _,
                out("xmm1") _,
                options(nostack, nomem),
            )
        },

        // An empty write to /dev/rewind, which exits to the monitor, where
        // the timer's interrupts come from: the two runs take 2 and 4.
        // Then a branch on its result, never taken; a first touch of a
        // page, which faults; and cpuid, an exit KVM handles. Then the
        // backward branch: 2.
        // SAFETY: `pages` has a fresh page for each iteration, and cpuid's
        // rbx, which asm! cannot name, is saved around it.
        Probe::Exits => unsafe {
            asm!(
                "2:",
                "mov eax, {write}",
                "mov edi, {fd:e}",
                "xor esi, esi",
                "xor edx, edx",
                "syscall",
                "test rax, rax",
                "jnz 3f",
                "3:",
                "mov byte ptr [{page}], 1",
                "add {page}, {page_size}",
                "mov {save}, rbx",
                "xor eax, eax",
                "cpuid",
                "mov rbx, {save}",
                "dec {n}",
                "jnz 2b",
                n = inout(reg) iterations => _,
                page = inout(reg) pages => _,
                fd = in(reg) fd,
                save = out(reg) _,
                write = const libc::SYS_write,
                page_size = const PAGE_SIZE,
                out("rax") _,
                out("rcx") _,
                out("rdx") _,
                out("rdi") _,
                out("rsi") _,
                out("r11") _,
                options(nostack),
            )
        },
    }
}

/// An inspection, started by the kernel in a forked run at Rewind's
/// request, outside the job and with init's root. The kernel has stopped
/// every other process, so what it reads is the machine at the step asked
/// about.
fn inspect(request: &[String]) {
    // The devices are opened now, before entering the job's view, which
    // may not have them.
    let answer = |status: InspectStatus| mark(&Mark::InspectEnd(status));
    mark(&Mark::InspectBegin);

    // Errors go to standard error, where Rewind shows them.
    let (Ok(mut out), Ok(mut err)) = (open_device(STDOUT_DEVICE), open_device(STDERR_DEVICE))
    else {
        answer(InspectStatus::Failed);
        return;
    };

    let Some(request) = InspectRequest::parse(request) else {
        let _ = writeln!(err, "inspect: unknown request {request:?}");
        answer(InspectStatus::Failed);
        return;
    };
    // A pid as the process calls take it; 0 for the job's view.
    let pid_t = |pid: Option<u32>| pid.and_then(|p| i32::try_from(p).ok());
    let view = |pid: Option<u32>| pid_t(pid).unwrap_or(0);
    let status = match request {
        InspectRequest::Cat { pid, path } => cat(view(pid), &path, &mut out, &mut err),
        InspectRequest::Running { pid } => running(pid_t(pid), &mut out, &mut err),
        InspectRequest::Files { pid } => files(view(pid), &mut out, &mut err),
        InspectRequest::List { pid, path } => list(view(pid), &path, &mut out, &mut err),
        InspectRequest::Tree { pid, path } => tree(view(pid), &path, &mut out, &mut err),
        InspectRequest::Shell { pid, size, with } => match shell(view(pid), size, with) {
            Ok(()) => InspectStatus::Done,
            Err(e) => {
                let _ = writeln!(err, "inspect: {e}");
                InspectStatus::Failed
            }
        },
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

/// Process `pid`, or when None the process that was running at the step,
/// which the kernel names in REWIND_RUNNING, in sections: its pid, its
/// memory map, and the ELF files it had mapped that the host does not
/// have. The map is read with init's root, so its paths are whole paths in
/// the VM, a job's root included.
fn running(pid: Option<i32>, out: &mut File, err: &mut File) -> InspectStatus {
    let asked = pid.is_some();
    let pid = pid
        .or_else(|| std::env::var(RUNNING_ENV).ok()?.parse().ok())
        .unwrap_or(0);
    if pid <= 0 {
        let _ = writeln!(err, "no process was running at this step, only the kernel");
        return InspectStatus::NotFound;
    }

    let maps = match fs::read_to_string(format!("/proc/{pid}/maps")) {
        Ok(maps) => maps,
        Err(e) if asked && e.kind() == std::io::ErrorKind::NotFound => {
            let _ = writeln!(err, "no process {pid} at this step");
            return InspectStatus::NotFound;
        }
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

/// The entries below directory `path` on `out`, as process `pid` sees
/// the directory.
fn list(pid: i32, path: &str, out: &mut File, err: &mut File) -> InspectStatus {
    // See the directory as the process did: its root and working
    // directory when it is still alive, else the job's.
    if let Err(e) = enter_view_of(pid) {
        let _ = writeln!(err, "inspect: {e}");
        return InspectStatus::Failed;
    }
    let listing = match list_below(Path::new(path), MAX_LISTED, &enters) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = writeln!(err, "{path}: no such directory at this step");
            return InspectStatus::NotFound;
        }
        Err(e) => {
            let _ = writeln!(err, "{path}: {e}");
            return InspectStatus::Failed;
        }
    };

    // The lines, in chunks; the device splits them into records.
    for chunk in listing.lines().as_bytes().chunks(INSPECT_CHUNK) {
        if out.write_all(chunk).is_err() {
            return InspectStatus::Failed;
        }
    }
    InspectStatus::Done
}

/// Whether a walk, a listing's or a tree's, reads a directory it comes
/// to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enter {
    Yes,
    No,
}

/// Whether a walk reads directory `dir`. Not when the kernel makes the
/// directory's entries up, as it does under /proc and /sys: they are no
/// files of the run, and there are thousands of them, enough to fill a
/// listing of the root. A listing or a tree asked of such a directory
/// itself still reads the directory.
fn enters(dir: &Path) -> Enter {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return Enter::No;
    };
    // SAFETY: statfs writes only inside the struct, which zeroes are a
    // valid value of, and reads a NUL-terminated path.
    let filesystem = unsafe {
        let mut found: libc::statfs = std::mem::zeroed();
        if libc::statfs(path.as_ptr(), &mut found) != 0 {
            return Enter::Yes;
        }
        // The field and the constants are signed in one C library and
        // unsigned in another; every value of either fits an i128.
        i128::from(found.f_type)
    };
    let made_up = [
        i128::from(libc::PROC_SUPER_MAGIC),
        i128::from(libc::SYSFS_MAGIC),
    ];
    if made_up.contains(&filesystem) {
        return Enter::No;
    }
    Enter::Yes
}

/// The entries below directory `root`, at most `max` of them: the
/// directory's own entries by name, then those of each directory among
/// them that `enters` says to read, and so on down, so a listing that is
/// cut keeps the entries nearest `root`. Links are listed, not followed.
/// An error when `root` cannot be read as a directory; a directory below
/// it that cannot be read is listed and not entered.
fn list_below(
    root: &Path,
    max: usize,
    enters: &dyn Fn(&Path) -> Enter,
) -> std::io::Result<Listing> {
    use std::collections::VecDeque;

    let mut listing = Listing {
        entries: Vec::new(),
        complete: ListedAll::Yes,
    };

    // The directories still to read, as paths below `root`.
    let mut unread: VecDeque<PathBuf> = VecDeque::from([PathBuf::new()]);
    while let Some(dir) = unread.pop_front() {
        let entries = match fs::read_dir(root.join(&dir)) {
            Ok(entries) => entries,
            Err(e) if dir.as_os_str().is_empty() => return Err(e),
            Err(_) => continue,
        };
        let mut names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
        names.sort();

        for name in names {
            let below = dir.join(&name);
            let Ok(meta) = fs::symlink_metadata(root.join(&below)) else {
                continue;
            };
            if listing.entries.len() == max {
                listing.complete = ListedAll::No;
                return Ok(listing);
            }

            // What the entry is, and for a link where it points.
            let kind = meta.file_type();
            let (kind, target) = if kind.is_symlink() {
                let target = fs::read_link(root.join(&below)).ok();
                (ListedKind::Symlink, target.map(|t| t.display().to_string()))
            } else if kind.is_dir() {
                if enters(&root.join(&below)) == Enter::Yes {
                    unread.push_back(below.clone());
                }
                (ListedKind::Directory, None)
            } else if kind.is_file() {
                (ListedKind::File, None)
            } else {
                (ListedKind::Other, None)
            };
            let size = match kind {
                ListedKind::File => meta.len(),
                _ => 0,
            };
            listing.entries.push(Listed {
                path: below.display().to_string(),
                kind,
                mode: meta.permissions().mode() & PERMISSION_BITS,
                size,
                target,
            });
        }
    }
    Ok(listing)
}

/// The file, link or directory `path` names and everything below it on
/// `out`, as process `pid` sees the path.
fn tree(pid: i32, path: &str, out: &mut File, err: &mut File) -> InspectStatus {
    // See the path as the process did: its root and working directory
    // when it is still alive, else the job's.
    if let Err(e) = enter_view_of(pid) {
        let _ = writeln!(err, "inspect: {e}");
        return InspectStatus::Failed;
    }

    // Nothing is sent of a path nothing has, so the answer is empty.
    if let Err(e) = fs::symlink_metadata(path) {
        if e.kind() == std::io::ErrorKind::NotFound {
            let _ = writeln!(err, "{path}: no such file or directory at this step");
            return InspectStatus::NotFound;
        }
        let _ = writeln!(err, "{path}: {e}");
        return InspectStatus::Failed;
    }

    // The device splits what is written to it into records.
    match send_tree(Path::new(path), out, &enters) {
        Ok(()) => InspectStatus::Done,
        Err(e) => {
            let _ = writeln!(err, "{path}: {e}");
            InspectStatus::Failed
        }
    }
}

/// Writes `root` and everything below it to `out` as a tree: a
/// [`TreeEntry`] for each, a directory's entries after its own by name,
/// and a file's bytes after the file's entry. Links are sent, not
/// followed. A directory below `root` that `enters` says not to read is
/// sent without what is below it, as a listing lists one, so a tree of
/// the root stays out of /proc and /sys; `root` itself is always read. An
/// error when anything of the tree cannot be read: a tree is whole or it
/// is not one.
fn send_tree(
    root: &Path,
    out: &mut impl Write,
    enters: &dyn Fn(&Path) -> Enter,
) -> std::io::Result<()> {
    // The paths below `root` still to send, the next one last. A
    // directory's entries go on in reverse order of name, so they come
    // off by name, and all of one entry's before the next name.
    let mut unsent: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(below) = unsent.pop() {
        // Joining an empty path would add a slash, which a file refuses.
        let at = if below.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&below)
        };
        let meta = fs::symlink_metadata(&at)?;

        // What the entry is, and for a link where it points.
        let kind = meta.file_type();
        let (kind, target) = if kind.is_symlink() {
            let target = fs::read_link(&at)?;
            (ListedKind::Symlink, target.as_os_str().as_bytes().to_vec())
        } else if kind.is_dir() {
            (ListedKind::Directory, Vec::new())
        } else if kind.is_file() {
            (ListedKind::File, Vec::new())
        } else {
            (ListedKind::Other, Vec::new())
        };
        let size = match kind {
            ListedKind::File => meta.len(),
            _ => 0,
        };
        let entry = TreeEntry {
            path: below.as_os_str().as_bytes().to_vec(),
            kind,
            mode: meta.permissions().mode() & PERMISSION_BITS,
            uid: meta.uid(),
            gid: meta.gid(),
            mtime: meta.mtime(),
            size,
            target,
        };
        if entry.path.len() > MAX_TREE_NAME || entry.target.len() > MAX_TREE_NAME {
            return Err(std::io::Error::other(format!(
                "{} has a longer name than a tree holds",
                at.display()
            )));
        }
        out.write_all(&entry.header())?;

        let is_root = below.as_os_str().is_empty();
        match kind {
            ListedKind::File => send_contents(&at, size, out)?,
            ListedKind::Directory if is_root || enters(&at) == Enter::Yes => {
                let mut names: Vec<_> = fs::read_dir(&at)?
                    .map(|entry| entry.map(|e| e.file_name()))
                    .collect::<std::io::Result<_>>()?;
                names.sort();
                unsent.extend(names.iter().rev().map(|name| below.join(name)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Writes the `size` bytes of the file at `path` to `out`, in chunks. An
/// error when the file holds fewer: its entry has promised that many.
fn send_contents(path: &Path, size: u64, out: &mut impl Write) -> std::io::Result<()> {
    use std::io::Read;

    let mut file = File::open(path)?.take(size);
    let mut buf = vec![0u8; INSPECT_CHUNK];
    let mut sent = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        sent += n as u64;
    }
    if sent < size {
        return Err(std::io::Error::other(format!(
            "{} is shorter than its {size} bytes",
            path.display()
        )));
    }
    Ok(())
}

/// The bits of a file's mode that are its permissions, set-id and sticky
/// bits included, apart from the bits that say what kind of file it is.
const PERMISSION_BITS: u32 = 0o7777;

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
    let root_c = CString::new(root).map_err(|e| format!("chroot {root}: {e}"))?;
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
    let rc = unsafe { libc::sched_getaffinity(0, size_of_val(&set), &mut set) };
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

/// A terminal line for the job's output, as nix-daemon opens a builder's
/// pseudoterminal: not init's controlling terminal, and owned by the job's
/// user, so a program can open it again by name, as /dev/stdout.
fn open_terminal(path: &str, job: &Job) -> Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .map_err(|e| format!("opening {path}: {e}"))?;
    // SAFETY: changes the owner of the open file's device node.
    let rc = unsafe { libc::fchown(file.as_raw_fd(), job.uid, job.gid) };
    if rc != 0 {
        return Err(format!(
            "giving {path} to the job: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

/// Writes a mark to the timeline. Best effort.
/// Writes `mark` to /dev/rewind in one write, which the device records as
/// one mark.
fn mark(mark: &Mark) {
    if let Ok(mut f) = open_device(MARK_DEVICE) {
        let _ = f.write_all(mark.to_string().as_bytes());
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
    let c = |s: &str| CString::new(s).map_err(|e| format!("mounting {source} on {target}: {e}"));
    let (source_c, target_c, fstype_c, data_c) = (c(source)?, c(target)?, c(fstype)?, c(data)?);
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
    use rewind_init::{TreeReader, TreeSink};

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
            output: Output::Terminal,
        };
        assert_eq!(
            resolve(&job, "/").unwrap(),
            PathBuf::from("/nix/store/aaa-bash/bin/bash")
        );
        job.program = None;
        job.argv[0] = "/bin/sh".into();
        assert_eq!(resolve(&job, "/").unwrap(), PathBuf::from("/bin/sh"));
    }

    /// A listing has every entry below a directory, nearest first and by
    /// name within a directory, and is cut at its limit. Lays out a tree
    /// with a file, an executable, a link, an empty directory and a file
    /// two directories down in a temporary directory, lists it whole and
    /// then with room for three entries.
    #[test]
    fn a_listing_walks_a_directory_nearest_entries_first() {
        let root = std::env::temp_dir().join(format!("rewind-init-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("share/man")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/shuffle"), b"\x7fELF").unwrap();
        fs::set_permissions(root.join("bin/shuffle"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("share/man/shuffle.1"), b"shuffle").unwrap();
        fs::set_permissions(
            root.join("share/man/shuffle.1"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        symlink("shuffle", root.join("bin/sh")).unwrap();

        let everywhere = |_: &Path| Enter::Yes;
        let listing = list_below(&root, MAX_LISTED, &everywhere).unwrap();
        assert_eq!(listing.complete, ListedAll::Yes);
        let seen: Vec<(&str, ListedKind, u64, Option<&str>)> = listing
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.kind, e.size, e.target.as_deref()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("bin", ListedKind::Directory, 0, None),
                ("empty", ListedKind::Directory, 0, None),
                ("share", ListedKind::Directory, 0, None),
                ("bin/sh", ListedKind::Symlink, 0, Some("shuffle")),
                ("bin/shuffle", ListedKind::File, 4, None),
                ("share/man", ListedKind::Directory, 0, None),
                ("share/man/shuffle.1", ListedKind::File, 7, None),
            ]
        );
        let mode = |path: &str| {
            listing
                .entries
                .iter()
                .find(|e| e.path == path)
                .map(|e| e.mode)
        };
        assert_eq!(mode("bin/shuffle"), Some(0o755));
        assert_eq!(mode("share/man/shuffle.1"), Some(0o644));

        // Room for three entries: the three nearest, and the cut noted.
        let cut = list_below(&root, 3, &everywhere).unwrap();
        assert_eq!(cut.complete, ListedAll::No);
        let paths: Vec<&str> = cut.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["bin", "empty", "share"]);

        // A directory the walk is told not to enter is listed, and
        // nothing below it is. Named as the walk's own directory, the
        // same directory is read.
        let not_share = |dir: &Path| {
            if dir.ends_with("share") {
                Enter::No
            } else {
                Enter::Yes
            }
        };
        let kept_out = list_below(&root, MAX_LISTED, &not_share).unwrap();
        let paths: Vec<&str> = kept_out.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["bin", "empty", "share", "bin/sh", "bin/shuffle"]
        );
        let named = list_below(&root.join("share"), MAX_LISTED, &not_share).unwrap();
        let paths: Vec<&str> = named.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["man", "man/shuffle.1"]);

        // A path that is no directory is an error, as the caller reports.
        assert!(list_below(&root.join("bin/shuffle"), MAX_LISTED, &everywhere).is_err());
        assert_eq!(
            list_below(&root.join("nope"), MAX_LISTED, &everywhere)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A tree is sent whole, each directory's entries by name and a
    /// directory's own before the next name's, with each file's bytes
    /// after the file's entry. Lays out a tree with a file, an executable,
    /// a link, an empty directory and a file two directories down in a
    /// temporary directory, sends the tree into memory and reads it back
    /// with the tree's reader. Then sends a file and a link on their own,
    /// the tree with one directory not to be read, and a path nothing
    /// has.
    #[test]
    fn a_tree_is_sent_depth_first_by_name() {
        /// What the reader hands on: each entry, with a file's bytes.
        #[derive(Default)]
        struct Kept(Vec<(TreeEntry, Vec<u8>)>);

        impl TreeSink for Kept {
            fn entry(&mut self, entry: &TreeEntry) -> std::io::Result<()> {
                self.0.push((entry.clone(), Vec::new()));
                Ok(())
            }

            fn contents(&mut self, bytes: &[u8]) -> std::io::Result<()> {
                if let Some((_, contents)) = self.0.last_mut() {
                    contents.extend_from_slice(bytes);
                }
                Ok(())
            }

            fn end_of_file(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// An entry as the test compares one: its path, its kind, its
        /// file's bytes and its link's target.
        type Seen<'a> = (&'a [u8], ListedKind, &'a [u8], &'a [u8]);

        /// The tree at `root` as its reader hands it on, with every
        /// directory read but those whose path ends with `skipped`.
        fn sent_without(
            root: &Path,
            skipped: Option<&str>,
        ) -> std::io::Result<Vec<(TreeEntry, Vec<u8>)>> {
            let enters = |dir: &Path| match skipped {
                Some(name) if dir.ends_with(name) => Enter::No,
                _ => Enter::Yes,
            };
            let mut tree = Vec::new();
            send_tree(root, &mut tree, &enters)?;
            let mut kept = Kept::default();
            let mut reader = TreeReader::default();
            reader.feed(&tree, &mut kept)?;
            reader.finish()?;
            Ok(kept.0)
        }

        /// The whole tree at `root`.
        fn sent(root: &Path) -> std::io::Result<Vec<(TreeEntry, Vec<u8>)>> {
            sent_without(root, None)
        }

        let root = std::env::temp_dir().join(format!("rewind-init-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("share/man")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/shuffle"), b"\x7fELF\n").unwrap();
        fs::set_permissions(root.join("bin/shuffle"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("share/man/shuffle.1"), b"shuffle").unwrap();
        fs::set_permissions(
            root.join("share/man/shuffle.1"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        fs::write(root.join("share/notes"), b"").unwrap();
        symlink("shuffle", root.join("bin/sh")).unwrap();

        let tree = sent(&root).unwrap();
        let seen: Vec<Seen> = tree
            .iter()
            .map(|(e, contents)| (&e.path[..], e.kind, &contents[..], &e.target[..]))
            .collect();
        assert_eq!(
            seen,
            vec![
                (&b""[..], ListedKind::Directory, &b""[..], &b""[..]),
                (b"bin", ListedKind::Directory, b"", b""),
                (b"bin/sh", ListedKind::Symlink, b"", b"shuffle"),
                (b"bin/shuffle", ListedKind::File, b"\x7fELF\n", b""),
                (b"empty", ListedKind::Directory, b"", b""),
                (b"share", ListedKind::Directory, b"", b""),
                (b"share/man", ListedKind::Directory, b"", b""),
                (b"share/man/shuffle.1", ListedKind::File, b"shuffle", b""),
                (b"share/notes", ListedKind::File, b"", b""),
            ]
        );

        // Each entry carries its permissions, its size, and the owner and
        // time the filesystem holds for it.
        let entry = |path: &[u8]| {
            tree.iter()
                .map(|(e, _)| e)
                .find(|e| e.path == path)
                .unwrap()
        };
        assert_eq!(entry(b"bin/shuffle").mode, 0o755);
        assert_eq!(entry(b"bin/shuffle").size, 5);
        assert_eq!(entry(b"share/man/shuffle.1").mode, 0o640);
        let meta = fs::metadata(root.join("bin/shuffle")).unwrap();
        assert_eq!(entry(b"bin/shuffle").uid, meta.uid());
        assert_eq!(entry(b"bin/shuffle").gid, meta.gid());
        assert_eq!(entry(b"bin/shuffle").mtime, meta.mtime());

        // A file on its own is a tree of one entry, and so is a link,
        // which is sent, not followed.
        let file = sent(&root.join("bin/shuffle")).unwrap();
        assert_eq!(file.len(), 1);
        assert_eq!(file[0].0.kind, ListedKind::File);
        assert_eq!(file[0].0.path, b"");
        assert_eq!(file[0].1, b"\x7fELF\n");
        let link = sent(&root.join("bin/sh")).unwrap();
        assert_eq!(link.len(), 1);
        assert_eq!(link[0].0.kind, ListedKind::Symlink);
        assert_eq!(link[0].0.target, b"shuffle");

        // A directory the walk is told not to read is sent, and nothing
        // below it is. Named as the tree's own root, the same directory
        // is read.
        let kept_out = sent_without(&root, Some("share")).unwrap();
        let paths: Vec<&[u8]> = kept_out.iter().map(|(e, _)| &e.path[..]).collect();
        let want: Vec<&[u8]> = vec![b"", b"bin", b"bin/sh", b"bin/shuffle", b"empty", b"share"];
        assert_eq!(paths, want);
        let named = sent_without(&root.join("share"), Some("share")).unwrap();
        let paths: Vec<&[u8]> = named.iter().map(|(e, _)| &e.path[..]).collect();
        let want: Vec<&[u8]> = vec![b"", b"man", b"man/shuffle.1", b"notes"];
        assert_eq!(paths, want);

        // A path nothing has is an error, as the caller reports.
        assert_eq!(
            sent(&root.join("nope")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// An image root gets the localhost lines a Nix build's sandbox has
    /// when its own /etc/hosts is missing or empty, as `docker export`
    /// leaves it, and keeps one that says anything. Lays out an image and
    /// the root it is seen through in temporary directories for each case,
    /// then reads the root's /etc/hosts.
    #[test]
    fn an_image_without_hosts_gets_localhost() {
        let base = std::env::temp_dir().join(format!("rewind-init-hosts-{}", std::process::id()));
        let case = |name: &str, hosts: Option<&str>| -> Option<String> {
            let image = base.join(name).join("image");
            let root = base.join(name).join("root");
            fs::create_dir_all(image.join("etc")).unwrap();
            fs::create_dir_all(&root).unwrap();
            if let Some(hosts) = hosts {
                fs::write(image.join(HOSTS_FILE), hosts).unwrap();
            }
            default_hosts(&image, &root).unwrap();
            fs::read_to_string(root.join(HOSTS_FILE)).ok()
        };

        assert_eq!(case("missing", None).as_deref(), Some(LOCALHOST_HOSTS));
        assert_eq!(case("empty", Some("")).as_deref(), Some(LOCALHOST_HOSTS));
        assert_eq!(case("own", Some("10.0.0.1 db\n")), None);
        fs::remove_dir_all(&base).unwrap();
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
