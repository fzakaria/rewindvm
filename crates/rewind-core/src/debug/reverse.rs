//! Running a fork backwards for gdb: `reverse-stepi` and `reverse-continue`,
//! which gdb builds `reverse-next`, `reverse-finish` and the rest on.
//!
//! A run is deterministic, so going back is going forward again from
//! earlier and stopping sooner. Where the fork is, its position, is the
//! start of a step and what the fork did since: single steps, runs to the
//! next hit of a breakpoint or watchpoint in the debugged process, and runs
//! to a branch count. Replaying those from the step's start comes back to
//! exactly the same instruction, with no guessing from registers.
//!
//! A step's start is reached from a checkpoint the fork kept in memory,
//! rewound in place, which costs milliseconds, else from a fork of the run
//! at the step, built from its keyframes. One instruction back is the
//! instruction before the position: replayed from where the position's
//! last move began, run fast to one branch short of the position under
//! counter time, then single-stepped until the step that reaches it. Under
//! exit time, which counts no branches, the whole way is single-stepped.
//! Running back to a trap replays the run from keyframe to keyframe,
//! newest first, and stops at the last hit before the position: at a
//! breakpoint as it is reached, and before the instruction that wrote a
//! watched value, as gdb expects of a watchpoint run backwards.
//!
//! Under exit time a step can be millions of instructions, each a VM exit
//! to single-step, so going back can take minutes. Every so many single
//! steps, hits of gdb's traps, and chunks of whole steps, the fork looks
//! at gdb's connection: a Ctrl-C stops it where it has got to, a position like any
//! other, which gdb can inspect and run from; a closed connection ends
//! the session.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use rewind_vmm::Machine;
use rewind_vmm::checkpoint::CheckpointId;
use rewind_vmm::debug::{DebugStop, Stepping, Trap};
use rewind_vmm::snapshot::Pages;
use rewind_vmm::{Ignore, Outcome};

use super::{Debuggee, Follow, Watch};
use crate::run::{Keep, Run};
use crate::{Home, keyframes};

/// The run a fork came from, which running the fork backwards replays.
pub trait History: std::fmt::Debug {
    /// A machine at the start of step `step` of the run.
    fn machine_at(&self, step: u64) -> Result<Machine>;
    /// The records the run made after step `step`.
    fn records_after(&self, step: u64) -> Result<Vec<(u64, Vec<u8>)>>;
    /// The steps of the run's keyframes, in order.
    fn keyframes(&self) -> Result<Vec<u64>>;
    /// The page store keyframes read their pages from.
    fn store(&self) -> Result<rewind_store::Store>;
}

/// A run in a home, as a fork's history.
#[derive(Debug)]
pub struct RunHistory<'a> {
    pub home: &'a Home,
    pub run: &'a Run,
}

impl History for RunHistory<'_> {
    fn machine_at(&self, step: u64) -> Result<Machine> {
        self.run
            .machine_at(self.home, step, Keep::Keyframe, &mut Ignore)
    }

    fn records_after(&self, step: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        self.run.records_after(step)
    }

    fn keyframes(&self) -> Result<Vec<u64>> {
        Ok(self.run.keyframes()?.steps())
    }

    fn store(&self) -> Result<rewind_store::Store> {
        rewind_store::Store::open(&self.home.store())
    }
}

/// A page store's pages, for rewinding a machine restored from keyframes.
pub(crate) struct StorePages(pub(crate) rewind_store::Store);

impl Pages for StorePages {
    fn put(&mut self, _page: &[u8]) -> Result<[u8; 32]> {
        bail!("rewinding stores no pages")
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        keyframes::ReadPages(&self.0).get(hash, out)
    }
}

/// Where a fork is in its run: the start of a step, then what it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) step: u64,
    pub(super) moves: Moves,
}

impl Position {
    /// The start of step `step`.
    pub(super) fn start(step: u64) -> Position {
        Position {
            step,
            moves: Moves::default(),
        }
    }

    /// This position with `mv` after it: single steps after single steps
    /// add up.
    fn then(&self, mv: Move) -> Position {
        Position {
            step: self.step,
            moves: self.moves.then(mv),
        }
    }
}

/// A position's moves: a list each position shares with the one it was
/// made from, newest first. A search back over a step with many hits
/// makes a position per hit, each one move past the last, so adding a
/// move or copying a position takes the same time however many moves
/// there are; a copy per hit of a list per position took time growing as
/// the square of the hits.
#[derive(Clone, Default)]
pub(super) struct Moves(Option<std::sync::Arc<Link>>);

