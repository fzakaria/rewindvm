//! gdb at a step: a fork of a run, stopped at the step, served over the
//! GDB remote protocol. gdb sees one x86-64 CPU with the VM's memory as the
//! VM's page tables map it: the kernel, which the VM kernel's vmlinux has
//! the symbols for, and the user space of whichever process was running.
//! The CPU is gdb's first thread, with the same id at every stop and
//! named for the task on it, so steps and traps always stop the thread gdb
//! ran. Every thread of the process gdb debugs is a thread too, with its
//! process's memory and the user registers it saved when it last entered
//! the kernel, or the vCPU's while it runs in user space, so gdb shows
//! where each one waits; they move only when the CPU runs them.
//! Breakpoints and watchpoints at user addresses stop the fork only in the
//! process gdb is debugging; other processes map the same addresses to
//! memory of their own. Continuing and stepping run the fork, never the
//! recording. The fork's
//! records are compared with the run's as it goes, and gdb's user is told
//! the step where the two first differ: from there on the fork is not the
//! run, whether gdb changed its memory or the debugging itself moved it.

use std::net::TcpStream;

use anyhow::{Context, Result};
use gdbstub::common::Signal;
use gdbstub::common::Tid;
use gdbstub::conn::ConnectionExt;
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{DisconnectReason, GdbStub, MultiThreadStopReason};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::multithread::{
    MultiThreadBase, MultiThreadResume, MultiThreadResumeOps, MultiThreadSchedulerLocking,
    MultiThreadSchedulerLockingOps, MultiThreadSingleStep, MultiThreadSingleStepOps,
};
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, HwBreakpoint, HwBreakpointOps, HwWatchpoint, HwWatchpointOps,
    SwBreakpoint, SwBreakpointOps, WatchKind,
};
use gdbstub::target::ext::thread_extra_info::{ThreadExtraInfo, ThreadExtraInfoOps};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::x86::X86_64_SSE;
use gdbstub_arch::x86::reg::X86_64CoreRegs;
use rewind_vmm::debug::{Access, DebugStop, MAX_TRAPS, Stepping, Trap};
use rewind_vmm::pv::{TaskLayout, pt_regs};
use rewind_vmm::{Machine, Observer, Outcome};

use crate::threads::{Memory, Tasks, Thread};

/// How far the fork runs between looks at the connection, in steps, so a
/// Ctrl-C in gdb stops a VM that is running free.
const CHUNK_STEPS: u64 = 10_000;

/// The order gdb's x86-64 description lists the general purpose
/// registers in.
const GPR_COUNT: usize = 16;

/// The most bytes one debug register watches.
const MAX_PIECE: u64 = 8;

/// CR3's page table base, bits 12 to 51, which names an address space;
/// the bits below are the PCID, which changes as the kernel switches. The
/// guest boots with mitigations off, so a process has one page table, not
/// a user and a kernel one.
const CR3_PAGE_TABLE: u64 = 0x000f_ffff_ffff_f000;

/// Where x86-64's lower half, user space, ends.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// RFLAGS' resume flag: the next instruction runs without its breakpoint
/// trapping, and the CPU clears it once that instruction has.
const RFLAGS_RF: u64 = 1 << 16;

/// The CPU's thread id for gdb, the same at every stop whichever task is
/// on it, so a step or a trap always stops the thread gdb ran. One past
/// PID_MAX_LIMIT, the most pids a 64-bit kernel hands out, so no task has
/// it.
const CPU_TID: usize = 4_194_305;

/// RPL bits of a code segment selector: 3 in user space.
const SELECTOR_RPL: u64 = 0b11;
const USER_RPL: u64 = 0b11;

/// Whose traps and threads gdb sees: the process with this id, which
/// `rewind gdb` loads the symbols of, or any, when it debugs no process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    Process(u32),
    #[default]
    Any,
}

/// Whether a task is the one on the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnCpu {
    Yes,
    No,
}

/// Where gdb gets a thread's registers from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The vCPU, whichever task is on it.
    Cpu,
    /// A thread of the debugged process, by its task struct's address: the
    /// vCPU while the thread runs in user space on it, else the user
    /// registers it saved in its struct pt_regs.
    Task(u64, OnCpu),
}

/// A thread as gdb sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    tid: Tid,
    source: Source,
    name: String,
}

/// The CPU's thread id as gdb takes it.
fn cpu_tid() -> Tid {
    Tid::new(CPU_TID).expect("CPU_TID is not 0")
}

