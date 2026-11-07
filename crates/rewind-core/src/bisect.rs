//! `rewind replay --bisect`: from where a replay found the machine first
//! differ, down to the step and the instruction that made it differ.
//!
//! A replay that compares the machine at keyframes (see [`crate::state`])
//! brackets a difference between the last keyframe that matched and the
//! first that did not, or between the last keyframe and the event that
//! differed. The recording holds the machine only at its keyframes, so
//! narrowing further compares two fresh replays with each other: both are
//! restored at the bracket's start and run side by side, and the first
//! step after which their machines differ is where something the run's
//! inputs do not decide reached it. From the step before, both are
//! single-stepped and their registers compared after each instruction; the
//! first instruction whose result differs is the cause. This works when
//! the source gives a different answer each time it is read, as `rdrand`
//! does. A source that differs only sometimes is tried again, `tries`
//! times.
//!
//! The two copies capture their machines the same way at the same places,
//! so whatever a capture does to a machine it does to both.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use rewind_vmm::debug::{DebugStop, Stepping};
use rewind_vmm::snapshot::Keyframe;
use rewind_vmm::state::StateDifference;
use rewind_vmm::{Machine, Observer, Outcome};
use serde::Serialize;

use crate::Home;
use crate::keyframes::Memory;
use crate::run::{Keep, Run, SpecExt};
use crate::state::HashPages;

/// The steps between two captures while the copies close in on the
/// stretch that differs, before they go a step at a time through it.
const STRIDE: u64 = 256;

/// The most instructions single-stepped in each copy before the search
/// gives up on a step: at about 6 µs an instruction, half a minute each.
pub const INSTRUCTION_BUDGET: u64 = 5_000_000;

/// The most bytes an x86 instruction has.
const MAX_INSTRUCTION: usize = 15;

/// The code segment's privilege level for user code.
const USER_DPL: u8 = 3;

/// Where a bisect looks: the steps from `from`, a keyframe or boot, to
/// `to`, by which the machine is known to differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Bracket {
    pub from: Option<u64>,
    pub to: u64,
}

/// Whether the instruction ran in user space or in the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    User,
    Kernel,
}

/// The first instruction whose result differed between the two copies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Instruction {
    pub rip: u64,
    pub mode: Mode,
    /// The process on the CPU and its name, from the VM's kernel.
    pub pid: Option<u32>,
    pub process: Option<String>,
    /// The instruction's bytes, as many as could be read.
    pub bytes: Vec<u8>,
    /// What the host instruction table names it, with why its result can
    /// differ, when it is one of the instructions that read the host.
    pub host: Option<crate::host_instructions::Host>,
    /// The registers that differ after it, one copy's value and the
    /// other's.
    pub registers: Vec<StateDifference>,
    /// How many instructions into the step it was.
    pub index: u64,
}

/// Where the two copies first differ within the step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WithinStep {
    /// An instruction's result differs.
    Instruction(Instruction),
    /// Every instruction of the step gave the same registers in both, and
    /// the machines differ after the exit that ends it: the monitor or
    /// KVM wrote the difference there, or it is in memory or state no
    /// register shows.
    AtTheExit {
        instructions: u64,
        state: Vec<StateDifference>,
        pages: Vec<u32>,
    },
    /// The step ran past the budget of single steps.
    OutOfBudget { instructions: u64 },
}

/// What a bisect found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Bisected {
    /// The first step after which the copies' machines differ, and where
    /// within it.
    Found {
        bracket: Bracket,
        step: u64,
        within: WithinStep,
        tries: u32,
    },
    /// Every try's two copies agreed over the whole bracket: the source
    /// gave the same answer to both, as one decided by this host but
    /// fixed on it does.
    Agreed { bracket: Bracket, tries: u32 },
}

/// Keeps the records a copy makes, with their steps.
#[derive(Default)]
struct Records(Vec<(u64, Vec<u8>)>);

impl Observer for Records {
    fn record(&mut self, step: u64, record: &[u8]) {
        self.0.push((step, record.to_vec()));
    }
}

/// One of the two replays, with its memory as its captures leave it.
struct Copy {
    machine: Machine,
    memory: Memory,
    parent: Option<u64>,
    records: Records,
    /// The last capture.
    captured: Option<Keyframe>,
    /// The pages the last capture holds.
    written: Vec<u32>,
}

