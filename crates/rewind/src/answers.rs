//! `rewind export --with-answers`: what a replay finds at the steps a
//! reader of the export will look at first, packed into the export for a
//! reader with no machine to replay on (see [`rewind_trace::answers`]).
//!
//! The steps are the one the run failed on, each assertion the job broke,
//! and each bookmark. At each, every live thread of the job is located as
//! `rewind where --pid --tid` locates it, the threads of one process from
//! one fork and one gdb session, and the files the job had written by
//! then are read in one fork, as `rewind diff --outputs` reads them. The
//! processes and the files of every step are answered side by side, as
//! many at once as the machine has cores and memory for, and put back in
//! step order, so the export's bytes do not depend on which finished
//! first.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use rewind_core::inspect::{self, Inspection};
use rewind_core::{Home, Run};
use rewind_trace::Trace;
use rewind_trace::answers::{self, Gathered, LIMITS, LeftOutWhy, Packed, Place, ThreadAnswer, Why};
use rewind_trace::manifest::BOOKMARKS;
use rewind_trace::properties::{Glob, written_files};
use rewind_trace::summary::Recording;

use crate::color;
use crate::gdb::Server;
use crate::locate;

/// The glob every written file matches.
const EVERY_FILE: &str = "**";

/// The variable that sets how many answers are worked on at once, in place
/// of the number the machine's cores and memory allow.
const WORKERS_ENV: &str = "REWIND_ANSWER_WORKERS";

/// What answering one process or one step's files takes of the machine: a
/// core of the VM's and a core of gdb's, and a GiB of memory, about 0.3
/// for the VM and up to 0.5 for gdb.
const CORES_PER_WORKER: usize = 2;
const BYTES_PER_WORKER: u64 = 1 << 30;

/// Where Linux says how much memory is free to use, and the line that says
/// it, in KiB.
const MEMINFO: &str = "/proc/meminfo";
const MEM_AVAILABLE: &str = "MemAvailable:";
const KIB: u64 = 1024;

/// One piece of the answers at a step, worked on by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Unit {
    /// Where the threads `tids` of process `pid` were.
    Threads { step: u64, pid: u32, tids: Vec<u32> },
    /// The files the job had written by the step.
    Files { step: u64 },
}

/// What one unit found.
enum Found {
    Threads(Vec<ThreadAnswer>),
    Files(Vec<(String, Result<Vec<u8>, LeftOutWhy>)>),
}

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

    // Each step's processes and its files, worked on side by side, with
    // one debuginfod server for every gdb.
    let live: Vec<(u64, Vec<(u32, u32)>)> = steps
        .keys()
        .map(|&step| (step, answers::live_threads(trace, step)))
        .collect();
    let units = units_of(&live);
    let workers = workers(units.len());
    let server = Server::shared(home);
    let paint = color::stderr();
    eprintln!(
        "{} answering {} steps, {} at a time",
        paint.prefix(),
        steps.len(),
        workers
    );
    let found = in_parallel(&units, workers, |unit| {
        let step = match unit {
            Unit::Threads { step, .. } | Unit::Files { step } => *step,
        };
        let why = steps.get(&step).map(|w| why_words(w)).unwrap_or_default();
        match unit {
            Unit::Threads { step, pid, tids } => {
                eprintln!(
                    "{} answering step {step} ({why}): process {pid}",
                    paint.prefix()
                );
                Found::Threads(thread_answers(home, run, *step, (*pid, tids), &server))
            }
            Unit::Files { step } => {
                eprintln!(
                    "{} answering step {step} ({why}): the job's files",
                    paint.prefix()
                );
                Found::Files(files_at(home, run, trace, *step))
            }
        }
    });

    // Each step's answers put back together in step order, its processes
    // in the order they were listed.
    let mut gathered: Vec<Gathered> = steps
        .into_iter()
        .map(|(step, why)| Gathered {
            step,
            why,
            threads: Vec::new(),
            files: Vec::new(),
        })
        .collect();
    for (unit, found) in units.iter().zip(found) {
        let step = match unit {
            Unit::Threads { step, .. } | Unit::Files { step } => *step,
        };
        let Some(at) = gathered.iter_mut().find(|g| g.step == step) else {
            continue;
        };
        match found {
            Found::Threads(mut threads) => at.threads.append(&mut threads),
            Found::Files(files) => at.files = files,
        }
    }
    Ok(answers::pack(gathered, LIMITS))
}