/// One move of a list, the moves before it, and how many moves the list
/// holds with it.
struct Link {
    mv: Move,
    before: Moves,
    len: usize,
}

impl Moves {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |link| link.len)
    }

    /// The last move and the moves before it.
    fn split_last(&self) -> Option<(Move, Moves)> {
        self.0.as_ref().map(|link| (link.mv, link.before.clone()))
    }

    /// These moves with `mv` after them: single steps after single steps
    /// add up.
    fn then(&self, mv: Move) -> Moves {
        if let (Some((Move::Steps(n), before)), Move::Steps(more)) = (self.split_last(), mv) {
            return before.then(Move::Steps(n + more));
        }
        Moves(Some(std::sync::Arc::new(Link {
            mv,
            before: self.clone(),
            len: self.len() + 1,
        })))
    }

    /// Each move, oldest first, with the moves before it.
    fn in_order(&self) -> Vec<(Moves, Move)> {
        let mut out = Vec::with_capacity(self.len());
        let mut at = self.clone();
        while let Some((mv, before)) = at.split_last() {
            out.push((before.clone(), mv));
            at = before;
        }
        out.reverse();
        out
    }
}

impl PartialEq for Moves {
    fn eq(&self, other: &Moves) -> bool {
        let (mut a, mut b) = (self.clone(), other.clone());
        if a.len() != b.len() {
            return false;
        }
        loop {
            match (&a.0, &b.0) {
                (None, None) => return true,
                (Some(x), Some(y)) if std::sync::Arc::ptr_eq(x, y) => return true,
                (Some(x), Some(y)) if x.mv == y.mv => {
                    let (next_a, next_b) = (x.before.clone(), y.before.clone());
                    (a, b) = (next_a, next_b);
                }
                _ => return false,
            }
        }
    }
}

impl Eq for Moves {}

impl std::fmt::Debug for Moves {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.in_order().into_iter().map(|(_, mv)| mv))
            .finish()
    }
}

impl Drop for Moves {
    /// Drops the links no other list shares one at a time: dropping a
    /// list of thousands of moves link by link from the newest would
    /// recurse that deep.
    fn drop(&mut self) {
        let mut next = self.0.take();
        while let Some(link) = next {
            next = match std::sync::Arc::try_unwrap(link) {
                Ok(mut link) => link.before.0.take(),
                Err(_) => None,
            };
        }
    }
}

/// One thing a fork did from a step's start on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Move {
    /// This many single steps.
    Steps(u64),
    /// A run to the next hit of a trap in the debugged process.
    Hit(GdbTrap),
    /// A run to the first instruction boundary at this branch count.
    Branches(u64),
}

/// A trap gdb set, as a position's move names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GdbTrap {
    Breakpoint(u64),
    Watch(Watch),
}

impl GdbTrap {
    /// The debug registers the trap takes.
    fn registers(self) -> Vec<Trap> {
        match self {
            GdbTrap::Breakpoint(address) => vec![Trap::Execute(address)],
            GdbTrap::Watch(watch) => super::watch_registers(&watch),
        }
    }
}

/// How a step toward a position shows it has reached it.
#[derive(Clone, Copy, Debug)]
enum Arrival {
    /// The step's instruction wrote what the watch watches.
    Watch(Watch),
    /// The step lands at the breakpoint's address in the debugged process.
    Breakpoint(u64),
    /// The step lands at this branch count.
    Branches(u64),
    /// The step's instruction made the exit that starts this step.
    StepStart(u64),
}

/// How many branches short of a branch count the counter's interrupt is
/// asked for: the first try, then wider ones for a counter whose interrupt
/// came later than that. The counter's skid on a Zen 4 laptop was 9 to 73
/// branches, with one outlier of 1269 (see rewind-vmm's PREEMPT_MARGIN).
const MARGINS: [u64; 3] = [512, 8192, 1 << 17];

/// How many single steps a replay takes between looks at gdb's
/// connection: a single step is a VM exit of a few microseconds, so this
/// many take some milliseconds.
const LISTEN_EVERY: u64 = 1024;

/// How many hits of gdb's traps in the debugged process a replay sees
/// between looks at gdb's connection: each is a VM exit.
const LISTEN_EVERY_HITS: u64 = 64;

/// How many whole steps a replay runs between looks at gdb's connection.
const LISTEN_EVERY_STEPS: u64 = super::CHUNK_STEPS;

