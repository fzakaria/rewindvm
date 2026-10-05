//! Waiting, in tests, for a lock a test let go of to be free.
//!
//! Other tests in the same process start programs, and a child holds a
//! copy of every open file, and so every lock, from its fork until it
//! execs. A lock a test dropped can stay held that long, so a test that
//! checks a lock right after dropping it waits for the check to hold.

use std::time::Duration;

/// How often, and how far apart, `settle` tries again.
const TRIES: u32 = 200;
const WAIT: Duration = Duration::from_millis(10);

/// The first `Some` that `attempt` gives, trying again while it gives
/// None. `what` names the wait in the panic when it never ends.
pub(crate) fn settle<T>(what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
    for _ in 0..TRIES {
        if let Some(found) = attempt() {
            return found;
        }
        std::thread::sleep(WAIT);
    }
    panic!("{what} did not happen in {TRIES} tries");
}
