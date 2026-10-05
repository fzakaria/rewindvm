//! The guided tour: six short stops that each point at one part of the
//! window and move the playhead to what they talk about.
//!
//! This module holds the stops, moving between them, and remembering that
//! the user dismissed the tour; `ui::tour` draws the callout.

use std::path::PathBuf;

use crate::model::Timeline;

/// The part of the window a stop points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Timeline,
    FailureButton,
    EventCard,
    DivergenceButton,
    ForkButton,
    RunsPill,
}

/// Where a stop puts the playhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playhead {
    /// The start of the check phase, or the middle of a run without one.
    CheckPhase,
    Failure,
    Divergence,
}

/// One stop of the tour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stop {
    pub anchor: Anchor,
    pub title: &'static str,
    pub body: &'static str,
    pub playhead: Playhead,
    /// What the stop needs from the run to make sense.
    pub needs: Needs,
}

/// What a stop needs from the run on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    Failure,
    Divergence,
    /// Other runs of the same build, for the Runs panel.
    Family,
}

/// The phase the first stop starts the playhead at.
const CHECK_PHASE: &str = "check";

pub const STOPS: [Stop; 6] = [
    Stop {
        anchor: Anchor::Timeline,
        title: "The whole run",
        body: "One step is one time the VM stops and hands control to Rewind, and a step always means the same machine state. Drag the playhead, or use Left and Right to move between events and Shift for single steps; Stop at picks which events, such as one thread's. The segments are the build's phases.",
        playhead: Playhead::CheckPhase,
        needs: Needs::Nothing,
    },
    Stop {
        anchor: Anchor::FailureButton,
        title: "The failure",
        body: "The red mark is the failure: the first crash signal, or else the process whose nonzero exit failed the run, or where a run that hung was stopped. Jump to failure, or f, puts the playhead on it.",
        playhead: Playhead::Failure,
        needs: Needs::Failure,
    },
    Stop {
        anchor: Anchor::EventCard,
        title: "What happened at this step",
        body: "Here the last event is the crash signal: the thread that got it and the address it faulted on. The log, the process tree and the files show the machine as it was at this step.",
        playhead: Playhead::Failure,
        needs: Needs::Failure,
    },
    Stop {
        anchor: Anchor::DivergenceButton,
        title: "Where the two runs part",
        body: "The blue mark is the first event where this run differs from the one it is compared with, here a passing build of the same inputs. Before it the two are identical, event for event. Jump to divergence, or d, goes there.",
        playhead: Playhead::Divergence,
        needs: Needs::Divergence,
    },
    Stop {
        anchor: Anchor::ForkButton,
        title: "Branch from any step",
        body: "Fork from here runs the same inputs again from the playhead under another thread schedule, to see whether the failure depends on it. Shell and gdb work on a throwaway copy of the VM at the playhead. Forking needs the engine and KVM on your machine, and your own runs; the example can only be scrubbed.",
        playhead: Playhead::Divergence,
        needs: Needs::Nothing,
    },
    Stop {
        anchor: Anchor::RunsPill,
        title: "Every run of this build",
        body: "The runs pill, or the Runs tab beside At this step, shows every run of this build, drawn as a tree. Schedule 0 is the run with its threads left alone. Under it hang its forks, each from the step it forked at, and the schedules rewind check ran from boot; those that ended the way it did fold into one row. A schedule is the seed that perturbs the threads. Click a run to open it beside the run it hangs under; right-click it to compare, copy or remove it.",
        playhead: Playhead::Divergence,
        needs: Needs::Family,
    },
];

/// The stops that make sense for a run: without a failure or a compared
/// run, the stops about them are left out.
pub fn stops_for(has_failure: bool, has_divergence: bool, has_family: bool) -> Vec<Stop> {
    STOPS
        .iter()
        .copied()
        .filter(|s| match s.needs {
            Needs::Nothing => true,
            Needs::Failure => has_failure,
            Needs::Divergence => has_divergence,
            Needs::Family => has_family,
        })
        .collect()
}

/// Where a stop puts the playhead on `timeline`.
pub fn playhead_step(playhead: Playhead, timeline: &Timeline, divergence: Option<u64>) -> u64 {
    let middle = timeline.total / 2;
    match playhead {
        Playhead::CheckPhase => timeline
            .phases
            .iter()
            .find(|p| p.name == CHECK_PHASE)
            .map_or(middle, |p| p.start),
        Playhead::Failure => timeline.failure.map_or(middle, |f| f.step),
        Playhead::Divergence => divergence.unwrap_or(middle),
    }
}

