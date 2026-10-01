//! What a run did, step by step.
//!
//! A run's trace is every record its guest emitted, each with the step it
//! arrived on. Because the run is deterministic, the trace is also an index
//! into it: the state at step N is whatever the events up to N describe,
//! and the machine itself can be brought back to step N to look further.
//!
//! This crate reads and writes traces and answers the questions the
//! scrubber asks of one: which processes were alive at a step, what they
//! had printed, which files they had written, which phase the build was
//! in, and where two runs first went different ways.

mod event;

use std::collections::BTreeMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

pub use event::{DecodeError, Event, EventKind, HEADER_LEN, signal_name};

/// Every event of a run, in step order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    pub events: Vec<Event>,
}

/// A process's life, as the trace records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The process that forked it; 0 for the first process.
    pub parent: u32,
    /// The command line of its most recent exec, or, if it never exec'd,
    /// its parent's: a forked child runs its parent's program. Empty for
    /// processes nothing exec'd before them, such as kernel threads.
    pub argv: Vec<String>,
    /// Whether `argv` is from its own exec rather than its parent's.
    pub execd: bool,
    /// The steps it was forked and exited on; `end` is None while it runs.
    pub start: u64,
    pub end: Option<u64>,
    /// The kernel's exit_code: status in bits 8 to 15, or the terminating
    /// signal in bits 0 to 6.
    pub status: Option<u32>,
    /// Threads other than the main one, with the steps they started and
    /// ended on.
    pub threads: Vec<(u32, u64, Option<u64>)>,
}

impl Process {
    pub fn alive_at(&self, step: u64) -> bool {
        self.start <= step && self.end.is_none_or(|end| step < end)
    }

    /// A short name for display: the command, or the pid if it never
    /// exec'd.
    pub fn name(&self) -> String {
        match self.argv.first() {
            Some(cmd) => cmd.clone(),
            None => format!("pid {}", self.pid),
        }
    }
}

/// A named stretch of the run: a nixpkgs build phase, or the span between
/// two marks written to /dev/rewind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: String,
    pub start: u64,
    pub end: u64,
}

/// Where two traces first disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The index of the first differing event in each trace.
    pub index: usize,
    /// The steps of the first differing events, or of the end of a trace
    /// that ran out first.
    pub left_step: u64,
    pub right_step: u64,
}

/// A line of output, reassembled from the writes that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The step the line was completed on.
    pub step: u64,
    pub pid: u32,
    pub fd: u32,
    pub text: String,
}

