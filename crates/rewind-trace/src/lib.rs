//! What a run did, step by step.
//!
//! A run's trace is every record its guest emitted, each with the step it
//! arrived on. Because the run is deterministic, the trace is also an index
//! into it: the state at step N is whatever the events up to N describe,
//! and the machine itself can be brought back to step N to look further.
//!
//! This crate reads and writes traces and answers the questions the
//! scrubber asks of one: which processes were alive at a step, what they
//! had printed, which files they had written, and where two runs first
//! went different ways. It also holds what the engine records beside a
//! trace and the app reads: the manifest, and how a run stopped and
//! ended. A trace file also holds the run's read and write records,
//! which [`io_records`] decodes, and its context switches, which
//! [`switches`] decodes; a [`Trace`] keeps the events apart from both.

// Only rewind-vmm and the command touch memory and the kernel directly.
#![forbid(unsafe_code)]

pub mod blamed;
pub mod compare;
pub mod contents;
pub mod ending;
mod event;
pub mod export;
pub mod failure;
pub mod file_diff;
pub mod io_records;
pub mod located;
pub mod machine;
pub mod manifest;
pub mod properties;
pub mod prune;
pub mod replaying;
pub mod schedule;
pub mod sdk;
pub mod searching;
pub mod slices;
pub mod stop;
pub mod switches;
pub mod verdict;
pub mod writes;

use std::collections::BTreeMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

pub use event::{DecodeError, Event, EventKind, HEADER_LEN, RECORD_MAX, signal_name};
/// Init's marks, which every trace holds, for readers that do not depend
/// on the init themselves.
pub use rewind_init::Mark;
/// A directory's listing as init answers it and `rewind files --json`
/// prints it, for the same readers.
pub use rewind_init::{Listed, ListedAll, ListedKind, Listing};

/// The words, before the step, that a replay making other records than its
/// run made is reported with: the engine writes them and the desktop app
/// recognizes them, as the sign that this build of rewind runs the run's
/// inputs another way than the build that recorded it.
pub const WENT_ANOTHER_WAY: &str = "went another way at step";

/// Every event of a run, in step order, and its context switches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    pub events: Vec<Event>,
    /// Every context switch, in step order: which task the CPU ran from
    /// each step on ([`switches`]). A run recorded by a guest kernel from
    /// before switches were recorded has none.
    pub switches: Vec<switches::Switch>,
}

/// A process's life, as the trace records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The process that forked it; 0 for the first process.
    pub parent: u32,
    /// The command line of its last exec in the run, or, if it never
    /// exec'd, its parent's: a forked child runs its parent's program. Empty
    /// for processes nothing exec'd before them, such as kernel threads.
    /// [Process::as_of] gives the command line at a step instead.
    pub argv: Vec<String>,
    /// Whether `argv` is from its own exec rather than its parent's.
    pub execd: bool,
    /// The command line it was forked with: its parent's at the fork.
    pub forked_argv: Vec<String>,
    /// Each of its execs, with the step it happened on, in order.
    pub execs: Vec<(u64, Vec<String>)>,
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

    /// The step its memory began at, as of `step`: its last exec on or
    /// before `step`, else the step it was forked on. Before then the
    /// memory was another program's.
    pub fn memory_began(&self, step: u64) -> u64 {
        self.execs
            .iter()
            .rev()
            .map(|(at, _)| *at)
            .find(|&at| at <= step)
            .unwrap_or(self.start)
    }

    /// The process as it was at `step`: `argv` and `execd` describe its
    /// last exec on or before `step`, or the command line it was forked
    /// with if it had not exec'd yet.
    pub fn as_of(&self, step: u64) -> Process {
        let mut process = self.clone();
        match self.execs.iter().rev().find(|(at, _)| *at <= step) {
            Some((_, argv)) => {
                process.argv = argv.clone();
                process.execd = true;
            }
            None => {
                process.argv = self.forked_argv.clone();
                process.execd = false;
            }
        }
        process
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

/// Where two traces first disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The index of the first differing event in each trace, or of the
    /// first differing trace record of read and write records when `part`
    /// says so.
    pub index: usize,
    /// The steps of the first differing events, or of the end of a trace
    /// that ran out first.
    pub left_step: u64,
    pub right_step: u64,
    pub part: Part,
}

