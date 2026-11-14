//! What `rewind time` and the desktop app's Time tab show of a run: its
//! time on one CPU, the least it could take on enough CPUs, that chain by
//! Nix phase and in order, and the rows that would make the job finish
//! sooner, for the whole job or inside one process.
//!
//! Both read the same [`Timing`], so the command and the app say the same
//! thing. Every row carries how long its part took, how much of it is on
//! the chain, and how long the job would still take if the part took no
//! time ([`Row::without_ns`]); only what is on the chain can lower that.

use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use crate::critical::{
    self, Chain, Computing, Graph, Nothing, Piece, PieceKind, Sleeps, Start, Threads, TimeError,
    syscall_name,
};
use crate::io_records::{Io, IoLog};
use crate::{EventKind, Life, Trace};

/// How many processes, programs and sleeps get a what-if each: the ones
/// the chain spends the most in. Each what-if is a pass over every run.
const CANDIDATES: usize = 40;

/// The share of a process's time on the chain its sleeps must be to fold
/// into the process's row.
const FOLD_SHARE: f64 = 0.9;

/// The share of the tree's time a process must hold, with what it
/// started, to have a line of its own in the tree.
pub const TREE_SHARE: f64 = 0.01;

/// How many lines the tree has at most, besides its roots: filled with
/// the processes that hold the most of the path.
const TREE_LINES: usize = 16;

/// The share of a process's time a child must hold for the two to share
/// a line: a wrapper such as `make` running one `bash -c` says nothing.
const WRAPPER_SHARE: f64 = 0.95;

/// How much of the path's tree to build: lines for the processes that
/// hold the most, as `rewind time` prints it, or every process on the
/// path, as a flame graph draws it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Budget {
    Lines,
    Whole,
}

/// What a thread on the critical path was doing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum State {
    Computing,
    /// Asleep on a timer, in this system call.
    Asleep(String),
}

impl State {
    pub fn name(&self) -> String {
        match self {
            State::Computing => "computing".to_string(),
            State::Asleep(call) => format!("asleep in {call}"),
        }
    }
}

/// A process on the critical path, with what it started: the path as a
/// tree of processes, each with its time on the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathNode {
    pub life: Life,
    /// The programs of the wrappers that share its line, outermost
    /// first: processes whose time on the path is nearly all this one's.
    pub through: Vec<String>,
    /// Its command line on one line, without its pid.
    pub label: String,
    /// Time on the path in the process and everything it started, and how
    /// much of that asleep.
    pub total_ns: u64,
    pub asleep_ns: u64,
    /// The first process of the line: `life` itself, or the outermost
    /// wrapper that shares its line.
    pub top: Life,
    /// The process's own time on the path, by state, most first.
    pub own: Vec<OwnPart>,
    /// The children shown, most first.
    pub children: Vec<PathNode>,
    /// The children on the path not shown: how many, and their time.
    pub hidden: usize,
    pub hidden_ns: u64,
}

/// A process's own time on the path in one state, and where to look at
/// it: the biggest piece's thread and step, the step its thread fell
/// asleep at for a sleep, or began computing at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnPart {
    pub state: State,
    pub ns: u64,
    /// How many pieces of the path it is: sleeps, or runs of the CPU.
    pub count: usize,
    pub tid: u32,
    pub step: u64,
    /// The biggest piece's length.
    pub biggest_ns: u64,
}

impl OwnPart {
    /// A part with one piece in it.
    fn of(piece: &OnPath) -> OwnPart {
        OwnPart {
            state: piece.state.clone(),
            ns: piece.ns,
            count: 1,
            tid: piece.tid,
            step: piece.step,
            biggest_ns: piece.ns,
        }
    }

    /// Adds a piece, which becomes the one to look at if it is the
    /// biggest.
    fn add(&mut self, piece: &OnPath) {
        self.ns += piece.ns;
        self.count += 1;
        if piece.ns > self.biggest_ns {
            self.biggest_ns = piece.ns;
            self.tid = piece.tid;
            self.step = piece.step;
        }
    }
}

/// The critical path as a tree of processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathTree {
    /// The time on the path the tree covers.
    pub ns: u64,
    /// The path by state, most first.
    pub states: Vec<(State, u64)>,
    pub roots: Vec<PathNode>,
    /// Time on the path in kernel threads, outside any process.
    pub kernel_ns: u64,
}

/// One piece of the path, for building its tree: whose, doing what, how
/// long, and its thread and the step to look at it.
struct OnPath {
    life: Life,
    state: State,
    ns: u64,
    tid: u32,
    step: u64,
}

/// What to call a process in the tree: its program, and its command line.
struct Naming<'a> {
    program: &'a dyn Fn(Life) -> String,
    shown: &'a dyn Fn(Life) -> String,
}

/// A tree being built: each process's time on the path, and which
/// processes have a line or share their parent's.
struct Shape<'a> {
    children: &'a HashMap<Life, Vec<Life>>,
    naming: &'a Naming<'a>,
    total: HashMap<Life, u64>,
    asleep: HashMap<Life, u64>,
    own: HashMap<Life, BTreeMap<State, OwnPart>>,
    lined: HashSet<Life>,
    /// Each wrapper and the child that shares its line.
    merged: HashMap<Life, Life>,
}

impl Shape<'_> {
    fn on(&self, life: &Life) -> u64 {
        self.total.get(life).copied().unwrap_or(0)
    }

    /// The children of `life` on the path, most first.
    fn kids(&self, life: Life) -> Vec<Life> {
        let mut kids: Vec<Life> = self
            .children
            .get(&life)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .copied()
            .filter(|c| self.on(c) > 0)
            .collect();
        kids.sort_by_key(|l| (std::cmp::Reverse(self.on(l)), *l));
        kids
    }

    /// Gives `life` a line, and its line to each child below it that holds
    /// nearly all of its parent's time; returns the last of them, whose
    /// children are next in line.
    fn line(&mut self, life: Life) -> Life {
        self.lined.insert(life);
        let mut at = life;
        while let Some(&big) = self.kids(at).first() {
            if (self.on(&big) as f64) < self.on(&at) as f64 * WRAPPER_SHARE {
                break;
            }
            self.merged.insert(at, big);
            at = big;
        }
        at
    }

    /// Lines for the processes holding the most of the path, biggest
    /// first: with [`Budget::Lines`], until the tree has [`TREE_LINES`] of
    /// them or the next is under `min_ns`; with [`Budget::Whole`], all.
    fn choose(&mut self, roots: &[Life], min_ns: u64, budget: Budget) {
        let (most, least) = match budget {
            Budget::Lines => (TREE_LINES, min_ns),
            Budget::Whole => (usize::MAX, 0),
        };
        let mut next: BinaryHeap<(u64, std::cmp::Reverse<Life>)> = BinaryHeap::new();
        for &r in roots {
            let bottom = self.line(r);
            for k in self.kids(bottom) {
                next.push((self.on(&k), std::cmp::Reverse(k)));
            }
        }
        let mut lines = 0;
        while let Some((ns, std::cmp::Reverse(life))) = next.pop() {
            if lines == most || ns < least {
                break;
            }
            lines += 1;
            let bottom = self.line(life);
            for k in self.kids(bottom) {
                next.push((self.on(&k), std::cmp::Reverse(k)));
            }
        }
    }

    /// The line `life` begins, with the wrappers it goes through, and the
    /// lines under it.
    fn node(&self, life: Life) -> PathNode {
        let mut through = Vec::new();
        let top = life;
        let mut at = life;
        while let Some(&next) = self.merged.get(&at) {
            through.push((self.naming.program)(at));
            at = next;
        }
        let kids = self.kids(at);
        let (shown, hidden): (Vec<Life>, Vec<Life>) =
            kids.into_iter().partition(|k| self.lined.contains(k));
        let mut own: Vec<OwnPart> = self
            .own
            .get(&at)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        own.sort_by_key(|p| (std::cmp::Reverse(p.ns), p.state.clone()));
        PathNode {
            life: at,
            top,
            through,
            label: one_line(&(self.naming.shown)(at)),
            total_ns: self.on(&life),
            asleep_ns: self.asleep.get(&life).copied().unwrap_or(0),
            own,
            children: shown.iter().map(|&c| self.node(c)).collect(),
            hidden: hidden.len(),
            hidden_ns: hidden.iter().map(|l| self.on(l)).sum(),
        }
    }
}

