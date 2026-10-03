//! gdb at a step: a fork of a run, stopped at the step, served over the
//! GDB remote protocol. gdb sees one x86-64 CPU with the VM's memory as the
//! VM's page tables map it: the kernel, which the VM kernel's vmlinux has
//! the symbols for, and the user space of whichever process was running.
//! Breakpoints and watchpoints at user addresses stop the fork only in the
//! process gdb is debugging; other processes map the same addresses to
//! memory of their own. Continuing and stepping run the fork, never the
//! recording. The fork's
//! records are compared with the run's as it goes, and gdb's user is told
//! the step where the two first differ: from there on the fork is not the
//! run, whether gdb changed its memory or the debugging itself moved it.

use std::net::TcpStream;

use anyhow::Result;
use gdbstub::common::Signal;
use gdbstub::conn::ConnectionExt;
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{DisconnectReason, GdbStub, SingleThreadStopReason};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadResumeOps, SingleThreadSingleStep,
    SingleThreadSingleStepOps,
};
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, HwBreakpoint, HwBreakpointOps, HwWatchpoint, HwWatchpointOps,
    SwBreakpoint, SwBreakpointOps, WatchKind,
};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::x86::X86_64_SSE;
use gdbstub_arch::x86::reg::X86_64CoreRegs;
use rewind_vmm::debug::{Access, DebugStop, MAX_TRAPS, Stepping, Trap};
use rewind_vmm::{Machine, Observer, Outcome};

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

/// Whose traps gdb sees: the process running at the fork's step, which
/// `rewind gdb` loads the symbols of, or any, when no process was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    Process,
    #[default]
    Any,
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
    /// The page table of the process gdb debugs, if one was running.
    space: Option<u64>,
    follow: Follow,
    /// Whether gdb's user has been told the fork left the recording.
    told: bool,
}

impl Debuggee {
    /// A debuggee for `machine`, a fork of a run at a step, with `made`,
    /// the records the run made after that step. With [`Scope::Process`],
    /// the address space the machine is in now is the one gdb debugs.
    pub fn new(machine: Machine, made: Vec<(u64, Vec<u8>)>, scope: Scope) -> Result<Debuggee> {
        let space = match scope {
            Scope::Process => Some(page_table(machine.special_registers()?.cr3)),
            Scope::Any => None,
        };
        Ok(Debuggee {
            machine,
            traps: Traps::default(),
            mode: Mode::Continue,
            space,
            follow: Follow::new(made),
            told: false,
        })
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
        BaseOps::SingleThread(self)
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

impl SingleThreadBase for Debuggee {
    fn read_registers(&mut self, regs: &mut X86_64CoreRegs) -> TargetResult<(), Self> {
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

    fn write_registers(&mut self, regs: &X86_64CoreRegs) -> TargetResult<(), Self> {
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

    fn read_addrs(&mut self, start: u64, data: &mut [u8]) -> TargetResult<usize, Self> {
        self.machine.read_virtual(start, data).map_err(fatal)
    }

    fn write_addrs(&mut self, start: u64, data: &[u8]) -> TargetResult<(), Self> {
        self.machine
            .write_virtual(start, data)
            .map_err(|_| TargetError::NonFatal)
    }

    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for Debuggee {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("a signal cannot be delivered to the whole VM".into());
        }
        self.mode = Mode::Continue;
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for Debuggee {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("a signal cannot be delivered to the whole VM".into());
        }
        self.mode = Mode::Step;
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
        Ok(self.traps.add_breakpoint(address))
    }

    fn remove_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.remove_breakpoint(address))
    }
}

impl HwBreakpoint for Debuggee {
    fn add_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.traps.add_breakpoint(address))
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
    type StopReason = SingleThreadStopReason<u64>;

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
                Outcome::Debug(DebugStop::Step) => {
                    return Ok(Event::TargetStopped(SingleThreadStopReason::DoneStep));
                }
                // A trap in another process's address space is passed:
                // the fork runs on, or a step is done.
                Outcome::Debug(DebugStop::Breakpoint(address))
                    if !target.in_scope(address).map_err(target_error)? =>
                {
                    if target.mode == Mode::Step {
                        return Ok(Event::TargetStopped(SingleThreadStopReason::DoneStep));
                    }
                    target.pass_breakpoint().map_err(target_error)?;
                    continue;
                }
                Outcome::Debug(DebugStop::Watchpoint(address))
                    if !target.in_scope(address).map_err(target_error)? =>
                {
                    if target.mode == Mode::Step {
                        return Ok(Event::TargetStopped(SingleThreadStopReason::DoneStep));
                    }
                    continue;
                }
                Outcome::Debug(DebugStop::Breakpoint(_)) => {
                    return Ok(Event::TargetStopped(SingleThreadStopReason::SwBreak(())));
                }
                Outcome::Debug(DebugStop::Watchpoint(piece)) => {
                    let stop = match target.traps.watch_at(piece) {
                        Some((addr, kind)) => SingleThreadStopReason::Watch {
                            tid: (),
                            kind,
                            addr,
                        },
                        None => SingleThreadStopReason::Signal(Signal::SIGTRAP),
                    };
                    return Ok(Event::TargetStopped(stop));
                }
                Outcome::Stopped(_) => {
                    return Ok(Event::TargetStopped(SingleThreadStopReason::Exited(0)));
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

    fn on_interrupt(_target: &mut Debuggee) -> Result<Option<Self::StopReason>, String> {
        Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT)))
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