impl Trace {
    /// Reads a trace file: each event is its step as eight little-endian
    /// bytes followed by the record exactly as the guest wrote it.
    pub fn read(path: &Path) -> io::Result<Trace> {
        let mut r = BufReader::new(std::fs::File::open(path)?);
        let mut events = Vec::new();
        loop {
            let mut step = [0u8; 8];
            match r.read_exact(&mut step) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let mut len = [0u8; 4];
            r.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len) as usize;
            if len < HEADER_LEN {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "short record"));
            }
            let mut record = vec![0u8; len];
            record[..4].copy_from_slice(&(len as u32).to_le_bytes());
            r.read_exact(&mut record[4..])?;
            let event = Event::decode(u64::from_le_bytes(step), &record)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            events.push(event);
        }
        Ok(Trace { events })
    }

    /// The last step anything happened on.
    pub fn last_step(&self) -> u64 {
        self.events.last().map_or(0, |e| e.step)
    }

    /// The index of the first event after `step`.
    pub fn index_after(&self, step: u64) -> usize {
        self.events.partition_point(|e| e.step <= step)
    }

    /// Events up to and including `step`.
    pub fn until(&self, step: u64) -> &[Event] {
        &self.events[..self.index_after(step)]
    }

    /// The step of the event before `step`, if any.
    pub fn previous_step(&self, step: u64) -> Option<u64> {
        let i = self.events.partition_point(|e| e.step < step);
        i.checked_sub(1).map(|i| self.events[i].step)
    }

    /// The step of the event after `step`, if any.
    pub fn next_step(&self, step: u64) -> Option<u64> {
        self.events.get(self.index_after(step)).map(|e| e.step)
    }

    /// Every process the trace saw, in the order they started.
    pub fn processes(&self) -> Vec<Process> {
        let mut procs: BTreeMap<u32, Process> = BTreeMap::new();
        let mut order: Vec<u32> = Vec::new();
        let entry =
            |procs: &mut BTreeMap<u32, Process>, order: &mut Vec<u32>, pid, parent, step| {
                procs.entry(pid).or_insert_with(|| {
                    order.push(pid);
                    Process {
                        pid,
                        parent,
                        argv: Vec::new(),
                        execd: false,
                        start: step,
                        end: None,
                        status: None,
                        threads: Vec::new(),
                    }
                });
            };

        for e in &self.events {
            match &e.kind {
                EventKind::Fork { child, thread } => {
                    entry(&mut procs, &mut order, e.pid, 0, 0);
                    if *thread {
                        if let Some(p) = procs.get_mut(&e.pid) {
                            p.threads.push((*child, e.step, None));
                        }
                    } else {
                        // A pid can be reused once its process has exited.
                        if procs.get(child).is_some_and(|p| p.end.is_some()) {
                            let old = procs.remove(child).unwrap();
                            let key = u32::MAX - order.len() as u32;
                            procs.insert(
                                key,
                                Process {
                                    pid: old.pid,
                                    ..old
                                },
                            );
                            if let Some(slot) = order.iter_mut().rev().find(|p| **p == *child) {
                                *slot = key;
                            }
                        }
                        entry(&mut procs, &mut order, *child, e.pid, e.step);

                        // A forked child runs its parent's program until it
                        // execs, and Linux shows it under that command line.
                        let parent_argv = procs.get(&e.pid).map(|p| p.argv.clone());
                        if let (Some(argv), Some(c)) = (parent_argv, procs.get_mut(child))
                            && c.argv.is_empty()
                        {
                            c.argv = argv;
                        }
                    }
                }
                EventKind::Exec { argv, .. } => {
                    entry(&mut procs, &mut order, e.pid, 0, 0);
                    let p = procs.get_mut(&e.pid).unwrap();
                    p.argv = argv.clone();
                    p.execd = true;
                }
                EventKind::Exit { status, thread, .. } => {
                    if let Some(p) = procs.get_mut(&e.pid) {
                        if *thread {
                            if let Some(t) = p.threads.iter_mut().find(|t| t.0 == e.tid) {
                                t.2 = Some(e.step);
                            }
                        } else {
                            p.end = Some(e.step);
                            p.status = Some(*status);
                        }
                    }
                }
                _ => {}
            }
        }
        order
            .into_iter()
            .filter_map(|pid| procs.remove(&pid))
            .collect()
    }

    /// Output lines completed by `step`, both streams interleaved in the
    /// order they were written. A write without a trailing newline is held
    /// until the rest of its line arrives, as a terminal would show it.
    pub fn lines_until(&self, step: u64) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut pending: BTreeMap<(u32, u32), String> = BTreeMap::new();
        for e in self.until(step) {
            let EventKind::Output { fd, bytes } = &e.kind else {
                continue;
            };
            let buf = pending.entry((e.pid, *fd)).or_default();
            buf.push_str(&String::from_utf8_lossy(bytes));
            while let Some(nl) = buf.find('\n') {
                let text: String = buf.drain(..=nl).collect();
                lines.push(Line {
                    step: e.step,
                    pid: e.pid,
                    fd: *fd,
                    text: text.trim_end_matches('\n').to_string(),
                });
            }
        }
        lines
    }

    /// Phases, from nixpkgs' "Running phase: X" lines and from marks.
    pub fn phases(&self) -> Vec<Phase> {
        const NIX_PHASE: &str = "Running phase: ";
        let mut starts: Vec<(u64, String)> = Vec::new();
        for line in self.lines_until(u64::MAX) {
            if let Some(name) = line.text.trim().strip_prefix(NIX_PHASE) {
                starts.push((line.step, name.trim().to_string()));
            }
        }
        for e in &self.events {
            if let EventKind::Mark { text } = &e.kind {
                starts.push((e.step, text.clone()));
            }
        }
        starts.sort_by_key(|(step, _)| *step);

        let last = self.last_step();
        let mut phases = Vec::new();
        for (i, (start, name)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map_or(last, |(s, _)| *s);
            phases.push(Phase {
                name: name.clone(),
                start: *start,
                end,
            });
        }
        phases
    }

    /// The first place two traces differ. None when they are identical,
    /// which for two runs of the same inputs is always the answer.
    pub fn divergence(&self, other: &Trace) -> Option<Divergence> {
        let n = self.events.len().min(other.events.len());
        let index = (0..n)
            .find(|&i| self.events[i] != other.events[i])
            .or((self.events.len() != other.events.len()).then_some(n))?;
        let step_of = |t: &Trace| t.events.get(index).map_or(t.last_step(), |e| e.step);
        Some(Divergence {
            index,
            left_step: step_of(self),
            right_step: step_of(other),
        })
    }

    /// The command line of the process a failed run's failure came from:
    /// the first to receive a fatal signal, else the first to exit
    /// non-zero.
    pub fn culprit(&self) -> Option<Vec<String>> {
        self.failures().into_iter().next().map(|(argv, _)| argv)
    }

    /// The culprit of this run's failure, given `other`, a run of the same
    /// inputs that ended differently: the first program here to end badly
    /// that did not end the same way in `other`. A configure probe that
    /// exits non-zero in every build is skipped. Falls back to
    /// [`Trace::culprit`] when every failure here also happened there.
    pub fn culprit_against(&self, other: &Trace) -> Option<Vec<String>> {
        let theirs = other.failures();
        self.failures()
            .into_iter()
            .find(|failure| !theirs.contains(failure))
            .map(|(argv, _)| argv)
            .or_else(|| self.culprit())
    }

    /// Every program that ended badly, with how: those killed by a fatal
    /// signal first, then those that exited non-zero, each in order.
    fn failures(&self) -> Vec<(Vec<String>, Ending)> {
        let procs = self.processes();
        let argv_of = |pid: u32| {
            procs
                .iter()
                .rev()
                .find(|p| p.pid == pid && !p.argv.is_empty())
                .map(|p| p.argv.clone())
        };
        let signalled = self.events.iter().filter_map(|e| match &e.kind {
            EventKind::Signal { signo, .. } if FATAL_SIGNALS.contains(signo) => {
                Some((argv_of(e.pid)?, Ending::Signal(*signo)))
            }
            _ => None,
        });
        let exited = self.events.iter().filter_map(|e| match &e.kind {
            EventKind::Exit {
                status,
                thread: false,
                ..
            } if *status != 0 => Some((argv_of(e.pid)?, Ending::Exit(*status))),
            _ => None,
        });
        signalled.chain(exited).collect()
    }

    /// The events of the processes running `argv`, each with its thread
    /// numbered by the order the program's threads first appear.
    pub fn program_events(&self, argv: &[String]) -> ProgramEvents {
        let pids: Vec<u32> = self
            .processes()
            .iter()
            .filter(|p| p.argv == argv)
            .map(|p| p.pid)
            .collect();
        let mut numbers: BTreeMap<u32, usize> = BTreeMap::new();
        let mut events = ProgramEvents::default();
        for (index, e) in self.events.iter().enumerate() {
            if !pids.contains(&e.pid) {
                continue;
            }
            let next = numbers.len();
            events.indices.push(index);
            events.threads.push(*numbers.entry(e.tid).or_insert(next));
        }
        events
    }

    /// Where one program's own events first differ between two traces.
    /// Only events of processes running `argv` count, steps are ignored,
    /// and threads are compared by the order they first appear rather
    /// than by their ids, which a rescheduled run may hand out
    /// differently. None when the program did the same things in the same
    /// order in both.
    pub fn divergence_in(&self, other: &Trace, argv: &[String]) -> Option<ProgramDivergence> {
        let (left, right) = (self.program_events(argv), other.program_events(argv));
        let same = |i: usize| {
            left.threads[i] == right.threads[i]
                && self.events[left.indices[i]].kind == other.events[right.indices[i]].kind
        };
        let n = left.indices.len().min(right.indices.len());
        let position = (0..n)
            .find(|&i| !same(i))
            .or((left.indices.len() != right.indices.len()).then_some(n))?;
        Some(ProgramDivergence {
            position,
            left,
            right,
        })
    }
}

