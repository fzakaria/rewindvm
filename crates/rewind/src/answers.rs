//! `rewind export --with-answers`: what a replay finds at the steps a
//! reader of the export will look at first, packed into the export for a
//! reader with no machine to replay on (see [`rewind_trace::answers`]).
//!
//! The steps are the one the run failed on, each assertion the job broke,
//! and each bookmark. At each, every live thread of the job is located as
//! `rewind where --pid --tid` locates it, and the files the job had
//! written by then are read in one fork, as `rewind diff --outputs` reads
//! them.

use anyhow::Result;
use rewind_core::inspect::{self, Inspection};
use rewind_core::{Home, Run};
use rewind_trace::Trace;
use rewind_trace::answers::{self, Gathered, LIMITS, LeftOutWhy, Packed, Place, ThreadAnswer, Why};
use rewind_trace::manifest::BOOKMARKS;
use rewind_trace::properties::{Glob, written_files};
use rewind_trace::summary::Recording;

use crate::color;
use crate::locate;

/// The glob every written file matches.
const EVERY_FILE: &str = "**";

/// What a replay of `run` finds at the steps worth answering, packed
/// within the export's limits. A thread `rewind where` cannot locate is
/// answered with why, and a read of files that fails leaves them out with
/// why, so the export says what it lacks; each is also said on standard
/// error.
pub(crate) fn gather(home: &Home, run: &Run) -> Result<Packed> {
    let trace = run.trace()?;
    let recording = Recording::of(&run.manifest, trace);
    let bookmarks = std::fs::read(run.dir.join(BOOKMARKS)).ok();
    let steps = answers::steps_to_answer(
        recording.failure().map(|f| f.step),
        run.manifest.verdict.failed_assertions(),
        bookmarks.as_deref(),
        recording.end(),
    );
    let paint = color::stderr();
    let mut gathered = Vec::with_capacity(steps.len());
    for (step, why) in steps {
        eprintln!(
            "{} answering step {step}: {}",
            paint.prefix(),
            why_words(&why)
        );
        let threads = answers::live_threads(trace, step)
            .into_iter()
            .map(|thread| thread_answer(home, run, step, thread))
            .collect();
        let files = files_at(home, run, trace, step);
        gathered.push(Gathered {
            step,
            why,
            threads,
            files,
        });
    }
    Ok(answers::pack(gathered, LIMITS))
}

/// Why a step is answered, in words for the progress line.
fn why_words(why: &[Why]) -> String {
    why.iter()
        .map(|why| match why {
            Why::Failure => "the run failed here".to_string(),
            Why::Assertion { text } => text.clone(),
            Why::Bookmark { note } => format!("bookmark \"{note}\""),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Where thread `tid` of process `pid` was at `step`, or why `rewind
/// where` could not say.
fn thread_answer(home: &Home, run: &Run, step: u64, (pid, tid): (u32, u32)) -> ThreadAnswer {
    let place = match locate::located(home, run, step, (Some(pid), Some(tid))) {
        Ok(located) => Place::Located {
            located: Box::new(located),
        },
        Err(e) => {
            let error = format!("{e:#}");
            eprintln!(
                "{} thread {tid} of process {pid} at step {step}: {error}",
                color::stderr().prefix()
            );
            Place::Failed { error }
        }
    };
    ThreadAnswer { pid, tid, place }
}

/// Each file the job had written by `step` and left in place, with its
/// bytes at the step, or why the export has none of them. A read that
/// fails says why, there and on standard error.
fn files_at(
    home: &Home,
    run: &Run,
    trace: &Trace,
    step: u64,
) -> Vec<(String, Result<Vec<u8>, LeftOutWhy>)> {
    let by_then = Trace {
        events: trace.until(step).to_vec(),
        switches: Vec::new(),
    };
    let paths = written_files(&by_then, &Glob::new(EVERY_FILE));
    if paths.is_empty() {
        return Vec::new();
    }
    let read = match inspect::files(home, run, step, None, &paths) {
        Ok(Inspection::Contents(bytes)) => rewind_init::sections(&bytes)
            .map(|found| {
                found
                    .into_iter()
                    .map(|(path, bytes)| (path, bytes.to_vec()))
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| "the VM's answer is not in sections".to_string()),
        Ok(Inspection::NotFound(message) | Inspection::Failed(message)) => Err(message),
        Err(e) => Err(format!("{e:#}")),
    };
    if let Err(why) = &read {
        eprintln!(
            "{} reading the job's files at step {step}: {why}",
            color::stderr().prefix()
        );
    }
    files_read(paths, read)
}

/// Each of `paths` with its bytes among the files `read` holds, the ones
/// the VM sent, or why the export has none of them: the VM sent none of
/// the file, or the read failed, for the reason `read` gives.
fn files_read(
    paths: Vec<String>,
    read: Result<Vec<(String, Vec<u8>)>, String>,
) -> Vec<(String, Result<Vec<u8>, LeftOutWhy>)> {
    let sent = match read {
        Ok(sent) => sent,
        Err(error) => {
            let why = LeftOutWhy::ReadFailed { error };
            return paths
                .into_iter()
                .map(|path| (path, Err(why.clone())))
                .collect();
        }
    };
    paths
        .into_iter()
        .map(|path| {
            let bytes = sent
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, b)| b.clone())
                .ok_or(LeftOutWhy::NotRead);
            (path, bytes)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Matching the files a read of the job's files sent to the files the
    // job wrote, from reads made up by hand.
    use super::*;

    #[test]
    fn a_failed_read_leaves_each_file_out_with_its_error() {
        // A read that sent one of two files: the other is left out as
        // not sent. A read that failed: both are left out with its error.
        let paths = vec!["/b/out.log".to_string(), "/b/gone".to_string()];
        let sent = vec![("/b/out.log".to_string(), b"ok\n".to_vec())];
        assert_eq!(
            files_read(paths.clone(), Ok(sent)),
            [
                ("/b/out.log".to_string(), Ok(b"ok\n".to_vec())),
                ("/b/gone".to_string(), Err(LeftOutWhy::NotRead)),
            ]
        );
        let failed = LeftOutWhy::ReadFailed {
            error: "the fork did not answer".into(),
        };
        assert_eq!(
            files_read(paths, Err("the fork did not answer".into())),
            [
                ("/b/out.log".to_string(), Err(failed.clone())),
                ("/b/gone".to_string(), Err(failed)),
            ]
        );
    }
}
