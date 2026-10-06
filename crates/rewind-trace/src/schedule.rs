//! Where a schedule seed perturbs a run, and how: the reschedules, stalls
//! and late timers the monitor applies at the steps the seed picks. Each is
//! a function of the seed and the step alone, so the monitor, the engine
//! and the app all work it out the same way from a run's spec.

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

const NS_PER_US: u64 = 1_000;
pub const STALL_MIN_NS: u64 = 10_000;
pub const STALL_DOUBLINGS: u64 = 8;

/// Whether the exit that made a step armed the guest's timer, which only
/// the monitor sees as it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    Armed,
    NotArmed,
    /// Asked of a run's spec alone, with no machine to run.
    Unknown,
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

    /// What the seed does at `step`, in words: "a reschedule", "a 160 µs
    /// stall", "a timer 23.4 µs late", or several joined with "and". A
    /// late timer changes the run only if the exit that made the step armed
    /// one, which only the monitor sees: `timer` says whether it did. Not
    /// knowing, a late timer is named only for a step that carries nothing
    /// else.
    pub fn words_at(&self, step: u64, timer: Timer) -> String {
        let mut parts = Vec::new();
        if self.preempt_at(step) {
            parts.push("a reschedule".to_string());
        }
        if let Some(ns) = self.stall_at(step) {
            parts.push(format!("a {} µs stall", ns / NS_PER_US));
        }
        let slack = self.slack_at(step);
        let late = match timer {
            Timer::Armed => slack > 0,
            Timer::Unknown => parts.is_empty(),
            Timer::NotArmed => false,
        };
        if late {
            let us = slack as f64 / NS_PER_US as f64;
            parts.push(format!("a timer {us:.1} µs late"));
        }
        if parts.is_empty() {
            return "nothing".into();
        }
        parts.join(" and ")
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
    // Where a seed asks for reschedules, stalls and late timers, and how
    // far two schedules agree.
    use super::*;

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

    #[test]
    fn a_step_reads_as_what_the_seed_does_there() {
        // A reschedule or a stall where the seed asks for one, both where
        // it asks for both, and a late timer where the exit armed one. Not
        // knowing whether it did, a late timer is named only where nothing
        // else is, and where the exit armed none, nothing late is named.
        let s = Schedule {
            seed: 4,
            window: 0..u64::MAX,
            earlier: Vec::new(),
        };
        let find = |f: &dyn Fn(u64) -> bool| (0..100_000).find(|&step| f(step)).unwrap();
        let late = |step: u64| format!("a timer {:.1} µs late", s.slack_at(step) as f64 / 1e3);
        let reschedule =
            find(&|st| s.preempt_at(st) && s.stall_at(st).is_none() && s.slack_at(st) > 0);
        assert_eq!(s.words_at(reschedule, Timer::Unknown), "a reschedule");
        assert_eq!(s.words_at(reschedule, Timer::NotArmed), "a reschedule");
        assert_eq!(
            s.words_at(reschedule, Timer::Armed),
            format!("a reschedule and {}", late(reschedule))
        );
        let stall = find(&|st| !s.preempt_at(st) && s.stall_at(st).is_some());
        let us = s.stall_at(stall).unwrap() / NS_PER_US;
        assert_eq!(
            s.words_at(stall, Timer::NotArmed),
            format!("a {us} µs stall")
        );
        let both = find(&|st| s.preempt_at(st) && s.stall_at(st).is_some());
        let us = s.stall_at(both).unwrap() / NS_PER_US;
        assert_eq!(
            s.words_at(both, Timer::NotArmed),
            format!("a reschedule and a {us} µs stall")
        );
        let timer = find(&|st| !s.preempt_at(st) && s.stall_at(st).is_none() && s.slack_at(st) > 0);
        assert_eq!(s.words_at(timer, Timer::Armed), late(timer));
        assert_eq!(s.words_at(timer, Timer::Unknown), late(timer));
        assert_eq!(s.words_at(timer, Timer::NotArmed), "nothing");
    }
}
