//! The Time tab's model: a run's critical path and what would make it
//! finish sooner, as `rewind time` works them out
//! (rewind_trace::time_report), laid out for the tab.
//!
//! The report is the one the command prints, worded the same way. This
//! module turns it into what the tab draws: the path in order as segments
//! of a bar a given number of pixels wide, the Nix phases as a strip on
//! the same scale, the path as a flame graph of processes and what each
//! did, the rows that would make the run sooner, the source lines a
//! part's time was spent at, and the trail of processes zoomed into, which
//! the breadcrumb shows and Escape walks back out.

use std::collections::HashSet;

use crate::source::Located;
use rewind_trace::Life;
use rewind_trace::Trace;
use rewind_trace::critical::{Run, TimeError};
use rewind_trace::io_records::IoLog;
use rewind_trace::machine::ClockSource;
use rewind_trace::time_report::{
    Budget, Inside, OwnPart, PathNode, PathTree, Phase, Report, Row, RowKind, State, Stretch,
    Timing,
};

/// Rows that would make the job less than this share of its chain sooner
/// are left out of the job's table, as `rewind time` leaves them out.
const MIN_GAIN_SHARE: f64 = 0.005;

/// How many rows "What would shorten it" shows at most.
pub const LEVERS_SHOWN: usize = 10;

/// How many of a process's biggest children its card lists.
pub const CHILDREN_SHOWN: usize = 6;

/// A frame of the flame graph narrower than this share of the frame
/// zoomed into is not drawn, nor anything under it.
pub const FRAME_MIN_SHARE: f32 = 0.002;

/// How many rows deep the flame graph draws.
pub const FRAME_ROWS: usize = 24;

/// How many frames of a stack a part's source lines show, from the
/// innermost in the program's own code out.
pub const WHERE_FRAMES: usize = 3;

/// How many of a process's waits on other processes are listed.
pub const WAITS_SHOWN: usize = 12;

/// Two stretches of the chain that look alike are drawn with a gap
/// between them when both are at least this many pixels wide, so a long
/// stretch of one process reads apart from the next.
const SEPARATED_PX: f32 = 6.0;

/// The gap between two such stretches.
pub const GAP_PX: f32 = 1.0;

/// A phase needs this share of the chain to carry its name.
const PHASE_LABEL_MIN_SHARE: f32 = 0.08;

/// What nixpkgs puts at the end of a phase's name, dropped on the strip
/// as the timeline drops it.
const PHASE_SUFFIX: &str = "Phase";

/// Programs whose first argument is the script they run, which names the
/// process better than the interpreter does.
const INTERPRETERS: &[&str] = &[
    "perl", "python", "python3", "bash", "sh", "ruby", "node", "tclsh",
];

/// What the tab says for a run with no clock reading at its end: one with
/// no manifest, or one that has not finished.
pub const NOT_FINISHED: &str =
    "The run has no time on record for its end: it has no manifest, or it has not finished.";

/// What the tab says while the chain is worked out.
pub const WORKING: &str = "Working out the chain\u{2026}";

/// The job's report and its chain in order, worked out once per run.
#[derive(Debug)]
pub struct Computed {
    /// What `Timing::inside` reads, for a process the user goes into.
    pub timing: Timing,
    pub report: Report,
    pub stretches: Vec<Stretch>,
    /// The whole path as a tree, and each phase's part of it, by the
    /// phase's index in the report.
    pub tree: PathTree,
    pub phase_trees: Vec<PathTree>,
    /// The name of every program a process ran, sorted, which a group's
    /// pick names by index.
    pub programs: Vec<String>,
}

impl Computed {
    /// The index of `program` in [`Computed::programs`].
    pub fn program_id(&self, program: &str) -> Option<u32> {
        let at = self
            .programs
            .binary_search_by(|p| p.as_str().cmp(program))
            .ok()?;
        u32::try_from(at).ok()
    }

    /// The processes of a group: every process that ran the program,
    /// under `under` when the group is one process's.
    pub fn members(&self, program: u32, under: Option<Life>) -> HashSet<Life> {
        let Some(name) = self.programs.get(program as usize) else {
            return HashSet::new();
        };
        let all = self.timing.lives_of(name).into_iter();
        let Some(under) = under else {
            return all.collect();
        };
        let below: HashSet<Life> = self.timing.descendants(under).into_iter().collect();
        all.filter(|l| below.contains(l)).collect()
    }

    /// What a row of a card picks: the process it is, or the group of
    /// processes it is, under `under` for a process's own rows.
    pub fn row_pick(&self, row: &Row, under: Option<Life>) -> Option<Pick> {
        if let RowKind::Program { program, .. } = &row.kind {
            let program = self.program_id(program)?;
            return Some(Pick::Program { program, under });
        }
        row.life.map(Pick::Process)
    }

    /// The tree for `phase`, or the whole path's.
    pub fn tree_of(&self, phase: Option<usize>) -> &PathTree {
        phase
            .and_then(|i| self.phase_trees.get(i))
            .unwrap_or(&self.tree)
    }
}

/// What a row of a process's levers picks when it is a part of the
/// process itself: a thread's computing picks the process's own computing,
/// and a thread's sleeps in a call its own sleeps in that call, so the
/// card shows the stack and the source as for the own part's row.
pub fn own_pick(row: &Row, node: &PathNode) -> Option<Pick> {
    let state = match &row.kind {
        RowKind::Thread { .. } => State::Computing,
        RowKind::ThreadSleeps { call, .. } => State::Asleep(call.clone()),
        _ => return None,
    };
    let part = node.own.iter().position(|own| own.state == state)?;
    Some(Pick::Own {
        life: node.life,
        part,
    })
}

/// Works out the run's chain and report: `trace`, its read records `io`,
/// the clock when the run stopped, `end_ns`, and the clock it was recorded
/// with. Why there are none, as a sentence, when there are none.
pub fn compute(
    trace: &Trace,
    io: &IoLog,
    end_ns: Option<u64>,
    clock: ClockSource,
) -> Result<Computed, String> {
    if clock == ClockSource::Exits {
        return Err(sentence(&TimeError::ExitClock));
    }
    let end_ns = end_ns.ok_or_else(|| NOT_FINISHED.to_string())?;
    let timing = Timing::new(trace, io, end_ns).map_err(|e| sentence(&e))?;
    let report = timing.report();
    let stretches = timing.stretches();
    let tree = timing.path_tree(None, None, Budget::Whole);

    // Each phase runs from its first step to the next phase's.
    let phase_trees = report
        .phases
        .iter()
        .enumerate()
        .map(|(i, _)| timing.path_tree(None, Some(phase_steps(&report.phases, i)), Budget::Whole))
        .collect();
    let programs: std::collections::BTreeSet<String> = timing
        .lives
        .iter()
        .flatten()
        .map(|&life| timing.program(life))
        .collect();
    Ok(Computed {
        timing,
        report,
        stretches,
        tree,
        phase_trees,
        programs: programs.into_iter().collect(),
    })
}

/// One process from the inside, as the tab shows it: its threads, their
/// sleeps and what they waited for (`Timing::inside`), and what it started
/// (`Timing::started`).
#[derive(Debug)]
pub struct Drilled {
    pub inside: Inside,
    pub started: Vec<Row>,
}

