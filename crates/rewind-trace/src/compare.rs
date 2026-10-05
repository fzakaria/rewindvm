//! Where two runs first behave differently, as `rewind check` and the
//! desktop app show it.
//!
//! When the failing run names a culprit program (the one that crashed or
//! failed first, and did not in the other run), only that program's own
//! events are compared, with threads numbered by the order they appear, as
//! [`Trace::divergence_in`] does. Otherwise, or when the program behaved
//! the same in both, every event is compared by what happened and in which
//! process and thread. Either way steps are ignored: a difference that is
//! only a shift in when things happened is not one. [`Trace::divergence`]
//! is the record-for-record comparison, for replays and `rewind diff`.

use crate::{Event, Trace};

/// How one run compares with another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The program compared, when the comparison is about one.
    pub program: Option<Vec<String>>,
    /// Where the runs part; None when they did the same things.
    pub point: Option<DivergencePoint>,
}

/// One run's side of a divergence: the first event that differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Side {
    /// The index of the event in the run's trace.
    pub index: usize,
    /// The event's thread number within the program (0 for its first
    /// thread), when the comparison is about one program.
    pub thread: Option<usize>,
}

/// Where two runs part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DivergencePoint {
    /// How many events matched before.
    pub matched: usize,
    /// The first differing event on this run's side and on the other's;
    /// None on a side whose events ran out first.
    pub here: Option<Side>,
    pub there: Option<Side>,
    /// The step the difference is at in this run: its differing event,
    /// or the last matching one when this run's events ran out first.
    pub step: u64,
    /// The same in the other run.
    pub other_step: u64,
}

impl Comparison {
    /// How `this` run, which ended at step `this_end`, compares with
    /// `other`, which ended at `other_end`.
    pub fn of(this: &Trace, this_end: u64, other: &Trace, other_end: u64) -> Comparison {
        // The culprit's own events first.
        let program = this
            .culprit_against(other)
            .or_else(|| other.culprit_against(this));
        if let Some(argv) = &program
            && let Some(d) = this.divergence_in(other, argv)
        {
            let side = |event: Option<(usize, usize)>| {
                event.map(|(index, thread)| Side {
                    index,
                    thread: Some(thread),
                })
            };
            let (here, there) = (side(d.left_event()), side(d.right_event()));
            let last_same = |indices: &[usize]| d.position.checked_sub(1).map(|i| indices[i]);
            let point = DivergencePoint {
                matched: d.position,
                here,
                there,
                step: step_of(
                    this,
                    here.map(|s| s.index).or(last_same(&d.left.indices)),
                    this_end,
                ),
                other_step: step_of(
                    other,
                    there.map(|s| s.index).or(last_same(&d.right.indices)),
                    other_end,
                ),
            };
            return Comparison {
                program,
                point: Some(point),
            };
        }

        // Every event, by what happened and where, but not when.
        let same = |x: &Event, y: &Event| x.pid == y.pid && x.tid == y.tid && x.kind == y.kind;
        let (a, b) = (&this.events, &other.events);
        let n = a.len().min(b.len());
        let position = (0..n)
            .find(|&i| !same(&a[i], &b[i]))
            .or((a.len() != b.len()).then_some(n));
        let point = position.map(|i| {
            let side = |events: &[Event]| {
                (i < events.len()).then_some(Side {
                    index: i,
                    thread: None,
                })
            };
            let (here, there) = (side(a), side(b));
            let at = |side: Option<Side>| side.map(|s| s.index).or(i.checked_sub(1));
            DivergencePoint {
                matched: i,
                here,
                there,
                step: step_of(this, at(here), this_end),
                other_step: step_of(other, at(there), other_end),
            }
        });
        Comparison {
            program: None,
            point,
        }
    }

    /// The step of the first divergence, in this run.
    pub fn step(&self) -> Option<u64> {
        self.point.as_ref().map(|p| p.step)
    }
}

/// The step of event `index`, or the end of the run without one.
fn step_of(trace: &Trace, index: Option<usize>, end: u64) -> u64 {
    index
        .and_then(|i| trace.events.get(i))
        .map_or(end, |e| e.step)
}

#[cfg(test)]
mod tests {
    // Two small runs of one program compared: the same events at other
    // steps, and a write that differs.
    use super::*;
    use crate::EventKind;

    fn write(step: u64, tid: u32, text: &str) -> Event {
        Event {
            step,
            pid: 4,
            tid,
            kind: EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        }
    }

    #[test]
    fn the_same_events_later_are_no_divergence_and_another_write_is() {
        // b does what a did two steps later; c writes other text second.
        let a = Trace {
            events: vec![write(3, 4, "one\n"), write(5, 5, "two\n")],
        };
        let b = Trace {
            events: vec![write(5, 4, "one\n"), write(7, 5, "two\n")],
        };
        let c = Trace {
            events: vec![write(3, 4, "one\n"), write(6, 5, "three\n")],
        };
        assert_eq!(Comparison::of(&a, 5, &b, 7).point, None);
        assert!(a.divergence(&b).is_some());
        let parted = Comparison::of(&a, 5, &c, 6).point.unwrap();
        assert_eq!((parted.matched, parted.step, parted.other_step), (1, 5, 6));
    }
}
