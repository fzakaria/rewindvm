//! `rewind blame`: why a file came out different in two runs of the same
//! build. It works from the trace: the opens for writing, renames, execs
//! and forks among its events, and in runs that have them, the read and
//! write records (see rewind_trace::io_records). Files are read at steps
//! by forking the run, as `rewind cat` does.
//!
//! The file's bytes are compared at each run's end. The writer of the
//! file is the last process to open it for writing, following renames
//! back to the file renamed into place. A writer that copied another file
//! of the run, such as `install` copying a build product into an output,
//! is followed back to that file, and one that rewrote the file in place,
//! as strip does, to the file's earlier writer. The origin is the earliest
//! writer whose file already differed between the runs once it was done.
//! At the origin, the step a differing byte took its final value is found,
//! and the two writers' command lines are compared. Arguments the two runs
//! give in another order are traced to the step each run created them on,
//! since on the VM's tmpfs a directory lists its newest entries first.
//!
//! A run with read and write records names each of these directly. Its
//! write records say which write last covered the byte, its read-open
//! records say which file a writer copied and which files the build made
//! that the writer read, and a file nothing changed after its writer was
//! done is read at the run's end, where a keyframe is, rather than by
//! replaying to the middle of the run. From the origin, blame then
//! follows what the writers read back: the first file the build made that
//! differs between the runs, to its writers, and so on, as from a library
//! to the object it was linked from and the source that was compiled. A
//! file the two writers read after a different number of writes to it
//! ends the following there: one run read an older version of it, as the
//! second of two `git notes add` at once read the notes before the first
//! had updated them.
//!
//! A run recorded before runs had read and write records has none, and
//! blame finds its writers by forking it: copies by name and content, and
//! the step by bisection over the steps the writer had the file open. It
//! warns before it starts what that may cost.

use std::collections::HashMap;
use std::process::ExitCode;

use anyhow::{Result, bail};
use rewind_core::inspect::Inspection;
use rewind_core::{Home, Run};
use rewind_trace::io_records::{FileId, Io, IoLog, IoRecord};
use rewind_trace::writes::{
    ByteWrite, FinalWrite, O_TRUNC, Writer, base_name, file_of, final_write, label, last_writer,
    pid_of, thread_of,
};
use rewind_trace::{EventKind, Process, Trace};

use crate::color::{self, Same, Style};
use crate::exit;

/// How many bytes from the first differing offset are shown and bisected
/// on.
const WINDOW: usize = 16;

/// How many copies `blame` follows back before it gives up, so a cycle of
/// copies cannot loop forever.
const MAX_HOPS: usize = 8;

/// How many earlier files of the same name are compared with a file a
/// program made, looking for the one it copied.
const MAX_SAME_NAME: usize = 8;

/// Programs whose writes are copies of a file named in their arguments.
const COPIERS: &[&str] = &["install", "cp", "mv", "ln", "rsync", "cat"];

/// How many of the files the build made that a writer read are named
/// when none of them differs.
const INPUTS_LISTED: usize = 8;

/// Whether a run recorded its writes to files and its opens of them.
/// Runs with these records name each file's writers directly; runs
/// without them leave `blame` to find writers by forking and bisecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoRecords {
    Absent,
    Present,
}

/// What forking a run to find its writers costs: how far one probe may
/// replay, and how fast the run went when it was recorded.
#[derive(Clone, Debug)]
struct ProbeCost {
    id: String,
    steps: u64,
    /// The steps the run has keyframes at; a probe replays from the
    /// nearest one at or before its step, or from boot.
    keyframes: Vec<u64>,
    /// How long the run took to record, when it finished.
    wall_ms: Option<u64>,
    records: IoRecords,
}

impl ProbeCost {
    fn of(run: &Run, records: IoRecords) -> Result<ProbeCost> {
        Ok(ProbeCost {
            id: run.manifest.id.to_string(),
            steps: run.last_step()?,
            keyframes: run.keyframes().map(|k| k.steps()).unwrap_or_default(),
            wall_ms: run.manifest.outcome.as_ref().map(|o| o.wall_ms),
            records,
        })
    }

    /// The most steps one probe may replay: from boot or a keyframe to
    /// the next keyframe, or to the run's end.
    fn longest_replay(&self) -> u64 {
        let mut marks: Vec<u64> = self
            .keyframes
            .iter()
            .copied()
            .filter(|&k| k <= self.steps)
            .collect();
        marks.sort_unstable();
        let mut longest = 0;
        let mut from = 0;
        for k in marks {
            longest = longest.max(k - from);
            from = k;
        }
        longest.max(self.steps - from)
    }

    /// How long replaying `steps` of this run takes, at the speed it was
    /// recorded, when that is known.
    fn seconds(&self, steps: u64) -> Option<f64> {
        let ms = self.wall_ms?;
        (self.steps > 0).then(|| steps as f64 * ms as f64 / self.steps as f64 / 1000.0)
    }
}

/// Whether a run recorded its reads and writes, from its read and write
/// records: a run whose guest kernel buffers them has them from boot on,
/// and one recorded before they existed has none.
fn io_records(log: &IoLog) -> IoRecords {
    if log.records.is_empty() {
        return IoRecords::Absent;
    }
    IoRecords::Present
}

/// The fewest and the most probes, both runs together, that blame made on
/// the cases it was measured on: neatvnc, setuptools, sagittarius and a
/// six-object archive.
const PROBES_MEASURED: (u64, u64) = (20, 43);

/// What a probe costs beyond its replay: the fork, starting from a
/// keyframe, and reading the file. Blame took 23 to 37 s for 20 to 38
/// probes on neatvnc, setuptools and the archive, whose keyframes are
/// close together.
const PROBE_OVERHEAD_SECS: f64 = 1.0;

/// How many times blame replays a run's longest stretch without
/// keyframes before the keyframes it keeps where it looks make later
/// probes short. On sagittarius, where one run had no keyframes and a
/// replay from boot took about 4.5 min, blame took 12 to 22 min.
const LONG_REPLAYS: (f64, f64) = (3.0, 5.0);

/// How to avoid replaying, for the last line of the warning.
const RECORD_AGAIN: &str = "Runs recorded with this version of rewind have the records, but a \
     new recording may not show the same difference.";

/// The warning `blame` prints before it forks runs that have no read and
/// write records, or None when every run has them: only runs recorded
/// before records existed lack them.
fn bisection_warning(costs: &[ProbeCost]) -> Option<String> {
    let without: Vec<&ProbeCost> = costs
        .iter()
        .filter(|c| c.records == IoRecords::Absent)
        .collect();
    if without.is_empty() {
        return None;
    }

    // Which runs have no records.
    let names: Vec<&str> = without.iter().map(|c| short(&c.id)).collect();
    let what = match names.as_slice() {
        [one] => format!("rewind: run {one} has no read and write records."),
        _ => format!(
            "rewind: runs {} have no read and write records.",
            names.join(" and ")
        ),
    };

    // What replaying costs: each probe's overhead, and a few long replays
    // in each run.
    let replays: Option<Vec<f64>> = without
        .iter()
        .map(|c| c.seconds(c.longest_replay()))
        .collect();
    let (fewest, most) = PROBES_MEASURED;
    let (few, many) = LONG_REPLAYS;
    let cost = match replays {
        Some(replays) => {
            let long: f64 = replays.iter().sum();
            let time = span(
                fewest as f64 * PROBE_OVERHEAD_SECS + few * long,
                most as f64 * PROBE_OVERHEAD_SECS + many * long,
            );
            format!("Without them, blame replays the runs many times, which may take about {time}.")
        }
        None => "Without them, blame replays the runs many times, which may take minutes.".into(),
    };
    Some(format!("{what}\n{cost}\n{RECORD_AGAIN}"))
}

/// A range of durations, in the unit of its upper end: "2 to 3 min".
fn span(low: f64, high: f64) -> String {
    let (low, high) = if high < 90.0 {
        (low.round() as u64, format!("{} s", high.round() as u64))
    } else {
        (
            (low / 60.0).round() as u64,
            format!("{} min", (high / 60.0).round() as u64),
        )
    };
    format!("{low} to {high}")
}