/// Works out what the tab shows inside `life`.
pub fn drill(timing: &Timing, life: Life) -> Drilled {
    Drilled {
        inside: timing.inside(life),
        started: timing.started(life),
    }
}

/// An error as a sentence: its first letter capitalized, a period at its
/// end.
fn sentence(error: &TimeError) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return text;
    };
    format!("{}{}.", first.to_uppercase(), chars.as_str())
}

/// A duration on the VM's clock, at a precision that suits it, as `rewind
/// time` prints it.
pub fn duration(ns: u64) -> String {
    const US: u64 = 1_000;
    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    const TENFOLD: u64 = 10;
    match ns {
        n if n >= TENFOLD * S => format!("{:.1} s", n as f64 / S as f64),
        n if n >= S => format!("{:.2} s", n as f64 / S as f64),
        n if n >= TENFOLD * MS => format!("{:.0} ms", n as f64 / MS as f64),
        n if n >= MS => format!("{:.1} ms", n as f64 / MS as f64),
        n if n >= US => format!("{:.0} us", n as f64 / US as f64),
        n => format!("{n} ns"),
    }
}

/// The rows "What would shorten it" shows: those with a what-if that
/// would make the run at least [`MIN_GAIN_SHARE`] of its path sooner,
/// soonest first, at most `limit`.
pub fn levers<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
    chain_ns: u64,
    limit: usize,
) -> Vec<&'a Row> {
    let min_gain = ((chain_ns as f64 * MIN_GAIN_SHARE) as u64).max(1);
    let mut out: Vec<&Row> = rows
        .into_iter()
        .filter(|r| r.without_ns.is_some() && r.gain_ns(chain_ns) >= min_gain)
        .filter(|r| !matches!(r.kind, RowKind::Tree { .. }))
        .collect();
    out.sort_by(|a, b| {
        b.gain_ns(chain_ns)
            .cmp(&a.gain_ns(chain_ns))
            .then_with(|| a.label.cmp(&b.label))
    });
    out.truncate(limit);
    out
}

/// How much sooner the run would finish without `row`, as `1.2 s`.
pub fn sooner(row: &Row, chain_ns: u64) -> String {
    duration(row.gain_ns(chain_ns))
}

/// A process's rows with no what-if: what it waited for, which is the
/// time of another process. Only those of other processes, not of its own
/// threads, which its own rows already cover.
pub fn waits(inside: &Inside) -> Vec<&Row> {
    inside
        .rows
        .iter()
        .filter(|r| r.without_ns.is_none() && r.life.is_some())
        .collect()
}

/// What is picked in the flame graph or the card: a process, one part of
/// a process's own time by its index in the node's `own`, or every
/// process that ran a program, by the program's index in
/// [`Computed::programs`], under a process or in the whole run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Process(Life),
    Own { life: Life, part: usize },
    Program { program: u32, under: Option<Life> },
}

impl Pick {
    /// The process picked, for a pick of one.
    pub fn life(&self) -> Option<Life> {
        match self {
            Pick::Process(life) | Pick::Own { life, .. } => Some(*life),
            Pick::Program { .. } => None,
        }
    }
}

/// A piece of the run to go to and look at: its step, process and thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
}

impl Spot {
    pub fn of(life: Life, part: &OwnPart) -> Spot {
        Spot {
            step: part.step,
            pid: life.pid,
            tid: part.tid,
        }
    }
}

/// A line's own time on the path: what it did itself, without what it
/// started.
pub fn own_ns(node: &PathNode) -> u64 {
    node.own.iter().map(|p| p.ns).sum()
}

/// The lines of `tree` whose process is one of `members`, longest own
/// time first.
pub fn members_in<'a>(tree: &'a PathTree, members: &HashSet<Life>) -> Vec<&'a PathNode> {
    fn walk<'a>(node: &'a PathNode, members: &HashSet<Life>, out: &mut Vec<&'a PathNode>) {
        if members.contains(&node.life) || members.contains(&node.top) {
            out.push(node);
        }
        for child in &node.children {
            walk(child, members, out);
        }
    }
    let mut out = Vec::new();
    for root in &tree.roots {
        walk(root, members, &mut out);
    }
    out.sort_by(|a, b| {
        own_ns(b)
            .cmp(&own_ns(a))
            .then_with(|| a.life.pid.cmp(&b.life.pid))
    });
    out
}

/// Where a pick takes the playhead: a part to its biggest piece; a
/// process to its biggest own piece, or with no time of its own, to the
/// first stretch of the path in it or under it; a group, whose processes
/// are `members`, to its longest process's.
pub fn spot_of(
    tree: &PathTree,
    stretches: &[Stretch],
    pick: Pick,
    members: Option<&HashSet<Life>>,
) -> Option<Spot> {
    let node = match pick {
        Pick::Own { life, part } => {
            let node = find(tree, life)?;
            return node.own.get(part).map(|p| Spot::of(node.life, p));
        }
        Pick::Process(life) => find(tree, life)?,
        Pick::Program { .. } => *members_in(tree, members?).first()?,
    };
    if let Some(biggest) = node.own.iter().max_by_key(|p| p.ns) {
        return Some(Spot::of(node.life, biggest));
    }

    // A wrapper: the first stretch of anything under it.
    let mut under = HashSet::new();
    fn lives(node: &PathNode, out: &mut HashSet<Life>) {
        out.insert(node.life);
        out.insert(node.top);
        for child in &node.children {
            lives(child, out);
        }
    }
    lives(node, &mut under);
    let first = stretches
        .iter()
        .find(|s| s.life.is_some_and(|l| under.contains(&l)))?;
    Some(Spot {
        step: first.step,
        pid: node.life.pid,
        tid: node.life.pid,
    })
}

/// How a frame of the flame graph is painted: computing warm, asleep
/// blue, and a process by the share of its time asleep, as Brendan Gregg's
/// off-CPU flame graphs paint them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    Process { asleep: f32 },
    Computing,
    Asleep,
}

/// A frame of the flame graph: its row, where it spans as shares of the
/// width, how it is painted and named, its time, and what a click picks.
#[derive(Clone, Debug, PartialEq)]
pub struct IceFrame {
    pub depth: usize,
    pub from: f32,
    pub to: f32,
    pub paint: Paint,
    pub label: String,
    pub ns: u64,
    pub pick: Pick,
}

/// The flame graph of `top`: `top` across the whole width, and under each
/// process its children and its own time by state, widest first, each as
/// wide as its time on the path. Frames under [`FRAME_MIN_SHARE`] and rows
/// past [`FRAME_ROWS`] are left out.
pub fn icicle(top: &PathNode) -> Vec<IceFrame> {
    let mut out = Vec::new();
    if top.total_ns == 0 {
        return out;
    }
    place(top, 0, 0.0, 1.0, top.total_ns as f64, &mut out);
    out
}

/// What is under a process in the flame graph: a child, or one of its
/// own parts by index.
enum Under<'a> {
    Child(&'a PathNode),
    Own(usize),
}

