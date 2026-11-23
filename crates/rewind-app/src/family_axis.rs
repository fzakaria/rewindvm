//! The Runs panel's step axis: a family's runs drawn along their own
//! steps over one window, as the Threads tab draws threads. A run is
//! thin while its machine is still the machine of the run it is
//! measured against, a fork's parent or the schedule 0 run a run from
//! boot hangs under, and thick from the step where the machine first
//! differs to its last step. When one of the two runs failed and the
//! other did not, the run is also marked where the failing program first
//! behaves differently, the place `rewind check` prints.
//!
//! Both places are read from the two runs' traces. On a family of 146
//! runs `rewind check` made of mylib, about 6,000 steps each, reading
//! every trace and comparing each run with the run it is measured
//! against took 181 ms in all; on 65 runs of GNU hello's Nix build,
//! about 170,000 steps each and all passing, 1.5 s, nearly all of it
//! reading the traces. The panel works them out in the background when
//! the axis opens and keeps them while the family grows.
//!
//! After a run parts, its steps are its own: a step of two runs is the
//! same moment only before either parts.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use rewind_trace::Trace;

use crate::family::{Family, RunEntry};
use crate::lanes::Window;

pub use rewind_trace::parting::{Endings, Parting};

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
            let endings = Endings::of(run.failed, against.failed);
            let parting = Parting::of(trace.borrow(), last, other.borrow(), other_last, endings);
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
    /// Thin: the steps where the machine is still the other run's.
    pub same: Option<Span>,
    /// Thick: from where the machine first differs to the run's end.
    pub own: Option<Span>,
    /// The steps the run's schedule perturbed: to its end for a whole
    /// schedule or a fork, a window for a run `rewind check` made
    /// narrowing a schedule down.
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

        // The steps the run's schedule perturbed, whatever else is known.
        let perturbed = run
            .perturbs
            .and_then(|(from, until)| span(from, until.min(last)));

        // Where the machine first differs, and where the failing program
        // first behaves differently. Until the traces are compared, a
        // fork has where its manifest says it first differs from its
        // parent, and a run from boot nothing.
        let (machine, behaves) = match parted {
            Parted::Root => {
                return Track {
                    own: span(0, last),
                    perturbed,
                    end: point(last),
                    ..Track::default()
                };
            }
            Parted::Known(p) => (p.machine, p.behaves),
            Parted::Unknown => match run.first_difference {
                Some(step) => (Some(step), None),
                None => {
                    return Track {
                        perturbed,
                        end: point(last),
                        ..Track::default()
                    };
                }
            },
        };

        // Thin to where the machine differs, thick from there; a run that
        // never differs is thin to its end.
        let parts = machine.unwrap_or(last).min(last);
        Track {
            same: span(0, parts),
            own: machine.and_then(|m| span(m.min(last), last)),
            perturbed,
            machine: machine.and_then(point),
            behaves: behaves.and_then(point),
            end: point(last),
        }
    }
}

/// Where a run a folding row hides ended, across the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct End {
    pub at: f32,
    pub failed: bool,
}

/// What a folding row draws over a window for the runs it hides: a line
/// of how many run and how many have diverged, a mark where each ended,
/// and one where each one's failing program first behaves differently,
/// so the spread of a schedule's runs shows without a row for each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cluster {
    pub spread: Spread,
    pub ends: Vec<End>,
    pub behaves: Vec<f32>,
}

/// Where a run a folding row hides first differs, its last step, and
/// whether it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Life {
    parts: Option<u64>,
    last: u64,
    failed: bool,
}

/// The runs a folding row hides as one line: at each step, how many are
/// still running, how many of those have diverged from the run each
/// hangs under, and how many of the diverged ones failed. The line shows
/// runs that go through a window where none of them ends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spread {
    /// The runs the line stands for: those that have ended.
    pub count: usize,
    lives: Vec<Life>,
}

/// A stretch of the window, as fractions of its width, over which the
/// same number of a folding row's runs run, have diverged, and of those
/// failed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stretch {
    pub from: f32,
    pub to: f32,
    pub running: usize,
    pub diverged: usize,
    pub failed: usize,
}

impl Stretch {
    /// Whether `other` counts the same runs as this stretch.
    fn counts_as(&self, other: &Stretch) -> bool {
        (self.running, self.diverged, self.failed) == (other.running, other.diverged, other.failed)
    }
}

impl Spread {
    /// The stretch from `from` to `to` counted at `step`.
    fn counted(&self, from: f32, to: f32, step: u64) -> Stretch {
        let mut stretch = Stretch {
            from,
            to,
            running: 0,
            diverged: 0,
            failed: 0,
        };
        for life in self.lives.iter().filter(|l| step <= l.last) {
            stretch.running += 1;
            if !life.parts.is_some_and(|p| p <= step) {
                continue;
            }
            stretch.diverged += 1;
            stretch.failed += usize::from(life.failed);
        }
        stretch
    }

