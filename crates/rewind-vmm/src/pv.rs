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

/// The largest record the guest writes, header included.
pub const RECORD_MAX: usize = 8192;
/// The record header: len, kind, flags, pid, tid, aux.
pub const RECORD_HEADER: usize = 20;

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

/// Under a schedule seed, one exit in this many asks the guest to
/// reschedule. A request with nothing else runnable changes nothing, so
/// asking often costs little; asking rarely found mylib's shutdown race in
/// one schedule of 64, asking at one exit in four in half of them.
const PREEMPT_ONE_IN: u64 = 4;

/// Under a schedule seed, a timer armed in the window fires up to this
/// much later than asked, as Linux's default timer slack for user tasks
/// allows on real hardware.
pub const TIMER_SLACK_NS: u64 = 50_000;

/// Under a schedule seed, one exit in this many stalls the task running
/// then: it sleeps when it next returns to user space, for between
/// STALL_MIN_NS and STALL_MIN_NS << (STALL_DOUBLINGS - 1), each doubling
/// as likely as the next. A busy machine deschedules a process for
/// stretches like these, and many races need one task held up for a
/// while rather than switched away from for an instant.
const STALL_ONE_IN: u64 = 128;
pub const STALL_MIN_NS: u64 = 10_000;
pub const STALL_DOUBLINGS: u64 = 8;

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

/// A perturbed schedule. At some exits, chosen by the seed, the guest is
/// asked to reschedule, so another runnable thread may run from there, and
/// timers armed in the window fire a little late, so sleepers wake in a
/// different order. Both are functions of the seed and the step, and
/// before the window a perturbed run is the unperturbed one, exit for exit.
///
/// A fork of a fork keeps the perturbations of the runs it came from, each
/// over its own window, in `earlier`, so it is its parent exit for exit up
/// to its own window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    /// 0 for the unperturbed schedule.
    pub seed: u64,
    /// The steps preemptions may happen at.
    pub window: std::ops::Range<u64>,
    /// Perturbations inherited from earlier forks, each ending by the
    /// time the next one starts.
    pub earlier: Vec<Segment>,
}

/// One seed over one window of steps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub seed: u64,
    pub window: std::ops::Range<u64>,
}

/// splitmix64: a fixed, well-mixed function of its input.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Schedule {
    /// Whether to preempt the guest after the exit that made `step`: a
    /// function of the seed and the step alone, so a window perturbs a
    /// subset of the steps any larger window does.
    pub fn preempt_at(&self, step: u64) -> bool {
        self.seed_at(step)
            .is_some_and(|seed| mix(mix(seed) ^ step).is_multiple_of(PREEMPT_ONE_IN))
    }

    /// The seed that perturbs `step`, if any: the run's own window's, or
    /// else an inherited one's.
    pub fn seed_at(&self, step: u64) -> Option<u64> {
        let own = Segment {
            seed: self.seed,
            window: self.window.clone(),
        };
        std::iter::once(&own)
            .chain(&self.earlier)
            .find(|s| s.seed != 0 && s.window.contains(&step))
            .map(|s| s.seed)
    }

    /// The last step through which this schedule and `other` perturb every
    /// step alike, or u64::MAX when they always do. Which seed perturbs a
    /// step changes only where a window starts or ends, so comparing the
    /// two there finds the first step they differ at.
    pub fn same_through(&self, other: &Schedule) -> u64 {
        let mut edges: Vec<u64> = [self, other]
            .iter()
            .flat_map(|s| {
                std::iter::once(&s.window)
                    .chain(s.earlier.iter().map(|e| &e.window))
                    .flat_map(|w| [w.start, w.end])
            })
            .chain([0])
            .collect();
        edges.sort_unstable();
        edges
            .into_iter()
            .find(|step| self.seed_at(*step) != other.seed_at(*step))
            .map_or(u64::MAX, |step| step.saturating_sub(1))
    }

    /// How long to stall the task running after the exit that made
    /// `step`, if at all.
    pub fn stall_at(&self, step: u64) -> Option<u64> {
        let seed = self.seed_at(step)?;
        let h = mix(mix(seed ^ 0x57a1) ^ step);
        h.is_multiple_of(STALL_ONE_IN)
            .then(|| STALL_MIN_NS << ((h / STALL_ONE_IN) % STALL_DOUBLINGS))
    }

    /// Extra delay for a timer armed at `step`.
    pub fn slack_at(&self, step: u64) -> u64 {
        let Some(seed) = self.seed_at(step) else {
            return 0;
        };
        mix(mix(seed ^ 0x5eed) ^ step) % (TIMER_SLACK_NS + 1)
    }
}