/// The threads gdb sees: the CPU, named for the task on it, first, then
/// every thread of the debugged process. Thread ids are never 0; the idle
/// task's is, and it is no process's.
fn seen(cpu: Option<&Thread>, process: &[Thread]) -> Vec<Seen> {
    let name = match cpu {
        Some(t) if t.tid == 0 => "the CPU, idle".to_string(),
        Some(t) => format!("the CPU, in {} {}", t.name, t.tid),
        None => "the CPU".to_string(),
    };
    let on_cpu = Seen {
        tid: cpu_tid(),
        source: Source::Cpu,
        name,
    };
    let threads = process.iter().filter_map(|t| {
        let on = match cpu {
            Some(c) if c.task == t.task => OnCpu::Yes,
            _ => OnCpu::No,
        };
        Some(Seen {
            tid: Tid::new(t.tid as usize)?,
            source: Source::Task(t.task, on),
            name: t.name.clone(),
        })
    });
    std::iter::once(on_cpu).chain(threads).collect()
}

/// The number gdb gives thread `tid` of the debugged process among
/// `threads`: gdb numbers the threads a stub lists from 1, in the order
/// listed, and the CPU comes first. None for a thread the process lacks.
fn thread_number(threads: &[Seen], tid: u32) -> Option<usize> {
    threads
        .iter()
        .position(|t| matches!(t.source, Source::Task(..)) && t.tid.get() == tid as usize)
        .map(|index| index + 1)
}

/// gdb's registers from a struct pt_regs: the user registers a thread
/// saved, with the data segments user space runs with, which are 0.
fn saved_registers(words: &[u64; pt_regs::WORDS]) -> X86_64CoreRegs {
    let segments = gdbstub_arch::x86::reg::X86SegmentRegs {
        cs: words[pt_regs::CS] as u16 as u32,
        ss: words[pt_regs::SS] as u16 as u32,
        ..Default::default()
    };
    X86_64CoreRegs {
        // gdb's order: rax rbx rcx rdx rsi rdi rbp rsp r8 to r15.
        regs: [
            words[pt_regs::RAX],
            words[pt_regs::RBX],
            words[pt_regs::RCX],
            words[pt_regs::RDX],
            words[pt_regs::RSI],
            words[pt_regs::RDI],
            words[pt_regs::RBP],
            words[pt_regs::RSP],
            words[pt_regs::R8],
            words[pt_regs::R9],
            words[pt_regs::R10],
            words[pt_regs::R11],
            words[pt_regs::R12],
            words[pt_regs::R13],
            words[pt_regs::R14],
            words[pt_regs::R15],
        ],
        rip: words[pt_regs::RIP],
        eflags: words[pt_regs::RFLAGS] as u32,
        segments,
        ..Default::default()
    }
}

/// The VM kernel's memory, as the vCPU's page tables map it; the kernel's
/// half is the same in every process's.
impl Memory for Machine {
    fn read(&self, address: u64, buf: &mut [u8]) -> Result<()> {
        let n = self.read_virtual(address, buf)?;
        if n < buf.len() {
            anyhow::bail!("{:#x} is not mapped", address + n as u64);
        }
        Ok(())
    }
}

/// What gdb asked the fork to do next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Continue,
    Step,
}

impl Mode {
    fn stepping(self) -> Stepping {
        match self {
            Mode::Continue => Stepping::No,
            Mode::Step => Stepping::Yes,
        }
    }
}

/// A range gdb watches, as it asked for it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Watch {
    address: u64,
    len: u64,
    kind: WatchKind,
}

/// gdb's breakpoints and watchpoints, which share the CPU's four debug
/// address registers. A watched range takes one register for each aligned
/// piece of it the CPU can watch.
#[derive(Default)]
struct Traps {
    breakpoints: Vec<u64>,
    watches: Vec<Watch>,
    /// Breakpoints refused for want of a register, said once each.
    refused: Vec<u64>,
}

impl Traps {
    fn add_breakpoint(&mut self, address: u64) -> bool {
        if self.breakpoints.contains(&address) {
            return true;
        }
        if self.registers().len() == MAX_TRAPS {
            return false;
        }
        self.breakpoints.push(address);
        true
    }

    /// Says that a breakpoint at `address` found no debug register, the
    /// first time gdb asks for it, and returns false for gdb. gdb reports
    /// a breakpoint it cannot insert, except one in a shared library, which
    /// it sets aside without a word in batch mode, and the session would
    /// run past it.
    fn refuse_breakpoint(&mut self, address: u64) -> bool {
        if !self.refused.contains(&address) {
            self.refused.push(address);
            eprintln!(
                "rewind: no debug register left for a breakpoint at {address:#x}: \
                 breakpoints and watchpoints share the CPU's {MAX_TRAPS}, one for each \
                 location; delete one to set this"
            );
        }
        false
    }