/// `n` with its thousands grouped by commas.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The arguments of `rewind blame`, in two forms: a file and two runs,
/// `LEFT RIGHT PATH`; or values in memory and one run, `RUN --at STEP
/// EXPR...`.
#[derive(clap::Args)]
pub(crate) struct BlameArgs {
    /// LEFT RIGHT PATH: two runs, by id, the start of one, name or @, and
    /// a file by its absolute path in the VM. With --at: RUN, then the
    /// values in memory to blame, as expressions of the program's
    /// language, such as 'p->queue'.
    #[arg(required = true, value_name = "RUN")]
    pub(crate) targets: Vec<String>,
    /// The byte of the file to explain; by default the first that differs.
    #[arg(long, conflicts_with = "at")]
    pub(crate) offset: Option<usize>,
    /// Blame values in the memory of a process at this step: who last
    /// wrote each, from which line and thread, and where the process's
    /// other threads were then.
    #[arg(long, value_name = "STEP")]
    pub(crate) at: Option<u64>,
    /// The process the values are in; by default the step's event's.
    #[arg(long, requires = "at")]
    pub(crate) pid: Option<u32>,
    /// The thread whose frame names the values; by default the step's
    /// event's, or the main thread of --pid.
    #[arg(long, requires = "at")]
    pub(crate) tid: Option<u32>,
    /// The frame, by its number in `rewind where`, that names the values;
    /// by default the one in the program's own code.
    #[arg(long, requires = "at")]
    pub(crate) frame: Option<u32>,
    /// List each value's last N writes, newest first.
    #[arg(long, value_name = "N", default_value_t = 1, requires = "at",
          conflicts_with = "reads", value_parser = at_least_one)]
    pub(crate) writes: usize,
    /// List the reads as well as the writes, of every access since
    /// --since, oldest first. Telling reads from writes takes two of the
    /// CPU's debug registers for each 8 bytes, so the values can take 16
    /// bytes at most, where writes alone can take 32.
    #[arg(long, requires = "at")]
    pub(crate) reads: bool,
    /// Search no further back than this step; by default the process's
    /// start, or its last exec before --at.
    #[arg(long, value_name = "STEP", requires = "at")]
    pub(crate) since: Option<u64>,
    /// The most accesses --reads lists, the latest ones; with more, the
    /// search goes no further back and says how many it left out.
    #[arg(long, value_name = "N", default_value_t = super::blame_memory::DEFAULT_MAX,
          requires = "reads", value_parser = at_least_one)]
    pub(crate) max: usize,
    /// Blame the bytes at this address instead of an expression, for
    /// programs such as the desktop app that have the address from
    /// `rewind where --json`.
    #[arg(long, value_name = "ADDRESS", requires_all = ["at", "size"])]
    pub(crate) address: Option<String>,
    /// How many bytes --address names.
    #[arg(long, requires = "address")]
    pub(crate) size: Option<u64>,
    /// How --address's bytes print: pointer, signed, unsigned, bool,
    /// float or bytes.
    #[arg(long, value_name = "CLASS", requires = "address", value_parser = value_class)]
    pub(crate) class: Option<rewind_trace::located::ValueClass>,
    /// Print the answer as one JSON object on standard output, for
    /// programs such as the desktop app.
    #[arg(long, requires = "at")]
    pub(crate) json: bool,
}

/// A count of 1 or more.
fn at_least_one(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("give 1 or more".into()),
        Ok(n) => Ok(n),
        Err(error) => Err(error.to_string()),
    }
}

/// A value class by its name, as `rewind where --json` prints it.
fn value_class(text: &str) -> Result<rewind_trace::located::ValueClass, String> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|_| format!("{text} is not one of pointer, signed, unsigned, bool, float, bytes"))
}

/// `rewind blame RUN --at STEP EXPR...`.
fn blame_memory(
    home: &Home,
    args: &BlameArgs,
    at: u64,
    progress: crate::progress::Format,
) -> Result<ExitCode> {
    let (run, expressions) = args.targets.split_first().expect("clap requires a run");
    let address = match (&args.address, args.size) {
        (Some(address), Some(size)) => {
            let Some(parsed) = super::blame_memory::parse_hex(address) else {
                bail!("--address {address} is not an address in hex");
            };
            let class = args
                .class
                .unwrap_or(rewind_trace::located::ValueClass::Bytes);
            Some((parsed, size, class))
        }
        _ => None,
    };
    if address.is_some() && !expressions.is_empty() {
        bail!("give --address or expressions to blame, not both");
    }
    super::blame_memory::blame(
        home,
        super::blame_memory::Asked {
            run,
            step: at,
            expressions,
            thread: (args.pid, args.tid),
            frame: args.frame,
            address,
            wanted: if args.reads {
                rewind_core::accesses::Wanted::Accesses(args.max)
            } else {
                rewind_core::accesses::Wanted::LastWrites(args.writes)
            },
            since: args.since,
            json: args.json,
            progress,
        },
    )
}

/// What made a file's bytes in one run: the copies back to the process
/// that wrote them.
struct Chain {
    hops: Vec<Link>,
}

impl Chain {
    /// The `k`th writer, counting from the first.
    fn at(&self, k: usize) -> &Writer {
        &self.hops[self.hops.len() - 1 - k].writer
    }
}

/// One writer of a chain, and what it did with the file before it. The
/// listing words each writer from `source`: a writer that renamed its
/// file into place, as Cabal's copy does, opened another path than the
/// writer after it, and comparing paths would call that writer a copier.
struct Link {
    writer: Writer,
    source: Source,
}

/// Whether a writer's file was the same in both runs once it was done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Same,
    Differs,
}

/// One run, as blame reads it: its trace, its read and write records when
/// it has them, and the step its job exited at, where files are read
/// cheaply.
struct Side<'a> {
    home: &'a Home,
    run: &'a Run,
    trace: &'a Trace,
    log: Option<&'a IoLog>,
    end: u64,
    made: Made<'a>,
}

impl<'a> Side<'a> {
    fn new(home: &'a Home, run: &'a Run, log: Option<&'a IoLog>) -> Result<Side<'a>> {
        let trace = run.trace()?;
        Ok(Side {
            home,
            run,
            trace,
            log,
            end: end_step(run)?,
            made: Made::of(trace),
        })
    }

    fn id(&self) -> &str {
        short(&self.run.manifest.id)
    }

    /// The step to read a writer's file at to see it as the writer left
    /// it: the run's end when the run has records and nothing changed the
    /// file since, else the writer's close.
    fn done_step(&self, writer: &Writer) -> u64 {
        let Some(log) = self.log else {
            return writer.close;
        };
        let file = file_of(log, writer);
        if changed_after(self.trace, self.log, &writer.path, file, writer.close) {
            return writer.close;
        }
        self.end
    }
}

/// For each path, the steps a process of the run opened it for writing or
/// renamed a file to it at, in order: the files the build made.
struct Made<'a>(HashMap<&'a str, Vec<u64>>);

impl<'a> Made<'a> {
    fn of(trace: &'a Trace) -> Made<'a> {
        let mut made: HashMap<&str, Vec<u64>> = HashMap::new();
        for e in &trace.events {
            let path = match &e.kind {
                EventKind::Open { path, .. } => path,
                EventKind::Rename { to, .. } => to,
                _ => continue,
            };
            made.entry(path.as_str()).or_default().push(e.step);
        }
        Made(made)
    }

    /// Whether the build had made `path` by `step`.
    fn by(&self, path: &str, step: u64) -> bool {
        self.0
            .get(path)
            .is_some_and(|steps| steps.first().is_some_and(|first| *first <= step))
    }
}

/// Whether anything changed `path` after `step`: an open for writing, a
/// rename from or to it, an unlink, or a write to `file` when the file and
/// the records are known. Such a path is read as it was at the step; any
/// other can be read at the run's end, which costs no replay to the middle
/// of the run.
fn changed_after(
    trace: &Trace,
    log: Option<&IoLog>,
    path: &str,
    file: Option<FileId>,
    step: u64,
) -> bool {
    let events = &trace.events[trace.index_after(step)..];
    let touched = events.iter().any(|e| match &e.kind {
        EventKind::Open { path: p, .. } | EventKind::Unlink { path: p } => p == path,
        EventKind::Rename { from, to } => from == path || to == path,
        _ => false,
    });
    let written = match (log, file) {
        (Some(log), Some(file)) => log
            .records
            .iter()
            .any(|r| r.step > step && matches!(r.io, Io::Write { file: f, .. } if f == file)),
        _ => false,
    };
    touched || written
}

/// What a writer did with the file before it: copied another file of the
/// run into it, rewrote it in place, or made its bytes from other inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Copied(String),
    /// It read the file, or opened it without emptying it, so the earlier
    /// writer's bytes may be in it.
    Rewritten,
    /// None of the above: a first writer.
    Made,
}

