//! The step a run failed on, and how: the step the desktop app's failure
//! marker sits at and `rewind summary` names. The engine's verdict says
//! whether a run failed; this finds where in the trace it did.

use crate::ending::ExitStatus;
use crate::sdk::SdkMark;
use crate::stop::Stop;
use crate::{EventKind, JobExit, Process, Trace, signal};

/// What the failing event was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// A crash signal was delivered.
    Signal { signo: u32, addr: u64 },
    /// A process exited with a nonzero exit_code.
    Exit { status: u32 },
    /// The machine stopped before the job exited: at its time limit, or
    /// by a fault.
    Stopped,
    /// The job exited 0, but an assertion it made with the Rewind SDK did
    /// not hold: an `always` failed or an `unreachable` was reached.
    Assertion,
}

/// How a run's machine stopped, as far as finding its failure goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Any way but its guest powering off, such as at its time limit.
    Abnormally,
    /// By its guest powering off, or not known.
    Otherwise,
}

impl Stopped {
    /// How a machine that stopped as `stop` stopped; a run whose stop is
    /// not known stopped otherwise than abnormally.
    pub fn of(stop: Option<&Stop>) -> Stopped {
        match stop {
            Some(stop) if !stop.is_clean() => Stopped::Abnormally,
            _ => Stopped::Otherwise,
        }
    }
}

/// The step a run failed on, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    /// The index of the failing event in the trace.
    pub index: usize,
    pub kind: FailureKind,
}

/// Where the run failed, in a run of `total` steps whose machine stopped
/// as `stopped`. A job whose init reports status 0 failed only where an
/// assertion it made did not hold. Otherwise the first crash signal is
/// the failure, and without one, the start of the chain of nonzero exits
/// that ended the run: from the last process to exit nonzero, down
/// through the child it exited after. When a signal killed that child,
/// such as SIGPIPE, the failure is the signal reaching it rather than its
/// exit. A signal or exit before step `shared`, which a passing run made
/// too, is not the failure.
pub fn find(
    trace: &Trace,
    job_exit: Option<JobExit>,
    total: u64,
    stopped: Stopped,
    shared: Option<u64>,
) -> Option<Failure> {
    // A job that exited 0 failed only where an assertion it made did not
    // hold, as `rewind check`'s sdk property judges it.
    let made_too = |step: u64| shared.is_some_and(|s| step < s);
    if job_exit.is_some_and(|j| j.status == 0) {
        let until = job_exit.map_or(u64::MAX, |j| j.step);
        return trace.events.iter().enumerate().find_map(|(index, e)| {
            if e.step > until || made_too(e.step) {
                return None;
            }
            let broke = matches!(
                e.sdk_mark()?,
                SdkMark::Always { held: false, .. } | SdkMark::Unreachable { .. }
            );
            broke.then_some(Failure {
                step: e.step,
                pid: e.pid,
                tid: e.tid,
                index,
                kind: FailureKind::Assertion,
            })
        });
    }

    // Only what happened up to the job's exit, when init reported one:
    // after it, init tears down what the job left running, and those
    // processes die by its hand, not the job's failure.
    let until = job_exit.map_or(u64::MAX, |j| j.step);

    // A crash signal anywhere wins: the exits after it are its
    // consequences.
    let signal = trace.events.iter().enumerate().find_map(|(index, e)| {
        if e.step > until || made_too(e.step) {
            return None;
        }
        let EventKind::Signal { signo, addr, .. } = e.kind else {
            return None;
        };
        if !signal::FATAL.contains(&signo) {
            return None;
        }
        Some(Failure {
            step: e.step,
            pid: e.pid,
            tid: e.tid,
            index,
            kind: FailureKind::Signal { signo, addr },
        })
    });
    if signal.is_some() {
        return signal;
    }

    // A machine that stopped before the job exited failed where it
    // stopped: the nonzero exits along the way did not end the job. The
    // failing event is the last one the run made.
    if stopped == Stopped::Abnormally && job_exit.is_none() {
        let (index, last) = trace.events.iter().enumerate().next_back()?;
        return Some(Failure {
            step: total,
            pid: last.pid,
            tid: last.tid,
            index,
            kind: FailureKind::Stopped,
        });
    }

    // Processes that exited nonzero; threads are left out, since a
    // thread's exit_code says nothing about its process.
    let procs = trace.processes();
    let failed: Vec<&Process> = procs
        .iter()
        .filter(|p| p.end.is_some_and(|end| end <= until && !made_too(end)))
        .filter(|p| p.status.is_some_and(|s| s != 0))
        .collect();

    // Walk down from the last nonzero exit to the child it followed.
    let mut current = *failed.iter().max_by_key(|p| p.end)?;
    loop {
        let cause = failed
            .iter()
            .filter(|c| c.parent_life == Some(current.life()) && c.end <= current.end)
            .max_by_key(|c| c.end);
        match cause {
            Some(c) => current = c,
            None => break,
        }
    }

    // A process a signal killed, such as a writer killed by SIGPIPE when
    // the reader of its pipe exited, failed where the signal reached it:
    // its exit only follows from that.
    let status = ExitStatus::from_raw(current.status.unwrap_or_default());
    let end = current.end?;
    if let ExitStatus::Signal {
        signo: killed_by, ..
    } = status
    {
        let delivered = trace
            .events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, e)| {
                if e.pid != current.pid || e.step > end {
                    return None;
                }
                let EventKind::Signal { signo, addr, .. } = e.kind else {
                    return None;
                };
                (signo == killed_by).then_some(Failure {
                    step: e.step,
                    pid: e.pid,
                    tid: e.tid,
                    index,
                    kind: FailureKind::Signal { signo, addr },
                })
            });
        if delivered.is_some() {
            return delivered;
        }
    }

    // Otherwise the failure is that process's exit event, in the thread
    // that exited last, which need not be the main one.
    let step = end;
    let index = trace.events.iter().position(|e| {
        e.step == step
            && e.pid == current.pid
            && matches!(e.kind, EventKind::Exit { thread: false, .. })
    })?;
    Some(Failure {
        step,
        pid: current.pid,
        tid: trace.events[index].tid,
        index,
        kind: FailureKind::Exit {
            status: current.status.unwrap_or_default(),
        },
    })
}