    fn remove_breakpoint(&mut self, address: u64) -> bool {
        let before = self.breakpoints.len();
        self.breakpoints.retain(|b| *b != address);
        self.breakpoints.len() != before
    }

    /// Adds a watch if all its pieces fit in the registers left. x86 has
    /// no trap on reads alone, so a read watch is refused, and gdb says so.
    fn add_watch(&mut self, address: u64, len: u64, kind: WatchKind) -> bool {
        let watch = Watch { address, len, kind };
        if kind == WatchKind::Read {
            return false;
        }
        if self.watches.contains(&watch) {
            return true;
        }
        if self.registers().len() + pieces(address, len).len() > MAX_TRAPS {
            return false;
        }
        self.watches.push(watch);
        true
    }

    fn remove_watch(&mut self, address: u64, len: u64, kind: WatchKind) -> bool {
        let before = self.watches.len();
        self.watches.retain(|w| *w != Watch { address, len, kind });
        self.watches.len() != before
    }

    /// One trap per register: the breakpoints, then each watch's pieces.
    fn registers(&self) -> Vec<Trap> {
        let breakpoints = self.breakpoints.iter().map(|a| Trap::Execute(*a));
        let watches = self.watches.iter().flat_map(|w| {
            let access = match w.kind {
                WatchKind::Write => Access::Write,
                WatchKind::Read | WatchKind::ReadWrite => Access::ReadWrite,
            };
            pieces(w.address, w.len)
                .into_iter()
                .map(move |(address, len)| Trap::Watch {
                    address,
                    len,
                    access,
                })
        });
        breakpoints.chain(watches).collect()
    }

    /// The watch, as gdb set it, that the piece at `address` belongs to.
    fn watch_at(&self, address: u64) -> Option<(u64, WatchKind)> {
        self.watches
            .iter()
            .find(|w| (w.address..w.address + w.len).contains(&address))
            .map(|w| (w.address, w.kind))
    }
}

/// `len` bytes at `address` as the pieces a debug register can watch: at
/// each address, the largest power of two up to 8 that it is aligned to
/// and that does not run past the end.
fn pieces(address: u64, len: u64) -> Vec<(u64, u64)> {
    let end = address + len;
    let mut at = address;
    let mut out = Vec::new();
    while at < end {
        let mut size = MAX_PIECE;
        while !at.is_multiple_of(size) || at + size > end {
            size /= 2;
        }
        out.push((at, size));
        at += size;
    }
    out
}

/// The page table base of a CR3 value.
fn page_table(cr3: u64) -> u64 {
    cr3 & CR3_PAGE_TABLE
}

/// Whether a trap at `address`, taken with `cr3`, is in the address space
/// gdb debugs: a kernel address always is, being every process's.
fn in_scope(address: u64, space: Option<u64>, cr3: u64) -> bool {
    match space {
        Some(space) if address < USER_END => page_table(cr3) == space,
        _ => true,
    }
}

/// A forked machine under gdb.
pub struct Debuggee {
    machine: Machine,
    traps: Traps,
    mode: Mode,
    /// The thread gdb asked to step, which it expects the step to stop.
    stepped: Option<Tid>,
    /// The process gdb debugs, if any.
    process: Option<u32>,
    /// The page table of the process gdb debugs, if any.
    space: Option<u64>,
    /// Where the VM kernel keeps its tasks, when it says.
    layout: Option<TaskLayout>,
    /// The threads gdb sees, as of the last stop.
    threads: Vec<Seen>,
    follow: Follow,
    /// Whether gdb's user has been told the fork left the recording.
    told: bool,
}

impl Debuggee {
    /// A debuggee for `machine`, a fork of a run at a step, with `made`,
    /// the records the run made after that step. With [`Scope::Process`],
    /// that process's address space is the one gdb debugs, found in the
    /// kernel's tasks, or, from a kernel that publishes no task layout,
    /// the one the machine is in now.
    pub fn new(machine: Machine, made: Vec<(u64, Vec<u8>)>, scope: Scope) -> Result<Debuggee> {
        let layout = machine.task_layout()?;
        let process = match scope {
            Scope::Process(pid) => Some(pid),
            Scope::Any => None,
        };
        let space = match (process, layout) {
            (Some(pid), Some(layout)) => {
                let tasks = Tasks::new(&machine, layout);
                let task = tasks
                    .process(pid)?
                    .with_context(|| format!("no process {pid} at this step"))?;
                tasks.page_table(task)?
            }
            (Some(_), None) => Some(page_table(machine.special_registers()?.cr3)),
            (None, _) => None,
        };
        let mut debuggee = Debuggee {
            machine,
            traps: Traps::default(),
            mode: Mode::Continue,
            stepped: None,
            process,
            space,
            layout,
            threads: Vec::new(),
            follow: Follow::new(made),
            told: false,
        };
        debuggee.refresh_threads();
        Ok(debuggee)
    }