/// Signals that end a process unless it handles them: SIGILL, SIGABRT,
/// SIGBUS, SIGFPE and SIGSEGV.
pub const FATAL_SIGNALS: [u32; 5] = [4, 6, 7, 8, 11];

/// How a program ended badly, for comparing failures between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    Signal(u32),
    Exit(u32),
}

/// One program's events in a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramEvents {
    /// Indices into the trace's events, in order.
    pub indices: Vec<usize>,
    /// The thread number of each event: 0 for the program's first thread
    /// to appear, 1 for the next, and so on.
    pub threads: Vec<usize>,
}

/// Where one program's own events first differ between two traces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramDivergence {
    /// How many of the program's events matched before the difference.
    pub position: usize,
    pub left: ProgramEvents,
    pub right: ProgramEvents,
}

impl ProgramDivergence {
    /// The left trace's first differing event, as an index into its
    /// events and the event's thread number; None when the program's
    /// events ran out there first.
    pub fn left_event(&self) -> Option<(usize, usize)> {
        Self::at(&self.left, self.position)
    }

    /// The right trace's first differing event, as `left_event`.
    pub fn right_event(&self) -> Option<(usize, usize)> {
        Self::at(&self.right, self.position)
    }

    fn at(events: &ProgramEvents, position: usize) -> Option<(usize, usize)> {
        Some((
            *events.indices.get(position)?,
            *events.threads.get(position)?,
        ))
    }
}

