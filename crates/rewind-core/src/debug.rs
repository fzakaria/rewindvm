//! gdb at a step: a fork of a run, stopped at the step, served over the
//! GDB remote protocol. gdb sees one x86-64 CPU with the VM's memory as the
//! VM's page tables map it: the kernel, which the VM kernel's vmlinux has
//! the symbols for, and the user space of whichever process was running.
//! Continuing and stepping run the fork, never the recording. The fork's
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
    Breakpoints, BreakpointsOps, HwBreakpoint, HwBreakpointOps, SwBreakpoint, SwBreakpointOps,
};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::x86::X86_64_SSE;
use gdbstub_arch::x86::reg::X86_64CoreRegs;
use rewind_vmm::debug::{DebugStop, MAX_BREAKPOINTS, Stepping};
use rewind_vmm::{Machine, Observer, Outcome};

/// How far the fork runs between looks at the connection, in steps, so a
/// Ctrl-C in gdb stops a VM that is running free.
const CHUNK_STEPS: u64 = 10_000;

/// The order gdb's x86-64 description lists the general purpose
/// registers in.
const GPR_COUNT: usize = 16;

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

/// A forked machine under gdb.
pub struct Debuggee {
    machine: Machine,
    breakpoints: Vec<u64>,
    mode: Mode,
    follow: Follow,
    /// Whether gdb's user has been told the fork left the recording.
    told: bool,
}

impl Debuggee {
    /// A debuggee for `machine`, a fork of a run at a step, with `made`,
    /// the records the run made after that step.
    pub fn new(machine: Machine, made: Vec<(u64, Vec<u8>)>) -> Debuggee {
        Debuggee {
            machine,
            breakpoints: Vec::new(),
            mode: Mode::Continue,
            follow: Follow::new(made),
            told: false,
        }
    }

    /// Serves gdb on `conn` until it detaches or the connection closes.
    pub fn serve(&mut self, conn: TcpStream) -> Result<DisconnectReason> {
        let stub = GdbStub::new(conn);
        stub.run_blocking::<EventLoop>(self)
            .map_err(|e| anyhow::anyhow!("gdb session: {e}"))
    }

    fn add_breakpoint(&mut self, address: u64) -> bool {
        if self.breakpoints.contains(&address) {
            return true;
        }
        if self.breakpoints.len() == MAX_BREAKPOINTS {
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
// nothing is written into the VM's memory; there are four of them.
impl Breakpoints for Debuggee {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }

    fn support_hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for Debuggee {
    fn add_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.add_breakpoint(address))
    }

    fn remove_sw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.remove_breakpoint(address))
    }
}

impl HwBreakpoint for Debuggee {
    fn add_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.add_breakpoint(address))
    }

    fn remove_hw_breakpoint(&mut self, address: u64, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.remove_breakpoint(address))
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
        let breakpoints = target.breakpoints.clone();
        target
            .machine
            .set_debug(target.mode.stepping(), &breakpoints)
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
                Outcome::Debug(DebugStop::Breakpoint(_)) => {
                    return Ok(Event::TargetStopped(SingleThreadStopReason::SwBreak(())));
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