/// A command line on one line, its runs of whitespace one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The tree of `pieces` under `root`, or under every process without a
/// parent: each process's time on the path, with what it started.
fn build_tree(
    pieces: &[OnPath],
    parents: &HashMap<Life, Life>,
    children: &HashMap<Life, Vec<Life>>,
    naming: &Naming<'_>,
    root: Option<Life>,
    budget: Budget,
) -> Vec<PathNode> {
    let mut total: HashMap<Life, u64> = HashMap::new();
    let mut asleep: HashMap<Life, u64> = HashMap::new();
    let mut own: HashMap<Life, BTreeMap<State, OwnPart>> = HashMap::new();
    for p in pieces {
        own.entry(p.life)
            .or_default()
            .entry(p.state.clone())
            .and_modify(|part| part.add(p))
            .or_insert_with(|| OwnPart::of(p));

        // Each piece counts toward its process and every ancestor.
        let mut at = Some(p.life);
        while let Some(l) = at {
            *total.entry(l).or_default() += p.ns;
            if matches!(p.state, State::Asleep(_)) {
                *asleep.entry(l).or_default() += p.ns;
            }
            at = parents.get(&l).copied();
        }
    }

    let mut roots: Vec<Life> = match root {
        Some(r) => vec![r],
        None => total
            .keys()
            .copied()
            .filter(|l| !parents.contains_key(l))
            .collect(),
    };
    roots.sort_by_key(|l| (std::cmp::Reverse(total.get(l).copied().unwrap_or(0)), *l));
    let whole: u64 = roots
        .iter()
        .map(|l| total.get(l).copied().unwrap_or(0))
        .sum();
    let mut shape = Shape {
        children,
        naming,
        total,
        asleep,
        own,
        lined: HashSet::new(),
        merged: HashMap::new(),
    };
    shape.choose(&roots, (whole as f64 * TREE_SHARE) as u64, budget);
    roots.into_iter().map(|r| shape.node(r)).collect()
}

/// How many ancestors a folded stack names at most.
const FOLD_DEPTH: usize = 32;

/// How long a frame of a folded stack is at most, in characters.
const FRAME_WIDTH: usize = 72;

/// A command line as a frame of a folded stack: on one line, its runs of
/// whitespace one space, no `;`, which separates frames, and no longer
/// than [`FRAME_WIDTH`].
fn frame_name(shown: &str) -> String {
    let clean: String = shown
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(';', ",");
    if clean.chars().count() <= FRAME_WIDTH {
        return clean;
    }
    let mut out: String = clean.chars().take(FRAME_WIDTH - 1).collect();
    out.push('…');
    out
}

/// How many programs and processes a process's started list shows.
const STARTED_SHOWN: usize = 6;

/// What stdenv prints at the start of each phase of a Nix build.
const PHASE_MARKER: &str = "Running phase: ";

/// What the folded stacks call the job's time before its first phase.
pub const BEFORE_PHASES: &str = "before the phases";

/// A run read for timing: its graph, its chain on enough CPUs, and what
/// to call its processes.
#[derive(Debug)]
pub struct Timing {
    pub graph: Graph,
    pub chain: Chain,
    /// The process of each run, by the run's index; None for the idle
    /// task and kernel threads the trace saw no fork of.
    pub lives: Vec<Option<Life>>,
    names: Names,
    costs: HashMap<Life, u64>,
    phases: Vec<(String, u64, u64)>,
    /// Each process's children, by the life that forked them, and each
    /// process's parent.
    children: HashMap<Life, Vec<Life>>,
    parents: HashMap<Life, Life>,
}

/// The job as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The job's time on the VM's one CPU, and how much of it computing.
    pub took_ns: u64,
    pub work_ns: u64,
    /// The least it could take on enough CPUs: its longest chain.
    pub chain_ns: u64,
    /// Computing that is not on the chain.
    pub off_chain_ns: u64,
    pub phases: Vec<Phase>,
    /// The path before the first phase, from the job's start.
    pub before_phases_ns: u64,
    /// What would make the job sooner, soonest first.
    pub rows: Vec<Row>,
}

/// A Nix phase of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: String,
    pub took_ns: u64,
    pub on_chain_ns: u64,
    /// The step the phase began on.
    pub step: u64,
}

/// One process from the inside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inside {
    pub life: Life,
    pub label: String,
    pub took_ns: u64,
    pub on_chain_ns: u64,
    pub without_ns: u64,
    /// Its threads, their sleeps and what they waited for, soonest first.
    pub rows: Vec<Row>,
}

/// One line of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub label: String,
    /// How long the part took on the one CPU: computing, and sleeping on
    /// timers.
    pub took_ns: u64,
    /// How much of the chain is this part.
    pub on_chain_ns: u64,
    /// How long the job would still take, at least, if this part took no
    /// time. None for a wait, which is the time of another part.
    pub without_ns: Option<u64>,
    /// The process to look inside, for a row that is one.
    pub life: Option<Life>,
    /// The step the part is first on the chain, to go to.
    pub step: Option<u64>,
    /// The thread `step` is in, so a look at the step looks at the
    /// thread the row is about and not its process's main thread.
    pub tid: Option<u32>,
}

impl Row {
    /// How much sooner the job would finish without this part.
    pub fn gain_ns(&self, chain_ns: u64) -> u64 {
        self.without_ns.map_or(0, |w| chain_ns.saturating_sub(w))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A process: its threads' computing and sleeps.
    Process,
    /// Every process that ran one program, by the program's name.
    Program { program: String, processes: usize },
    /// A process's sleeps on timers, in one system call.
    Sleeps { count: usize },
    /// One thread's computing.
    Thread { tid: u32 },
    /// One thread's sleeps on timers, in one system call, `call` by name
    /// as a process's own part names it.
    ThreadSleeps {
        tid: u32,
        count: usize,
        call: String,
    },
    /// Every thread's sleeps in a process at once.
    AllSleeps,
    /// A thread on the chain waited for another, which then went on.
    Waits { tid: u32, count: usize },
    /// A process and every process it started.
    Tree { processes: usize },
}

/// A stretch of the chain, in order: one process computing, or asleep in
/// one system call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stretch {
    pub life: Option<Life>,
    pub label: String,
    pub state: State,
    /// Where on the chain it starts, and how long it is.
    pub at_ns: u64,
    pub ns: u64,
    /// The step it starts on: its first run's first step, or for a sleep
    /// the step its thread left the CPU on.
    pub step: u64,
}

