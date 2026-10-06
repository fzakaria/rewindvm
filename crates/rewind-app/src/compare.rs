//! The Compare tab's rows: the two runs' events from shortly before they
//! part, side by side, each pair with the span where its texts differ.
//!
//! The events compared are the ones the divergence card compares: the
//! program the comparison is about, else every event. Up to the point the
//! runs part they are alike, so a few of those are one row each, both
//! runs' steps beside the one text; after it, each run's next events are
//! paired by their order.

use std::ops::Range;

use rewind_trace::compare::Comparison;
use rewind_trace::{Event, Trace};

use crate::describe::{Length, describe_as, short_store_paths};

/// How many of the events both runs share the tab shows before they part.
pub const SHARED_SHOWN: usize = 3;

/// How many of each run's events the tab shows after they part.
pub const APART_SHOWN: usize = 12;

/// One run's event in a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The event's index in its run's trace.
    pub index: usize,
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    /// The event in full, as the scrubber writes it.
    pub text: String,
    /// The bytes of `text` that differ from the other run's event in the
    /// same row, when they differ.
    pub differs: Option<Range<usize>>,
}

/// A row of the Compare tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// An event both runs had, before they part: this run's, and the
    /// other's at its own step.
    Shared(Cell, Cell),
    /// This run's and the other run's events in the same place after they
    /// part; a run whose events ran out has none.
    Apart(Option<Cell>, Option<Cell>),
}

/// The rows comparing `this` run with `other`, as `comparison` found
/// them: the last few events they share, then each run's next events.
/// Runs that did the same things show their last few events, shared.
pub fn rows(this: &Trace, other: &Trace, comparison: &Comparison) -> Vec<Row> {
    let (mine, theirs) = compared(this, other, comparison);
    let parted = comparison
        .point
        .as_ref()
        .map_or(mine.len().min(theirs.len()), |p| p.matched);
    // Shared events are context, cut to a line as the scrubber cuts them;
    // the events after the runs part are whole, since what differs can be
    // anywhere in them.
    let cell = |trace: &Trace, index: usize, length: Length| {
        let event = &trace.events[index];
        let text = match length {
            Length::Short => short_store_paths(&describe_as(event, length).text),
            Length::Whole => describe_as(event, length).text,
        };
        Cell {
            index,
            step: event.step,
            pid: event.pid,
            tid: event.tid,
            text,
            differs: None,
        }
    };

    let shared = |k: usize| {
        Row::Shared(
            cell(this, mine[k], Length::Short),
            cell(other, theirs[k], Length::Short),
        )
    };
    let mut rows: Vec<Row> = (parted.saturating_sub(SHARED_SHOWN)..parted)
        .map(shared)
        .collect();
    if comparison.point.is_none() {
        return rows;
    }
    for k in parted..parted + APART_SHOWN {
        let mut here = mine.get(k).map(|&i| cell(this, i, Length::Whole));
        let mut there = theirs.get(k).map(|&i| cell(other, i, Length::Whole));
        if here.is_none() && there.is_none() {
            break;
        }
        if let (Some(h), Some(t)) = (&mut here, &mut there)
            && let Some((a, b)) = differing(&h.text, &t.text)
        {
            h.differs = Some(a);
            t.differs = Some(b);
        }
        rows.push(Row::Apart(here, there));
    }
    rows
}

/// The step of `other` that matches `step` of `this`, by the events the
/// comparison pairs: the step of the other run's event paired with this
/// run's last event at or before `step`, among those they share; past the
/// point they part, the other run's step there. None when neither run has
/// a compared event at or before it.
pub fn matching_step(
    this: &Trace,
    other: &Trace,
    comparison: &Comparison,
    step: u64,
) -> Option<u64> {
    if let Some(point) = &comparison.point
        && step >= point.step
    {
        return Some(point.other_step);
    }
    let (mine, theirs) = compared(this, other, comparison);
    let shared = comparison
        .point
        .as_ref()
        .map_or(mine.len().min(theirs.len()), |p| p.matched);
    let k = mine[..shared].partition_point(|&i| this.events[i].step <= step);
    let paired = k.checked_sub(1)?;
    Some(other.events[theirs[paired]].step)
}

