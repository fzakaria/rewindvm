//! What a view says while the engine brings a run back to a step. The
//! engine replays the run from the nearest keyframe at
//! or before the step, and says how far it got (see
//! [`rewind_trace::replaying`]). A short replay gets the view's own short
//! sentence. A long one, whose recent speed says it has more than
//! LONG_WAIT left, gets a line and a bar for its run: where the replay started
//! and goes, where it is, and how long the rest takes.

use std::time::Duration;

use rewind_trace::replaying::{Recent, Replaying, about};

use crate::describe::thousands;

/// A replay with more than this left, at its recent speed, is shown with
/// where it is and a bar; a shorter one is not worth more than the view's
/// short sentence.
const LONG_WAIT: Duration = Duration::from_secs(3);

/// What the view says under its rows while a long replay runs.
pub const QUICKER_NEXT_TIME: &str =
    "Rewind saves keyframes on the way, so looking near here again is quicker.";

/// The replays a view waits on, as far as the engine has said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Waiting {
    /// Each run's replay, in the order the runs were first heard of.
    replays: Vec<Heard>,
}

/// What the engine has said of one run's replay.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Heard {
    /// Its latest report.
    latest: Replaying,
    /// Its reports over the last while, for its recent speed.
    recent: Recent,
    /// Whether it has been shown as long.
    length: Length,
}

impl Heard {
    /// The time the rest of the replay takes at its recent speed.
    fn left(&self) -> Option<Duration> {
        self.recent.left(&self.latest)
    }
}

/// Whether a replay is shown with where it is, which it stays once it is,
/// so the bar does not vanish as the end nears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Length {
    Short,
    Long,
}

/// One long replay's line: what it says and how far along it is, from 0
/// to 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub run: String,
    pub text: String,
    pub fraction: f32,
}

impl Waiting {
    /// Takes `line`, which the engine said on its standard error: true when
    /// it was a replay's progress, and what the view shows may change.
    pub fn said(&mut self, line: &str) -> bool {
        let Some(replaying) = Replaying::parse(line) else {
            return false;
        };
        let index = match self
            .replays
            .iter()
            .position(|h| h.latest.run == replaying.run)
        {
            Some(index) => index,
            None => {
                self.replays.push(Heard {
                    latest: replaying.clone(),
                    recent: Recent::default(),
                    length: Length::Short,
                });
                self.replays.len() - 1
            }
        };
        let heard = &mut self.replays[index];

        // A replay of the same run from or to another step is a new one,
        // as `rewind blame` and `rewind diff --file` make.
        if (heard.latest.from, heard.latest.to) != (replaying.from, replaying.to) {
            heard.recent = Recent::default();
            heard.length = Length::Short;
        }
        heard.recent.heard(&replaying);
        heard.latest = replaying;
        if heard.left().is_some_and(|left| left > LONG_WAIT) {
            heard.length = Length::Long;
        }
        true
    }

    /// The lines of the long replays, each named by `label` of its run's
    /// id, or none while every replay is short.
    pub fn rows(&self, label: impl Fn(&str) -> Option<String>) -> Vec<Row> {
        self.replays
            .iter()
            .filter(|h| h.length == Length::Long)
            .map(|h| Row {
                run: h.latest.run.clone(),
                text: describe(&h.latest, h.left(), label(&h.latest.run)),
                fraction: h.latest.fraction(),
            })
            .collect()
    }

    /// The time the longest replay still has left, while one is long.
    pub fn left(&self) -> Option<Duration> {
        self.replays
            .iter()
            .filter(|h| h.length == Length::Long && !h.latest.done())
            .filter_map(Heard::left)
            .max()
    }
}