impl Copy {
    fn new(run: &Run, home: &Home, from: Option<u64>) -> Result<Copy> {
        let mut records = Records::default();
        let machine = match from {
            Some(kf) => run.machine_at(home, kf, Keep::Nothing, &mut records)?,
            None => Machine::boot(&run.manifest.spec.config()?)?,
        };
        let mut memory = Memory::new(machine.ram_pages());
        if let Some(kf) = from {
            memory = Memory::of(&run.keyframes()?.chain(kf)?, machine.ram_pages())?;
        }
        Ok(Copy {
            machine,
            memory,
            parent: from,
            records: Records::default(),
            captured: None,
            written: Vec::new(),
        })
    }

    /// Runs to `step`, true when it got there.
    fn run_to(&mut self, step: u64) -> Result<bool> {
        let outcome = self.machine.run(Some(step), &mut self.records)?;
        Ok(matches!(outcome, Outcome::Paused) && self.machine.step() == step)
    }

    /// Captures the machine where it is, as a replay does at a keyframe.
    fn capture(&mut self) -> Result<()> {
        let mut kf = self.machine.keyframe(&mut HashPages, self.parent)?;
        self.memory.apply(kf.parent, &kf.pages)?;
        self.written = kf.pages.iter().map(|(page, _)| *page).collect();
        kf.pages.clear();
        self.parent = Some(self.machine.step());
        self.captured = Some(kf);
        Ok(())
    }
}

/// How two copies' last captures differ: their named state, and the pages
/// either wrote since the capture before, which hold all their memory
/// differs in, since the two agreed at that capture.
fn differences(a: &Copy, b: &Copy) -> Result<(Vec<StateDifference>, Vec<u32>)> {
    let (Some(ka), Some(kb)) = (&a.captured, &b.captured) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let state = ka.state_differences(kb)?;
    let written: BTreeSet<u32> = a.written.iter().chain(&b.written).copied().collect();
    let pages = written
        .into_iter()
        .filter(|page| a.memory.hash_of(*page) != b.memory.hash_of(*page))
        .collect();
    Ok((state, pages))
}

/// Whether two copies differ where each stands: in records made, in how
/// far they got, or in their last captures.
fn differ(a: &Copy, b: &Copy, reached: (bool, bool)) -> Result<bool> {
    if reached.0 != reached.1 || a.records.0 != b.records.0 {
        return Ok(true);
    }
    let (state, pages) = differences(a, b)?;
    Ok(!state.is_empty() || !pages.is_empty())
}

/// Two fresh copies of `run` from the bracket's start.
fn pair(run: &Run, home: &Home, bracket: Bracket) -> Result<(Copy, Copy)> {
    Ok((
        Copy::new(run, home, bracket.from)?,
        Copy::new(run, home, bracket.from)?,
    ))
}

/// Runs both copies to `step` and captures them. Whether each got there.
fn both_to(a: &mut Copy, b: &mut Copy, step: u64) -> Result<(bool, bool)> {
    let reached = (a.run_to(step)?, b.run_to(step)?);
    if reached.0 {
        a.capture()?;
    }
    if reached.1 {
        b.capture()?;
    }
    Ok(reached)
}

/// The first step in the bracket after which two fresh copies differ,
/// found in strides and then a step at a time, or None when they agree
/// throughout.
fn first_step(run: &Run, home: &Home, bracket: Bracket) -> Result<Option<u64>> {
    let start = bracket.from.unwrap_or(0);

    // In strides, to the end of the first stride after which they differ.
    let (mut a, mut b) = pair(run, home, bracket)?;
    let mut lo = start;
    let mut hi = None;
    while lo < bracket.to {
        let next = (lo + STRIDE).min(bracket.to);
        let reached = both_to(&mut a, &mut b, next)?;
        if differ(&a, &b, reached)? {
            hi = Some(next);
            break;
        }
        if !reached.0 {
            break;
        }
        lo = next;
    }
    let Some(hi) = hi else {
        return Ok(None);
    };

    // Two fresh copies, captured at the same strides to the start of that
    // one, then a step at a time through it.
    let (mut a, mut b) = pair(run, home, bracket)?;
    let mut at = start;
    while at < lo {
        at = (at + STRIDE).min(lo);
        both_to(&mut a, &mut b, at)?;
    }
    if differ(&a, &b, (true, true))? {
        // This time the copies parted earlier: the source is read more
        // than once. The earlier stride is searched instead.
        return first_step(
            run,
            home,
            Bracket {
                from: bracket.from,
                to: lo,
            },
        );
    }
    for step in lo + 1..=hi {
        let reached = both_to(&mut a, &mut b, step)?;
        if differ(&a, &b, reached)? {
            return Ok(Some(step));
        }
    }
    Ok(None)
}