/// Places `node` at `depth` from `from` to `to`, then what is under it.
fn place(node: &PathNode, depth: usize, from: f32, to: f32, whole: f64, out: &mut Vec<IceFrame>) {
    if to - from < FRAME_MIN_SHARE || depth >= FRAME_ROWS {
        return;
    }
    let asleep = if node.total_ns == 0 {
        0.0
    } else {
        node.asleep_ns as f32 / node.total_ns as f32
    };
    out.push(IceFrame {
        depth,
        from,
        to,
        paint: Paint::Process { asleep },
        label: line_name(node),
        ns: node.total_ns,
        pick: Pick::Process(node.life),
    });

    // Its children and its own parts, widest first.
    let mut under: Vec<(u64, Under)> = node
        .children
        .iter()
        .map(|c| (c.total_ns, Under::Child(c)))
        .chain(
            node.own
                .iter()
                .enumerate()
                .map(|(i, p)| (p.ns, Under::Own(i))),
        )
        .collect();
    under.sort_by_key(|(ns, _)| std::cmp::Reverse(*ns));
    let mut at = from;
    for (ns, part) in under {
        let end = at + (ns as f64 / whole) as f32;
        match part {
            Under::Child(child) => place(child, depth + 1, at, end, whole, out),
            Under::Own(i) => {
                let own = &node.own[i];
                let drawn = end - at >= FRAME_MIN_SHARE && depth + 1 < FRAME_ROWS;
                if drawn {
                    out.push(IceFrame {
                        depth: depth + 1,
                        from: at,
                        to: end,
                        paint: match own.state {
                            State::Computing => Paint::Computing,
                            State::Asleep(_) => Paint::Asleep,
                        },
                        label: own.state.name(),
                        ns,
                        pick: Pick::Own {
                            life: node.life,
                            part: i,
                        },
                    });
                }
            }
        }
        at = end;
    }
}

/// A line's name as `rewind time` prints it: the wrappers that share it,
/// then the command line.
pub fn line_name(node: &PathNode) -> String {
    let mut name = String::new();
    for w in &node.through {
        name.push_str(w);
        name.push_str(" \u{203a} ");
    }
    name.push_str(&node.label);
    name
}

/// The line of `life` in `tree`: the one it has, or shares as a wrapper.
pub fn find(tree: &PathTree, life: Life) -> Option<&PathNode> {
    fn under(node: &PathNode, life: Life) -> Option<&PathNode> {
        if node.life == life || node.top == life {
            return Some(node);
        }
        node.children.iter().find_map(|c| under(c, life))
    }
    tree.roots.iter().find_map(|r| under(r, life))
}

/// One line of where a part's time was spent: a frame's function, its
/// file and line, and the line's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhereLine {
    /// gdb's number for the frame, which `frame N` in gdb goes to.
    pub level: u32,
    pub function: String,
    pub place: String,
    pub text: String,
}

/// The stack gdb found, from the innermost frame in the program's own
/// code out, at most [`WHERE_FRAMES`] frames: the source lines the part's
/// biggest piece was spent at.
pub fn where_lines(located: &Located) -> Vec<WhereLine> {
    let from = located.chosen.unwrap_or(0);
    located
        .frames
        .iter()
        .skip(from)
        .take(WHERE_FRAMES)
        .map(|f| {
            let file = f.file.clone().unwrap_or_default();
            let place = match f.line {
                Some(line) => format!("{file}:{line}"),
                None => file,
            };
            let text = f
                .fullname
                .as_ref()
                .zip(f.line)
                .and_then(|(name, line)| {
                    let source = located.files.get(name)?;
                    let at = line.checked_sub(source.first)? as usize;
                    source.lines.get(at).map(|l| l.trim().to_string())
                })
                .unwrap_or_default();
            WhereLine {
                level: f.level,
                function: f.function.clone().unwrap_or_else(|| "??".to_string()),
                place,
                text,
            }
        })
        .collect()
}

/// The frames gdb found inside libraries before the program's own code,
/// on one line, as `#0–#2 in libc.so.6: __libc_fork, make_child`: the
/// stack the where lines leave out, so their numbers, which are gdb's, do
/// not seem to skip. None when the stack starts in the program's code.
pub fn skipped_frames(located: &Located) -> Option<String> {
    let from = located.chosen.unwrap_or(0);
    let skipped = located.frames.get(..from).filter(|f| !f.is_empty())?;
    let first = skipped.first()?.level;
    let last = skipped.last()?.level;
    let levels = if first == last {
        format!("#{first}")
    } else {
        format!("#{first}\u{2013}#{last}")
    };
    let mut objects: Vec<&str> = Vec::new();
    for f in skipped {
        let object = f
            .object
            .as_deref()
            .map(|o| o.rsplit('/').next().unwrap_or(o))
            .unwrap_or("??");
        if !objects.contains(&object) {
            objects.push(object);
        }
    }
    let functions: Vec<&str> = skipped
        .iter()
        .map(|f| f.function.as_deref().unwrap_or("??"))
        .collect();
    Some(format!(
        "{levels} in {}: {}",
        objects.join(", "),
        functions.join(", ")
    ))
}

/// A short name for a process in the breadcrumb, from its label as the
/// report gives it, `perl autoconf-2.72/bin/autoreconf --install (pid
/// 70)`: the script an interpreter ran, else the program, and the pid.
pub fn crumb(label: &str, pid: u32) -> String {
    let pid_suffix = format!(" (pid {pid})");
    let command = label.strip_suffix(&pid_suffix).unwrap_or(label);
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or(command);
    let script = words
        .next()
        .filter(|w| INTERPRETERS.contains(&program) && !w.starts_with('-'));
    let name = script.unwrap_or(program);
    let base = name.rsplit('/').next().unwrap_or(name);
    format!("{base}{pid_suffix}")
}

/// The processes the user went into, the job first and the one shown
/// last. Going into a process already on the trail goes back to it, so a
/// process and one it waited for, which waited on it in turn, never make
/// the trail grow without end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trail {
    lives: Vec<Life>,
}

impl Trail {
    /// The process shown, or None for the job.
    pub fn shown(&self) -> Option<Life> {
        self.lives.last().copied()
    }

    pub fn lives(&self) -> &[Life] {
        &self.lives
    }

    /// Goes into `life`.
    pub fn enter(&mut self, life: Life) {
        if let Some(at) = self.lives.iter().position(|l| *l == life) {
            self.lives.truncate(at + 1);
            return;
        }
        self.lives.push(life);
    }

    /// Goes one process back out; false at the job, which has no way out.
    pub fn out(&mut self) -> bool {
        self.lives.pop().is_some()
    }

    /// Goes back to the crumb at `depth`: 0 is the job, 1 the first
    /// process gone into.
    pub fn back_to(&mut self, depth: usize) {
        self.lives.truncate(depth);
    }
}

/// What a stretch of the chain was doing, which sets its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    Computing,
    Sleeping,
}

/// A piece of the chain bar to paint, in pixels from the bar's left edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub from: f32,
    pub to: f32,
    pub doing: Doing,
}

/// The chain bar's segments, for a bar `width` pixels wide over a chain of
/// `chain_ns`. Stretches next to each other that look alike merge into one
/// segment, so thousands of stretches paint as a few hundred quads at
/// most; two that are both [`SEPARATED_PX`] wide keep a [`GAP_PX`] gap.
pub fn segments(stretches: &[Stretch], chain_ns: u64, width: f32) -> Vec<Segment> {
    if chain_ns == 0 {
        return Vec::new();
    }
    let x = |ns: u64| (ns as f64 / chain_ns as f64) as f32 * width;
    let mut out: Vec<Segment> = Vec::new();
    let mut last_wide = false;
    for s in stretches {
        let doing = match s.state {
            State::Asleep(_) => Doing::Sleeping,
            State::Computing => Doing::Computing,
        };
        let (from, to) = (x(s.at_ns), x(s.at_ns + s.ns));
        let wide = to - from >= SEPARATED_PX;

        // A stretch that looks like the segment before it extends that
        // segment, unless both are wide enough to show a gap between.
        if let Some(prev) = out.last_mut()
            && prev.doing == doing
        {
            if !(wide && last_wide) {
                prev.to = to;
                last_wide = wide;
                continue;
            }
            prev.to = (from - GAP_PX).max(prev.from);
        }
        out.push(Segment { from, to, doing });
        last_wide = wide;
    }
    out
}

