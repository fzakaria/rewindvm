//! Numbers for the app's requests to the engine and its other work in the
//! background.
//!
//! An answer comes back on the UI thread whenever the work is done, by
//! which time whatever asked may have asked again, been closed and opened
//! anew, or be showing another run. Each request takes a number no other
//! request in the window has had, and an answer is applied only while the
//! number it carries is still the one its asker waits on.

/// One request's number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    // Request numbers as panels take them, with no UI.
    use super::*;

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
