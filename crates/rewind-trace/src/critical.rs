//! Where a run's time went, and what it waited on: the longest chain of
//! work through a run's threads, and how much shorter the run could be if
//! one part of it took no time.
//!
//! The trace says, for every context switch, the VM's clock and whether
//! the thread that left the CPU could still run, and for every wakeup,
//! which thread or timer did it ([`crate::switches`],
//! [`crate::switches::Wake`]). From those this module rebuilds each
//! thread's runs on the CPU and, between two of them, why the second could
//! start: the thread was preempted and only waited for the CPU, a timer
//! ended its sleep, or another thread woke it at a point in that thread's
//! own run.
//!
//! A thread that waits for another to exit does not always block: a join
//! of a thread already done, or a wait for a child already a zombie,
//! returns without a wakeup to record. So every exit is a point the
//! thread that would wait for it starts after: for a thread, its
//! process's main thread, which joins it; for a process, the thread that
//! forked it, which reaps it. The point is where the exiting thread left
//! the CPU for the last time, and the waiter's first run from then on is
//! the one that starts after it. A waiter that never waited makes the
//! chain longer than it is, by work the run did in the order recorded; a
//! wait missed would make it shorter than the work the run needed.
//!
//! The kernel gives a thread id out again once its thread has exited, so
//! a thread is one life of an id, from the fork that made it: a run is
//! never the next of a run of an earlier thread with the same id.
//!
//! The VM has one CPU, so the run as recorded is the work of every thread
//! laid end to end. The chain this module finds is the one the run would
//! still take with as many CPUs as it has threads: every wait for the CPU
//! is dropped, and each run starts as soon as what it waited on allowed.
//! Its length is a lower bound on the run's time on any machine whose CPU
//! is as fast as the VM's clock says. [`Graph::finish`] recomputes it with
//! a chosen set of runs and sleeps taking no time, which is what "if this
//! took no time, the run would still take" means.
//!
//! What the clock counts is user-mode work and a fixed time per exit, so
//! the kernel's own work, starting a process among it, counts for little.
//! A wake's time is the clock when the monitor took it, at the end of the
//! stretch between two exits it happened in, and an exit's is the clock
//! at the switch that took its thread off the CPU for good. A fork is an
//! event, and events have steps and no times: a fork is placed inside its
//! run in proportion to the steps, which is early for a thread that
//! computed for long without a step before it forked.

use std::collections::HashMap;

use crate::event::EventKind;
use crate::switches::{Switch, WakeHow};
use crate::{Life, Trace};

/// One stretch the CPU ran a thread, from one switch to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub tid: u32,
    /// The thread's process; 0 for the idle task.
    pub pid: u32,
    /// Whether the thread is a program's rather than the kernel's own.
    pub memory: bool,
    pub start_step: u64,
    pub end_step: u64,
    pub start_ns: u64,
    pub end_ns: u64,
    /// Whether the thread could still run when it left the CPU.
    pub left_runnable: bool,
    /// The system call it blocked in when it left, if it did.
    pub left_syscall: Option<u32>,
}

impl Run {
    pub fn ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }

    pub fn is_idle(&self) -> bool {
        self.pid == 0
    }
}

/// Why a thread's run could start, as the trace tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The first run the trace saw of a thread nothing forked: it was
    /// there from boot.
    Boot,
    /// The first run of a thread `parent` forked, at `at` on the clock.
    Forked { parent: u32, at: u64 },
    /// The thread left its run before runnable, or nothing says it
    /// blocked: it only waited for the CPU.
    Ready,
    /// The thread blocked in `syscall` and a timer ended the wait `ns`
    /// after.
    Timer { ns: u64, syscall: Option<u32> },
    /// The thread blocked in `syscall` and thread `waker` woke it at `at`
    /// on the clock.
    Woken {
        waker: u32,
        at: u64,
        syscall: Option<u32>,
    },
}

/// What part of a run, or which sleep, a computation treats as taking no
/// time.
pub trait Zero {
    /// Whether the run at this index takes no time.
    fn run(&self, index: usize, run: &Run) -> bool;
    /// Whether the timer wait before the run at this index takes no time.
    fn sleep(&self, index: usize, run: &Run) -> bool;
}

/// Nothing taken away: the run as it was, on enough CPUs.
#[derive(Debug)]
pub struct Nothing;

impl Zero for Nothing {
    fn run(&self, _: usize, _: &Run) -> bool {
        false
    }
    fn sleep(&self, _: usize, _: &Run) -> bool {
        false
    }
}

/// What a run's start waited on, in the chain [`Graph::finish`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bound {
    /// Nothing before it in the run.
    Boot,
    /// Its thread's run before it, and the timer wait between, if any.
    Prev { run: usize, sleep: u64 },
    /// A point in another thread's run: a fork or a wake.
    Other { run: usize, at: u64 },
}

/// One piece of the chain: part of a run, or a sleep before one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    /// The run the piece is part of, or the run a sleep came before.
    pub run: usize,
    pub kind: PieceKind,
    /// How long it is on the chain, in nanoseconds.
    pub ns: u64,
    /// Where it starts on the clock as recorded: where its computing began
    /// in the run, or where its thread fell asleep.
    pub from_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PieceKind {
    /// The thread computing.
    Compute,
    /// The thread asleep until a timer, in `syscall`.
    Sleep { syscall: Option<u32> },
}

/// The chain through a run: its length, from the job's start to its end,
/// and its pieces, first to last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    pub ns: u64,
    pub pieces: Vec<Piece>,
    /// Where the chain passed from one thread to another that woke it,
    /// first to last: what a thread on the chain waited for.
    pub hops: Vec<Hop>,
}