/// A segment of the path bar is at least this many pixels wide, so a short
/// stretch still shows.
pub const MIN_SEGMENT_PX: f32 = 1.0;

/// Whether a segment fills its own width when painted, or is widened to
/// [`MIN_SEGMENT_PX`] and so overlaps the segment after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Thickness {
    Full,
    Thin,
}

/// `segments` in the order to paint them: those at least a pixel wide
/// first, then the thinner ones, so a burst widened to a pixel is painted
/// over its neighbors instead of under the one after it. Each kind keeps
/// its order along the bar.
pub fn paint_order(segments: &[Segment]) -> Vec<Segment> {
    let thickness = |s: &Segment| match s.to - s.from < MIN_SEGMENT_PX {
        true => Thickness::Thin,
        false => Thickness::Full,
    };
    let mut out = segments.to_vec();
    out.sort_by_key(thickness);
    out
}

/// The stretch at `fraction` of the way along the chain.
pub fn stretch_at(stretches: &[Stretch], chain_ns: u64, fraction: f32) -> Option<usize> {
    let at = (fraction.clamp(0.0, 1.0) as f64 * chain_ns as f64) as u64;
    let i = stretches.partition_point(|s| s.at_ns + s.ns <= at);
    if i < stretches.len() {
        return Some(i);
    }
    stretches.len().checked_sub(1)
}

/// The steps of phase `index` of `phases`: from its first step to the
/// next phase's.
pub fn phase_steps(phases: &[Phase], index: usize) -> std::ops::Range<u64> {
    let start = phases.get(index).map_or(0, |p| p.step);
    let end = phases.get(index + 1).map_or(u64::MAX, |next| next.step);
    start..end
}

/// Where the playhead is on the path: the stretch going on at its step,
/// and how far along the path that is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayheadOnPath {
    /// The stretch's index in the path's stretches.
    pub stretch: usize,
    pub ns: u64,
}

/// Where `step` is on the path whose stretches are `stretches`: in the
/// last stretch that starts at or before it, as far into that stretch as
/// the step is between its first step and the next stretch's. Any step
/// has a place on the path, since the path runs from the run's start to
/// its end, and a step where something off the path runs is at the
/// stretch of the path going on beside it.
pub fn playhead_on_path(stretches: &[Stretch], step: u64) -> Option<PlayheadOnPath> {
    let i = stretches
        .partition_point(|s| s.step <= step)
        .saturating_sub(1);
    let s = stretches.get(i)?;

    // How far into the stretch, by steps, up to the next stretch's first.
    let into = match stretches.get(i + 1) {
        Some(next) if next.step > s.step && step >= s.step => {
            let share = (step - s.step) as f64 / (next.step - s.step) as f64;
            (share.min(1.0) * s.ns as f64) as u64
        }
        _ => 0,
    };
    Some(PlayheadOnPath {
        stretch: i,
        ns: s.at_ns + into,
    })
}

/// What runs on the CPU at a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    Process(Life),
    /// A kernel thread the trace saw no fork of.
    Kernel,
    /// The idle task: nothing could run.
    Idle,
}

/// Whether the playhead is on the path: on it where the path's process
/// computes, or beside it, with what runs there instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtStep {
    OnPath,
    Beside(Runner),
}

/// The pid of the idle task.
const IDLE_PID: u32 = 0;

/// Whether `step`, in the path's `stretch`, is on the path, given the
/// CPU's `runs` in order and the process of each in `lives`. A step is on
/// the path when the stretch computes and the run at the step is its
/// process's; while the stretch sleeps, whatever runs is beside it.
pub fn at_playhead(runs: &[Run], lives: &[Option<Life>], stretch: &Stretch, step: u64) -> AtStep {
    let Some(i) = runs
        .partition_point(|r| r.start_step <= step)
        .checked_sub(1)
    else {
        return AtStep::Beside(Runner::Idle);
    };
    let life = lives.get(i).copied().flatten();
    if stretch.state == State::Computing && life.is_some() && life == stretch.life {
        return AtStep::OnPath;
    }
    let runner = match life {
        Some(l) => Runner::Process(l),
        None if runs[i].pid == IDLE_PID => Runner::Idle,
        None => Runner::Kernel,
    };
    AtStep::Beside(runner)
}

/// The frame of `frames`, the flame graph of `top`, that shows what
/// `stretch` does: its process's part in the stretch's state when that
/// part is drawn, or else the deepest drawn process on the line from
/// `top` down to its process. A process the tree leaves out is looked
/// for by its `lineage`, the stretch's process and then each parent up,
/// so the frame marked is that of the nearest process it runs under.
/// None for a stretch of the kernel, or of a process with nothing of its
/// lineage under `top`. When the process `picked` is on that line, its
/// frame is the one marked: a deeper frame lit under the picked one reads
/// as the frame clicked.
pub fn playhead_frame(
    frames: &[IceFrame],
    top: &PathNode,
    stretch: &Stretch,
    lineage: &[Life],
    picked: Option<Pick>,
) -> Option<usize> {
    let own = stretch.life?;
    let line = lineage.iter().find_map(|&l| line_to(top, l))?;
    let node = line.last()?;

    // The picked process's frame, when the stretch runs under it.
    if let Some(Pick::Process(life)) = picked
        && line.iter().any(|n| n.life == life)
        && let Some(i) = frames.iter().position(|f| f.pick == Pick::Process(life))
    {
        return Some(i);
    }

    // The process's own part in the stretch's state, when drawn.
    let its_own = node.life == own || node.top == own;
    if its_own && let Some(part) = node.own.iter().position(|p| p.state == stretch.state) {
        let pick = Pick::Own {
            life: node.life,
            part,
        };
        if let Some(i) = frames.iter().position(|f| f.pick == pick) {
            return Some(i);
        }
    }

    // Else the deepest process on the line that has a frame.
    line.iter().rev().find_map(|n| {
        let pick = Pick::Process(n.life);
        frames.iter().position(|f| f.pick == pick)
    })
}

/// The nodes from `node` down to the one of `life`, as [`find`] matches
/// it, or None when `life` is not under `node`.
fn line_to(node: &PathNode, life: Life) -> Option<Vec<&PathNode>> {
    if node.life == life || node.top == life {
        return Some(vec![node]);
    }
    for child in &node.children {
        if let Some(mut line) = line_to(child, life) {
            line.insert(0, node);
            return Some(line);
        }
    }
    None
}

/// The least width of a mark under the path bar, so the stretch of a
/// short pick on a long path still shows.
pub const MIN_MARK_PX: f32 = 2.0;

/// What a pick marks under the path bar: the stretches of a set of
/// processes, a process and what it started or every process of a
/// program, or those of one part of a process's own time, its stretches
/// in one state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marked {
    Lives(HashSet<Life>),
    Part { life: Life, state: State },
}