/// The units the answers at each step of `live`, with the threads alive
/// there, are worked on in: the threads of each process, each run of one
/// process's threads in the list its own, then the step's files.
fn units_of(live: &[(u64, Vec<(u32, u32)>)]) -> Vec<Unit> {
    let mut units = Vec::new();
    for (step, threads) in live {
        for run in threads.chunk_by(|a, b| a.0 == b.0) {
            units.push(Unit::Threads {
                step: *step,
                pid: run[0].0,
                tids: run.iter().map(|&(_, tid)| tid).collect(),
            });
        }
        units.push(Unit::Files { step: *step });
    }
    units
}

/// How many of `units` units to work on at once: the number
/// [`WORKERS_ENV`] gives, else one for every two cores, no more than the
/// GiB of memory free, and at least one.
fn workers(units: usize) -> usize {
    let asked = std::env::var(WORKERS_ENV)
        .ok()
        .and_then(|n| n.parse::<usize>().ok());
    let workers = asked.unwrap_or_else(|| {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let by_cores = cores / CORES_PER_WORKER;
        match available_memory() {
            Some(bytes) => by_cores.min((bytes / BYTES_PER_WORKER) as usize),
            None => by_cores,
        }
    });
    workers.clamp(1, units.max(1))
}

/// The bytes of memory Linux says are free to use, when it says.
fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string(MEMINFO).ok()?;
    let line = meminfo.lines().find(|l| l.starts_with(MEM_AVAILABLE))?;
    let kib: u64 = line
        .trim_start_matches(MEM_AVAILABLE)
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * KIB)
}

/// `work` done on each of `items`, by `workers` threads that each take the
/// next item not yet taken, the results in the order of `items` whichever
/// finished first.
fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else {
                        break;
                    };
                    let result = work(item);
                    results.lock().expect("a worker panicked")[i] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("a worker panicked")
        .into_iter()
        .map(|r| r.expect("every item is worked on"))
        .collect()
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

/// Where each of threads `tids` of process `pid` was at `step`, or why
/// `rewind where` could not say, from one fork and one gdb session that
/// asks `server`.
fn thread_answers(
    home: &Home,
    run: &Run,
    step: u64,
    (pid, tids): (u32, &[u32]),
    server: &Server,
) -> Vec<ThreadAnswer> {
    let failed = |tid: u32, error: String| {
        eprintln!(
            "{} thread {tid} of process {pid} at step {step}: {error}",
            color::stderr().prefix()
        );
        Place::Failed { error }
    };
    let mut located = match locate::located_threads(home, run, step, (pid, tids), server.clone()) {
        Ok(located) => located,
        Err(e) => {
            let error = format!("{e:#}");
            return tids
                .iter()
                .map(|&tid| ThreadAnswer {
                    pid,
                    tid,
                    place: failed(tid, error.clone()),
                })
                .collect();
        }
    };
    tids.iter()
        .map(|&tid| {
            let found = located
                .iter()
                .position(|(t, _)| *t == tid)
                .map(|i| located.swap_remove(i).1);
            let place = match found {
                Some(Ok(located)) => Place::Located {
                    located: Box::new(located),
                },
                Some(Err(e)) => failed(tid, format!("{e:#}")),
                None => failed(tid, format!("gdb did not walk thread {tid}")),
            };
            ThreadAnswer { pid, tid, place }
        })
        .collect()
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
    // job wrote, from reads made up by hand; splitting the answers into
    // units by process and step; and working on units side by side.
    use super::*;

    #[test]
    fn each_process_and_each_steps_files_is_a_unit() {
        // Step 5 has process 7's threads 7 and 8 and process 10's thread
        // 10; step 9 has no thread. Each process is a unit, in the order
        // listed, then the step's files.
        let live = vec![(5, vec![(7, 7), (7, 8), (10, 10)]), (9, Vec::new())];
        assert_eq!(
            units_of(&live),
            [
                Unit::Threads {
                    step: 5,
                    pid: 7,
                    tids: vec![7, 8]
                },
                Unit::Threads {
                    step: 5,
                    pid: 10,
                    tids: vec![10]
                },
                Unit::Files { step: 5 },
                Unit::Files { step: 9 },
            ]
        );
    }

    #[test]
    fn parallel_work_comes_back_in_the_items_order() {
        // Items that take longer the earlier they are, on four workers
        // and on one: each is worked on once, and the results are in the
        // items' order, not the order they finished in.
        let items: Vec<u64> = (0..12).collect();
        let ran = AtomicUsize::new(0);
        let work = |&i: &u64| {
            ran.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(12 - i));
            i * 10
        };
        let expected: Vec<u64> = items.iter().map(|i| i * 10).collect();
        assert_eq!(in_parallel(&items, 4, work), expected);
        assert_eq!(in_parallel(&items, 1, work), expected);
        assert_eq!(ran.load(Ordering::Relaxed), 2 * items.len());
        assert!(in_parallel(&Vec::<u64>::new(), 4, work).is_empty());
    }

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
