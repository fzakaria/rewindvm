//! The accesses to values in a process's memory over a stretch of a run:
//! the writes that gave a value what it holds at a step, and, when asked,
//! the reads of it.
//!
//! A run is deterministic, so the search replays it with the values
//! watched in the CPU's debug address registers. A watch traps after the
//! instruction that accessed the bytes, which is a VM exit the guest never
//! sees, so the replay runs at full speed between hits. The search goes
//! back a window at a time, newest first: from the latest keyframe before
//! where it is asked about up to there, then from the keyframe before that
//! up to the first window's start, and so on. Most values a failure is
//! about were written recently, and the first window usually holds the
//! answer.
//!
//! A debug register watches a virtual address whatever process is on the
//! CPU, so a hit counts only when the CPU had the process's page table:
//! one of its threads, or the kernel working for one of them, as a
//! `read` system call copying into the value does. Writes that never pass
//! through the watched virtual address do not trap: a fresh zeroed page,
//! another process writing memory both share, `process_vm_writev`.
//!
//! x86 cannot trap reads alone. To find reads, each piece of a value
//! takes two debug registers, one trapping writes and one trapping reads
//! and writes. One instruction's access sets the bits of both when it
//! wrote, and of the second alone when it only read, so a single replay
//! tells the two apart. An instruction that reads and writes the value,
//! as `add` to memory does, is a write.
//!
//! A search keeps a bounded number of accesses as it replays: each
//! value's last writes, or the latest accesses of every kind up to a cap,
//! so a long loop over a value costs replay time and not memory.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rewind_trace::searching::{Phase, Searching};
use rewind_vmm::debug::{Access as TrapAccess, DebugStop, MAX_TRAPS, Stepping, Trap, TrapSet};
use rewind_vmm::pv::pt_regs;
use rewind_vmm::{Ignore, Machine, Outcome};

use crate::Home;
use crate::debug::{StorePages, USER_END, page_table, pieces};
use crate::run::{Keep, Run};
use crate::threads::Tasks;

/// The most bytes one search for writes watches: each debug address
/// register watches at most 8.
pub const MAX_BYTES: u64 = MAX_TRAPS as u64 * 8;

/// How many debug registers each piece of a value takes in a search for
/// reads: one trapping writes, one trapping reads and writes.
const REGISTERS_PER_READ_PIECE: usize = 2;

/// How many steps a window's replay runs between reports of how far it
/// has got.
const REPORT_EVERY: u64 = 256;

/// One value watched: `len` bytes at `address`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub address: u64,
    pub len: u64,
}

/// What a search goes back for, and what it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wanted {
    /// Each value's last this many writes, back to `since` at most.
    LastWrites(usize),
    /// Every read and write back to `since`, the latest this many kept: a
    /// search that sees more says so and goes no further back.
    Accesses(usize),
}

/// A search for the accesses to `values`, in the memory of process
/// `pid`, in the steps from `since` up to the start of step `before`. The
/// process must have the same memory over the steps: `since` no earlier
/// than its start or its last exec.
#[derive(Clone, Debug)]
pub struct Search {
    pub pid: u32,
    pub values: Vec<Value>,
    pub before: u64,
    pub since: u64,
    pub wanted: Wanted,
}

/// Whether an access read or wrote the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
}

/// One access to a watched value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// The step it happened in: a fork at this step is just before it.
    pub step: u64,
    /// The index in [`Search::values`] of the value accessed.
    pub value: usize,
    pub kind: Kind,
    /// The instruction after the one that made the access, where the CPU
    /// trapped.
    pub after_pc: u64,
    /// The thread that made it, and the kernel's name for it.
    pub tid: u32,
    pub thread: String,
    /// The value's bytes before the access and after it; the same for a
    /// read. None where the process had no memory there.
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    /// The system call the thread was in, by number, for an access the
    /// kernel made for it.
    pub syscall: Option<u64>,
    /// Where it falls among the process's accesses to the value in its
    /// step, from 1: among the writes for a write, among every access for
    /// a read. [`run_to_access`] finds it again by this.
    pub hit: u32,
}

