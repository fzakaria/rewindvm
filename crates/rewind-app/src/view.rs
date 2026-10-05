//! The stretch of a run the timeline shows: the whole run, or a window of
//! it zoomed in on, so a divergence that is one pixel wide at 660 steps
//! per pixel can be seen step by step.

use crate::model::tick_interval;

/// The fewest steps a zoomed timeline shows.
pub const MIN_SPAN: u64 = 16;

/// How much one notch of the wheel, or one press of + or -, zooms.
pub const ZOOM_IN: f64 = 0.8;
pub const ZOOM_OUT: f64 = 1.0 / ZOOM_IN;

/// The steps from `lo` to `hi` the timeline spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub lo: u64,
    pub hi: u64,
}

impl View {
    /// The whole of a run of `total` steps.
    pub fn whole(total: u64) -> View {
        View { lo: 0, hi: total }
    }

    pub fn span(self) -> u64 {
        self.hi - self.lo
    }

    pub fn is_whole(self, total: u64) -> bool {
        self.lo == 0 && self.hi >= total
    }

    pub fn contains(self, step: u64) -> bool {
        (self.lo..=self.hi).contains(&step)
    }

    /// Where `step` sits along the timeline, 0 at its left end and 1 at
    /// its right; outside that for a step outside the view.
    pub fn fraction_of(self, step: u64) -> f32 {
        if self.span() == 0 {
            return 0.0;
        }
        ((step as f64 - self.lo as f64) / self.span() as f64) as f32
    }

    /// The step under a point `fraction` of the way along the timeline.
    pub fn step_at(self, fraction: f32) -> u64 {
        let fraction = fraction.clamp(0.0, 1.0) as f64;
        self.lo + (fraction * self.span() as f64).round() as u64
    }

    /// The view zoomed by `factor`, below 1 to zoom in, keeping the step
    /// under `fraction` where it is, within a run of `total` steps. It
    /// spans no fewer than MIN_SPAN steps and no more than the run.
    pub fn zoomed(self, fraction: f32, factor: f64, total: u64) -> View {
        let anchor = self.step_at(fraction) as f64;
        let span = (self.span() as f64 * factor)
            .round()
            .clamp(MIN_SPAN.min(total) as f64, total as f64) as u64;
        let lo = (anchor - fraction.clamp(0.0, 1.0) as f64 * span as f64).round();
        View::placed(lo, span, total)
    }

    /// The view moved by `fraction` of its span, right for a positive one,
    /// stopping at the ends of a run of `total` steps.
    pub fn panned(self, fraction: f32, total: u64) -> View {
        let lo = self.lo as f64 + fraction as f64 * self.span() as f64;
        View::placed(lo.round(), self.span(), total)
    }

    /// The view moved, keeping its span, so it shows `step` in its middle,
    /// as far as the ends of a run of `total` steps allow.
    pub fn centred_on(self, step: u64, total: u64) -> View {
        let lo = step as f64 - self.span() as f64 / 2.0;
        View::placed(lo.round(), self.span(), total)
    }

    /// A view `span` steps wide from `lo`, slid inside a run of `total`.
    fn placed(lo: f64, span: u64, total: u64) -> View {
        let span = span.min(total);
        let lo = (lo.max(0.0) as u64).min(total - span);
        View { lo, hi: lo + span }
    }

    /// The steps of about `target` tick marks strictly inside the view, at
    /// round numbers.
    pub fn ticks(self, target: u64) -> Vec<u64> {
        let interval = tick_interval(self.span(), target);
        let first = (self.lo / interval + 1) * interval;
        (0..)
            .map(|k| first + k * interval)
            .take_while(|step| *step < self.hi)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // Views over a run of 10,000 steps, zoomed, panned and recentred.
    use super::*;

    const TOTAL: u64 = 10_000;

    #[test]
    fn a_step_and_its_place_agree_in_and_out_of_a_zoom() {
        // The whole run maps step 2,500 a quarter of the way along; the
        // view from 2,000 to 3,000 maps it halfway, and a step outside the
        // view falls outside 0 to 1.
        let whole = View::whole(TOTAL);
        assert_eq!(whole.fraction_of(2_500), 0.25);
        assert_eq!(whole.step_at(0.25), 2_500);
        let part = View {
            lo: 2_000,
            hi: 3_000,
        };
        assert_eq!(part.fraction_of(2_500), 0.5);
        assert_eq!(part.step_at(0.5), 2_500);
        assert!(part.fraction_of(5_000) > 1.0);
        assert!(!part.contains(5_000));
    }

    #[test]
    fn zooming_keeps_the_step_under_the_pointer() {
        // Zooming in at a quarter of the way keeps step 2,500 there while
        // the span shrinks; zooming out past the run gives the whole run.
        let whole = View::whole(TOTAL);
        let zoomed = whole.zoomed(0.25, 0.5, TOTAL);
        assert_eq!(zoomed.span(), 5_000);
        assert_eq!(zoomed.step_at(0.25), 2_500);
        assert_eq!(zoomed.zoomed(0.25, 4.0, TOTAL), whole);
    }

    #[test]
    fn a_zoom_stops_at_the_fewest_steps_and_at_the_ends() {
        // Zooming far in stops at MIN_SPAN steps; zooming in near the end
        // keeps the view inside the run.
        let tiny = View::whole(TOTAL).zoomed(0.5, 0.000_001, TOTAL);
        assert_eq!(tiny.span(), MIN_SPAN);
        let at_end = View::whole(TOTAL).zoomed(1.0, 0.1, TOTAL);
        assert_eq!(at_end.hi, TOTAL);
        assert_eq!(at_end.span(), 1_000);
    }

    #[test]
    fn panning_and_recentring_stay_inside_the_run() {
        // Half a span right from 2,000..3,000 is 2,500..3,500; far left
        // stops at 0; centring on the last step puts the view at the end.
        let part = View {
            lo: 2_000,
            hi: 3_000,
        };
        assert_eq!(
            part.panned(0.5, TOTAL),
            View {
                lo: 2_500,
                hi: 3_500
            }
        );
        assert_eq!(part.panned(-10.0, TOTAL), View { lo: 0, hi: 1_000 });
        assert_eq!(
            part.centred_on(TOTAL, TOTAL),
            View {
                lo: 9_000,
                hi: TOTAL
            }
        );
        assert_eq!(
            part.centred_on(5_000, TOTAL),
            View {
                lo: 4_500,
                hi: 5_500
            }
        );
    }

    #[test]
    fn ticks_fall_on_round_steps_inside_the_view() {
        // About 10 ticks over 4,400..4,470 land every 10 steps, strictly
        // inside the view.
        let part = View {
            lo: 4_400,
            hi: 4_470,
        };
        assert_eq!(
            part.ticks(10),
            vec![4_410, 4_420, 4_430, 4_440, 4_450, 4_460]
        );
    }
}
