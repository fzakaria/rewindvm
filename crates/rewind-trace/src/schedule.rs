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
/// STALL_MIN_NS and the schedule's [`StallMax`], each doubling of
/// STALL_MIN_NS as likely as the next. A busy machine deschedules a
/// process for stretches like these, and many races need one task held up
/// for a while rather than switched away from for an instant.
const STALL_ONE_IN: u64 = 128;

const NS_PER_US: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_S: u64 = 1_000_000_000;
pub const STALL_MIN_NS: u64 = 10_000;

/// The doublings of STALL_MIN_NS a stall picks from by default, so the
/// longest stall is STALL_MIN_NS << (STALL_DOUBLINGS - 1), 1.28 ms.
pub const STALL_DOUBLINGS: u64 = 8;

/// The longest stall a schedule asks for by default, and the shortest
/// maximum `--stall-max` takes.
pub const STALL_MAX_DEFAULT_NS: u64 = STALL_MIN_NS << (STALL_DOUBLINGS - 1);

/// The longest maximum `--stall-max` takes: 1 s, 18 doublings. The
/// monitor hands the guest a stall's length as a 32-bit count of
/// nanoseconds, which holds 4.29 s, and a task held off the CPU for
/// longer than a second looks to the program like a hang, not a busy
/// machine: its own deadlines and watchdogs fire first.
pub const STALL_MAX_CAP_NS: u64 = NS_PER_S;

/// The units a stall's length is written in, as `--stall-max` reads it
/// and prints it, largest first; a length prints in the first that
/// divides it, so "us" comes before "µs".
const UNITS: [(&str, u64); 5] = [
    ("s", NS_PER_S),
    ("ms", NS_PER_MS),
    ("us", NS_PER_US),
    ("µs", NS_PER_US),
    ("ns", 1),
];

/// The longest stall a schedule asks for, in nanoseconds: STALL_MAX_DEFAULT_NS
/// unless `--stall-max` asks for longer. A stall picks one of the fewest
/// doublings of STALL_MIN_NS that reach the maximum, each as likely as
/// the next, and the longest doubling is cut to the maximum. Which steps
/// stall does not depend on the maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct StallMax(u64);

/// Why a duration is no longest stall.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum StallMaxError {
    #[error("{0:?} is not a duration such as 50ms, 1.28ms or 1s")]
    Unreadable(String),
    #[error("a longest stall of {} is shorter than the default, {}", written(*.0), written(STALL_MAX_DEFAULT_NS))]
    BelowDefault(u64),
    #[error("a longest stall of {} is longer than the most rewind allows, {}", written(*.0), written(STALL_MAX_CAP_NS))]
    AboveCap(u64),
}

impl Default for StallMax {
    fn default() -> StallMax {
        StallMax(STALL_MAX_DEFAULT_NS)
    }
}

impl StallMax {
    /// A longest stall of `ns` nanoseconds, if it is between the default
    /// and the cap.
    pub fn new(ns: u64) -> Result<StallMax, StallMaxError> {
        if ns < STALL_MAX_DEFAULT_NS {
            return Err(StallMaxError::BelowDefault(ns));
        }
        if ns > STALL_MAX_CAP_NS {
            return Err(StallMaxError::AboveCap(ns));
        }
        Ok(StallMax(ns))
    }

    pub fn ns(self) -> u64 {
        self.0
    }

    /// Whether this is the maximum every schedule had before
    /// `--stall-max`, which a spec leaves out.
    pub fn is_default(&self) -> bool {
        *self == StallMax::default()
    }

    /// How many doublings of STALL_MIN_NS a stall picks from: the fewest
    /// whose longest, STALL_MIN_NS << (doublings - 1), reaches the
    /// maximum. STALL_DOUBLINGS for the default.
    pub fn doublings(self) -> u64 {
        let mut doublings = 1;
        while STALL_MIN_NS << (doublings - 1) < self.0 {
            doublings += 1;
        }
        doublings
    }
}