pub(crate) fn blame(
    home: &Home,
    args: BlameArgs,
    progress: crate::progress::Format,
) -> Result<ExitCode> {
    if let Some(at) = args.at {
        return blame_memory(home, &args, at, progress);
    }
    let [left, right, path] = <[String; 3]>::try_from(args.targets).map_err(|targets| {
        anyhow::anyhow!(
            "blaming a file takes two runs and a path, not {} arguments; give --at STEP \
             to blame values in memory",
            targets.len()
        )
    })?;
    let offset = args.offset;
    let runs = [Run::find(home, &left)?, Run::find(home, &right)?];
    let logs = [runs[0].io()?, runs[1].io()?];

    // Without read and write records, even reading the file is done by
    // forking the runs, and finding each writer can take minutes; say so
    // first.
    let records = [io_records(&logs[0]), io_records(&logs[1])];
    let costs = [
        ProbeCost::of(&runs[0], records[0])?,
        ProbeCost::of(&runs[1], records[1])?,
    ];
    if let Some(warning) = bisection_warning(&costs) {
        eprintln!("{warning}");
    }
    let present = |i: usize| (records[i] == IoRecords::Present).then_some(&logs[i]);
    let sides = [
        Side::new(home, &runs[0], present(0))?,
        Side::new(home, &runs[1], present(1))?,
    ];
    let ids = [sides[0].id(), sides[1].id()];

    // Records the guest had no room for leave gaps, which blame cannot
    // see into.
    for side in &sides {
        let Some(log) = side.log else {
            continue;
        };
        let lost = log.lost();
        if lost > 0 {
            eprintln!(
                "{} run {} is missing {lost} read and write records, because its buffer was \
                 full. Blame may miss some reads or writes.",
                color::stderr().prefix(),
                side.id()
            );
        }
    }

    // The file as each run left it at the job's exit.
    let finals = [
        read(home, &runs[0], sides[0].end, &path)?,
        read(home, &runs[1], sides[1].end, &path)?,
    ];
    let (Some(a), Some(b)) = (&finals[0], &finals[1]) else {
        bail!("{path} is missing at the end of one of the runs");
    };
    let Some(first) = offset.or_else(|| first_difference(a, b)) else {
        println!(
            "{path} {} ({} bytes).",
            color::stdout().verdict(Same::Yes, "is the same in both runs"),
            grouped(a.len() as u64)
        );
        return Ok(ExitCode::SUCCESS);
    };
    let differing =
        a.iter().zip(b.iter()).filter(|(x, y)| x != y).count() + a.len().abs_diff(b.len());

    // Each run's chain of writers, back to the first.
    let chains = [chain(&sides[0], &path)?, chain(&sides[1], &path)?];

    // The origin is the earliest writer whose file already differed
    // between the runs once it was done; the chains are lined up from
    // their first writers. With records on both sides, writers after the
    // origin are compared only where that is cheap.
    let both = sides.iter().all(|s| s.log.is_some());
    let depth = chains[0].hops.len().min(chains[1].hops.len());
    let mut verdicts = vec![None; depth];
    let mut origin = None;
    for (k, verdict) in verdicts.iter_mut().enumerate() {
        let [x, y] = [chains[0].at(k), chains[1].at(k)];
        let steps = [sides[0].done_step(x), sides[1].done_step(y)];
        let cheap = steps[0] == sides[0].end && steps[1] == sides[1].end;
        if both && origin.is_some() && !cheap {
            continue;
        }
        let left = read(home, &runs[0], steps[0], &x.path)?;
        let right = read(home, &runs[1], steps[1], &y.path)?;
        let v = if left == right {
            Verdict::Same
        } else {
            Verdict::Differs
        };
        if v == Verdict::Differs && origin.is_none() {
            origin = Some(k);
        }
        *verdict = Some(v);
    }
    let origin_k = origin.unwrap_or(0);
    let origins = [
        chains[0].at(origin_k).clone(),
        chains[1].at(origin_k).clone(),
    ];

    // When the byte got the value it ends with: from the write records,
    // or by bisection without them.
    let settled = [
        settled(&sides[0], &origins[0], first)?,
        settled(&sides[1], &origins[1], first)?,
    ];

    // Who wrote the byte, named by the program that wrote it, as the
    // app's two-run file view names it. The origin's process may have
    // exec'd another program between opening the file and writing it, so
    // what made the byte is described as of the write: its command line,
    // what it read and the likely cause.
    let wrote = [
        byte_writer(sides[0].trace, &origins[0], &settled[0]),
        byte_writer(sides[1].trace, &origins[1], &settled[1]),
    ];
    let origins = [
        at_write(&origins[0], &settled[0]),
        at_write(&origins[1], &settled[1]),
    ];

    // What the writers read that the build made, followed back to where
    // the difference starts.
    let followed = if both {
        Some(follow_inputs(&sides, origins.clone())?)
    } else {
        None
    };

    // The answer first. The left run's id is amber and the right run's
    // blue wherever the answer and the bytes name them.
    let paint = color::stdout();
    let painted = [
        paint.paint(Style::Left, ids[0]),
        paint.paint(Style::Right, ids[1]),
    ];
    let [x, y] = &origins;
    let at = |s: &Settled| {
        s.step
            .map_or("an unknown step".into(), |s| format!("step {}", grouped(s)))
    };
    println!(
        "{} {}. {} wrote that byte at {} in run {} and at {} in run {}.",
        base_name(&path),
        paint.verdict(
            Same::No,
            format!("differs at byte {}", grouped(first as u64))
        ),
        who(&wrote[0], &wrote[1], &ids),
        at(&settled[0]),
        painted[0],
        at(&settled[1]),
        painted[1]
    );
    println!();

    // The bytes themselves.
    println!(
        "{} of {} bytes differ. The bytes from byte {}:",
        grouped(differing as u64),
        grouped(a.len().max(b.len()) as u64),
        grouped(first as u64)
    );
    for (id, bytes) in painted.iter().zip([a, b]) {
        println!("  {id}  {}", window(bytes, first));
    }
    println!();

    // Who wrote the file, from the last writer back.
    print_chain(&chains[0], &verdicts, origin_k, &path, ids[0]);
    let programs = |c: &Chain| {
        c.hops
            .iter()
            .map(|h| label(&h.writer.process))
            .collect::<Vec<_>>()
    };
    if programs(&chains[0]) == programs(&chains[1]) {
        println!(
            "Run {} made the file the same way, with the same programs.",
            ids[1]
        );
    } else {
        print_chain(&chains[1], &verdicts, origin_k, &path, ids[1]);
    }
    println!();

    // How the byte was written, and by another process than the writer
    // when one shared its file; once when both runs wrote it alike.
    let lines: Vec<String> = sides
        .iter()
        .zip(&settled)
        .zip(&origins)
        .map(|((side, s), writer)| match &s.write {
            Some(w) => {
                let pid = pid_of(side.trace, w.tid, w.step);
                let by = if pid == writer.process.pid {
                    String::new()
                } else {
                    format!(" by pid {pid}")
                };
                format!(
                    "one write of {} bytes{by}, from byte {}, set byte {}.",
                    grouped(u64::from(w.count)),
                    grouped(w.pos),
                    grouped(first as u64)
                )
            }
            None => "blame found the step by replaying the run, since no write record covers \
                     the byte."
                .to_string(),
        })
        .collect();
    if lines[0] == lines[1] {
        println!("In both runs, {}", lines[0]);
    } else {
        for (id, line) in ids.iter().zip(&lines) {
            println!("In run {id}, {line}");
        }
    }
    println!();

    // Whether the writers were asked for different things.
    explain_argv(&origins, &[sides[0].trace, sides[1].trace], &ids);

    // What they read, and the conclusion.
    match followed {
        Some(followed) => print_followed(&followed, &ids),
        None => {
            let name = label(&x.process);
            if x.process.argv == y.process.argv {
                println!(
                    "Likely cause: something {name} read, or randomness inside {name}. Without \
                     read records, blame cannot tell which."
                );
            } else {
                println!("Likely cause: {name} ran with different command lines in the two runs.");
            }
        }
    }
    Ok(ExitCode::from(exit::DIFFERS))
}

/// When a writer gave the byte the value it ends with: the step, and the
/// write that did it when the records say.
struct Settled {
    step: Option<u64>,
    write: Option<FinalWrite>,
}

fn settled(side: &Side, writer: &Writer, offset: usize) -> Result<Settled> {
    let write = side
        .log
        .and_then(|log| final_write(log, writer, offset as u64));
    if let Some(w) = write {
        return Ok(Settled {
            step: Some(w.step),
            write: Some(w),
        });
    }
    let Some(bytes) = read(side.home, side.run, writer.close, &writer.path)? else {
        return Ok(Settled {
            step: None,
            write: None,
        });
    };
    Ok(Settled {
        step: Some(settle(side.home, side.run, writer, &bytes, offset)?),
        write: None,
    })
}

/// A process as blame's sentences name it: its program and its pid.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Named {
    program: String,
    pid: u32,
}

impl Named {
    /// A writer, by the program its process ran at the step the writer
    /// was taken at: the open, or the write for one `at_write` made.
    fn of(writer: &Writer) -> Named {
        Named {
            program: label(&writer.process),
            pid: writer.process.pid,
        }
    }
}

/// Who gave the byte its value in one run, by the program at the step of
/// the write: the process the write record names, looked up as the app's
/// two-run file view looks it up, or without a record the writer, by the
/// program it ran at the step bisection found.
fn byte_writer(trace: &Trace, writer: &Writer, settled: &Settled) -> Named {
    if let Some(write) = &settled.write {
        let by = ByteWrite::of(&trace.processes(), write);
        return Named {
            program: by.program,
            pid: by.pid,
        };
    }
    let step = settled.step.unwrap_or(writer.open);
    Named {
        program: label(&writer.process.as_of(step)),
        pid: writer.process.pid,
    }
}

/// The writer with its process as it was when it gave the byte its value,
/// for the lines about what made the byte; as it was at the open when
/// that step is not known. A write the records name was made the step
/// before its record's.
fn at_write(writer: &Writer, settled: &Settled) -> Writer {
    let made = settled.write.map(|write| write.made());
    let Some(step) = made.or(settled.step) else {
        return writer.clone();
    };
    Writer {
        process: writer.process.as_of(step),
        ..writer.clone()
    }
}

/// A process in both runs, by its program, with its pid when that is the
/// same in both.
fn who(x: &Named, y: &Named, ids: &[&str; 2]) -> String {
    let name = &x.program;
    if x.pid == y.pid {
        return format!("{name} (pid {})", x.pid);
    }
    format!(
        "{name} (pid {} in run {}, {} in run {})",
        x.pid, ids[0], y.pid, ids[1]
    )
}

/// One run's writers of the file, from the last back, a sentence each
/// for what the writer did and one for what comparing the runs showed.
fn print_chain(chain: &Chain, verdicts: &[Option<Verdict>], origin_k: usize, path: &str, id: &str) {
    println!("Who wrote the file in run {id}, from the last writer back:");
    let n = chain.hops.len();
    for (i, hop) in chain.hops.iter().enumerate() {
        let k = n - 1 - i;
        let earlier = chain.hops.get(i + 1);
        let what = what_it_did(hop, earlier, path);
        let copy = matches!(hop.source, Source::Copied(_));
        let then = match (verdicts.get(k).copied().flatten(), k == origin_k) {
            (Some(Verdict::Differs), true) if k == 0 => {
                "It is the first writer, and the file already differs when it is done."
            }
            (Some(Verdict::Differs), true) => {
                "The file was the same in both runs before this, so the difference starts here."
            }
            (Some(Verdict::Differs), false) if copy => {
                "Its source already differs, so the copy did not cause the difference."
            }
            (Some(Verdict::Differs), false) => {
                "The file differs after it, but the difference started earlier."
            }
            (Some(Verdict::Same), _) => "The file is the same in both runs after it.",
            (None, _) if k < verdicts.len() => {
                "Not checked: an earlier writer already made the bytes differ."
            }
            (None, _) => "",
        };
        println!("  {what} {then}");
    }
}