/// How many accesses to each value a replay has seen in the step it is
/// in, to give each its [`Access::hit`].
#[derive(Debug, Default)]
struct Hits {
    /// For each value by index: the step counted, and the writes and the
    /// accesses of every kind counted there.
    by_value: HashMap<usize, (u64, u32, u32)>,
}

impl Hits {
    /// The hit of the next access to value `value` in step `step`, of
    /// kind `kind`: its place among the step's writes for a write, among
    /// every access for a read. A write counts among every access too.
    fn next(&mut self, value: usize, step: u64, kind: Kind) -> u32 {
        let counted = self.by_value.entry(value).or_insert((step, 0, 0));
        if counted.0 != step {
            *counted = (step, 0, 0);
        }
        counted.2 += 1;
        match kind {
            Kind::Write => {
                counted.1 += 1;
                counted.1
            }
            Kind::Read => counted.2,
        }
    }
}

/// One debug register of a search: a piece of value `value`, trapping
/// `access`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Watch {
    value: usize,
    address: u64,
    len: u64,
    access: TrapAccess,
}

/// The debug registers a search for `wanted` watches `values` with: for
/// writes, one per piece trapping writes; for reads, two per piece, one
/// trapping writes and one trapping reads and writes.
fn watches(values: &[Value], wanted: Wanted) -> Result<Vec<Watch>> {
    if values.is_empty() {
        bail!("nothing to watch");
    }

    // The lengths are whatever the caller was given, so their sum is
    // taken without wrapping, and a value that runs past the end of the
    // address space is past what the registers watch too.
    let total = values
        .iter()
        .try_fold(0u64, |total, v| {
            v.address.checked_add(v.len)?;
            total.checked_add(v.len)
        })
        .unwrap_or(u64::MAX);
    let kinds: &[TrapAccess] = match wanted {
        Wanted::LastWrites(0) | Wanted::Accesses(0) => bail!("a search keeps at least 1 access"),
        Wanted::LastWrites(_) => &[TrapAccess::Write],
        Wanted::Accesses(_) => &[TrapAccess::Write, TrapAccess::ReadWrite],
    };
    if total > MAX_BYTES {
        bail!(
            "the values take {total} bytes, past the {MAX_BYTES} the CPU's {MAX_TRAPS} debug \
             registers can watch at once"
        );
    }
    let mut out = Vec::new();
    for (value, v) in values.iter().enumerate() {
        for (address, len) in pieces(v.address, v.len) {
            for &access in kinds {
                out.push(Watch {
                    value,
                    address,
                    len,
                    access,
                });
            }
        }
    }
    if out.len() > MAX_TRAPS {
        let pieces = out.len() / kinds.len();
        let reads = match wanted {
            Wanted::Accesses(_) => format!(
                ", and a search for reads takes {REGISTERS_PER_READ_PIECE} of them for each"
            ),
            Wanted::LastWrites(_) => String::new(),
        };
        bail!(
            "the values take {pieces} debug registers as the CPU aligns them{reads}, past its \
             {MAX_TRAPS}"
        );
    }
    Ok(out)
}

/// The values one stop's `traps` accessed, by index, each once, and how:
/// a value a write trap of matched was written, one only a read-and-write
/// trap of matched was read.
fn accessed(watches: &[Watch], traps: TrapSet) -> Vec<(usize, Kind)> {
    let mut by_value: BTreeMap<usize, Kind> = BTreeMap::new();
    for (i, watch) in watches.iter().enumerate() {
        if !traps.contains(i) {
            continue;
        }
        let kind = match watch.access {
            TrapAccess::Write => Kind::Write,
            TrapAccess::ReadWrite => Kind::Read,
        };
        let seen = by_value.entry(watch.value).or_insert(kind);
        if kind == Kind::Write {
            *seen = Kind::Write;
        }
    }
    by_value.into_iter().collect()
}

