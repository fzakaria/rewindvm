//! The Runs panel's step axis: a family's runs drawn along their own
//! steps over one window, as the Threads tab draws threads. A run is
//! dotted while its machine is still the machine of the run it is
//! measured against, a fork's parent or the schedule 0 run a run from
//! boot hangs under, and solid from the step where the machine first
//! differs to its last step. A run is also marked where the program that
//! failed in one of the two first behaves differently, the place
//! `rewind check` prints.
//!
//! Both places are read from the two runs' traces. On a family of 146
//! runs `rewind check` made of mylib, reading every trace and comparing
//! each run with the run it is measured against took 181 ms in all, so
//! the panel works them out in the background when the axis opens and
//! keeps them while the family grows.
//!
//! After a run parts, its steps are its own: a step of two runs is the
//! same moment only before either parts.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use rewind_trace::Trace;
use rewind_trace::compare::Comparison;

use crate::family::{Family, RunEntry};
use crate::lanes::Window;

/// Where a run parts from the run it is measured against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Parting {
    /// The step of this run where the machine first differs; None when
    /// the two runs are the same machine to the end.
    pub machine: Option<u64>,
    /// The step of this run where the program that failed in one of the
    /// two runs, and not in the other, first behaves differently.
    pub behaves: Option<u64>,
}

impl Parting {
    /// Where `run`, which ended at step `run_last`, parts from `against`,
    /// which ended at `against_last`.
    pub fn of(run: &Trace, run_last: u64, against: &Trace, against_last: u64) -> Parting {
        // Record for record, as rewind diff compares two runs.
        let machine = against.divergence(run).map(|d| d.right_step);

        // The failing program's own events, as rewind check compares the
        // two runs it reports. A comparison that names no program compares
        // every event, which tells only that the machine differs.
        let compared = Comparison::of(run, run_last, against, against_last);
        let behaves = compared.program.and(compared.point).map(|p| p.step);
        Parting { machine, behaves }
    }
}

/// The partings of `family`'s runs that are not in `known`, each against
/// the run it is measured against, with traces read by `read`. A run
/// that has not ended, that is measured against nothing, or whose trace
/// or other trace `read` cannot give is left out.
pub fn partings<T: Borrow<Trace>>(
    family: &Family,
    known: &HashSet<String>,
    mut read: impl FnMut(&RunEntry) -> Option<T>,
) -> HashMap<String, Parting> {
    // The runs to work out, by the run each is measured against, so that
    // run's trace is read once for all of them.
    let mut by_against: HashMap<&str, (&RunEntry, Vec<&RunEntry>)> = HashMap::new();
    for run in &family.runs {
        if known.contains(&run.id) || run.last_step.is_none() {
            continue;
        }
        let Some(against) = family.tree_parent(run) else {
            continue;
        };
        if against.last_step.is_none() {
            continue;
        }
        by_against
            .entry(against.id.as_str())
            .or_insert((against, Vec::new()))
            .1
            .push(run);
    }

    let mut found = HashMap::new();
    for (against, runs) in by_against.into_values() {
        let (Some(other), Some(other_last)) = (read(against), against.last_step) else {
            continue;
        };
        for run in runs {
            let (Some(trace), Some(last)) = (read(run), run.last_step) else {
                continue;
            };
            let parting = Parting::of(trace.borrow(), last, other.borrow(), other_last);
            found.insert(run.id.clone(), parting);
        }
    }
    found
}

/// What a run's row knows of where it parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parted {
    /// It is measured against no run: the family's first run.
    Root,
    /// Not worked out yet.
    Unknown,
    Known(Parting),
}

/// A stretch of a row, as fractions of the window's width from its left
/// edge, each between 0 and 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub from: f32,
    pub to: f32,
}

/// What a run's row draws over a window, each part as fractions of the
/// window's width and None where it falls outside the window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Track {
    /// Dotted: the steps where the machine is still the other run's.
    pub same: Option<Span>,
    /// Solid: from where the machine first differs to the run's end.
    pub own: Option<Span>,
    /// The steps the run's schedule was confined to, for a run `rewind
    /// check` made narrowing a schedule down.
    pub perturbed: Option<Span>,
    pub machine: Option<f32>,
    pub behaves: Option<f32>,
    pub end: Option<f32>,
}