/// Which records of two traces a [`Divergence`] is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Events,
    /// The read and write records ([`io_records`]), with the events the
    /// same.
    IoRecords,
    /// The context switches ([`switches`]), with the events the same.
    Switches,
}

/// The job's exit, as init reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobExit {
    pub step: u64,
    /// The job's wait status.
    pub status: i32,
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
        Trace::decode(&records(path)?)
    }

    /// Reads a trace from the bytes of a trace file, held in memory: the
    /// browser viewer has no files to open.
    pub fn from_bytes(bytes: &[u8]) -> io::Result<Trace> {
        Trace::decode(&records_from(bytes)?)
    }

    /// The trace of a file's records, as [`records`] reads them. The read
    /// and write records among them are not events, and are left to
    /// [`crate::io_records::decode`]; the batches of switches are decoded
    /// into `switches`.
    pub fn decode(records: &[(u64, Vec<u8>)]) -> io::Result<Trace> {
        Trace::decode_from(records, switches::Decoder::default())
    }

    /// The trace of records a machine restored at a step wrote from there
    /// on, whose batches of switches name tasks the run's trace defined
    /// before: `decoder` is [`switches::Decoder::resume`] at that step.
    pub fn decode_from(
        records: &[(u64, Vec<u8>)],
        mut decoder: switches::Decoder,
    ) -> io::Result<Trace> {
        let invalid = |e: DecodeError| io::Error::new(io::ErrorKind::InvalidData, e);
        let mut events = Vec::with_capacity(records.len());
        let mut switches = Vec::new();
        for (step, record) in records {
            if io_records::is_io(record) {
                continue;
            }
            if switches::is_batch(record) {
                decoder
                    .decode(*step, record, &mut switches)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                continue;
            }
            events.push(Event::decode(*step, record).map_err(invalid)?);
        }
        Ok(Trace { events, switches })
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

    /// The step the job started on, from init's start mark: before it the
    /// VM is still booting.
    pub fn job_start(&self) -> Option<u64> {
        self.events
            .iter()
            .find(|e| e.init_mark() == Some(Mark::Start))
            .map(|e| e.step)
    }

    /// The job's exit, from init's exit mark.
    pub fn job_exit(&self) -> Option<JobExit> {
        self.events.iter().find_map(|e| match e.init_mark()? {
            Mark::Exit { status } => Some(JobExit {
                step: e.step,
                status,
            }),
            _ => None,
        })
    }

    /// What the job built, as init reported after it succeeded: each
    /// output's path and tree hash.
    pub fn outputs(&self) -> Vec<(String, String)> {
        self.events
            .iter()
            .filter_map(|e| match e.init_mark()? {
                Mark::Output { path, hash } => Some((path, hash)),
                _ => None,
            })
            .collect()
    }

    /// Every process the trace saw, in the order they started.
    pub fn processes(&self) -> Vec<Process> {
        let mut procs: BTreeMap<u32, Process> = BTreeMap::new();
        let mut order: Vec<u32> = Vec::new();
        fn entry<'a>(
            procs: &'a mut BTreeMap<u32, Process>,
            order: &mut Vec<u32>,
            pid: u32,
            parent: u32,
            step: u64,
        ) -> &'a mut Process {
            procs.entry(pid).or_insert_with(|| {
                order.push(pid);
                Process {
                    pid,
                    parent,
                    argv: Vec::new(),
                    execd: false,
                    forked_argv: Vec::new(),
                    execs: Vec::new(),
                    start: step,
                    end: None,
                    status: None,
                    threads: Vec::new(),
                }
            })
        }

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
                        if procs.get(child).is_some_and(|p| p.end.is_some())
                            && let Some(old) = procs.remove(child)
                        {
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
                            c.forked_argv = argv.clone();
                            c.argv = argv;
                        }
                    }
                }
                EventKind::Exec { argv, .. } => {
                    let p = entry(&mut procs, &mut order, e.pid, 0, 0);
                    p.argv = argv.clone();
                    p.execd = true;
                    p.execs.push((e.step, argv.clone()));
                }
                EventKind::Exit { status, thread, .. } => {
                    if let Some(p) = procs.get_mut(&e.pid) {
                        if *thread {
                            // Linux hands a thread id out again once its
                            // thread has exited, so the exit ends the
                            // newest lifetime with the id still live.
                            if let Some(t) = p
                                .threads
                                .iter_mut()
                                .rev()
                                .find(|t| t.0 == e.tid && t.2.is_none())
                            {
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

    /// The process that had id `pid` at `step`: the one alive then, else
    /// the nearest before it, else the first after it. A pid is given out
    /// again once its process has exited, so the newest process with an id
    /// is not always the one a step means. It is returned as it was at the
    /// step ([Process::as_of]), running the program it had exec'd by then.
    pub fn process_at(&self, pid: u32, step: u64) -> Option<Process> {
        process_in(&self.processes(), pid, step).map(|p| p.as_of(step))
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

    /// The first place two traces differ record for record, steps
    /// included. None when they are identical, which for two runs of the
    /// same inputs is always the answer. Where two runs first behave
    /// differently is [`compare::Comparison`].
    pub fn divergence(&self, other: &Trace) -> Option<Divergence> {
        let first_differing = |a: usize, b: usize, differs: &dyn Fn(usize) -> bool| {
            let n = a.min(b);
            (0..n).find(|&i| differs(i)).or((a != b).then_some(n))
        };

        // The events first, then the switches.
        let events = first_differing(self.events.len(), other.events.len(), &|i| {
            self.events[i] != other.events[i]
        });
        if let Some(index) = events {
            let step_of = |t: &Trace| t.events.get(index).map_or(t.last_step(), |e| e.step);
            return Some(Divergence {
                index,
                left_step: step_of(self),
                right_step: step_of(other),
                part: Part::Events,
            });
        }
        let index = first_differing(self.switches.len(), other.switches.len(), &|i| {
            self.switches[i] != other.switches[i]
        })?;
        let step_of = |t: &Trace| t.switches.get(index).map_or(t.last_step(), |s| s.step);
        Some(Divergence {
            index,
            left_step: step_of(self),
            right_step: step_of(other),
            part: Part::Switches,
        })
    }

    /// The trace after `step`: the events and switches at later steps.
    pub fn after(&self, step: u64) -> Trace {
        Trace {
            events: self.events[self.index_after(step)..].to_vec(),
            switches: self
                .switches
                .iter()
                .filter(|s| s.step > step)
                .cloned()
                .collect(),
        }
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
        let argv_of = |pid: u32, step: u64| {
            process_in(&procs, pid, step)
                .filter(|p| !p.argv.is_empty())
                .map(|p| p.argv.clone())
        };
        let signalled = self.events.iter().filter_map(|e| match &e.kind {
            EventKind::Signal { signo, .. } if signal::FATAL.contains(signo) => {
                Some((argv_of(e.pid, e.step)?, Ending::Signal(*signo)))
            }
            _ => None,
        });
        let exited = self.events.iter().filter_map(|e| match &e.kind {
            EventKind::Exit {
                status,
                thread: false,
                ..
            } if *status != 0 => Some((argv_of(e.pid, e.step)?, Ending::Exit(*status))),
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
    /// differently; so are the ids forks and execs name (see
    /// [`EventKind::alike`]). None when the program did the same things in
    /// the same order in both.
    pub fn divergence_in(&self, other: &Trace, argv: &[String]) -> Option<ProgramDivergence> {
        let (left, right) = (self.program_events(argv), other.program_events(argv));
        let same = |i: usize| {
            left.threads[i] == right.threads[i]
                && self.events[left.indices[i]]
                    .kind
                    .alike(&other.events[right.indices[i]].kind)
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

/// The process among `procs` that had id `pid` at `step`, as
/// [`Trace::process_at`] finds it.
fn process_in(procs: &[Process], pid: u32, step: u64) -> Option<&Process> {
    let with_pid = procs.iter().filter(|p| p.pid == pid);
    if let Some(alive) = with_pid.clone().find(|p| p.alive_at(step)) {
        return Some(alive);
    }
    let before = with_pid
        .clone()
        .filter(|p| p.start <= step)
        .max_by_key(|p| p.start);
    before.or_else(|| with_pid.min_by_key(|p| p.start))
}

/// The signals a crash is made of.
pub mod signal {
    pub const SIGILL: u32 = 4;
    pub const SIGABRT: u32 = 6;
    pub const SIGBUS: u32 = 7;
    pub const SIGFPE: u32 = 8;
    pub const SIGSEGV: u32 = 11;

    /// Signals that end a process unless it handles them; one delivered
    /// means the process crashed.
    pub const FATAL: [u32; 5] = [SIGSEGV, SIGBUS, SIGABRT, SIGILL, SIGFPE];
}

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

/// What a trace file starts with: this magic, then the version of its
/// format as a little-endian u32. Its records follow.
pub const TRACE_MAGIC: &[u8; 8] = b"rwtrace\0";
pub const TRACE_VERSION: u32 = 1;
const TRACE_HEADER_LEN: usize = TRACE_MAGIC.len() + 4;

/// A trace file's records as the guest wrote them, each with its step,
/// undecoded: what a fork copies from its parent's trace for the steps the
/// two runs share. A trace cut off mid-record, as one is when the process
/// writing it is killed, ends at its last whole record.
pub fn records(path: &Path) -> io::Result<Vec<(u64, Vec<u8>)>> {
    records_from(BufReader::new(std::fs::File::open(path)?))
}

/// The records of a trace file's bytes as `r` reads them, as [`records`]
/// returns them.
pub fn records_from(mut r: impl Read) -> io::Result<Vec<(u64, Vec<u8>)>> {
    let mut records = Vec::new();
    let cut_off = |e: &io::Error| e.kind() == io::ErrorKind::UnexpectedEof;

    // The format first; a file cut off before its header is whole has no
    // records yet.
    let mut header = [0u8; TRACE_HEADER_LEN];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if cut_off(&e) => return Ok(records),
        Err(e) => return Err(e),
    }
    let (magic, version) = header.split_at(TRACE_MAGIC.len());
    if magic != TRACE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a trace this build of rewind reads: traces written before rewind 1.0 \
             name no format",
        ));
    }
    let version = u32::from_le_bytes(version.try_into().expect("four bytes"));
    if version != TRACE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a trace of format {version}; this build of rewind reads format {TRACE_VERSION}"
            ),
        ));
    }

    loop {
        let mut step = [0u8; 8];
        match r.read_exact(&mut step) {
            Ok(()) => {}
            Err(e) if cut_off(&e) => break,
            Err(e) => return Err(e),
        }

        // Each record starts with its own length.
        let mut len = [0u8; 4];
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if cut_off(&e) => break,
            Err(e) => return Err(e),
        }
        // A length no record has is corruption, not a cut-off end, and is
        // refused before anything is read or allocated for it.
        let len = u32::from_le_bytes(len) as usize;
        if !(HEADER_LEN..=RECORD_MAX).contains(&len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a record of {len} bytes, which no record is"),
            ));
        }
        let mut record = vec![0u8; len];
        record[..4].copy_from_slice(&(len as u32).to_le_bytes());
        match r.read_exact(&mut record[4..]) {
            Ok(()) => {}
            Err(e) if cut_off(&e) => break,
            Err(e) => return Err(e),
        }
        records.push((u64::from_le_bytes(step), record));
    }
    Ok(records)
}