impl TryFrom<u64> for StallMax {
    type Error = StallMaxError;

    fn try_from(ns: u64) -> Result<StallMax, StallMaxError> {
        StallMax::new(ns)
    }
}

impl From<StallMax> for u64 {
    fn from(max: StallMax) -> u64 {
        max.0
    }
}

impl std::str::FromStr for StallMax {
    type Err = StallMaxError;

    /// A duration with its unit, as `--stall-max` takes it: a decimal
    /// number such as 50, 1.28 or 0.05, then s, ms, us, µs or ns, to a
    /// whole number of nanoseconds.
    fn from_str(text: &str) -> Result<StallMax, StallMaxError> {
        let unreadable = || StallMaxError::Unreadable(text.to_string());

        // The number runs up to the first character that is neither a
        // digit nor a point, and the unit is the rest.
        let split = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or_else(unreadable)?;
        let (number, unit) = text.split_at(split);
        let per_unit = UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, ns)| *ns)
            .ok_or_else(unreadable)?;

        // The whole part and the fraction, each digits only, at least one
        // of them given, and at most one point between them.
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        let digits_only = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() && fraction.is_empty() {
            return Err(unreadable());
        }
        if !digits_only(whole) || !digits_only(fraction) {
            return Err(unreadable());
        }

        // Whole units, then the fraction's nanoseconds, which must come
        // out whole: each digit is worth a tenth of the one before.
        const DECIMAL: u64 = 10;
        let whole_ns = if whole.is_empty() {
            0
        } else {
            whole
                .parse::<u64>()
                .ok()
                .and_then(|n| n.checked_mul(per_unit))
                .ok_or_else(unreadable)?
        };
        let mut fraction_ns = 0u64;
        let mut place = per_unit;
        for digit in fraction.bytes() {
            if !place.is_multiple_of(DECIMAL) {
                return Err(unreadable());
            }
            place /= DECIMAL;
            fraction_ns += u64::from(digit - b'0') * place;
        }
        let ns = whole_ns.checked_add(fraction_ns).ok_or_else(unreadable)?;
        StallMax::new(ns)
    }
}

impl std::fmt::Display for StallMax {
    /// The maximum as `--stall-max` takes it, in the largest unit it is a
    /// whole number of.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&written(self.0))
    }
}

/// `ns` as a whole number of the largest unit that divides it, as
/// `--stall-max` takes durations: "50ms", "1280us".
fn written(ns: u64) -> String {
    let (name, per_unit) = UNITS
        .iter()
        .find(|(_, per_unit)| ns.is_multiple_of(*per_unit))
        .expect("every length is a whole number of nanoseconds");
    format!("{}{name}", ns / per_unit)
}

/// A stall's length in words: in microseconds up to the default's
/// longest, as stalls always read, and in milliseconds to two places
/// beyond it, trailing zeros dropped: "160 µs", "2.56 ms", "50 ms".
pub fn stall_words(ns: u64) -> String {
    const HUNDREDTHS_PER_MS: u64 = 100;
    const TENTHS_PER_HUNDREDTH: u64 = 10;
    if ns <= STALL_MAX_DEFAULT_NS {
        return format!("{} µs", ns / NS_PER_US);
    }
    let hundredths = ns / (NS_PER_MS / HUNDREDTHS_PER_MS);
    let (ms, rest) = (
        hundredths / HUNDREDTHS_PER_MS,
        hundredths % HUNDREDTHS_PER_MS,
    );
    if rest == 0 {
        return format!("{ms} ms");
    }
    if rest.is_multiple_of(TENTHS_PER_HUNDREDTH) {
        return format!("{ms}.{} ms", rest / TENTHS_PER_HUNDREDTH);
    }
    format!("{ms}.{rest:02} ms")
}