/// Where an access falls in the run: its step, then the order the
/// replays saw it in, which is the run's order within a step, as one
/// window's replay sees all of a step.
type Order = (u64, u64);

/// The accesses a search keeps, bounded however many it sees: the latest
/// ones of each bucket, a bucket per value for last writes, one for every
/// access.
#[derive(Debug)]
struct Kept {
    wanted: Wanted,
    buckets: Vec<BTreeMap<Order, Access>>,
    /// How many accesses went past a full bucket.
    dropped: u64,
}

impl Kept {
    fn new(wanted: Wanted, values: usize) -> Kept {
        let buckets = match wanted {
            Wanted::LastWrites(_) => values,
            Wanted::Accesses(_) => 1,
        };
        Kept {
            wanted,
            buckets: (0..buckets).map(|_| BTreeMap::new()).collect(),
            dropped: 0,
        }
    }

    /// Keeps `access`, seen at `order`, if it is among the latest of its
    /// bucket. A write older than a value's last writes is not asked for;
    /// an access past the cap is left out, and counted.
    fn push(&mut self, order: Order, access: Access) {
        let (bucket, room) = match self.wanted {
            Wanted::LastWrites(n) => (access.value, n),
            Wanted::Accesses(cap) => (0, cap),
        };
        let bucket = &mut self.buckets[bucket];
        bucket.insert(order, access);
        if bucket.len() <= room {
            return;
        }
        bucket.pop_first();
        if let Wanted::Accesses(_) = self.wanted {
            self.dropped += 1;
        }
    }

    /// Whether the search has what it goes back for: each value's last
    /// writes, or every access after one that did not fit.
    fn done(&self) -> bool {
        match self.wanted {
            Wanted::LastWrites(n) => self.buckets.iter().all(|b| b.len() >= n),
            Wanted::Accesses(_) => self.dropped > 0,
        }
    }

    /// How many of the accesses kept are of `kind`.
    fn count(&self, kind: Kind) -> u64 {
        self.buckets
            .iter()
            .flat_map(|b| b.values())
            .filter(|a| a.kind == kind)
            .count() as u64
    }

    /// The accesses kept, oldest first.
    fn into_accesses(self) -> Vec<Access> {
        let mut all: Vec<(Order, Access)> = self.buckets.into_iter().flatten().collect();
        all.sort_by_key(|(order, _)| *order);
        all.into_iter().map(|(_, access)| access).collect()
    }
}

/// An access to find in a step: the process's `hit`-th write of `value`,
/// or its `hit`-th access of any kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sought {
    pub value: Value,
    pub kind: Kind,
    pub hit: u32,
}

/// Runs `machine`, at the start of step `step`, to just after process
/// `pid` makes the access `sought` names in that step, with the value's
/// bytes watched as a search watches them: the machine a look at the
/// access itself needs, where a fork at the step is at its start, which
/// can be many lines of the program before. Fails when the step ends
/// first.
pub fn run_to_access(machine: &mut Machine, pid: u32, step: u64, sought: Sought) -> Result<()> {
    let process = process_page_table(machine, pid, step)?;
    let access = match sought.kind {
        Kind::Write => TrapAccess::Write,
        Kind::Read => TrapAccess::ReadWrite,
    };
    let traps: Vec<Trap> = pieces(sought.value.address, sought.value.len)
        .into_iter()
        .map(|(address, len)| Trap::Watch {
            address,
            len,
            access,
        })
        .collect();
    machine.set_debug(Stepping::No, &traps)?;

    // Each trap with the process's page table is one of its accesses.
    let mut seen = 0;
    let found = loop {
        match machine.run(Some(step + 1), &mut Ignore)? {
            Outcome::Debug(DebugStop::Watchpoint { .. }) => {
                let cr3 = machine.special_registers()?.cr3;
                if page_table(cr3) != process {
                    continue;
                }
                seen += 1;
                if seen == sought.hit {
                    break true;
                }
            }
            Outcome::Debug(other) => bail!("looking for an access came to {other:?}"),
            Outcome::Paused | Outcome::Stopped(_) => break false,
        }
    };
    machine.clear_debug()?;
    if !found {
        bail!(
            "step {step} ended after {seen} of process {pid}'s accesses to the {} bytes at \
             {:#x}, before access {}",
            sought.value.len,
            sought.value.address,
            sought.hit
        );
    }
    Ok(())
}

