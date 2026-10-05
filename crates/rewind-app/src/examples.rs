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

/// The file `name` inside an example export.
#[cfg(test)]
fn file_of(export: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;
    let tar = zstd::decode_all(export).expect("an example is zstd");
    let mut archive = tar::Archive::new(&tar[..]);
    for entry in archive.entries().expect("an example is a tar") {
        let mut entry = entry.expect("an example's entries read");
        if entry.path().expect("an entry has a path").as_os_str() != name {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("the file reads");
        return bytes;
    }
    panic!("an example without {name}");
}

/// The trace inside an example export, for tests that need a real one.
#[cfg(test)]
pub fn trace_of(export: &[u8]) -> Vec<u8> {
    file_of(export, rewind_trace::manifest::TRACE)
}

/// The manifest inside an example export, for tests that need a real one
/// to change.
#[cfg(test)]
pub fn manifest_of(export: &[u8]) -> rewind_trace::manifest::Manifest {
    let bytes = file_of(export, rewind_trace::manifest::MANIFEST);
    serde_json::from_slice(&bytes).expect("an example's manifest reads")
}

/// Where [`trace_until`] cuts an example's trace.
#[cfg(test)]
#[derive(Clone, Copy)]
pub enum Until {
    /// Before init starts the job: boot alone, with nothing failing.
    JobStart,
    /// Before init reports the job's exit, as a run stopped at its time
    /// limit leaves it.
    JobExit,
}

/// An example's trace, cut off before `until`.
#[cfg(test)]
pub fn trace_until(export: &[u8], until: Until) -> Vec<u8> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CUTS: AtomicUsize = AtomicUsize::new(0);

    // The records are read from a file of this cut's own, since tests run
    // in parallel.
    let cut = CUTS.fetch_add(1, Ordering::Relaxed);
    let whole = std::env::temp_dir().join(format!("rewind-app-cut-{}-{cut}", std::process::id()));
    std::fs::write(&whole, trace_of(export)).unwrap();
    let records = rewind_trace::records(&whole).unwrap();
    std::fs::remove_file(&whole).unwrap();

    let trace = rewind_trace::Trace::decode(&records).unwrap();
    let end = match until {
        Until::JobStart => trace.job_start(),
        Until::JobExit => trace.job_exit().map(|exit| exit.step),
    }
    .expect("an example's job starts and exits");
    let mut out = rewind_trace::TraceWriter::new(Vec::new());
    for (step, record) in records.iter().filter(|(step, _)| *step < end) {
        out.record(*step, record).unwrap();
    }
    out.finish().unwrap()
}

#[cfg(test)]
mod tests {
    // The compiled-in examples, unpacked into a temporary cache directory
    // and indexed as the app would show them.
    use super::*;
    use crate::model::FailureKind;
    use crate::run::{Origin, Verdict};
    use rewind_trace::signal;

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
                signo: signal::SIGSEGV,
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
                "test_pool_shutdown did the same things in the same order in both runs until step 4,760.",
                "Then in this run, thread 9 of test_pool_shutdown writes \"job 1 done: 35269\" to stdout.",
                "In the passing run, thread 8 of test_pool_shutdown writes \"job 0 done: 12727\" to stdout.",
                "Both write to the same stream; the text differs.",
            ]
        );
    }
}