/// A thread on the chain that waited for another thread: the chain goes
/// on in the thread that woke it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hop {
    /// The run that waited, and the run of the thread that woke it.
    pub waiter: usize,
    pub waker: usize,
    /// The system call the waiter blocked in.
    pub syscall: Option<u32>,
    /// How long the waiter was blocked, on the one CPU.
    pub blocked_ns: u64,
}

/// A run's threads and what each of their runs waited on.
#[derive(Clone, Debug)]
pub struct Graph {
    pub runs: Vec<Run>,
    /// Why each run could start, by index.
    pub starts: Vec<Start>,
    /// Each run's thread's run before it, by index.
    prev: Vec<Option<usize>>,
    /// Each thread's runs, in order, by its life.
    by_thread: HashMap<ThreadLife, Vec<usize>>,
    /// Each thread id's forks, which tell its lives apart.
    forks: Forks,
    /// The exits each run starts after, by index: each the exiting thread
    /// and the clock at its exit.
    after: Vec<Vec<(u32, u64)>>,
    /// The job's start and end, as runs and times on the clock.
    pub job_start: (usize, u64),
    pub job_end: (usize, u64),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    #[error("the run has no context switches with times; it was recorded before rewind time")]
    NoTimes,
    #[error("the run has no job start and end marks")]
    NoJob,
    #[error("the run was recorded with exit time, where computing takes no time on the clock")]
    ExitClock,
}

impl Graph {
    /// The graph of `trace`'s runs and wakes, and `end_ns`, the clock
    /// when the run stopped.
    pub fn new(trace: &Trace, end_ns: u64) -> Result<Graph, TimeError> {
        let switches = &trace.switches;
        if switches.len() < 2 || switches.iter().all(|s| s.ns == 0) {
            return Err(TimeError::NoTimes);
        }
        let runs = runs_of(switches, trace.last_step(), end_ns);

        // Every fork of each thread id, at its time.
        let clock = Clock { runs: &runs };
        let mut forks = Forks::default();
        for e in &trace.events {
            if let EventKind::Fork { child, .. } = e.kind {
                forks.0.entry(child).or_default().push(ForkAt {
                    step: e.step,
                    ns: clock.at(e.step),
                    parent: e.tid,
                });
            }
        }

        // Each thread's runs, and the run before each in the same life.
        let mut by_thread: HashMap<ThreadLife, Vec<usize>> = HashMap::new();
        let mut prev = vec![None; runs.len()];
        for (i, run) in runs.iter().enumerate() {
            if run.is_idle() {
                continue;
            }
            let list = by_thread
                .entry(forks.at_step(run.tid, run.start_step))
                .or_default();
            prev[i] = list.last().copied();
            list.push(i);
        }

        // Wakes by woken thread, at their times.
        let mut wakes: HashMap<u32, Vec<(u64, u64, u32, WakeHow)>> = HashMap::new();
        for w in &trace.wakes {
            wakes
                .entry(w.tid)
                .or_default()
                .push((w.step, w.ns, w.waker, w.how));
        }

        // Why each run could start.
        let mut starts = vec![Start::Boot; runs.len()];
        for (i, run) in runs.iter().enumerate() {
            if run.is_idle() {
                continue;
            }
            let Some(before) = prev[i] else {
                starts[i] = match forks.of(forks.at_step(run.tid, run.start_step)) {
                    Some(f) => Start::Forked {
                        parent: f.parent,
                        at: f.ns,
                    },
                    None => Start::Boot,
                };
                continue;
            };
            let left = &runs[before];
            if left.left_runnable {
                starts[i] = Start::Ready;
                continue;
            }

            // The wake that ended the block: the first of the thread's
            // after it left and by the step it ran again.
            let wake = wakes.get(&run.tid).and_then(|list| {
                let from = list.partition_point(|(step, ..)| *step <= left.end_step);
                list[from..]
                    .iter()
                    .find(|(step, ..)| *step <= run.start_step)
                    .copied()
            });
            starts[i] = match wake {
                // A wait for Rewind's own guest code is not the workload's.
                None | Some((_, _, _, WakeHow::Platform)) => Start::Ready,
                Some((_, at, waker, WakeHow::Task)) => {
                    match through_kernel(&runs, &by_thread, &forks, &starts, waker, at) {
                        Some((waker, at)) => Start::Woken {
                            waker,
                            at,
                            syscall: left.left_syscall,
                        },
                        None => Start::Timer {
                            ns: at.saturating_sub(left.end_ns),
                            syscall: left.left_syscall,
                        },
                    }
                }
                Some((_, at, ..)) => Start::Timer {
                    ns: at.saturating_sub(left.end_ns),
                    syscall: left.left_syscall,
                },
            };
        }

        // The exits each run starts after: a thread's exit for a run of
        // its process's main thread, a process's for a run of the thread
        // that forked it.
        let mut after = vec![Vec::new(); runs.len()];
        for e in &trace.events {
            let EventKind::Exit { thread, .. } = e.kind else {
                continue;
            };
            let waiter = match thread {
                true if e.tid == e.pid => continue,
                true => e.pid,
                false => match forks.of(forks.at_step(e.pid, e.step)) {
                    Some(f) => f.parent,
                    None => continue,
                },
            };

            // The exit is over when its thread leaves the CPU for the
            // last time, which the switches give on the clock. The exit's
            // event is no measure of it: a thread that computed up to its
            // exit takes its first step there, and the steps of the exit
            // itself follow, so the event is near the start of the run's
            // steps and near the end of the run's time.
            let exiting = by_thread
                .get(&forks.at_step(e.tid, e.step))
                .and_then(|list| list.last());
            let Some(&exiting) = exiting else {
                continue;
            };
            let over = &runs[exiting];

            // The waiter's first run from then on is the first that can
            // have found the thread gone. A thread taken off the CPU in
            // the middle of its exit is not gone yet for the runs the
            // waiter has meanwhile.
            let Some(list) = by_thread.get(&forks.at_step(waiter, e.step)) else {
                continue;
            };
            let next = list.partition_point(|&j| runs[j].start_step < over.end_step);
            let Some(&next) = list.get(next) else {
                continue;
            };
            after[next].push((e.tid, over.end_ns));
        }

        // The job's start and end, from init's marks.
        let (Some(start), Some(end)) = (trace.job_start(), trace.job_exit()) else {
            return Err(TimeError::NoJob);
        };
        let job_start = clock.point(start);
        let job_end = clock.point(end.step);
        Ok(Graph {
            runs,
            starts,
            prev,
            by_thread,
            forks,
            after,
            job_start,
            job_end,
        })
    }