/// What a search found.
#[derive(Clone, Debug)]
pub struct Found {
    /// The accesses kept, oldest first.
    pub accesses: Vec<Access>,
    /// The earliest step searched from.
    pub searched_from: u64,
    /// Each value's bytes at `searched_from`, and at the start of step
    /// `before`.
    pub initial: Vec<Option<Vec<u8>>>,
    pub now: Vec<Option<Vec<u8>>>,
    pub windows: u32,
    /// How many accesses the search saw past the ones it kept: those
    /// before the earliest one kept, with [`Wanted::Accesses`], whose
    /// search then went no further back.
    pub dropped: u64,
    /// How long the search took, in milliseconds.
    pub elapsed_ms: u64,
}

/// What a search says as it goes.
pub type Report<'a> = &'a mut dyn FnMut(&Searching);

/// Searches `run` as `search` asks, telling `report` how far it has got.
pub fn search(home: &Home, run: &Run, search: &Search, report: Report<'_>) -> Result<Found> {
    Ok(search_keeping(home, run, search, report)?.0)
}

/// Searches `run` as [`search`] does, and keeps the machine of the window
/// the latest write was found in, brought to the start of that write's
/// step: a fork there, for a caller that looks at the write next, without
/// restoring a keyframe again. None when no write was found.
pub fn search_keeping(
    home: &Home,
    run: &Run,
    search: &Search,
    report: Report<'_>,
) -> Result<(Found, Option<Machine>)> {
    let watches = watches(&search.values, search.wanted)?;
    let keyframes = run.keyframes()?.steps();
    let started = Instant::now();
    let mut progress = Searching {
        phase: Phase::Restoring,
        before: search.before,
        window: (search.before, search.before),
        reached: search.before,
        steps: 0,
        windows: 0,
        writes: 0,
        reads: 0,
        elapsed_ms: 0,
    };

    // Windows newest first, each from the latest keyframe before where
    // the last one began, and no earlier than `since`. A window that held
    // no write hands its machine to the next, which puts it at its own
    // keyframe in place rather than restoring a new one.
    let mut kept = Kept::new(search.wanted, search.values.len());
    let mut replayed = Replayed {
        seen: 0,
        writes_in_window: None,
    };
    let mut writer: Option<(Window<'_>, u64)> = None;
    let mut spare: Option<Window<'_>> = None;
    let mut loaded = crate::keyframes::Loaded::default();
    let mut end = search.before;
    let mut initial = Vec::new();
    let mut now = None;
    let mut start = end;
    while end > search.since {
        start = keyframes
            .iter()
            .copied()
            .rfind(|&s| s < end)
            .unwrap_or(0)
            .max(search.since);
        progress.phase = Phase::Restoring;
        progress.window = (start, end);
        progress.reached = start;
        progress.elapsed_ms = started.elapsed().as_millis() as u64;
        report(&progress);

        let at_keyframe = keyframes.binary_search(&start).is_ok();
        let mut window = match spare.take() {
            Some(spare) if at_keyframe => spare.moved_to(run, start, &mut loaded)?,
            _ => Window::new(home, run, search, start)?,
        };
        initial = window.values()?;
        progress.phase = Phase::Searching;
        replayed.writes_in_window = None;
        window.replay(
            end,
            &watches,
            (&mut kept, &mut replayed),
            (&mut progress, &started),
            report,
        )?;
        if now.is_none() {
            now = Some(window.values()?);
        }
        progress.steps += end - start;
        progress.windows += 1;

        // The newest window with a write holds the latest write.
        match replayed.writes_in_window {
            Some(step) if writer.is_none() => writer = Some((window, step)),
            _ => spare = Some(window),
        }
        if kept.done() {
            break;
        }
        end = start;
    }
    progress.writes = kept.count(Kind::Write);
    progress.reads = kept.count(Kind::Read);
    progress.elapsed_ms = started.elapsed().as_millis() as u64;
    report(&progress);

    // The latest write's window, replayed again from its start to the
    // write's step, with nothing watched.
    let machine = match writer {
        Some((mut window, step)) => {
            window.rewind()?;
            if window.machine.step() < step {
                window.machine.run(Some(step), &mut Ignore)?;
            }
            Some(window.machine)
        }
        None => None,
    };
    let dropped = kept.dropped;
    let found = Found {
        accesses: kept.into_accesses(),
        searched_from: start,
        initial,
        now: now.unwrap_or_default(),
        windows: progress.windows,
        dropped,
        elapsed_ms: progress.elapsed_ms,
    };
    Ok((found, machine))
}

/// What a search's replays have seen: how many accesses, which orders
/// them, and the step of the latest write in the window being replayed.
#[derive(Debug)]
struct Replayed {
    seen: u64,
    writes_in_window: Option<u64>,
}

/// A machine at a window's start, to replay the window from.
struct Window<'a> {
    search: &'a Search,
    machine: Machine,
    /// The page table of the process searched.
    page_table: u64,
    start: u64,
    /// The machine at the window's start, kept to replay the window again,
    /// and the store its pages come from.
    checkpoint: rewind_vmm::checkpoint::CheckpointId,
    pages: StorePages,
}