/// The byte gdb sends for Ctrl-C.
const INTERRUPT: u8 = 0x03;

/// What gdb has sent while the fork runs backwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Heard {
    Nothing,
    /// Ctrl-C, not yet read.
    Interrupt,
    /// The connection closed.
    Gone,
}

/// gdb's connection, looked at but not read while the fork runs
/// backwards: a copy of it, whose peeks leave the bytes for gdbstub.
#[derive(Debug, Default)]
pub(super) struct Listener(Option<std::net::TcpStream>);

impl Listener {
    /// A listener on `conn`, gdb's connection, or on none when it cannot
    /// be copied.
    pub(super) fn on(conn: &std::net::TcpStream) -> Listener {
        Listener(conn.try_clone().ok())
    }

    /// What gdb has sent, without reading it. Only Ctrl-C is something
    /// to stop for: gdb sends nothing else while the fork runs.
    pub(super) fn heard(&self) -> Heard {
        let Some(conn) = &self.0 else {
            return Heard::Nothing;
        };
        if conn.set_nonblocking(true).is_err() {
            return Heard::Gone;
        }
        let mut byte = [0u8];
        let peeked = conn.peek(&mut byte);
        let _ = conn.set_nonblocking(false);
        match peeked {
            Ok(0) => Heard::Gone,
            Ok(_) if byte[0] == INTERRUPT => Heard::Interrupt,
            Ok(_) => Heard::Nothing,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Heard::Nothing,
            Err(_) => Heard::Gone,
        }
    }
}

/// gdb's Ctrl-C, or its connection closing, while the fork ran
/// backwards; the fork's position is where it stopped.
#[derive(Debug)]
pub(super) struct Interrupted(pub(super) Heard);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gdb interrupted running backwards")
    }
}

impl std::error::Error for Interrupted {}

/// A counter interrupt that came too late to stop at a branch count.
#[derive(Debug)]
struct Passed;

impl std::fmt::Display for Passed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the branch counter's interrupt came past the branch count"
        )
    }
}

impl std::error::Error for Passed {}

/// What running backwards came to.
pub(super) enum Back {
    /// One instruction back.
    Stepped,
    /// The last breakpoint hit before where the fork was.
    Breakpoint,
    /// Before the instruction that last wrote a watched value, and the
    /// watch's address.
    Watch(Watch),
    /// The start of the run: nothing is before it.
    Start,
    /// gdb's Ctrl-C, or its connection closing, before the fork got there:
    /// it stopped on the way, at its position.
    Interrupted(Heard),
}

/// The fork's checkpoints in memory, by the step whose start each keeps.
pub(super) type Checkpoints = BTreeMap<u64, CheckpointId>;