impl Timing {
    /// Reads `trace` for timing, with its read records `io`, which name
    /// the source a compiler read, and `end_ns`, the clock when the run
    /// stopped.
    pub fn new(trace: &Trace, io: &IoLog, end_ns: u64) -> Result<Timing, TimeError> {
        let graph = Graph::new(trace, end_ns)?;
        let chain = graph.chain(&Nothing);
        let lives = critical::lives(&graph, trace);
        let names = Names::new(trace, io);
        let mut children: HashMap<Life, Vec<Life>> = HashMap::new();
        let mut parents: HashMap<Life, Life> = HashMap::new();
        for p in trace.processes() {
            if let Some(parent) = p.parent_life {
                children.entry(parent).or_default().push(p.life());
                parents.insert(p.life(), parent);
            }
        }

        // What each process took: its computing and its sleeps on timers.
        let mut costs: HashMap<Life, u64> = HashMap::new();
        for (i, r) in graph.runs.iter().enumerate() {
            let Some(life) = lives[i] else {
                continue;
            };
            let sleep = match graph.starts[i] {
                Start::Timer { ns, .. } => ns,
                _ => 0,
            };
            *costs.entry(life).or_default() += r.ns() + sleep;
        }

        // The Nix phases, each with the clock and step it began at.
        let mut phases = Vec::new();
        for e in &trace.events {
            let EventKind::Output { bytes, .. } = &e.kind else {
                continue;
            };
            for line in String::from_utf8_lossy(bytes).lines() {
                if let Some(phase) = line.strip_prefix(PHASE_MARKER) {
                    phases.push((phase.trim().to_string(), graph.time_at(e.step), e.step));
                }
            }
        }
        Ok(Timing {
            graph,
            chain,
            lives,
            names,
            costs,
            phases,
            children,
            parents,
        })
    }

    /// The job as a whole: its time, its chain by phase, and what would
    /// shorten it most.
    pub fn report(&self) -> Report {
        let (graph, chain, lives) = (&self.graph, &self.chain, &self.lives);
        let (start, end) = (graph.job_start.1, graph.job_end.1);
        let work_ns: u64 = graph
            .runs
            .iter()
            .filter(|r| !r.is_idle() && r.start_ns >= start && r.end_ns <= end)
            .map(|r| r.ns())
            .sum();
        let on_compute: u64 = chain
            .pieces
            .iter()
            .filter(|p| p.kind == PieceKind::Compute)
            .map(|p| p.ns)
            .sum();

        // The chain by phase, each piece split where a phase begins.
        let mut on_phase = vec![0u64; self.phases.len()];
        let mut before_phases_ns = 0;
        for piece in &chain.pieces {
            for (phase, ns) in self.by_phase(piece) {
                match phase {
                    Some(i) => on_phase[i] += ns,
                    None => before_phases_ns += ns,
                }
            }
        }
        let phases = self
            .phases
            .iter()
            .enumerate()
            .map(|(i, (name, from, step))| {
                let to = self.phases.get(i + 1).map_or(end, |(_, t, _)| *t);
                Phase {
                    name: name.clone(),
                    took_ns: to.saturating_sub(*from),
                    on_chain_ns: on_phase[i],
                    step: *step,
                }
            })
            .collect();

        // The processes, programs and sleeps the chain spends the most in.
        let mut on_chain: HashMap<Life, u64> = HashMap::new();
        let mut first_step: HashMap<Life, (u64, u32)> = HashMap::new();
        let mut sleeps: HashMap<(Life, Option<u32>), (u64, usize)> = HashMap::new();
        let mut sleep_step: HashMap<(Life, Option<u32>), (u64, u32)> = HashMap::new();
        for piece in &chain.pieces {
            let Some(life) = lives[piece.run] else {
                continue;
            };
            let run = &graph.runs[piece.run];
            *on_chain.entry(life).or_default() += piece.ns;
            first_step.entry(life).or_insert((run.start_step, run.tid));
            if let PieceKind::Sleep { syscall } = piece.kind {
                let entry = sleeps.entry((life, syscall)).or_default();
                entry.0 += piece.ns;
                entry.1 += 1;
                sleep_step
                    .entry((life, syscall))
                    .or_insert_with(|| (self.fell_asleep(piece.run), run.tid));
            }
        }

        let mut rows: Vec<Row> = Vec::new();
        for (life, ns) in top(&on_chain) {
            rows.push(Row {
                kind: RowKind::Process,
                label: self.names.process(life),
                took_ns: self.took(life),
                on_chain_ns: ns,
                without_ns: Some(graph.finish(&Threads(&|i, _| lives[i] == Some(life)))),
                life: Some(life),
                step: first_step.get(&life).map(|f| f.0),
                tid: first_step.get(&life).map(|f| f.1),
            });
        }
        let mut programs: HashMap<String, (u64, usize)> = HashMap::new();
        for (&life, &ns) in &on_chain {
            let entry = programs.entry(self.names.program(life)).or_default();
            entry.0 += ns;
            entry.1 += 1;
        }
        let groups: HashMap<String, u64> = programs
            .into_iter()
            .filter(|(_, (_, count))| *count > 1)
            .map(|(p, (ns, _))| (p, ns))
            .collect();
        for (program, ns) in top(&groups) {
            let all = self.names.lives_of(&program);
            rows.push(Row {
                kind: RowKind::Program {
                    program: program.clone(),
                    processes: all.len(),
                },
                label: format!("every {program}, {} processes", all.len()),
                took_ns: all.iter().map(|&l| self.took(l)).sum(),
                on_chain_ns: ns,
                without_ns: {
                    let theirs = runs_in(lives, &all.iter().copied().collect());
                    Some(graph.finish(&Threads(&|i, _| theirs[i])))
                },
                life: None,
                step: None,
                tid: None,
            });
        }
        let sleep_ns: HashMap<(Life, Option<u32>), u64> =
            sleeps.iter().map(|(k, (ns, _))| (*k, *ns)).collect();
        for ((life, syscall), ns) in top(&sleep_ns) {
            let count = sleeps[&(life, syscall)].1;
            rows.push(Row {
                kind: RowKind::Sleeps { count },
                label: format!(
                    "{} in {}: {}",
                    plural(count, "sleep", "sleeps"),
                    call_name(syscall),
                    self.names.process(life)
                ),
                took_ns: ns,
                on_chain_ns: ns,
                without_ns: Some(graph.finish(&Sleeps(&|i, _| {
                    lives[i] == Some(life)
                        && matches!(graph.starts[i], Start::Timer { syscall: s, .. } if s == syscall)
                }))),
                life: Some(life),
                step: sleep_step.get(&(life, syscall)).map(|f| f.0),
                tid: sleep_step.get(&(life, syscall)).map(|f| f.1),
            });
        }
        fold_sleeps(&mut rows);
        sort(&mut rows);
        Report {
            took_ns: end.saturating_sub(start),
            work_ns,
            chain_ns: chain.ns,
            off_chain_ns: work_ns.saturating_sub(on_compute),
            phases,
            before_phases_ns,
            rows,
        }
    }