impl<'a> Window<'a> {
    fn new(home: &Home, run: &Run, search: &'a Search, start: u64) -> Result<Window<'a>> {
        let mut machine = run.machine_at(home, start, Keep::Keyframe, &mut Ignore)?;
        machine.complete_pending_exit()?;
        let page_table = process_page_table(&machine, search.pid, start)?;
        let checkpoint = machine.checkpoint()?;
        let pages = StorePages(rewind_store::Store::open(&home.store())?);
        Ok(Window {
            search,
            machine,
            page_table,
            start,
            checkpoint,
            pages,
        })
    }

    /// This window's machine, put at the keyframe at `start` in place,
    /// as the window from there.
    fn moved_to(
        mut self,
        run: &Run,
        start: u64,
        loaded: &mut crate::keyframes::Loaded,
    ) -> Result<Window<'a>> {
        run.restore_in_place(&mut self.machine, start, &self.pages.0, loaded)?;
        self.machine.complete_pending_exit()?;
        self.page_table = process_page_table(&self.machine, self.search.pid, start)?;
        self.checkpoint = self.machine.checkpoint()?;
        self.start = start;
        Ok(self)
    }

    /// The machine back at the window's start.
    fn rewind(&mut self) -> Result<()> {
        self.machine.rewind(self.checkpoint, &self.pages)
    }

    /// Each value's bytes as the process has them now.
    fn values(&self) -> Result<Vec<Option<Vec<u8>>>> {
        self.search.values.iter().map(|v| self.read(v)).collect()
    }

    fn read(&self, value: &Value) -> Result<Option<Vec<u8>>> {
        let mut bytes = vec![0u8; value.len as usize];
        let read = self
            .machine
            .read_virtual_in(self.page_table, value.address, &mut bytes)?;
        Ok((read == bytes.len()).then_some(bytes))
    }