impl Debuggee<'_> {
    /// Notes where a forward stop left the fork: at another step's start,
    /// or `mv` further into the step it was in. A stop at a step's start
    /// completes the step's pending exit, as a replay to it does.
    pub(super) fn arrived(&mut self, mv: Option<Move>) -> Result<()> {
        let step = self.machine.step();
        if step != self.position.step {
            self.position = Position::start(step);
        }
        match mv {
            Some(mv) => self.position = self.position.then(mv),
            None if self.position.moves.is_empty() => self.machine.complete_pending_exit()?,
            None => {}
        }
        Ok(())
    }

    /// Why the fork cannot run backwards, when it cannot: gdb changed its
    /// registers or memory, or it left the recording, so the run replayed
    /// is not what the fork did.
    pub(super) fn cannot_go_back(&self) -> Option<String> {
        if self.altered {
            return Some(
                "gdb changed the fork's registers or memory, so replaying the run cannot \
                 go back over what the fork did since"
                    .into(),
            );
        }
        self.follow.left_at.map(|step| {
            format!(
                "the fork left the recording at step {step}, so replaying the run cannot \
                 go back over what the fork did since"
            )
        })
    }

    /// One instruction back, or the start of the run; or where gdb's
    /// Ctrl-C stopped the fork on the way.
    pub(super) fn step_back(&mut self) -> Result<Back> {
        let back = self.step_back_unless_interrupted();
        self.unless_interrupted(back)
    }

    /// The last hit of one of gdb's traps before here, or the start of the
    /// run when there is none; or where gdb's Ctrl-C stopped the fork on
    /// the way.
    pub(super) fn continue_back(&mut self) -> Result<Back> {
        let back = self.continue_back_unless_interrupted();
        self.unless_interrupted(back)
    }

    /// `back`, or, when gdb interrupted it, the fork readied for gdb where
    /// it stopped.
    fn unless_interrupted(&mut self, back: Result<Back>) -> Result<Back> {
        match back {
            Err(e) if e.is::<Interrupted>() => {
                let heard = e.downcast_ref::<Interrupted>().expect("checked").0;
                if heard == Heard::Interrupt {
                    self.ready_for_gdb()?;
                }
                Ok(Back::Interrupted(heard))
            }
            other => other,
        }
    }

    /// Fails with [`Interrupted`], the fork's position set to `at`, where
    /// it is, when gdb has sent Ctrl-C or gone.
    fn listen(&mut self, at: impl FnOnce() -> Position) -> Result<()> {
        match self.listener.heard() {
            Heard::Nothing => Ok(()),
            heard => {
                self.position = at();
                Err(Interrupted(heard).into())
            }
        }
    }

    fn step_back_unless_interrupted(&mut self) -> Result<Back> {
        let here = self.position.clone();
        let back = with_margins(|margin| self.before(&here, margin))?;
        let Some(back) = back else {
            self.go_to_or_retry(&Position::start(0))?;
            return Ok(Back::Start);
        };
        self.go_to_or_retry(&back)?;
        Ok(Back::Stepped)
    }

    fn continue_back_unless_interrupted(&mut self) -> Result<Back> {
        let here = self.position.clone();
        let Some((hit, target)) = with_margins(|margin| self.last_hit_before(&here, margin))?
        else {
            self.go_to_or_retry(&Position::start(0))?;
            return Ok(Back::Start);
        };
        match target {
            GdbTrap::Breakpoint(_) => {
                self.go_to_or_retry(&hit)?;
                Ok(Back::Breakpoint)
            }
            GdbTrap::Watch(watch) => {
                let before = with_margins(|margin| self.before(&hit, margin))?;
                self.go_to_or_retry(&before.unwrap_or_else(|| Position::start(0)))?;
                Ok(Back::Watch(watch))
            }
        }
    }

    /// Brings the fork to `target`, with a wider margin each time the
    /// counter's interrupt comes too late, and readies it for gdb again.
    fn go_to_or_retry(&mut self, target: &Position) -> Result<()> {
        with_margins(|margin| self.go_to(target, margin))?;
        self.ready_for_gdb()
    }

    /// Brings the fork to `target`: the start of its step, then its moves.
    fn go_to(&mut self, target: &Position, margin: u64) -> Result<()> {
        self.at_step_start(target.step)?;
        for (before, mv) in target.moves.in_order() {
            let from = Position {
                step: target.step,
                moves: before,
            };
            self.replay(mv, &from, margin)?;
        }
        self.position = target.clone();
        Ok(())
    }

    /// Brings the fork to the start of step `step`: rewound to its
    /// checkpoint there, else rewound to the latest one before and run
    /// forward, else a fork of the run there. The start is kept as a
    /// checkpoint for next time.
    fn at_step_start(&mut self, step: u64) -> Result<()> {
        let history = self.history.as_ref().expect("going back takes a history");
        match self.checkpoints.range(..=step).next_back() {
            Some((&at, &id)) => {
                let pages = StorePages(history.store()?);
                self.machine.rewind(id, &pages)?;
                if at == step {
                    self.position = Position::start(step);
                    return Ok(());
                }
                self.machine.clear_debug()?;
                self.machine.run(Some(step), &mut Ignore)?;
            }
            None => {
                self.machine = history.machine_at(step)?;
                self.checkpoints.clear();
            }
        }
        if self.machine.step() != step {
            bail!(
                "the run ended at step {} before step {step}",
                self.machine.step()
            );
        }
        let id = self.machine.checkpoint()?;
        self.checkpoints.insert(step, id);
        self.position = Position::start(step);
        Ok(())
    }

    /// Does `mv` again from where the fork is, `from`, with none of gdb's
    /// traps.
    fn replay(&mut self, mv: Move, from: &Position, margin: u64) -> Result<()> {
        match mv {
            Move::Steps(n) => {
                self.machine.set_debug(Stepping::Yes, &[])?;
                for i in 0..n {
                    if i > 0 && i % LISTEN_EVERY == 0 {
                        self.listen(|| from.then(Move::Steps(i)))?;
                    }
                    match self.machine.run(None, &mut Ignore)? {
                        Outcome::Debug(DebugStop::Step) => {}
                        other => bail!("a single step replayed came to {other:?}"),
                    }
                }
            }
            Move::Hit(target) => {
                self.machine.set_debug(Stepping::No, &target.registers())?;
                loop {
                    match self.machine.run(None, &mut Ignore)? {
                        Outcome::Debug(DebugStop::Breakpoint(address)) => {
                            if self.in_scope(address)? {
                                break;
                            }
                            self.machine.pass_breakpoint()?;
                        }
                        Outcome::Debug(DebugStop::Watchpoint { address, .. }) => {
                            if self.in_scope(address)? {
                                break;
                            }
                        }
                        other => bail!("a run to a trap replayed came to {other:?}"),
                    }
                }
            }
            Move::Branches(count) => {
                self.machine.set_debug(Stepping::No, &[])?;
                self.machine.set_branch_stop(Some(count), margin)?;
                match self.machine.run(None, &mut Ignore)? {
                    Outcome::Debug(DebugStop::Branches) => {}
                    Outcome::Debug(DebugStop::PassedBranches) => return Err(Passed.into()),
                    other => bail!("a run to a branch count replayed came to {other:?}"),
                }
            }
        }
        Ok(())
    }

    /// The instruction boundary before `here`, or None at the run's start.
    /// One single step fewer when `here` ends in single steps; else found
    /// from where `here`'s last move began, closing in on its branch count
    /// when the run counts branches, then single-stepping until the step
    /// that reaches `here`.
    fn before(&mut self, here: &Position, margin: u64) -> Result<Option<Position>> {
        if let Some((Move::Steps(n), before)) = here.moves.split_last() {
            let moves = match n {
                1 => before,
                n => before.then(Move::Steps(n - 1)),
            };
            return Ok(Some(Position {
                step: here.step,
                moves,
            }));
        }
        let (anchor, arrival) = match here.moves.split_last() {
            Some((last, rest)) => {
                let arrival = match last {
                    Move::Hit(GdbTrap::Watch(watch)) => Arrival::Watch(watch),
                    Move::Hit(GdbTrap::Breakpoint(address)) => Arrival::Breakpoint(address),
                    Move::Branches(count) => Arrival::Branches(count),
                    Move::Steps(_) => unreachable!("handled above"),
                };
                let anchor = Position {
                    step: here.step,
                    moves: rest,
                };
                (anchor, arrival)
            }
            None if here.step == 0 => return Ok(None),
            None => (
                Position::start(here.step - 1),
                Arrival::StepStart(here.step),
            ),
        };

        // The branch count at `here`, then from the anchor as close as one
        // branch short of it.
        if self.position != *here {
            self.go_to(here, margin)?;
        }
        let count_here = self.machine.branch_count()?;
        self.go_to(&anchor, margin)?;
        let mut found = anchor.clone();
        if let (Some(here_count), Some(anchor_count)) = (count_here, self.machine.branch_count()?)
            && here_count > anchor_count + 1
        {
            self.replay(Move::Branches(here_count - 1), &found, margin)?;
            found = found.then(Move::Branches(here_count - 1));
        }

        // Single steps, each a boundary before `here`, until the one that
        // reaches it.
        let registers = match arrival {
            Arrival::Watch(watch) => super::watch_registers(&watch),
            _ => Vec::new(),
        };
        self.machine.set_debug(Stepping::Yes, &registers)?;
        let mut steps = 0;
        loop {
            if steps > 0 && steps % LISTEN_EVERY == 0 {
                self.listen(|| found.then(Move::Steps(steps)))?;
            }
            let outcome = self.machine.run(None, &mut Ignore)?;
            let reached = match (arrival, outcome) {
                (Arrival::Watch(_), Outcome::Debug(DebugStop::Watchpoint { address, .. })) => {
                    self.in_scope(address)?
                }
                (Arrival::Breakpoint(address), Outcome::Debug(DebugStop::Step)) => {
                    self.machine.registers()?.rip == address && self.in_scope(address)?
                }
                (Arrival::Branches(count), Outcome::Debug(DebugStop::Step)) => {
                    self.machine.branch_count()? == Some(count)
                }
                (Arrival::StepStart(step), Outcome::Debug(DebugStop::Step)) => {
                    self.machine.step() == step
                }
                (_, Outcome::Debug(DebugStop::Step | DebugStop::Watchpoint { .. })) => false,
                (_, other) => bail!("stepping toward where gdb was came to {other:?}"),
            };
            if reached {
                break;
            }
            steps += 1;
        }
        if steps > 0 {
            found = found.then(Move::Steps(steps));
        }
        self.position = here.clone();
        Ok(Some(found))
    }

    /// The last hit of one of gdb's traps in the debugged process before
    /// `here`, and the trap: the run is replayed with gdb's traps set from
    /// a window's start, the latest checkpoint or keyframe at or before
    /// `here`, up to `here`; with no hit there, from the window before,
    /// and so on back to the run's start.
    fn last_hit_before(
        &mut self,
        here: &Position,
        margin: u64,
    ) -> Result<Option<(Position, GdbTrap)>> {
        let keyframes = self
            .history
            .as_ref()
            .expect("going back takes a history")
            .keyframes()?;
        let mut end = None;
        let mut before = here.step;
        loop {
            let start = self
                .checkpoints
                .range(..=before)
                .map(|(&step, _)| step)
                .chain(keyframes.iter().copied().filter(|&s| s <= before))
                .max()
                .unwrap_or(0);
            let found = self.scan(start, end, here, margin)?;
            if found.is_some() || start == 0 {
                return Ok(found);
            }
            end = Some(start);
            before = start - 1;
        }
    }

    /// Replays the run from the start of step `start` with gdb's traps
    /// set, up to the start of step `end`, or with None up to `here`, and
    /// returns the last hit in the debugged process on the way.
    fn scan(
        &mut self,
        start: u64,
        end: Option<u64>,
        here: &Position,
        margin: u64,
    ) -> Result<Option<(Position, GdbTrap)>> {
        self.go_to(&Position::start(start), margin)?;
        self.set_traps_for_scan(None)?;
        let mut scan = Scan::default();
        let until = end.unwrap_or(here.step);

        // Whole steps, up to `until`'s start, in chunks with a look at
        // gdb's connection between them.
        loop {
            let pause = (self.machine.step() + LISTEN_EVERY_STEPS).min(until);
            let outcome = self.machine.run(Some(pause), &mut Ignore)?;
            let at = self.machine.step();
            match outcome {
                Outcome::Paused if at >= until => break,
                // A step's start is a position once its exit is done,
                // as a replay to it leaves it.
                Outcome::Paused => {
                    let heard = self.listener.heard();
                    if heard != Heard::Nothing {
                        self.machine.complete_pending_exit()?;
                        self.position = Position::start(at);
                        return Err(Interrupted(heard).into());
                    }
                }
                Outcome::Stopped(_) => return Ok(scan.last),
                Outcome::Debug(stop) => self.note_hit(&mut scan, at, stop)?,
            }
        }
        if end.is_some() {
            return Ok(scan.last);
        }

        // Then `here`'s moves within its step, gdb's traps set alongside
        // each move's own. A breakpoint where a move ended is a hit there,
        // passed before the next move runs.
        self.machine.complete_pending_exit()?;
        let mut done = Position::start(here.step);
        for (_, mv) in here.moves.in_order() {
            scan.since = done.clone();
            match mv {
                Move::Steps(n) => {
                    self.set_traps_for_scan(None)?;
                    let registers = self.traps.registers();
                    self.machine.set_debug(Stepping::Yes, &registers)?;
                    for j in 0..n {
                        let boundary = match j {
                            0 => done.clone(),
                            j => done.then(Move::Steps(j)),
                        };
                        if j > 0 && j % LISTEN_EVERY == 0 {
                            self.listen(|| boundary.clone())?;
                        }
                        loop {
                            match self.machine.run(None, &mut Ignore)? {
                                Outcome::Debug(DebugStop::Step) => break,
                                Outcome::Debug(DebugStop::Watchpoint {
                                    address: piece, ..
                                }) => {
                                    let after = done.then(Move::Steps(j + 1));
                                    self.note_watch(&mut scan, piece, after)?;
                                    break;
                                }
                                Outcome::Debug(DebugStop::Breakpoint(address)) => {
                                    self.note_breakpoint(&mut scan, address, boundary.clone())?;
                                    self.machine.pass_breakpoint()?;
                                }
                                other => bail!("a single step replayed came to {other:?}"),
                            }
                        }
                    }
                }
                Move::Hit(target) => {
                    self.set_traps_for_scan(Some(target))?;
                    self.pass_breakpoint_here(&mut scan, &done)?;
                    loop {
                        let outcome = self.machine.run(None, &mut Ignore)?;
                        let Outcome::Debug(stop) = outcome else {
                            bail!("a run to a trap replayed came to {outcome:?}");
                        };
                        if self.is_move_target(target, stop)? {
                            break;
                        }
                        self.note_hit(&mut scan, here.step, stop)?;
                    }
                }
                Move::Branches(count) => {
                    self.set_traps_for_scan(None)?;
                    self.pass_breakpoint_here(&mut scan, &done)?;
                    self.machine.set_branch_stop(Some(count), margin)?;
                    loop {
                        match self.machine.run(None, &mut Ignore)? {
                            Outcome::Debug(DebugStop::Branches) => break,
                            Outcome::Debug(DebugStop::PassedBranches) => return Err(Passed.into()),
                            Outcome::Debug(stop) => self.note_hit(&mut scan, here.step, stop)?,
                            other => bail!("a run to a branch count replayed came to {other:?}"),
                        }
                    }
                }
            }
            done = done.then(mv);
        }
        Ok(scan.last)
    }
}