    /// Reads the threads gdb sees again, after the machine has run. A task
    /// list the kernel was changing at the stop leaves the CPU alone.
    fn refresh_threads(&mut self) {
        self.threads = match self.read_threads() {
            Ok(threads) => threads,
            Err(e) => {
                eprintln!("rewind: only the CPU's thread at this stop: {e:#}");
                seen(None, &[])
            }
        };
    }

    fn read_threads(&self) -> Result<Vec<Seen>> {
        let Some(layout) = self.layout else {
            return Ok(seen(None, &[]));
        };
        let tasks = Tasks::new(&self.machine, layout);
        let current = tasks.current()?;
        let cpu = Thread {
            tid: tasks.tid(current)?,
            task: current,
            name: tasks.name(current)?,
        };
        let process = match self.process {
            Some(pid) => match tasks.process(pid)? {
                Some(task) => tasks.threads(task)?,
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        Ok(seen(Some(&cpu), &process))
    }

    /// Where thread `tid`'s registers come from, if gdb sees it.
    fn source(&self, tid: Tid) -> Option<Source> {
        self.threads.iter().find(|t| t.tid == tid).map(|t| t.source)
    }

    /// The number gdb gives thread `tid` of the debugged process when it
    /// connects, for its `thread` command; None when the process has no
    /// such thread at this stop.
    pub fn thread_number(&self, tid: u32) -> Option<usize> {
        thread_number(&self.threads, tid)
    }

    /// Whether the CPU is running user space.
    fn in_user_space(&self) -> Result<bool> {
        let cs = self.machine.special_registers()?.cs.selector;
        Ok(u64::from(cs) & SELECTOR_RPL == USER_RPL)
    }

    /// Whether the CPU is in the address space gdb debugs.
    fn on_cpu_space(&self) -> Result<bool> {
        let cr3 = self.machine.special_registers()?.cr3;
        Ok(self.space.is_none_or(|space| page_table(cr3) == space))
    }

    /// Whether a trap at `address`, just taken, is the debugged process's.
    fn in_scope(&self, address: u64) -> Result<bool> {
        let cr3 = self.machine.special_registers()?.cr3;
        Ok(in_scope(address, self.space, cr3))
    }

    /// Lets the instruction at a breakpoint another process reached run
    /// without trapping again.
    fn pass_breakpoint(&mut self) -> Result<()> {
        let mut regs = self.machine.registers()?;
        regs.rflags |= RFLAGS_RF;
        self.machine.set_registers(&regs)
    }

    /// Serves gdb on `conn` until it detaches or the connection closes.
    pub fn serve(&mut self, conn: TcpStream) -> Result<DisconnectReason> {
        let stub = GdbStub::new(conn);
        stub.run_blocking::<EventLoop>(self)
            .map_err(|e| anyhow::anyhow!("gdb session: {e}"))
    }
}

impl Target for Debuggee {
    type Arch = X86_64_SSE;
    type Error = String;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::MultiThread(self)
    }

    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

/// The run's records after the fork's step, compared one by one with the
/// fork's as it makes them, keeping the step of the first that differs: a
/// record with other bytes, at another step, or one the run never made.
struct Follow {
    made: std::vec::IntoIter<(u64, Vec<u8>)>,
    left_at: Option<u64>,
}

impl Follow {
    fn new(made: Vec<(u64, Vec<u8>)>) -> Follow {
        Follow {
            made: made.into_iter(),
            left_at: None,
        }
    }
}

impl Observer for Follow {
    fn record(&mut self, step: u64, record: &[u8]) {
        if self.left_at.is_some() {
            return;
        }
        match self.made.next() {
            Some((made_step, made)) if made_step == step && made == record => {}
            _ => self.left_at = Some(step),
        }
    }
}

/// A VMM error as a fatal target error.
fn fatal<E: std::fmt::Display>(e: E) -> TargetError<String> {
    TargetError::Fatal(e.to_string())
}

impl MultiThreadBase for Debuggee {
    fn read_registers(&mut self, regs: &mut X86_64CoreRegs, tid: Tid) -> TargetResult<(), Self> {
        // A thread off the CPU, or in the kernel on it: the user registers
        // it saved in its pt_regs. A thread whose registers do not read,
        // such as one that exited since the last stop, is an error gdb
        // reports for that thread; the session goes on.
        if let Some(Source::Task(task, on)) = self.source(tid)
            && !(on == OnCpu::Yes && self.in_user_space().map_err(fatal)?)
        {
            let layout = self.layout.ok_or(TargetError::NonFatal)?;
            let words = Tasks::new(&self.machine, layout)
                .user_registers(task)
                .map_err(|e| {
                    eprintln!("rewind: {e:#}");
                    TargetError::NonFatal
                })?;
            *regs = saved_registers(&words);
            return Ok(());
        }

        let r = self.machine.registers().map_err(fatal)?;
        let s = self.machine.special_registers().map_err(fatal)?;
        // gdb's order: rax rbx rcx rdx rsi rdi rbp rsp r8 to r15.
        let gprs: [u64; GPR_COUNT] = [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp, r.rsp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15,
        ];
        regs.regs = gprs;
        regs.rip = r.rip;
        regs.eflags = r.rflags as u32;
        regs.segments.cs = s.cs.selector.into();
        regs.segments.ss = s.ss.selector.into();
        regs.segments.ds = s.ds.selector.into();
        regs.segments.es = s.es.selector.into();
        regs.segments.fs = s.fs.selector.into();
        regs.segments.gs = s.gs.selector.into();
        Ok(())
    }

    fn write_registers(&mut self, regs: &X86_64CoreRegs, tid: Tid) -> TargetResult<(), Self> {
        // Only the CPU's registers can be changed.
        if let Some(Source::Task(..)) = self.source(tid) {
            return Err(TargetError::NonFatal);
        }
        let mut r = self.machine.registers().map_err(fatal)?;
        let g = regs.regs;
        (r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp, r.rsp) =
            (g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]);
        (r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15) =
            (g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]);
        r.rip = regs.rip;
        r.rflags = regs.eflags.into();
        self.machine.set_registers(&r).map_err(fatal)
    }

