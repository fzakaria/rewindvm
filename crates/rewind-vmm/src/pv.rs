//! The Rewind paravirtual device: the guest side is
//! arch/x86/kernel/cpu/rewind.c in guest/linux/rewind-guest.patch, and the
//! two must agree on every constant here.

/// outl: guest physical address of a record.
pub const PORT_EMIT: u16 = 0x5e0;
/// inl: write the virtual clock into the shared page.
pub const PORT_CLOCK: u16 = 0x5e4;
/// outl: arm the timer this many nanoseconds from now; 0 disarms it.
pub const PORT_TIMER: u16 = 0x5ec;
/// outl: nothing in the guest is runnable.
pub const PORT_IDLE: u16 = 0x5f0;
/// outl: stop the machine, with a [`GuestExit`] value.
pub const PORT_EXIT: u16 = 0x5f4;
/// outl: guest physical address of the shared page.
pub const PORT_SETUP: u16 = 0x5f8;

/// The first serial port. Only the early console uses it, and only when
/// the command line asks for `earlyprintk`.
pub const PORT_COM1: u16 = 0x3f8;
pub const PORT_COM1_LSR: u16 = 0x3fd;
/// Line status: transmitter empty and holding register empty, so the early
/// console never waits.
pub const LSR_IDLE: u8 = 0x60;

/// Offsets of the shared page's fields.
pub const SHARED_NOW: u64 = 0;
pub const SHARED_EPOCH: u64 = 8;

/// The largest record the guest writes, header included, and its header:
/// len, kind, flags, pid, tid, aux.
pub use rewind_trace::{HEADER_LEN as RECORD_HEADER, RECORD_MAX};

/// Values the guest writes to [`PORT_EXIT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestExit {
    PowerOff,
    Restart,
    Halt,
}

impl GuestExit {
    pub fn from_port(value: u32) -> GuestExit {
        match value {
            0 => GuestExit::PowerOff,
            1 => GuestExit::Restart,
            _ => GuestExit::Halt,
        }
    }
}

/// The virtual clock and its one timer.
///
/// Time moves in two ways only. Every exit advances it by a fixed quantum,
/// so a guest that keeps exiting (reading the clock in a loop, writing
/// output) sees time pass. An idle exit jumps it to the armed deadline, the
/// way a halted CPU wakes at its next timer. Neither depends on how long the
/// host took, so the guest sees the same times on every run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    /// Nanoseconds since boot.
    pub now: u64,
    /// When the armed timer fires, if one is armed.
    pub deadline: Option<u64>,
    /// How far one exit moves time.
    pub quantum: u64,
}

impl Clock {
    pub fn new(quantum: u64) -> Clock {
        Clock {
            now: 0,
            deadline: None,
            quantum,
        }
    }

    pub fn tick(&mut self) {
        self.now += self.quantum;
    }

    pub fn arm(&mut self, delta: u64) {
        self.deadline = if delta == 0 {
            None
        } else {
            Some(self.now + delta)
        };
    }

    /// Jumps to the armed deadline. False when nothing is armed: the guest
    /// has nothing runnable and nothing scheduled, and will never wake.
    pub fn idle(&mut self) -> bool {
        match self.deadline {
            Some(deadline) => {
                self.now = self.now.max(deadline);
                true
            }
            None => false,
        }
    }

    pub fn due(&self) -> bool {
        self.deadline.is_some_and(|d| self.now >= d)
    }
}

/// Offset of the interrupt reasons in the shared page, and the reasons.
pub const SHARED_PENDING: u64 = 16;
pub const PENDING_TIMER: u32 = 1 << 0;
pub const PENDING_PREEMPT: u32 = 1 << 1;
pub const PENDING_INSPECT: u32 = 1 << 2;

/// Offsets of an inspection request in the shared page: its length, then
/// its arguments, each ending in NUL. The kernel reads at most
/// REQUEST_MAX bytes.
pub const SHARED_REQUEST_LEN: u64 = 20;
pub const SHARED_REQUEST: u64 = 24;
pub const REQUEST_MAX: usize = 2048;

/// Input for /dev/rewind-console, after the request: its length, which the
/// kernel sets back to 0 once it has taken the bytes, then the bytes.
pub const PENDING_INPUT: u32 = 1 << 3;
pub const SHARED_INPUT_LEN: u64 = SHARED_REQUEST + REQUEST_MAX as u64;
pub const SHARED_INPUT: u64 = SHARED_INPUT_LEN + 4;
pub const INPUT_MAX: usize = 1024;

