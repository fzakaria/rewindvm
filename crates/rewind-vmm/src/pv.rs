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

/// A perturbed schedule. At some exits, chosen by the seed, the guest is
/// asked to reschedule, so another runnable thread may run from there, and
/// timers armed in the window fire a little late, so sleepers wake in a
/// different order. Both are functions of the seed and the step, and
/// before the window a perturbed run is the unperturbed one, exit for exit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    /// 0 for the unperturbed schedule.
    pub seed: u64,
    /// The steps preemptions may happen at.
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
        self.seed != 0
            && self.window.contains(&step)
            && mix(mix(self.seed) ^ step).is_multiple_of(PREEMPT_ONE_IN)
    }

    /// How long to stall the task running after the exit that made
    /// `step`, if at all.
    pub fn stall_at(&self, step: u64) -> Option<u64> {
        if self.seed == 0 || !self.window.contains(&step) {
            return None;
        }
        let h = mix(mix(self.seed ^ 0x57a1) ^ step);
        h.is_multiple_of(STALL_ONE_IN)
            .then(|| STALL_MIN_NS << ((h / STALL_ONE_IN) % STALL_DOUBLINGS))
    }

    /// Extra delay for a timer armed at `step`.
    pub fn slack_at(&self, step: u64) -> u64 {
        if self.seed == 0 || !self.window.contains(&step) {
            return 0;
        }
        mix(mix(self.seed ^ 0x5eed) ^ step) % (TIMER_SLACK_NS + 1)
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
        };
        let perturbed = Schedule {
            seed: 3,
            window: 100..200,
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
        };
        let perturbed = Schedule {
            seed: 3,
            window: 1000..100_000,
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
        let s = Schedule { seed, window };
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
}
