//! Numbers for the app's requests to the engine and its other work in the
//! background.
//!
//! An answer comes back on the UI thread whenever the work is done, by
//! which time whatever asked may have asked again, been closed and opened
//! anew, or be showing another run. Each request takes a number no other
//! request in the window has had, and an answer is applied only while the
//! number it carries is still the one its asker waits on.

/// One request's number: a later request's is greater.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Request(u64);

/// Where a window's requests get their numbers. It is never reset, so a
/// panel opened anew never waits on a number an earlier one used.
#[derive(Debug, Default)]
pub struct Requests {
    last: u64,
}

impl Requests {
    /// A number no earlier request had.
    pub fn issue(&mut self) -> Request {
        self.last += 1;
        Request(self.last)
    }
}

/// Why something kept in memory is read again in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reread {
    /// What is read changed, as the engine's runs do when one is deleted:
    /// a reading started before the change may not show it.
    Changed,
    /// A look for news every so often: a reading under way will do.
    Poll,
}

/// What to do with a reading that finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finished {
    /// Show what the reading found.
    Show,
    /// What was read changed while the reading ran: drop what the reading
    /// found and read again.
    ReadAgain,
    /// The reading is not the one waited on: drop what the reading found.
    Drop,
}

/// Readings of one thing in the background, one at a time, such as the
/// scan of the engine's runs. A reading asked for while one runs starts
/// none, so a poll cannot pile readings up; and a reading that a change
/// overtook is dropped and made again, so the reading shown last is never
/// one from before the change.
#[derive(Debug, Default)]
pub struct Readings {
    /// The reading under way.
    running: Option<Request>,
    /// Whether what is read changed since the reading under way started.
    overtaken: bool,
}

impl Readings {
    /// Asks for a reading. Returns the number to start one under, or None
    /// while one runs.
    pub fn ask(&mut self, why: Reread, requests: &mut Requests) -> Option<Request> {
        // The reading under way started before the change asked about.
        if self.running.is_some() {
            self.overtaken |= why == Reread::Changed;
            return None;
        }
        let request = requests.issue();
        self.running = Some(request);
        self.overtaken = false;
        Some(request)
    }

    /// The reading `request` finished: what to do with what it found.
    pub fn finished(&mut self, request: Request) -> Finished {
        if self.running != Some(request) {
            return Finished::Drop;
        }
        self.running = None;
        if std::mem::take(&mut self.overtaken) {
            return Finished::ReadAgain;
        }
        Finished::Show
    }
}

#[cfg(test)]
mod tests {
    // Request numbers as panels take them, with no UI.
    use super::*;

    #[test]
    fn readings_run_one_at_a_time_and_a_change_makes_another() {
        // A poll while a reading runs starts none, and the reading is
        // shown when it finishes. A change while a reading runs starts
        // none either, but the reading under way is dropped when it
        // finishes and a new one is asked for, which is then shown: the
        // reading from before a delete never lands after the delete. A
        // number no reading waits on is dropped.
        let mut requests = Requests::default();
        let mut readings = Readings::default();
        let first = readings.ask(Reread::Poll, &mut requests).unwrap();
        assert_eq!(readings.ask(Reread::Poll, &mut requests), None);
        assert_eq!(readings.finished(first), Finished::Show);
        assert_eq!(readings.finished(first), Finished::Drop);

        let before = readings.ask(Reread::Changed, &mut requests).unwrap();
        assert_eq!(readings.ask(Reread::Changed, &mut requests), None);
        assert_eq!(readings.ask(Reread::Poll, &mut requests), None);
        assert_eq!(readings.finished(before), Finished::ReadAgain);
        let after = readings.ask(Reread::Changed, &mut requests).unwrap();
        assert_ne!(before, after);
        assert_eq!(readings.finished(after), Finished::Show);
    }

    #[test]
    fn a_request_number_is_never_given_twice() {
        // Two panels in turn, each taking one number, as a file viewer
        // opened on one file and then another does: the second waits on a
        // number the first's answer does not carry.
        let mut requests = Requests::default();
        let first = requests.issue();
        let second = requests.issue();
        assert_ne!(first, second);
        assert_ne!(requests.issue(), first);
    }
}