    /// The stretches of `window`, split into `columns` columns of equal
    /// width, each counted at its middle step; adjacent columns with
    /// the same counts join, and columns where no run is running are
    /// left out.
    pub fn across(&self, window: Window, columns: usize) -> Vec<Stretch> {
        let mut stretches: Vec<Stretch> = Vec::new();
        let width = columns as f32;
        for column in 0..columns {
            let (from, to) = (column as f32 / width, (column + 1) as f32 / width);
            let step = window.step_across((column as f32 + 0.5) / width);
            let stretch = self.counted(from, to, step);

            // Carry on the stretch before when the counts hold.
            if let Some(before) = stretches.last_mut()
                && before.to == from
                && before.counts_as(&stretch)
            {
                before.to = to;
                continue;
            }
            if stretch.running == 0 {
                continue;
            }
            stretches.push(stretch);
        }
        stretches
    }
}

impl Cluster {
    /// The marks of `runs` over `window`, with what is known of where
    /// each parts from `parted`. A run that has not ended, and a mark
    /// outside the window, draw nothing.
    pub fn of<'a>(
        runs: impl IntoIterator<Item = &'a RunEntry>,
        parted: impl Fn(&RunEntry) -> Parted,
        window: Window,
    ) -> Cluster {
        let mut cluster = Cluster::default();
        for run in runs {
            let Some(last) = run.last_step else {
                continue;
            };

            // Where the run first differs, as its own track has it: a run
            // with nothing to part from is its own from step 0, and one
            // not yet compared has what its manifest says, if anything.
            let parts = match parted(run) {
                Parted::Root => Some(0),
                Parted::Known(p) => p.machine,
                Parted::Unknown => run.first_difference,
            };
            cluster.spread.lives.push(Life {
                parts,
                last,
                failed: run.failed,
            });
            cluster.spread.count += 1;

            if let Some(at) = window.at(last) {
                cluster.ends.push(End {
                    at,
                    failed: run.failed,
                });
            }
            let behaves = match parted(run) {
                Parted::Known(p) => p.behaves,
                Parted::Root | Parted::Unknown => None,
            };
            cluster
                .behaves
                .extend(behaves.and_then(|step| window.at(step)));
        }
        cluster
    }
}

/// Whether `window` shows every step of runs whose longest ends at `last`.
pub fn shows_every_step(window: Window, last: u64) -> bool {
    window.from() == 0 && window.to() >= last
}

/// What the step axis's playhead link does to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToPlayhead {
    /// Centers it on the playhead at its width.
    Center(Window),
    /// Zooms in around the playhead: a window that shows every step
    /// cannot move, so centering it would do nothing.
    Zoom(Window),
}

impl ToPlayhead {
    /// The link's move for `window` over runs whose longest ends at
    /// `last`, with the playhead at `step`; a zoom in narrows the window
    /// to `zoomed_half` steps either side.
    pub fn of(window: Window, step: u64, last: u64, zoomed_half: u64) -> ToPlayhead {
        if shows_every_step(window, last) {
            return ToPlayhead::Zoom(Window::around(step, zoomed_half.min(window.half), last));
        }
        ToPlayhead::Center(Window::around(step, window.half, last))
    }

    /// The window the link moves to.
    pub fn window(self) -> Window {
        match self {
            ToPlayhead::Center(w) | ToPlayhead::Zoom(w) => w,
        }
    }

