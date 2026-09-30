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
    /// The command line of its most recent exec, or empty if it never
    /// exec'd.
    pub argv: Vec<String>,
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
                    }
                }
                EventKind::Exec { argv, .. } => {
                    entry(&mut procs, &mut order, e.pid, 0, 0);
                    procs.get_mut(&e.pid).unwrap().argv = argv.clone();
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
}
