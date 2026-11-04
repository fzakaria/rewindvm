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
//! x86 cannot trap reads alone. Reads come from a second replay of each
//! window that traps reads and writes together, matched in order against
//! the writes of the first: the accesses that are not writes are reads.

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rewind_trace::searching::{Phase, Searching};
use rewind_vmm::debug::{Access as TrapAccess, DebugStop, MAX_TRAPS, Stepping, Trap};
use rewind_vmm::pv::pt_regs;
use rewind_vmm::{Ignore, Machine, Outcome};

use crate::Home;
use crate::debug::{StorePages, USER_END, page_table, pieces};
use crate::run::{Keep, Run};
use crate::threads::Tasks;

/// The most bytes one search watches: each debug address register
/// watches at most 8.
pub const MAX_BYTES: u64 = MAX_TRAPS as u64 * 8;

/// How many steps a window's replay runs between reports of how far it
/// has got.
const REPORT_EVERY: u64 = 256;

/// One value watched: `len` bytes at `address`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub address: u64,
    pub len: u64,
}

/// Which accesses a search keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kinds {
    Writes,
    ReadsAndWrites,
}

/// How far back a search goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// Until each value's last this many writes are found, or `since`.
    Writes(usize),
    /// Back to `since`, whatever it finds.
    Since,
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
    pub kinds: Kinds,
    pub until: Until,
    /// The most accesses kept, the latest ones; a search that sees more
    /// says so and stops.
    pub cap: usize,
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
    /// How many accesses the search saw past the ones it kept, when it
    /// stopped because of `cap`.
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
    let total: u64 = search.values.iter().map(|v| v.len).sum();
    if search.values.is_empty() {
        bail!("nothing to watch");
    }
    if total > MAX_BYTES {
        bail!(
            "the values take {total} bytes, past the {MAX_BYTES} the CPU's {MAX_TRAPS} debug \
             registers can watch at once"
        );
    }
    let watched: Vec<(u64, u64)> = search
        .values
        .iter()
        .flat_map(|v| pieces(v.address, v.len))
        .collect();
    if watched.len() > MAX_TRAPS {
        bail!(
            "the values take {} debug registers as the CPU aligns them, past its {MAX_TRAPS}",
            watched.len()
        );
    }

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
    // the last one began, and no earlier than `since`.
    let mut kept: VecDeque<Access> = VecDeque::new();
    let mut writer: Option<(Window<'_>, u64)> = None;
    let mut dropped = 0;
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

        let mut window = Window::new(home, run, search, start)?;
        initial = window.values()?;
        progress.phase = Phase::Searching;
        let writes = window.replay(
            end,
            TrapAccess::Write,
            &watched,
            &mut progress,
            &started,
            report,
        )?;
        if now.is_none() {
            now = Some(window.values()?);
        }
        let accesses = match search.kinds {
            Kinds::Writes => writes,
            Kinds::ReadsAndWrites => {
                window.rewind()?;
                let all = window.replay(
                    end,
                    TrapAccess::ReadWrite,
                    &watched,
                    &mut progress,
                    &started,
                    report,
                )?;
                classify(all, &writes)
            }
        };
        progress.steps += end - start;
        progress.windows += 1;
        progress.writes = count(&kept, Kind::Write) + count(&accesses, Kind::Write);
        progress.reads = count(&kept, Kind::Read) + count(&accesses, Kind::Read);

        // The newest window with a write holds the latest write.
        let latest = accesses
            .iter()
            .rev()
            .find(|a| a.kind == Kind::Write)
            .map(|a| a.step);

        // The window's accesses go before the later windows'.
        for access in accesses.into_iter().rev() {
            if kept.len() >= search.cap {
                dropped += 1;
                continue;
            }
            kept.push_front(access);
        }
        if writer.is_none()
            && let Some(step) = latest
        {
            writer = Some((window, step));
        }
        if dropped > 0 || done(search, &kept) {
            break;
        }
        end = start;
    }
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
    let found = Found {
        accesses: kept.into(),
        searched_from: start,
        initial,
        now: now.unwrap_or_default(),
        windows: progress.windows,
        dropped,
        elapsed_ms: progress.elapsed_ms,
    };
    Ok((found, machine))
}

/// Whether a search has found what it goes back for.
fn done(search: &Search, kept: &VecDeque<Access>) -> bool {
    match search.until {
        Until::Since => false,
        Until::Writes(n) => (0..search.values.len()).all(|v| {
            kept.iter()
                .filter(|a| a.value == v && a.kind == Kind::Write)
                .count()
                >= n
        }),
    }
}

/// How many of `accesses` are of `kind`.
fn count<'a>(accesses: impl IntoIterator<Item = &'a Access>, kind: Kind) -> u64 {
    accesses.into_iter().filter(|a| a.kind == kind).count() as u64
}

