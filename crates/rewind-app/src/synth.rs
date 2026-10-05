//! Synthetic runs for development and tests.
//!
//! Real traces of large builds are slow to record and the engine cannot
//! record them yet, so this module writes one that looks like a nixpkgs
//! build of a C library: the guest kernel boots, bash runs the unpack,
//! patch, configure, build and check phases, make runs compilers four at a
//! time, and ctest runs the test binaries. In the failing variant the last
//! test's worker threads race at shutdown and one of them takes a SIGSEGV.
//! The passing variant is the same run up to that race, where the other
//! worker wins, the tests pass and the install and fixup phases follow.
//!
//! Records are laid out exactly as the guest writes them (see
//! rewind_trace's event.rs): a 20 byte header of length, kind, flags, pid,
//! tid and aux, then the payload.

use std::io;
use std::path::Path;

use rewind_trace::{Event, EventKind, HEADER_LEN, TraceWriter};

use crate::model::signo;
use crate::run::{MANIFEST_FILE, TRACE_FILE};

/// Record kinds and flags, as numbered by the guest.
mod record {
    pub const CONSOLE: u16 = 1;
    pub const OUTPUT: u16 = 2;
    pub const EXEC: u16 = 3;
    pub const FORK: u16 = 4;
    pub const EXIT: u16 = 5;
    pub const SIGNAL: u16 = 6;
    pub const OPEN: u16 = 7;
    pub const UNLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const MARK: u16 = 10;
    pub const FLAG_THREAD: u16 = 1;
}

/// Encodes an event as the guest's record: the inverse of `Event::decode`.
pub fn encode(event: &Event) -> Vec<u8> {
    let nul_joined = |parts: &[&str]| {
        let mut out = Vec::new();
        for part in parts {
            out.extend_from_slice(part.as_bytes());
            out.push(0);
        }
        out
    };
    let thread_flag = |thread: bool| if thread { record::FLAG_THREAD } else { 0 };

    let (kind, flags, aux, payload): (u16, u16, u32, Vec<u8>) = match &event.kind {
        EventKind::Console { text } => (record::CONSOLE, 0, 0, text.as_bytes().to_vec()),
        EventKind::Output { fd, bytes } => (record::OUTPUT, 0, *fd, bytes.clone()),
        EventKind::Exec {
            filename,
            argv,
            old_pid,
        } => {
            let mut parts = vec![filename.as_str()];
            parts.extend(argv.iter().map(String::as_str));
            (record::EXEC, 0, *old_pid, nul_joined(&parts))
        }
        EventKind::Fork { child, thread } => {
            (record::FORK, thread_flag(*thread), *child, Vec::new())
        }
        EventKind::Exit {
            status,
            comm,
            thread,
        } => (
            record::EXIT,
            thread_flag(*thread),
            *status,
            comm.as_bytes().to_vec(),
        ),
        EventKind::Signal { signo, code, addr } => {
            let mut payload = code.to_le_bytes().to_vec();
            payload.extend_from_slice(&addr.to_le_bytes());
            (record::SIGNAL, 0, *signo, payload)
        }
        EventKind::Open { path, flags } => (record::OPEN, 0, *flags, path.as_bytes().to_vec()),
        EventKind::Unlink { path } => (record::UNLINK, 0, 0, path.as_bytes().to_vec()),
        EventKind::Rename { from, to } => (record::RENAME, 0, 0, nul_joined(&[from, to])),
        EventKind::Mark { text } => (record::MARK, 0, 0, text.as_bytes().to_vec()),
        EventKind::Unknown { kind, data } => (*kind, 0, 0, data.clone()),
    };

    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&((HEADER_LEN + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&event.pid.to_le_bytes());
    out.extend_from_slice(&event.tid.to_le_bytes());
    out.extend_from_slice(&aux.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Which way the last test's shutdown race goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// worker-1 frees the queue first and worker-0 crashes.
    Failing,
    /// worker-0 drains the queue first and everything passes.
    Passing,
}

/// The size and shape of a synthetic run.
#[derive(Clone, Debug)]
pub struct SynthConfig {
    /// C files make compiles into the library.
    pub sources: usize,
    /// Test binaries ctest runs; the last is the one that races.
    pub tests: usize,
    /// Lines each passing test prints.
    pub test_lines: usize,
    /// Tasks each worker thread of the racing test reports.
    pub worker_tasks: usize,
    /// Seeds the random choices, which are the same in both variants.
    pub seed: u64,
    pub variant: Variant,
}

impl SynthConfig {
    /// About sixty thousand events: a large library build.
    pub fn large(variant: Variant) -> SynthConfig {
        SynthConfig {
            sources: 3000,
            tests: 60,
            test_lines: 24,
            worker_tasks: 800,
            seed: 3,
            variant,
        }
    }

    /// A few hundred events, for tests.
    pub fn small(variant: Variant) -> SynthConfig {
        SynthConfig {
            sources: 6,
            tests: 3,
            test_lines: 4,
            worker_tasks: 5,
            seed: 3,
            variant,
        }
    }
}

/// Writes a synthetic run directory: `trace.bin` and `manifest.json`.
pub fn write_run(dir: &Path, config: &SynthConfig, name: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(TRACE_FILE), generate(config))?;
    let stop = match config.variant {
        Variant::Failing => "failed",
        Variant::Passing => "exited",
    };
    let manifest = serde_json::json!({
        "name": name,
        "command": ["nix-build", "-A", "mylib"],
        "mode": "nix",
        "drv": DRV,
        "seed": config.seed.to_string(),
        "outcome": { "stop": stop },
    });
    std::fs::write(
        dir.join(MANIFEST_FILE),
        serde_json::to_string_pretty(&manifest).map_err(io::Error::other)?,
    )
}

/// Generates the trace file's bytes.
pub fn generate(config: &SynthConfig) -> Vec<u8> {
    let mut g = Gen::new(config.seed);
    build(&mut g, config);
    g.writer.finish().expect("writing to memory cannot fail")
}

/// The derivation the synthetic run builds.
pub const DRV: &str = "/nix/store/9x2kq8v1c7m3dzf0ha5slw4n6yrbp2jt-mylib-0.3.0.drv";
const SRC_TARBALL: &str = "/nix/store/3m0v7dcl9sqj2k1fxw8hyb6a4pn5zrgi-mylib-0.3.0.tar.gz";
const OUT: &str = "/nix/store/c8b1xk2m5q7rvw0n4fzd3hlj9sayp6gt-mylib-0.3.0";
const BUILDER: &str = "/nix/store/v6xdf1qz8l2mh0ksnb4ryc9j7w3agp5t-default-builder.sh";
const PATCH: &str = "/nix/store/kq3j8x1d5w0fz9hm2ncy7slbv6r4atgp-fix-queue-alignment.patch";
const ROOT: &str = "/build/mylib-0.3.0";
const BUILD_DIR: &str = "/build/mylib-0.3.0/build";

/// Open flags for a file written from scratch: O_WRONLY|O_CREAT|O_TRUNC.
const WRITE_FLAGS: u32 = 0o1101;
/// si_code for a SIGCHLD from a child that exited.
const CLD_EXITED: i32 = 1;
/// si_code for a SIGSEGV on an unmapped address.
const SEGV_MAPERR: i32 = 1;
/// The address worker-0 faults on: a field of the freed queue.
const FAULT_ADDR: u64 = 0x10;
/// The SIGCHLD signal number.
const SIGCHLD: u32 = 17;
/// The bit of the kernel's exit_code set when a core was dumped.
const CORE_DUMPED: u32 = 0x80;
/// How many compilers make runs at once.
const MAKE_JOBS: usize = 4;
/// One in this many compiles prints a warning.
const WARNING_EVERY: u64 = 23;

/// The kernel's exit_code for a normal exit with a status.
fn exited(code: u32) -> u32 {
    code << 8
}

/// Who an operation acts as: a fixed pid, or a process or thread a job
/// forked earlier, by the slot the fork filled.
#[derive(Clone, Copy, Debug)]
enum Who {
    Pid(u32),
    Slot(usize),
}

/// One step of a job. Jobs are lists of these so that `make -j4` can
/// interleave four of them.
#[derive(Clone, Debug)]
enum Op {
    Fork {
        parent: Who,
        slot: usize,
        thread: bool,
    },
    Exec(Who, Vec<String>),
    Out(Who, u32, String),
    Open(Who, String),
    Unlink(Who, String),
    Rename(Who, String, String),
    Signal(Who, u32, i32, u64),
    Exit(Who, u32, String, ThreadExit),
    /// The job computes for a number of steps drawn from a cost range.
    Busy(Cost),
}

/// A range of steps some work takes, low and high.
type Cost = (u64, u64);

/// What the synthetic run's work costs in steps, chosen so the phases take
/// shares of the run like a real library build: configure about a tenth,
/// build about half, check about a third.
mod cost {
    use super::Cost;
    pub const UNPACK_FILE: Cost = (50, 300);
    pub const PATCH: Cost = (100_000, 200_000);
    pub const CMAKE_CHECK: Cost = (40_000, 120_000);
    pub const CC1: Cost = (1_000, 8_000);
    pub const ASSEMBLE: Cost = (200, 1_500);
    pub const LINK: Cost = (5_000, 40_000);
    pub const TEST_CASE: Cost = (2_000, 10_000);
}

/// Whether an exit ends a whole process or one of its threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadExit {
    Process,
    Thread,
}