    /// The link's words.
    pub fn words(self) -> &'static str {
        match self {
            ToPlayhead::Center(_) => "Center on playhead",
            ToPlayhead::Zoom(_) => "Zoom to playhead",
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
            parts_from: None,
            trace_hash: Some(format!("hash-{id}")),
            progress: Progress::Ended,
            shares: None,
            cores: 1,
            window: None,
            perturbs: None,
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
    fn a_run_is_thin_until_its_machine_differs_then_thick_to_its_end() {
        // A run whose machine first differs at step 40 and ends at 90:
        // thin 0 to 40, thick 40 to 90, marks at 40 and 90.
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
    fn the_first_run_is_thick_from_step_0_and_an_identical_run_thin_to_its_end() {
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

        // A narrowed run with nothing to measure against, as the pair of
        // an export has in the browser, still shows its perturbed steps.
        let mut narrowed = run("w", None, 8);
        narrowed.window = Some((30, 45));
        narrowed.perturbs = Some((30, 45));
        let alone = Track::of(&narrowed, Parted::Root, window());
        assert_eq!(
            alone.perturbed,
            Some(Span {
                from: at(30),
                to: at(45)
            })
        );

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
        narrowed.perturbs = Some((30, 45));
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
    fn a_whole_schedule_and_a_fork_show_the_steps_they_perturb() {
        // A run from boot under schedule 3 perturbs from where the job
        // started, step 14, to its end; a fork from its fork step to its
        // end. A schedule 0 run perturbs nothing.
        let mut whole = run("c", None, 3);
        whole.perturbs = Some((14, u64::MAX));
        let track = Track::of(&whole, known(Some(PARTS), None), window());
        assert_eq!(
            track.perturbed,
            Some(Span {
                from: at(14),
                to: at(LAST)
            })
        );

        let mut fork = run("f", Some("base"), 1);
        fork.perturbs = Some((FORK_STEP, u64::MAX));
        let track = Track::of(&fork, known(Some(PARTS), None), window());
        assert_eq!(track.perturbed.map(|s| s.from), Some(at(FORK_STEP)));

        let base = Track::of(&run("base", None, 0), Parted::Root, window());
        assert_eq!(base.perturbed, None);
    }

    #[test]
    fn the_playhead_link_centers_the_window_or_zooms_in_when_it_shows_everything() {
        // Over part of the runs the window centers on the playhead at its
        // width; over all of them centering cannot move it, so the link
        // zooms in around the playhead instead.
        let last = 1000;
        let part = Window::around(100, 50, last);
        assert_eq!(
            ToPlayhead::of(part, 600, last, 20),
            ToPlayhead::Center(Window::around(600, 50, last))
        );
        let whole = Window::around(0, 500, last);
        assert_eq!(
            ToPlayhead::of(whole, 600, last, 20),
            ToPlayhead::Zoom(Window::around(600, 20, last))
        );
        assert!(shows_every_step(whole, last));
        assert!(!shows_every_step(part, last));
    }

    #[test]
    fn a_folding_row_marks_where_each_run_it_hides_ends_and_its_program_differs() {
        // Two hidden runs: one passed and ended at 60, one failed and
        // ended at 90 with its program differing at 70. A run past the
        // window, and one that has not ended, draw nothing.
        let mut passed = run("p", None, 3);
        passed.last_step = Some(60);
        let mut failed = run("f", None, 4);
        failed.failed = true;
        let mut later = run("l", None, 5);
        later.last_step = Some(500);
        let mut running = run("r", None, 6);
        running.last_step = None;
        let parted = |r: &RunEntry| match r.id.as_str() {
            "f" => known(Some(PARTS), Some(70)),
            _ => known(Some(PARTS), None),
        };
        let cluster = Cluster::of([&passed, &failed, &later, &running], parted, window());
        assert_eq!(
            cluster.ends,
            vec![
                End {
                    at: at(60),
                    failed: false
                },
                End {
                    at: at(LAST),
                    failed: true
                }
            ]
        );
        assert_eq!(cluster.behaves, vec![at(70)]);
    }

    #[test]
    fn a_folding_row_draws_how_many_of_its_runs_run_and_have_diverged() {
        // Three hidden runs: a parts at 20 and ends at 90, b fails,
        // parting at 40 and ending at 50, and c, not yet compared, ends
        // at 80. Across steps 0 to 100, a column a step wide, the counts
        // change where a run parts and after a run's last step, b counts
        // among the diverged runs that failed while it runs, and past
        // every run's end nothing is drawn.
        let a = run("a", None, 3);
        let mut b = run("b", None, 4);
        b.last_step = Some(50);
        b.failed = true;
        let mut c = run("c", None, 5);
        c.last_step = Some(80);
        let parted = |r: &RunEntry| match r.id.as_str() {
            "a" => known(Some(20), None),
            "b" => known(Some(PARTS), None),
            _ => Parted::Unknown,
        };
        let spread = Cluster::of([&a, &b, &c], parted, window()).spread;
        assert_eq!(spread.count, 3);
        let counts = |spread: &Spread| -> Vec<(f32, f32, usize, usize, usize)> {
            spread
                .across(window(), 101)
                .iter()
                .map(|s| (s.from, s.to, s.running, s.diverged, s.failed))
                .collect()
        };
        assert_eq!(
            counts(&spread),
            vec![
                (at(0), at(20), 3, 0, 0),
                (at(20), at(PARTS), 3, 1, 0),
                (at(PARTS), at(51), 3, 2, 1),
                (at(51), at(81), 2, 1, 0),
                (at(81), at(91), 1, 1, 0),
            ]
        );

        // A run with nothing to part from is its own from step 0.
        let root = Cluster::of([&a], |_| Parted::Root, window()).spread;
        assert_eq!(counts(&root), vec![(at(0), at(91), 1, 1, 0)]);
    }

    #[test]
    fn what_falls_outside_the_window_is_clipped_or_left_out() {
        // Over steps 50 to 150 a run that parts at 40 and ends at 90 is
        // thick from the left edge; neither its thin steps nor the mark
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
        fork.failed = true;
        let mut check = run("check3", None, 3);
        check.last_step = Some(32);
        check.failed = true;
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