/// A tour in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tour {
    pub stops: Vec<Stop>,
    pub index: usize,
}

/// What moving forward did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advance {
    Moved,
    Finished,
}

impl Tour {
    pub fn new(stops: Vec<Stop>) -> Tour {
        Tour { stops, index: 0 }
    }

    pub fn current(&self) -> Option<&Stop> {
        self.stops.get(self.index)
    }

    pub fn advance(&mut self) -> Advance {
        if self.index + 1 >= self.stops.len() {
            return Advance::Finished;
        }
        self.index += 1;
        Advance::Moved
    }

    /// Moves back one stop; the first stop stays where it is.
    pub fn back(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    pub fn is_first(&self) -> bool {
        self.index == 0
    }

    pub fn is_last(&self) -> bool {
        self.index + 1 >= self.stops.len()
    }
}

/// The file whose presence says the user has seen or dismissed the tour.
const DISMISSED_FILE: &str = "tour-dismissed";
const CONFIG_SUBDIR: &str = "rewind";
const XDG_CONFIG_ENV: &str = "XDG_CONFIG_HOME";
const HOME_CONFIG_DIR: &str = ".config";

/// The app's settings directory: $XDG_CONFIG_HOME/rewind, else
/// ~/.config/rewind.
pub fn config_dir() -> Option<PathBuf> {
    let config = std::env::var_os(XDG_CONFIG_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(HOME_CONFIG_DIR)))?;
    Some(config.join(CONFIG_SUBDIR))
}

fn dismissed_path() -> Option<PathBuf> {
    Some(config_dir()?.join(DISMISSED_FILE))
}

/// Whether the user has finished or skipped the tour before.
pub fn was_dismissed() -> bool {
    dismissed_path().is_some_and(|p| p.exists())
}

/// Remembers that the user finished or skipped the tour. A failure to
/// write only means the tour offers itself again next time.
pub fn remember_dismissed() {
    let Some(path) = dismissed_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, b"");
}

#[cfg(test)]
mod tests {
    // The tour's stops and moves over a hand-built run, without a window.
    use super::*;
    use rewind_trace::{Event, EventKind, Trace};

    fn timeline() -> Timeline {
        let out = |step, text: &str| Event {
            step,
            pid: 1,
            tid: 1,
            kind: EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        };
        let events = vec![
            out(10, "Running phase: buildPhase\n"),
            out(50, "Running phase: checkPhase\n"),
            Event {
                step: 80,
                pid: 2,
                tid: 3,
                kind: EventKind::Signal {
                    signo: 11,
                    code: 1,
                    addr: 8,
                },
            },
            out(100, "done\n"),
        ];
        Timeline::new(Trace { events }, None, None)
    }

    #[test]
    fn stops_that_do_not_apply_are_left_out() {
        // Without a failure, a comparison or other runs only the timeline
        // and fork stops remain; with all three, all six; with only other
        // runs, the Runs stop joins the two.
        let anchors = |stops: Vec<Stop>| stops.iter().map(|s| s.anchor).collect::<Vec<_>>();
        assert_eq!(
            anchors(stops_for(false, false, false)),
            vec![Anchor::Timeline, Anchor::ForkButton]
        );
        assert_eq!(stops_for(true, true, true).len(), STOPS.len());
        assert_eq!(
            anchors(stops_for(false, false, true)),
            vec![Anchor::Timeline, Anchor::ForkButton, Anchor::RunsPill]
        );
    }

    #[test]
    fn each_stop_puts_the_playhead_on_its_subject() {
        // The check phase starts at 50, the crash is at 80, and the
        // divergence given is 70; without one, the middle of the run.
        let t = timeline();
        assert_eq!(playhead_step(Playhead::CheckPhase, &t, None), 50);
        assert_eq!(playhead_step(Playhead::Failure, &t, None), 80);
        assert_eq!(playhead_step(Playhead::Divergence, &t, Some(70)), 70);
        assert_eq!(playhead_step(Playhead::Divergence, &t, None), 50);
    }

    #[test]
    fn next_and_back_stay_inside_the_tour() {
        // Back on the first stop stays; next on the last stop finishes.
        let mut tour = Tour::new(stops_for(true, true, true));
        tour.back();
        assert!(tour.is_first());
        for _ in 1..STOPS.len() {
            assert_eq!(tour.advance(), Advance::Moved);
        }
        assert!(tour.is_last());
        assert_eq!(tour.advance(), Advance::Finished);
        assert_eq!(tour.current().unwrap().anchor, Anchor::RunsPill);
    }
}