/// A job in progress: its operations and the (pid, tid) each slot holds.
struct Job {
    ops: Vec<Op>,
    next: usize,
    slots: Vec<(u32, u32)>,
}

impl Job {
    fn new(ops: Vec<Op>) -> Job {
        Job {
            ops,
            next: 0,
            slots: Vec::new(),
        }
    }
}

/// The generator: the trace being written, the step counter, the next
/// free pid and a small deterministic random number source.
struct Gen {
    writer: TraceWriter<Vec<u8>>,
    step: u64,
    next_pid: u32,
    rng: u64,
}

/// The first pid user space forks after init and the kernel threads.
const FIRST_USER_PID: u32 = 60;

/// Steps between two ordinary events.
const TICK_MIN: u64 = 1;
const TICK_MAX: u64 = 40;

impl Gen {
    fn new(seed: u64) -> Gen {
        const SEED_MIX: u64 = 0x9e37_79b9_7f4a_7c15;
        Gen {
            writer: TraceWriter::new(Vec::new()),
            step: 0,
            next_pid: FIRST_USER_PID,
            rng: seed.wrapping_mul(SEED_MIX) | 1,
        }
    }

    /// xorshift64: plenty for choosing delays and warnings.
    fn random(&mut self) -> u64 {
        const A: u32 = 13;
        const B: u32 = 7;
        const C: u32 = 17;
        self.rng ^= self.rng << A;
        self.rng ^= self.rng >> B;
        self.rng ^= self.rng << C;
        self.rng
    }

    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.random() % (hi - lo + 1)
    }

    fn emit(&mut self, pid: u32, tid: u32, kind: EventKind) {
        self.step += self.between(TICK_MIN, TICK_MAX);
        let event = Event {
            step: self.step,
            pid,
            tid,
            kind,
        };
        self.writer
            .record(self.step, &encode(&event))
            .expect("writing to memory cannot fail");
    }

    fn resolve(job: &Job, who: Who) -> (u32, u32) {
        match who {
            Who::Pid(pid) => (pid, pid),
            Who::Slot(slot) => job.slots[slot],
        }
    }

    /// Runs one operation of a job. Returns false once the job is done.
    fn advance(&mut self, job: &mut Job, busy_share: u64) -> bool {
        let Some(op) = job.ops.get(job.next).cloned() else {
            return false;
        };
        job.next += 1;
        match op {
            Op::Fork {
                parent,
                slot,
                thread,
            } => {
                let (pid, tid) = Self::resolve(job, parent);
                let child = self.next_pid;
                self.next_pid += 1;
                self.emit(pid, tid, EventKind::Fork { child, thread });
                let entry = if thread { (pid, child) } else { (child, child) };
                if job.slots.len() <= slot {
                    job.slots.resize(slot + 1, (0, 0));
                }
                job.slots[slot] = entry;
            }
            Op::Exec(who, argv) => {
                let (pid, tid) = Self::resolve(job, who);
                let filename = format!("/nix/store/bin/{}", argv[0]);
                self.emit(
                    pid,
                    tid,
                    EventKind::Exec {
                        filename,
                        argv,
                        old_pid: pid,
                    },
                );
            }
            Op::Out(who, fd, text) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(
                    pid,
                    tid,
                    EventKind::Output {
                        fd,
                        bytes: text.into_bytes(),
                    },
                );
            }
            Op::Open(who, path) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(
                    pid,
                    tid,
                    EventKind::Open {
                        path,
                        flags: WRITE_FLAGS,
                    },
                );
            }
            Op::Unlink(who, path) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(pid, tid, EventKind::Unlink { path });
            }
            Op::Rename(who, from, to) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(pid, tid, EventKind::Rename { from, to });
            }
            Op::Signal(who, signo, code, addr) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(pid, tid, EventKind::Signal { signo, code, addr });
            }
            Op::Exit(who, status, comm, how) => {
                let (pid, tid) = Self::resolve(job, who);
                self.emit(
                    pid,
                    tid,
                    EventKind::Exit {
                        status,
                        comm,
                        thread: how == ThreadExit::Thread,
                    },
                );
            }
            Op::Busy((lo, hi)) => {
                self.step += self.between(lo, hi) / busy_share;
            }
        }
        true
    }

    /// Runs one job to the end.
    fn run(&mut self, ops: Vec<Op>) {
        let mut job = Job::new(ops);
        while self.advance(&mut job, 1) {}
    }

    /// Runs jobs `width` at a time, interleaving their operations the way a
    /// parallel make interleaves its compilers on one CPU.
    fn run_parallel(&mut self, jobs: Vec<Vec<Op>>, width: usize) {
        let mut pending = jobs.into_iter();
        let mut active: Vec<Job> = Vec::new();
        loop {
            while active.len() < width {
                let Some(ops) = pending.next() else {
                    break;
                };
                active.push(Job::new(ops));
            }
            if active.is_empty() {
                return;
            }
            let pick = (self.random() % active.len() as u64) as usize;
            if !self.advance(&mut active[pick], width as u64) {
                active.swap_remove(pick);
            }
        }
    }

    fn console(&mut self, text: &str) {
        self.emit(
            0,
            0,
            EventKind::Console {
                text: format!("{text}\n"),
            },
        );
    }
}

