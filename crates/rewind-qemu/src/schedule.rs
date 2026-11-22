//! A run's schedule as the QEMU machine reads it: the steps where it
//! perturbs the guest, worked out here by rewind-trace's own
//! [`Schedule`], so the machine computes none of it.
//!
//! The file starts with [`MAGIC`] and holds one entry for each step the
//! schedule touches, in step order, all little endian: u64 step, u64 the
//! slack a timer armed at the step gets, u32 the reasons the step's
//! interrupt carries (rewind-vmm's PENDING_PREEMPT, PENDING_STALL and
//! PENDING_RESEED), u32 the stall's length in nanoseconds, then the 32
//! bytes a reseed mixes in.

use rewind_trace::schedule::{RESEED_BYTES, Schedule};

/// The first bytes of a schedule file.
pub(crate) const MAGIC: &[u8; 8] = b"rwqsch1\0";

/// rewind-vmm's interrupt reasons (pv.rs), as the guest reads them.
pub(crate) const PENDING_PREEMPT: u32 = 1 << 1;
pub(crate) const PENDING_STALL: u32 = 1 << 4;
pub(crate) const PENDING_RESEED: u32 = 1 << 5;

/// The schedule file for steps 1 to `last` of a run on `schedule`.
pub(crate) fn encode(schedule: &Schedule, last: u64) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for step in 1..=last {
        let slack = schedule.slack_at(step);
        let mut reasons = 0;
        if schedule.preempt_at(step) {
            reasons |= PENDING_PREEMPT;
        }
        let stall = schedule.stall_at(step);
        if stall.is_some() {
            reasons |= PENDING_STALL;
        }
        let reseed = schedule.reseed_at(step);
        if reseed.is_some() {
            reasons |= PENDING_RESEED;
        }
        if slack == 0 && reasons == 0 {
            continue;
        }
        out.extend_from_slice(&step.to_le_bytes());
        out.extend_from_slice(&slack.to_le_bytes());
        out.extend_from_slice(&reasons.to_le_bytes());
        out.extend_from_slice(&(stall.unwrap_or(0) as u32).to_le_bytes());
        let bytes = reseed.map_or([0; RESEED_BYTES], |seed| Schedule::reseed_bytes(seed, step));
        out.extend_from_slice(&bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    // The schedule file against rewind-trace's Schedule asked step by
    // step, for a perturbed window and for the unperturbed schedule.
    use super::*;
    use rewind_trace::schedule::Reseed;

    /// The length of one entry: step, slack, reasons, stall and reseed.
    const ENTRY: usize = 8 + 8 + 4 + 4 + RESEED_BYTES;

    fn schedule(seed: u64, window: std::ops::Range<u64>) -> Schedule {
        Schedule {
            seed,
            window,
            stall_max: Default::default(),
            earlier: Vec::new(),
            reseeds: Vec::new(),
        }
    }

    /// The unperturbed schedule touches no step, so its file is the
    /// magic alone.
    #[test]
    fn the_unperturbed_schedule_is_empty() {
        assert_eq!(encode(&schedule(0, 0..u64::MAX), 10_000), MAGIC.to_vec());
    }

    /// Each entry of a perturbed window says what the Schedule says of
    /// its step, and a step the file leaves out is one the Schedule
    /// leaves alone.
    #[test]
    fn entries_say_what_the_schedule_says() {
        const LAST: u64 = 5000;
        let mut s = schedule(7, 1000..4000);
        s.reseeds.push(Reseed {
            step: 2500,
            seed: 3,
        });
        let file = encode(&s, LAST);
        let entries = &file[MAGIC.len()..];
        assert_eq!(entries.len() % ENTRY, 0);

        let mut listed = std::collections::BTreeSet::new();
        for e in entries.chunks(ENTRY) {
            let step = u64::from_le_bytes(e[0..8].try_into().unwrap());
            let slack = u64::from_le_bytes(e[8..16].try_into().unwrap());
            let reasons = u32::from_le_bytes(e[16..20].try_into().unwrap());
            let stall = u32::from_le_bytes(e[20..24].try_into().unwrap());
            assert_eq!(slack, s.slack_at(step));
            assert_eq!(reasons & PENDING_PREEMPT != 0, s.preempt_at(step));
            assert_eq!(stall as u64, s.stall_at(step).unwrap_or(0));
            if step == 2500 {
                assert_eq!(e[24..], Schedule::reseed_bytes(3, 2500));
            }
            listed.insert(step);
        }
        assert!(listed.contains(&2500));
        for step in (1..=LAST).filter(|s| !listed.contains(s)) {
            assert_eq!(s.slack_at(step), 0);
            assert!(!s.preempt_at(step));
            assert!(s.stall_at(step).is_none());
        }
    }
}