/// Where within `step` the two copies first differ: both are brought to
/// the step before, captured as the search captured them, and single-
/// stepped side by side.
fn within_step(run: &Run, home: &Home, bracket: Bracket, step: u64) -> Result<WithinStep> {
    let start = bracket.from.unwrap_or(0);
    let (mut a, mut b) = pair(run, home, bracket)?;

    // To the step before, the way the search went: in strides to its
    // stride, then a step at a time.
    let before = step - 1;
    let stride_start = start + (before - start) / STRIDE * STRIDE;
    let mut at = start;
    while at < stride_start {
        at = (at + STRIDE).min(stride_start);
        both_to(&mut a, &mut b, at)?;
    }
    while at < before {
        at += 1;
        both_to(&mut a, &mut b, at)?;
    }

    // One instruction at a time in each, comparing registers after each.
    a.machine.set_debug(Stepping::Yes, &[])?;
    b.machine.set_debug(Stepping::Yes, &[])?;
    let mut index = 0;
    loop {
        let rip = a.machine.registers()?.rip;
        let mode = mode_of(&a.machine)?;
        let bytes = instruction_bytes(&a.machine, rip);
        keep_stepping(&mut a.machine, &bytes)?;
        keep_stepping(&mut b.machine, &bytes)?;
        let (oa, ob) = (
            a.machine.run(Some(step), &mut a.records)?,
            b.machine.run(Some(step), &mut b.records)?,
        );
        index += 1;
        let stepped = |o: &Outcome| matches!(o, Outcome::Debug(DebugStop::Step));
        if stepped(&oa) && stepped(&ob) {
            let registers = register_differences(&a.machine, &b.machine)?;
            if !registers.is_empty() {
                let (pid, process) = process_on_cpu(&a.machine);
                return Ok(WithinStep::Instruction(Instruction {
                    rip,
                    mode,
                    pid,
                    process,
                    host: crate::host_instructions::decode(&bytes),
                    bytes,
                    registers,
                    index,
                }));
            }
            if index >= INSTRUCTION_BUDGET {
                return Ok(WithinStep::OutOfBudget {
                    instructions: index,
                });
            }
            continue;
        }

        // The exit that ends the step, in one copy or both.
        a.machine.set_debug(Stepping::No, &[])?;
        b.machine.set_debug(Stepping::No, &[])?;
        let reached = (a.run_to(step)?, b.run_to(step)?);
        if reached.0 {
            a.capture()?;
        }
        if reached.1 {
            b.capture()?;
        }
        let (state, pages) = differences(&a, &b)?;
        return Ok(WithinStep::AtTheExit {
            instructions: index,
            state,
            pages,
        });
    }
}

/// Where to bisect after `replayed`: from the last keyframe that matched,
/// or where the replay started, to the first keyframe whose machine
/// differed, or else to the step of the first event or record that did.
/// None for a replay that came out the same.
pub fn bracket_of(replayed: &crate::run::Replayed) -> Option<Bracket> {
    if let Some(first) = &replayed.machine.first {
        return Some(Bracket {
            from: first.matched.or(replayed.keyframe),
            to: first.step,
        });
    }
    let d = replayed.divergence.as_ref()?;
    let to = d.left_step.min(d.right_step);
    let from = replayed
        .machine
        .compared
        .iter()
        .copied()
        .filter(|s| *s < to)
        .max()
        .or(replayed.keyframe);
    Some(Bracket { from, to })
}

/// Bisects `bracket` of `run`, trying up to `tries` times for copies that
/// differ.
pub fn bisect(run: &Run, home: &Home, bracket: Bracket, tries: u32) -> Result<Bisected> {
    let tries = tries.max(1);
    for attempt in 1..=tries {
        let Some(step) = first_step(run, home, bracket)? else {
            continue;
        };
        let within = within_step(run, home, bracket, step)
            .with_context(|| format!("single-stepping step {step}"))?;
        return Ok(Bisected::Found {
            bracket,
            step,
            within,
            tries: attempt,
        });
    }
    Ok(Bisected::Agreed { bracket, tries })
}

/// RFLAGS' trap flag, which single-stepping sets.
const RFLAGS_TF: u64 = 1 << 8;