/// Appends events to a trace file as the guest emits them, after the
/// header naming the trace's format.
#[derive(Debug)]
pub struct TraceWriter<W: Write> {
    out: BufWriter<W>,
    /// Whether the header is written yet.
    headed: bool,
}

impl<W: Write> TraceWriter<W> {
    pub fn new(out: W) -> Self {
        TraceWriter {
            out: BufWriter::new(out),
            headed: false,
        }
    }

    pub fn record(&mut self, step: u64, record: &[u8]) -> io::Result<()> {
        self.head()?;
        self.out.write_all(&step.to_le_bytes())?;
        self.out.write_all(record)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.head()?;
        self.out.flush()?;
        self.out.into_inner().map_err(|e| e.into_error())
    }

    /// Writes the header, once, before anything else.
    fn head(&mut self) -> io::Result<()> {
        if self.headed {
            return Ok(());
        }
        self.out.write_all(TRACE_MAGIC)?;
        self.out.write_all(&TRACE_VERSION.to_le_bytes())?;
        self.headed = true;
        Ok(())
    }
}

#[cfg(test)]
mod format_tests {
    // A trace file names its format before its records. A trace of another
    // format, or one from before traces named theirs, is refused with what
    // it is; a file cut off before its header is whole, as when the run was
    // killed before its first record, is a trace with no records.
    use super::*;