#[cfg(test)]
mod tests {
    // Clock: the two ways time moves, and when the timer is due. Schedule:
    // where a seed asks for preemptions.
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

    #[test]
    fn slack_is_bounded_and_off_by_default() {
        let unperturbed = Schedule {
            seed: 0,
            window: 0..u64::MAX,
            earlier: Vec::new(),
        };
        let perturbed = Schedule {
            seed: 3,
            window: 100..200,
            earlier: Vec::new(),
        };
        assert_eq!(unperturbed.slack_at(150), 0);
        assert_eq!(perturbed.slack_at(50), 0);
        assert!((100..200).all(|s| perturbed.slack_at(s) <= TIMER_SLACK_NS));
        assert!((100..200).any(|s| perturbed.slack_at(s) > 0));
    }

    #[test]
    fn stalls_are_rare_bounded_and_off_by_default() {
        // No stalls without a seed or outside the window; with both, some,
        // each between the shortest and longest stall.
        let unperturbed = Schedule {
            seed: 0,
            window: 0..u64::MAX,
            earlier: Vec::new(),
        };
        let perturbed = Schedule {
            seed: 3,
            window: 1000..100_000,
            earlier: Vec::new(),
        };
        assert!((0..10_000).all(|s| unperturbed.stall_at(s).is_none()));
        assert!((0..1000).all(|s| perturbed.stall_at(s).is_none()));
        let stalls: Vec<u64> = (1000..100_000)
            .filter_map(|s| perturbed.stall_at(s))
            .collect();
        assert!(stalls.len() > 300 && stalls.len() < 1300);
        let longest = STALL_MIN_NS << (STALL_DOUBLINGS - 1);
        assert!(
            stalls
                .iter()
                .all(|ns| (STALL_MIN_NS..=longest).contains(ns))
        );
        assert!(stalls.contains(&STALL_MIN_NS) && stalls.contains(&longest));
    }

    fn preemptions(seed: u64, window: std::ops::Range<u64>) -> Vec<u64> {
        let s = Schedule {
            seed,
            window,
            earlier: Vec::new(),
        };
        (0..10_000).filter(|step| s.preempt_at(*step)).collect()
    }

    #[test]
    fn a_seed_picks_repeatable_steps_inside_its_window() {
        assert!(preemptions(0, 0..u64::MAX).is_empty());
        let all = preemptions(7, 0..u64::MAX);
        assert_eq!(all, preemptions(7, 0..u64::MAX));
        assert_ne!(all, preemptions(8, 0..u64::MAX));
        assert!(all.len() > 50);

        // A window perturbs exactly the steps the full schedule does
        // within it.
        let inner = preemptions(7, 2000..3000);
        let expected: Vec<u64> = all
            .into_iter()
            .filter(|s| (2000..3000).contains(s))
            .collect();
        assert_eq!(inner, expected);
    }

    /// A fork of a fork: seed 7 inherited over 1000..3000, then the fork's
    /// own seed 9 from 3000.
    fn fork_of_fork() -> Schedule {
        Schedule {
            seed: 9,
            window: 3000..u64::MAX,
            earlier: vec![Segment {
                seed: 7,
                window: 1000..3000,
            }],
        }
    }

    #[test]
    fn earlier_segments_perturb_their_own_windows() {
        // Each step is perturbed as the segment holding it says, exactly as
        // that segment alone would, and steps outside every segment are not.
        let s = fork_of_fork();
        let both: Vec<u64> = (0..10_000).filter(|step| s.preempt_at(*step)).collect();
        let mut expected = preemptions(7, 1000..3000);
        expected.extend(preemptions(9, 3000..10_000));
        assert_eq!(both, expected);
        assert!((0..1000).all(|step| s.slack_at(step) == 0 && s.stall_at(step).is_none()));
    }

    #[test]
    fn same_through_finds_the_last_step_two_schedules_agree_on() {
        // A fork of a fork at 3000 agrees with its parent, seed 7 from 1000,
        // until the step before its own. Two schedules that perturb nothing
        // agree everywhere, and an unperturbed run parts from a perturbed
        // one where the perturbation starts.
        let parent = Schedule {
            seed: 7,
            window: 1000..u64::MAX,
            earlier: Vec::new(),
        };
        let none = Schedule {
            seed: 0,
            window: 0..u64::MAX,
            earlier: Vec::new(),
        };
        assert_eq!(parent.same_through(&fork_of_fork()), 2999);
        assert_eq!(fork_of_fork().same_through(&parent), 2999);
        assert_eq!(none.same_through(&none), u64::MAX);
        assert_eq!(none.same_through(&parent), 999);
        assert_eq!(fork_of_fork().same_through(&fork_of_fork()), u64::MAX);
    }
}