/// A stall's length in words after the article it is read with: "a 160
/// µs", "an 80 µs". The number takes "an" when read aloud it starts with
/// eight, eleven or eighteen: a leading 8, or a leading 11 or 18 in a
/// number whose digits split into groups of three with two left over at
/// the front, as 11 and 18000 do.
pub fn a_stall(ns: u64) -> String {
    const DIGITS_PER_GROUP: usize = 3;
    const PAIR_AT_FRONT: usize = 2;
    let words = stall_words(ns);
    let whole = words
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    let an = whole.starts_with('8')
        || (whole.len() % DIGITS_PER_GROUP == PAIR_AT_FRONT
            && (whole.starts_with("11") || whole.starts_with("18")));
    let article = if an { "an" } else { "a" };
    format!("{article} {words}")
}

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
/// asked to reschedule, so another runnable thread may run from there, or
/// the running task is stalled, and timers armed in the window fire a
/// little late, so sleepers wake in a different order. Each is a function
/// of the seed and the step, and before the window a perturbed run is the
/// unperturbed one, exit for exit.
///
/// A fork of a fork keeps the perturbations of the runs it came from, each
/// over its own window, in `earlier`, so it is its parent exit for exit up
/// to its own window.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    /// 0 for the unperturbed schedule.
    pub seed: u64,
    /// The steps preemptions may happen at.
    pub window: std::ops::Range<u64>,
    /// The longest stall the seed asks for in the window.
    pub stall_max: StallMax,
    /// Perturbations inherited from earlier forks, each ending by the
    /// time the next one starts.
    pub earlier: Vec<Segment>,
    /// The steps where the guest's RNG takes in new bytes, from a fork
    /// given another seed than its parent's, in step order.
    pub reseeds: Vec<Reseed>,
}

/// A fork's new RNG seed and the step it takes effect at: the guest mixes
/// bytes derived from the two into its RNG there and rekeys it, so the
/// fork draws other random numbers from then on.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reseed {
    pub step: u64,
    pub seed: u64,
}

/// The bytes a reseed mixes in: 32, as the guest's shared page holds them.
pub const RESEED_BYTES: usize = 32;