    /// The clock at `step`, placed inside the run it was taken in.
    pub fn time_at(&self, step: u64) -> u64 {
        Clock { runs: &self.runs }.at(step)
    }

    /// The run of the same thread before run `run`, if any.
    pub fn before(&self, run: usize) -> Option<usize> {
        self.prev[run]
    }

    /// The step `piece` of a chain starts on: its run's first for
    /// computing, and for a sleep the step its thread left the CPU on,
    /// the end of the thread's run before the one the sleep came before.
    pub fn piece_step(&self, piece: &Piece) -> u64 {
        let run = &self.runs[piece.run];
        match piece.kind {
            PieceKind::Compute => run.start_step,
            PieceKind::Sleep { .. } => self
                .before(piece.run)
                .map_or(run.start_step, |b| self.runs[b].end_step),
        }
    }

    /// The chain from the job's start to its end with `zero` taking no
    /// time, on as many CPUs as there are threads.
    pub fn chain(&self, zero: &dyn Zero) -> Chain {
        let (es, bound) = self.earliest(zero);
        let length = |run: usize, at: u64| -> u64 {
            let r = &self.runs[run];
            if zero.run(run, r) {
                0
            } else {
                at.saturating_sub(r.start_ns).min(r.ns())
            }
        };
        let start = es[self.job_start.0] + length(self.job_start.0, self.job_start.1);
        let end = es[self.job_end.0] + length(self.job_end.0, self.job_end.1);

        // Back from the end, along what each start waited on.
        let mut pieces = Vec::new();
        let mut hops = Vec::new();
        let mut cursor = Some(self.job_end);
        while let Some((run, at)) = cursor {
            // The job's first run counts only from the job's start.
            let mut ns = length(run, at);
            let mut from_ns = self.runs[run].start_ns;
            if run == self.job_start.0 {
                let before = length(run, self.job_start.1);
                ns = ns.saturating_sub(before);
                from_ns += before;
            }
            if ns > 0 {
                pieces.push(Piece {
                    run,
                    kind: PieceKind::Compute,
                    ns,
                    from_ns,
                });
            }
            if run == self.job_start.0 {
                break;
            }
            cursor = match bound[run] {
                Bound::Boot => None,
                Bound::Prev { run: before, sleep } => {
                    if sleep > 0 {
                        let syscall = match self.starts[run] {
                            Start::Timer { syscall, .. } => syscall,
                            _ => None,
                        };
                        pieces.push(Piece {
                            run,
                            kind: PieceKind::Sleep { syscall },
                            ns: sleep,
                            from_ns: self.runs[before].end_ns,
                        });
                    }
                    Some((before, self.runs[before].end_ns))
                }
                Bound::Other { run: other, at } => {
                    if let Start::Woken { syscall, .. } = self.starts[run] {
                        let blocked_ns = self.prev[run].map_or(0, |before| {
                            self.runs[run]
                                .start_ns
                                .saturating_sub(self.runs[before].end_ns)
                        });
                        hops.push(Hop {
                            waiter: run,
                            waker: other,
                            syscall,
                            blocked_ns,
                        });
                    }
                    Some((other, at))
                }
            };
        }
        pieces.reverse();
        hops.reverse();
        Chain {
            ns: end.saturating_sub(start),
            pieces,
            hops,
        }
    }

    /// The chain's length alone, which is all a what-if needs.
    pub fn finish(&self, zero: &dyn Zero) -> u64 {
        let (es, _) = self.earliest(zero);
        let length = |run: usize, at: u64| -> u64 {
            let r = &self.runs[run];
            if zero.run(run, r) {
                0
            } else {
                at.saturating_sub(r.start_ns).min(r.ns())
            }
        };
        let start = es[self.job_start.0] + length(self.job_start.0, self.job_start.1);
        let end = es[self.job_end.0] + length(self.job_end.0, self.job_end.1);
        end.saturating_sub(start)
    }