    /// Every process `life` started, and those they started, not itself.
    pub fn descendants(&self, life: Life) -> Vec<Life> {
        let mut out = Vec::new();
        let mut todo = vec![life];
        while let Some(l) = todo.pop() {
            for &child in self.children.get(&l).map_or(&[][..], Vec::as_slice) {
                out.push(child);
                todo.push(child);
            }
        }
        out
    }

    /// What `life` started, from the inside: the whole tree at once, its
    /// programs, and the processes in it the chain spends the most in.
    pub fn started(&self, life: Life) -> Vec<Row> {
        let (graph, chain, lives) = (&self.graph, &self.chain, &self.lives);
        let under = self.descendants(life);
        if under.is_empty() {
            return Vec::new();
        }
        let below: HashSet<Life> = under.iter().copied().collect();
        let mut tree = below.clone();
        tree.insert(life);
        let tree_runs = runs_in(lives, &tree);

        let mut on: HashMap<Life, u64> = HashMap::new();
        let mut first_step: HashMap<Life, (u64, u32)> = HashMap::new();
        for p in &chain.pieces {
            if let Some(l) = lives[p.run]
                && below.contains(&l)
            {
                let run = &graph.runs[p.run];
                *on.entry(l).or_default() += p.ns;
                first_step.entry(l).or_insert((run.start_step, run.tid));
            }
        }

        let mut rows = vec![Row {
            kind: RowKind::Tree {
                processes: under.len(),
            },
            label: format!("it and the {} processes it started", under.len()),
            took_ns: tree.iter().map(|&l| self.took(l)).sum(),
            on_chain_ns: on.values().sum::<u64>()
                + chain
                    .pieces
                    .iter()
                    .filter(|p| lives[p.run] == Some(life))
                    .map(|p| p.ns)
                    .sum::<u64>(),
            without_ns: Some(graph.finish(&Threads(&|i, _| tree_runs[i]))),
            life: None,
            step: None,
            tid: None,
        }];

        // The programs it started, those run more than once as a group.
        let mut programs: HashMap<String, (u64, Vec<Life>)> = HashMap::new();
        for &l in &under {
            let e = programs.entry(self.names.program(l)).or_default();
            e.0 += on.get(&l).copied().unwrap_or(0);
            e.1.push(l);
        }
        let mut groups: Vec<(String, u64, Vec<Life>)> = programs
            .into_iter()
            .filter(|(_, (ns, ls))| *ns > 0 && ls.len() > 1)
            .map(|(p, (ns, ls))| (p, ns, ls))
            .collect();
        groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (program, ns, group) in groups.into_iter().take(STARTED_SHOWN) {
            rows.push(Row {
                kind: RowKind::Program {
                    program: program.clone(),
                    processes: group.len(),
                },
                label: format!("every {program} under it, {} processes", group.len()),
                took_ns: group.iter().map(|&l| self.took(l)).sum(),
                on_chain_ns: ns,
                without_ns: {
                    let theirs = runs_in(lives, &group.iter().copied().collect());
                    Some(graph.finish(&Threads(&|i, _| theirs[i])))
                },
                life: None,
                step: None,
                tid: None,
            });
        }

        // The processes under it the chain spends the most in.
        for (l, ns) in top(&on).into_iter().take(STARTED_SHOWN) {
            rows.push(Row {
                kind: RowKind::Process,
                label: self.names.process(l),
                took_ns: self.took(l),
                on_chain_ns: ns,
                without_ns: Some(graph.finish(&Threads(&|i, _| lives[i] == Some(l)))),
                life: Some(l),
                step: first_step.get(&l).map(|f| f.0),
                tid: first_step.get(&l).map(|f| f.1),
            });
        }
        sort(&mut rows);
        rows
    }