/// One seed over one window of steps, with the longest stall it asks for
/// there.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub seed: u64,
    pub window: std::ops::Range<u64>,
    pub stall_max: StallMax,
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
        self.perturbing(step).map(|(seed, _)| seed)
    }

    /// The seed that perturbs `step` and the longest stall it asks for
    /// there, if any seed does: the run's own window's, or else an
    /// inherited one's.
    fn perturbing(&self, step: u64) -> Option<(u64, StallMax)> {
        let own = Segment {
            seed: self.seed,
            window: self.window.clone(),
            stall_max: self.stall_max,
        };
        std::iter::once(&own)
            .chain(&self.earlier)
            .find(|s| s.seed != 0 && s.window.contains(&step))
            .map(|s| (s.seed, s.stall_max))
    }

    /// The last step through which this schedule and `other` perturb every
    /// step alike, or u64::MAX when they always do. Which seed perturbs a
    /// step, and how long its stalls may be, changes only where a window
    /// starts or ends, so comparing the two there finds the first step
    /// they differ at. One seed under two maxima stalls at the same steps
    /// for other lengths, so the two are taken to part where the window
    /// starts.
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
        let perturbed = edges
            .into_iter()
            .find(|step| self.perturbing(*step) != other.perturbing(*step));

        // The two runs part at a reseed only one of them makes, or that
        // they make with different seeds, as they do where a window
        // starts: the step's own interrupt is the first thing that
        // differs.
        let reseeded = self
            .reseeds
            .iter()
            .chain(&other.reseeds)
            .map(|r| r.step)
            .filter(|step| self.reseed_at(*step) != other.reseed_at(*step))
            .min();
        [perturbed, reseeded]
            .into_iter()
            .flatten()
            .min()
            .map_or(u64::MAX, |step| step.saturating_sub(1))
    }

    /// The seed the guest's RNG takes in after the exit that made `step`,
    /// if any.
    pub fn reseed_at(&self, step: u64) -> Option<u64> {
        self.reseeds.iter().find(|r| r.step == step).map(|r| r.seed)
    }

    /// The bytes a reseed with `seed` at `step` mixes in: a fixed,
    /// well-mixed function of the two.
    pub fn reseed_bytes(seed: u64, step: u64) -> [u8; RESEED_BYTES] {
        let mut bytes = [0u8; RESEED_BYTES];
        let mut state = mix(seed ^ 0x2e5e_ed00) ^ step;
        for chunk in bytes.chunks_mut(8) {
            state = mix(state);
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        bytes
    }

    /// How long to stall the task running after the exit that made
    /// `step`, if at all.
    pub fn stall_at(&self, step: u64) -> Option<u64> {
        let (seed, max) = self.perturbing(step)?;
        let h = mix(mix(seed ^ 0x57a1) ^ step);
        if !h.is_multiple_of(STALL_ONE_IN) {
            return None;
        }

        // One of the maximum's doublings, each as likely as the next, the
        // longest cut to the maximum. Under the default maximum the
        // longest doubling is the maximum, so nothing is cut.
        let doubling = (h / STALL_ONE_IN) % max.doublings();
        Some((STALL_MIN_NS << doubling).min(max.ns()))
    }

    /// What the seed does at `step`, in words: "a request to switch
    /// threads" for a reschedule, "a 160 µs pause of the running thread"
    /// for a stall, "a timer firing 23.4 µs late", or several joined with
    /// "and". A late timer changes the run only if the exit that made the step armed
    /// one, which only the monitor sees: `timer` says whether it did. Not
    /// knowing, a late timer is named only for a step that carries nothing
    /// else.
    pub fn words_at(&self, step: u64, timer: Timer) -> String {
        let mut parts = Vec::new();
        if self.preempt_at(step) {
            parts.push("a request to switch threads".to_string());
        }
        if let Some(ns) = self.stall_at(step) {
            parts.push(format!("{} pause of the running thread", a_stall(ns)));
        }
        let slack = self.slack_at(step);
        let late = match timer {
            Timer::Armed => slack > 0,
            Timer::Unknown => parts.is_empty(),
            Timer::NotArmed => false,
        };
        if late {
            let us = slack as f64 / NS_PER_US as f64;
            parts.push(format!("a timer firing {us:.1} µs late"));
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
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
        };
        let perturbed = Schedule {
            seed: 3,
            window: 100..200,
            earlier: Vec::new(),
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
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
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
        };
        let perturbed = Schedule {
            seed: 3,
            window: 1000..100_000,
            earlier: Vec::new(),
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
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

    #[test]
    fn default_stalls_are_pinned() {
        // The stalls every run made without --stall-max gets, pinned by
        // value: the first six stalls of four seeds, and the count and a
        // running digest of every stall over a million steps. A change to
        // how a stall is picked under the default maximum fails here,
        // since recorded perturbed runs would replay another way.
        const STEPS: u64 = 1_000_000;
        const DIGEST_FACTOR: u64 = 31;
        // A seed, its first stalls as (step, ns), how many it makes in
        // all, and their digest.
        type Pinned = (u64, [(u64, u64); 6], usize, u64);
        let pinned: [Pinned; 4] = [
            (
                1,
                [
                    (175, 640_000),
                    (205, 320_000),
                    (435, 80_000),
                    (895, 320_000),
                    (1370, 10_000),
                    (1634, 80_000),
                ],
                7882,
                7_434_163_062_781_547_824,
            ),
            (
                3,
                [
                    (72, 640_000),
                    (109, 1_280_000),
                    (118, 20_000),
                    (284, 20_000),
                    (542, 10_000),
                    (615, 160_000),
                ],
                7723,
                10_929_767_047_105_955_088,
            ),
            (
                7,
                [
                    (78, 80_000),
                    (104, 1_280_000),
                    (132, 20_000),
                    (194, 10_000),
                    (217, 80_000),
                    (221, 20_000),
                ],
                7826,
                16_477_472_444_425_075_424,
            ),
            (
                0xdead_beef,
                [
                    (273, 10_000),
                    (454, 640_000),
                    (482, 80_000),
                    (553, 640_000),
                    (573, 20_000),
                    (744, 20_000),
                ],
                7790,
                7_578_925_441_008_711_008,
            ),
        ];
        for (seed, first, count, digest) in pinned {
            let s = Schedule {
                seed,
                window: 0..u64::MAX,
                ..Schedule::default()
            };
            let stalls: Vec<(u64, u64)> = (0..STEPS)
                .filter_map(|step| s.stall_at(step).map(|ns| (step, ns)))
                .collect();
            assert_eq!(stalls[..first.len()], first, "seed {seed}");
            assert_eq!(stalls.len(), count, "seed {seed}");
            let folded = stalls.iter().fold(0u64, |acc, (_, ns)| {
                acc.wrapping_mul(DIGEST_FACTOR).wrapping_add(*ns)
            });
            assert_eq!(folded, digest, "seed {seed}");
        }
    }

    /// A schedule perturbing every step with `seed`, its stalls as long as
    /// `stall_max` allows.
    fn everywhere(seed: u64, stall_max: StallMax) -> Schedule {
        Schedule {
            seed,
            window: 0..u64::MAX,
            stall_max,
            ..Schedule::default()
        }
    }

    #[test]
    fn a_stall_max_sets_the_doublings() {
        // Durations parse with their units, and each maximum gets the
        // fewest doublings of the shortest stall that reach it: the
        // default 1.28 ms its 8, 50 ms 14 (81.92 ms), 100 ms 15
        // (163.84 ms) and the cap of 1 s 18 (1.31 s). Below the default,
        // above the cap, and anything without a unit is refused.
        let parsed = |text: &str| text.parse::<StallMax>();
        assert_eq!(parsed("1.28ms").unwrap(), StallMax::default());
        assert_eq!(parsed("1280us").unwrap(), StallMax::default());
        assert_eq!(parsed("1280µs").unwrap(), StallMax::default());
        assert_eq!(StallMax::default().doublings(), STALL_DOUBLINGS);
        let doublings = |text: &str| parsed(text).unwrap().doublings();
        assert_eq!(doublings("1.3ms"), 9);
        assert_eq!(doublings("2.56ms"), 9);
        assert_eq!(doublings("50ms"), 14);
        assert_eq!(doublings("81.92ms"), 14);
        assert_eq!(doublings("100ms"), 15);
        assert_eq!(doublings("1s"), 18);
        assert_eq!(parsed("50ms").unwrap().ns(), 50_000_000);
        assert_eq!(parsed("0.05s").unwrap().ns(), 50_000_000);
        assert_eq!(parsed("50000000ns").unwrap().ns(), 50_000_000);
        for refused in [
            "1ms",
            "1279us",
            "1.5s",
            "2s",
            "50",
            "ms",
            "fast",
            "-5ms",
            "1.2.3ms",
            "1.0000000001s",
            "99999999999999999999s",
            "",
        ] {
            assert!(parsed(refused).is_err(), "{refused:?} parsed");
        }
        assert!(StallMax::new(STALL_MAX_CAP_NS).is_ok());
        assert!(StallMax::new(STALL_MAX_CAP_NS + 1).is_err());

        // A maximum prints as the flag that gives it back.
        for text in ["50ms", "1280us", "1s", "2500us", "12345678ns"] {
            assert_eq!(parsed(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn long_stalls_stay_under_the_max_and_reach_it() {
        // Under a 50 ms maximum the steps that stall are the default's,
        // and only how long each lasts differs. Every stall is between the
        // shortest and the maximum, the longest doubling is cut to the
        // maximum, and over two million steps both ends turn up, with each
        // of the 14 doublings about as often as the next.
        const STEPS: u64 = 2_000_000;
        let max = "50ms".parse::<StallMax>().unwrap();
        let long = everywhere(3, max);
        let short = everywhere(3, StallMax::default());
        let stalls: Vec<u64> = (0..STEPS).filter_map(|step| long.stall_at(step)).collect();
        assert!(
            (0..STEPS).all(|step| long.stall_at(step).is_some() == short.stall_at(step).is_some())
        );
        assert!(
            stalls
                .iter()
                .all(|ns| (STALL_MIN_NS..=max.ns()).contains(ns))
        );
        assert!(stalls.contains(&STALL_MIN_NS) && stalls.contains(&max.ns()));
        let per_doubling = stalls.len() as u64 / max.doublings();
        for doubling in 0..max.doublings() {
            let ns = (STALL_MIN_NS << doubling).min(max.ns());
            let seen = stalls.iter().filter(|s| **s == ns).count() as u64;
            assert!(
                seen > per_doubling * 3 / 4 && seen < per_doubling * 5 / 4,
                "{ns}: {seen}"
            );
        }
    }

    #[test]
    fn schedules_with_other_maxima_part_where_their_windows_start() {
        // A seed under a longer maximum stalls at the same steps for other
        // lengths, so two runs of one seed that differ only in the maximum
        // are taken to part where the window starts, and a fork of a fork
        // keeps each segment's own maximum.
        let max = "50ms".parse::<StallMax>().unwrap();
        let default = Schedule {
            seed: 3,
            window: 1000..u64::MAX,
            ..Schedule::default()
        };
        let long = Schedule {
            stall_max: max,
            ..default.clone()
        };
        assert_eq!(default.same_through(&long), 999);
        let forked = Schedule {
            seed: 5,
            window: 3000..u64::MAX,
            earlier: vec![Segment {
                seed: 3,
                window: 1000..3000,
                stall_max: max,
            }],
            ..Schedule::default()
        };
        assert_eq!(long.same_through(&forked), 2999);
        assert!((1000..3000).all(|step| forked.stall_at(step) == long.stall_at(step)));
    }

    #[test]
    fn a_stall_takes_the_article_its_number_is_read_with() {
        // "an" before the numbers read aloud starting with eight, eleven
        // or eighteen, and "a" before the rest, for stalls in
        // microseconds and in milliseconds.
        assert_eq!(a_stall(80_000), "an 80 µs");
        assert_eq!(a_stall(8_000_000), "an 8 ms");
        assert_eq!(a_stall(11_000_000), "an 11 ms");
        assert_eq!(a_stall(18_500_000), "an 18.5 ms");
        assert_eq!(a_stall(800_000_000), "an 800 ms");
        assert_eq!(a_stall(10_000), "a 10 µs");
        assert_eq!(a_stall(160_000), "a 160 µs");
        assert_eq!(a_stall(1_280_000), "a 1280 µs");
        assert_eq!(a_stall(110_000_000), "a 110 ms");
        assert_eq!(a_stall(11_000_000_000), "an 11000 ms");
    }

    #[test]
    fn a_long_stall_reads_in_milliseconds() {
        // Stalls up to the default's longest read in microseconds, as
        // they always have; longer ones in milliseconds, to two places
        // with trailing zeros dropped.
        assert_eq!(stall_words(10_000), "10 µs");
        assert_eq!(stall_words(1_280_000), "1280 µs");
        assert_eq!(stall_words(1_300_000), "1.3 ms");
        assert_eq!(stall_words(2_560_000), "2.56 ms");
        assert_eq!(stall_words(10_240_000), "10.24 ms");
        assert_eq!(stall_words(50_000_000), "50 ms");
        assert_eq!(stall_words(1_000_000_000), "1000 ms");
        let s = everywhere(4, "50ms".parse().unwrap());
        let step = (0..1_000_000)
            .find(|&st| !s.preempt_at(st) && s.stall_at(st) == Some(50_000_000))
            .unwrap();
        assert_eq!(
            s.words_at(step, Timer::NotArmed),
            "a 50 ms pause of the running thread"
        );
    }

    fn preemptions(seed: u64, window: std::ops::Range<u64>) -> Vec<u64> {
        let s = Schedule {
            seed,
            window,
            earlier: Vec::new(),
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
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
                stall_max: StallMax::default(),
            }],
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
        }
    }

    #[test]
    fn a_reseed_parts_two_runs_after_its_step() {
        // A fork reseeded at 4000 is its parent through step 3999: the
        // interrupt after the exit that made step 4000 carries the bytes.
        // A reseed both make alike parts nothing, and one with another
        // seed parts them the same way.
        let parent = fork_of_fork();
        let child = Schedule {
            reseeds: vec![Reseed {
                step: 4000,
                seed: 2,
            }],
            ..parent.clone()
        };
        assert_eq!(child.reseed_at(4000), Some(2));
        assert_eq!(child.reseed_at(4001), None);
        assert_eq!(parent.same_through(&child), 3999);
        assert_eq!(child.same_through(&child.clone()), u64::MAX);
        let other = Schedule {
            reseeds: vec![Reseed {
                step: 4000,
                seed: 3,
            }],
            ..parent.clone()
        };
        assert_eq!(child.same_through(&other), 3999);
        assert_ne!(
            Schedule::reseed_bytes(2, 4000),
            Schedule::reseed_bytes(3, 4000)
        );
        assert_ne!(
            Schedule::reseed_bytes(2, 4000),
            Schedule::reseed_bytes(2, 4001)
        );
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
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
        };
        let none = Schedule {
            seed: 0,
            window: 0..u64::MAX,
            earlier: Vec::new(),
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
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
            reseeds: Vec::new(),
            stall_max: StallMax::default(),
        };
        let find = |f: &dyn Fn(u64) -> bool| (0..100_000).find(|&step| f(step)).unwrap();
        let late = |step: u64| {
            format!(
                "a timer firing {:.1} µs late",
                s.slack_at(step) as f64 / 1e3
            )
        };
        let reschedule =
            find(&|st| s.preempt_at(st) && s.stall_at(st).is_none() && s.slack_at(st) > 0);
        assert_eq!(
            s.words_at(reschedule, Timer::Unknown),
            "a request to switch threads"
        );
        assert_eq!(
            s.words_at(reschedule, Timer::NotArmed),
            "a request to switch threads"
        );
        assert_eq!(
            s.words_at(reschedule, Timer::Armed),
            format!("a request to switch threads and {}", late(reschedule))
        );
        let stall = find(&|st| !s.preempt_at(st) && s.stall_at(st).is_some());
        let pause = a_stall(s.stall_at(stall).unwrap());
        assert_eq!(
            s.words_at(stall, Timer::NotArmed),
            format!("{pause} pause of the running thread")
        );
        let both = find(&|st| s.preempt_at(st) && s.stall_at(st).is_some());
        let pause = a_stall(s.stall_at(both).unwrap());
        assert_eq!(
            s.words_at(both, Timer::NotArmed),
            format!("a request to switch threads and {pause} pause of the running thread")
        );
        let timer = find(&|st| !s.preempt_at(st) && s.stall_at(st).is_none() && s.slack_at(st) > 0);
        assert_eq!(s.words_at(timer, Timer::Armed), late(timer));
        assert_eq!(s.words_at(timer, Timer::Unknown), late(timer));
        assert_eq!(s.words_at(timer, Timer::NotArmed), "nothing");
    }
}