    /// The earliest each run could start with `zero` taking no time, and
    /// what bound it. The runs are in the order they ran, and everything a
    /// run waits on happened before it in that order, so one pass does.
    fn earliest(&self, zero: &dyn Zero) -> (Vec<u64>, Vec<Bound>) {
        let n = self.runs.len();
        let mut es = vec![0u64; n];
        let mut bound = vec![Bound::Boot; n];
        let dur = |i: usize| -> u64 {
            let r = &self.runs[i];
            if zero.run(i, r) { 0 } else { r.ns() }
        };

        for i in 0..n {
            let run = &self.runs[i];
            if run.is_idle() {
                continue;
            }

            // Where another thread was, on the new schedule, at a time it
            // had on the old one: inside one of its runs, scaled to what
            // that run takes now.
            let point = |es: &[u64], tid: u32, at: u64| -> Option<(usize, u64)> {
                let list = self.by_thread.get(&self.forks.at_ns(tid, at))?;
                let k = list.partition_point(|&j| self.runs[j].start_ns <= at);
                let j = list[k.checked_sub(1)?];
                let r = &self.runs[j];
                let into = if zero.run(j, r) {
                    0
                } else {
                    at.saturating_sub(r.start_ns).min(r.ns())
                };
                Some((j, es[j] + into))
            };

            let after_prev = |es: &[u64], sleep: u64| -> Option<(u64, Bound)> {
                let before = self.prev[i]?;
                Some((
                    es[before] + dur(before) + sleep,
                    Bound::Prev { run: before, sleep },
                ))
            };

            let (mut start, mut why) = match self.starts[i] {
                Start::Boot => (0, Bound::Boot),
                Start::Forked { parent, at } => match point(&es, parent, at) {
                    Some((j, t)) => (t, Bound::Other { run: j, at }),
                    None => (0, Bound::Boot),
                },
                Start::Ready => after_prev(&es, 0).unwrap_or((0, Bound::Boot)),
                Start::Timer { ns, .. } => {
                    let sleep = if zero.sleep(i, run) { 0 } else { ns };
                    after_prev(&es, sleep).unwrap_or((0, Bound::Boot))
                }
                Start::Woken { waker, at, .. } => {
                    let own = after_prev(&es, 0).unwrap_or((0, Bound::Boot));
                    match point(&es, waker, at) {
                        Some((j, t)) if t > own.0 => (t, Bound::Other { run: j, at }),
                        _ => own,
                    }
                }
            };

            // No sooner than the exits it would have waited for.
            for &(tid, at) in &self.after[i] {
                if let Some((j, t)) = point(&es, tid, at)
                    && t > start
                {
                    start = t;
                    why = Bound::Other { run: j, at };
                }
            }
            es[i] = start;
            bound[i] = why;
        }
        (es, bound)
    }
}

/// How deep [`through_kernel`] follows kernel threads waking each other.
const KERNEL_HOPS: usize = 8;

/// The thread, and the time, a wake by `waker` at `at` really waited on.
/// A program's thread is that itself. A kernel thread, a kworker running
/// delayed work or ksoftirqd, does work others queued, mostly when a timer
/// they armed fires: its own runs before are not what the woken thread
/// waited for. So a wake by a kernel thread is the wake that started the
/// kernel thread's run: by a program's thread, which is returned, or by a
/// timer, which is None, the woken thread's own wait for a timer. The
/// kernel threads' runs are those before the woken one, so their starts
/// are already known.
fn through_kernel(
    runs: &[Run],
    by_thread: &HashMap<ThreadLife, Vec<usize>>,
    forks: &Forks,
    starts: &[Start],
    mut waker: u32,
    mut at: u64,
) -> Option<(u32, u64)> {
    for _ in 0..KERNEL_HOPS {
        let list = by_thread.get(&forks.at_ns(waker, at))?;
        let k = list.partition_point(|&j| runs[j].start_ns <= at);
        let run = list[k.checked_sub(1)?];
        if runs[run].memory {
            return Some((waker, at));
        }
        match starts[run] {
            Start::Woken {
                waker: before,
                at: then,
                ..
            } => {
                waker = before;
                at = then;
            }
            _ => return None,
        }
    }
    None
}

/// The runs between consecutive switches; the last ends at the trace's
/// last step and `end_ns`.
fn runs_of(switches: &[Switch], last_step: u64, end_ns: u64) -> Vec<Run> {
    let mut runs = Vec::with_capacity(switches.len());
    for (i, s) in switches.iter().enumerate() {
        let next = switches.get(i + 1);
        runs.push(Run {
            tid: s.tid,
            pid: s.pid,
            memory: s.memory,
            start_step: s.step,
            end_step: next.map_or(last_step.max(s.step), |n| n.step),
            start_ns: s.ns,
            end_ns: next.map_or(end_ns.max(s.ns), |n| n.ns),
            left_runnable: next.is_some_and(|n| n.prev_runnable),
            left_syscall: next.and_then(|n| n.prev_syscall),
        });
    }
    runs
}

/// One life of a thread id: the id, and how many forks of it came
/// before, 0 for a thread there from boot.
type ThreadLife = (u32, usize);

/// A fork of a thread: its step, the clock then, and the thread that
/// forked it.
#[derive(Clone, Copy, Debug)]
struct ForkAt {
    step: u64,
    ns: u64,
    parent: u32,
}

/// Every fork of each thread id, in order, which tell the id's lives
/// apart.
#[derive(Clone, Debug, Default)]
struct Forks(HashMap<u32, Vec<ForkAt>>);

impl Forks {
    /// The life of `tid` at `step`: after every fork of it before.
    fn at_step(&self, tid: u32, step: u64) -> ThreadLife {
        let before = self
            .0
            .get(&tid)
            .map_or(0, |list| list.partition_point(|f| f.step < step));
        (tid, before)
    }