    /// One process from the inside: each thread's computing, its sleeps
    /// and what it waited for on the chain, and every thread's sleeps at
    /// once.
    pub fn inside(&self, life: Life) -> Inside {
        let (graph, chain, lives) = (&self.graph, &self.chain, &self.lives);
        let mine = |i: usize| lives[i] == Some(life);
        let first = |pred: &dyn Fn(usize) -> bool| -> Option<(u64, u32)> {
            chain
                .pieces
                .iter()
                .find(|p| pred(p.run))
                .map(|p| (graph.runs[p.run].start_step, graph.runs[p.run].tid))
        };

        let under: HashSet<Life> = self.descendants(life).into_iter().collect();
        let mut tids: Vec<u32> = (0..graph.runs.len())
            .filter(|&i| mine(i))
            .map(|i| graph.runs[i].tid)
            .collect();
        tids.sort_unstable();
        tids.dedup();
        let mut rows = Vec::new();
        for &tid in &tids {
            let ours = |i: usize| mine(i) && graph.runs[i].tid == tid;

            // A row names its thread only when the process has others.
            let thread = if tids.len() > 1 {
                format!("thread {tid}: ")
            } else {
                String::new()
            };
            rows.push(Row {
                kind: RowKind::Thread { tid },
                label: format!("{thread}computing"),
                took_ns: (0..graph.runs.len())
                    .filter(|&i| ours(i))
                    .map(|i| graph.runs[i].ns())
                    .sum(),
                on_chain_ns: chain
                    .pieces
                    .iter()
                    .filter(|p| ours(p.run) && p.kind == PieceKind::Compute)
                    .map(|p| p.ns)
                    .sum(),
                without_ns: Some(graph.finish(&Computing(&|i, _| ours(i)))),
                life: None,
                step: first(&ours).map(|f| f.0),
                tid: Some(tid),
            });

            // Its sleeps on the chain, by system call.
            let mut by_call: BTreeMap<Option<u32>, (u64, usize, u64)> = BTreeMap::new();
            for p in chain.pieces.iter().filter(|p| ours(p.run)) {
                if let PieceKind::Sleep { syscall } = p.kind {
                    let e = by_call.entry(syscall).or_default();
                    e.0 += p.ns;
                    e.1 += 1;
                    if e.1 == 1 {
                        e.2 = self.fell_asleep(p.run);
                    }
                }
            }
            for (syscall, (ns, count, step)) in by_call {
                rows.push(Row {
                    kind: RowKind::ThreadSleeps {
                        tid,
                        count,
                        call: call_name(syscall).to_string(),
                    },
                    label: format!(
                        "{thread}{} in {}",
                        plural(count, "sleep", "sleeps"),
                        call_name(syscall)
                    ),
                    took_ns: ns,
                    on_chain_ns: ns,
                    without_ns: Some(graph.finish(&Sleeps(&|i, _| {
                        ours(i)
                            && matches!(graph.starts[i], Start::Timer { syscall: s, .. } if s == syscall)
                    }))),
                    life: None,
                    step: Some(step),
                    tid: Some(tid),
                });
            }

            // What it waited for on the chain, by the thread that woke it:
            // another of its threads, or a process outside what it started,
            // whose wait for its own children the started rows already show.
            let mut waits: BTreeMap<WaitKey, (u64, usize, u64)> = BTreeMap::new();
            for hop in chain.hops.iter().filter(|h| ours(h.waiter)) {
                if lives[hop.waker].is_some_and(|l| under.contains(&l)) {
                    continue;
                }
                let waker = graph.runs[hop.waker].tid;
                let e = waits
                    .entry((waker, lives[hop.waker], hop.syscall))
                    .or_default();
                e.0 += hop.blocked_ns;
                e.1 += 1;
                if e.2 == 0 {
                    e.2 = graph.runs[hop.waiter].start_step;
                }
            }
            for ((waker, waker_life, syscall), (ns, count, step)) in waits {
                let whom = match waker_life {
                    Some(l) if l == life => format!("thread {waker}"),
                    Some(l) => self.names.process(l),
                    None => format!("kernel thread {waker}"),
                };
                rows.push(Row {
                    kind: RowKind::Waits { tid, count },
                    label: format!(
                        "{thread}waited in {} for {whom}, {}",
                        call_name(syscall),
                        plural(count, "time", "times")
                    ),
                    took_ns: ns,
                    on_chain_ns: 0,
                    without_ns: None,
                    life: waker_life.filter(|&l| l != life),
                    step: Some(step),
                    tid: Some(tid),
                });
            }
        }

        // Every thread's sleeps at once: threads that sleep alike stand in
        // for one another, so one thread's alone may save little.
        let any_sleep = chain
            .pieces
            .iter()
            .any(|p| mine(p.run) && matches!(p.kind, PieceKind::Sleep { .. }));
        if tids.len() > 1 && any_sleep {
            rows.push(Row {
                kind: RowKind::AllSleeps,
                label: "every thread's sleeps".to_string(),
                took_ns: (0..graph.runs.len())
                    .filter(|&i| mine(i))
                    .map(|i| match graph.starts[i] {
                        Start::Timer { ns, .. } => ns,
                        _ => 0,
                    })
                    .sum(),
                on_chain_ns: chain
                    .pieces
                    .iter()
                    .filter(|p| mine(p.run) && matches!(p.kind, PieceKind::Sleep { .. }))
                    .map(|p| p.ns)
                    .sum(),
                without_ns: Some(graph.finish(&Sleeps(&|i, _| mine(i)))),
                life: None,
                step: first(&mine).map(|f| f.0),
                tid: first(&mine).map(|f| f.1),
            });
        }
        sort(&mut rows);
        Inside {
            life,
            label: self.names.process(life),
            took_ns: self.took(life),
            on_chain_ns: chain
                .pieces
                .iter()
                .filter(|p| mine(p.run))
                .map(|p| p.ns)
                .sum(),
            without_ns: graph.finish(&Threads(&|i, _| mine(i))),
            rows,
        }
    }

    /// The critical path as a tree of processes, under `root` or the
    /// whole of it, and within phase `phase` or all of it: each process's
    /// time on the path with everything it
    /// started, its own time by state, and its children, as many as
    /// `budget` gives lines.
    pub fn path_tree(&self, root: Option<Life>, phase: Option<usize>, budget: Budget) -> PathTree {
        let under: Option<HashSet<Life>> = root.map(|r| {
            let mut all: HashSet<Life> = self.descendants(r).into_iter().collect();
            all.insert(r);
            all
        });
        let mut pieces = Vec::new();
        let mut kernel_ns = 0;
        let mut states: BTreeMap<State, u64> = BTreeMap::new();
        for piece in &self.chain.pieces {
            let run = &self.graph.runs[piece.run];

            // Within a phase, the part of the piece in it.
            let ns = match phase {
                Some(i) => self
                    .by_phase(piece)
                    .into_iter()
                    .filter(|(p, _)| *p == Some(i))
                    .map(|(_, ns)| ns)
                    .sum(),
                None => piece.ns,
            };
            if ns == 0 {
                continue;
            }
            let life = self.lives[piece.run];
            if let Some(under) = &under
                && !life.is_some_and(|l| under.contains(&l))
            {
                continue;
            }

            // A sleep is looked at where its thread fell asleep; computing
            // where its run began.
            // A piece begun before the phase is looked at from its start.
            let (state, step) = match piece.kind {
                PieceKind::Compute => (State::Computing, run.start_step),
                PieceKind::Sleep { syscall } => (
                    State::Asleep(call_name(syscall).to_string()),
                    self.fell_asleep(piece.run),
                ),
            };
            let step = match phase.and_then(|i| self.phases.get(i)) {
                Some((_, _, begins)) => step.max(*begins),
                None => step,
            };
            *states.entry(state.clone()).or_default() += ns;
            let Some(life) = life else {
                kernel_ns += ns;
                continue;
            };
            pieces.push(OnPath {
                life,
                state,
                ns,
                tid: run.tid,
                step,
            });
        }
        let mut states: Vec<(State, u64)> = states.into_iter().collect();
        states.sort_by_key(|(s, ns)| (std::cmp::Reverse(*ns), s.clone()));
        let program = |l: Life| self.names.program(l);
        let shown = |l: Life| self.names.shown(l);
        let naming = Naming {
            program: &program,
            shown: &shown,
        };
        PathTree {
            ns: states.iter().map(|(_, ns)| ns).sum(),
            states,
            roots: build_tree(
                &pieces,
                &self.parents,
                &self.children,
                &naming,
                root,
                budget,
            ),
            kernel_ns,
        }
    }

    /// The chain as folded stacks, the lines flamegraph.pl and speedscope
    /// read: the Nix phase, the process's ancestors down to it, then what
    /// it did, computing or asleep in a call; and the nanoseconds of the
    /// chain on that stack. Zooming into a frame opens that process.
    pub fn folded(&self) -> Vec<(String, u64)> {
        let mut totals: BTreeMap<String, u64> = BTreeMap::new();
        for piece in &self.chain.pieces {
            let mut frames = vec![String::new()];
            match self.lives[piece.run] {
                Some(life) => {
                    let mut line = vec![life];
                    let mut at = life;
                    while let Some(&parent) = self.parents.get(&at) {
                        if line.len() > FOLD_DEPTH {
                            break;
                        }
                        line.push(parent);
                        at = parent;
                    }
                    for &l in line.iter().rev() {
                        frames.push(frame_name(&self.names.shown(l)));
                    }
                }
                None => frames.push("kernel".to_string()),
            }
            frames.push(match piece.kind {
                PieceKind::Compute => "computing".to_string(),
                PieceKind::Sleep { syscall } => format!("asleep in {}", call_name(syscall)),
            });

            // The piece under each phase it is in, its first frame.
            for (phase, ns) in self.by_phase(piece) {
                frames[0] = match phase {
                    Some(i) => self.phases[i].0.clone(),
                    None => BEFORE_PHASES.to_string(),
                };
                *totals.entry(frames.join(";")).or_default() += ns;
            }
        }
        totals.into_iter().collect()
    }