/// What one writer of a chain did, as a sentence: copied the file, changed
/// it in place, or, as the first writer, wrote it.
fn what_it_did(hop: &Link, earlier: Option<&Link>, path: &str) -> String {
    let writer = &hop.writer;
    let name = format!("{} (pid {})", label(&writer.process), writer.process.pid);
    let step = grouped(writer.open);
    match (&hop.source, earlier) {
        (Source::Copied(from), _) => {
            format!("{name} copied it from {from}, starting at step {step}.")
        }
        (_, Some(_)) => format!("{name} changed it in place, starting at step {step}."),
        (_, None) if writer.path != path => {
            format!("{name} wrote {}, starting at step {step}.", writer.path)
        }
        (_, None) => format!("{name} wrote it, starting at step {step}."),
    }
}

/// One writer blame went back to through what it read, in both runs, and
/// what it read that the build made.
struct Hop {
    /// The file that led here, as each run named it; None at the origin.
    via: Option<(Input, Input)>,
    writers: [Writer; 2],
    /// How many files the build made each writer read, and how many
    /// others.
    made: [usize; 2],
    others: [usize; 2],
    /// The files the build made both writers read, lined up, and whether
    /// each is the same in both runs; None where it could not be read.
    pairs: Vec<(Input, Input)>,
    compared: Vec<Option<Verdict>>,
}

/// Why following the inputs back stopped.
enum Stop {
    /// Every file the build made that the last writers read is the same.
    SameInputs,
    /// More than one of them could not be compared, and none differs.
    Uncompared(usize),
    /// No process of the run wrote the file that led on.
    NoWriter(String),
    /// The writers read the file that led on at different points of its
    /// history, so its writers in the two runs are not one program.
    OlderVersion(OlderVersion),
    /// [`MAX_HOPS`] writers back, and still going.
    TooFar,
}

/// From the origin's writers back through what they read: at each, the
/// files the build made that both writers read, compared where the runs'
/// ends show them, and the first that differs, or else the one that could
/// not be compared, followed to its writers. Stops at writers whose inputs
/// from the build were the same.
fn follow_inputs(sides: &[Side; 2], origins: [Writer; 2]) -> Result<(Vec<Hop>, Stop)> {
    let logs = [
        sides[0].log.expect("records on both sides"),
        sides[1].log.expect("records on both sides"),
    ];
    let mut hops = Vec::new();
    let mut writers = origins;
    let mut via = None;
    for _ in 0..MAX_HOPS {
        let (left, left_others) = inputs_of(&sides[0].made, logs[0], &writers[0]);
        let (right, right_others) = inputs_of(&sides[1].made, logs[1], &writers[1]);

        // Both writers' inputs, compared where the ends show them.
        let pairs = pair_inputs(&left, &right);
        let lefts: Vec<&Input> = pairs.iter().map(|(l, _)| l).collect();
        let rights: Vec<&Input> = pairs.iter().map(|(_, r)| r).collect();
        let bytes = [
            read_inputs(&sides[0], &lefts)?,
            read_inputs(&sides[1], &rights)?,
        ];
        let compared: Vec<Option<Verdict>> = (0..pairs.len())
            .map(|i| match (&bytes[0][i], &bytes[1][i]) {
                (Some(a), Some(b)) if a == b => Some(Verdict::Same),
                (Some(_), Some(_)) => Some(Verdict::Differs),
                _ => None,
            })
            .collect();

        // The next hop: the first input that differs, or else the only one
        // that could not be compared.
        let differs = compared.iter().position(|v| *v == Some(Verdict::Differs));
        let unknown: Vec<usize> = (0..pairs.len())
            .filter(|&i| compared[i].is_none())
            .collect();
        let next = match (differs, unknown.as_slice()) {
            (Some(i), _) => Some(i),
            (None, [only]) => Some(*only),
            (None, _) => None,
        };
        let hop = Hop {
            via: via.take(),
            writers: writers.clone(),
            made: [left.len(), right.len()],
            others: [left_others, right_others],
            pairs,
            compared,
        };
        let Some(next) = next else {
            let stop = match unknown.len() {
                0 => Stop::SameInputs,
                n => Stop::Uncompared(n),
            };
            hops.push(hop);
            return Ok((hops, stop));
        };

        // That input's writers, in each run. Writers that read it after a
        // different number of writes read it from different processes,
        // and comparing those as one program would blame their command
        // lines, so the older version is the answer.
        let (l, r) = hop.pairs[next].clone();
        hops.push(hop);
        if let Some(older) = older_version([sides[0].trace, sides[1].trace], (&l, &r)) {
            return Ok((hops, Stop::OlderVersion(older)));
        }
        let (Some(wl), Some(wr)) = (
            last_writer(sides[0].trace, &l.path, l.step),
            last_writer(sides[1].trace, &r.path, r.step),
        ) else {
            return Ok((hops, Stop::NoWriter(l.path)));
        };
        writers = [wl, wr];
        via = Some((l, r));
    }
    Ok((hops, Stop::TooFar))
}

/// What following the inputs found, a few lines a writer, then the
/// conclusion.
fn print_followed((hops, stop): &(Vec<Hop>, Stop), ids: &[&str; 2]) {
    for hop in hops {
        let [x, y] = &hop.writers;

        // Who this writer is, and how blame got to it.
        if let Some((l, r)) = &hop.via {
            println!(
                "{} wrote {} at step {}.",
                who(&Named::of(x), &Named::of(y), ids),
                pair_paths(&(l.clone(), r.clone()), ids),
                grouped(x.open)
            );
            if x.process.argv != y.process.argv {
                print_argv_difference(x, y, ids);
            }
        }
        println!("What {} read:", label(&x.process));

        // What it read that the build made, and how that compared.
        let made = format!("{} {}", counted(hop.made, ids), files(hop.made[0]));
        let differ: Vec<usize> = (0..hop.pairs.len())
            .filter(|&i| hop.compared[i] == Some(Verdict::Differs))
            .collect();
        let unknown: Vec<usize> = (0..hop.pairs.len())
            .filter(|&i| hop.compared[i].is_none())
            .collect();
        let inputs: Vec<&Input> = hop.pairs.iter().map(|(l, _)| l).collect();
        match (differ.first(), unknown.as_slice()) {
            (Some(&i), _) => {
                println!(
                    "  {made} that the build made. {} between the runs. The first is {}.",
                    match differ.len() {
                        1 => "1 of them differs".to_string(),
                        n => format!("{n} of them differ"),
                    },
                    pair_paths(&hop.pairs[i], ids)
                );
            }
            (None, [only]) => {
                println!(
                    "  {made} that the build made. One, {}, was deleted or changed before the \
                     run ended, so blame cannot compare it.",
                    pair_paths(&hop.pairs[*only], ids)
                );
            }
            (None, []) if hop.pairs.is_empty() => {
                println!("  No files that the build made.");
            }
            (None, []) => {
                println!(
                    "  {made} that the build made, all the same in both runs{}.",
                    listed(&inputs)
                );
            }
            (None, many) => {
                println!(
                    "  {made} that the build made. None of them differs, but blame could not \
                     compare {}.",
                    many.len()
                );
            }
        }
        println!(
            "  {} other {} that the build did not make, such as programs and libraries.",
            counted(hop.others, ids),
            files(hop.others[0])
        );
    }
    println!();

    // The conclusion, from where the following stopped.
    let Some(last) = hops.last() else {
        return;
    };
    let [x, y] = &last.writers;
    let name = who(&Named::of(x), &Named::of(y), ids);
    match stop {
        Stop::SameInputs if x.process.argv == y.process.argv => println!(
            "Likely cause: something inside {name}, such as randomness or the order its threads \
             ran in. Its command line and the files the build made that it read are the same in \
             both runs."
        ),
        Stop::SameInputs => println!(
            "Likely cause: {name} ran with different command lines in the two runs. The files \
             the build made that it read are the same in both runs."
        ),
        Stop::Uncompared(n) => println!(
            "Likely cause: inside {name}, or in one of the {n} files it read that blame could \
             not compare."
        ),
        Stop::NoWriter(path) => println!(
            "Likely cause: not found. No process of the runs wrote {path}, which {name} read, so \
             blame stops there."
        ),
        Stop::OlderVersion(older) => println!("{}", older_version_cause(&name, older, ids)),
        Stop::TooFar => println!(
            "Likely cause: not found within {MAX_HOPS} writers back. The last one blame reached \
             is {name}."
        ),
    }
}

/// A file the two runs' writers read at different points of its history:
/// which run read the older version, and the change that run's reader
/// read before, which the other run's reader read after.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OlderVersion {
    path: String,
    /// The run that read the older version: 0 for the left, 1 for the
    /// right.
    older: usize,
    /// Who made the change the older run's reader read before, and when.
    changed: Named,
    step: u64,
}

/// Whether the two runs read an input at different points of its
/// history: after a different number of writes to it, each a rename into
/// its place or an open of it for writing. None when the counts match,
/// and the input's writers in the two runs are then one program.
fn older_version(traces: [&Trace; 2], (left, right): (&Input, &Input)) -> Option<OlderVersion> {
    let writes = [
        writes_of(traces[0], &left.path),
        writes_of(traces[1], &right.path),
    ];
    let seen = [
        writes[0]
            .iter()
            .filter(|(_, step)| *step <= left.step)
            .count(),
        writes[1]
            .iter()
            .filter(|(_, step)| *step <= right.step)
            .count(),
    ];
    if seen[0] == seen[1] {
        return None;
    }

    // The run that had seen fewer writes read the older version; the next
    // write after its read is the change the other run had seen.
    let older = usize::from(seen[1] < seen[0]);
    let read = [left.step, right.step][older];
    let &(pid, step) = writes[older].iter().find(|(_, step)| *step > read)?;
    let program = traces[older]
        .process_at(pid, step)
        .map_or_else(|| format!("pid {pid}"), |p| label(&p.as_of(step)));
    Some(OlderVersion {
        path: left.path.clone(),
        older,
        changed: Named { program, pid },
        step,
    })
}

