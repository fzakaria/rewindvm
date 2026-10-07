//! "Check from here": schedules tried from the playhead as forks of the
//! run, the way `rewind check --run RUN --schedule-from STEP --all
//! --no-narrow --json` tries them, and how each ended against the run.

use std::path::PathBuf;

use serde::Deserialize;

/// How many schedules a check from here can try, and how many it tries
/// until another count is chosen.
pub const SIZES: [u64; 4] = [8, 16, 32, 64];
pub const DEFAULT_SCHEDULES: u64 = 16;

/// What the amber button at the end of the controls does at the
/// playhead: fork the run once, or check it with this many schedules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HereAction {
    #[default]
    Fork,
    Check {
        schedules: u64,
    },
}

/// What `rewind check --json` prints once it has tried every schedule.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Checked {
    /// The run itself, standing for schedule 0, then each schedule tried.
    pub schedules: Vec<Tried>,
    pub tried: u64,
    pub differing: u64,
}

/// One run `rewind check` tried.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Tried {
    pub id: String,
    pub dir: PathBuf,
    pub schedule: u64,
    /// How it ended, in the app's words: exited:2, killed:SIGSEGV,
    /// timed-out.
    pub ending: Option<String>,
    /// Whether it ended differently from the run.
    pub differs: bool,
}

/// How a schedule ended against the run it was forked from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// As the run did.
    Same,
    /// Some other way.
    Differs,
    /// It ran out of time, which says nothing of how it would have ended.
    TimedOut,
}

impl Tried {
    pub fn outcome(&self) -> Outcome {
        if self.ending.as_deref() == Some(rewind_trace::stop::TIMED_OUT_ENDING) {
            return Outcome::TimedOut;
        }
        if self.differs {
            return Outcome::Differs;
        }
        Outcome::Same
    }
}

impl Checked {
    /// The schedules tried, without the run itself.
    pub fn forks(&self) -> &[Tried] {
        self.schedules.get(1..).unwrap_or_default()
    }

    /// How the schedules ended, in words: "11 ended as this run did
    /// (killed:SIGSEGV), 4 differently, 1 timed out".
    pub fn summary(&self) -> String {
        let count = |outcome: Outcome| {
            self.forks()
                .iter()
                .filter(|t| t.outcome() == outcome)
                .count()
        };
        let ending = self
            .schedules
            .first()
            .and_then(|run| run.ending.as_deref())
            .unwrap_or_default();
        let mut parts = vec![format!(
            "{} ended as this run did ({ending})",
            count(Outcome::Same)
        )];
        let differs = count(Outcome::Differs);
        if differs > 0 {
            parts.push(format!("{differs} differently"));
        }
        let timed_out = count(Outcome::TimedOut);
        if timed_out > 0 {
            parts.push(format!("{timed_out} timed out"));
        }
        parts.join(", ")
    }
}

/// The schedule a line `rewind check` says while it works is about, as in
/// "schedule   3: exited:1 ...": the progress of a check from here.
pub fn schedule_said(line: &str) -> Option<u64> {
    let (number, _) = line.strip_prefix(SCHEDULE_LINE)?.split_once(':')?;
    number.trim().parse().ok()
}

/// How `rewind check` starts the line it says for each schedule.
const SCHEDULE_LINE: &str = "schedule ";

#[cfg(test)]
mod tests {
    // Checks from made-up JSON in the shape `rewind check --json` prints.
    use super::*;

    fn tried(schedule: u64, ending: &str, differs: bool) -> Tried {
        Tried {
            id: format!("run{schedule}"),
            dir: PathBuf::from(format!("/runs/run{schedule}")),
            schedule,
            ending: Some(ending.to_string()),
            differs,
        }
    }

    /// A schedule that timed out counts as timed out whether or not its
    /// ending differs; the rest are the same or differ as check says.
    #[test]
    fn a_schedule_ended_the_same_differently_or_out_of_time() {
        assert_eq!(tried(1, "killed:SIGSEGV", false).outcome(), Outcome::Same);
        assert_eq!(tried(2, "exited:0", true).outcome(), Outcome::Differs);
        assert_eq!(tried(3, "timed-out", true).outcome(), Outcome::TimedOut);
    }

    /// The JSON reads with the fields the app uses and ignores the rest;
    /// the summary counts each outcome and names the run's ending.
    #[test]
    fn the_engines_json_reads_and_sums_up() {
        let json = r#"{"schedules":[
            {"id":"base","dir":"/runs/base","schedule":0,"ending":"killed:SIGSEGV","differs":false,"steps":4412},
            {"id":"a","dir":"/runs/a","schedule":1,"ending":"killed:SIGSEGV","differs":false},
            {"id":"b","dir":"/runs/b","schedule":2,"ending":"exited:0","differs":true},
            {"id":"c","dir":"/runs/c","schedule":3,"ending":"timed-out","differs":true},
            {"id":"d","dir":"/runs/d","schedule":4,"ending":"exited:0","differs":true}
        ],"tried":4,"differing":3,"schedule_0_failed":true,"narrowed":null}"#;
        let checked: Checked = serde_json::from_str(json).unwrap();
        assert_eq!(checked.forks().len(), 4);
        assert_eq!(checked.forks()[1].id, "b");
        assert_eq!(
            checked.summary(),
            "1 ended as this run did (killed:SIGSEGV), 2 differently, 1 timed out"
        );

        // Nothing timed out, nothing differs.
        let same = Checked {
            schedules: vec![tried(0, "exited:0", false), tried(1, "exited:0", false)],
            tried: 1,
            differing: 0,
        };
        assert_eq!(same.summary(), "1 ended as this run did (exited:0)");
    }

    /// Check's per-schedule lines name their schedule; other lines do not.
    #[test]
    fn progress_lines_name_their_schedule() {
        assert_eq!(
            schedule_said("schedule   3: exited:1        355 steps    run 28fb816f"),
            Some(3)
        );
        assert_eq!(schedule_said("schedule  16: exited:0"), Some(16));
        assert_eq!(
            schedule_said("4 of 16 perturbed schedules ended differently"),
            None
        );
        assert_eq!(schedule_said("rewind: schedules 1..8 executing"), None);
    }
}
