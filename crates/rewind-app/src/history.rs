//! Where the playhead has been: the places it jumped from, so a jump to
//! the failure, the divergence or a clicked line can be undone and done
//! again, as a browser's Back and Forward do for pages.
//!
//! A place is a step, or a step with what else Back should bring back,
//! such as a pick: two picks on one step are two places.
//!
//! Only jumps are kept. Stepping by an event or a step, and dragging the
//! playhead, move it without a record; the next jump records where that
//! left it.

/// The most places kept on either side; older ones are dropped.
const MAX_KEPT: usize = 200;

/// The places behind and ahead of the playhead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History<P = u64> {
    /// The places jumped from, the latest last.
    back: Vec<P>,
    /// The places gone back from, the nearest last.
    forward: Vec<P>,
}

impl<P> Default for History<P> {
    fn default() -> Self {
        History {
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl<P: Copy + PartialEq> History<P> {
    /// Records a jump from `from` to `to`. A jump to where the playhead
    /// already is records nothing. A new jump drops the places ahead, as
    /// a new page does in a browser.
    pub fn jumped(&mut self, from: P, to: P) {
        if from == to {
            return;
        }
        if self.back.last() != Some(&from) {
            push_kept(&mut self.back, from);
        }
        self.forward.clear();
    }

    /// The place to go back to from `at`, if any, with `at` kept to come
    /// forward to again.
    pub fn back(&mut self, at: P) -> Option<P> {
        let to = pop_other_than(&mut self.back, at)?;
        push_kept(&mut self.forward, at);
        Some(to)
    }

    /// The place to go forward to from `at`, if any, with `at` kept to go
    /// back to again.
    pub fn forward(&mut self, at: P) -> Option<P> {
        let to = pop_other_than(&mut self.forward, at)?;
        push_kept(&mut self.back, at);
        Some(to)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

/// Pushes `place` onto `places`, dropping the oldest past `MAX_KEPT`.
fn push_kept<P>(places: &mut Vec<P>, place: P) {
    places.push(place);
    if places.len() > MAX_KEPT {
        places.remove(0);
    }
}

/// Pops the latest place that is not `at`: stepping can bring the
/// playhead back onto a place it jumped from, and going there would not
/// move it.
fn pop_other_than<P: PartialEq>(places: &mut Vec<P>, at: P) -> Option<P> {
    while let Some(place) = places.pop() {
        if place != at {
            return Some(place);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Jumps, backs and forwards on a History, checked against the steps a
    // browser's Back and Forward would visit.
    use super::*;

    #[test]
    fn back_and_forward_retrace_the_jumps() {
        // Jumps from 10 to the failure at 900, then to the divergence at
        // 400: Back goes to 900, then 10; Forward comes back to 900, 400.
        let mut h = History::default();
        h.jumped(10, 900);
        h.jumped(900, 400);
        assert_eq!(h.back(400), Some(900));
        assert_eq!(h.back(900), Some(10));
        assert_eq!(h.back(10), None);
        assert_eq!(h.forward(10), Some(900));
        assert_eq!(h.forward(900), Some(400));
        assert_eq!(h.forward(400), None);
    }

    #[test]
    fn stepping_after_a_jump_is_where_back_returns_from() {
        // A jump from 10 to 900, then stepping to 905: Back goes to 10, and
        // Forward returns to 905, where the stepping left the playhead.
        let mut h = History::default();
        h.jumped(10, 900);
        assert_eq!(h.back(905), Some(10));
        assert_eq!(h.forward(10), Some(905));
    }

    #[test]
    fn a_new_jump_drops_the_steps_ahead() {
        // Back from 900 to 10, then a jump to 50: there is no Forward to
        // 900 any more, and Back goes to 10.
        let mut h = History::default();
        h.jumped(10, 900);
        assert_eq!(h.back(900), Some(10));
        h.jumped(10, 50);
        assert!(!h.can_go_forward());
        assert_eq!(h.back(50), Some(10));
    }

    #[test]
    fn a_jump_in_place_and_steps_already_there_are_skipped() {
        // A jump to the step the playhead is on records nothing; Back from
        // a step the playhead stepped back onto skips it.
        let mut h = History::default();
        h.jumped(5, 5);
        assert!(!h.can_go_back());
        h.jumped(10, 900);
        h.jumped(900, 400);
        assert_eq!(h.back(900), Some(10));
    }

    #[test]
    fn two_places_on_one_step_are_two_entries() {
        // Places are a step and a pick: a pick of a process at 900, then
        // of its own part at the same step, then a jump to 400. Back goes
        // to the part, then to the process, then to where it started;
        // Forward retraces them.
        let mut h: History<(u64, u8)> = History::default();
        h.jumped((10, 0), (900, 1));
        h.jumped((900, 1), (900, 2));
        h.jumped((900, 2), (400, 2));
        assert_eq!(h.back((400, 2)), Some((900, 2)));
        assert_eq!(h.back((900, 2)), Some((900, 1)));
        assert_eq!(h.back((900, 1)), Some((10, 0)));
        assert_eq!(h.forward((10, 0)), Some((900, 1)));
        assert_eq!(h.forward((900, 1)), Some((900, 2)));
    }

    #[test]
    fn only_the_latest_jumps_are_kept() {
        // Past MAX_KEPT jumps, the oldest is dropped.
        let mut h = History::default();
        for step in 0..=MAX_KEPT as u64 {
            h.jumped(step, step + 1);
        }
        let mut at = MAX_KEPT as u64 + 1;
        let mut visited = 0;
        while let Some(step) = h.back(at) {
            at = step;
            visited += 1;
        }
        assert_eq!(visited, MAX_KEPT);
        assert_eq!(at, 1);
    }
}
