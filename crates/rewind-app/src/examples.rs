//! The example runs compiled into the app, so that someone without KVM or
//! the engine can open a real run and take the tour.
//!
//! Both are trace-only exports of the tutorial's mylib build
//! (docs/tutorial-nix.md): one where test_pool_shutdown crashes under a
//! perturbed thread schedule, and the passing build it is compared with.

use anyhow::Result;

use crate::run::{Run, Session};

/// The failing build: make check exits 2 after a SIGSEGV in a worker.
pub const FAILING: &[u8] = include_bytes!("../examples/runs/mylib-fail.rwd");

/// The same build under the default schedule, where every test passes.
pub const PASSING: &[u8] = include_bytes!("../examples/runs/mylib-pass.rwd");

/// Unpacks both examples into the cache and opens the failing one
/// compared with the passing one.
pub fn open() -> Result<Session> {
    let failing = Run::open_example(FAILING)?;
    let passing = Run::open_example(PASSING)?;
    Ok(Session::new(failing, Some(passing)))
}

#[cfg(test)]
mod tests {
    // The compiled-in examples, unpacked into a temporary cache directory
    // and indexed as the app would show them.
    use super::*;
    use crate::model::{FailureKind, signo};
    use crate::run::{Origin, Verdict};

    #[test]
    fn the_examples_show_a_crash_and_where_the_runs_part() {
        // The failing run fails with a SIGSEGV, the passing run passes,
        // and the two first differ before the crash.
        crate::archive::test_cache();
        let session = open().unwrap();

        let failure = session.run.timeline.failure.unwrap();
        assert!(matches!(
            failure.kind,
            FailureKind::Signal {
                signo: signo::SIGSEGV,
                ..
            }
        ));
        assert_eq!(session.run.verdict(), Verdict::Failed);
        assert_eq!(session.run.origin, Origin::Example);
        let passing = session.other.as_ref().unwrap();
        assert_eq!(passing.verdict(), Verdict::Passed);
        let divergence = session.divergence_step().unwrap();
        assert!(divergence < failure.step);

        // In words: the first thing test_pool_shutdown does differently.
        let Some(crate::run::Agreement::Parted { lines, .. }) = session.agreement() else {
            panic!("the examples do not part");
        };
        assert_eq!(
            lines,
            vec![
                "test_pool_shutdown did the same things in the same order in both runs until step 4,256.",
                "Then in this run, thread 2 of test_pool_shutdown writes \"job 7 done: 5785\" to stdout.",
                "In the passing run, thread 3 of test_pool_shutdown writes \"job 6 done: 13750\" to stdout.",
                "Both write to the same stream; the text differs.",
            ]
        );
    }
}