/// Text lines as owned strings, for Op arguments.
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

/// Operations that run a child to completion: fork, exec, the body, exit
/// with `status`, and the SIGCHLD its parent gets.
fn child(parent: Who, slot: usize, cmd: &[&str], body: Vec<Op>, status: u32) -> Vec<Op> {
    let me = Who::Slot(slot);
    let comm = cmd[0].rsplit('/').next().unwrap_or(cmd[0]).to_string();
    let mut ops = vec![
        Op::Fork {
            parent,
            slot,
            thread: false,
        },
        Op::Exec(me, argv(cmd)),
    ];
    ops.extend(body);
    ops.push(Op::Exit(me, status, comm, ThreadExit::Process));
    ops.push(Op::Signal(parent, SIGCHLD, CLD_EXITED, 0));
    ops
}

/// Kernel boot messages, printed before init runs.
const BOOT_LINES: &[&str] = &[
    "Linux version 6.12.111 (rewind@rewind) (gcc (GCC) 14.2.0) #1 PREEMPT_DYNAMIC",
    "Command line: nolapic_timer lpj=1000000 panic=-1 rdinit=/init",
    "BIOS-provided physical RAM map:",
    "Hypervisor detected: Rewind",
    "printk: legacy console [rewind0] enabled",
    "Memory: 2012140K/2096696K available",
    "clocksource: rewind: mask: 0xffffffffffffffff",
    "Calibrating delay loop (skipped) preset value.. 200.00 BogoMIPS",
    "pid_max: default: 32768 minimum: 301",
    "devtmpfs: initialized",
    "NET: Registered PF_UNIX/PF_LOCAL protocol family",
    "clocksource: Switched to clocksource rewind",
    "Unpacking initramfs...",
    "Freeing initrd memory: 792K",
    "virtio_blk virtio0: [vda] 16777216 512-byte logical blocks",
    "EXT4-fs (vda): mounted filesystem with ordered data mode",
    "Freeing unused kernel image (initmem) memory: 1736K",
    "Run /init as init process",
];