    /// The chain in order, one stretch per process computing or asleep
    /// in one system call.
    pub fn stretches(&self) -> Vec<Stretch> {
        let pieces = &self.chain.pieces;
        let state_of = |kind: PieceKind| match kind {
            PieceKind::Compute => State::Computing,
            PieceKind::Sleep { syscall } => State::Asleep(call_name(syscall).to_string()),
        };
        let mut out = Vec::new();
        let mut at = 0u64;
        let mut i = 0;
        while i < pieces.len() {
            let life = self.lives[pieces[i].run];
            let state = state_of(pieces[i].kind);
            let mut ns = 0;
            let mut j = i;
            while j < pieces.len()
                && self.lives[pieces[j].run] == life
                && state_of(pieces[j].kind) == state
            {
                ns += pieces[j].ns;
                j += 1;
            }
            let who = life.map_or_else(|| "the kernel".to_string(), |l| self.names.process(l));
            let label = match &state {
                State::Asleep(call) => format!("sleeps in {call}: {who}"),
                State::Computing => who,
            };
            out.push(Stretch {
                life,
                label,
                state,
                at_ns: at,
                ns,
                step: self.graph.piece_step(&pieces[i]),
            });
            at += ns;
            i = j;
        }
        out
    }

    /// `life` and then each process up from it to the first: its parent,
    /// its parent's parent, and so on.
    pub fn lineage(&self, life: Life) -> Vec<Life> {
        let mut out = vec![life];
        let mut at = life;
        while let Some(&parent) = self.parents.get(&at) {
            out.push(parent);
            at = parent;
        }
        out
    }

    /// The process `which` names: a pid, a program's name, or a word of a
    /// command line, such as the test a runner was given; of several, the
    /// one the chain spends the most in.
    pub fn pick(&self, which: &str) -> Option<Life> {
        let matches = |l: &Life| -> bool {
            match which.parse::<u32>() {
                Ok(pid) => l.pid == pid,
                Err(_) => self.names.program(*l) == which || self.names.has_word(*l, which),
            }
        };
        let mut on: HashMap<Life, u64> = HashMap::new();
        for p in &self.chain.pieces {
            if let Some(l) = self.lives[p.run] {
                *on.entry(l).or_default() += p.ns;
            }
        }
        if let Some((&life, _)) = on
            .iter()
            .filter(|(l, _)| matches(l))
            .max_by_key(|(l, ns)| (**ns, std::cmp::Reverse(**l)))
        {
            return Some(life);
        }
        let mut all: Vec<Life> = self
            .names
            .all
            .keys()
            .copied()
            .filter(|l| matches(l))
            .collect();
        all.sort();
        all.first().copied()
    }

    /// The step a sleep before run `run` began on: where its thread left
    /// the CPU.
    /// How much of `piece` is in each phase, by where it lies on the
    /// clock as recorded: None for the part before the first phase. A run
    /// can go on past a phase's marker, and a sleep can span one.
    fn by_phase(&self, piece: &Piece) -> Vec<(Option<usize>, u64)> {
        let (from, to) = (piece.from_ns, piece.from_ns + piece.ns);
        let overlap = |a: u64, b: u64| to.min(b).saturating_sub(from.max(a));
        let mut out = Vec::new();
        let first = self.phases.first().map_or(u64::MAX, |p| p.1);
        let before = overlap(0, first);
        if before > 0 {
            out.push((None, before));
        }
        for (i, (_, begins, _)) in self.phases.iter().enumerate() {
            let ends = self.phases.get(i + 1).map_or(u64::MAX, |p| p.1);
            let ns = overlap(*begins, ends);
            if ns > 0 {
                out.push((Some(i), ns));
            }
        }
        out
    }

    fn fell_asleep(&self, run: usize) -> u64 {
        self.graph
            .before(run)
            .map_or(self.graph.runs[run].start_step, |b| {
                self.graph.runs[b].end_step
            })
    }

    /// What to call a process.
    pub fn name(&self, life: Life) -> String {
        self.names.process(life)
    }

    /// The program a process ran: the file name of what it executed, or
    /// the script an interpreter ran.
    pub fn program(&self, life: Life) -> String {
        self.names.program(life)
    }

    /// Every process that ran `program`.
    pub fn lives_of(&self, program: &str) -> Vec<Life> {
        self.names.lives_of(program)
    }

    fn took(&self, life: Life) -> u64 {
        self.costs.get(&life).copied().unwrap_or(0)
    }
}

/// `part` of `whole` as a percentage to show: `<1%` for a part too small
/// to round to one, and `>99%` for a part short of the whole that would
/// round to all of it, so a part that is there never reads as none and
/// one that is not all never reads as all.
pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".to_string();
    }
    let p = (part as f64 * 100.0 / whole as f64).round() as u64;
    if p == 0 && part > 0 {
        return "<1%".to_string();
    }
    if p == 100 && part < whole {
        return ">99%".to_string();
    }
    format!("{p}%")
}

/// Which runs are of the processes in `group`, by run, so a what-if
/// asks a list once per run and not the group: a group can be hundreds
/// of processes and the graph tens of thousands of runs.
fn runs_in(lives: &[Option<Life>], group: &HashSet<Life>) -> Vec<bool> {
    lives
        .iter()
        .map(|l| l.is_some_and(|l| group.contains(&l)))
        .collect()
}

/// What a process waited for on the chain: the waker's thread id and
/// process, and the system call the process blocked in.
type WaitKey = (u32, Option<Life>, Option<u32>);

/// A process's sleeps that are nearly all of its time on the chain say
/// the same thing as the process's own row: they become that row's
/// reason, and their row goes.
fn fold_sleeps(rows: &mut Vec<Row>) {
    let mut reasons: HashMap<Life, String> = HashMap::new();
    for r in rows.iter() {
        let (RowKind::Sleeps { .. }, Some(life)) = (&r.kind, r.life) else {
            continue;
        };
        let whole = rows
            .iter()
            .find(|p| p.kind == RowKind::Process && p.life == Some(life))
            .map_or(0, |p| p.on_chain_ns);
        if whole > 0 && r.on_chain_ns as f64 >= whole as f64 * FOLD_SHARE {
            let reason = r.label.split(':').next().unwrap_or_default().to_string();
            reasons.insert(life, reason);
        }
    }
    rows.retain(|r| {
        !(matches!(r.kind, RowKind::Sleeps { .. })
            && r.life.is_some_and(|l| reasons.contains_key(&l)))
    });
    for r in rows.iter_mut() {
        if r.kind != RowKind::Process {
            continue;
        }
        if let Some(reason) = r.life.and_then(|l| reasons.get(&l)) {
            r.label = format!("{reason}: {}", r.label);
        }
    }
}

/// Rows soonest first: those whose part, taking no time, leaves the job
/// shortest; waits, which have no what-if, after them by how long.
fn sort(rows: &mut [Row]) {
    rows.sort_by_key(|r| {
        (
            r.without_ns.unwrap_or(u64::MAX),
            std::cmp::Reverse(r.on_chain_ns),
            std::cmp::Reverse(r.took_ns),
        )
    });
}