    /// Replays the window from where the machine is to the start of step
    /// `end` with `watches` in the debug registers, keeping the process's
    /// accesses in `kept`.
    fn replay(
        &mut self,
        end: u64,
        watches: &[Watch],
        (kept, replayed): (&mut Kept, &mut Replayed),
        (progress, started): (&mut Searching, &Instant),
        report: Report<'_>,
    ) -> Result<()> {
        let traps: Vec<Trap> = watches
            .iter()
            .map(|w| Trap::Watch {
                address: w.address,
                len: w.len,
                access: w.access,
            })
            .collect();
        self.machine.set_debug(Stepping::No, &traps)?;
        let mut now = self.values()?;
        let mut hits = Hits::default();
        let base = progress.steps;
        loop {
            let at = self.machine.step();
            if at >= end {
                break;
            }
            let pause = (at + REPORT_EVERY).min(end);
            match self.machine.run(Some(pause), &mut Ignore)? {
                Outcome::Paused => {
                    progress.reached = self.machine.step();
                    progress.steps = base + (progress.reached - self.start);
                    progress.elapsed_ms = started.elapsed().as_millis() as u64;
                    report(progress);
                }
                Outcome::Stopped(_) => break,
                Outcome::Debug(DebugStop::Watchpoint { traps, .. }) => {
                    for mut access in self.hit(&accessed(watches, traps), &mut now)? {
                        access.hit = hits.next(access.value, access.step, access.kind);
                        match access.kind {
                            Kind::Write => {
                                progress.writes += 1;
                                replayed.writes_in_window = Some(access.step);
                            }
                            Kind::Read => progress.reads += 1,
                        }
                        replayed.seen += 1;
                        kept.push((access.step, replayed.seen), access);
                    }
                }
                Outcome::Debug(other) => bail!("a search for accesses came to {other:?}"),
            }
        }
        self.machine.clear_debug()?;
        Ok(())
    }

    /// The accesses one stop made to the values in `values`, by index and
    /// kind, when the CPU had the process's page table, with `now`, each
    /// value's bytes before the stop, brought up to date.
    fn hit(&self, values: &[(usize, Kind)], now: &mut [Option<Vec<u8>>]) -> Result<Vec<Access>> {
        let cr3 = self.machine.special_registers()?.cr3;
        if values.is_empty() || page_table(cr3) != self.page_table {
            return Ok(Vec::new());
        }
        let after_pc = self.machine.registers()?.rip;

        // The thread on the CPU, and the system call it was in when the
        // kernel made the access for it.
        let (tid, thread, syscall) = match self.machine.task_layout()? {
            Some(layout) => {
                let tasks = Tasks::new(&self.machine, layout);
                let (_, thread) = tasks.current_thread()?;
                let syscall = match after_pc >= USER_END {
                    true => Some(tasks.user_registers(thread.task)?[pt_regs::ORIG_RAX]),
                    false => None,
                };
                (thread.tid, thread.name, syscall)
            }
            None => (0, String::new(), None),
        };

        // Each value's bytes on both sides: a read leaves them as they are.
        let mut out = Vec::new();
        for &(value, kind) in values {
            let after = self.read(&self.search.values[value])?;
            let was = std::mem::replace(&mut now[value], after.clone());
            let before = match kind {
                Kind::Write => was,
                Kind::Read => after.clone(),
            };
            out.push(Access {
                step: self.machine.step(),
                value,
                kind,
                after_pc,
                tid,
                thread: thread.clone(),
                before,
                after,
                syscall,
                hit: 0,
            });
        }
        Ok(out)
    }
}