/// Directories and file names the synthetic library's sources are drawn
/// from.
const MODULES: &[&str] = &[
    "core", "io", "net", "codec", "sync", "mem", "text", "crypto",
];
const STEMS: &[&str] = &[
    "pool", "queue", "alloc", "arena", "hash", "map", "list", "ring", "str", "buf", "file", "path",
    "sock", "timer", "sched", "lock", "atomic", "log", "fmt", "parse", "json", "utf8", "crc",
    "sha", "base64", "bits", "vec", "heap", "slab", "epoll",
];
const SUFFIXES: &[&str] = &[
    "", "_util", "_impl", "_ops", "_api", "_init", "_iter", "_cache", "_stats", "_debug",
];

/// The path of source file `i` under src/.
fn source_name(i: usize) -> String {
    let stem = STEMS[i % STEMS.len()];
    let suffix = SUFFIXES[(i / STEMS.len()) % SUFFIXES.len()];
    let module = MODULES[(i / (STEMS.len() * SUFFIXES.len())) % MODULES.len()];
    let round = i / (STEMS.len() * SUFFIXES.len() * MODULES.len());
    if round == 0 {
        return format!("{module}/{stem}{suffix}.c");
    }
    format!("{module}/{stem}{suffix}_{round}.c")
}

/// The name of test `i`; the last test is the one that races.
fn test_name(i: usize, count: usize) -> String {
    if i + 1 == count {
        return "test_pool_shutdown".to_string();
    }
    format!("test_{}", STEMS[i % STEMS.len()]).replace("test_pool", "test_pool_basic")
        + &if i >= STEMS.len() {
            format!("_{}", i / STEMS.len())
        } else {
            String::new()
        }
}

/// The feature checks CMake runs at configure time.
const CMAKE_CHECKS: &[&str] = &[
    "pthread_create in pthreads",
    "sys/epoll.h",
    "stdatomic.h",
    "C11 threads",
    "clock_gettime",
    "posix_memalign",
    "aligned_alloc",
    "__builtin_expect",
    "__builtin_ctzll",
    "strlcpy",
    "memfd_create",
    "getrandom",
    "sched_getcpu",
    "pthread_setname_np",
    "futex",
    "linux/io_uring.h",
];

/// The whole run: boot, then bash running the phases.
fn build(g: &mut Gen, config: &SynthConfig) {
    // The kernel boots and its threads start.
    for line in BOOT_LINES {
        g.console(&format!("[    0.000000] {line}"));
    }
    const INIT: u32 = 1;
    const KTHREADD: u32 = 2;
    const FIRST_KTHREAD: u32 = 30;
    const KTHREADS: u32 = 6;
    for k in 0..KTHREADS {
        g.emit(
            KTHREADD,
            KTHREADD,
            EventKind::Fork {
                child: FIRST_KTHREAD + k,
                thread: false,
            },
        );
    }

    // init starts, points stdout and stderr at the host, and forks the
    // builder.
    let init = Who::Pid(INIT);
    g.run(vec![
        Op::Exec(init, argv(&["/init"])),
        Op::Open(init, "/dev/rewind-stdout".into()),
        Op::Open(init, "/dev/rewind-stderr".into()),
        Op::Out(init, 1, format!("building '{DRV}'...\n")),
    ]);

    // bash is a job of its own that the phases below add to, so that its
    // slot stays filled across them.
    const BASH: usize = 0;
    let mut bash = Job::new(vec![
        Op::Fork {
            parent: init,
            slot: BASH,
            thread: false,
        },
        Op::Exec(Who::Slot(BASH), argv(&["bash", "-e", BUILDER])),
    ]);
    while g.advance(&mut bash, 1) {}
    let bash_pid = bash.slots[BASH].0;
    let sh = Who::Pid(bash_pid);

    unpack_phase(g, sh, config);
    patch_phase(g, sh);
    configure_phase(g, sh);
    build_phase(g, sh, config);
    let passed = check_phase(g, sh, config);

    if passed {
        install_phase(g, sh, config);
        fixup_phase(g, sh);
        g.run(vec![
            Op::Exit(sh, 0, "bash".into(), ThreadExit::Process),
            Op::Signal(init, SIGCHLD, CLD_EXITED, 0),
        ]);
    } else {
        g.run(vec![
            Op::Exit(sh, exited(2), "bash".into(), ThreadExit::Process),
            Op::Signal(init, SIGCHLD, CLD_EXITED, 0),
            Op::Out(
                init,
                2,
                format!("error: builder for '{DRV}' failed with exit code 2\n"),
            ),
        ]);
    }
    g.console("reboot: Power down");
}

fn phase(sh: Who, name: &str) -> Op {
    Op::Out(sh, 1, format!("Running phase: {name}\n"))
}

fn unpack_phase(g: &mut Gen, sh: Who, config: &SynthConfig) {
    const TAR: usize = 1;
    let mut body = Vec::new();
    for i in 0..config.sources {
        body.push(Op::Busy(cost::UNPACK_FILE));
        body.push(Op::Open(
            Who::Slot(TAR),
            format!("{ROOT}/src/{}", source_name(i)),
        ));
    }
    for i in 0..config.tests {
        body.push(Op::Open(
            Who::Slot(TAR),
            format!("{ROOT}/tests/{}.c", test_name(i, config.tests)),
        ));
    }
    body.push(Op::Open(Who::Slot(TAR), format!("{ROOT}/CMakeLists.txt")));

    let mut ops = vec![
        phase(sh, "unpackPhase"),
        Op::Out(sh, 1, format!("unpacking source archive {SRC_TARBALL}\n")),
    ];
    ops.extend(child(sh, TAR, &["tar", "xf", SRC_TARBALL], body, 0));
    ops.push(Op::Out(sh, 1, "source root is mylib-0.3.0\n".into()));
    g.run(ops);
}

