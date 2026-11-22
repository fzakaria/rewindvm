//! Where a run parts from the run it hangs under in its family: a fork's
//! parent, or for a run from boot under another schedule the schedule 0
//! run of its inputs. The desktop app's Runs panel draws both places
//! along the runs' steps, and an export carries them in `parting.json`
//! for a reader that has only the one run, as the browser viewer has.

use serde::{Deserialize, Serialize};

use crate::Trace;
use crate::compare::Comparison;
use crate::manifest::RunId;

/// The member of an export that says where its run parts from the run it
/// hangs under, written when that run was on the exporting machine.
pub const PARTING: &str = "parting.json";

/// The most bytes of `parting.json` a reader takes: a run id and two
/// steps, far below this, so a larger file is not one an export wrote.
pub const MAX_PARTING: u64 = 4096;

/// Where a run parts from the run it is measured against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parting {
    /// The step of this run where the machine first differs; None when
    /// the two runs are the same machine to the end.
    pub machine: Option<u64>,
    /// The step of this run where the program that failed in one of the
    /// two runs, and not in the other, first behaves differently. None
    /// when the two runs end alike.
    pub behaves: Option<u64>,
}

/// Whether two runs end alike: both failing, or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endings {
    Alike,
    /// One run failed and the other did not, as the pair `rewind check`
    /// reports.
    Differ,
}

impl Endings {
    /// The endings of a run that failed or not and another that failed or
    /// not.
    pub fn of(failed: bool, other_failed: bool) -> Endings {
        if failed == other_failed {
            Endings::Alike
        } else {
            Endings::Differ
        }
    }
}

impl Parting {
    /// Where `run`, which ended at step `run_last`, parts from `against`,
    /// which ended at `against_last`.
    pub fn of(
        run: &Trace,
        run_last: u64,
        against: &Trace,
        against_last: u64,
        endings: Endings,
    ) -> Parting {
        // Record for record, as rewind diff compares two runs.
        let machine = against.divergence(run).map(|d| d.right_step);

        // Two runs that end alike have no failing program to follow. A
        // program inside either can still fail in one and not the other,
        // as a configure probe of a Nix build does from schedule to
        // schedule, and marking it would point at noise. Skipping the
        // comparison is also most of the cost: on 64 runs of GNU hello's
        // build, 170,000 steps each, it took 2.7 s of 3.6 s.
        if endings == Endings::Alike {
            return Parting {
                machine,
                behaves: None,
            };
        }

        // The failing program's own events, as rewind check compares the
        // two runs it reports, when that program's events part. Without
        // a program whose events part there is no mark: a comparison of
        // every event tells only that the machine differs.
        let program = run
            .culprit_against(against)
            .or_else(|| against.culprit_against(run))
            .filter(|argv| run.divergence_in(against, argv).is_some());
        let behaves = program.and_then(|argv| {
            Comparison::about(run, run_last, against, against_last, Some(argv))
                .point
                .map(|p| p.step)
        });
        Parting { machine, behaves }
    }
}

/// What `parting.json` holds: the run an exported run parts from, and
/// where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartsFrom {
    pub run: RunId,
    #[serde(flatten)]
    pub parting: Parting,
}