impl Marked {
    /// Whether `stretch` is one the pick marks.
    pub fn holds(&self, stretch: &Stretch) -> bool {
        match self {
            Marked::Lives(lives) => stretch.life.is_some_and(|l| lives.contains(&l)),
            Marked::Part { life, state } => stretch.life == Some(*life) && stretch.state == *state,
        }
    }
}

/// The marks under a path bar `width` pixels wide over a chain of
/// `chain_ns` where the stretches `marked` holds are, from and to in
/// pixels from the bar's left edge; with `steps`, a phase's, only the
/// stretches that start in it. Marks that touch are one, and each is at
/// least [`MIN_MARK_PX`] wide and inside the bar.
pub fn pick_marks(
    stretches: &[Stretch],
    chain_ns: u64,
    marked: &Marked,
    steps: Option<std::ops::Range<u64>>,
    width: f32,
) -> Vec<(f32, f32)> {
    if chain_ns == 0 {
        return Vec::new();
    }
    let x = |ns: u64| (ns as f64 / chain_ns as f64) as f32 * width;
    let mut out: Vec<(f32, f32)> = Vec::new();
    for s in stretches {
        let in_steps = steps.as_ref().is_none_or(|r| r.contains(&s.step));
        if !in_steps || !marked.holds(s) {
            continue;
        }

        // The stretch's span, widened to the least mark and kept inside.
        let from = x(s.at_ns);
        let to = x(s.at_ns + s.ns).max(from + MIN_MARK_PX);
        let (from, to) = if to > width {
            ((width - MIN_MARK_PX).max(0.0), width)
        } else {
            (from, to)
        };

        // A mark that touches the one before extends it.
        if let Some(prev) = out.last_mut()
            && from <= prev.1
        {
            prev.1 = prev.1.max(to);
            continue;
        }
        out.push((from, to));
    }
    out
}

/// Whether a phase's block on the strip carries its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Shown,
    Hidden,
}

/// A phase's block on the strip under the chain bar, as shares of the
/// chain: where it starts and how long it is.
#[derive(Clone, Debug, PartialEq)]
pub struct PhaseBlock {
    pub name: String,
    pub from: f32,
    pub share: f32,
    pub named: Named,
    /// The phase's index in the report.
    pub index: usize,
}

/// The phases on the chain bar's scale: each as long as its time on the
/// chain, in order, after the stretch of the chain before the first phase
/// (nixpkgs' setup). A phase with no time on the chain has no block.
pub fn phase_blocks(phases: &[Phase], chain_ns: u64) -> Vec<PhaseBlock> {
    if chain_ns == 0 {
        return Vec::new();
    }
    let on_phases: u64 = phases.iter().map(|p| p.on_chain_ns).sum();
    let mut at = chain_ns.saturating_sub(on_phases);
    let mut out = Vec::new();
    for (index, p) in phases.iter().enumerate() {
        if p.on_chain_ns == 0 {
            continue;
        }
        let share = (p.on_chain_ns as f64 / chain_ns as f64) as f32;
        out.push(PhaseBlock {
            name: short_phase(&p.name),
            from: (at as f64 / chain_ns as f64) as f32,
            share,
            named: if share >= PHASE_LABEL_MIN_SHARE {
                Named::Shown
            } else {
                Named::Hidden
            },
            index,
        });
        at += p.on_chain_ns;
    }
    out
}