fn patch_phase(g: &mut Gen, sh: Who) {
    const PATCH_SLOT: usize = 1;
    let me = Who::Slot(PATCH_SLOT);
    let target = format!("{ROOT}/src/core/queue.c");
    let temp = format!("{ROOT}/src/core/queue.cXXXXXX");
    let body = vec![
        Op::Out(me, 1, "patching file src/core/queue.c\n".into()),
        Op::Busy(cost::PATCH),
        Op::Open(me, temp.clone()),
        Op::Rename(me, temp, target),
    ];
    let mut ops = vec![
        phase(sh, "patchPhase"),
        Op::Out(sh, 1, format!("applying patch {PATCH}\n")),
    ];
    ops.extend(child(sh, PATCH_SLOT, &["patch", "-p1"], body, 0));
    ops.push(Op::Out(
        sh,
        1,
        "patching script interpreter paths in ./scripts\n".into(),
    ));
    g.run(ops);
}

fn configure_phase(g: &mut Gen, sh: Who) {
    const CMAKE: usize = 1;
    const CC: usize = 2;
    let cmake = Who::Slot(CMAKE);
    let mut body = vec![
        Op::Out(
            cmake,
            1,
            "-- The C compiler identification is GNU 14.2.0\n".into(),
        ),
        Op::Out(cmake, 1, "-- Detecting C compiler ABI info - done\n".into()),
    ];

    // Each check compiles a scratch program and cleans up after it.
    for (i, check) in CMAKE_CHECKS.iter().enumerate() {
        let scratch = format!("{BUILD_DIR}/CMakeFiles/CMakeScratch/TryCompile-{i:04x}");
        body.push(Op::Out(cmake, 1, format!("-- Looking for {check}\n")));
        let cc_body = vec![
            Op::Busy(cost::CMAKE_CHECK),
            Op::Open(Who::Slot(CC), format!("{scratch}/cmTC_{i:04x}")),
        ];
        body.extend(child(
            cmake,
            CC,
            &["gcc", "-o", "cmTC", "CheckSymbolExists.c"],
            cc_body,
            0,
        ));
        body.push(Op::Unlink(cmake, format!("{scratch}/cmTC_{i:04x}")));
        body.push(Op::Out(
            cmake,
            1,
            format!("-- Looking for {check} - found\n"),
        ));
    }
    body.extend([
        Op::Out(cmake, 1, "-- Configuring done (1.4s)\n".into()),
        Op::Open(cmake, format!("{BUILD_DIR}/CMakeCache.txt")),
        Op::Open(cmake, format!("{BUILD_DIR}/Makefile")),
        Op::Out(cmake, 1, "-- Generating done (0.1s)\n".into()),
        Op::Out(
            cmake,
            1,
            format!("-- Build files have been written to: {BUILD_DIR}\n"),
        ),
    ]);

    let mut ops = vec![
        phase(sh, "configurePhase"),
        Op::Out(sh, 1, "fixing cmake files...\n".into()),
        Op::Out(
            sh,
            1,
            format!("cmake flags: -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX={OUT}\n"),
        ),
    ];
    ops.extend(child(
        sh,
        CMAKE,
        &["cmake", "..", "-DCMAKE_BUILD_TYPE=Release"],
        body,
        0,
    ));
    g.run(ops);
}

/// Operations for one compile under make: gcc runs cc1 and then as, and
/// removes the assembly in between.
fn compile_job(g: &mut Gen, make: Who, source: &str, object: &str, progress: String) -> Vec<Op> {
    const GCC: usize = 0;
    const CC1: usize = 1;
    const AS: usize = 2;
    let gcc = Who::Slot(GCC);
    let asm = format!("/build/cc{:06x}.s", g.random() & 0xff_ffff);

    // cc1 sometimes warns on standard error.
    let mut cc1_body = vec![Op::Busy(cost::CC1), Op::Open(Who::Slot(CC1), asm.clone())];
    if g.random().is_multiple_of(WARNING_EVERY) {
        cc1_body.push(Op::Out(
            Who::Slot(CC1),
            2,
            format!("{source}:118:9: warning: unused variable 'tmp' [-Wunused-variable]\n"),
        ));
    }

    let mut gcc_body = child(
        gcc,
        CC1,
        &["cc1", "-quiet", source, "-O2", "-fPIC"],
        cc1_body,
        0,
    );
    gcc_body.extend(child(
        gcc,
        AS,
        &["as", "--64", "-o", object, &asm],
        vec![
            Op::Busy(cost::ASSEMBLE),
            Op::Open(Who::Slot(AS), object.to_string()),
        ],
        0,
    ));
    gcc_body.push(Op::Unlink(gcc, asm));

    let mut ops = vec![Op::Out(make, 1, progress)];
    ops.extend(child(
        make,
        GCC,
        &["gcc", "-O2", "-fPIC", "-c", source, "-o", object],
        gcc_body,
        0,
    ));
    ops
}

/// Operations for a link under make.
fn link_job(make: Who, output: &str, progress: String) -> Vec<Op> {
    const GCC: usize = 0;
    const LD: usize = 1;
    let body = child(
        Who::Slot(GCC),
        LD,
        &["ld", "-o", output],
        vec![
            Op::Busy(cost::LINK),
            Op::Open(Who::Slot(LD), output.to_string()),
        ],
        0,
    );
    let mut ops = vec![Op::Out(make, 1, progress)];
    ops.extend(child(make, GCC, &["gcc", "-o", output], body, 0));
    ops
}