    fn read_addrs(&mut self, start: u64, data: &mut [u8], tid: Tid) -> TargetResult<usize, Self> {
        // A thread of the process sees its process's memory, whichever
        // process is on the CPU.
        if let (Some(Source::Task(..)), Some(space)) = (self.source(tid), self.space) {
            return self
                .machine
                .read_virtual_in(space, start, data)
                .map_err(fatal);
        }
        self.machine.read_virtual(start, data).map_err(fatal)
    }

    fn write_addrs(&mut self, start: u64, data: &[u8], tid: Tid) -> TargetResult<(), Self> {
        // Writes go through the CPU's page tables, so a thread of the
        // process writes only while its process is on the CPU.
        if let Some(Source::Task(..)) = self.source(tid)
            && !self.on_cpu_space().map_err(fatal)?
        {
            return Err(TargetError::NonFatal);
        }
        self.machine
            .write_virtual(start, data)
            .map_err(|_| TargetError::NonFatal)
    }

    fn list_active_threads(&mut self, thread_is_active: &mut dyn FnMut(Tid)) -> Result<(), String> {
        for t in &self.threads {
            thread_is_active(t.tid);
        }
        Ok(())
    }

    fn support_resume(&mut self) -> Option<MultiThreadResumeOps<'_, Self>> {
        Some(self)
    }

    fn support_thread_extra_info(&mut self) -> Option<ThreadExtraInfoOps<'_, Self>> {
        Some(self)
    }
}

impl ThreadExtraInfo for Debuggee {
    /// The CPU's task, or a thread's name and whether it is on the CPU.
    fn thread_extra_info(&self, tid: Tid, buf: &mut [u8]) -> Result<usize, String> {
        let Some(t) = self.threads.iter().find(|t| t.tid == tid) else {
            return Ok(0);
        };
        let info = match t.source {
            Source::Task(_, OnCpu::Yes) => format!("{}, on the CPU", t.name),
            Source::Cpu | Source::Task(_, OnCpu::No) => t.name.clone(),
        };
        let n = info.len().min(buf.len());
        buf[..n].copy_from_slice(&info.as_bytes()[..n]);
        Ok(n)
    }
}

// The VM has one CPU, and its scheduler picks the task on it, so gdb's
// resume actions come down to one: run the CPU, or step it. A step asked
// of a thread off the CPU steps the CPU, and gdb then stops in whichever
// thread that was.
impl MultiThreadResume for Debuggee {
    fn resume(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn clear_resume_actions(&mut self) -> Result<(), Self::Error> {
        self.mode = Mode::Continue;
        self.stepped = None;
        Ok(())
    }

    fn set_resume_action_continue(
        &mut self,
        _tid: Tid,
        signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("a signal cannot be delivered to the whole VM".into());
        }
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<MultiThreadSingleStepOps<'_, Self>> {
        Some(self)
    }