/// Where a seed perturbs a run is part of the run's spec, so it lives with
/// the spec, where the app reads it too.
pub use rewind_trace::schedule::{
    STALL_DOUBLINGS, STALL_MIN_NS, Schedule, Segment, TIMER_SLACK_NS,
};

/// A stall's length in the shared page, after the console input, with its
/// interrupt reason.
pub const PENDING_STALL: u32 = 1 << 4;
pub const SHARED_STALL_NS: u64 = SHARED_INPUT + INPUT_MAX as u64;

/// The kernel's task layout in the shared page, after the stall's length
/// and aligned to 8 bytes, as [`TaskLayout`]'s fields in order, each a u64.
pub const SHARED_TASKS: u64 = (SHARED_STALL_NS + 4).next_multiple_of(8);

/// Where `rewind gdb` finds a process's threads: what the kernel writes
/// into the shared page at setup. Addresses are kernel virtual ones;
/// offsets are within the struct each field names. All zero from a kernel
/// that predates it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskLayout {
    /// init_task, whose `tasks` list holds every process.
    pub init_task: u64,
    /// The variable holding the running task's address.
    pub current_task: u64,
    /// The direct map's base: mm->pgd less this is the page table's
    /// physical address.
    pub page_offset: u64,
    /// In struct task_struct.
    pub tasks: u64,
    pub thread_node: u64,
    pub signal: u64,
    pub pid: u64,
    pub tgid: u64,
    pub stack: u64,
    pub mm: u64,
    pub comm: u64,
    /// In struct signal_struct: the list of the process's threads, linked
    /// through each task's `thread_node`.
    pub thread_head: u64,
    /// In struct mm_struct.
    pub pgd: u64,
    /// From a task's stack to its struct pt_regs.
    pub pt_regs: u64,
}

/// How many u64 fields [`TaskLayout`] has.
pub const TASK_LAYOUT_FIELDS: usize = 14;

impl TaskLayout {
    /// The layout from its fields in the shared page's order, or None when
    /// the kernel wrote none.
    pub fn from_fields(f: [u64; TASK_LAYOUT_FIELDS]) -> Option<TaskLayout> {
        if f[0] == 0 {
            return None;
        }
        Some(TaskLayout {
            init_task: f[0],
            current_task: f[1],
            page_offset: f[2],
            tasks: f[3],
            thread_node: f[4],
            signal: f[5],
            pid: f[6],
            tgid: f[7],
            stack: f[8],
            mm: f[9],
            comm: f[10],
            thread_head: f[11],
            pgd: f[12],
            pt_regs: f[13],
        })
    }
}

/// struct pt_regs as u64s, in the order of ptrace's user_regs_struct, and
/// where each register is among them.
pub mod pt_regs {
    pub const WORDS: usize = 21;
    pub const R15: usize = 0;
    pub const R14: usize = 1;
    pub const R13: usize = 2;
    pub const R12: usize = 3;
    pub const RBP: usize = 4;
    pub const RBX: usize = 5;
    pub const R11: usize = 6;
    pub const R10: usize = 7;
    pub const R9: usize = 8;
    pub const R8: usize = 9;
    pub const RAX: usize = 10;
    pub const RCX: usize = 11;
    pub const RDX: usize = 12;
    pub const RSI: usize = 13;
    pub const RDI: usize = 14;
    pub const RIP: usize = 16;
    pub const CS: usize = 17;
    pub const RFLAGS: usize = 18;
    pub const RSP: usize = 19;
    pub const SS: usize = 20;
}

#[cfg(test)]
mod tests {
    // Clock: the two ways time moves, and when the timer is due.
    use super::*;

    #[test]
    fn ticks_and_fires_at_deadline() {
        let mut clock = Clock::new(1000);
        clock.arm(2500);
        clock.tick();
        clock.tick();
        assert!(!clock.due());
        clock.tick();
        assert!(clock.due());
    }

    #[test]
    fn idle_jumps_to_deadline() {
        let mut clock = Clock::new(1000);
        clock.arm(1_000_000);
        assert!(clock.idle());
        assert_eq!(clock.now, 1_000_000);
        assert!(clock.due());
    }

    #[test]
    fn idle_without_a_timer_never_wakes() {
        let mut clock = Clock::new(1000);
        assert!(!clock.idle());
        clock.arm(10);
        clock.arm(0);
        assert!(!clock.idle());
    }
}