impl PartsFrom {
    /// The parting in `bytes`, as an export carries it; None when they do
    /// not parse as one.
    pub fn parse(bytes: &[u8]) -> Option<PartsFrom> {
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    // The partings of small traces of one program: a base that passes, a
    // run whose program fails, and a run that passes another way.
    use super::*;
    use crate::{Event, EventKind};

    /// The pid of the test program in every trace here.
    const TEST_PID: u32 = 5;

    fn ev(step: u64, pid: u32, kind: EventKind) -> Event {
        Event {
            step,
            ns: step + 1,
            pid,
            tid: pid,
            kind,
        }
    }

    fn out(step: u64, text: &str) -> Event {
        ev(
            step,
            TEST_PID,
            EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        )
    }

    /// A trace of the shell starting ./test, which writes `lines` at the
    /// steps given and then exits with `status`.
    fn test_run(lines: &[(u64, &str)], exit_step: u64, status: u32) -> Trace {
        let mut events = vec![
            ev(
                1,
                1,
                EventKind::Fork {
                    child: TEST_PID,
                    thread: false,
                },
            ),
            ev(
                2,
                TEST_PID,
                EventKind::Exec {
                    filename: "./test".into(),
                    argv: vec!["./test".into()],
                    old_pid: TEST_PID,
                },
            ),
        ];
        events.extend(lines.iter().map(|(step, text)| out(*step, text)));
        events.push(ev(
            exit_step,
            TEST_PID,
            EventKind::Exit {
                status,
                comm: String::new(),
                thread: false,
            },
        ));
        Trace {
            events,
            ..Default::default()
        }
    }

    #[test]
    fn a_failing_run_parts_where_its_machine_differs_and_its_program_behaves_differently() {
        // The base writes "one" at 10 and "two" at 20. The failing run
        // writes "one" two steps later, so its machine differs at 12,
        // then writes "boom" where the base wrote "two": the program
        // behaves differently at 22.
        let base = test_run(&[(10, "one\n"), (20, "two\n")], 30, 0);
        let failing = test_run(&[(12, "one\n"), (22, "boom\n")], 32, 2 << 8);
        let parting = Parting::of(&failing, 32, &base, 30, Endings::Differ);
        assert_eq!(parting.machine, Some(12));
        assert_eq!(parting.behaves, Some(22));
    }

    #[test]
    fn two_runs_that_pass_part_with_no_program_mark() {
        // A run that writes the base's lines later passes as the base
        // did: its machine differs, but no program failed to follow.
        let base = test_run(&[(10, "one\n"), (20, "two\n")], 30, 0);
        let later = test_run(&[(12, "one\n"), (22, "two\n")], 32, 0);
        assert_eq!(
            Parting::of(&later, 32, &base, 30, Endings::Alike),
            Parting {
                machine: Some(12),
                behaves: None
            }
        );
        assert_eq!(
            Parting::of(&base, 30, &base, 30, Endings::Alike),
            Parting {
                machine: None,
                behaves: None
            }
        );
    }

    #[test]
    fn runs_that_end_alike_get_no_program_mark_though_a_program_failed_in_one() {
        // Both runs pass, as two schedules of a build do, but a program
        // inside one exits 2 where the other's exited 0, as a configure
        // probe can: the machine differs and no program mark is drawn.
        let base = test_run(&[(10, "one\n"), (20, "two\n")], 30, 0);
        let probe_failed = test_run(&[(12, "one\n"), (22, "boom\n")], 32, 2 << 8);
        assert_eq!(
            Parting::of(&probe_failed, 32, &base, 30, Endings::Alike),
            Parting {
                machine: Some(12),
                behaves: None
            }
        );
    }

    #[test]
    fn endings_differ_when_one_run_failed_and_the_other_did_not() {
        // Two passing runs, and two failing ones, end alike.
        assert_eq!(Endings::of(false, false), Endings::Alike);
        assert_eq!(Endings::of(true, true), Endings::Alike);
        assert_eq!(Endings::of(true, false), Endings::Differ);
        assert_eq!(Endings::of(false, true), Endings::Differ);
    }

    #[test]
    fn a_parting_file_names_the_run_and_both_steps() {
        // What an export writes reads back; a file that is not one reads
        // as none.
        let carried = PartsFrom {
            run: RunId::parse("1def0466c25329db").unwrap(),
            parting: Parting {
                machine: Some(4692),
                behaves: Some(4705),
            },
        };
        let bytes = serde_json::to_vec(&carried).unwrap();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"run":"1def0466c25329db","machine":4692,"behaves":4705}"#
        );
        assert_eq!(PartsFrom::parse(&bytes), Some(carried));
        assert_eq!(PartsFrom::parse(b"{\"run\":\"../x\"}"), None);
    }
}