/// Each access of a replay that trapped reads and writes, as a read or a
/// write: `writes`, from a replay that trapped writes alone, is the same
/// replay's writes, in order, so an access that is the next of them is
/// that write and any other is a read.
fn classify(all: Vec<Access>, writes: &[Access]) -> Vec<Access> {
    let mut next = writes.iter().peekable();
    all.into_iter()
        .map(|mut access| {
            let is_write = next
                .peek()
                .is_some_and(|w| w.step == access.step && w.after_pc == access.after_pc);
            if is_write {
                let write = next.next().expect("peeked");
                access.before.clone_from(&write.before);
                access.after.clone_from(&write.after);
                access.kind = Kind::Write;
            } else {
                access.kind = Kind::Read;
                access.before.clone_from(&access.after);
            }
            access
        })
        .collect()
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
    /// `end`, trapping `access` to the bytes at `watched`, and returns the
    /// process's accesses, in order, as writes.
    fn replay(
        &mut self,
        end: u64,
        access_kind: TrapAccess,
        watched: &[(u64, u64)],
        progress: &mut Searching,
        started: &Instant,
        report: Report<'_>,
    ) -> Result<Vec<Access>> {
        let traps: Vec<Trap> = watched
            .iter()
            .map(|&(address, len)| Trap::Watch {
                address,
                len,
                access: access_kind,
            })
            .collect();
        self.machine.set_debug(Stepping::No, &traps)?;
        let mut now = self.values()?;
        let mut found = Vec::new();
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
                Outcome::Debug(DebugStop::Watchpoint(piece)) => {
                    let Some(access) = self.hit(piece, &mut now)? else {
                        continue;
                    };
                    // Live counts are of writes; reads are told apart
                    // once the window's second replay is done.
                    if access_kind == TrapAccess::Write {
                        progress.writes += 1;
                    }
                    found.push(access);
                }
                Outcome::Debug(other) => bail!("a search for accesses came to {other:?}"),
            }
        }
        self.machine.clear_debug()?;
        Ok(found)
    }

    /// The access the trap on the watched piece at `piece` is, when the
    /// CPU had the process's page table, with `now`, each value's bytes
    /// before it, brought up to date.
    fn hit(&self, piece: u64, now: &mut [Option<Vec<u8>>]) -> Result<Option<Access>> {
        let cr3 = self.machine.special_registers()?.cr3;
        if page_table(cr3) != self.page_table {
            return Ok(None);
        }
        let value = self
            .search
            .values
            .iter()
            .position(|v| (v.address..v.address + v.len).contains(&piece))
            .context("a trap on bytes no value covers")?;
        let after = self.read(&self.search.values[value])?;
        let before = std::mem::replace(&mut now[value], after.clone());
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
        Ok(Some(Access {
            step: self.machine.step(),
            value,
            kind: Kind::Write,
            after_pc,
            tid,
            thread,
            before,
            after,
            syscall,
        }))
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
        .page_table(task)?
        .with_context(|| format!("process {pid} has no memory of its own at step {step}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(step: u64, after_pc: u64, byte: u8) -> Access {
        Access {
            step,
            value: 0,
            kind: Kind::Write,
            after_pc,
            tid: 1,
            thread: "t".into(),
            before: None,
            after: Some(vec![byte]),
            syscall: None,
        }
    }

    /// The accesses of a replay trapping reads and writes are the writes
    /// of one trapping writes alone, in the same order, with reads between:
    /// one at the same instruction as a write in another step, and one
    /// at a later instruction of the write's step, stay reads.
    #[test]
    fn accesses_that_are_not_the_next_write_are_reads() {
        let writes = vec![access(10, 0x100, 1), access(12, 0x200, 2)];
        let all = vec![
            access(9, 0x100, 0),
            access(10, 0x100, 1),
            access(10, 0x180, 1),
            access(12, 0x200, 2),
            access(13, 0x300, 2),
        ];
        let kinds: Vec<Kind> = classify(all, &writes).iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Read, Kind::Write, Kind::Read, Kind::Write, Kind::Read]
        );
    }

    /// A read keeps the bytes it saw on both sides; a write keeps the
    /// write replay's bytes before it.
    #[test]
    fn a_read_s_bytes_do_not_change() {
        let mut write = access(10, 0x100, 1);
        write.before = Some(vec![0]);
        let classified = classify(vec![access(9, 0x50, 0), access(10, 0x100, 1)], &[write]);
        assert_eq!(classified[0].before, Some(vec![0]));
        assert_eq!(classified[0].after, Some(vec![0]));
        assert_eq!(classified[1].before, Some(vec![0]));
    }
}