/// Appends events to a trace file as the guest emits them.
pub struct TraceWriter<W: Write> {
    out: BufWriter<W>,
}

impl<W: Write> TraceWriter<W> {
    pub fn new(out: W) -> Self {
        TraceWriter {
            out: BufWriter::new(out),
        }
    }

    pub fn record(&mut self, step: u64, record: &[u8]) -> io::Result<()> {
        self.out.write_all(&step.to_le_bytes())?;
        self.out.write_all(record)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        self.out.into_inner().map_err(|e| e.into_error())
    }
}

#[cfg(test)]
mod tests {
    // Trace queries over a small hand-built run: a shell forks make, make
    // forks two compilers, one thread comes and goes, and a test segfaults.
    use super::*;

    fn ev(step: u64, pid: u32, tid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid,
            kind,
        }
    }

    fn out(step: u64, pid: u32, text: &str) -> Event {
        ev(
            step,
            pid,
            pid,
            EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        )
    }

    fn sample() -> Trace {
        use EventKind::*;
        Trace {
            events: vec![
                ev(
                    1,
                    1,
                    1,
                    Exec {
                        filename: "/bin/sh".into(),
                        argv: vec!["sh".into()],
                        old_pid: 1,
                    },
                ),
                out(2, 1, "Running phase: buildPhase\n"),
                ev(
                    3,
                    1,
                    1,
                    Fork {
                        child: 2,
                        thread: false,
                    },
                ),
                ev(
                    4,
                    2,
                    2,
                    Exec {
                        filename: "/bin/make".into(),
                        argv: vec!["make".into()],
                        old_pid: 2,
                    },
                ),
                ev(
                    5,
                    2,
                    2,
                    Fork {
                        child: 3,
                        thread: false,
                    },
                ),
                out(6, 3, "compil"),
                out(7, 3, "ing\n"),
                ev(
                    8,
                    3,
                    3,
                    Exit {
                        status: 0,
                        comm: "cc".into(),
                        thread: false,
                    },
                ),
                ev(
                    9,
                    2,
                    2,
                    Fork {
                        child: 4,
                        thread: true,
                    },
                ),
                ev(
                    10,
                    2,
                    4,
                    Exit {
                        status: 0,
                        comm: "make".into(),
                        thread: true,
                    },
                ),
                out(11, 1, "Running phase: checkPhase\n"),
                ev(
                    12,
                    2,
                    2,
                    Signal {
                        signo: 11,
                        code: 1,
                        addr: 0,
                    },
                ),
                ev(
                    13,
                    2,
                    2,
                    Exit {
                        status: 11,
                        comm: "make".into(),
                        thread: false,
                    },
                ),
            ],
        }
    }

    #[test]
    fn processes_have_parents_lifetimes_and_threads() {
        let procs = sample().processes();
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1, 2, 3]);
        let make = &procs[1];
        assert_eq!(make.parent, 1);
        assert_eq!(make.argv, vec!["make".to_string()]);
        assert_eq!((make.start, make.end, make.status), (3, Some(13), Some(11)));
        assert_eq!(make.threads, vec![(4, 9, Some(10))]);
        assert!(procs[2].alive_at(7));
        assert!(!procs[2].alive_at(8));
    }

    #[test]
    fn lines_are_reassembled_from_partial_writes() {
        let t = sample();
        let lines = t.lines_until(6);
        assert_eq!(lines.len(), 1);
        let lines = t.lines_until(7);
        assert_eq!(lines[1].text, "compiling");
        assert_eq!(lines[1].step, 7);
    }

    #[test]
    fn phases_come_from_nixpkgs_output() {
        let phases = sample().phases();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "buildPhase");
        assert_eq!((phases[0].start, phases[0].end), (2, 11));
        assert_eq!((phases[1].start, phases[1].end), (11, 13));
    }

    #[test]
    fn divergence_finds_the_first_differing_event() {
        let a = sample();
        assert_eq!(a.divergence(&a.clone()), None);

        let mut b = a.clone();
        b.events[5] = out(6, 3, "different");
        let d = a.divergence(&b).unwrap();
        assert_eq!((d.index, d.left_step, d.right_step), (5, 6, 6));

        let mut c = a.clone();
        c.events.truncate(10);
        let d = a.divergence(&c).unwrap();
        assert_eq!((d.index, d.left_step, d.right_step), (10, 11, 10));
    }

    #[test]
    fn navigation_moves_between_event_steps() {
        let t = sample();
        assert_eq!(t.next_step(7), Some(8));
        assert_eq!(t.previous_step(8), Some(7));
        assert_eq!(t.previous_step(1), None);
        assert_eq!(t.until(5).len(), 5);
    }

    #[test]
    fn a_written_trace_reads_back() {
        let mut w = TraceWriter::new(Vec::new());
        let mut record = Vec::new();
        record.extend_from_slice(&((HEADER_LEN + 3) as u32).to_le_bytes());
        record.extend_from_slice(&10u16.to_le_bytes());
        record.extend_from_slice(&[0u8; 14]);
        record.extend_from_slice(b"hi\n");
        w.record(99, &record).unwrap();
        let bytes = w.finish().unwrap();

        let dir = std::env::temp_dir().join(format!("rewind-trace-{}", std::process::id()));
        std::fs::write(&dir, bytes).unwrap();
        let t = Trace::read(&dir).unwrap();
        std::fs::remove_file(&dir).unwrap();
        assert_eq!(t.events.len(), 1);
        assert_eq!(t.events[0].step, 99);
        assert_eq!(t.events[0].kind, EventKind::Mark { text: "hi".into() });
    }

    /// A test program run as pid 7 with two worker threads, 8 and 9, each
    /// writing a line; the steps and the order the lines come in are the
    /// caller's.
    fn pool(steps: [u64; 6], first: (u32, &str), second: (u32, &str), crash: Option<u32>) -> Trace {
        use EventKind::*;
        let fork = |step, child| {
            ev(
                step,
                7,
                7,
                Fork {
                    child,
                    thread: true,
                },
            )
        };
        let write = |step, tid, text: &str| {
            ev(
                step,
                7,
                tid,
                Output {
                    fd: 1,
                    bytes: text.as_bytes().to_vec(),
                },
            )
        };
        let mut events = vec![
            ev(
                steps[0],
                1,
                1,
                Fork {
                    child: 7,
                    thread: false,
                },
            ),
            ev(
                steps[1],
                7,
                7,
                Exec {
                    filename: "/t/pool".into(),
                    argv: vec!["./pool".into()],
                    old_pid: 7,
                },
            ),
            fork(steps[2], 8),
            fork(steps[3], 9),
            write(steps[4], first.0, first.1),
            write(steps[5], second.0, second.1),
        ];
        if let Some(tid) = crash {
            events.push(ev(
                steps[5] + 1,
                7,
                tid,
                Signal {
                    signo: 11,
                    code: 1,
                    addr: 0x108,
                },
            ));
        }
        Trace { events }
    }

    #[test]
    fn a_step_shift_is_not_where_a_program_diverges() {
        // Two runs where every event of the program is the same but moved
        // a few steps by a reschedule, until the second write comes from
        // the other thread. The raw comparison stops at the first shifted
        // step; the program's comparison finds the write.
        let passing = pool([1, 2, 3, 4, 5, 6], (8, "a\n"), (9, "b\n"), None);
        let failing = pool([1, 2, 5, 6, 9, 12], (8, "a\n"), (8, "b\n"), Some(8));

        let raw = failing.divergence(&passing).unwrap();
        assert_eq!(raw.index, 2);
        assert_eq!(failing.events[2].kind, passing.events[2].kind);

        let argv = failing.culprit().unwrap();
        assert_eq!(argv, vec!["./pool".to_string()]);
        let d = failing.divergence_in(&passing, &argv).unwrap();
        assert_eq!(d.position, 4);
        assert_eq!(d.left_event(), Some((5, 1)));
        assert_eq!(d.right_event(), Some((5, 2)));
    }

    #[test]
    fn threads_are_compared_by_the_order_they_appear() {
        // The same writes from threads with other ids do not diverge, and
        // neither do the writes from the two threads swapped: each run
        // numbers its first writer 1 and its second 2.
        let argv = vec!["./pool".to_string()];
        let a = pool([1, 2, 3, 4, 5, 6], (8, "a\n"), (9, "b\n"), None);
        let renumbered = pool([1, 2, 3, 4, 5, 6], (18, "a\n"), (19, "b\n"), None);
        assert_eq!(a.divergence_in(&renumbered, &argv), None);
        assert_eq!(
            a.program_events(&argv).threads,
            renumbered.program_events(&argv).threads
        );

        let swapped = pool([1, 2, 3, 4, 5, 6], (9, "a\n"), (8, "b\n"), None);
        assert_eq!(a.divergence_in(&swapped, &argv), None);
    }

    /// A process that execs `argv` as `pid`, forked from `parent`, and
    /// ends at `end` with `ending`.
    fn program(
        step: u64,
        parent: u32,
        pid: u32,
        argv: &[&str],
        ending: Option<EventKind>,
    ) -> Vec<Event> {
        let mut events = vec![
            ev(
                step,
                parent,
                parent,
                EventKind::Fork {
                    child: pid,
                    thread: false,
                },
            ),
            ev(
                step + 1,
                pid,
                pid,
                EventKind::Exec {
                    filename: argv[0].into(),
                    argv: argv.iter().map(|a| a.to_string()).collect(),
                    old_pid: pid,
                },
            ),
        ];
        if let Some(kind) = ending {
            events.push(ev(step + 2, pid, pid, kind));
        }
        events
    }

    fn exit(status: u32) -> Option<EventKind> {
        Some(EventKind::Exit {
            status,
            comm: String::new(),
            thread: false,
        })
    }

    #[test]
    fn the_culprit_skips_a_failure_both_runs_share() {
        // A configure probe exits 1 in both runs; only the failing run's
        // test exits 2, so the test is the culprit.
        let probe = || program(10, 1, 5, &["cc", "-E", "probe.c"], exit(1));
        let mut failing = probe();
        failing.extend(program(20, 1, 6, &["./test"], exit(2)));
        let mut passing = probe();
        passing.extend(program(20, 1, 6, &["./test"], exit(0)));
        let (failing, passing) = (Trace { events: failing }, Trace { events: passing });

        assert_eq!(
            failing.culprit(),
            Some(vec!["cc".into(), "-E".into(), "probe.c".into()])
        );
        assert_eq!(
            failing.culprit_against(&passing),
            Some(vec!["./test".into()])
        );
    }

    #[test]
    fn a_forked_child_shows_its_parents_command_until_it_execs() {
        // A shell forks a subshell that never execs: it is the shell, not
        // a kernel thread.
        let mut events = program(1, 1, 2, &["bash", "test.sh"], None);
        events.push(ev(
            5,
            2,
            2,
            EventKind::Fork {
                child: 3,
                thread: false,
            },
        ));
        let trace = Trace { events };
        let sub = trace.processes().into_iter().find(|p| p.pid == 3).unwrap();
        assert_eq!(sub.argv, vec!["bash".to_string(), "test.sh".to_string()]);
    }
}
