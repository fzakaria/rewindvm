//! `rewind ls`: which runs it lists, and how a program reads them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use rewind_core::Run;
use rewind_core::run::{Unreadable, Verdict};
use rewind_trace::ending;

use crate::color::{Paint, Style};

/// How a run stands, as `rewind ls --status` picks runs by.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Its manifest's verdict says it passed: the job exited 0 having
    /// made every output and broken no assertion.
    Passed,
    /// Its verdict says it failed, for any reason, timed out included.
    Failed,
    /// It was stopped at its time limit.
    TimedOut,
    /// A process is executing it now.
    Running,
    /// It never finished, and nothing executes it.
    Interrupted,
    /// Its manifest does not read.
    Unreadable,
}

/// Whether a process is executing a run now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Executing {
    Yes,
    No,
}

impl Status {
    /// How a run whose manifest judged it `verdict` stands. A run that
    /// timed out is `TimedOut`, which [`Status::takes`] counts as failed
    /// too; one with no verdict yet is running while a process executes
    /// it and interrupted otherwise.
    pub(crate) fn of(verdict: &Verdict, executing: Executing) -> Status {
        match (verdict, executing) {
            (Verdict::Passed, _) => Status::Passed,
            (Verdict::Failed { .. }, _) if verdict.timed_out() => Status::TimedOut,
            (Verdict::Failed { .. }, _) => Status::Failed,
            (Verdict::Unfinished, Executing::Yes) => Status::Running,
            (Verdict::Unfinished, Executing::No) => Status::Interrupted,
        }
    }

    /// Whether a run that stands as `have` is one this status asks for.
    pub(crate) fn takes(self, have: Status) -> bool {
        self == have || (self == Status::Failed && have == Status::TimedOut)
    }

    /// The word `--status` takes it by and `--json` says it with.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::TimedOut => "timed-out",
            Status::Running => "running",
            Status::Interrupted => "interrupted",
            Status::Unreadable => "unreadable",
        }
    }
}

/// What `rewind ls` keeps or leaves out a run by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub id: String,
    /// None for a run whose manifest does not read.
    pub name: Option<String>,
    /// When it was made, or for a run whose manifest does not read, when
    /// its directory last changed, in seconds since the Unix epoch.
    pub created: u64,
    pub parent: Option<String>,
    pub status: Status,
}

impl Entry {
    pub(crate) fn of(run: &Run) -> Entry {
        let m = &run.manifest;
        let executing = if m.outcome.is_none() && run.executing() {
            Executing::Yes
        } else {
            Executing::No
        };
        Entry {
            id: m.id.to_string(),
            name: Some(m.name.clone()),
            created: m.created,
            parent: m.parent.as_ref().map(|p| p.run.to_string()),
            status: Status::of(&m.verdict, executing),
        }
    }

    pub(crate) fn unreadable(run: &Unreadable) -> Entry {
        Entry {
            id: run.id.clone(),
            name: None,
            created: run.written,
            parent: None,
            status: Status::Unreadable,
        }
    }
}

/// Which runs `rewind ls` lists.
#[derive(Default)]
pub(crate) struct Filter {
    /// Only runs whose name contains this.
    pub name: Option<String>,
    pub status: Option<Status>,
    /// Only these runs: the forks of a run, and forks of those.
    pub among: Option<BTreeSet<String>>,
    /// Only runs made at or after this, in seconds since the Unix epoch.
    pub since: Option<u64>,
}

impl Filter {
    /// Whether `entry` is listed. A run whose manifest does not read has
    /// no name or parent to match.
    pub(crate) fn keeps(&self, entry: &Entry) -> bool {
        let named = self.name.as_ref().is_none_or(|want| {
            entry
                .name
                .as_ref()
                .is_some_and(|name| name.contains(want.as_str()))
        });
        let standing = self.status.is_none_or(|want| want.takes(entry.status));
        let among = self
            .among
            .as_ref()
            .is_none_or(|ids| ids.contains(&entry.id));
        let recent = self.since.is_none_or(|since| entry.created >= since);
        named && standing && among && recent
    }
}