/// The indices of the events the comparison compares in each run: the
/// compared program's, else every event.
fn compared(this: &Trace, other: &Trace, comparison: &Comparison) -> (Vec<usize>, Vec<usize>) {
    match &comparison.program {
        Some(argv) => (
            this.program_events(argv).indices,
            other.program_events(argv).indices,
        ),
        None => (
            (0..this.events.len()).collect(),
            (0..other.events.len()).collect(),
        ),
    }
}

/// Where two texts differ, as a byte range in each: what is left of each
/// once the start and the end they share are taken off. None for equal
/// texts.
pub fn differing(a: &str, b: &str) -> Option<(Range<usize>, Range<usize>)> {
    if a == b {
        return None;
    }
    let start: usize = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    let (rest_a, rest_b) = (&a[start..], &b[start..]);
    let end: usize = rest_a
        .chars()
        .rev()
        .zip(rest_b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    Some((start..a.len() - end, start..b.len() - end))
}

/// An event as the tab shows it, for code that has one but not its row.
pub fn text(event: &Event) -> String {
    describe_as(event, Length::Whole).text
}

#[cfg(test)]
mod tests {
    // Two small runs of one program, written by hand: the rows around
    // where they part, the span of two texts that differs, and the step
    // of one run that matches a step of the other.
    use super::*;
    use rewind_trace::EventKind;

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

    /// Five writes; the other run's are three steps later, and from the
    /// fourth on it writes other text.
    fn runs() -> (Trace, Trace) {
        let this = Trace {
            events: ["a", "b", "c", "d", "e"]
                .iter()
                .enumerate()
                .map(|(i, t)| write(10 * (i as u64 + 1), 4, &format!("job {t} done\n")))
                .collect(),
        };
        let other = Trace {
            events: ["a", "b", "c", "x", "y", "z"]
                .iter()
                .enumerate()
                .map(|(i, t)| write(10 * (i as u64 + 1) + 3, 4, &format!("job {t} done\n")))
                .collect(),
        };
        (this, other)
    }

    #[test]
    fn rows_show_the_shared_events_then_each_run_s_own() {
        // Three shared rows, each with both runs' steps; then the writes
        // that differ paired by their order, the letter that differs
        // marked in each, and the other run's last write alone.
        let (this, other) = runs();
        let comparison = Comparison::of(&this, 60, &other, 70);
        let rows = rows(&this, &other, &comparison);
        assert_eq!(rows.len(), 3 + 3);
        let Row::Shared(here, there) = &rows[0] else {
            panic!("a shared row first: {rows:?}");
        };
        assert_eq!((here.step, there.step), (10, 13));
        let Row::Apart(Some(here), Some(there)) = &rows[3] else {
            panic!("the first row apart has both: {rows:?}");
        };
        assert_eq!(here.text, r#"write(1, "job d done\n")"#);
        assert_eq!(&here.text[here.differs.clone().unwrap()], "d");
        assert_eq!(&there.text[there.differs.clone().unwrap()], "x");
        assert!(matches!(&rows[5], Row::Apart(None, Some(t)) if t.step == 63));
    }

    #[test]
    fn the_differing_span_is_what_the_ends_do_not_share() {
        // A store hash in the middle, a longer text, equal texts, and
        // characters wider than a byte.
        let (a, b) =
            differing("/nix/store/1a2b-nix/bin/nix", "/nix/store/9z2b-nix/bin/nix").unwrap();
        assert_eq!((a, b), (11..13, 11..13));
        let (a, b) = differing("job 1", "job 10").unwrap();
        assert_eq!((a, b), (5..5, 5..6));
        assert_eq!(differing("same", "same"), None);
        let (a, b) = differing("é1é", "é2é").unwrap();
        assert_eq!((a, b), (2..3, 2..3));
    }

    #[test]
    fn a_step_matches_the_other_run_s_by_the_events_they_share() {
        // Before they part, a step goes to the other run's step of the
        // last shared event at or before it; at or past the point they
        // part, to the other run's step there; before any event, nowhere.
        let (this, other) = runs();
        let comparison = Comparison::of(&this, 60, &other, 70);
        assert_eq!(matching_step(&this, &other, &comparison, 25), Some(23));
        assert_eq!(matching_step(&this, &other, &comparison, 30), Some(33));
        assert_eq!(matching_step(&this, &other, &comparison, 45), Some(43));
        assert_eq!(matching_step(&this, &other, &comparison, 55), Some(43));
        assert_eq!(matching_step(&this, &other, &comparison, 5), None);
    }
}