/// The keys of `on` the chain spends the most in, most first, at most
/// [`CANDIDATES`] of them.
fn top<K: Clone + Ord>(on: &HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut all: Vec<(K, u64)> = on.iter().map(|(k, v)| (k.clone(), *v)).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.truncate(CANDIDATES);
    all
}

fn call_name(syscall: Option<u32>) -> &'static str {
    syscall.and_then(syscall_name).unwrap_or("a timer")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What to call processes: their program, and their command line with
/// store paths shortened to their names.
#[derive(Debug)]
struct Names {
    all: HashMap<Life, (String, String)>,
}

/// Programs whose most telling argument is the source file they read.
const COMPILERS: &[&str] = &[
    "cc1", "cc1plus", "cc1obj", "as", "rustc", "clang", "clang++",
];

/// File endings of sources a compiler reads.
const SOURCE_ENDINGS: &[&str] = &[
    ".c", ".cc", ".cpp", ".cxx", ".C", ".s", ".S", ".rs", ".m", ".mm", ".i", ".ii",
];

impl Names {
    fn new(trace: &Trace, io: &IoLog) -> Names {
        let procs = trace.processes();

        // The first source file each process opened for reading: a long
        // command line is cut short in the trace, and a compiler's input
        // is often past the cut.
        let mut opened: HashMap<Life, String> = HashMap::new();
        let by_pid = crate::ByPid::new(&procs);
        for r in &io.records {
            let Io::ReadOpen { tid, path } = r.io else {
                continue;
            };
            let path = io.path(path);
            if !SOURCE_ENDINGS.iter().any(|e| path.ends_with(e)) {
                continue;
            }
            if let Some(p) = by_pid.at(tid, r.step) {
                opened.entry(p.life()).or_insert_with(|| path.to_string());
            }
        }

        let mut all = HashMap::new();
        for p in &procs {
            let program = p.program_name().unwrap_or_else(|| format!("pid {}", p.pid));
            let source = opened.get(&p.life()).cloned().or_else(|| {
                p.argv
                    .iter()
                    .skip(1)
                    .find(|w| !w.starts_with('-') && SOURCE_ENDINGS.iter().any(|e| w.ends_with(e)))
                    .cloned()
            });
            let shown = match source {
                Some(source) if COMPILERS.contains(&program.as_str()) => {
                    format!("{program} {}", short_path(&source))
                }
                _ if p.argv.len() <= 1 => program.clone(),
                _ => {
                    let words: Vec<String> = p.argv.iter().skip(1).map(|w| short_path(w)).collect();
                    format!("{program} {}", words.join(" "))
                }
            };
            all.insert(p.life(), (program, shown));
        }
        Names { all }
    }

    fn program(&self, life: Life) -> String {
        self.all
            .get(&life)
            .map_or_else(|| format!("pid {}", life.pid), |(p, _)| p.clone())
    }

    fn process(&self, life: Life) -> String {
        let argv = self
            .all
            .get(&life)
            .map_or_else(|| format!("pid {}", life.pid), |(_, a)| a.clone());
        format!("{argv} (pid {})", life.pid)
    }

    /// Whether a word of the process's command line is `word`, or a path
    /// whose last part is: `configure` names `./configure`.
    /// A process's command line as shown, without its pid.
    fn shown(&self, life: Life) -> String {
        self.all
            .get(&life)
            .map_or_else(|| format!("pid {}", life.pid), |(_, a)| a.clone())
    }

    fn has_word(&self, life: Life, word: &str) -> bool {
        self.all.get(&life).is_some_and(|(_, shown)| {
            shown
                .split_whitespace()
                .any(|w| w == word || w.rsplit('/').next() == Some(word))
        })
    }

    fn lives_of(&self, program: &str) -> Vec<Life> {
        self.all
            .iter()
            .filter(|(_, (p, _))| p == program)
            .map(|(l, _)| *l)
            .collect()
    }
}

/// The store directory and the length of a store path's hash.
const STORE: &str = "/nix/store/";
const HASH_LEN: usize = 32;