impl Track {
    /// `run`'s row over `window`, from what is known of where it parts.
    pub fn of(run: &RunEntry, parted: Parted, window: Window) -> Track {
        let Some(last) = run.last_step else {
            return Track::default();
        };
        let span = |from: u64, to: u64| span_in(window, from, to);
        let point = |step: u64| window.at(step);

        // Where the machine first differs, and where the failing program
        // first behaves differently. Until the traces are compared, a
        // fork has where its manifest says it first differs from its
        // parent, and a run from boot nothing.
        let (machine, behaves) = match parted {
            Parted::Root => {
                return Track {
                    own: span(0, last),
                    end: point(last),
                    ..Track::default()
                };
            }
            Parted::Known(p) => (p.machine, p.behaves),
            Parted::Unknown => match run.first_difference {
                Some(step) => (Some(step), None),
                None => {
                    return Track {
                        end: point(last),
                        ..Track::default()
                    };
                }
            },
        };

        // Dotted to where the machine differs, solid from there; a run
        // that never differs is dotted to its end.
        let parts = machine.unwrap_or(last).min(last);
        Track {
            same: span(0, parts),
            own: machine.and_then(|m| span(m.min(last), last)),
            perturbed: run
                .window
                .and_then(|(from, until)| span(from, until.min(last))),
            machine: machine.and_then(point),
            behaves: behaves.and_then(point),
            end: point(last),
        }
    }
}

/// The steps `from..to` over `window`, as fractions of its width clipped
/// to it; None when none of them is in it.
fn span_in(window: Window, from: u64, to: u64) -> Option<Span> {
    let first = window.center - window.half as i64;
    let width = (2 * window.half + 1) as f32;
    let across = |step: u64| ((step as i64 - first) as f32 / width).clamp(0.0, 1.0);
    let (from, to) = (across(from), across(to));
    (to > from).then_some(Span { from, to })
}

#[cfg(test)]
mod tests {
    // Rows of runs described in memory over windows of 101 steps, and the
    // partings of small traces of one program: a base that passes, a run
    // whose program fails, and a run that passes another way.
    use super::*;
    use crate::family::{Parent, Progress};
    use rewind_trace::{Event, EventKind};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// Half the width of every window here: 101 steps in all.
    const HALF: u64 = 50;
    /// The step the runs here end at, unless a test says otherwise.
    const LAST: u64 = 90;
    /// The step a fork here was forked at.
    const FORK_STEP: u64 = 20;
    /// Where the machine of a run here first differs.
    const PARTS: u64 = 40;
    const INPUTS: &str = "inputs";

    /// The window of steps 0 to 100.
    fn window() -> Window {
        Window::around(HALF, HALF, LAST)
    }

    /// How far across `window()` a step is.
    fn at(step: u64) -> f32 {
        step as f32 / (2 * HALF + 1) as f32
    }

