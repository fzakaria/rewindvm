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

use rewind_init::{EXIT_MARK, JOB_PATH, Job, OUTPUT_MARK, Root, tree_hash};

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

fn main() {
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
    match job.root {
        Root::Store => store_root(&job)?,
        Root::Image => image_root()?,
        Root::Initramfs => {}
    }

    for f in &job.files {
        fs::write(&f.path, &f.contents).map_err(|e| format!("writing {}: {e}", f.path))?;
        chown(&f.path, job.uid, job.gid)?;
    }

    let status = spawn_and_reap(&job)?;
    if status == 0 {
        for output in &job.outputs {
            match tree_hash(Path::new(output)) {
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

/// Image mode: the image is a root filesystem; overlay it writable and
/// chroot into it, taking /dev, /proc and /sys along.
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
        let target = format!("/newroot{dir}");
        mkdir(&target)?;
        mount(dir, &target, "", libc::MS_MOVE, "")?;
    }
    for dir in ["/newroot/tmp", "/newroot/build"] {
        mkdir(dir)?;
        chmod(dir, 0o1777)?;
    }

    let root = CString::new("/newroot").unwrap();
    // SAFETY: a valid path; the process is single threaded.
    if unsafe { libc::chroot(root.as_ptr()) } != 0 {
        return Err(format!("chroot: {}", std::io::Error::last_os_error()));
    }
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    Ok(())
}

/// Runs the job and reaps every process until its main process exits,
/// then stops the rest. Returns the main process's wait status.
fn spawn_and_reap(job: &Job) -> Result<i32> {
    let program = resolve(job)?;
    let stdout = open_device(STDOUT_DEVICE)?;
    let stderr = open_device(STDERR_DEVICE)?;

    let mut cmd = Command::new(&program);
    cmd.arg0(&job.argv[0])
        .args(&job.argv[1..])
        .env_clear()
        .envs(job.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&job.cwd)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .uid(job.uid)
        .gid(job.gid);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
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

/// The program to exec: as given if it contains a slash, else the first
/// match on the job's PATH.
fn resolve(job: &Job) -> Result<PathBuf> {
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
        let executable = fs::metadata(&candidate)
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