/// The page table of process `pid` on `machine`, at `step`.
pub fn process_page_table(machine: &Machine, pid: u32, step: u64) -> Result<u64> {
    let layout = machine
        .task_layout()?
        .context("the run's kernel does not say where its tasks are")?;
    let tasks = Tasks::new(machine, layout);
    let task = tasks
        .process(pid)?
        .with_context(|| format!("no process {pid} at step {step}"))?;
    tasks
        .process_page_table(task)?
        .with_context(|| format!("process {pid} has no memory of its own at step {step}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(step: u64, value: usize, kind: Kind) -> Access {
        Access {
            step,
            value,
            kind,
            after_pc: 0x401000,
            tid: 1,
            thread: "t".into(),
            before: None,
            after: None,
            syscall: None,
            hit: 1,
        }
    }

    /// The traps of a stop, from the indices of the watches it matched.
    fn traps(indices: &[usize]) -> TrapSet {
        let watch = Trap::Watch {
            address: 0x1000,
            len: 1,
            access: TrapAccess::Write,
        };
        let all = [watch; MAX_TRAPS];
        let dr6 = indices.iter().fold(0u64, |dr6, i| dr6 | 1 << i);
        rewind_vmm::debug::trap_set(dr6, &all)
    }

    /// Each access counts where it falls among its value's accesses in
    /// its step, the count starting again in each step and for each
    /// value: steps 10, 10, 11 of value 0 and step 10 of value 1. A read
    /// counts among every access, a write among the writes and every
    /// access: write, read, write in one step are writes 1 and 2, the
    /// read access 2.
    #[test]
    fn hits_count_within_a_step_and_a_value() {
        let mut counts = Hits::default();
        assert_eq!(counts.next(0, 10, Kind::Write), 1);
        assert_eq!(counts.next(0, 10, Kind::Write), 2);
        assert_eq!(counts.next(1, 10, Kind::Write), 1);
        assert_eq!(counts.next(0, 11, Kind::Write), 1);

        let mut counts = Hits::default();
        assert_eq!(counts.next(0, 5, Kind::Write), 1);
        assert_eq!(counts.next(0, 5, Kind::Read), 2);
        assert_eq!(counts.next(0, 5, Kind::Write), 2);
    }

    /// One 64-bit store over two 4-byte values matches a watch of each,
    /// and the stop is a write of both.
    #[test]
    fn one_store_writes_every_value_it_covers() {
        let values = [
            Value {
                address: 0x1000,
                len: 4,
            },
            Value {
                address: 0x1004,
                len: 4,
            },
        ];
        let watches = watches(&values, Wanted::LastWrites(1)).unwrap();
        assert_eq!(watches.len(), 2);
        assert_eq!(
            accessed(&watches, traps(&[0, 1])),
            [(0, Kind::Write), (1, Kind::Write)]
        );
        assert_eq!(accessed(&watches, traps(&[1])), [(1, Kind::Write)]);
    }

    /// A value that takes two debug registers, 4 bytes from an address 2
    /// past an 8-byte boundary, is one access when a store matches both
    /// of its pieces; two values over the same bytes are each accessed.
    #[test]
    fn a_value_is_accessed_once_however_many_of_its_pieces_match() {
        let split = [Value {
            address: 0x1006,
            len: 4,
        }];
        let watches = watches(&split, Wanted::LastWrites(1)).unwrap();
        assert_eq!(watches.len(), 2);
        assert_eq!(accessed(&watches, traps(&[0, 1])), [(0, Kind::Write)]);

        let aliases = [
            Value {
                address: 0x1000,
                len: 8,
            },
            Value {
                address: 0x1000,
                len: 8,
            },
        ];
        let watches = watches_for(&aliases);
        assert_eq!(
            accessed(&watches, traps(&[0, 1])),
            [(0, Kind::Write), (1, Kind::Write)]
        );
    }

    fn watches_for(values: &[Value]) -> Vec<Watch> {
        watches(values, Wanted::LastWrites(1)).unwrap()
    }

    /// A search for reads gives each piece a write trap and a read-and-
    /// write trap: the second alone is a read, both a write, the same
    /// instruction's place either way, as a REP MOVSB that reads a byte
    /// in one iteration and writes it in the next stops at one RIP.
    #[test]
    fn a_read_matches_only_the_read_and_write_trap() {
        let byte = [Value {
            address: 0x1002,
            len: 1,
        }];
        let watches = watches(&byte, Wanted::Accesses(10)).unwrap();
        assert_eq!(
            watches.iter().map(|w| w.access).collect::<Vec<_>>(),
            [TrapAccess::Write, TrapAccess::ReadWrite]
        );
        assert_eq!(accessed(&watches, traps(&[1])), [(0, Kind::Read)]);
        assert_eq!(accessed(&watches, traps(&[0, 1])), [(0, Kind::Write)]);
        assert_eq!(accessed(&watches, traps(&[])), []);
    }

    /// A search for reads takes two registers a piece, so three pieces do
    /// not fit, where a search for writes fits four; a search keeps at
    /// least one access.
    #[test]
    fn a_search_for_reads_fits_half_the_pieces() {
        let three = [Value {
            address: 0x1000,
            len: 24,
        }];
        assert!(watches(&three, Wanted::LastWrites(1)).is_ok());
        let error = watches(&three, Wanted::Accesses(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("2 of them for each"), "{error}");
        assert!(watches(&three, Wanted::LastWrites(0)).is_err());
    }

    /// Values whose lengths add up past a u64, or that run past the end
    /// of the address space, are refused by their size before any is split
    /// into pieces.
    #[test]
    fn a_search_refuses_lengths_that_overflow() {
        const HALF: u64 = 1 << 63;
        let wrapping = [
            Value {
                address: 0x1000,
                len: HALF,
            },
            Value {
                address: 0x2000,
                len: HALF,
            },
        ];
        assert!(watches(&wrapping, Wanted::LastWrites(1)).is_err());
        let past_the_end = [Value {
            address: u64::MAX - 3,
            len: 8,
        }];
        assert!(watches(&past_the_end, Wanted::LastWrites(1)).is_err());
    }

    /// Last writes keep each value's latest n whatever the order windows
    /// are seen in: 20,000 writes of a counter after one of another value
    /// keep both values' last write, and no more than n of the counter's.
    #[test]
    fn last_writes_are_kept_for_each_value() {
        let mut kept = Kept::new(Wanted::LastWrites(1), 2);
        kept.push((302, 1), access(302, 0, Kind::Write));
        for i in 0..20_000 {
            kept.push((302, 2 + i), access(302, 1, Kind::Write));
        }
        assert_eq!(kept.buckets[1].len(), 1);
        assert_eq!(kept.dropped, 0);
        assert!(kept.done());
        let accesses = kept.into_accesses();
        assert_eq!(accesses.iter().map(|a| a.value).collect::<Vec<_>>(), [0, 1]);

        // An older window's writes come in after a newer one's and are
        // dropped for the value that has its writes.
        let mut kept = Kept::new(Wanted::LastWrites(1), 2);
        kept.push((20, 1), access(20, 1, Kind::Write));
        assert!(!kept.done());
        kept.push((10, 2), access(10, 1, Kind::Write));
        kept.push((10, 3), access(10, 0, Kind::Write));
        let steps: Vec<(u64, usize)> = kept
            .into_accesses()
            .iter()
            .map(|a| (a.step, a.value))
            .collect();
        assert_eq!(steps, [(10, 0), (20, 1)]);
    }

    /// Every access past the cap is counted and dropped, the latest
    /// kept, oldest first, however many come; a cap below the number of
    /// values keeps the latest accesses of any value.
    #[test]
    fn accesses_past_the_cap_are_counted() {
        let mut kept = Kept::new(Wanted::Accesses(1), 2);
        for i in 0..1000 {
            kept.push((5, i), access(5, (i % 2) as usize, Kind::Read));
        }
        assert_eq!(kept.dropped, 999);
        assert!(kept.done());
        assert_eq!(kept.buckets[0].len(), 1);
        assert_eq!(kept.into_accesses()[0].value, 1);
    }
}