    #[test]
    fn a_trace_names_its_format_first() {
        let path = std::env::temp_dir().join(format!("rewind-format-{}.bin", std::process::id()));
        let mut record = vec![0u8; HEADER_LEN];
        record[..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());

        let mut writer = TraceWriter::new(std::fs::File::create(&path).unwrap());
        writer.record(3, &record).unwrap();
        writer.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..TRACE_MAGIC.len()], TRACE_MAGIC);
        assert_eq!(
            bytes[TRACE_MAGIC.len()..TRACE_HEADER_LEN],
            TRACE_VERSION.to_le_bytes()
        );
        assert_eq!(records(&path).unwrap(), vec![(3, record.clone())]);

        let mut other = bytes.clone();
        other[TRACE_MAGIC.len()..TRACE_HEADER_LEN]
            .copy_from_slice(&(TRACE_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &other).unwrap();
        let err = records(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains(&format!("format {}", TRACE_VERSION + 1))
        );

        std::fs::write(&path, &bytes[TRACE_HEADER_LEN..]).unwrap();
        let err = records(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("before"));

        std::fs::write(&path, &bytes[..TRACE_HEADER_LEN - 1]).unwrap();
        assert_eq!(records(&path).unwrap(), Vec::new());

        TraceWriter::new(std::fs::File::create(&path).unwrap())
            .finish()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes[..TRACE_HEADER_LEN]);
        std::fs::remove_file(&path).unwrap();
    }
}