    /// The life of `tid` at `ns` on the clock.
    fn at_ns(&self, tid: u32, ns: u64) -> ThreadLife {
        let before = self
            .0
            .get(&tid)
            .map_or(0, |list| list.partition_point(|f| f.ns < ns));
        (tid, before)
    }

    /// The fork that began `life`; None for a thread there from boot.
    fn of(&self, life: ThreadLife) -> Option<ForkAt> {
        let index = life.1.checked_sub(1)?;
        self.0.get(&life.0)?.get(index).copied()
    }
}

/// Times on the clock for steps: a step inside a run is placed between
/// the run's start and end in proportion to its steps.
struct Clock<'a> {
    runs: &'a [Run],
}

impl Clock<'_> {
    /// The run a step was taken in: the last that started before it.
    fn run_at(&self, step: u64) -> usize {
        self.runs
            .partition_point(|r| r.start_step < step)
            .saturating_sub(1)
    }

    fn at(&self, step: u64) -> u64 {
        self.point(step).1
    }

    /// The run a step was taken in, and its time.
    fn point(&self, step: u64) -> (usize, u64) {
        let i = self.run_at(step);
        let r = &self.runs[i];
        let steps = r.end_step.saturating_sub(r.start_step);
        if steps == 0 {
            return (i, r.start_ns);
        }
        let into = step.clamp(r.start_step, r.end_step) - r.start_step;
        let ns = u128::from(r.ns()) * u128::from(into) / u128::from(steps);
        (i, r.start_ns + ns as u64)
    }
}

/// Zeroes every run, and every timer wait, of a set of threads.
pub struct Threads<'a>(pub &'a dyn Fn(usize, &Run) -> bool);

impl std::fmt::Debug for Threads<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Threads")
    }
}

impl Zero for Threads<'_> {
    fn run(&self, index: usize, run: &Run) -> bool {
        (self.0)(index, run)
    }
    fn sleep(&self, index: usize, run: &Run) -> bool {
        (self.0)(index, run)
    }
}

/// Zeroes the runs of a set of threads, and none of their timer waits:
/// their computing alone.
pub struct Computing<'a>(pub &'a dyn Fn(usize, &Run) -> bool);

impl std::fmt::Debug for Computing<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Computing")
    }
}

impl Zero for Computing<'_> {
    fn run(&self, index: usize, run: &Run) -> bool {
        (self.0)(index, run)
    }
    fn sleep(&self, _: usize, _: &Run) -> bool {
        false
    }
}

/// Zeroes only the timer waits before a set of runs.
pub struct Sleeps<'a>(pub &'a dyn Fn(usize, &Run) -> bool);

impl std::fmt::Debug for Sleeps<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sleeps")
    }
}

impl Zero for Sleeps<'_> {
    fn run(&self, _: usize, _: &Run) -> bool {
        false
    }
    fn sleep(&self, index: usize, run: &Run) -> bool {
        (self.0)(index, run)
    }
}

/// The life of the process each run's thread belonged to, for runs of
/// programs: the pid alive at the run's start.
pub fn lives(graph: &Graph, trace: &Trace) -> Vec<Option<Life>> {
    let procs = trace.processes();
    let by_pid = crate::ByPid::new(&procs);
    graph
        .runs
        .iter()
        .map(|r| {
            if r.is_idle() {
                return None;
            }
            by_pid.at(r.pid, r.start_step).map(|p| p.life())
        })
        .collect()
}