/// A replay in words, after the name of its run when there is one:
/// where it started and goes, and where it is with the time `left`, or
/// that it got there.
fn describe(r: &Replaying, left: Option<Duration>, label: Option<String>) -> String {
    let start = if r.from == 0 {
        "its start".to_string()
    } else {
        format!("the keyframe at step {}", thousands(r.from))
    };
    let (replayed, replaying) = match &label {
        Some(label) => (format!("{label}: replayed"), format!("{label}: replaying")),
        None => (
            "Replayed the run".to_string(),
            "Replaying the run".to_string(),
        ),
    };
    if r.done() {
        return format!("{replayed} from {start} to step {}.", thousands(r.to));
    }
    let going = format!(
        "{replaying} from {start} to step {}: now at step {}",
        thousands(r.to),
        thousands(r.at)
    );
    match left {
        Some(left) => format!("{going}, {} left.", about(left)),
        None => format!("{going}."),
    }
}

#[cfg(test)]
mod tests {
    // Waiting over made-up progress lines: a short replay stays short, a
    // long one gets a row with where it is and the time left and keeps it,
    // and each of two runs gets its own row under its label.
    use super::*;

    fn line(run: &str, (from, at, to): (u64, u64, u64), elapsed_ms: u64) -> String {
        Replaying {
            run: run.into(),
            from,
            at,
            to,
            elapsed_ms,
        }
        .line()
    }

    #[test]
    fn a_short_replay_has_no_row() {
        // 1,000 steps from a keyframe, done in nine seconds, with under
        // three seconds left when the speed is first known: no row, and
        // the engine's other lines are not progress.
        let mut waiting = Waiting::default();
        assert!(!waiting.said("rewind: walking thread 3's stack in gdb"));
        assert!(waiting.said(&line("a", (5_000, 5_000, 6_000), 0)));
        assert!(waiting.said(&line("a", (5_000, 5_700, 6_000), 6_000)));
        assert!(waiting.said(&line("a", (5_000, 6_000, 6_000), 9_000)));
        assert_eq!(waiting.rows(|_| None), vec![]);
        assert_eq!(waiting.left(), None);
    }

    #[test]
    fn a_long_replay_says_where_it_is_until_it_is_there() {
        // From boot to step 129,747: 33,000 steps in 20 s leaves about a
        // minute, a row a third of the way along. Near the end the row
        // stays, and at the step it says the replay got there.
        let mut waiting = Waiting::default();
        waiting.said(&line("a", (0, 0, 129_747), 0));
        assert_eq!(waiting.rows(|_| None), vec![]);
        waiting.said(&line("a", (0, 43_000, 129_747), 20_000));
        let rows = waiting.rows(|_| None);
        assert_eq!(
            rows[0].text,
            "Replaying the run from its start to step 129,747: now at step 43,000, about 40 seconds left."
        );
        assert!((rows[0].fraction - 43_000.0 / 129_747.0).abs() < 1e-6);
        assert_eq!(waiting.left().map(|left| left.as_secs()), Some(40));

        waiting.said(&line("a", (0, 129_000, 129_747), 60_000));
        assert_eq!(waiting.rows(|_| None).len(), 1);
        waiting.said(&line("a", (0, 129_747, 129_747), 60_400));
        assert_eq!(
            waiting.rows(|_| None)[0].text,
            "Replayed the run from its start to step 129,747."
        );
        assert_eq!(waiting.left(), None);
    }

    #[test]
    fn two_runs_each_get_a_row_under_their_label() {
        // This run replays from the keyframe at step 50,000 and the other
        // from its start, both long: a row each, labeled, in the order
        // first heard.
        let mut waiting = Waiting::default();
        waiting.said(&line("this", (50_000, 60_000, 150_000), 10_000));
        waiting.said(&line("other", (0, 20_000, 120_000), 10_000));
        let label = |run: &str| {
            Some(
                if run == "this" {
                    "This run"
                } else {
                    "The other run"
                }
                .to_string(),
            )
        };
        let rows = waiting.rows(label);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].text,
            "This run: replaying from the keyframe at step 50,000 to step 150,000: now at step 60,000, about 2 minutes left."
        );
        assert!(
            rows[1]
                .text
                .starts_with("The other run: replaying from its start")
        );
        assert_eq!(waiting.left(), Some(Duration::from_secs(90)));
    }
}