    fn run(id: &str, parent: Option<&str>, schedule: u64) -> RunEntry {
        RunEntry {
            dir: PathBuf::from(format!("/runs/{id}")),
            id: id.to_string(),
            title: "pool".to_string(),
            family: "/nix/store/x-pool.drv".to_string(),
            parent: parent.map(|p| Parent {
                id: p.to_string(),
                step: FORK_STEP,
            }),
            schedule,
            ending: "exited:0".to_string(),
            failed: false,
            passed: true,
            first_difference: None,
            last_step: Some(LAST),
            trace_hash: Some(format!("hash-{id}")),
            progress: Progress::Ended,
            shares: None,
            cores: 1,
            window: None,
            imported: false,
            inputs: Some(INPUTS.to_string()),
            created: 0,
            has_keyframes: false,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn known(machine: Option<u64>, behaves: Option<u64>) -> Parted {
        Parted::Known(Parting { machine, behaves })
    }

    #[test]
    fn a_run_is_dotted_until_its_machine_differs_then_solid_to_its_end() {
        // A run whose machine first differs at step 40 and ends at 90:
        // dotted 0 to 40, solid 40 to 90, marks at 40 and 90.
        let track = Track::of(
            &run("f", Some("base"), 1),
            known(Some(PARTS), None),
            window(),
        );
        assert_eq!(
            track.same,
            Some(Span {
                from: 0.0,
                to: at(PARTS)
            })
        );
        assert_eq!(
            track.own,
            Some(Span {
                from: at(PARTS),
                to: at(LAST)
            })
        );
        assert_eq!(track.machine, Some(at(PARTS)));
        assert_eq!(track.end, Some(at(LAST)));
        assert_eq!((track.behaves, track.perturbed), (None, None));
    }

    #[test]
    fn the_first_run_is_solid_from_step_0_and_an_identical_run_dotted_to_its_end() {
        // The run measured against nothing is all its own; a run the same
        // machine as its parent to the end is all its parent's.
        let first = Track::of(&run("base", None, 0), Parted::Root, window());
        assert_eq!(
            first.own,
            Some(Span {
                from: 0.0,
                to: at(LAST)
            })
        );
        assert_eq!((first.same, first.machine), (None, None));

        let same = Track::of(&run("f", Some("base"), 1), known(None, None), window());
        assert_eq!(
            same.same,
            Some(Span {
                from: 0.0,
                to: at(LAST)
            })
        );
        assert_eq!((same.own, same.machine), (None, None));
        assert_eq!(same.end, Some(at(LAST)));
    }

    #[test]
    fn the_program_mark_and_the_perturbed_steps_are_drawn_where_they_are() {
        // A run check narrowed to steps 30..45, whose failing program
        // first behaves differently at 60.
        let mut narrowed = run("w", None, 8);
        narrowed.window = Some((30, 45));
        let track = Track::of(&narrowed, known(Some(PARTS), Some(60)), window());
        assert_eq!(
            track.perturbed,
            Some(Span {
                from: at(30),
                to: at(45)
            })
        );
        assert_eq!(track.behaves, Some(at(60)));
    }

    #[test]
    fn what_falls_outside_the_window_is_clipped_or_left_out() {
        // Over steps 50 to 150 a run that parts at 40 and ends at 90 is
        // solid from the left edge; neither its dotted steps nor the mark
        // where it parts show.
        let later = Window::around(100, HALF, 200);
        let track = Track::of(&run("f", Some("base"), 1), known(Some(PARTS), None), later);
        assert_eq!(track.same, None);
        assert_eq!(track.machine, None);
        let own = track.own.unwrap();
        assert_eq!(own.from, 0.0);
        assert!((own.to - 40.0 / 101.0).abs() < 1e-6);
    }

    #[test]
    fn before_its_parting_is_known_a_fork_uses_its_manifests() {
        // A fork's manifest says where it first differs from its parent;
        // a run from boot has nothing to go on and draws only its end.
        let mut fork = run("f", Some("base"), 1);
        fork.first_difference = Some(PARTS);
        let track = Track::of(&fork, Parted::Unknown, window());
        assert_eq!(track.machine, Some(at(PARTS)));
        assert_eq!(
            track.own,
            Some(Span {
                from: at(PARTS),
                to: at(LAST)
            })
        );

        let from_boot = Track::of(&run("c", None, 3), Parted::Unknown, window());
        assert_eq!((from_boot.same, from_boot.own), (None, None));
        assert_eq!(from_boot.end, Some(at(LAST)));
    }

    #[test]
    fn a_run_not_ended_draws_nothing() {
        let mut running = run("f", Some("base"), 1);
        running.last_step = None;
        running.progress = Progress::Running;
        assert_eq!(
            Track::of(&running, Parted::Unknown, window()),
            Track::default()
        );
    }

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

    /// The pid of the test program in every trace here.
    const TEST_PID: u32 = 5;

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
        let parting = Parting::of(&failing, 32, &base, 30);
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
            Parting::of(&later, 32, &base, 30),
            Parting {
                machine: Some(12),
                behaves: None
            }
        );
        assert_eq!(
            Parting::of(&base, 30, &base, 30),
            Parting {
                machine: None,
                behaves: None
            }
        );
    }

    #[test]
    fn a_familys_partings_read_each_trace_once_and_skip_what_is_known() {
        // base is recorded; f forks it; check3 is a run from boot under
        // it; known was worked out before; running has not ended. The
        // base's trace is read once for the two runs measured against it.
        let base_trace = test_run(&[(10, "one\n"), (20, "two\n")], 30, 0);
        let failing = test_run(&[(12, "one\n"), (22, "boom\n")], 32, 2 << 8);
        let mut running = run("running", Some("base"), 4);
        running.last_step = None;
        running.progress = Progress::Running;
        let mut base = run("base", None, 0);
        base.last_step = Some(30);
        let mut fork = run("f", Some("base"), 1);
        fork.last_step = Some(32);
        let mut check = run("check3", None, 3);
        check.last_step = Some(32);
        let family = Family {
            runs: vec![base, fork, check, run("known", Some("base"), 2), running],
        };
        let traces: HashMap<&str, &Trace> = [
            ("base", &base_trace),
            ("f", &failing),
            ("check3", &failing),
            ("known", &failing),
            ("running", &failing),
        ]
        .into_iter()
        .collect();
        let mut reads: HashMap<String, usize> = HashMap::new();
        let skip: HashSet<String> = ["known".to_string()].into_iter().collect();
        let found = partings(&family, &skip, |r: &RunEntry| {
            *reads.entry(r.id.clone()).or_default() += 1;
            traces.get(r.id.as_str()).copied()
        });

        let expected = Parting {
            machine: Some(12),
            behaves: Some(22),
        };
        assert_eq!(found.get("f"), Some(&expected));
        assert_eq!(found.get("check3"), Some(&expected));
        assert_eq!(found.len(), 2);
        assert_eq!(reads.get("base"), Some(&1));
        assert_eq!(reads.get("known"), None);
        assert_eq!(reads.get("running"), None);
    }
}