    fn support_scheduler_locking(&mut self) -> Option<MultiThreadSchedulerLockingOps<'_, Self>> {
        Some(self)
    }
}

// gdb asks to run one thread alone when it steps past a breakpoint. The
// CPU's thread is the only one that runs anyway: the other threads move
// only when the guest's scheduler puts them on the CPU.
impl MultiThreadSchedulerLocking for Debuggee {
    fn set_resume_action_scheduler_lock(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl MultiThreadSingleStep for Debuggee {
    fn set_resume_action_step(
        &mut self,
        tid: Tid,
        signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("a signal cannot be delivered to the whole VM".into());
        }
        self.mode = Mode::Step;
        self.stepped = Some(tid);
        Ok(())
    }
}

// gdb's `break` and `hbreak` both become debug register breakpoints, so
// nothing is written into the VM's memory, and `watch` and `awatch` become
// debug register watchpoints; there are four registers between them.
impl Breakpoints for Debuggee {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }

    fn support_hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
        Some(self)
    }

    fn support_hw_watchpoint(&mut self) -> Option<HwWatchpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for Debuggee {
    fn add_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.add_breakpoint(address) || self.traps.refuse_breakpoint(address))
    }

    fn remove_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.remove_breakpoint(address))
    }
}

impl HwBreakpoint for Debuggee {
    fn add_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.add_breakpoint(address) || self.traps.refuse_breakpoint(address))
    }

    fn remove_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.remove_breakpoint(address))
    }
}

impl HwWatchpoint for Debuggee {
    fn add_hw_watchpoint(
        &mut self,
        address: u64,
        len: u64,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.traps.add_watch(address, len, kind))
    }

    fn remove_hw_watchpoint(
        &mut self,
        address: u64,
        len: u64,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.traps.remove_watch(address, len, kind))
    }
}

/// Runs the fork for gdb in chunks, looking at the connection between
/// them for a Ctrl-C.
enum EventLoop {}

impl BlockingEventLoop for EventLoop {
    type Target = Debuggee;
    type Connection = TcpStream;
    type StopReason = MultiThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut Debuggee,
        conn: &mut TcpStream,
    ) -> Result<Event<Self::StopReason>, WaitForStopReasonError<String, std::io::Error>> {
        let target_error = |e: anyhow::Error| WaitForStopReasonError::Target(e.to_string());
        let traps = target.traps.registers();
        target
            .machine
            .set_debug(target.mode.stepping(), &traps)
            .map_err(target_error)?;

        // Each stop reads the threads again, and names the CPU's.
        let stop = |target: &mut Debuggee, reason: fn(Tid) -> MultiThreadStopReason<u64>| {
            target.refresh_threads();
            Ok(Event::TargetStopped(reason(cpu_tid())))
        };

        // A step is the vCPU's one instruction, done in the thread gdb
        // stepped, which gdb expects to stop; a stop in another thread
        // would read to gdb as a signal it never asked for. The process's
        // thread on the CPU shows its user registers while the vCPU runs
        // the kernel, so its pc moves once the kernel returns to it. A
        // stepped thread that has exited cannot stop: the CPU's thread
        // stops with no signal, which ends gdb's stepping without one.
        let step_done = |target: &mut Debuggee| {
            target.refresh_threads();
            let (tid, signal) = match target.stepped {
                Some(tid) if target.source(tid).is_none() => (cpu_tid(), Signal::SIGZERO),
                Some(tid) => (tid, Signal::SIGTRAP),
                None => (cpu_tid(), Signal::SIGTRAP),
            };
            Ok(Event::TargetStopped(
                MultiThreadStopReason::SignalWithThread { tid, signal },
            ))
        };

        loop {
            let until = target.machine.step() + CHUNK_STEPS;
            let outcome = target
                .machine
                .run(Some(until), &mut target.follow)
                .map_err(target_error)?;

            // Said once, when it happens, so gdb's user knows what follows
            // is not what the run did.
            if let (Some(step), false) = (target.follow.left_at, target.told) {
                target.told = true;
                eprintln!(
                    "rewind: the fork left the recording at step {step}; \
                     from there on it is not the run"
                );
            }
            match outcome {
                Outcome::Debug(DebugStop::Step) => return step_done(target),
                // A trap in another process's address space is passed:
                // the fork runs on, or a step is done.
                Outcome::Debug(DebugStop::Breakpoint(address))
                    if !target.in_scope(address).map_err(target_error)? =>
                {
                    if target.mode == Mode::Step {
                        return step_done(target);
                    }
                    target.pass_breakpoint().map_err(target_error)?;
                    continue;
                }
                Outcome::Debug(DebugStop::Watchpoint(address))
                    if !target.in_scope(address).map_err(target_error)? =>
                {
                    if target.mode == Mode::Step {
                        return step_done(target);
                    }
                    continue;
                }
                Outcome::Debug(DebugStop::Breakpoint(_)) => {
                    return stop(target, MultiThreadStopReason::SwBreak);
                }
                Outcome::Debug(DebugStop::Watchpoint(piece)) => {
                    target.refresh_threads();
                    let tid = cpu_tid();
                    let reason = match target.traps.watch_at(piece) {
                        Some((addr, kind)) => MultiThreadStopReason::Watch { tid, kind, addr },
                        None => MultiThreadStopReason::SignalWithThread {
                            tid,
                            signal: Signal::SIGTRAP,
                        },
                    };
                    return Ok(Event::TargetStopped(reason));
                }
                Outcome::Stopped(_) => {
                    return Ok(Event::TargetStopped(MultiThreadStopReason::Exited(0)));
                }
                Outcome::Paused => {}
            }

            // Between chunks: anything from gdb, a Ctrl-C most likely.
            if conn
                .peek()
                .map_err(WaitForStopReasonError::Connection)?
                .is_some()
            {
                let byte = conn.read().map_err(WaitForStopReasonError::Connection)?;
                return Ok(Event::IncomingData(byte));
            }
        }
    }

    fn on_interrupt(target: &mut Debuggee) -> Result<Option<Self::StopReason>, String> {
        target.refresh_threads();
        Ok(Some(MultiThreadStopReason::SignalWithThread {
            tid: cpu_tid(),
            signal: Signal::SIGINT,
        }))
    }
}

