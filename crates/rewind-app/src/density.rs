//! How densely the guest switched tasks along the timeline: the share of
//! each bucket's steps that were a context switch, which the band under
//! the track draws darker where threads handed the CPU back and forth and
//! lighter where one task ran alone.

use rewind_trace::switches::Switch;

use crate::view::View;

/// The context switches of a view, counted into equal buckets.
#[derive(Clone, Debug, PartialEq)]
pub struct Density {
    pub view: View,
    /// The share, 0 to 1, of each bucket's steps that were a switch.
    pub shares: Vec<f32>,
}

impl Density {
    /// Counts the switches of `switches` that fall in `view` into
    /// `buckets` equal buckets. A bucket's share is its switches over its
    /// steps, so a stretch where every step is a switch comes out 1.
    pub fn of(switches: &[Switch], view: View, buckets: usize) -> Density {
        let span = view.span();
        if buckets == 0 || span == 0 {
            return Density {
                view,
                shares: vec![0.0; buckets],
            };
        }

        // The switches inside the view, each into the bucket its step
        // falls in; the view's last step goes in the last bucket.
        let mut counts = vec![0u32; buckets];
        let first = switches.partition_point(|s| s.step < view.lo);
        let end = switches.partition_point(|s| s.step <= view.hi);
        let per_bucket = span as f64 / buckets as f64;
        for switch in &switches[first..end] {
            let i = ((switch.step - view.lo) as f64 / per_bucket) as usize;
            counts[i.min(buckets - 1)] += 1;
        }
        let shares = counts
            .into_iter()
            .map(|c| (c as f64 / per_bucket).min(1.0) as f32)
            .collect();
        Density { view, shares }
    }

    /// The steps bucket `i` covers, first and last.
    pub fn bucket_steps(&self, i: usize) -> (u64, u64) {
        let per_bucket = self.view.span() as f64 / self.shares.len() as f64;
        let lo = self.view.lo + (i as f64 * per_bucket) as u64;
        let hi = self.view.lo + ((i + 1) as f64 * per_bucket) as u64;
        (lo, hi.min(self.view.hi))
    }

    /// The bucket under a point `fraction` of the way along the band.
    pub fn bucket_at(&self, fraction: f32) -> usize {
        let i = (fraction.clamp(0.0, 1.0) * self.shares.len() as f32) as usize;
        i.min(self.shares.len().saturating_sub(1))
    }
}

/// How many of `switches` fall in steps `lo` to `hi`, inclusive.
pub fn switches_between(switches: &[Switch], lo: u64, hi: u64) -> usize {
    let first = switches.partition_point(|s| s.step < lo);
    let end = switches.partition_point(|s| s.step <= hi);
    end.saturating_sub(first)
}

#[cfg(test)]
mod tests {
    // Densities of a hand-built switch list over a run of 1,000 steps,
    // for the whole run and for a zoom.
    use super::*;

    fn switch(step: u64) -> Switch {
        Switch {
            step,
            pid: 5,
            tid: 5,
            memory: true,
            name: "five".into(),
        }
    }

    /// Switches at every step from 100 to 199, and one at 700.
    fn switches() -> Vec<Switch> {
        let mut all: Vec<Switch> = (100..200).map(switch).collect();
        all.push(switch(700));
        all
    }

    #[test]
    fn a_bucket_s_share_is_its_switches_over_its_steps() {
        // Ten buckets of 100 steps: the second is all switches, the
        // eighth has one in a hundred, the rest none.
        let d = Density::of(&switches(), View::whole(1_000), 10);
        assert_eq!(d.shares.len(), 10);
        assert_eq!(d.shares[0], 0.0);
        assert_eq!(d.shares[1], 1.0);
        assert!((d.shares[7] - 0.01).abs() < 1e-6);
        assert_eq!(d.shares[9], 0.0);
        assert_eq!(d.bucket_steps(1), (100, 200));
        assert_eq!(d.bucket_steps(9), (900, 1_000));
    }

    #[test]
    fn a_zoom_counts_only_the_switches_it_shows() {
        // Zoomed on 150 to 250 in four buckets of 25 steps: the first two
        // are all switches, the last two have none, and the switch at 700
        // is outside.
        let d = Density::of(&switches(), View { lo: 150, hi: 250 }, 4);
        assert_eq!(d.shares, vec![1.0, 1.0, 0.0, 0.0]);
        assert_eq!(d.bucket_at(0.0), 0);
        assert_eq!(d.bucket_at(0.6), 2);
        assert_eq!(d.bucket_at(1.0), 3);
        assert_eq!(switches_between(&switches(), 150, 250), 50);
        assert_eq!(switches_between(&switches(), 600, 800), 1);
    }

    #[test]
    fn a_share_never_passes_one_and_an_empty_view_is_all_zero() {
        // Two buckets over 100 to 102, where every step is a switch, both
        // come out 1 and not more; no buckets or no span gives zeros.
        let d = Density::of(&switches(), View { lo: 100, hi: 102 }, 2);
        assert_eq!(d.shares, vec![1.0, 1.0]);
        assert_eq!(
            Density::of(&switches(), View { lo: 5, hi: 5 }, 3).shares,
            vec![0.0; 3]
        );
        assert!(Density::of(&[], View::whole(1_000), 0).shares.is_empty());
    }
}