/// What a replay with gdb's traps set has seen: where the fork is, as a
/// position, and the last hit of a trap in the debugged process.
#[derive(Default)]
struct Scan {
    since: Position,
    last: Option<(Position, GdbTrap)>,
    /// How many hits in the debugged process it has seen.
    hits: u64,
}

impl Default for Position {
    fn default() -> Position {
        Position::start(0)
    }
}

impl Debuggee<'_> {
    /// Sets gdb's traps on the fork for a replay, as gdb set them, with
    /// `target`'s register too when given: a breakpoint gdb holds in a
    /// register moves into int3 to make room.
    fn set_traps_for_scan(&mut self, target: Option<GdbTrap>) -> Result<()> {
        self.place_traps()?;
        let mut registers = self.traps.registers();
        if let Some(target) = target {
            let needed = target.registers();
            let mut memory = super::Int3Writer {
                machine: &mut self.machine,
                space: self.space,
            };
            if !self.traps.free_registers(needed.len(), &mut memory)? {
                bail!(
                    "running backwards to here takes a debug register that gdb's \
                     watchpoints and hardware breakpoints hold"
                );
            }
            registers = self.traps.registers();
            registers.extend(needed);
        }
        self.machine.set_debug(Stepping::No, &registers)?;
        Ok(())
    }

    /// Whether `stop` is the hit `target`, a move of the position being
    /// replayed, runs to.
    fn is_move_target(&mut self, target: GdbTrap, stop: DebugStop) -> Result<bool> {
        Ok(match (target, stop) {
            (GdbTrap::Breakpoint(address), DebugStop::Breakpoint(at)) => {
                at == address && self.in_scope(at)?
            }
            (GdbTrap::Watch(watch), DebugStop::Watchpoint { address: piece, .. }) => {
                (watch.address..watch.address + watch.len).contains(&piece)
                    && self.in_scope(piece)?
            }
            _ => false,
        })
    }

    /// Notes a stop of a replay with gdb's traps, in step `step`: a hit in
    /// the debugged process becomes the last hit, and later positions
    /// count from it. A breakpoint is passed for the replay to run on.
    fn note_hit(&mut self, scan: &mut Scan, step: u64, stop: DebugStop) -> Result<()> {
        if scan.since.step != step {
            scan.since = Position::start(step);
        }
        // At a hit in the debugged process the fork is where `since` says,
        // so every so many of them gdb's Ctrl-C can stop it there: a busy
        // loop writing a watched value hits again and again without an
        // exit in between. A breakpoint is listened for before it is
        // passed, while the fork is still at it.
        match stop {
            DebugStop::Breakpoint(address) => {
                let hit = scan.since.then(Move::Hit(GdbTrap::Breakpoint(address)));
                if self.note_breakpoint(scan, address, hit.clone())? {
                    scan.since = hit;
                    self.listen_at_hit(scan)?;
                }
                self.machine.pass_breakpoint()?;
            }
            DebugStop::Watchpoint { address: piece, .. } => {
                let hit = scan
                    .since
                    .then(Move::Hit(GdbTrap::Watch(self.watch_of(piece)?)));
                if self.note_watch(scan, piece, hit.clone())? {
                    scan.since = hit;
                    self.listen_at_hit(scan)?;
                }
            }
            other => bail!("a replay with gdb's traps came to {other:?}"),
        }
        Ok(())
    }

    /// Counts a hit in the debugged process, the fork at `scan.since`, and
    /// every [`LISTEN_EVERY_HITS`] of them looks at gdb's connection.
    fn listen_at_hit(&mut self, scan: &mut Scan) -> Result<()> {
        scan.hits += 1;
        if !scan.hits.is_multiple_of(LISTEN_EVERY_HITS) {
            return Ok(());
        }
        let at = scan.since.clone();
        self.listen(|| at)
    }

    /// The watch gdb set that the watched piece at `piece` belongs to.
    fn watch_of(&self, piece: u64) -> Result<Watch> {
        self.traps
            .watch_containing(piece)
            .ok_or_else(|| anyhow::anyhow!("no watch of gdb's covers {piece:#x}"))
    }

    /// Notes the breakpoint at `address` reached at `at`, when it is in the
    /// debugged process. Returns whether it was.
    fn note_breakpoint(&mut self, scan: &mut Scan, address: u64, at: Position) -> Result<bool> {
        if !self.in_scope(address)? {
            return Ok(false);
        }
        scan.last = Some((at, GdbTrap::Breakpoint(address)));
        Ok(true)
    }

    /// Notes the watched write to the piece at `piece`, the fork at `at`
    /// after it, when it is in the debugged process. Returns whether it
    /// was.
    fn note_watch(&mut self, scan: &mut Scan, piece: u64, at: Position) -> Result<bool> {
        if !self.in_scope(piece)? {
            return Ok(false);
        }
        let watch = self.watch_of(piece)?;
        scan.last = Some((at, GdbTrap::Watch(watch)));
        Ok(true)
    }

    /// A breakpoint of gdb's at the instruction the fork is at, `at`,
    /// where a move ended, is a hit there; it is passed so the next move
    /// runs on from it.
    fn pass_breakpoint_here(&mut self, scan: &mut Scan, at: &Position) -> Result<()> {
        let rip = self.machine.registers()?.rip;
        if !self.traps.breakpoints.iter().any(|b| b.address == rip) {
            return Ok(());
        }
        self.note_breakpoint(scan, rip, at.clone())?;
        self.machine.pass_breakpoint()
    }

    /// Readies the fork for gdb after going back: gdb's breakpoints placed
    /// again, int3 written into the memory as it is now, the threads read
    /// again, where the kernel says they are by then, and the run's records
    /// after the step to compare with.
    fn ready_for_gdb(&mut self) -> Result<()> {
        self.place_traps()?;
        self.layout = self.machine.task_layout()?;
        let history = self.history.as_ref().expect("going back takes a history");
        self.follow = Follow::new(history.records_after(self.position.step)?);
        self.told = false;
        self.refresh_threads();
        Ok(())
    }
}