fn build_phase(g: &mut Gen, sh: Who, config: &SynthConfig) {
    const MAKE: usize = 1;
    const PERCENT: usize = 100;
    let total = config.sources + 2 * config.tests + 1;
    let percent = |done: usize| done * PERCENT / total.max(1);

    // make starts; its pid is needed by the compile jobs below.
    let mut start = Job::new(vec![
        phase(sh, "buildPhase"),
        Op::Out(sh, 1, "build flags: -j4 -l4 SHELL=bash\n".into()),
        Op::Fork {
            parent: sh,
            slot: MAKE,
            thread: false,
        },
        Op::Exec(Who::Slot(MAKE), argv(&["make", "-j4", "-l4"])),
    ]);
    while g.advance(&mut start, 1) {}
    let make_pid = start.slots[MAKE].0;
    let make = Who::Pid(make_pid);

    // The library's objects, four compilers at a time.
    let mut jobs = Vec::new();
    for i in 0..config.sources {
        let name = source_name(i);
        let source = format!("{ROOT}/src/{name}");
        let object = format!("{BUILD_DIR}/src/CMakeFiles/mylib.dir/{name}.o");
        let progress = format!(
            "[{:3}%] Building C object src/CMakeFiles/mylib.dir/{name}.o\n",
            percent(i)
        );
        jobs.push(compile_job(g, make, &source, &object, progress));
    }
    g.run_parallel(jobs, MAKE_JOBS);

    // The shared library.
    let lib = format!("{BUILD_DIR}/libmylib.so.0.3.0");
    let done = config.sources;
    g.run(link_job(
        make,
        &lib,
        format!(
            "[{:3}%] Linking C shared library libmylib.so\n",
            percent(done)
        ),
    ));
    g.run(vec![
        Op::Rename(
            make,
            format!("{lib}.tmp"),
            format!("{BUILD_DIR}/libmylib.so"),
        ),
        Op::Out(
            make,
            1,
            format!("[{:3}%] Built target mylib\n", percent(done)),
        ),
    ]);

    // The test binaries: a compile and a link each.
    let mut jobs = Vec::new();
    for i in 0..config.tests {
        let name = test_name(i, config.tests);
        let source = format!("{ROOT}/tests/{name}.c");
        let object = format!("{BUILD_DIR}/tests/CMakeFiles/{name}.dir/{name}.c.o");
        let at = done + 1 + 2 * i;
        let mut job = compile_job(
            g,
            make,
            &source,
            &object,
            format!(
                "[{:3}%] Building C object tests/CMakeFiles/{name}.dir/{name}.c.o\n",
                percent(at)
            ),
        );
        job.extend(link_job(
            make,
            &format!("{BUILD_DIR}/tests/{name}"),
            format!("[{:3}%] Linking C executable {name}\n", percent(at + 1)),
        ));
        job.push(Op::Out(
            make,
            1,
            format!("[{:3}%] Built target {name}\n", percent(at + 1)),
        ));
        jobs.push(job);
    }
    g.run_parallel(jobs, MAKE_JOBS);

    g.run(vec![
        Op::Exit(make, 0, "make".into(), ThreadExit::Process),
        Op::Signal(sh, SIGCHLD, CLD_EXITED, 0),
    ]);
}

