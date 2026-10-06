//! `rewind check`'s narrowing: given a schedule whose perturbation makes a
//! run end differently from schedule 0, the smallest window of steps it
//! can perturb and still end differently.
//!
//! Each step's perturbation depends only on the seed and the step, so a
//! smaller window perturbs a subset of the same steps, and two runs are
//! identical up to their window, so where they part is inside it, next to
//! the interleaving that matters. The window one step shorter ends like
//! schedule 0, and its run is the same as the window's until that last
//! step: the schedule's perturbation there decides how the run ends.

use anyhow::Result;

/// The window narrowing settled on, and the run made with it.
pub struct Narrowed<R> {
    pub from: u64,
    pub until: u64,
    pub run: R,
    /// The run over `from..until - 1`, which does not end differently and
    /// is the same run as `run` until step `until - 1`. None when the
    /// window is one step, since without it nothing is perturbed and the
    /// run is schedule 0's.
    pub without_last: Option<R>,
}

/// The window within `start..end` a schedule's perturbation narrows to
/// while its run still ends differently: first the latest start, then the
/// earliest end. The start comes first because in a long run a
/// perturbation anywhere early changes everything after it; the latest
/// start keeps the window near the end that differs.
///
/// `probe` runs the schedule once per window it is given, as one batch,
/// and returns the runs in the same order; `differs` says whether a run
/// ended differently. Each round tries `jobs` windows at once, so a round
/// divides the range by `jobs` + 1. `whole` is the run over all of
/// `start..end`, which is returned when nothing narrower ends differently.
pub fn narrow<R>(
    (start, end): (u64, u64),
    jobs: usize,
    whole: R,
    mut probe: impl FnMut(Vec<(u64, u64)>) -> Result<Vec<R>>,
    differs: impl Fn(&R) -> Result<bool>,
) -> Result<Narrowed<R>> {
    let mut found_run = whole;
    let points = |lo: u64, hi: u64| -> Vec<u64> {
        let n = (jobs as u64).min(hi - lo - 1).max(1);
        (1..=n).map(|i| lo + (hi - lo) * i / (n + 1)).collect()
    };

    // The latest start: `lo` differs, `hi` (an empty window) does not.
    let (mut lo, mut hi) = (start, end);
    while hi.saturating_sub(lo) > 1 {
        let starts = points(lo, hi);
        let runs = probe(starts.iter().map(|&f| (f, end)).collect())?;
        let mut found = None;
        for (f, run) in starts.iter().zip(runs).rev() {
            if differs(&run)? {
                found = Some((*f, run));
                break;
            }
        }
        match found {
            Some((f, run)) => {
                lo = f;
                hi = starts
                    .iter()
                    .copied()
                    .filter(|x| *x > f)
                    .min()
                    .unwrap_or(hi);
                found_run = run;
            }
            None => hi = starts[0],
        }
    }
    let from = lo;

    // The earliest end: `hi` differs, `lo` (an empty window, whose run is
    // schedule 0's and none of these) does not. `lo_run` is the run that
    // ends at `lo`, once one does.
    let (mut lo, mut hi) = (from, end);
    let mut lo_run = None;
    while hi.saturating_sub(lo) > 1 {
        let ends = points(lo, hi);
        let runs = probe(ends.iter().map(|&e| (from, e)).collect())?;
        let mut below = None;
        let mut found = None;
        for (e, run) in ends.iter().zip(runs) {
            if differs(&run)? {
                found = Some((*e, run));
                break;
            }
            below = Some((*e, run));
        }
        if let Some((e, run)) = below {
            lo = e;
            lo_run = Some(run);
        }
        if let Some((e, run)) = found {
            hi = e;
            found_run = run;
        }
    }
    Ok(Narrowed {
        from,
        until: hi,
        run: found_run,
        without_last: lo_run,
    })
}

#[cfg(test)]
mod tests {
    // Narrowing over made-up runs: a run is the window it perturbed, and
    // it ends differently exactly when that window holds the steps that
    // matter.
    use super::*;

    /// A run that ends differently when its window holds every step of
    /// `matters`: a race that needs a preemption at each of them.
    fn differs_if(matters: std::ops::Range<u64>) -> impl Fn(&(u64, u64)) -> Result<bool> {
        move |&(from, until)| Ok(from <= matters.start && matters.end <= until)
    }

    #[test]
    fn the_window_closes_on_the_steps_that_matter() {
        // From 100..10_000, with 1, 4 and 16 jobs, and one step or a few
        // that matter: the window ends up exactly those steps, the run
        // returned is the one made with it, and the run without the
        // window's last step is there too, unless that leaves no window.
        for jobs in [1, 4, 16] {
            for matters in [4_321..4_322, 7_000..7_013, 100..101, 9_999..10_000] {
                let mut probed = 0;
                let narrowed = narrow(
                    (100, 10_000),
                    jobs,
                    (100, 10_000),
                    |windows| {
                        probed += windows.len();
                        Ok(windows)
                    },
                    differs_if(matters.clone()),
                )
                .unwrap();
                assert_eq!(
                    (narrowed.from, narrowed.until),
                    (matters.start, matters.end),
                    "jobs {jobs}"
                );
                assert_eq!(narrowed.run, (matters.start, matters.end));
                let shorter =
                    (matters.end - 1 > matters.start).then_some((matters.start, matters.end - 1));
                assert_eq!(narrowed.without_last, shorter, "jobs {jobs}");
                assert!(probed < 200, "{probed} runs for jobs {jobs}");
            }
        }
    }

    #[test]
    fn a_window_too_small_to_narrow_is_kept() {
        // A range of one step has nothing to narrow: no run is made and the
        // whole range comes back with the run over it.
        let narrowed = narrow(
            (50, 51),
            4,
            "whole",
            |_| panic!("nothing to narrow"),
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(
            (narrowed.from, narrowed.until, narrowed.run),
            (50, 51, "whole")
        );
        assert_eq!(narrowed.without_last, None);
    }
}