/// Runs `attempt` with each margin in turn until the counter's interrupt
/// comes in time.
fn with_margins<T>(mut attempt: impl FnMut(u64) -> Result<T>) -> Result<T> {
    let mut last = None;
    for margin in MARGINS {
        match attempt(margin) {
            Err(e) if e.is::<Passed>() => last = Some(e),
            other => return other,
        }
    }
    Err(last.expect("at least one margin"))
}

#[cfg(test)]
mod tests {
    // Positions' moves, built by hand as a search back builds them, with
    // no machine.
    use super::*;

    /// A position's moves, built one at a time: single steps add up, the
    /// moves come back oldest first each with the ones before it, and a
    /// position and the one made from it share what they have in common.
    #[test]
    fn moves_are_shared_with_the_positions_they_came_from() {
        let breakpoint = Move::Hit(GdbTrap::Breakpoint(0x401000));
        let a = Position::start(7).then(Move::Steps(2)).then(Move::Steps(3));
        let b = a.then(breakpoint);
        let moves = |p: &Position| -> Vec<Move> {
            p.moves.in_order().into_iter().map(|(_, m)| m).collect()
        };
        assert_eq!(moves(&a), [Move::Steps(5)]);
        assert_eq!(moves(&b), [Move::Steps(5), breakpoint]);
        assert_eq!(b.moves.in_order()[1].0, a.moves);
        assert_eq!(b.moves.split_last(), Some((breakpoint, a.moves.clone())));
        assert_ne!(a, b);
        assert_eq!(b, Position::start(7).then(Move::Steps(5)).then(breakpoint));
    }

    /// A list of a million moves, as many hits in one step make, is made
    /// and dropped without recursing a million deep.
    #[test]
    fn a_long_list_of_moves_drops() {
        let hit = Move::Hit(GdbTrap::Breakpoint(0x401000));
        let mut at = Position::start(1);
        for _ in 0..1_000_000 {
            at = at.then(hit);
        }
        assert_eq!(at.moves.len(), 1_000_000);
        drop(at);
    }
}