/// A run as `rewind ls` lists it.
pub(crate) enum Row<'a> {
    Run(&'a Run),
    Unreadable(&'a Unreadable),
}

/// A run's line: its summary, or for one whose manifest does not read,
/// why, painted for standard output.
pub(crate) fn line(row: &Row, paint: Paint) -> String {
    match row {
        Row::Run(run) => crate::show::summary(run, paint),
        Row::Unreadable(u) => format!(
            "{}  {}  {}",
            u.id,
            paint.paint(Style::Differs, "unreadable"),
            u.reason
        ),
    }
}

/// A run as one JSON object, for `--json`.
pub(crate) fn json(entry: &Entry, row: &Row) -> serde_json::Value {
    let run = match row {
        Row::Run(run) => run,
        Row::Unreadable(u) => {
            return serde_json::json!({
                "id": u.id,
                "dir": u.dir,
                "state": entry.status.word(),
                "reason": u.reason,
            });
        }
    };
    let m = &run.manifest;
    let outcome = m.outcome.as_ref();
    serde_json::json!({
        "id": m.id,
        "name": m.name,
        "dir": run.dir,
        "created": m.created,
        "parent": m.parent,
        "state": entry.status.word(),
        "status": outcome.and_then(|o| o.status),
        "ending": outcome.map(|o| ending::label(o, &m.verdict)),
        "verdict": m.verdict,
        "steps": outcome.map(|o| o.step),
    })
}

/// The runs forked from `root` among `entries`, and forks of those, by id.
pub(crate) fn forks_of(entries: &[Entry], root: &str) -> BTreeSet<String> {
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in entries {
        if let Some(parent) = &e.parent {
            children.entry(parent).or_default().push(&e.id);
        }
    }
    let mut forks = BTreeSet::new();
    let mut next = vec![root];
    while let Some(id) = next.pop() {
        for &child in children.get(id).into_iter().flatten() {
            if forks.insert(child.to_string()) {
                next.push(child);
            }
        }
    }
    forks
}

/// The time `--since` names, in seconds since the Unix epoch, given the
/// time now: an amount ago, such as 30m, 2h, 7d or 2w, or a day, such as
/// 2026-10-01, from its start in UTC.
pub(crate) fn since(text: &str, now: u64) -> Result<u64> {
    if let Some(day) = day_start(text) {
        return day;
    }

    // An amount ago: digits, then the unit's letter.
    let unit = text
        .chars()
        .last()
        .with_context(|| format!("--since {text:?}: {SINCE_FORMS}"))?;
    let amount = &text[..text.len() - unit.len_utf8()];
    let seconds = UNITS
        .iter()
        .find(|(letter, _)| *letter == unit)
        .map(|(_, seconds)| *seconds);
    let (Some(seconds), Ok(amount)) = (seconds, amount.parse::<u64>()) else {
        bail!("--since {text:?}: {SINCE_FORMS}");
    };
    Ok(now.saturating_sub(amount.saturating_mul(seconds)))
}

/// What `--since` takes, for its errors.
const SINCE_FORMS: &str =
    "give an amount ago such as 30m, 2h, 7d or 2w, or a day such as 2026-10-01";

/// The units of an amount ago, and the seconds in each.
const UNITS: &[(char, u64)] = &[
    ('s', 1),
    ('m', 60),
    ('h', 60 * 60),
    ('d', DAY),
    ('w', 7 * DAY),
];

const DAY: u64 = 24 * 60 * 60;

/// The start of the day `text` names as YYYY-MM-DD, in UTC, in seconds
/// since the Unix epoch; None when it is not of that form, and an error
/// when it is but names no day.
fn day_start(text: &str) -> Option<Result<u64>> {
    let mut parts = text.splitn(3, '-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(year, 4) && digits(month, 2) && digits(day, 2)) {
        return None;
    }
    let (year, month, day): (i64, u32, u32) =
        (year.parse().ok()?, month.parse().ok()?, day.parse().ok()?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => 0,
    };
    if day == 0 || day > days_in_month {
        return Some(Err(anyhow::anyhow!("--since {text}: there is no such day")));
    }
    let days = days_from_civil(year, month, day);
    Some(
        u64::try_from(days)
            .map(|d| d * DAY)
            .map_err(|_| anyhow::anyhow!("--since {text}: give a day after 1970-01-01")),
    )
}

/// Days from 1970-01-01 to a day of the proleptic Gregorian calendar, by
/// Howard Hinnant's days_from_civil.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    // Which runs `rewind ls` keeps, over entries made by hand, and the
    // times --since takes.
    use super::*;
    use rewind_trace::verdict::{FailedAssertion, Reason};

    fn entry(id: &str, name: &str, created: u64, parent: Option<&str>, status: Status) -> Entry {
        Entry {
            id: id.into(),
            name: Some(name.into()),
            created,
            parent: parent.map(String::from),
            status,
        }
    }

    fn failed(reasons: Vec<Reason>) -> Verdict {
        Verdict::Failed { reasons }
    }

    #[test]
    fn a_run_stands_by_its_manifests_verdict() {
        // A verdict of passed is passed; one failed for any reason is
        // failed, a broken assertion after an exit of 0 included; one
        // failed by a timeout, among other reasons or alone, is timed-out;
        // and a run with no verdict yet is running while executed and
        // interrupted otherwise.
        use Status::*;
        let stands = |verdict: &Verdict| Status::of(verdict, Executing::No);
        assert_eq!(stands(&Verdict::Passed), Passed);
        assert_eq!(stands(&failed(vec![Reason::Exited { code: 2 }])), Failed);
        assert_eq!(stands(&failed(vec![Reason::Crashed])), Failed);
        let asserted = failed(vec![Reason::FailedAssertions {
            assertions: vec![FailedAssertion {
                step: 9,
                pid: 7,
                tid: 7,
                text: "always \"even\" failed".into(),
            }],
        }]);
        assert_eq!(stands(&asserted), Failed);
        assert_eq!(stands(&failed(vec![Reason::TimedOut])), TimedOut);
        assert_eq!(
            stands(&failed(vec![Reason::TimedOut, Reason::Exited { code: 1 }])),
            TimedOut
        );
        assert_eq!(Status::of(&Verdict::Unfinished, Executing::Yes), Running);
        assert_eq!(Status::of(&Verdict::Unfinished, Executing::No), Interrupted);
    }

    #[test]
    fn a_status_takes_the_runs_that_stand_so() {
        // Failed takes a timed-out run too; every other status only its
        // own.
        use Status::*;
        assert!(Failed.takes(Failed));
        assert!(Failed.takes(TimedOut));
        assert!(TimedOut.takes(TimedOut));
        assert!(!TimedOut.takes(Failed));
        assert!(!Passed.takes(Failed));
        assert!(Passed.takes(Passed));
        assert!(!Running.takes(Interrupted));
        assert_eq!(TimedOut.word(), "timed-out");
        assert_eq!(Unreadable.word(), "unreadable");
    }

    #[test]
    fn a_filter_keeps_runs_that_match_everything_it_asks() {
        // Three runs and one whose manifest does not read: each part of
        // the filter alone, then two together, and the unreadable run kept
        // only by what it can answer.
        use Status::*;
        let runs = [
            entry("a", "mylib-0.3.0", 100, None, Failed),
            entry(
                "b",
                "mylib-0.3.0 (fork of a at 5, schedule 1)",
                200,
                Some("a"),
                Passed,
            ),
            entry("c", "hello", 300, None, Passed),
            Entry::unreadable(&rewind_core::run::Unreadable {
                id: "d".into(),
                dir: "/runs/d".into(),
                reason: "missing field".into(),
                written: 400,
            }),
        ];
        let kept = |filter: Filter| -> Vec<&str> {
            runs.iter()
                .filter(|e| filter.keeps(e))
                .map(|e| e.id.as_str())
                .collect()
        };
        assert_eq!(kept(Filter::default()), vec!["a", "b", "c", "d"]);
        let name = Some("mylib".to_string());
        assert_eq!(
            kept(Filter {
                name: name.clone(),
                ..Filter::default()
            }),
            vec!["a", "b"]
        );
        let status = |s| Filter {
            status: Some(s),
            ..Filter::default()
        };
        assert_eq!(kept(status(Passed)), vec!["b", "c"]);
        assert_eq!(kept(status(Unreadable)), vec!["d"]);
        let among = Some(forks_of(&runs, "a"));
        assert_eq!(
            kept(Filter {
                among,
                ..Filter::default()
            }),
            vec!["b"]
        );
        assert_eq!(
            kept(Filter {
                since: Some(250),
                ..Filter::default()
            }),
            vec!["c", "d"]
        );
        assert_eq!(
            kept(Filter {
                name,
                status: Some(Failed),
                ..Filter::default()
            }),
            vec!["a"]
        );
    }

    #[test]
    fn forks_of_a_run_take_their_forks_too() {
        // r's fork a has a fork b; x is no relation. The run itself is
        // not its own fork.
        use Status::*;
        let runs = [
            entry("r", "r", 0, None, Passed),
            entry("a", "a", 1, Some("r"), Passed),
            entry("b", "b", 2, Some("a"), Passed),
            entry("x", "x", 3, None, Passed),
        ];
        let ids: Vec<String> = forks_of(&runs, "r").into_iter().collect();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert!(forks_of(&runs, "b").is_empty());
    }

    #[test]
    fn since_takes_an_amount_ago_or_a_day() {
        // 2026-10-04 12:00 UTC, in seconds since the Unix epoch.
        let midnight = 1_791_072_000;
        let now = midnight + 12 * 3600;
        assert_eq!(since("30m", now).unwrap(), now - 30 * 60);
        assert_eq!(since("2h", now).unwrap(), now - 2 * 3600);
        assert_eq!(since("7d", now).unwrap(), now - 7 * 86_400);
        assert_eq!(since("2w", now).unwrap(), now - 14 * 86_400);
        assert_eq!(since("45s", now).unwrap(), now - 45);
        assert_eq!(since("2026-10-04", now).unwrap(), midnight);
        assert_eq!(since("1970-01-02", now).unwrap(), 86_400);
        assert_eq!(since("2024-02-29", now).unwrap(), 1_709_164_800);
        for bad in [
            "",
            "7",
            "d",
            "7y",
            "2026-13-01",
            "2026-10-32",
            "2026-02-30",
            "yesterday",
        ] {
            assert!(since(bad, now).is_err(), "{bad}");
        }
    }
}