/// A word with each store path in it shortened to what follows its hash:
/// `/nix/store/<hash>-bash-5.3/bin/bash` reads `bash-5.3/bin/bash`.
pub fn short_path(word: &str) -> String {
    let mut out = String::new();
    let mut rest = word;
    while let Some(at) = rest.find(STORE) {
        out.push_str(&rest[..at]);
        let after = &rest[at + STORE.len()..];
        let hashed = after.len() > HASH_LEN && after.as_bytes()[HASH_LEN] == b'-';
        rest = if hashed {
            &after[HASH_LEN + 1..]
        } else {
            out.push_str(STORE);
            after
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    // Naming: store paths in a command line read as the names after their
    // hashes, so a row fits a terminal or a pane.
    use super::*;

    #[test]
    fn a_store_path_is_shortened_to_its_name() {
        assert_eq!(
            short_path("/nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-bash-5.3p15/bin/bash"),
            "bash-5.3p15/bin/bash"
        );
        assert_eq!(
            short_path("-I/nix/store/10dxp0qxqxxsyiljrh2kp0xqhz6arhcx-zlib/include"),
            "-Izlib/include"
        );
        assert_eq!(short_path("/nix/store/short"), "/nix/store/short");
    }

    /// A process `pid` in a test's process tree.
    fn life(pid: u32) -> Life {
        Life { pid, start: 0 }
    }

    #[test]
    fn a_groups_runs_are_those_of_its_processes() {
        // Runs of processes 1, 2 and 3 and an idle run: for the group of 1
        // and 3, the runs of 1 and 3 are in it, and 2's and the idle run
        // are not.
        let lives = [
            Some(life(1)),
            Some(life(2)),
            None,
            Some(life(3)),
            Some(life(1)),
        ];
        let group: HashSet<Life> = [life(1), life(3)].into_iter().collect();
        assert_eq!(
            runs_in(&lives, &group),
            vec![true, false, false, true, true]
        );
    }

    #[test]
    fn a_share_never_rounds_a_part_to_none_or_to_all() {
        // A tenth of a percent reads as under one, 99.86% as over 99, and
        // none, all and a plain half as they are.
        assert_eq!(share(1, 1_000), "<1%");
        assert_eq!(share(9_986, 10_000), ">99%");
        assert_eq!(share(0, 10), "0%");
        assert_eq!(share(10, 10), "100%");
        assert_eq!(share(5, 10), "50%");
        assert_eq!(share(5, 0), "0%");
    }

    /// Init's line of output at `step`, as a phase's marker is.
    fn says(step: u64, line: &str) -> crate::Event {
        critical::tests::event(
            step,
            1,
            1,
            EventKind::Output {
                fd: 1,
                bytes: format!("{line}\n").into_bytes(),
            },
        )
    }

    /// The path in each phase, by the report, by the phase's own tree and
    /// by the folded stacks, the time before the phases first.
    fn by_phase(timing: &Timing) -> Vec<(u64, u64, u64)> {
        let report = timing.report();
        let folded = timing.folded();
        let in_folded = |name: &str| -> u64 {
            folded
                .iter()
                .filter(|(stack, _)| stack.split(';').next() == Some(name))
                .map(|(_, ns)| ns)
                .sum()
        };
        let mut out = vec![(
            report.before_phases_ns,
            report.before_phases_ns,
            in_folded(BEFORE_PHASES),
        )];
        for (i, p) in report.phases.iter().enumerate() {
            let tree = timing.path_tree(None, Some(i), Budget::Whole);
            out.push((p.on_chain_ns, tree.ns, in_folded(&p.name)));
        }
        out
    }

    #[test]
    fn a_run_on_the_cpu_through_two_phases_counts_in_each() {
        // Init computes from step 0 to 100, the clock one past the step;
        // its job runs from step 1 to 90, and it prints buildPhase at 10
        // and checkPhase at 50. One run spans both phases: 9 ns are before
        // them and 40 in each, by the report, the trees and the stacks.
        let c = |step, pid, tid, left| critical::tests::switch(step, pid, tid, left);
        let trace = Trace {
            events: vec![
                critical::tests::job_mark(1, "rewind-start"),
                says(10, "Running phase: buildPhase"),
                says(50, "Running phase: checkPhase"),
                critical::tests::job_mark(90, "rewind-exit 0"),
            ],
            switches: vec![c(0, 1, 1, None), c(100, 0, 0, Some(60))],
            wakes: vec![],
        };
        let timing = Timing::new(&trace, &IoLog::default(), 101).unwrap();
        assert_eq!(
            by_phase(&timing),
            vec![(9, 9, 9), (40, 40, 40), (40, 40, 40)]
        );
    }

    #[test]
    fn a_sleep_across_a_phases_start_counts_in_each() {
        // Init computes from step 0 to 5 and, after process 2 runs to
        // 10, from 10 to 20; it sleeps in clock_nanosleep until a timer at
        // 50 and computes to 60. Its job runs from step 1 to 59, the clock
        // one past the step. Process 2, off the path, prints buildPhase at
        // the end of its first run and checkPhase at 25, while init
        // sleeps. The path is 4 before the phases, 10 computing and 5
        // asleep in buildPhase, and 25 asleep and 9 computing in
        // checkPhase.
        let c = |step, pid, tid, left| critical::tests::switch(step, pid, tid, left);
        let mut build = says(10, "Running phase: buildPhase");
        build.pid = 2;
        build.tid = 2;
        let mut check = says(25, "Running phase: checkPhase");
        check.pid = 2;
        check.tid = 2;
        let trace = Trace {
            events: vec![
                critical::tests::job_mark(1, "rewind-start"),
                build,
                check,
                critical::tests::job_mark(59, "rewind-exit 0"),
            ],
            switches: vec![
                c(0, 1, 1, None),
                c(5, 2, 2, None),
                c(10, 1, 1, None),
                c(20, 2, 2, Some(230)),
                c(30, 0, 0, Some(230)),
                c(50, 1, 1, Some(60)),
                c(60, 0, 0, Some(60)),
            ],
            wakes: vec![crate::switches::Wake {
                step: 50,
                ns: 51,
                tid: 1,
                waker: 0,
                how: crate::switches::WakeHow::Hardirq,
            }],
        };
        let timing = Timing::new(&trace, &IoLog::default(), 61).unwrap();
        assert_eq!(timing.chain.ns, 53);
        assert_eq!(
            by_phase(&timing),
            vec![(4, 4, 4), (15, 15, 15), (34, 34, 34)]
        );
    }

    #[test]
    fn a_threads_row_looks_at_that_thread() {
        // Init's main thread joins two workers; the worker that computes
        // 100 steps is on the path. Its computing row is at the step its
        // run starts and names thread 2, so a look at the step looks at
        // the worker and not at main, blocked in its join there.
        let trace = critical::tests::joined_after_exit();
        let timing = Timing::new(&trace, &IoLog::default(), 121).unwrap();
        let life = Life { pid: 1, start: 0 };
        let inside = timing.inside(life);
        let worker = inside
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Thread { tid: 2 })
            .unwrap();
        assert_eq!((worker.step, worker.tid), (Some(5), Some(2)));
    }

    /// A piece of the path on thread 1 at `step`.
    fn piece(life: Life, state: State, ns: u64, step: u64) -> OnPath {
        OnPath {
            life,
            state,
            ns,
            tid: 1,
            step,
        }
    }

    // The path's tree: a shell runs a wrapper, which runs a test runner
    // that sleeps, and a tiny step. The wrapper holds nothing of its own
    // and shares the runner's line; each line counts what its process
    // started; a child too small for a line is counted, not shown; and a
    // process's own time is split by state.
    #[test]
    fn the_tree_counts_what_each_process_started() {
        let (shell, wrapper, runner, test, tiny) = (life(1), life(2), life(3), life(4), life(5));
        let parents = HashMap::from([
            (wrapper, shell),
            (runner, wrapper),
            (test, runner),
            (tiny, shell),
        ]);
        let children = HashMap::from([
            (shell, vec![wrapper, tiny]),
            (wrapper, vec![runner]),
            (runner, vec![test]),
        ]);
        let sleep = || State::Asleep("clock_nanosleep".to_string());
        let pieces = [
            piece(shell, State::Computing, 100, 10),
            piece(runner, State::Computing, 200, 20),
            piece(test, sleep(), 400, 30),
            piece(test, sleep(), 200, 40),
            piece(test, State::Computing, 99, 50),
            piece(tiny, State::Computing, 1, 60),
        ];
        let program = |l: Life| format!("p{}", l.pid);
        let shown = |l: Life| format!("p{}  --flag\n  x", l.pid);
        let naming = Naming {
            program: &program,
            shown: &shown,
        };

        let roots = build_tree(&pieces, &parents, &children, &naming, None, Budget::Lines);
        assert_eq!(roots.len(), 1);
        let top = &roots[0];
        assert_eq!((top.total_ns, top.asleep_ns), (1000, 600));
        assert_eq!((top.hidden, top.hidden_ns), (1, 1));
        let runner = &top.children[0];
        assert_eq!(runner.through, vec!["p2".to_string()]);
        assert_eq!(runner.label, "p3 --flag x");
        assert_eq!(runner.total_ns, 899);
        let own: Vec<(State, u64, usize, u64)> = runner.children[0]
            .own
            .iter()
            .map(|p| (p.state.clone(), p.ns, p.count, p.step))
            .collect();
        assert_eq!(
            own,
            vec![(sleep(), 600, 2, 30), (State::Computing, 99, 1, 50)]
        );
        assert_eq!(runner.top, life(2));

        // The whole tree has a line for every process on the path.
        let whole = build_tree(&pieces, &parents, &children, &naming, None, Budget::Whole);
        assert_eq!((whole[0].hidden, whole[0].children.len()), (0, 2));

        // Under one process, the tree is that process's.
        let under = build_tree(
            &pieces,
            &parents,
            &children,
            &naming,
            Some(life(3)),
            Budget::Lines,
        );
        assert_eq!(under[0].life, life(3));
        assert_eq!(under[0].children[0].total_ns, 699);
    }
}