/// A phase's name without nixpkgs' "Phase" at its end.
fn short_phase(name: &str) -> String {
    match name.strip_suffix(PHASE_SUFFIX) {
        Some(short) if !short.is_empty() => short.to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    // The Time tab's layout and state from made-up stretches, phases and
    // rows of a chain of 100 ns.
    use super::*;
    use rewind_trace::time_report::OwnPart;

    const CHAIN: u64 = 100;

    fn life(pid: u32) -> Life {
        Life { pid, start: 0 }
    }

    fn stretch(pid: u32, at_ns: u64, ns: u64, sleeping: bool) -> Stretch {
        let state = if sleeping {
            State::Asleep("clock_nanosleep".to_string())
        } else {
            State::Computing
        };
        Stretch {
            life: Some(life(pid)),
            label: format!("pid {pid}"),
            state,
            at_ns,
            ns,
            step: at_ns,
        }
    }

    fn row(without_ns: Option<u64>, on_chain_ns: u64) -> Row {
        Row {
            kind: RowKind::Process,
            label: "make (pid 7)".to_string(),
            took_ns: on_chain_ns,
            on_chain_ns,
            without_ns,
            life: Some(life(7)),
            step: Some(1),
        }
    }

    fn phase(name: &str, on_chain_ns: u64) -> Phase {
        Phase {
            name: name.to_string(),
            took_ns: on_chain_ns,
            on_chain_ns,
            step: 0,
        }
    }

    #[test]
    fn durations_read_as_the_command_prints_them() {
        // One duration in each precision band, as `rewind time` prints
        // them in its tests.
        assert_eq!(duration(16_600_000_000), "16.6 s");
        assert_eq!(duration(2_030_000_000), "2.03 s");
        assert_eq!(duration(36_000_000), "36 ms");
        assert_eq!(duration(1_500_000), "1.5 ms");
        assert_eq!(duration(45_000), "45 us");
        assert_eq!(duration(12), "12 ns");
    }

    #[test]
    fn an_error_reads_as_a_sentence() {
        // The exit clock's error, which starts in lower case and has no
        // period, comes back capitalized with one.
        let text = sentence(&TimeError::ExitClock);
        assert!(text.starts_with("The run was recorded with exit time"));
        assert!(text.ends_with('.'));
    }

    #[test]
    fn a_run_on_the_exit_clock_has_no_report() {
        // A run recorded on the exit clock is turned away before its
        // trace is read, with the error `rewind time` gives.
        let got = compute(
            &Trace::default(),
            &IoLog::default(),
            Some(1),
            ClockSource::Exits,
        );
        assert_eq!(got.err(), Some(sentence(&TimeError::ExitClock)));
    }

    #[test]
    fn the_shortening_list_is_one_number_soonest_first() {
        // On a path of 1,000 ns the least gain shown is 5 ns: a row
        // gaining 4 is left out, a wait has no gain, the whole tree of a
        // process is left to its card, and the rest come soonest first,
        // as many as the limit lets.
        let mut tree = row(Some(100), 900);
        tree.kind = RowKind::Tree { processes: 3 };
        let rows = [
            row(Some(990), 10),
            row(Some(900), 100),
            row(Some(996), 4),
            row(None, 50),
            tree,
        ];
        let got = levers(rows.iter(), 1_000, LEVERS_SHOWN);
        let gains: Vec<String> = got.iter().map(|r| sooner(r, 1_000)).collect();
        assert_eq!(gains, vec!["100 ns", "10 ns"]);
        assert_eq!(levers(rows.iter(), 1_000, 1).len(), 1);
    }

    /// A line of a made-up tree, with no own time and no children.
    fn node(pid: u32, total_ns: u64, asleep_ns: u64) -> PathNode {
        PathNode {
            life: life(pid),
            top: life(pid),
            through: Vec::new(),
            label: format!("p{pid}"),
            total_ns,
            asleep_ns,
            own: Vec::new(),
            children: Vec::new(),
            hidden: 0,
            hidden_ns: 0,
        }
    }

    /// A part of a process's own time.
    fn own(state: State, ns: u64) -> OwnPart {
        OwnPart {
            state,
            ns,
            count: 1,
            tid: 1,
            step: 1,
            biggest_ns: ns,
        }
    }

    /// A part of a process's own time at `step`.
    fn own_at(state: State, ns: u64, step: u64) -> OwnPart {
        OwnPart {
            step,
            tid: step as u32,
            ..own(state, ns)
        }
    }

    #[test]
    fn a_group_lists_its_processes_by_their_own_time() {
        // A runner of 100 ns computes 10 itself and runs two tests that
        // spend 60 and 30 of their own: all three ran the program, so the
        // group is the tests, longest first, then the runner, each by its
        // own time and not with what it started; a process outside the
        // group is left out.
        let mut slow = node(2, 60, 0);
        slow.own = vec![own(State::Computing, 60)];
        let mut quick = node(3, 30, 0);
        quick.own = vec![own(State::Computing, 30)];
        let mut runner = node(1, 100, 0);
        runner.own = vec![own(State::Computing, 10)];
        runner.children = vec![quick, slow];
        let tree = PathTree {
            ns: 100,
            states: Vec::new(),
            roots: vec![runner],
            kernel_ns: 0,
        };
        let members: HashSet<Life> = [life(1), life(2), life(3)].into_iter().collect();
        let got: Vec<(u32, u64)> = members_in(&tree, &members)
            .iter()
            .map(|n| (n.life.pid, own_ns(n)))
            .collect();
        assert_eq!(got, vec![(2, 60), (3, 30), (1, 10)]);
        let some: HashSet<Life> = [life(3)].into_iter().collect();
        assert_eq!(members_in(&tree, &some).len(), 1);
    }

    #[test]
    fn a_pick_goes_to_its_biggest_piece() {
        // A test sleeps 60 ns from step 5 and computes 30 from step 9: the
        // test goes to its sleep, its computing to step 9, and a wrapper
        // with no time of its own to the first stretch of the path in it
        // or under it; a group goes to its longest process's biggest
        // piece.
        let mut test = node(2, 90, 60);
        test.own = vec![
            own_at(State::Asleep("clock_nanosleep".to_string()), 60, 5),
            own_at(State::Computing, 30, 9),
        ];
        let mut wrapper = node(1, 90, 60);
        wrapper.children = vec![test];
        let tree = PathTree {
            ns: 90,
            states: Vec::new(),
            roots: vec![wrapper],
            kernel_ns: 0,
        };
        let stretches = vec![stretch(9, 0, 5, false), stretch(2, 5, 60, true)];
        let at = |pick: Pick| spot_of(&tree, &stretches, pick, None).map(|s| (s.step, s.pid));
        assert_eq!(at(Pick::Process(life(2))), Some((5, 2)));
        assert_eq!(
            at(Pick::Own {
                life: life(2),
                part: 1
            }),
            Some((9, 2))
        );
        assert_eq!(at(Pick::Process(life(1))), Some((5, 1)));
        let members: HashSet<Life> = [life(2)].into_iter().collect();
        let group = Pick::Program {
            program: 0,
            under: None,
        };
        let got = spot_of(&tree, &stretches, group, Some(&members)).map(|s| (s.step, s.pid));
        assert_eq!(got, Some((5, 2)));
    }

    #[test]
    fn a_threads_row_picks_the_own_part_in_its_state() {
        // A test computes 30 ns and sleeps 60 in clock_nanosleep: the
        // lever row for its thread's computing picks its computing part,
        // the row for its thread's sleeps in clock_nanosleep its sleep
        // part, and a sleep in a call it has no part for picks nothing.
        let mut test = node(2, 90, 60);
        test.own = vec![
            own(State::Asleep("clock_nanosleep".to_string()), 60),
            own(State::Computing, 30),
        ];
        let lever = |kind: RowKind| Row {
            kind,
            life: None,
            ..row(Some(10), 30)
        };
        let sleeps = |call: &str| RowKind::ThreadSleeps {
            tid: 1,
            count: 1,
            call: call.to_string(),
        };
        let part = |part| {
            Some(Pick::Own {
                life: life(2),
                part,
            })
        };
        assert_eq!(own_pick(&lever(RowKind::Thread { tid: 1 }), &test), part(1));
        assert_eq!(own_pick(&lever(sleeps("clock_nanosleep")), &test), part(0));
        assert_eq!(own_pick(&lever(sleeps("epoll_pwait")), &test), None);
    }

    #[test]
    fn the_flame_graph_puts_each_part_under_its_process_widest_first() {
        // A runner of 100 ns computes 10 itself and runs a test of 90
        // that sleeps 60 and computes 30: the test is under the runner
        // before the runner's own computing, the test's sleep before its
        // computing, each as wide as its time, and a part too thin to
        // draw is left out.
        let mut test = node(2, 90, 60);
        test.own = vec![
            own(State::Asleep("clock_nanosleep".to_string()), 60),
            own(State::Computing, 30),
        ];
        let mut runner = node(1, 100, 60);
        runner.children = vec![test];
        runner.own = vec![
            own(State::Computing, 10),
            own(State::Asleep("futex".to_string()), 0),
        ];

        let frames = icicle(&runner);
        let shape: Vec<(usize, String, f32, f32)> = frames
            .iter()
            .map(|f| (f.depth, f.label.clone(), f.from, f.to))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "p1".to_string(), 0.0, 1.0),
                (1, "p2".to_string(), 0.0, 0.9),
                (2, "asleep in clock_nanosleep".to_string(), 0.0, 0.6),
                (2, "computing".to_string(), 0.6, 0.90000004),
                (1, "computing".to_string(), 0.9, 1.0),
            ]
        );
        assert_eq!(frames[0].paint, Paint::Process { asleep: 0.6 });
        assert_eq!(frames[2].paint, Paint::Asleep);
        assert_eq!(
            frames[2].pick,
            Pick::Own {
                life: life(2),
                part: 0
            }
        );

        // Each process is found by its line or a wrapper that shares it.
        let mut wrapped = runner.clone();
        wrapped.top = life(9);
        let tree = PathTree {
            ns: 100,
            states: Vec::new(),
            roots: vec![wrapped],
            kernel_ns: 0,
        };
        assert_eq!(find(&tree, life(2)).map(|n| n.total_ns), Some(90));
        assert_eq!(find(&tree, life(9)).map(|n| n.life), Some(life(1)));
        assert!(find(&tree, life(3)).is_none());
    }

    #[test]
    fn where_lines_start_at_the_programs_own_frame() {
        // gdb's stack of a sleep: the C library's two frames, then the
        // program's two; the lines start at the program's first frame,
        // each with gdb's number for it and its text from the file gdb
        // read, and the C library's frames are summed up on one line.
        let frame =
            |level: u32, function: &str, file: &str, line: u32| rewind_trace::located::Frame {
                function: Some(function.to_string()),
                file: Some(file.to_string()),
                fullname: Some(format!("/src/{file}")),
                line: Some(line),
                level,
                pc: String::new(),
                object: Some("/nix/store/0000-glibc-2.40/lib/libc.so.6".to_string()),
                arguments: Vec::new(),
                locals: Vec::new(),
                expressions: Vec::new(),
                evaluated: Vec::new(),
            };
        let file = |first: u32, lines: &[&str]| crate::source::SourceFile {
            extent: rewind_trace::located::Extent::Window,
            first,
            lines: lines.iter().map(|l| l.to_string()).collect(),
            widest: 0,
        };
        let located = Located {
            pid: 5,
            tid: 5,
            process: "lt-run-tests".to_string(),
            frames: vec![
                frame(0, "__clock_nanosleep", "nanosleep.c", 48),
                frame(1, "clock_nanosleep", "nanosleep.c", 78),
                frame(2, "uv_sleep", "core.c", 2),
                frame(3, "run_test_hrtime", "test-hrtime.c", 39),
            ],
            chosen: Some(2),
            files: [
                (
                    "/src/core.c".to_string(),
                    file(1, &["do", "  rc = nanosleep(&timeout, &timeout);"]),
                ),
                (
                    "/src/test-hrtime.c".to_string(),
                    file(39, &["    uv_sleep(45);"]),
                ),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            skipped_frames(&located),
            Some("#0\u{2013}#1 in libc.so.6: __clock_nanosleep, clock_nanosleep".to_string())
        );
        let got = where_lines(&located);
        assert_eq!(
            got,
            vec![
                WhereLine {
                    level: 2,
                    function: "uv_sleep".to_string(),
                    place: "core.c:2".to_string(),
                    text: "rc = nanosleep(&timeout, &timeout);".to_string(),
                },
                WhereLine {
                    level: 3,
                    function: "run_test_hrtime".to_string(),
                    place: "test-hrtime.c:39".to_string(),
                    text: "uv_sleep(45);".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_crumb_names_the_script_an_interpreter_ran() {
        // A perl script is named by the script, a compiler by itself, and
        // an interpreter given only flags by the interpreter.
        assert_eq!(
            crumb(
                "perl autoconf-2.72/bin/autoreconf --install --force (pid 70)",
                70
            ),
            "autoreconf (pid 70)"
        );
        assert_eq!(crumb("cc1 /build/a.c (pid 5)", 5), "cc1 (pid 5)");
        assert_eq!(crumb("bash -e (pid 9)", 9), "bash (pid 9)");
    }

    #[test]
    fn the_trail_goes_in_and_back_out() {
        // Into 70, then 694, then out once; into 694 and back to 70 by its
        // crumb; into a process already on the trail goes back to it; out
        // at the job does nothing.
        let mut trail = Trail::default();
        assert_eq!(trail.shown(), None);
        trail.enter(life(70));
        trail.enter(life(694));
        assert_eq!(trail.shown(), Some(life(694)));
        assert!(trail.out());
        assert_eq!(trail.shown(), Some(life(70)));
        trail.enter(life(694));
        trail.back_to(1);
        assert_eq!(trail.lives(), &[life(70)]);
        trail.enter(life(694));
        trail.enter(life(70));
        assert_eq!(trail.lives(), &[life(70)]);
        trail.back_to(0);
        assert!(!trail.out());
    }

    #[test]
    fn narrow_stretches_alike_merge_into_one_segment() {
        // Ten computing stretches of 10 ns on a bar 100 px wide: each is
        // 10 px, so they keep gaps. On a bar 20 px wide each is 2 px, and
        // they merge into one segment the bar's width.
        let stretches: Vec<Stretch> = (0..10)
            .map(|i| stretch(i, i as u64 * 10, 10, false))
            .collect();
        let wide = segments(&stretches, CHAIN, 100.0);
        assert_eq!(wide.len(), 10);
        assert_eq!(wide[0].to, 10.0 - GAP_PX);
        let narrow = segments(&stretches, CHAIN, 20.0);
        assert_eq!(narrow.len(), 1);
        assert_eq!((narrow[0].from, narrow[0].to), (0.0, 20.0));
    }

    #[test]
    fn a_sleep_is_a_segment_of_its_own() {
        // Computing, a sleep, computing, all narrow: three segments, the
        // sleep in the middle, with no gaps.
        let stretches = vec![
            stretch(1, 0, 40, false),
            stretch(1, 40, 20, true),
            stretch(2, 60, 40, false),
        ];
        let got = segments(&stretches, CHAIN, 10.0);
        let doing: Vec<Doing> = got.iter().map(|s| s.doing).collect();
        assert_eq!(doing, [Doing::Computing, Doing::Sleeping, Doing::Computing]);
        assert_eq!((got[1].from, got[1].to), (4.0, 6.0));
    }

    #[test]
    fn a_stretch_thinner_than_a_pixel_paints_over_its_neighbors() {
        // A sleep of 400 ns, a burst of computing of 2 ns and a sleep of
        // 598 ns on a bar 10 px wide: the burst is a fifth of a pixel,
        // widened to a pixel when painted, so the sleep after it would
        // paint over most of it. Painted, the two sleeps come first in
        // their order and the burst last, on top.
        let stretches = vec![
            stretch(1, 0, 400, true),
            stretch(1, 400, 2, false),
            stretch(1, 402, 598, true),
        ];
        let got = segments(&stretches, 1000, 10.0);
        let painted: Vec<(Doing, i32)> = paint_order(&got)
            .iter()
            .map(|s| (s.doing, (s.from * 100.0).round() as i32))
            .collect();
        assert_eq!(
            painted,
            [
                (Doing::Sleeping, 0),
                (Doing::Sleeping, 402),
                (Doing::Computing, 400)
            ]
        );
    }

    #[test]
    fn the_pick_is_marked_under_the_bar_where_it_is_on_the_path() {
        // A bar 50 px wide over a chain of 100 ns: pid 2 computes at
        // 40-60 ns and sleeps at 99-100 ns, pid 3 sleeps at 60-80 ns. With
        // pid 2 picked its marks are 20-30 px and its last stretch, half a
        // pixel, widened to the least mark inside the bar. With pids 2 and
        // 3 the two that touch are one mark. A pick not on the path has
        // none. Pid 2's computing alone is its first stretch; and only the
        // steps from 90 on, its last.
        let stretches = vec![
            stretch(1, 0, 40, false),
            stretch(2, 40, 20, false),
            stretch(3, 60, 20, true),
            stretch(1, 80, 19, false),
            stretch(2, 99, 1, true),
        ];
        let tenth = |px: f32| (px * 10.0).round() / 10.0;
        let marks = |marked: Marked, steps: Option<std::ops::Range<u64>>| -> Vec<(f32, f32)> {
            pick_marks(&stretches, CHAIN, &marked, steps, 50.0)
                .into_iter()
                .map(|(from, to)| (tenth(from), tenth(to)))
                .collect()
        };
        let lives = |pids: &[u32]| Marked::Lives(pids.iter().map(|&p| life(p)).collect());
        let end = 50.0 - MIN_MARK_PX;
        assert_eq!(marks(lives(&[2]), None), [(20.0, 30.0), (end, 50.0)]);
        assert_eq!(marks(lives(&[2, 3]), None), [(20.0, 40.0), (end, 50.0)]);
        assert_eq!(marks(lives(&[9]), None), []);
        let computing = Marked::Part {
            life: life(2),
            state: State::Computing,
        };
        assert_eq!(marks(computing, None), [(20.0, 30.0)]);
        assert_eq!(marks(lives(&[2]), Some(90..u64::MAX)), [(end, 50.0)]);
    }

    #[test]
    fn the_stretch_under_the_pointer_is_found_by_its_place() {
        // Stretches over 0-40, 40-60 and 60-100: a fifth of the way is
        // the first, half way the second, and the very end the last.
        let stretches = vec![
            stretch(1, 0, 40, false),
            stretch(2, 40, 20, true),
            stretch(3, 60, 40, false),
        ];
        assert_eq!(stretch_at(&stretches, CHAIN, 0.2), Some(0));
        assert_eq!(stretch_at(&stretches, CHAIN, 0.5), Some(1));
        assert_eq!(stretch_at(&stretches, CHAIN, 1.0), Some(2));
        assert_eq!(stretch_at(&[], CHAIN, 0.5), None);
    }

    #[test]
    fn phases_follow_the_setup_on_the_chains_scale() {
        // 10 ns of the chain before the first phase, then phases of 60,
        // 0 and 30 ns: the empty phase has no block, the others start at
        // 0.1 and 0.7, and the one of 3 ns too short to carry a name.
        let phases = [
            phase("configurePhase", 60),
            phase("patchPhase", 0),
            phase("buildPhase", 27),
            phase("fixupPhase", 3),
        ];
        let got = phase_blocks(&phases, CHAIN);
        let names: Vec<&str> = got.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["configure", "build", "fixup"]);
        assert!((got[0].from - 0.1).abs() < 1e-6);
        assert!((got[1].from - 0.7).abs() < 1e-6);
        assert_eq!(got[1].index, 2);
        assert_eq!(got[2].named, Named::Hidden);
    }

    #[test]
    fn the_playhead_sits_along_the_stretch_its_step_is_in() {
        // Three stretches of 10 ns from steps 0, 10 and 30: step 0 is the
        // path's start, step 20 is halfway through the second stretch by
        // its steps, and a step past the last stretch's first step is that
        // stretch's start, having no next stretch to measure by. A path of
        // no stretches has no playhead on it.
        let mut stretches = vec![
            stretch(1, 0, 10, false),
            stretch(2, 10, 10, true),
            stretch(3, 20, 10, false),
        ];
        stretches[2].step = 30;
        let at = |step: u64| playhead_on_path(&stretches, step).map(|p| (p.stretch, p.ns));
        assert_eq!(at(0), Some((0, 0)));
        assert_eq!(at(20), Some((1, 15)));
        assert_eq!(at(40), Some((2, 20)));
        assert_eq!(playhead_on_path(&[], 5).map(|p| p.ns), None);
    }

    #[test]
    fn the_playhead_marks_the_frame_of_what_its_stretch_does() {
        // A runner computes 409 of its own, starts a test that sleeps 500
        // in clock_nanosleep and computes 90, a probe of 1 too thin to
        // draw, and a helper the tree leaves out. A stretch of the test
        // asleep marks the test's sleep frame, one of the runner its
        // computing frame, and one of the probe or the helper the runner's
        // frame, the nearest drawn process each runs under. A process with
        // nothing drawn above it marks nothing.
        let mut test = node(2, 590, 500);
        test.own = vec![
            own(State::Asleep("clock_nanosleep".to_string()), 500),
            own(State::Computing, 90),
        ];
        let mut probe = node(3, 1, 0);
        probe.own = vec![own(State::Computing, 1)];
        let mut runner = node(1, 1000, 500);
        runner.own = vec![own(State::Computing, 409)];
        runner.children = vec![test, probe];
        let frames = icicle(&runner);
        let marked = |lineage: &[u32], sleeping: bool| {
            let lineage: Vec<Life> = lineage.iter().map(|&pid| life(pid)).collect();
            let s = stretch(lineage[0].pid, 0, 1, sleeping);
            playhead_frame(&frames, &runner, &s, &lineage, None).map(|i| frames[i].pick)
        };
        assert_eq!(
            marked(&[2, 1], true),
            Some(Pick::Own {
                life: life(2),
                part: 0
            })
        );
        assert_eq!(
            marked(&[1], false),
            Some(Pick::Own {
                life: life(1),
                part: 0
            })
        );
        assert_eq!(marked(&[3, 1], false), Some(Pick::Process(life(1))));
        assert_eq!(marked(&[4, 1], false), Some(Pick::Process(life(1))));
        assert_eq!(marked(&[9], false), None);
    }

    #[test]
    fn a_picked_process_keeps_the_playhead_mark_for_what_runs_in_it() {
        // A runner starts a test that sleeps 500 in clock_nanosleep. With
        // the test picked, a stretch of the test asleep marks the test's
        // own frame, not its sleep frame under it, so the frame lit is the
        // one clicked. A pick off the stretch's line, or a pick of a part,
        // leaves the mark where it would be with nothing picked.
        let mut test = node(2, 500, 0);
        test.own = vec![own(State::Asleep("clock_nanosleep".to_string()), 500)];
        let mut other = node(3, 100, 0);
        other.own = vec![own(State::Computing, 100)];
        let mut runner = node(1, 1000, 400);
        runner.own = vec![own(State::Computing, 400)];
        runner.children = vec![test, other];
        let frames = icicle(&runner);
        let lineage = [life(2), life(1)];
        let s = stretch(2, 0, 1, true);
        let marked = |picked: Option<Pick>| {
            playhead_frame(&frames, &runner, &s, &lineage, picked).map(|i| frames[i].pick)
        };
        let sleep = Pick::Own {
            life: life(2),
            part: 0,
        };
        assert_eq!(marked(None), Some(sleep));
        assert_eq!(
            marked(Some(Pick::Process(life(2)))),
            Some(Pick::Process(life(2)))
        );
        assert_eq!(
            marked(Some(Pick::Process(life(1)))),
            Some(Pick::Process(life(1)))
        );
        assert_eq!(marked(Some(Pick::Process(life(3)))), Some(sleep));
        assert_eq!(marked(Some(sleep)), Some(sleep));
    }

    /// A run of `pid` from `start_step` to `end_step`.
    fn run(pid: u32, start_step: u64, end_step: u64) -> Run {
        Run {
            tid: pid,
            pid,
            memory: pid != 0,
            start_step,
            end_step,
            start_ns: start_step,
            end_ns: end_step,
            left_runnable: false,
            left_syscall: None,
        }
    }

    #[test]
    fn the_playhead_is_on_the_path_only_where_the_path_computes() {
        // The path computes in pid 1 from step 0 and then sleeps in pid 1
        // from step 10. Runs: pid 1 for steps 0 to 10, pid 2 for 10 to 15,
        // and the idle task after. A step in pid 1's computing is on the
        // path; a step in pid 2's run, while the path sleeps, is beside it
        // in pid 2; a step in the idle task is beside it with the CPU idle.
        let runs = vec![run(1, 0, 10), run(2, 10, 15), run(0, 15, 20)];
        let lives = vec![Some(life(1)), Some(life(2)), None];
        let computing = stretch(1, 0, 10, false);
        let mut asleep = stretch(1, 10, 10, true);
        asleep.step = 10;
        assert_eq!(at_playhead(&runs, &lives, &computing, 3), AtStep::OnPath);
        assert_eq!(
            at_playhead(&runs, &lives, &asleep, 12),
            AtStep::Beside(Runner::Process(life(2)))
        );
        assert_eq!(
            at_playhead(&runs, &lives, &asleep, 17),
            AtStep::Beside(Runner::Idle)
        );
    }
}