/// The check phase. Returns whether the tests passed.
fn check_phase(g: &mut Gen, sh: Who, config: &SynthConfig) -> bool {
    const MAKE: usize = 1;
    const CTEST: usize = 2;
    const TEST: usize = 3;
    const WORKER_0: usize = 4;
    const WORKER_1: usize = 5;

    let mut job = Job::new(vec![
        phase(sh, "checkPhase"),
        Op::Out(sh, 1, "check flags: -j4 SHELL=bash test\n".into()),
        Op::Fork {
            parent: sh,
            slot: MAKE,
            thread: false,
        },
        Op::Exec(Who::Slot(MAKE), argv(&["make", "test"])),
        Op::Out(Who::Slot(MAKE), 1, "Running tests...\n".into()),
        Op::Fork {
            parent: Who::Slot(MAKE),
            slot: CTEST,
            thread: false,
        },
        Op::Exec(
            Who::Slot(CTEST),
            argv(&["ctest", "--force-new-ctest-process"]),
        ),
        Op::Out(Who::Slot(CTEST), 1, format!("Test project {BUILD_DIR}\n")),
        Op::Open(
            Who::Slot(CTEST),
            format!("{BUILD_DIR}/Testing/Temporary/LastTest.log"),
        ),
    ]);
    while g.advance(&mut job, 1) {}
    let make = Who::Pid(job.slots[MAKE].0);
    let ctest = Who::Pid(job.slots[CTEST].0);
    let count = config.tests;
    let width = count.to_string().len();

    // Every test but the last passes.
    for i in 0..count.saturating_sub(1) {
        let name = test_name(i, count);
        let me = Who::Slot(TEST);
        let mut body = Vec::new();
        for case in 0..config.test_lines / 2 {
            body.push(Op::Out(me, 1, format!("[ RUN      ] {name}.case_{case}\n")));
            body.push(Op::Busy(cost::TEST_CASE));
            body.push(Op::Out(
                me,
                1,
                format!("[       OK ] {name}.case_{case} (0 ms)\n"),
            ));
        }
        let mut ops = vec![Op::Out(
            ctest,
            1,
            format!("      Start {:>width$}: {name}\n", i + 1),
        )];
        ops.extend(child(
            ctest,
            TEST,
            &[&format!("{BUILD_DIR}/tests/{name}")],
            body,
            0,
        ));
        ops.push(Op::Out(
            ctest,
            1,
            format!(
                "{:>width$}/{count} Test #{:<width$}: {name:.<32} Passed    0.{:02} sec\n",
                i + 1,
                i + 1,
                g.between(1, 99)
            ),
        ));
        g.run(ops);
    }

    // The last test starts two workers that process tasks, then shuts the
    // pool down, where the two workers race.
    let name = test_name(count - 1, count);
    let test = Who::Slot(TEST);
    let w0 = Who::Slot(WORKER_0);
    let w1 = Who::Slot(WORKER_1);
    let mut ops = vec![
        Op::Out(ctest, 1, format!("      Start {count:>width$}: {name}\n")),
        Op::Fork {
            parent: ctest,
            slot: TEST,
            thread: false,
        },
        Op::Exec(test, argv(&[&format!("{BUILD_DIR}/tests/{name}")])),
        Op::Out(test, 1, "pool: starting 2 workers\n".into()),
        Op::Fork {
            parent: test,
            slot: WORKER_0,
            thread: true,
        },
        Op::Fork {
            parent: test,
            slot: WORKER_1,
            thread: true,
        },
    ];
    let mut job = Job::new(std::mem::take(&mut ops));
    while g.advance(&mut job, 1) {}

    // The workers' tasks interleave the way the scheduler ran them.
    let mut w0_tasks = Vec::new();
    let mut w1_tasks = Vec::new();
    for t in 0..config.worker_tasks {
        w0_tasks.push(Op::Out(w0, 1, format!("worker-0: task {t} done\n")));
        w1_tasks.push(Op::Out(w1, 1, format!("worker-1: task {t} done\n")));
    }
    let mut workers = vec![Job::new(w0_tasks), Job::new(w1_tasks)];
    for w in &mut workers {
        w.slots = job.slots.clone();
    }
    loop {
        if workers.is_empty() {
            break;
        }
        let pick = (g.random() % workers.len() as u64) as usize;
        if !g.advance(&mut workers[pick], 1) {
            workers.swap_remove(pick);
        }
    }
    job.ops
        .push(Op::Out(test, 1, "pool: shutdown requested\n".into()));
    while g.advance(&mut job, 1) {}

    // The race. The runs are identical up to here and differ from here on.
    let (pid, _) = job.slots[TEST];
    let passed = match config.variant {
        Variant::Failing => {
            let crashed = signo::SIGSEGV | CORE_DUMPED;
            job.ops.extend([
                Op::Out(w1, 1, "worker-1: woke first, freeing queue\n".into()),
                Op::Signal(w0, signo::SIGSEGV, SEGV_MAPERR, FAULT_ADDR),
                Op::Exit(w1, crashed, "worker-1".into(), ThreadExit::Thread),
                Op::Exit(w0, crashed, "worker-0".into(), ThreadExit::Thread),
                Op::Open(test, format!("{BUILD_DIR}/tests/core.{pid}")),
                Op::Exit(test, crashed, name.clone(), ThreadExit::Process),
                Op::Signal(ctest, SIGCHLD, CLD_EXITED, 0),
                Op::Out(
                    ctest,
                    1,
                    format!(
                        "{count}/{count} Test #{count}: {name:.<32}***Exception: SegFault  0.02 sec\n"
                    ),
                ),
                Op::Out(
                    ctest,
                    1,
                    format!(
                        "\n{}% tests passed, 1 tests failed out of {count}\n",
                        (count - 1) * 100 / count
                    ),
                ),
                Op::Out(ctest, 1, "The following tests FAILED:\n".into()),
                Op::Out(ctest, 1, format!("\t  {count} - {name} (SEGFAULT)\n")),
                Op::Out(ctest, 2, "Errors while running CTest\n".into()),
                Op::Exit(ctest, exited(8), "ctest".into(), ThreadExit::Process),
                Op::Signal(make, SIGCHLD, CLD_EXITED, 0),
                Op::Out(make, 2, "make: *** [Makefile:91: test] Error 8\n".into()),
                Op::Exit(make, exited(2), "make".into(), ThreadExit::Process),
                Op::Signal(sh, SIGCHLD, CLD_EXITED, 0),
            ]);
            false
        }
        Variant::Passing => {
            job.ops.extend([
                Op::Out(w0, 1, "worker-0: woke first, draining queue\n".into()),
                Op::Out(w1, 1, "worker-1: queue drained, exiting\n".into()),
                Op::Exit(w1, 0, "worker-1".into(), ThreadExit::Thread),
                Op::Exit(w0, 0, "worker-0".into(), ThreadExit::Thread),
                Op::Out(test, 1, "[  PASSED  ] test_pool_shutdown\n".into()),
                Op::Exit(test, 0, name.clone(), ThreadExit::Process),
                Op::Signal(ctest, SIGCHLD, CLD_EXITED, 0),
                Op::Out(
                    ctest,
                    1,
                    format!("{count}/{count} Test #{count}: {name:.<32} Passed    0.31 sec\n"),
                ),
                Op::Out(
                    ctest,
                    1,
                    format!("\n100% tests passed, 0 tests failed out of {count}\n"),
                ),
                Op::Exit(ctest, 0, "ctest".into(), ThreadExit::Process),
                Op::Signal(make, SIGCHLD, CLD_EXITED, 0),
                Op::Exit(make, 0, "make".into(), ThreadExit::Process),
                Op::Signal(sh, SIGCHLD, CLD_EXITED, 0),
            ]);
            true
        }
    };
    while g.advance(&mut job, 1) {}
    passed
}