/// The writes to `path` in `trace`, in order: who and at which step, for
/// each rename into its place and each open of it for writing.
fn writes_of(trace: &Trace, path: &str) -> Vec<(u32, u64)> {
    trace
        .events
        .iter()
        .filter(|e| match &e.kind {
            EventKind::Rename { to, .. } => to == path,
            EventKind::Open { path: p, .. } => p == path,
            _ => false,
        })
        .map(|e| (e.pid, e.step))
        .collect()
}

/// The likely cause when `reader`, the writer blame followed back to,
/// read an input at different points of its history in the two runs.
fn older_version_cause(reader: &str, older: &OlderVersion, ids: &[&str; 2]) -> String {
    let (old, new) = (ids[older.older], ids[1 - older.older]);
    format!(
        "Likely cause: {reader} read {} before {} (pid {}) changed it at step {} in run {old}, \
         and after that change in run {new}. It read an older version of the file in run {old}.",
        older.path,
        older.changed.program,
        older.changed.pid,
        grouped(older.step)
    )
}

/// A count in both runs: once when the same, else both.
fn counted([a, b]: [usize; 2], ids: &[&str; 2]) -> String {
    if a == b {
        return grouped(a as u64);
    }
    format!(
        "{} ({} in run {})",
        grouped(a as u64),
        grouped(b as u64),
        ids[1]
    )
}

/// "file" or "files", for a count.
fn files(n: usize) -> &'static str {
    if n == 1 {
        return "file";
    }
    "files"
}

/// Where two writers' command lines first differ.
fn print_argv_difference(x: &Writer, y: &Writer, ids: &[&str; 2]) {
    let (a, b) = (&x.process.argv, &y.process.argv);
    let i = (0..a.len().max(b.len()))
        .find(|&i| a.get(i) != b.get(i))
        .unwrap_or(0);
    let arg = |v: &[String]| v.get(i).cloned().unwrap_or_else(|| "nothing".into());
    println!(
        "  Its command lines differ. Argument {i} is {} in run {} and {} in run {}.",
        arg(a),
        ids[0],
        arg(b),
        ids[1]
    );
}

/// The read-opens of `process`'s threads from its last exec at or before
/// `from` to `until`, each path once at its first read: what the program
/// it ran then read.
fn reads_of(log: &IoLog, process: &Process, from: u64, until: u64) -> Vec<(u64, String)> {
    let start = process
        .execs
        .iter()
        .rev()
        .find(|(at, _)| *at <= from)
        .map_or(process.start, |(at, _)| *at);
    let mut seen = std::collections::HashSet::new();
    log.records
        .iter()
        .filter(|r| r.step >= start && r.step <= until)
        .filter_map(|r: &IoRecord| match r.io {
            Io::ReadOpen { tid, path } if thread_of(process, tid) && seen.insert(path) => {
                Some((r.step, log.path(path).to_string()))
            }
            _ => None,
        })
        .collect()
}

/// What a writer did with the file before it, from its read-open
/// records: copied a file of the run it read that it names in its
/// arguments, as a copier does, or that has the same name, as an install
/// step's copy has; rewrote it, having read it first or opened it without
/// emptying it; or made it. `known_as` is the path blame asked about,
/// which differs from `writer.path` when the writer wrote a temporary
/// file and renamed it into place, as Cabal's copy does; the same name is
/// the name it was renamed to.
fn source_by_records(made: &Made, log: &IoLog, writer: &Writer, known_as: &str) -> Source {
    let reads = reads_of(log, &writer.process, writer.open, writer.close);
    let name = known_as.rsplit('/').next().unwrap_or(known_as);
    let program = writer.process.name();
    let copier = COPIERS.contains(&program.rsplit('/').next().unwrap_or(&program));

    // A file of the run it read and copied, the latest such first.
    let named = |path: &str| {
        writer.process.argv.iter().skip(1).any(|arg| {
            let arg = arg.trim_start_matches("./");
            !arg.starts_with('-') && (path == arg || path.ends_with(&format!("/{arg}")))
        })
    };
    let copied = reads.iter().rev().find(|(step, path)| {
        *path != writer.path
            && made.by(path, *step)
            && ((copier && named(path)) || path.rsplit('/').next() == Some(name))
    });
    if let Some((_, path)) = copied {
        return Source::Copied(path.clone());
    }

    // The file itself, read before the writer opened it for writing, or
    // opened without being emptied.
    let read_first = reads
        .iter()
        .any(|(step, path)| *path == writer.path && *step <= writer.open);
    let kept = writer.flags & O_TRUNC == 0 && made.by(&writer.path, writer.open.saturating_sub(1));
    if read_first || kept {
        return Source::Rewritten;
    }
    Source::Made
}

/// One file the build made that a writer read: its path and the step the
/// writer first read it at.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Input {
    path: String,
    step: u64,
}

/// The files the build made that a writer read while it ran its program,
/// other than the one it wrote, and how many other files it read.
fn inputs_of(made: &Made, log: &IoLog, writer: &Writer) -> (Vec<Input>, usize) {
    let reads = reads_of(log, &writer.process, writer.open, writer.close);
    let total = reads.len();
    let made: Vec<Input> = reads
        .into_iter()
        .filter(|(step, path)| *path != writer.path && made.by(path, *step))
        .map(|(step, path)| Input { path, step })
        .collect();
    let others = total - made.len();
    (made, others)
}

/// The inputs of two writers lined up: the same path in both, then, of
/// the rest, those with the same extension in the order they were read,
/// as a compiler's temporary files with random names line up.
fn pair_inputs(left: &[Input], right: &[Input]) -> Vec<(Input, Input)> {
    let mut pairs = Vec::new();
    let mut rest_left = Vec::new();
    let mut rest_right: Vec<&Input> = right
        .iter()
        .filter(|r| !left.iter().any(|l| l.path == r.path))
        .collect();
    for l in left {
        match right.iter().find(|r| r.path == l.path) {
            Some(r) => pairs.push((l.clone(), r.clone())),
            None => rest_left.push(l),
        }
    }
    let extension = |p: &str| p.rsplit_once('.').map(|(_, e)| e.to_string());
    for l in rest_left {
        let Some(at) = rest_right
            .iter()
            .position(|r| extension(&r.path) == extension(&l.path))
        else {
            continue;
        };
        pairs.push((l.clone(), rest_right.remove(at).clone()));
    }
    pairs.sort_by_key(|(l, _)| l.step);
    pairs
}

/// Each input's bytes as the writer read them, where the run's end shows
/// them: inputs nothing changed after the writer read them, read in one
/// fork at the end. None for the rest, and for inputs gone by then.
fn read_inputs(side: &Side, inputs: &[&Input]) -> Result<Vec<Option<Vec<u8>>>> {
    let readable: Vec<String> = inputs
        .iter()
        .filter(|i| !changed_after(side.trace, side.log, &i.path, None, i.step))
        .map(|i| i.path.clone())
        .collect();
    let mut found: HashMap<String, Vec<u8>> = HashMap::new();
    if !readable.is_empty() {
        let answer = rewind_core::inspect::files(side.home, side.run, side.end, None, &readable)?;
        if let Inspection::Contents(bytes) = answer {
            for (name, body) in rewind_init::sections(&bytes).unwrap_or_default() {
                found.insert(name, body.to_vec());
            }
        }
    }
    Ok(inputs.iter().map(|i| found.remove(&i.path)).collect())
}

/// A command line cut short for a line of its own: its first few
/// arguments, store paths by their last part.
fn brief(argv: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut words: Vec<&str> = argv
        .iter()
        .take(SHOWN)
        .map(|a| {
            if a.starts_with("/nix/store/") {
                return base_name(a);
            }
            a.as_str()
        })
        .collect();
    if argv.len() > SHOWN {
        words.push("…");
    }
    words.join(" ")
}

/// A pair of inputs' paths: once when they are the same, else the first
/// run's with the second's after it.
fn pair_paths((l, r): &(Input, Input), ids: &[&str; 2]) -> String {
    if l.path == r.path {
        return l.path.clone();
    }
    format!("{} ({} in run {})", l.path, r.path, ids[1])
}

/// The first few of `inputs`, after a colon, for a line that counts them.
fn listed(inputs: &[&Input]) -> String {
    if inputs.is_empty() {
        return String::new();
    }
    let mut names: Vec<&str> = inputs
        .iter()
        .take(INPUTS_LISTED)
        .map(|i| i.path.as_str())
        .collect();
    if inputs.len() > INPUTS_LISTED {
        names.push("…");
    }
    format!(": {}", names.join(", "))
}

/// The step a run's job exited at, else its last step.
fn end_step(run: &Run) -> Result<u64> {
    match run.trace()?.job_exit() {
        Some(exit) => Ok(exit.step),
        None => run.last_step(),
    }
}

/// A file at a step, or None when it did not exist then.
fn read(home: &Home, run: &Run, step: u64, path: &str) -> Result<Option<Vec<u8>>> {
    match rewind_core::inspect::cat(home, run, step, None, path)? {
        Inspection::Contents(bytes) => Ok(Some(bytes)),
        Inspection::NotFound(_) => Ok(None),
        Inspection::Failed(message) => bail!("reading {path} at step {step}: {message}"),
    }
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let shared = a.len().min(b.len());
    (0..shared)
        .find(|&i| a[i] != b[i])
        .or((a.len() != b.len()).then_some(shared))
}