#[cfg(test)]
mod tests {
    // A fork's records compared with the run's, as gdb runs it: the step
    // of the first that differs is kept, and only that one.
    use super::*;

    /// Address spaces are told apart by CR3's page table base alone: the
    /// PCID in its low bits changes as the kernel switches. User addresses
    /// are filtered by them, kernel addresses never, and with no process
    /// to debug nothing is.
    #[test]
    fn user_traps_count_in_the_debugged_address_space_only() {
        let space = Some(page_table(0x0000_0001_2345_6000));
        assert!(in_scope(0x55b3_8f77_c320, space, 0x0000_0001_2345_6003));
        assert!(!in_scope(0x55b3_8f77_c320, space, 0x0000_0001_2345_7000));
        assert!(in_scope(
            0xffff_ffff_8128_5085,
            space,
            0x0000_0001_2345_7000
        ));
        assert!(in_scope(0x55b3_8f77_c320, None, 0x0000_0001_2345_7000));
    }

    fn thread(tid: u32, task: u64) -> Thread {
        Thread {
            tid,
            task,
            name: "phil".into(),
        }
    }

    /// The CPU is gdb's first thread, with an id of its own at every stop
    /// and named for the task on it; every thread of the process follows
    /// by its own id, the one on the CPU marked so.
    #[test]
    fn the_cpu_comes_first_and_every_thread_after() {
        let process = [thread(40, 0x100), thread(41, 0x200), thread(42, 0x300)];
        let threads = seen(Some(&thread(41, 0x200)), &process);
        let got: Vec<(usize, Source)> = threads.iter().map(|t| (t.tid.get(), t.source)).collect();
        assert_eq!(
            got,
            vec![
                (CPU_TID, Source::Cpu),
                (40, Source::Task(0x100, OnCpu::No)),
                (41, Source::Task(0x200, OnCpu::Yes)),
                (42, Source::Task(0x300, OnCpu::No)),
            ]
        );
        assert_eq!(threads[0].name, "the CPU, in phil 41");

        let idle = Thread {
            tid: 0,
            task: 0x900,
            name: "swapper/0".into(),
        };
        let threads = seen(Some(&idle), &process);
        assert_eq!(threads[0].tid.get(), CPU_TID);
        assert_eq!(threads[0].name, "the CPU, idle");
        assert!(
            threads[1..]
                .iter()
                .all(|t| matches!(t.source, Source::Task(_, OnCpu::No)))
        );
    }

    /// gdb numbers threads from 1 in the order the stub lists them, so the
    /// CPU is thread 1 and the process's threads follow; the CPU's own id
    /// and a thread the process lacks have no process thread's number.
    #[test]
    fn gdb_numbers_the_process_threads_after_the_cpu() {
        let process = [thread(40, 0x100), thread(41, 0x200), thread(42, 0x300)];
        let threads = seen(Some(&thread(41, 0x200)), &process);
        assert_eq!(thread_number(&threads, 40), Some(2));
        assert_eq!(thread_number(&threads, 42), Some(4));
        assert_eq!(thread_number(&threads, 43), None);
        assert_eq!(thread_number(&threads, CPU_TID as u32), None);
    }

