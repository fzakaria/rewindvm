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