fn install_phase(g: &mut Gen, sh: Who, config: &SynthConfig) {
    const MAKE: usize = 1;
    let me = Who::Slot(MAKE);
    let mut body = Vec::new();
    for dest in [
        "lib/libmylib.so.0.3.0",
        "lib/libmylib.so",
        "lib/pkgconfig/mylib.pc",
    ] {
        body.push(Op::Out(me, 1, format!("-- Installing: {OUT}/{dest}\n")));
        body.push(Op::Open(me, format!("{OUT}/{dest}")));
    }
    for i in 0..config.sources.min(MODULES.len() * STEMS.len()) / SUFFIXES.len() {
        let header = source_name(i).replace(".c", ".h");
        body.push(Op::Out(
            me,
            1,
            format!("-- Installing: {OUT}/include/mylib/{header}\n"),
        ));
        body.push(Op::Open(me, format!("{OUT}/include/mylib/{header}")));
    }
    let mut ops = vec![phase(sh, "installPhase")];
    ops.extend(child(sh, MAKE, &["make", "install"], body, 0));
    g.run(ops);
}

fn fixup_phase(g: &mut Gen, sh: Who) {
    const TOOL: usize = 1;
    let mut ops = vec![
        phase(sh, "fixupPhase"),
        Op::Out(
            sh,
            1,
            format!("shrinking RPATHs of ELF executables and libraries in {OUT}\n"),
        ),
    ];
    ops.extend(child(
        sh,
        TOOL,
        &[
            "patchelf",
            "--shrink-rpath",
            &format!("{OUT}/lib/libmylib.so.0.3.0"),
        ],
        vec![Op::Open(
            Who::Slot(TOOL),
            format!("{OUT}/lib/libmylib.so.0.3.0"),
        )],
        0,
    ));
    ops.push(Op::Out(
        sh,
        1,
        format!("stripping (with command strip and flags -S -p) in  {OUT}/lib\n"),
    ));
    ops.extend(child(
        sh,
        TOOL,
        &["strip", "-S", "-p", &format!("{OUT}/lib/libmylib.so.0.3.0")],
        vec![Op::Open(
            Who::Slot(TOOL),
            format!("{OUT}/lib/libmylib.so.0.3.0"),
        )],
        0,
    ));
    g.run(ops);
}

#[cfg(test)]
mod tests {
    // The generator against the real decoder: traces are written with
    // `generate`, read back with rewind_trace, and indexed with the model.
    use super::*;
    use crate::model::{FailureKind, LogFilter, Timeline};
    use rewind_trace::Trace;

    fn read(config: &SynthConfig) -> Trace {
        let path = std::env::temp_dir().join(format!(
            "rewind-app-synth-{}-{:?}-{}.bin",
            std::process::id(),
            config.variant,
            config.sources
        ));
        std::fs::write(&path, generate(config)).unwrap();
        let trace = Trace::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        trace
    }

    #[test]
    fn every_event_kind_round_trips_through_the_decoder() {
        // Each kind encoded and decoded again comes back equal.
        let kinds = vec![
            EventKind::Console {
                text: "boot\n".into(),
            },
            EventKind::Output {
                fd: 2,
                bytes: b"x\n".to_vec(),
            },
            EventKind::Exec {
                filename: "/bin/cc".into(),
                argv: vec!["cc".into(), "-c".into()],
                old_pid: 9,
            },
            EventKind::Fork {
                child: 10,
                thread: true,
            },
            EventKind::Exit {
                status: 0x8b,
                comm: "t".into(),
                thread: false,
            },
            EventKind::Signal {
                signo: 11,
                code: 1,
                addr: 0x10,
            },
            EventKind::Open {
                path: "/a".into(),
                flags: 0o1101,
            },
            EventKind::Unlink { path: "/b".into() },
            EventKind::Rename {
                from: "/c".into(),
                to: "/d".into(),
            },
            EventKind::Mark { text: "m".into() },
        ];
        for kind in kinds {
            let event = Event {
                step: 5,
                pid: 9,
                tid: 10,
                kind,
            };
            assert_eq!(Event::decode(5, &encode(&event)).unwrap(), event);
        }
    }

    #[test]
    fn the_large_failing_run_looks_like_a_nixpkgs_build() {
        // At least fifty thousand events, the five phases in order, and a
        // SIGSEGV in a worker thread as the failure.
        let trace = read(&SynthConfig::large(Variant::Failing));
        assert!(
            trace.events.len() >= 50_000,
            "{} events",
            trace.events.len()
        );
        let t = Timeline::new(trace, None, None);
        let names: Vec<&str> = t.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["start", "unpack", "patch", "configure", "build", "check"]
        );
        let failure = t.failure.unwrap();
        assert!(matches!(
            failure.kind,
            FailureKind::Signal {
                signo: signo::SIGSEGV,
                ..
            }
        ));
        assert_ne!(failure.pid, failure.tid);
        assert!(t.line_count_at(LogFilter::Output, t.total) > 5_000);
    }

    #[test]
    fn the_two_variants_diverge_at_the_race() {
        // The failing and passing runs agree up to the shutdown race and
        // differ at the first line one of the workers prints about it.
        let fail = read(&SynthConfig::small(Variant::Failing));
        let pass = read(&SynthConfig::small(Variant::Passing));
        let d = fail.divergence(&pass).unwrap();
        let left = &fail.events[d.index];
        let EventKind::Output { bytes, .. } = &left.kind else {
            panic!("diverged at {left:?}");
        };
        assert!(String::from_utf8_lossy(bytes).contains("worker-1: woke first"));
        assert_eq!(d.left_step, d.right_step);
        let pass = Timeline::new(pass, None, None);
        assert_eq!(pass.failure, None);
        assert_eq!(pass.phases.last().unwrap().name, "fixup");
    }
}