/// The bytes from `offset`, as hex and as text.
fn window(bytes: &[u8], offset: usize) -> String {
    let end = (offset + WINDOW).min(bytes.len());
    let slice = &bytes[offset.min(end)..end];
    let hex: Vec<String> = slice.iter().map(|b| format!("{b:02x}")).collect();
    let text: String = slice
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{}  |{text}|", hex.join(" "))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// The writers of `path` in a run, from the last one back: through
/// copies to the file copied, and through rewrites in place, as ranlib
/// and strip make, to the file's earlier writers.
fn chain(side: &Side, path: &str) -> Result<Chain> {
    let trace = side.trace;
    let mut hops = Vec::new();
    let mut path = path.to_string();
    let mut before = side.end;
    for _ in 0..MAX_HOPS {
        let Some(writer) = last_writer(trace, &path, before) else {
            break;
        };

        // What the writer did with the file before it, from its read-open
        // records. Without them, a copier's source is the argument naming
        // a file the run wrote earlier with the same bytes, and any other
        // writer may have read the file it rewrites, so its earlier writer
        // comes next.
        let source = match side.log {
            Some(log) => source_by_records(&side.made, log, &writer, &path),
            None => match copied_from(side.home, side.run, trace, &writer, &path)? {
                Some(copied) => Source::Copied(copied),
                None => Source::Rewritten,
            },
        };
        before = writer.open.saturating_sub(1);
        let next = match &source {
            Source::Copied(copied) => Some(copied.clone()),
            Source::Rewritten => Some(writer.path.clone()),
            Source::Made => None,
        };
        hops.push(Link { writer, source });
        let Some(next) = next else {
            break;
        };
        path = next;
    }
    if hops.is_empty() {
        bail!(
            "no process of run {} opened {path} for writing",
            side.run.manifest.id
        );
    }
    Ok(Chain { hops })
}

/// The file a writer copied into `writer.path`. For a copier, such as
/// install or cp, an argument naming a file the run wrote before with the
/// copy's bytes. For any other program, an earlier file of the same name
/// with the same bytes, as meson install copies what meson setup
/// generated. The same name is that of `known_as`, the path blame asked
/// about, which a writer that renamed its temporary file into place wrote
/// under another name.
fn copied_from(
    home: &Home,
    run: &Run,
    trace: &Trace,
    writer: &Writer,
    known_as: &str,
) -> Result<Option<String>> {
    let program = writer.process.name();
    let base = program.rsplit('/').next().unwrap_or(&program);
    let copy = read(home, run, writer.close, &writer.path)?;

    // A copier names its source.
    if COPIERS.contains(&base) {
        for arg in writer.process.argv.iter().skip(1) {
            if arg.starts_with('-') {
                continue;
            }
            let Some(candidate) = written_path(trace, arg, writer.open) else {
                continue;
            };
            if candidate == writer.path {
                continue;
            }
            if read(home, run, writer.open, &candidate)? == copy {
                return Ok(Some(candidate));
            }
        }
    }

    // Any other program: the newest earlier files of the same name with
    // the same bytes, as cmake's install step copies a library it built.
    // A program that rewrote the file in place, as ranlib and strip do,
    // matches none of them, and its earlier writer comes next instead.
    let name = known_as.rsplit('/').next().unwrap_or(known_as);
    let mut tried = Vec::new();
    for e in trace.events.iter().rev().filter(|e| e.step < writer.open) {
        let EventKind::Open { path, .. } = &e.kind else {
            continue;
        };
        if *path == writer.path || tried.contains(path) {
            continue;
        }
        if path.rsplit('/').next() != Some(name) {
            continue;
        }
        if tried.len() == MAX_SAME_NAME {
            break;
        }
        tried.push(path.clone());

        // Read it as its own writer left it: a build often removes a
        // file once it is packed, as bdist_wheel does.
        let Some(earlier) = last_writer(trace, path, writer.open) else {
            continue;
        };
        if read(home, run, earlier.close, path)? == copy {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}

/// The absolute path of a file the run wrote before `before` that `arg`
/// names, relative or not.
fn written_path(trace: &Trace, arg: &str, before: u64) -> Option<String> {
    let arg = arg.trim_start_matches("./");
    trace
        .events
        .iter()
        .rev()
        .filter(|e| e.step < before)
        .find_map(|e| match &e.kind {
            EventKind::Open { path, .. } if path == arg || path.ends_with(&format!("/{arg}")) => {
                Some(path.clone())
            }
            _ => None,
        })
}

/// The first step at which the bytes at `offset` of the writer's file
/// equal their final value, by bisection over the steps the file was
/// open.
fn settle(home: &Home, run: &Run, writer: &Writer, last: &[u8], offset: usize) -> Result<u64> {
    let end = (offset + WINDOW).min(last.len());
    let want = &last[offset.min(end)..end];
    let settled = |step: u64| -> Result<bool> {
        let Some(bytes) = read(home, run, step, &writer.path)? else {
            return Ok(false);
        };
        Ok(bytes.get(offset..end) == Some(want))
    };

    first_settled(writer.open, writer.close, settled)
}

/// The first step after `open`, up to `close`, at which `settled` holds,
/// by bisection: `settled` does not hold at `open`, holds at `close`, and
/// once it holds it keeps holding.
fn first_settled(
    open: u64,
    close: u64,
    mut settled: impl FnMut(u64) -> Result<bool>,
) -> Result<u64> {
    // Invariant: unsettled at lo, settled at hi. A close that is not
    // after the open leaves no step between them to try.
    let (mut lo, mut hi) = (open, close);
    while hi.saturating_sub(lo) > 1 {
        let mid = lo + (hi - lo) / 2;
        if settled(mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(hi)
}

/// Compares the two writers' command lines, and traces arguments given in
/// another order to the steps the runs created them on.
fn explain_argv(origins: &[Writer; 2], traces: &[&Trace; 2], ids: &[&str; 2]) {
    let [x, y] = origins;
    let name = label(&x.process);
    let (a, b) = (&x.process.argv, &y.process.argv);
    if a == b {
        println!(
            "{name} ran with the same command line in both runs, so the difference comes from \
             something it read or from randomness."
        );
        println!();
        return;
    }

    // Other arguments, beyond another order.
    let mut sorted_a = a.clone();
    let mut sorted_b = b.clone();
    sorted_a.sort();
    sorted_b.sort();
    if sorted_a != sorted_b {
        println!("{name} ran with different command lines in the two runs.");
        print_argv_difference(x, y, ids);
        println!();
        return;
    }

    // The same arguments in another order: when each run created each.
    let Some(i) = (0..a.len()).find(|&i| a[i] != b[i]) else {
        return;
    };
    println!(
        "{name} got the same {} arguments in both runs, but in a different order from \
         argument {i} on.",
        a.len()
    );
    for arg in [&a[i], &b[i]] {
        let steps: Vec<String> = traces
            .iter()
            .zip(ids)
            .map(|(t, id)| {
                let created = t.events.iter().find_map(|e| match &e.kind {
                    EventKind::Open { path, .. }
                        if path.ends_with(&format!("/{}", arg.trim_start_matches("./"))) =>
                    {
                        Some(e.step)
                    }
                    _ => None,
                });
                match created {
                    Some(step) => format!("at step {} in run {id}", grouped(step)),
                    None => format!("not at all in run {id}"),
                }
            })
            .collect();
        println!("  {arg} was created {}.", steps.join(" and "));
    }

    // Who built the command line: what the writer's process ran before,
    // and the programs it started meanwhile.
    for (trace, writer) in traces.iter().zip(origins) {
        let p = &writer.process;
        let earlier: Vec<&(u64, Vec<String>)> =
            p.execs.iter().filter(|(at, _)| *at < writer.open).collect();
        if earlier.len() < 2 {
            continue;
        }
        let (from, shell) = &earlier[earlier.len() - 2];
        let (to, _) = earlier[earlier.len() - 1];
        let children: Vec<String> = trace
            .processes()
            .into_iter()
            .filter(|c| c.parent_life == Some(p.life()) && c.start > *from && c.start < *to)
            .map(|c| brief(&c.argv))
            .collect();
        println!(
            "  Before that, pid {} ran {} and started: {}.",
            p.pid,
            brief(shell),
            children.join("; ")
        );
        break;
    }
    println!();
}

#[cfg(test)]
mod tests {
    // The warning blame prints before it forks runs to find their writers,
    // built from run costs made by hand, with and without read and write
    // records; and what blame reads from the records, from traces and
    // logs made by hand.
    use super::*;
    use rewind_trace::Event;
    use rewind_trace::writes::FileWrites;

    /// A neatvnc-sized run: 30,056 steps recorded in 15.1 s, with one
    /// keyframe where its job exited, as `rewind check` leaves one.
    fn neatvnc(id: &str, records: IoRecords) -> ProbeCost {
        ProbeCost {
            id: id.into(),
            steps: 30_056,
            keyframes: vec![30_052],
            wall_ms: Some(15_127),
            records,
        }
    }

    #[test]
    fn runs_without_records_warn_before_bisecting() {
        // Neither run has records: the warning names both runs, says
        // what replaying them costs, and how new runs avoid it, in at
        // most three lines.
        let runs = [
            neatvnc("a84b0112f11563fc", IoRecords::Absent),
            neatvnc("863054ef27a743e7", IoRecords::Absent),
        ];
        let warning = bisection_warning(&runs).expect("a warning");
        assert!(
            warning.contains("a84b0112 and 863054ef have no read and write records."),
            "{warning}"
        );
        // 20 to 43 probes of a second each, plus 3 to 5 replays of 15 s
        // from boot in each of the two runs.
        assert!(warning.contains("about 2 to 3 min"), "{warning}");
        // Records are not a run input: new runs have them, with no flag.
        assert!(
            warning.contains("recorded with this version of rewind"),
            "{warning}"
        );
        assert!(!warning.contains("--record-io"), "{warning}");
        assert!(warning.lines().count() <= 3, "{warning}");
    }

    #[test]
    fn runs_with_records_do_not_warn() {
        // Both runs have records, so blame does not bisect, and says
        // nothing.
        let runs = [
            neatvnc("a84b0112f11563fc", IoRecords::Present),
            neatvnc("863054ef27a743e7", IoRecords::Present),
        ];
        assert_eq!(bisection_warning(&runs), None);
    }

    #[test]
    fn only_runs_without_records_are_named() {
        // One run has records: the warning is about the other alone.
        let runs = [
            neatvnc("a84b0112f11563fc", IoRecords::Present),
            neatvnc("863054ef27a743e7", IoRecords::Absent),
        ];
        let warning = bisection_warning(&runs).expect("a warning");
        assert!(
            warning.contains("run 863054ef has no read and write records"),
            "{warning}"
        );
        assert!(!warning.contains("a84b0112"), "{warning}");
    }

    /// The path ids of the logs below.
    const LIB_BUILT: u32 = 0;
    const LIB_OUT: u32 = 1;
    const OBJECT: u32 = 2;
    const LIBC: u32 = 3;

    fn log(records: Vec<(u64, Io)>) -> IoLog {
        IoLog {
            records: records
                .into_iter()
                .map(|(step, io)| IoRecord { step, io })
                .collect(),
            paths: vec![
                "/build/lib.so".into(),
                "/out/lib.so".into(),
                "/build/a.o".into(),
                "/nix/store/glibc/lib/libc.so.6".into(),
            ],
        }
    }

    fn process(pid: u32, argv: &[&str], start: u64) -> Process {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        Process {
            pid,
            parent: 1,
            parent_life: Some(rewind_trace::Life { pid: 1, start: 0 }),
            argv: argv.clone(),
            execd: true,
            forked_argv: Vec::new(),
            execs: vec![(start, argv)],
            start,
            end: None,
            status: None,
            comm: None,
            threads: Vec::new(),
            main_end: None,
        }
    }

    fn writer(path: &str, process: Process, open: u64, close: u64, flags: u32) -> Writer {
        Writer {
            path: path.into(),
            process,
            open,
            close,
            flags,
        }
    }

    /// A trace in which the linker opens /build/a.o for writing at step 2
    /// (the compiler's output) and /build/lib.so at 10, and the install
    /// step opens /out/lib.so at 20.
    fn built() -> Trace {
        let open = |step, pid, path: &str| Event {
            step,
            pid,
            tid: pid,
            kind: EventKind::Open {
                path: path.into(),
                flags: O_TRUNC,
            },
        };
        Trace {
            events: vec![
                open(2, 9, "/build/a.o"),
                open(10, 10, "/build/lib.so"),
                open(20, 20, "/out/lib.so"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_copy_is_found_from_what_the_copier_read() {
        // install read the library the build made, named in its
        // arguments, and then wrote the output: the output is a copy of
        // it.
        let trace = built();
        let install = process(20, &["install", "/build/lib.so", "/out/lib.so"], 18);
        let log = log(vec![
            (
                19,
                Io::ReadOpen {
                    tid: 20,
                    path: LIB_BUILT,
                },
            ),
            (
                21,
                Io::WriteOpen {
                    tid: 20,
                    file: 1,
                    path: LIB_OUT,
                },
            ),
        ]);
        let w = writer("/out/lib.so", install, 20, 22, O_TRUNC);
        assert_eq!(
            source_by_records(&Made::of(&trace), &log, &w, "/out/lib.so"),
            Source::Copied("/build/lib.so".into())
        );
    }

    #[test]
    fn a_copy_renamed_into_place_is_found_by_its_final_name() {
        // Cabal's Setup copy, which is no copier blame knows by name, read
        // the library the build made, wrote a temporary file beside the
        // output and renamed it into place. The writer of /out/lib.so is
        // the writer of the temporary file, whose name matches nothing;
        // the name it was renamed to matches the library it read.
        const TEMPORARY: u32 = 4;
        let open = |step, pid, path: &str| Event {
            step,
            pid,
            tid: pid,
            kind: EventKind::Open {
                path: path.into(),
                flags: O_TRUNC,
            },
        };
        let trace = Trace {
            events: vec![
                open(10, 10, "/build/lib.so"),
                open(20, 20, "/out/.copyFile20-1.tmp"),
                Event {
                    step: 23,
                    pid: 20,
                    tid: 20,
                    kind: EventKind::Rename {
                        from: "/out/.copyFile20-1.tmp".into(),
                        to: "/out/lib.so".into(),
                    },
                },
            ],
            ..Default::default()
        };
        let setup = process(20, &["Setup", "copy"], 18);
        let mut log = log(vec![
            (
                19,
                Io::ReadOpen {
                    tid: 20,
                    path: LIB_BUILT,
                },
            ),
            (
                21,
                Io::WriteOpen {
                    tid: 20,
                    file: 1,
                    path: TEMPORARY,
                },
            ),
        ]);
        log.paths.push("/out/.copyFile20-1.tmp".into());
        let w = writer("/out/.copyFile20-1.tmp", setup, 20, 22, O_TRUNC);
        assert_eq!(
            source_by_records(&Made::of(&trace), &log, &w, "/out/lib.so"),
            Source::Copied("/build/lib.so".into())
        );
    }

    #[test]
    fn a_writer_after_a_rename_changed_the_file_in_place() {
        // Setup copied the library into a temporary file and renamed it to
        // /out/lib.so, which strip then rewrote. The two writers' paths
        // differ, but strip changed the file in place and Setup is the one
        // that copied: the sentences follow what chain found each writer
        // did, not the paths.
        let setup = Link {
            writer: writer(
                "/out/.copyFile20-1.tmp",
                process(20, &["Setup", "copy"], 18),
                20,
                22,
                O_TRUNC,
            ),
            source: Source::Copied("/build/lib.so".into()),
        };
        let strip = Link {
            writer: writer(
                "/out/lib.so",
                process(30, &["strip", "/out/lib.so"], 28),
                31,
                33,
                O_TRUNC,
            ),
            source: Source::Rewritten,
        };
        assert_eq!(
            what_it_did(&strip, Some(&setup), "/out/lib.so"),
            "strip (pid 30) changed it in place, starting at step 31."
        );
        assert_eq!(
            what_it_did(&setup, None, "/out/lib.so"),
            "Setup copy (pid 20) copied it from /build/lib.so, starting at step 20."
        );
    }

    #[test]
    fn a_file_read_before_it_is_written_is_rewritten() {
        // strip read the output, then opened it for writing: the bytes it
        // wrote came from the output's earlier writer, which comes next.
        let trace = built();
        let strip = process(30, &["strip", "/out/lib.so"], 28);
        let log = log(vec![
            (
                29,
                Io::ReadOpen {
                    tid: 30,
                    path: LIB_OUT,
                },
            ),
            (
                32,
                Io::WriteOpen {
                    tid: 30,
                    file: 1,
                    path: LIB_OUT,
                },
            ),
        ]);
        let w = writer("/out/lib.so", strip, 31, 33, O_TRUNC);
        assert_eq!(
            source_by_records(&Made::of(&trace), &log, &w, "/out/lib.so"),
            Source::Rewritten
        );
    }

    #[test]
    fn a_linker_makes_its_output() {
        // The linker read an object the build made and the C library,
        // neither of the output's name: its output's bytes are its own.
        let trace = built();
        let ld = process(10, &["ld", "-o", "/build/lib.so", "/build/a.o"], 8);
        let log = log(vec![
            (
                9,
                Io::ReadOpen {
                    tid: 10,
                    path: OBJECT,
                },
            ),
            (
                9,
                Io::ReadOpen {
                    tid: 10,
                    path: LIBC,
                },
            ),
            (
                11,
                Io::WriteOpen {
                    tid: 10,
                    file: 0,
                    path: LIB_BUILT,
                },
            ),
        ]);
        let w = writer("/build/lib.so", ld, 10, 12, O_TRUNC);
        assert_eq!(
            source_by_records(&Made::of(&trace), &log, &w, "/build/lib.so"),
            Source::Made
        );
    }

    #[test]
    fn the_final_write_is_the_last_to_cover_the_byte() {
        // The linker wrote its output front to back, then went back to
        // patch bytes 50 to 59: byte 55 took its final value in the
        // patch, and byte 120 in the second write. A write by the same
        // thread to another file does not count.
        let ld = process(10, &["ld"], 8);
        let write = |pos, count| Io::Write {
            tid: 10,
            file: 0,
            pos,
            count,
        };
        let log = log(vec![
            (
                11,
                Io::WriteOpen {
                    tid: 10,
                    file: 0,
                    path: LIB_BUILT,
                },
            ),
            (12, write(0, 100)),
            (13, write(100, 50)),
            (14, write(50, 10)),
            (
                15,
                Io::Write {
                    tid: 10,
                    file: 3,
                    pos: 0,
                    count: 200,
                },
            ),
        ]);
        let w = writer("/build/lib.so", ld, 10, 16, O_TRUNC);
        let patch = final_write(&log, &w, 55).unwrap();
        assert_eq!((patch.step, patch.pos, patch.count), (14, 50, 10));
        assert_eq!(final_write(&log, &w, 120).unwrap().step, 13);
        assert_eq!(final_write(&log, &w, 500), None);
    }

    #[test]
    fn inputs_are_the_files_the_build_made() {
        // The linker read an object the build made and the C library from
        // the store: one input, and one other file.
        let trace = built();
        let ld = process(10, &["ld"], 8);
        let log = log(vec![
            (
                9,
                Io::ReadOpen {
                    tid: 10,
                    path: OBJECT,
                },
            ),
            (
                9,
                Io::ReadOpen {
                    tid: 10,
                    path: LIBC,
                },
            ),
        ]);
        let w = writer("/build/lib.so", ld, 10, 12, O_TRUNC);
        let (inputs, others) = inputs_of(&Made::of(&trace), &log, &w);
        assert_eq!(
            inputs,
            vec![Input {
                path: "/build/a.o".into(),
                step: 9
            }]
        );
        assert_eq!(others, 1);
    }

    /// A builder that bash starts as pid 34, opens /out/lib.so for its
    /// redirection at step 10, then exec's python3 at step 12, which
    /// writes the file at step 14: the trace, its records, and the
    /// writer `last_writer` finds, as it was at the open.
    fn redirected_then_execd() -> (Trace, IoLog, Writer) {
        let event = |step, kind| Event {
            step,
            pid: 34,
            tid: 34,
            kind,
        };
        let exec = |step, argv: &[&str]| {
            event(
                step,
                EventKind::Exec {
                    filename: argv[0].into(),
                    argv: argv.iter().map(|a| a.to_string()).collect(),
                    old_pid: 34,
                },
            )
        };
        let trace = Trace {
            events: vec![
                exec(
                    5,
                    &["/nix/store/x-bash/bin/bash", "-c", "exec python3 gen.py"],
                ),
                event(
                    10,
                    EventKind::Open {
                        path: "/out/lib.so".into(),
                        flags: O_TRUNC,
                    },
                ),
                exec(12, &["/nix/store/y-python3/bin/python3", "-c", "gen()"]),
                event(
                    20,
                    EventKind::Close {
                        path: "/out/lib.so".into(),
                    },
                ),
            ],
            ..Default::default()
        };
        let log = log(vec![
            (
                11,
                Io::WriteOpen {
                    tid: 34,
                    file: 0,
                    path: LIB_OUT,
                },
            ),
            (
                14,
                Io::Write {
                    tid: 34,
                    file: 0,
                    pos: 0,
                    count: 23,
                },
            ),
        ]);
        let writer = last_writer(&trace, "/out/lib.so", 30).expect("pid 34 opened it");
        (trace, log, writer)
    }

    #[test]
    fn the_byte_s_writer_is_named_by_the_program_at_the_write() {
        // bash opened the file and python3 wrote it in the same process:
        // the opening line names bash, while the answer names python3,
        // as the app's two-run file view does for the same write, with
        // the write record and without it.
        let (trace, log, writer) = redirected_then_execd();
        assert_eq!(label(&writer.process), "bash");
        let with_record = Settled {
            step: Some(14),
            write: final_write(&log, &writer, 6),
        };
        let bisected = Settled {
            step: Some(14),
            write: None,
        };
        let python3 = Named {
            program: "python3".into(),
            pid: 34,
        };
        let named = [
            byte_writer(&trace, &writer, &with_record),
            byte_writer(&trace, &writer, &bisected),
        ];
        assert_eq!(named, [python3.clone(), python3]);
        assert_eq!(
            who(&named[0], &named[1], &["afcd2ec2", "611f40ef"]),
            "python3 (pid 34)"
        );
        let app = FileWrites::of(&trace, &log, "/out/lib.so", 30);
        assert_eq!(app.byte(6).map(|b| b.program.as_str()), Some("python3"));
    }

    #[test]
    fn the_origin_is_described_as_it_was_at_the_write() {
        // The lines about what made the byte, the writer's command line,
        // what it read and the likely cause, take the writer as it was
        // at the write: python3's command line, not bash's. A writer
        // whose step is not known stays as it was at the open.
        let (_, _, writer) = redirected_then_execd();
        let settled = Settled {
            step: Some(14),
            write: None,
        };
        let origin = at_write(&writer, &settled);
        assert_eq!(label(&origin.process), "python3");
        assert_eq!(origin.process.argv[1..], ["-c", "gen()"]);
        assert_eq!((origin.open, origin.close), (writer.open, writer.close));
        let unknown = Settled {
            step: None,
            write: None,
        };
        assert_eq!(label(&at_write(&writer, &unknown).process), "bash");
    }

    /// A write whose record is at the step of the writer's exec was made
    /// by the program before the exec: the record is taken at the step
    /// after the write. Here the record is at step 12, where bash execs
    /// python3, so the byte's writer and the origin's command line are
    /// bash's.
    #[test]
    fn a_write_taken_at_an_exec_is_the_program_before_s() {
        const EXEC_STEP: u64 = 12;
        let (trace, _, writer) = redirected_then_execd();
        let settled = Settled {
            step: Some(EXEC_STEP),
            write: Some(FinalWrite {
                step: EXEC_STEP,
                tid: 34,
                pos: 0,
                count: 23,
            }),
        };
        assert_eq!(byte_writer(&trace, &writer, &settled).program, "bash");
        assert_eq!(label(&at_write(&writer, &settled).process), "bash");
    }

    /// The bisection finds the first step a byte had its final value,
    /// here step 14 of a file open from step 10 to step 20. A close
    /// before the open, which a writer should never have, ends the
    /// bisection at once and does not underflow.
    #[test]
    fn the_bisection_takes_a_close_before_the_open() {
        const SETTLED_FROM: u64 = 14;
        let settled = |step: u64| Ok(step >= SETTLED_FROM);
        assert_eq!(first_settled(10, 20, settled).unwrap(), SETTLED_FROM);
        assert_eq!(first_settled(25, 4, settled).unwrap(), 4);
    }

    /// The git notes runs' history of the notes ref, by hand: pid 53
    /// writes it, pid 55 renames its lock over it at `second_write`, and
    /// pid 56 reads it at step 2,700.
    fn notes_ref(second_write: u64) -> Trace {
        let event = |step, pid, kind| Event {
            step,
            pid,
            tid: pid,
            kind,
        };
        let exec = |step, pid, note: &str| {
            let argv = ["git", "notes", "add", "-m", note];
            event(
                step,
                pid,
                EventKind::Exec {
                    filename: "git".into(),
                    argv: argv.iter().map(|a| a.to_string()).collect(),
                    old_pid: pid,
                },
            )
        };
        let replace = |step, pid| {
            event(
                step,
                pid,
                EventKind::Rename {
                    from: format!("{NOTES_REF}.lock"),
                    to: NOTES_REF.into(),
                },
            )
        };
        let mut events = vec![
            exec(2_500, 53, "base"),
            replace(2_568, 53),
            exec(2_655, 55, "note 1"),
            exec(2_688, 56, "note 2"),
            replace(second_write, 55),
        ];
        events.sort_by_key(|e| e.step);
        Trace {
            events,
            ..Default::default()
        }
    }

    const NOTES_REF: &str = "/build/r/.git/refs/notes/commits";

    #[test]
    fn a_reader_of_an_older_version_is_not_followed_to_another_writer() {
        // pid 56 read the notes ref at step 2,700 in both runs: in the
        // first run before pid 55 changed it at 2,721, in the second after
        // it changed it at 2,683. The readers saw different versions, so
        // the first run read the older one, and pid 55's change is what
        // it missed. Read after the same writes in both runs, the
        // versions match and blame follows the writers as before.
        let failing = notes_ref(2_721);
        let passing = notes_ref(2_683);
        let read = Input {
            path: NOTES_REF.into(),
            step: 2_700,
        };
        let older = older_version([&failing, &passing], (&read, &read));
        assert_eq!(
            older,
            Some(OlderVersion {
                path: NOTES_REF.into(),
                older: 0,
                changed: Named {
                    program: "git notes".into(),
                    pid: 55,
                },
                step: 2_721,
            })
        );
        let flipped = older_version([&passing, &failing], (&read, &read));
        assert_eq!(flipped.map(|o| o.older), Some(1));
        assert_eq!(older_version([&failing, &failing], (&read, &read)), None);
    }

    #[test]
    fn an_older_version_is_the_likely_cause_in_words() {
        // The conclusion names the reader, the change it read before in
        // one run and after in the other, and which run read the older
        // version.
        let older = OlderVersion {
            path: NOTES_REF.into(),
            older: 0,
            changed: Named {
                program: "git notes".into(),
                pid: 55,
            },
            step: 2_721,
        };
        assert_eq!(
            older_version_cause("git notes (pid 56)", &older, &["94c59fdf", "ad0de5ae"]),
            "Likely cause: git notes (pid 56) read /build/r/.git/refs/notes/commits before \
             git notes (pid 55) changed it at step 2,721 in run 94c59fdf, and after that \
             change in run ad0de5ae. It read an older version of the file in run 94c59fdf."
        );
    }

    #[test]
    fn inputs_pair_by_path_then_by_extension() {
        // Both assemblers read the same header and a temporary file of
        // another random name in each run: the header pairs with itself,
        // and the two temporary files with each other.
        let input = |path: &str, step| Input {
            path: path.into(),
            step,
        };
        let left = [input("/tmp/ccAbc.s", 5), input("/build/x.h", 6)];
        let right = [input("/build/x.h", 7), input("/tmp/ccXyz.s", 8)];
        let pairs = pair_inputs(&left, &right);
        let paths: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(l, r)| (l.path.as_str(), r.path.as_str()))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("/tmp/ccAbc.s", "/tmp/ccXyz.s"),
                ("/build/x.h", "/build/x.h")
            ]
        );
    }
}