    /// A saved thread's registers are its pt_regs words in gdb's order,
    /// with the selectors it saved and data segments of 0.
    #[test]
    fn saved_registers_follow_gdbs_order() {
        let mut words = [0u64; pt_regs::WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            *w = 0x1000 + i as u64;
        }
        words[pt_regs::CS] = 0x33;
        words[pt_regs::SS] = 0x2b;
        let regs = saved_registers(&words);
        assert_eq!(regs.regs[0], 0x1000 + pt_regs::RAX as u64);
        assert_eq!(regs.regs[6], 0x1000 + pt_regs::RBP as u64);
        assert_eq!(regs.regs[7], 0x1000 + pt_regs::RSP as u64);
        assert_eq!(regs.regs[15], 0x1000 + pt_regs::R15 as u64);
        assert_eq!(regs.rip, 0x1000 + pt_regs::RIP as u64);
        assert_eq!(regs.segments.cs, 0x33);
        assert_eq!(regs.segments.ss, 0x2b);
        assert_eq!(regs.segments.fs, 0);
    }

    /// Splits watched ranges as the CPU needs them: the largest aligned
    /// piece of up to 8 bytes at each address, so an aligned pointer is one
    /// piece and an odd range is several.
    #[test]
    fn a_watched_range_splits_into_aligned_pieces() {
        assert_eq!(pieces(0x1000, 8), vec![(0x1000, 8)]);
        assert_eq!(pieces(0x1004, 4), vec![(0x1004, 4)]);
        assert_eq!(
            pieces(0x1001, 7),
            vec![(0x1001, 1), (0x1002, 2), (0x1004, 4)]
        );
        assert_eq!(pieces(0x1000, 16), vec![(0x1000, 8), (0x1008, 8)]);
        assert_eq!(pieces(0x1000, 0), vec![]);
    }

    /// Breakpoints and watch pieces share the four registers: a watch that
    /// does not fit is refused whole, a read-only watch is refused since
    /// x86 has none, and a hit on any piece names the watch gdb set.
    #[test]
    fn breakpoints_and_watches_share_the_four_registers() {
        let mut traps = Traps::default();
        assert!(traps.add_breakpoint(0x400000));
        assert!(traps.add_watch(0x1000, 8, WatchKind::Write));
        assert!(!traps.add_watch(0x2001, 7, WatchKind::Write));
        assert!(!traps.add_watch(0x3000, 4, WatchKind::Read));
        assert!(traps.add_watch(0x2002, 6, WatchKind::ReadWrite));
        assert_eq!(
            traps.registers(),
            vec![
                Trap::Execute(0x400000),
                Trap::Watch {
                    address: 0x1000,
                    len: 8,
                    access: Access::Write
                },
                Trap::Watch {
                    address: 0x2002,
                    len: 2,
                    access: Access::ReadWrite
                },
                Trap::Watch {
                    address: 0x2004,
                    len: 4,
                    access: Access::ReadWrite
                },
            ]
        );
        assert_eq!(traps.watch_at(0x2004), Some((0x2002, WatchKind::ReadWrite)));
        assert_eq!(traps.watch_at(0x5000), None);

        assert!(traps.remove_watch(0x2002, 6, WatchKind::ReadWrite));
        assert!(!traps.remove_watch(0x2002, 6, WatchKind::ReadWrite));
        assert_eq!(traps.registers().len(), 2);
    }

    /// Feeds a follower the run's own records, then records that differ
    /// in bytes and in step, and checks it keeps the first difference.
    #[test]
    fn a_fork_leaves_the_recording_at_its_first_different_record() {
        let made = vec![
            (10, b"a".to_vec()),
            (12, b"b".to_vec()),
            (15, b"c".to_vec()),
        ];

        let mut same = Follow::new(made.clone());
        same.record(10, b"a");
        same.record(12, b"b");
        same.record(15, b"c");
        assert_eq!(same.left_at, None);

        let mut other_bytes = Follow::new(made.clone());
        other_bytes.record(10, b"a");
        other_bytes.record(12, b"x");
        other_bytes.record(15, b"c");
        assert_eq!(other_bytes.left_at, Some(12));

        let mut other_step = Follow::new(made.clone());
        other_step.record(11, b"a");
        assert_eq!(other_step.left_at, Some(11));

        let mut past_the_end = Follow::new(made);
        for (step, record) in [(10, b"a"), (12, b"b"), (15, b"c"), (20, b"d")] {
            past_the_end.record(step, record);
        }
        assert_eq!(past_the_end.left_at, Some(20));
    }
}