/// The names of the x86-64 system calls a thread commonly blocks in.
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    Some(match nr {
        0 => "read",
        1 => "write",
        7 => "poll",
        23 => "select",
        24 => "sched_yield",
        34 => "pause",
        35 => "nanosleep",
        40 => "sendfile",
        42 => "connect",
        43 => "accept",
        45 => "recvfrom",
        47 => "recvmsg",
        59 => "execve",
        61 => "wait4",
        65 => "semop",
        73 => "flock",
        74 => "fsync",
        75 => "fdatasync",
        128 => "rt_sigtimedwait",
        130 => "rt_sigsuspend",
        202 => "futex",
        230 => "clock_nanosleep",
        232 => "epoll_wait",
        247 => "waitid",
        257 => "openat",
        270 => "pselect6",
        271 => "ppoll",
        275 => "splice",
        281 => "epoll_pwait",
        288 => "accept4",
        426 => "io_uring_enter",
        441 => "epoll_pwait2",
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    // The chain through runs built by hand: two threads, one waking the
    // other, and a timer sleep, checked against what each wait allows.
    use super::*;

    /// A run of `tid` from `start` to `end` ns, its steps the same as its
    /// times in microseconds.
    fn run(tid: u32, start: u64, end: u64, runnable: bool) -> Run {
        Run {
            tid,
            pid: tid,
            memory: true,
            start_step: start,
            end_step: end,
            start_ns: start,
            end_ns: end,
            left_runnable: runnable,
            left_syscall: None,
        }
    }

    /// A graph from runs and starts, the job from the first run's start to
    /// the last run's end.
    fn graph(runs: Vec<Run>, starts: Vec<Start>) -> Graph {
        let mut by_thread: HashMap<ThreadLife, Vec<usize>> = HashMap::new();
        let mut prev = vec![None; runs.len()];
        for (i, r) in runs.iter().enumerate() {
            let list = by_thread.entry((r.tid, 0)).or_default();
            prev[i] = list.last().copied();
            list.push(i);
        }
        let last = runs.len() - 1;
        let end = runs[last].end_ns;
        let after = vec![Vec::new(); runs.len()];
        Graph {
            runs,
            starts,
            prev,
            by_thread,
            forks: Forks::default(),
            after,
            job_start: (0, 0),
            job_end: (last, end),
        }
    }

    /// A switch at `step` to thread `tid` of process `pid`, the clock one
    /// past the step. The thread that left blocked in `left_in`, or was
    /// still runnable when that is None.
    pub(crate) fn switch(step: u64, pid: u32, tid: u32, left_in: Option<u32>) -> Switch {
        Switch {
            step,
            pid,
            tid,
            memory: pid != 0,
            name: "t".into(),
            ns: step + 1,
            prev_runnable: left_in.is_none(),
            prev_syscall: left_in,
        }
    }

    /// An event of thread `tid` of process `pid` at `step`.
    pub(crate) fn event(step: u64, pid: u32, tid: u32, kind: EventKind) -> crate::Event {
        crate::Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    /// Init's job mark at `step`: the job's start, or its exit.
    pub(crate) fn job_mark(step: u64, text: &str) -> crate::Event {
        event(
            step,
            1,
            1,
            EventKind::Mark {
                text: text.to_string(),
            },
        )
    }

    /// A wake of `tid` by thread `waker` at `step`, the clock one past it.
    pub(crate) fn task_wake(step: u64, tid: u32, waker: u32) -> crate::switches::Wake {
        crate::switches::Wake {
            step,
            ns: step + 1,
            tid,
            waker,
            how: WakeHow::Task,
        }
    }

    /// The exit of thread `tid` of process `pid` at `step`.
    fn exit(step: u64, pid: u32, tid: u32, thread: bool) -> crate::Event {
        event(
            step,
            pid,
            tid,
            EventKind::Exit {
                status: 0,
                comm: "t".into(),
                thread,
            },
        )
    }

    /// The fork of `child` by thread `tid` of process `pid` at `step`.
    fn fork(step: u64, pid: u32, tid: u32, child: u32, thread: bool) -> crate::Event {
        event(step, pid, tid, EventKind::Fork { child, thread })
    }

    /// Init's main thread (1) starts threads 2 and 3 and joins 3 first. On
    /// the one CPU thread 2 computes 100 steps and exits while main is
    /// blocked in the join, waking nobody; thread 3 computes 10, and its
    /// exit wakes main, whose join of thread 2 finds it done and does not
    /// block. Both threads were joined, so the job waited for both.
    pub(crate) fn joined_after_exit() -> Trace {
        Trace {
            events: vec![
                job_mark(1, "rewind-start"),
                fork(3, 1, 1, 2, true),
                fork(4, 1, 1, 3, true),
                exit(104, 1, 2, true),
                exit(114, 1, 3, true),
                job_mark(119, "rewind-exit 0"),
            ],
            switches: vec![
                switch(0, 1, 1, None),
                switch(5, 1, 2, Some(202)),
                switch(105, 1, 3, Some(60)),
                switch(115, 1, 1, Some(60)),
                switch(120, 0, 0, Some(60)),
            ],
            wakes: vec![task_wake(114, 1, 3)],
        }
    }

    #[test]
    fn a_join_that_finds_its_thread_done_still_waits_for_it() {
        // The chain is main up to the forks, thread 2's computing up to
        // where it left the CPU for good, all 100 of it, and main after,
        // where following wakes alone finds thread 3's 10 and misses
        // thread 2.
        let trace = joined_after_exit();
        let g = Graph::new(&trace, 121).unwrap();
        let chain = g.chain(&Nothing);
        let long: u64 = chain
            .pieces
            .iter()
            .filter(|p| g.runs[p.run].tid == 2)
            .map(|p| p.ns)
            .sum();
        assert_eq!(long, 100);
        assert!(chain.ns >= 100, "chain {}", chain.ns);

        // Without thread 2 the chain is thread 3's instead.
        let without = g.finish(&Threads(&|_, r| r.tid == 2));
        assert!(without < 30, "without thread 2: {without}");
    }

    /// The pids of the job [`pipe_job`] runs and of the child the job
    /// forks.
    const PIPE_PARENT: u32 = 34;
    const PIPE_CHILD: u32 = 35;

    /// How long the parent of [`pipe_job`] computes before it exits.
    const PIPE_PARENT_WORK: u64 = 60_000;

    /// A job as `rewind run` recorded one. Init (1) forks the job, which
    /// forks a child and blocks reading a pipe. The child writes the pipe,
    /// which wakes the job, computes for `child_work` and exits; the job
    /// then computes for [`PIPE_PARENT_WORK`] and exits, and never waits
    /// for the child. Init reaps both.
    ///
    /// The clock is not the steps: a thread that computes takes no step
    /// until its exit, which is the first of the ten steps its last run
    /// has. The child is taken off the CPU before its exit is over, as the
    /// recorded one was, and finishes exiting after the job has exited.
    fn pipe_job(child_work: u64) -> Trace {
        /// What the clock moves for a step.
        const STEP_NS: u64 = 5;
        /// The system calls the threads block in.
        const READ: u32 = 0;
        const EXIT: u32 = 60;
        const WAIT4: u32 = 61;

        // The clock at each switch: every step takes STEP_NS, and the
        // computing is all before the first step of the run it is in.
        let job_forked = 10 * STEP_NS;
        let child_runs = 20 * STEP_NS;
        let job_runs = child_runs + child_work + 10 * STEP_NS;
        let job_exited = job_runs + PIPE_PARENT_WORK + 11 * STEP_NS;
        let child_resumes = job_exited + 2 * STEP_NS;
        let child_exited = child_resumes + STEP_NS;
        let machine_idle = child_exited + 11 * STEP_NS;
        let at = |step: u64, pid: u32, left_in: Option<u32>, ns: u64| Switch {
            ns,
            ..switch(step, pid, pid, left_in)
        };
        let wake = |step: u64, ns: u64, tid: u32, waker: u32| crate::switches::Wake {
            ns,
            ..task_wake(step, tid, waker)
        };

        Trace {
            events: vec![
                job_mark(1, "rewind-start"),
                fork(3, 1, 1, PIPE_PARENT, false),
                fork(12, PIPE_PARENT, PIPE_PARENT, PIPE_CHILD, false),
                exit(21, PIPE_CHILD, PIPE_CHILD, false),
                exit(31, PIPE_PARENT, PIPE_PARENT, false),
                job_mark(52, "rewind-exit 0"),
            ],
            switches: vec![
                at(0, 1, None, 0),
                at(10, PIPE_PARENT, Some(WAIT4), job_forked),
                at(20, PIPE_CHILD, Some(READ), child_runs),
                at(30, PIPE_PARENT, None, job_runs),
                at(41, 1, Some(EXIT), job_exited),
                at(43, PIPE_CHILD, Some(WAIT4), child_resumes),
                at(44, 1, Some(EXIT), child_exited),
                at(55, 0, Some(EXIT), machine_idle),
            ],
            wakes: vec![
                wake(11, job_forked + STEP_NS, 1, PIPE_PARENT),
                wake(21, child_runs, PIPE_PARENT, PIPE_CHILD),
                wake(44, child_exited, 1, PIPE_CHILD),
            ],
        }
    }

    #[test]
    fn a_thread_s_computing_before_its_exit_is_on_the_chain() {
        // Tests that work a thread did before exiting counts though the
        // exit is among the first steps of the thread's last run. The
        // job of pipe_job must finish its own computing whatever its
        // child does, and never waits for the child. So with a child that
        // computes nothing and with one that computes for 40,000, the
        // chain holds all of the job's computing, read from the switch
        // records, and is shorter than the job's and the child's
        // computing end to end.
        const CHILD_WORK: u64 = 40_000;
        for child_work in [0, CHILD_WORK] {
            let trace = pipe_job(child_work);
            let end_ns = trace.switches.last().unwrap().ns;
            let g = Graph::new(&trace, end_ns).unwrap();
            let chain = g.chain(&Nothing);
            let of_parent: u64 = chain
                .pieces
                .iter()
                .filter(|p| g.runs[p.run].tid == PIPE_PARENT)
                .map(|p| p.ns)
                .sum();
            assert!(
                of_parent >= PIPE_PARENT_WORK,
                "child work {child_work}: the job has {of_parent} on the chain"
            );
            assert!(
                (PIPE_PARENT_WORK..PIPE_PARENT_WORK + CHILD_WORK).contains(&chain.ns),
                "child work {child_work}: chain {}",
                chain.ns
            );

            // Without the child's computing the job still takes its own.
            let without = g.finish(&Threads(&|_, r| r.tid == PIPE_CHILD));
            assert!(
                without >= PIPE_PARENT_WORK,
                "child work {child_work}: without the child {without}"
            );
        }
    }

    /// Init (1) forks a child at step 5 and waits for it; the child runs
    /// from 10 to 20 and its exit wakes init. Init forks a second child at
    /// 45, pid `second`, which runs from 50 to 60 and wakes init again.
    fn two_children(second: u32) -> Trace {
        Trace {
            events: vec![
                job_mark(1, "rewind-start"),
                fork(5, 1, 1, 2, false),
                exit(19, 2, 2, false),
                fork(45, 1, 1, second, false),
                exit(59, second, second, false),
                job_mark(69, "rewind-exit 0"),
            ],
            switches: vec![
                switch(0, 1, 1, None),
                switch(10, 2, 2, Some(61)),
                switch(20, 1, 1, Some(60)),
                switch(50, second, second, Some(61)),
                switch(60, 1, 1, Some(60)),
                switch(70, 0, 0, Some(60)),
            ],
            wakes: vec![task_wake(19, 1, 2), task_wake(59, 1, second)],
        }
    }

    #[test]
    fn a_reused_thread_id_starts_a_life_of_its_own() {
        // The second child given pid 2 again is forked, not the first
        // child running on: the chain and its pieces are the same as with
        // pid 3.
        let fresh = two_children(3);
        let reused = two_children(2);
        let g_fresh = Graph::new(&fresh, 71).unwrap();
        let g_reused = Graph::new(&reused, 71).unwrap();
        assert_eq!(g_reused.chain(&Nothing).ns, g_fresh.chain(&Nothing).ns);
        assert!(matches!(
            g_reused.starts[3],
            Start::Forked { parent: 1, .. }
        ));
        assert_eq!(g_reused.before(3), None);
        assert_eq!(
            g_reused.finish(&Computing(&|i, _| i == 3)),
            g_fresh.finish(&Computing(&|i, _| i == 3))
        );
    }

    #[test]
    fn a_wait_for_the_cpu_is_dropped_from_the_chain() {
        // Main (1) forks worker (2) at 10 and waits for it; the worker
        // runs 10 to 40, and its exit wakes main, which runs 40 to 50. The
        // chain is all three: 50.
        let runs = vec![
            run(1, 0, 10, false),
            run(2, 10, 40, false),
            run(1, 40, 50, false),
        ];
        let starts = vec![
            Start::Boot,
            Start::Forked { parent: 1, at: 10 },
            Start::Woken {
                waker: 2,
                at: 40,
                syscall: Some(61),
            },
        ];
        let g = graph(runs, starts);
        let chain = g.chain(&Nothing);
        assert_eq!(chain.ns, 50);
        let on: Vec<(usize, u64)> = chain.pieces.iter().map(|p| (p.run, p.ns)).collect();
        assert_eq!(on, vec![(0, 10), (1, 30), (2, 10)]);
    }

    /// Main forks two workers at 10 and joins them in order. On the one
    /// CPU worker 2 runs 10-40 and its exit wakes main, which runs 40-41
    /// and joins worker 3; worker 3 runs 41-61 and its exit wakes main for
    /// 61-71.
    fn two_joins() -> Graph {
        let runs = vec![
            run(1, 0, 10, false),
            run(2, 10, 40, false),
            run(1, 40, 41, false),
            run(3, 41, 61, false),
            run(1, 61, 71, false),
        ];
        let starts = vec![
            Start::Boot,
            Start::Forked { parent: 1, at: 10 },
            Start::Woken {
                waker: 2,
                at: 40,
                syscall: Some(202),
            },
            Start::Forked { parent: 1, at: 10 },
            Start::Woken {
                waker: 3,
                at: 61,
                syscall: Some(202),
            },
        ];
        graph(runs, starts)
    }

    #[test]
    fn two_workers_on_one_cpu_overlap_on_enough() {
        // On enough CPUs both workers start at 10: worker 2 ends at 40,
        // worker 3 at 30, so main's second join finds it done and the
        // chain is main 10, worker 2 30, main 1, main 10: 51.
        let g = two_joins();
        let chain = g.chain(&Nothing);
        assert_eq!(chain.ns, 51);
        let on: Vec<(usize, u64)> = chain.pieces.iter().map(|p| (p.run, p.ns)).collect();
        assert_eq!(on, vec![(0, 10), (1, 30), (2, 1), (4, 10)]);
    }

    #[test]
    fn taking_away_a_run_off_the_chain_saves_nothing() {
        // Worker 3 is off the chain: without it the run takes as long.
        // Without worker 2, main waits for worker 3 instead, which ends
        // at 30: main 10, worker 3 20, main 10 is 40.
        let g = two_joins();
        assert_eq!(g.finish(&Threads(&|_, r| r.tid == 3)), 51);
        assert_eq!(g.finish(&Threads(&|_, r| r.tid == 2)), 40);
    }

    #[test]
    fn a_wake_by_a_kernel_thread_is_traced_to_what_woke_it() {
        // A kworker (9) woken by a timer wakes a program's thread: the
        // program waited for the timer, not for the kworker's earlier
        // runs. A kworker a program's thread (1) woke passes the wake on
        // as that thread's.
        let mut kworker = run(9, 50, 51, false);
        kworker.memory = false;
        let mut relay = run(9, 70, 71, false);
        relay.memory = false;
        let runs = vec![run(1, 0, 10, false), kworker, run(1, 60, 70, false), relay];
        let starts = vec![
            Start::Boot,
            Start::Timer {
                ns: 40,
                syscall: None,
            },
            Start::Ready,
            Start::Woken {
                waker: 1,
                at: 65,
                syscall: None,
            },
        ];
        let mut by_thread: HashMap<ThreadLife, Vec<usize>> = HashMap::new();
        for (i, r) in runs.iter().enumerate() {
            by_thread.entry((r.tid, 0)).or_default().push(i);
        }
        let forks = Forks::default();
        let wake = |waker, at| through_kernel(&runs, &by_thread, &forks, &starts, waker, at);
        assert_eq!(wake(9, 50), None);
        assert_eq!(wake(9, 70), Some((1, 65)));
        assert_eq!(wake(1, 5), Some((1, 5)));
    }

    #[test]
    fn a_timer_sleep_is_on_the_chain_and_can_be_taken_away() {
        // One thread computes 10, sleeps 100 on a timer, computes 10.
        let runs = vec![run(1, 0, 10, false), run(1, 110, 120, false)];
        let starts = vec![
            Start::Boot,
            Start::Timer {
                ns: 100,
                syscall: Some(230),
            },
        ];
        let g = graph(runs, starts);
        let chain = g.chain(&Nothing);
        assert_eq!(chain.ns, 120);
        assert!(
            chain
                .pieces
                .iter()
                .any(|p| p.kind == PieceKind::Sleep { syscall: Some(230) } && p.ns == 100)
        );
        assert_eq!(g.finish(&Sleeps(&|_, _| true)), 20);
        // Its computing alone, without the sleep: 100.
        assert_eq!(g.finish(&Computing(&|_, _| true)), 100);
    }

    #[test]
    fn a_sleep_starts_where_its_thread_fell_asleep() {
        // One thread computes from step 0 to 10, sleeps 100 on a timer and
        // computes from 110 to 120. The pieces start at steps 0, 10 and
        // 110: the sleep where the thread left the CPU, not at the run it
        // woke for.
        let runs = vec![run(1, 0, 10, false), run(1, 110, 120, false)];
        let starts = vec![
            Start::Boot,
            Start::Timer {
                ns: 100,
                syscall: Some(230),
            },
        ];
        let g = graph(runs, starts);
        let chain = g.chain(&Nothing);
        let steps: Vec<u64> = chain.pieces.iter().map(|p| g.piece_step(p)).collect();
        assert_eq!(steps, vec![0, 10, 110]);
    }
}