#[cfg(test)]
mod partial_tests {
    // A trace cut off mid-record, as one is when the process writing it is
    // killed, reads up to its last whole record; one corrupted in the
    // middle is refused.
    use super::*;

    #[test]
    fn a_length_no_record_has_is_refused_without_reading_it() {
        // A whole record, then one whose length field says more than the
        // guest ever writes, then one too short for its header: each is
        // refused as corrupt, rather than read as a cut-off end or met by
        // allocating what the length asks for.
        let path = std::env::temp_dir().join(format!("rewind-corrupt-{}.bin", std::process::id()));
        let record = |len: u32| {
            let mut r = len.to_le_bytes().to_vec();
            r.resize(len as usize, 0);
            r
        };
        for bad_len in [u32::MAX, RECORD_MAX as u32 + 1, HEADER_LEN as u32 - 1] {
            let mut writer = TraceWriter::new(std::fs::File::create(&path).unwrap());
            writer.record(3, &record(HEADER_LEN as u32)).unwrap();
            let mut bytes = writer.finish().unwrap();
            use std::io::Write;
            bytes.write_all(&7u64.to_le_bytes()).unwrap();
            bytes.write_all(&bad_len.to_le_bytes()).unwrap();
            bytes.write_all(&[0u8; 64]).unwrap();
            drop(bytes);
            let err = records(&path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "length {bad_len}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_trace_cut_off_mid_record_reads_its_whole_records() {
        let path = std::env::temp_dir().join(format!("rewind-partial-{}.bin", std::process::id()));
        let record = |len: u32| {
            let mut r = len.to_le_bytes().to_vec();
            r.resize(len as usize, 0);
            r
        };
        let mut writer = TraceWriter::new(std::fs::File::create(&path).unwrap());
        writer.record(3, &record(HEADER_LEN as u32)).unwrap();
        writer.record(5, &record(HEADER_LEN as u32 + 4)).unwrap();
        writer.finish().unwrap();
        let whole = std::fs::read(&path).unwrap();

        for cut in [
            whole.len() - 1,
            whole.len() - 6,
            whole.len() - (HEADER_LEN + 4 + 8) + 3,
        ] {
            std::fs::write(&path, &whole[..cut]).unwrap();
            let got = records(&path).unwrap();
            assert_eq!(got.len(), 1, "cut at {cut}");
            assert_eq!(got[0].0, 3);
        }
        std::fs::remove_file(&path).unwrap();
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

    /// Process 1 forks `child` at `step`.
    fn fork_of(step: u64, child: u32) -> Event {
        ev(
            step,
            1,
            1,
            EventKind::Fork {
                child,
                thread: false,
            },
        )
    }

    /// Process `pid` execs the program `name`, with it as its only argument.
    fn exec_of(step: u64, pid: u32, name: &str) -> Event {
        ev(
            step,
            pid,
            pid,
            EventKind::Exec {
                filename: name.into(),
                argv: vec![name.into()],
                old_pid: pid,
            },
        )
    }

    /// Process `pid` exits.
    fn exit_of(step: u64, pid: u32) -> Event {
        ev(
            step,
            pid,
            pid,
            EventKind::Exit {
                status: 0,
                comm: String::new(),
                thread: false,
            },
        )
    }

    #[test]
    fn a_reused_pid_names_the_process_alive_at_the_step() {
        // Process 5 runs "first" from step 1 to 3, then a second process 5
        // runs "second" from step 6: each step names the one alive then,
        // or the nearest before it, and a pid the run never had is none.
        let (fork, exec, exit) = (fork_of, exec_of, exit_of);
        let trace = Trace {
            events: vec![
                fork(1, 5),
                exec(2, 5, "first"),
                exit(3, 5),
                fork(6, 5),
                exec(7, 5, "second"),
            ],
            ..Default::default()
        };
        let name = |step| trace.process_at(5, step).map(|p| p.name());
        assert_eq!(name(2).as_deref(), Some("first"));
        assert_eq!(name(4).as_deref(), Some("first"));
        assert_eq!(name(8).as_deref(), Some("second"));
        assert!(trace.process_at(5, 2).unwrap().alive_at(2));
        assert!(!trace.process_at(5, 4).unwrap().alive_at(4));
        assert_eq!(trace.process_at(9, 2), None);
    }

    /// A process that execs more than once runs a different program at
    /// different steps, so asking about a step must give the command line
    /// in effect then, not the run's last one. Process 5 is forked from
    /// process 1 while it runs "shell", then execs "flox", "flox-activations"
    /// and "bash"; each step between the execs checks the name and whether
    /// the command line is its own or its parent's.
    #[test]
    fn a_process_that_execs_again_names_the_program_it_ran_then() {
        let trace = Trace {
            events: vec![
                exec_of(0, 1, "shell"),
                fork_of(1, 5),
                exec_of(2, 5, "flox"),
                exec_of(4, 5, "flox-activations"),
                exec_of(6, 5, "bash"),
            ],
            ..Default::default()
        };
        let at = |step| trace.process_at(5, step).unwrap();

        assert_eq!(at(1).argv, vec!["shell".to_string()]);
        assert!(
            !at(1).execd,
            "before its first exec it runs its parent's program"
        );
        assert_eq!(at(3).name(), "flox");
        assert!(at(3).execd);
        assert_eq!(at(5).name(), "flox-activations");
        assert_eq!(at(7).name(), "bash");
    }

    /// A process's memory begins at its last exec by a step, else at its
    /// fork: before then it was another program's. Process 5 is forked at
    /// step 1 and execs at steps 2 and 4, and a second process 5 is forked
    /// at step 8 and execs at step 9; each step checks where the memory of
    /// the process alive then began.
    #[test]
    fn a_process_memory_begins_at_its_last_exec_else_its_fork() {
        let trace = Trace {
            events: vec![
                fork_of(1, 5),
                exec_of(2, 5, "first"),
                exec_of(4, 5, "second"),
                exit_of(6, 5),
                fork_of(8, 5),
                exec_of(9, 5, "third"),
            ],
            ..Default::default()
        };
        let began = |step| trace.process_at(5, step).unwrap().memory_began(step);

        assert_eq!(began(1), 1, "forked, not exec'd yet");
        assert_eq!(began(2), 2);
        assert_eq!(began(3), 2, "the exec at step 4 is after the step");
        assert_eq!(began(5), 4);
        assert_eq!(began(8), 8, "the second process 5's fork");
        assert_eq!(began(10), 9);
    }

    #[test]
    fn only_inits_own_marks_frame_the_job() {
        // Init (pid 1) marks the start, an output and the exit; the job
        // (pid 7) writes the same words first. Only init's count, and the
        // job's are plain marks.
        let mark = |step, pid, text: &str| {
            ev(
                step,
                pid,
                pid,
                EventKind::Mark {
                    text: text.to_string(),
                },
            )
        };
        let trace = Trace {
            events: vec![
                mark(5, 1, "rewind-start"),
                mark(6, 7, "rewind-exit 0"),
                mark(7, 7, "rewind-output /nix/store/x-fake sha256:00"),
                mark(8, 7, "rewind-start"),
                mark(9, 1, "rewind-output /nix/store/x-out sha256:ff"),
                mark(10, 1, "rewind-exit 512"),
            ],
            ..Default::default()
        };
        assert_eq!(trace.job_start(), Some(5));
        assert_eq!(
            trace.job_exit(),
            Some(JobExit {
                step: 10,
                status: 512
            })
        );
        assert_eq!(
            trace.outputs(),
            vec![("/nix/store/x-out".to_string(), "sha256:ff".to_string())]
        );
        assert_eq!(trace.events[1].init_mark(), None);
        assert_eq!(trace.events[0].init_mark(), Some(Mark::Start));
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
            ..Default::default()
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
    fn a_reused_thread_id_ends_its_own_lifetime() {
        // make's thread 4 exits at 10, then make starts a new thread that
        // Linux gives the same id 4 at 11 and that exits at 12. Each exit
        // ends the lifetime that was live, so the first stays (9, 10).
        use EventKind::*;
        let mut t = sample();
        let after_first_exit = t.events.iter().position(|e| e.step == 11).unwrap();
        t.events.splice(
            after_first_exit..after_first_exit,
            [
                ev(
                    11,
                    2,
                    2,
                    Fork {
                        child: 4,
                        thread: true,
                    },
                ),
                ev(
                    12,
                    2,
                    4,
                    Exit {
                        status: 0,
                        comm: "make".into(),
                        thread: true,
                    },
                ),
            ],
        );
        let make = &t.processes()[1];
        assert_eq!(make.threads, vec![(4, 9, Some(10)), (4, 11, Some(12))]);
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

    /// With the events the same, the switches are compared: a switch to
    /// another task, or a run of switches that ends early, is a
    /// divergence in the switches, at the switch's step on each side.
    #[test]
    fn divergence_compares_the_switches_once_the_events_are_the_same() {
        let switch = |step, tid, name: &str| switches::Switch {
            step,
            pid: 4,
            tid,
            memory: true,
            name: name.into(),
        };
        let mut a = sample();
        a.switches = vec![switch(2, 4, "sh"), switch(5, 5, "sh"), switch(9, 4, "sh")];
        assert_eq!(a.divergence(&a.clone()), None);

        let mut b = a.clone();
        b.switches[1] = switch(5, 6, "sh");
        let d = a.divergence(&b).unwrap();
        assert_eq!(
            (d.index, d.left_step, d.right_step, d.part),
            (1, 5, 5, Part::Switches)
        );

        // A side whose switches ran out is at its last step.
        let mut c = a.clone();
        c.switches.truncate(2);
        let d = a.divergence(&c).unwrap();
        assert_eq!(
            (d.index, d.left_step, d.right_step, d.part),
            (2, 9, c.last_step(), Part::Switches)
        );

        // An event that differs comes first, whatever the switches do.
        let mut e = b.clone();
        e.events[5] = out(6, 3, "different");
        assert_eq!(a.divergence(&e).unwrap().part, Part::Events);
    }

    /// The trace after a step keeps the events and the switches past it.
    #[test]
    fn after_a_step_keeps_later_events_and_switches() {
        let mut a = sample();
        a.switches = vec![
            switches::Switch {
                step: 3,
                pid: 1,
                tid: 1,
                memory: true,
                name: "init".into(),
            },
            switches::Switch {
                step: 8,
                pid: 0,
                tid: 0,
                memory: false,
                name: "swapper/0".into(),
            },
        ];
        let after = a.after(5);
        assert!(after.events.iter().all(|e| e.step > 5));
        assert_eq!(after.events.len(), a.events.len() - a.index_after(5));
        assert_eq!(after.switches.len(), 1);
        assert_eq!(after.switches[0].step, 8);
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
        Trace {
            events,
            ..Default::default()
        }
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

    #[test]
    fn ids_the_kernel_hands_out_are_no_divergence() {
        // A rescheduled run can give the program's threads other ids, so
        // its forks name other children and its exec another old pid. The
        // same threads doing the same things in the same order do not
        // diverge; a fork of a process where the other run forks a thread
        // does.
        let argv = vec!["./pool".to_string()];
        let a = pool([1, 2, 3, 4, 5, 6], (8, "a\n"), (9, "b\n"), None);
        let mut renumbered = pool([1, 2, 3, 4, 5, 6], (18, "a\n"), (19, "b\n"), None);
        for e in &mut renumbered.events {
            match &mut e.kind {
                EventKind::Fork { child, .. } if *child != 7 => *child += 10,
                EventKind::Exec { old_pid, .. } => *old_pid = 70,
                _ => {}
            }
        }
        assert_eq!(a.divergence_in(&renumbered, &argv), None);

        let mut process = a.clone();
        for e in &mut process.events {
            if let EventKind::Fork { child: 9, thread } = &mut e.kind {
                *thread = false;
            }
        }
        assert!(a.divergence_in(&process, &argv).is_some());
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
        let (failing, passing) = (
            Trace {
                events: failing,
                ..Default::default()
            },
            Trace {
                events: passing,
                ..Default::default()
            },
        );

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
        let trace = Trace {
            events,
            ..Default::default()
        };
        let sub = trace.processes().into_iter().find(|p| p.pid == 3).unwrap();
        assert_eq!(sub.argv, vec!["bash".to_string(), "test.sh".to_string()]);
    }

    #[test]
    fn a_trace_reads_from_bytes_in_memory() {
        // A trace written into a Vec and read back from the same bytes,
        // with no file in between, as the browser viewer reads the trace
        // out of an export it holds in memory.
        let mut w = TraceWriter::new(Vec::new());
        let mut record = Vec::new();
        record.extend_from_slice(&((HEADER_LEN + 3) as u32).to_le_bytes());
        record.extend_from_slice(&10u16.to_le_bytes());
        record.extend_from_slice(&[0u8; 14]);
        record.extend_from_slice(b"hi\n");
        w.record(42, &record).unwrap();
        let bytes = w.finish().unwrap();

        let t = Trace::from_bytes(&bytes).unwrap();
        assert_eq!(t.events.len(), 1);
        assert_eq!(t.events[0].step, 42);
        assert_eq!(t.events[0].kind, EventKind::Mark { text: "hi".into() });
        assert!(Trace::from_bytes(b"not a trace!").is_err());
    }

    #[test]
    fn read_and_write_records_are_not_events() {
        // A trace file holding a mark and, at the same step, a write
        // record: the trace has the mark alone, and the log the write.
        let mut mark = Vec::new();
        mark.extend_from_slice(&((HEADER_LEN + 3) as u32).to_le_bytes());
        mark.extend_from_slice(&10u16.to_le_bytes());
        mark.extend_from_slice(&[0u8; 14]);
        mark.extend_from_slice(b"hi\n");
        let mut guest = Vec::new();
        guest.extend_from_slice(&((io_records::GUEST_HEADER_LEN + 12) as u16).to_le_bytes());
        guest.extend_from_slice(&[1, 0]);
        guest.extend_from_slice(&5u32.to_le_bytes());
        guest.extend_from_slice(&[0u8; 12]);
        guest.extend_from_slice(&0u64.to_le_bytes());
        guest.extend_from_slice(&9u32.to_le_bytes());
        let mut records = vec![(42, mark)];
        for r in io_records::Encoder::new().encode(&guest, 0) {
            records.push((42, r));
        }

        let t = Trace::decode(&records).unwrap();
        assert_eq!(t.events.len(), 1);
        assert_eq!(t.events[0].kind, EventKind::Mark { text: "hi".into() });
        let log = io_records::decode(&records).unwrap();
        assert_eq!(log.records.len(), 1);
        assert_eq!(log.records[0].step, 42);
    }

    #[test]
    fn records_come_back_exactly_as_written() {
        // Writes two raw records with a TraceWriter and reads them back
        // with records(), which a fork uses to copy its parent's trace up
        // to the step it starts from.
        let raw = |len: usize, fill: u8| {
            let mut r = vec![fill; len];
            r[..4].copy_from_slice(&(len as u32).to_le_bytes());
            r
        };
        let written = vec![(7, raw(HEADER_LEN, 1)), (9, raw(HEADER_LEN + 5, 2))];
        let path = std::env::temp_dir().join(format!("rewind-records-{}", std::process::id()));
        let mut w = TraceWriter::new(std::fs::File::create(&path).unwrap());
        for (step, record) in &written {
            w.record(*step, record).unwrap();
        }
        w.finish().unwrap();
        let read = records(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read, written);
    }
}