/// What an instruction that loads RFLAGS loads it from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadsFlags {
    /// iretq: the frame on the stack, RFLAGS third.
    Iret,
    /// sysretq: R11.
    Sysret,
    /// popf: the top of the stack.
    Popf,
    No,
}

/// What `bytes`, an instruction, loads RFLAGS from.
fn loads_flags(bytes: &[u8]) -> LoadsFlags {
    let rest = match bytes.first() {
        Some(b) if b & 0xf0 == 0x40 => &bytes[1..],
        _ => bytes,
    };
    match rest {
        [0xcf, ..] => LoadsFlags::Iret,
        [0x0f, 0x07, ..] => LoadsFlags::Sysret,
        [0x9d, ..] => LoadsFlags::Popf,
        _ => LoadsFlags::No,
    }
}

/// Where in the stack iretq's frame keeps RFLAGS: after RIP and CS.
const IRET_RFLAGS: u64 = 16;

/// Sets the trap flag in the RFLAGS the instruction `bytes` is about to
/// load, so single-stepping goes on past it. KVM steps the vCPU by
/// setting TF, and an iretq, sysretq or popf loads flags saved before the
/// stepping began, without it: from there the vCPU would run unstepped to
/// the next exit. Both copies are given the same flag, so whatever it
/// changes it changes in both.
fn keep_stepping(machine: &mut Machine, bytes: &[u8]) -> Result<()> {
    let mut regs = machine.registers()?;
    let at = match loads_flags(bytes) {
        LoadsFlags::No => return Ok(()),
        LoadsFlags::Sysret => {
            regs.r11 |= RFLAGS_TF;
            return machine.set_registers(&regs);
        }
        LoadsFlags::Iret => regs.rsp + IRET_RFLAGS,
        LoadsFlags::Popf => regs.rsp,
    };
    let mut flags = [0u8; 8];
    if machine.read_virtual(at, &mut flags)? < flags.len() {
        return Ok(());
    }
    let flags = u64::from_le_bytes(flags) | RFLAGS_TF;
    machine.write_virtual(at, &flags.to_le_bytes())
}

/// Whether the vCPU is in user space or the kernel.
fn mode_of(machine: &Machine) -> Result<Mode> {
    Ok(if machine.special_registers()?.cs.dpl == USER_DPL {
        Mode::User
    } else {
        Mode::Kernel
    })
}

/// Up to an instruction's worth of bytes at `rip`.
fn instruction_bytes(machine: &Machine, rip: u64) -> Vec<u8> {
    let mut bytes = vec![0u8; MAX_INSTRUCTION];
    let n = machine.read_virtual(rip, &mut bytes).unwrap_or(0);
    bytes.truncate(n);
    bytes
}

/// The general purpose registers and flags that differ between the two
/// machines, the first machine's value on the left.
fn register_differences(a: &Machine, b: &Machine) -> Result<Vec<StateDifference>> {
    Ok(a.named_registers()?
        .into_iter()
        .zip(b.named_registers()?)
        .filter(|((_, x), (_, y))| x != y)
        .map(|((name, x), (_, y))| StateDifference {
            name: name.to_string(),
            left: format!("{x:#x}"),
            right: format!("{y:#x}"),
        })
        .collect())
}

/// The process on the CPU, by its id and name, when the VM's kernel says.
fn process_on_cpu(machine: &Machine) -> (Option<u32>, Option<String>) {
    let Ok(Some(layout)) = machine.task_layout() else {
        return (None, None);
    };
    match crate::threads::Tasks::new(machine, layout).current_thread() {
        Ok((pid, thread)) => (Some(pid), Some(thread.name)),
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    // The instructions single-stepping must give TF to, decoded from the
    // bytes a compiler and the kernel's entry code emit.
    use super::*;

    #[test]
    fn the_instructions_that_load_rflags_are_told_apart() {
        // iretq is 48 cf, sysretq 48 0f 07, popf 9d; a mov and a syscall
        // (0f 05) load nothing.
        assert_eq!(loads_flags(&[0x48, 0xcf]), LoadsFlags::Iret);
        assert_eq!(loads_flags(&[0x48, 0x0f, 0x07]), LoadsFlags::Sysret);
        assert_eq!(loads_flags(&[0x9d]), LoadsFlags::Popf);
        assert_eq!(loads_flags(&[0x48, 0x89, 0xc3]), LoadsFlags::No);
        assert_eq!(loads_flags(&[0x0f, 0x05]), LoadsFlags::No);
        assert_eq!(loads_flags(&[]), LoadsFlags::No);
    }
}
