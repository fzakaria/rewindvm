//! How a run's machine stopped, in the words of its manifest's
//! `outcome.stop`. The engine writes them, and the command and the desktop
//! app read them, so the words are kept here, where all three read runs.

/// A guest that powered off, as init powers it off once the job is done.
pub const POWERED_OFF: &str = "poweroff";

/// The start of the words for a run stopped at its time limit, before what
/// it was doing then.
pub const TIMED_OUT: &str = "timed out";

/// What `rewind ls` and the app call a run stopped at its time limit.
pub const TIMED_OUT_ENDING: &str = "timed-out";

/// Whether a run that stopped this way ended as a job that finishes does:
/// its guest powered off. A timeout, a fault or anything else means the
/// job never finished.
pub fn clean(stop: &str) -> bool {
    stop == POWERED_OFF
}

/// Whether a run that stopped this way was stopped at its time limit.
pub fn timed_out(stop: &str) -> bool {
    stop.starts_with(TIMED_OUT)
}

#[cfg(test)]
mod tests {
    // The engine's words for a stop, as the readers sort them.
    use super::*;

    #[test]
    fn only_a_guest_that_powered_off_stopped_cleanly() {
        // A power-off is clean; a timeout, worded as the engine words one,
        // is a timeout and not clean; a fault is neither.
        assert!(clean(POWERED_OFF));
        let hung = "timed out computing without exits for 2.2s, in user space at 0x41b33e";
        assert!(timed_out(hung) && !clean(hung));
        assert!(!timed_out("triple fault") && !clean("triple fault"));
    }
}