#[cfg(test)]
mod tests {
    // Finding the failing event in traces built by hand.
    use super::*;
    use crate::Event;

    /// A job whose main thread ends with pthread_exit fails where its last
    /// thread ends the process: process 7 starts thread 8, its main thread
    /// exits at step 4, and thread 8 exits the process with status 3 at
    /// step 9, which init reports at 10. The failure is the exit at 9, in
    /// thread 8.
    #[test]
    fn a_process_fails_at_its_last_thread_s_exit() {
        let event = |step, pid, tid, kind| Event {
            step,
            pid,
            tid,
            kind,
        };
        let status: u32 = 3 << 8;
        let exit = |step, tid, thread| {
            event(
                step,
                7,
                tid,
                EventKind::Exit {
                    status,
                    comm: "pexit".into(),
                    thread,
                },
            )
        };
        let trace = Trace {
            events: vec![
                event(
                    1,
                    1,
                    1,
                    EventKind::Fork {
                        child: 7,
                        thread: false,
                    },
                ),
                event(
                    3,
                    7,
                    7,
                    EventKind::Fork {
                        child: 8,
                        thread: true,
                    },
                ),
                exit(4, 7, true),
                exit(9, 8, false),
            ],
            ..Default::default()
        };
        let job_exit = JobExit {
            step: 10,
            status: status as i32,
        };
        let failure = find(&trace, Some(job_exit), 10, Stopped::Otherwise, None).unwrap();
        assert_eq!((failure.step, failure.pid, failure.tid), (9, 7, 8));
        assert_eq!(failure.kind, FailureKind::Exit { status });
    }
}
