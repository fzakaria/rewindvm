//! Debugging a forked machine: its registers, its memory as the VM's own
//! page tables map it, breakpoints, watchpoints and single steps. `rewind
//! gdb` serves these over the GDB remote protocol.
//!
//! Breakpoints and watchpoints are the CPU's four debug address registers
//! rather than a value compared after every step, so the VM runs at full
//! speed. A debug trap is a VM exit the VM never sees, and it is not a
//! step: a machine runs the same whether or not it is being debugged.
//! Breakpoints past the registers are int3 written into the VM's memory,
//! whose #BP KVM hands the monitor rather than the guest. Memory reads see
//! the byte int3 replaced. An int3 is in a physical page, and a page of
//! code is often mapped by many processes, so the debugger decides which
//! stops are its and lifts int3 for the others' one instruction, stepped
//! with interrupts held. An int3 of the guest's own goes back to the
//! guest. A process that reads the file a page of code belongs to sees
//! int3 in it, and the fork then goes its own way.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_GUESTDBG_BLOCKIRQ, KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW_BP,
    KVM_GUESTDBG_USE_SW_BP, Msrs, kvm_guest_debug, kvm_msr_entry, kvm_sregs,
};

use crate::Machine;

/// The vCPU's general purpose registers, pc and flags, as
/// [`Machine::registers`] reads them.
pub use kvm_bindings::kvm_regs;

/// The CPU has four debug address registers, DR0 to DR3, shared by
/// breakpoints and watchpoints.
pub const MAX_TRAPS: usize = 4;

/// DR7: the local enable bit of register i is bit 2i. Its condition and
/// length are four bits from bit 16 + 4i: two for the condition, then two
/// for how many bytes it watches.
const DR7_LOCAL_ENABLE: u64 = 1;
const DR7_BITS_PER_ENABLE: u32 = 2;
const DR7_CONDITIONS: u32 = 16;
const DR7_BITS_PER_CONDITION: u32 = 4;
const DR7_LEN_SHIFT: u32 = 2;
const DR7_INDEX: usize = 7;

/// DR7's condition bits: executing the address, writing it, or reading or
/// writing it. x86 cannot trap on reads alone.
const DR7_EXECUTE: u64 = 0b00;
const DR7_WRITE: u64 = 0b01;
const DR7_READ_WRITE: u64 = 0b11;

/// DR6 on a debug trap: which breakpoint matched (bits 0 to 3), or BS
/// (bit 14) for a single step.
const DR6_HIT_MASK: u64 = 0xf;
const DR6_SINGLE_STEP: u64 = 1 << 14;

/// RFLAGS' trap flag, which KVM sets to single-step the vCPU.
const RFLAGS_TF: u64 = 1 << 8;

/// RFLAGS' resume flag: the next instruction runs without its breakpoint
/// trapping, and the CPU clears it once that instruction has.
const RFLAGS_RF: u64 = 1 << 16;

/// The exception int3 raises.
const BP_VECTOR: u32 = 3;

const PAGE_SIZE: u64 = 4096;
const PAGE_SHIFT: u32 = 12;

/// A page table entry: present, a large page (in a page directory or the
/// table above it), and the physical address it points at, bits 12 to 51.
const PTE_PRESENT: u64 = 1;
const PTE_LARGE: u64 = 1 << 7;
const PTE_ADDRESS: u64 = 0x000f_ffff_ffff_f000;

/// Each level of the walk indexes its table with 9 bits of the address.
const INDEX_BITS: u32 = 9;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
const PTE_SIZE: u64 = 8;

/// CR4's LA57 bit: five levels of page tables rather than four.
const CR4_LA57: u64 = 1 << 12;

/// CR0's PG bit: paging is on.
const CR0_PG: u64 = 1 << 31;

/// How many levels of page tables translate an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Levels {
    Four,
    Five,
}

impl Levels {
    fn count(self) -> u32 {
        match self {
            Levels::Four => 4,
            Levels::Five => 5,
        }
    }

    /// Whether `address` is canonical for this many levels: every bit
    /// above the ones the tables index is a copy of the highest of those.
    fn canonical(self, address: u64) -> bool {
        let indexed = PAGE_SHIFT + INDEX_BITS * self.count();
        let above = (address as i64) >> (indexed - 1);
        above == 0 || above == -1
    }
}

/// Whether a debugger runs the machine one instruction at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stepping {
    No,
    Yes,
}

/// Which accesses a watchpoint traps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Write,
    ReadWrite,
}

/// What one debug address register traps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// Executing the instruction at this address.
    Execute(u64),
    /// An access to `len` bytes at `address`: 1, 2, 4 or 8 of them,
    /// aligned to that many. The trap comes after the access.
    Watch {
        address: u64,
        len: u64,
        access: Access,
    },
}

/// What a debugger has asked of the machine.
#[derive(Clone, Debug)]
pub(crate) struct Debugging {
    stepping: Stepping,
    traps: Vec<Trap>,
}

/// The int3 instruction, one byte long.
pub const INT3: u8 = 0xcc;

/// int3 written into the machine's memory for a debugger, by physical
/// address, with the bytes they replaced.
#[derive(Clone, Debug, Default)]
pub(crate) struct Int3s {
    pub(crate) replaced: std::collections::BTreeMap<u64, u8>,
    /// The one put back to its byte while the instruction there runs once.
    pub(crate) lifted: Option<u64>,
}

impl Int3s {
    /// Puts the bytes int3 replaced into `chunk`, just read from physical
    /// address `physical`.
    pub(crate) fn shadow(&self, physical: u64, chunk: &mut [u8]) {
        let end = physical + chunk.len() as u64;
        for (&at, &byte) in self.replaced.range(physical..end) {
            chunk[(at - physical) as usize] = byte;
        }
    }

    /// Readies `data` to be written at physical address `physical`: where
    /// int3 is written, the byte written becomes the one int3 replaced, and
    /// int3 stays in memory, unless it is lifted.
    pub(crate) fn write_through(&mut self, physical: u64, data: &mut [u8]) {
        let end = physical + data.len() as u64;
        let lifted = self.lifted;
        for (&at, byte) in self.replaced.range_mut(physical..end) {
            let i = (at - physical) as usize;
            *byte = data[i];
            if lifted != Some(at) {
                data[i] = INT3;
            }
        }
    }
}

/// Whose a #DB exit is: the debugger's, a step the machine took itself,
/// or a stray single step nobody took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbExit {
    Stop(DebugStop),
    Machine,
    Stray,
}

/// Whose a #DB exit with this DR6 is, given the debugger's `traps` and
/// whether the debugger and the machine are single-stepping. A watched
/// access is the debugger's even when a step trapped with it, since the
/// step's instruction made it; a breakpoint only when no step did, as it
/// traps again when the machine resumes. A single step is the debugger's
/// when it asked for one, else the machine's when it took one, else stray:
/// TF that a step left in a saved copy of RFLAGS.
fn db_exit(dr6: u64, traps: &[Trap], debugger: Stepping, machine: Stepping) -> DbExit {
    let hits = (0..MAX_TRAPS)
        .filter(|i| dr6 & DR6_HIT_MASK & (1 << i) != 0)
        .filter_map(|i| traps.get(i));

    // Every watch the access matched, the first by its address: one
    // instruction can touch the bytes of several.
    let mut first = None;
    let mut breakpoint = None;
    for trap in hits {
        match *trap {
            Trap::Watch { address, .. } => first = first.or(Some(address)),
            Trap::Execute(address) => breakpoint = breakpoint.or(Some(address)),
        }
    }
    if let Some(address) = first {
        let traps = trap_set(dr6, traps);
        return DbExit::Stop(DebugStop::Watchpoint { address, traps });
    }
    if dr6 & DR6_SINGLE_STEP != 0 {
        return match (debugger, machine) {
            (Stepping::Yes, _) => DbExit::Stop(DebugStop::Step),
            (Stepping::No, Stepping::Yes) => DbExit::Machine,
            (Stepping::No, Stepping::No) => DbExit::Stray,
        };
    }
    match breakpoint {
        Some(address) => DbExit::Stop(DebugStop::Breakpoint(address)),
        None => DbExit::Machine,
    }
}

/// Where a single step across the kernel's boundary ends, the debug
/// register that traps the address, and the flags of an `iretq` frame to
/// put back once it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Crossing {
    address: u64,
    register: usize,
    restore: Option<SavedFlags>,
    /// The write watch the register was borrowed from, when it was one,
    /// with its bytes as they were before the step.
    borrowed: Option<BorrowedWatch>,
}

/// A write watch whose register holds the trap at the kernel's exception
/// entry for a single step: the watch misses a write the stepped
/// instruction makes, so the bytes it watches are read before the step,
/// in the page table of `cr3`, and compared after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BorrowedWatch {
    address: u64,
    len: usize,
    cr3: u64,
    before: [u8; MAX_WATCH_LEN],
}

/// The most bytes one debug register watches.
const MAX_WATCH_LEN: usize = 8;

/// The flags an `iretq` frame held at `at`, a kernel stack address, before
/// their resume flag was cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedFlags {
    at: u64,
    flags: u64,
}

/// The exception a page fault raises. The CPU enters its handler, as any
/// exception's or interrupt's, with the trap flag cleared.
const PF_VECTOR: u64 = 14;

/// The size of a gate in the 64-bit IDT.
const GATE_SIZE: usize = 16;

/// The handler address an IDT gate holds: bits 0 to 15 in its bytes 0
/// and 1, 16 to 31 in bytes 6 and 7, and 32 to 63 in bytes 8 to 11.
fn gate_address(gate: &[u8; GATE_SIZE]) -> u64 {
    let low = u64::from(u16::from_le_bytes([gate[0], gate[1]]));
    let middle = u64::from(u16::from_le_bytes([gate[6], gate[7]]));
    let high = u64::from(u32::from_le_bytes([gate[8], gate[9], gate[10], gate[11]]));
    low | middle << 16 | high << 32
}

/// The bytes of an IDT stub's prologue before its call to the common
/// entry: endbr64, clac or the nop it is patched to when SMAP is off,
/// cld, and the push of -1 an exception with no error code makes.
const ENDBR64: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];
const CLAC: [u8; 3] = [0x0f, 0x01, 0xca];
const NOP3: [u8; 3] = [0x0f, 0x1f, 0x00];
const CLD: [u8; 1] = [0xfc];
const PUSH_MINUS_ONE: [u8; 2] = [0x6a, 0xff];

/// x86's call with a 32-bit displacement from the next instruction.
const CALL_REL32: u8 = 0xe8;
const CALL_LEN: usize = 5;

/// How many bytes of a stub are read for its prologue and call.
const STUB_BYTES: usize = 16;

/// The address the IDT stub at `stub`, whose first bytes are `code`, calls
/// after its prologue: in Linux, error_entry, which every exception's and
/// interrupt's stub calls but those of the exceptions with stacks of their
/// own, #DB, NMI, #DF and #MC. None when the stub calls nothing there.
fn common_entry(stub: u64, code: &[u8]) -> Option<u64> {
    stub_instructions(stub, code).map(|(_, target)| target)
}

/// The instructions of the IDT stub at `stub`, whose first bytes are
/// `code`, each where it starts, through its call, and the call's target.
/// None when the bytes are no stub's prologue and call.
fn stub_instructions(stub: u64, code: &[u8]) -> Option<(Vec<u64>, u64)> {
    let mut at = 0;
    let mut instructions = Vec::new();
    for prologue in [&ENDBR64[..], &CLAC, &NOP3, &CLD, &PUSH_MINUS_ONE] {
        if code[at..].starts_with(prologue) {
            instructions.push(stub + at as u64);
            at += prologue.len();
        }
    }
    let call = code.get(at..at + CALL_LEN)?;
    if call[0] != CALL_REL32 {
        return None;
    }
    instructions.push(stub + at as u64);
    let displacement = i32::from_le_bytes([call[1], call[2], call[3], call[4]]);
    let next = stub + (at + CALL_LEN) as u64;
    Some((
        instructions,
        next.wrapping_add_signed(i64::from(displacement)),
    ))
}

/// The stub whose call returns to `returns_to`, from `before`, the bytes
/// just before that address: the longest run of them, up to
/// [`STUB_BYTES`], that decodes as a stub's prologue and a call ending
/// there. Its instructions and the call's target, as
/// [`stub_instructions`] gives them.
fn stub_ending_at(returns_to: u64, before: &[u8]) -> Option<(Vec<u64>, u64)> {
    (CALL_LEN..=STUB_BYTES.min(before.len()))
        .rev()
        .find_map(|len| {
            let start = returns_to - len as u64;
            let (instructions, target) = stub_instructions(start, &before[before.len() - len..])?;
            let ends = *instructions.last()? + CALL_LEN as u64;
            (ends == returns_to).then_some((instructions, target))
        })
}

/// An external interrupt's vector stub: endbr64, a push of the vector as
/// an 8-bit immediate, and a jump to the common interrupt stub, by a 32-
/// or an 8-bit displacement.
const PUSH_IMM8: u8 = 0x6a;
const PUSH_IMM8_LEN: usize = 2;
const JMP_REL32: u8 = 0xe9;
const JMP_REL32_LEN: usize = 5;
const JMP_REL8: u8 = 0xeb;
const JMP_REL8_LEN: usize = 2;

/// The instructions of the vector stub at `stub`, whose first bytes are
/// `code`, each where it starts, when it jumps to `common`, the common
/// interrupt stub; None for other code.
fn vector_stub(stub: u64, code: &[u8], common: u64) -> Option<Vec<u64>> {
    let mut instructions = Vec::new();
    let mut at = 0;
    if code.starts_with(&ENDBR64) {
        instructions.push(stub);
        at += ENDBR64.len();
    }
    if *code.get(at)? != PUSH_IMM8 {
        return None;
    }
    instructions.push(stub + at as u64);
    at += PUSH_IMM8_LEN;
    instructions.push(stub + at as u64);
    let (len, displacement) = match *code.get(at)? {
        JMP_REL32 => {
            let bytes = code.get(at + 1..at + JMP_REL32_LEN)?;
            let displacement = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            (JMP_REL32_LEN, i64::from(displacement))
        }
        JMP_REL8 => (JMP_REL8_LEN, i64::from(*code.get(at + 1)? as i8)),
        _ => return None,
    };
    let next = stub + (at + len) as u64;
    (next.wrapping_add_signed(displacement) == common).then_some(instructions)
}

/// error_entry's first instructions, which trade the stub's return
/// address, pushed where pt_regs keeps rdi, for rdi: push rsi, load the
/// return address from above it into rsi, store rdi over it.
const PUSH_RSI: [u8; 1] = [0x56];
const LOAD_RSI: [u8; 5] = [0x48, 0x8b, 0x74, 0x24, 0x08];
const STORE_RDI: [u8; 5] = [0x48, 0x89, 0x7c, 0x24, 0x08];

/// Where in error_entry at `entry`, whose first bytes are `code`, rdi's
/// word of pt_regs holds rdi again: after the store over the return
/// address. None for other code.
fn rdi_saved_at(entry: u64, code: &[u8]) -> Option<u64> {
    let mut at = 0;
    for instruction in [&PUSH_RSI[..], &LOAD_RSI, &STORE_RDI] {
        if !code[at..].starts_with(instruction) {
            return None;
        }
        at += instruction.len();
    }
    Some(entry + at as u64)
}

/// The kernel's common exception entry, and where in it the word of
/// pt_regs that keeps rdi holds rdi again, when it decodes as Linux's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExceptionEntry {
    pub address: u64,
    pub rdi_saved_at: Option<u64>,
}

/// The debug register the trap at the kernel's return to user space
/// takes, among the debugger's `traps`: the one after them, else, with all
/// four theirs, the last that is not a read-write watch, borrowed until
/// the return. Single steps lose the return without the trap. The one
/// instruction it is borrowed for, `sysretq` or `iretq`, runs at a kernel
/// address and writes no memory, so neither a breakpoint in user space nor
/// a write watch misses a hit; `iretq` reads its frame, which a read-write
/// watch could see. None when every register is one.
fn return_register(traps: &[Trap]) -> Option<usize> {
    if traps.len() < MAX_TRAPS {
        return Some(traps.len());
    }
    traps.iter().rposition(|trap| {
        !matches!(
            trap,
            Trap::Watch {
                access: Access::ReadWrite,
                ..
            }
        )
    })
}

/// The debug register the trap at the kernel's entry for a `syscall` at
/// `at` takes, among the debugger's `traps`: the one after them, else,
/// with all four theirs, the last that is not a breakpoint at `at`,
/// borrowed until the kernel's entry. `syscall` reads and writes no
/// memory, so no watch misses a hit while it runs; a breakpoint at the
/// instruction itself traps before it runs, and keeps its register. None
/// when every register is one.
fn entry_register(traps: &[Trap], at: u64) -> Option<usize> {
    if traps.len() < MAX_TRAPS {
        return Some(traps.len());
    }
    traps.iter().rposition(|trap| *trap != Trap::Execute(at))
}

/// The debug register the trap at the kernel's exception entry takes for
/// a single step from `at`, among the debugger's `traps`, with
/// `crossing` the register a crossing holds: the first free one, else,
/// with all four the debugger's, the last that is not the crossing's, a
/// read-write watch or a breakpoint at `at`, borrowed for the step. A
/// write watch borrowed misses a write the stepped instruction makes,
/// which the machine finds by comparing the bytes it watches before and
/// after ([`BorrowedWatch`]); a breakpoint elsewhere traps before an
/// instruction, so a step that lands on it is one gdb sees land there. A
/// read-write watch would miss a read, which nothing can find after, and
/// a breakpoint at `at` keeps its register as at a `syscall`. None when
/// every register is one of those.
fn exception_register(traps: &[Trap], at: u64, crossing: Option<usize>) -> Option<usize> {
    if let Some(free) = (traps.len()..MAX_TRAPS).find(|&register| Some(register) != crossing) {
        return Some(free);
    }
    traps
        .iter()
        .rposition(|trap| {
            !matches!(
                trap,
                Trap::Watch {
                    access: Access::ReadWrite,
                    ..
                }
            ) && *trap != Trap::Execute(at)
        })
        .filter(|register| Some(*register) != crossing)
}

/// What KVM traps for the debugger, the monitor and the int3s together:
/// the debugger's debug registers and steps; with int3 written anywhere,
/// every #BP, so none reaches the guest unseen; and while the monitor
/// steps toward a preemption point or an int3 is lifted, single steps
/// with interrupts held, so each lands after its one instruction.
fn guest_debug(
    debugging: Option<&Debugging>,
    monitor: Stepping,
    approach: Stepping,
    crossings: &[Crossing],
    int3s: &Int3s,
) -> Result<kvm_guest_debug> {
    let mut debug = kvm_guest_debug::default();
    let mut traps = debugging.map_or_else(Vec::new, |d| d.traps.clone());
    if let Some(debugging) = debugging
        && debugging.stepping == Stepping::Yes
    {
        debug.control |= KVM_GUESTDBG_SINGLESTEP;
    }
    // The kernel's entry or return to user space the machine steps toward,
    // and the kernel's exception entry, take registers after the
    // debugger's, or one of theirs borrowed, which a trap there is not
    // the debugger's for.
    let mut crossings = crossings.to_vec();
    crossings.sort_by_key(|crossing| crossing.register);
    for crossing in crossings {
        let trap = Trap::Execute(crossing.address);
        match traps.get_mut(crossing.register) {
            Some(borrowed) => *borrowed = trap,
            None => traps.push(trap),
        }
    }
    if debugging.is_some() || !traps.is_empty() {
        debug.control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP;
        debug.arch.debugreg[DR7_INDEX] = dr7(&traps)?;
        for (i, trap) in traps.iter().enumerate() {
            debug.arch.debugreg[i] = match trap {
                Trap::Execute(address) | Trap::Watch { address, .. } => *address,
            };
        }
    }
    if !int3s.replaced.is_empty() {
        debug.control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP;
    }
    if monitor == Stepping::Yes || int3s.lifted.is_some() {
        debug.control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP | KVM_GUESTDBG_BLOCKIRQ;
    }
    // Closing in on a branch count steps as the debugger does, interrupts
    // let through, so each lands where it would have without the steps.
    if approach == Stepping::Yes {
        debug.control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP;
    }
    Ok(debug)
}

/// Why a debugged machine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugStop {
    /// One instruction ran.
    Step,
    /// Execution reached the breakpoint at this address.
    Breakpoint(u64),
    /// An instruction accessed watched bytes, and has run: the bytes the
    /// first of its watches is at, and every watch it matched.
    Watchpoint { address: u64, traps: TrapSet },
    /// The guest has retired the branches [`Machine::set_branch_stop`]
    /// asked for: the first instruction boundary at that count.
    Branches,
    /// The counter's interrupt came so late that the guest had retired
    /// more branches than asked for when the machine looked; closing in
    /// again with a wider margin can reach the count.
    PassedBranches,
}

/// Some of a debugger's traps, by their place in the list it set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrapSet(u8);

impl TrapSet {
    /// Whether the trap at `index` is in the set.
    pub fn contains(self, index: usize) -> bool {
        index < MAX_TRAPS && self.0 & (1 << index) != 0
    }
}

/// The watches among `traps` whose bits DR6 has set.
pub fn trap_set(dr6: u64, traps: &[Trap]) -> TrapSet {
    let mut set = TrapSet::default();
    for (i, trap) in traps.iter().enumerate().take(MAX_TRAPS) {
        if dr6 & DR6_HIT_MASK & (1 << i) != 0 && matches!(trap, Trap::Watch { .. }) {
            set.0 |= 1 << i;
        }
    }
    set
}

/// A debugger's stop at a branch count: the count, how many branches
/// short of it the counter's interrupt is asked for, and how close the
/// machine is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BranchStop {
    target: u64,
    margin: u64,
    approach: Approach,
}

/// The kernel's returns to user space: `sysretq` goes to RCX, `iretq` to
/// the frame on its stack, the address then the code segment.
const SYSRETQ: [u8; 3] = [0x48, 0x0f, 0x07];
const IRETQ: [u8; 2] = [0x48, 0xcf];
const WORD: usize = 8;

/// The words of an `iretq` frame read: the address, the code segment and
/// the flags.
const IRET_FRAME_WORDS: usize = 3;

/// The way user space enters the kernel for a system call: `syscall`
/// jumps to LSTAR from 64-bit code and to CSTAR from 32-bit code, with the
/// flags FMASK leaves, which Linux sets to clear the trap flag.
const SYSCALL: [u8; 2] = [0x0f, 0x05];
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;

/// The privilege level in a code segment selector's low bits: 3 in user
/// space.
const SELECTOR_RPL: u16 = 0b11;
const USER_RPL: u16 = 0b11;

/// How a machine closes in on a branch count: far from it, running with
/// the counter's interrupt armed a margin short of it, or single-stepping
/// the rest of the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Approach {
    Far,
    Armed,
    Stepping,
}

impl Approach {
    /// The approach from here with `left` branches to go to the count,
    /// more than none, and how many branches from now to arm the
    /// counter's interrupt for, if it is to be armed: `margin` short of
    /// the count while the count is further off than that, and single
    /// steps from there. An armed interrupt is armed again, for the count
    /// as it is now: the interrupt fires once, and one that came early, as
    /// on a hybrid host when the thread moved to another kind of core,
    /// would leave the guest running unwatched to its next exit.
    fn closer(self, left: u64, margin: u64) -> (Approach, Option<u64>) {
        match self {
            Approach::Stepping => (Approach::Stepping, None),
            Approach::Far | Approach::Armed if left <= margin => (Approach::Stepping, None),
            Approach::Far | Approach::Armed => (Approach::Armed, Some(left - margin)),
        }
    }
}

impl Machine {
    /// Turns debugging on with these traps, one per debug address register,
    /// stepping one instruction at a time when asked. With debugging on,
    /// `run` returns [`crate::Outcome::Debug`] at every trap that is the
    /// debugger's, an int3 of [`Machine::insert_int3`] among them.
    ///
    /// Interrupts are not held off while the debugger steps, though a step
    /// then lands in an interrupt handler now and then: one held off is
    /// taken later than when the run was recorded, and the machine goes
    /// another way from there.
    pub fn set_debug(&mut self, stepping: Stepping, traps: &[Trap]) -> Result<()> {
        self.debugging = Some(Debugging {
            stepping,
            traps: traps.to_vec(),
        });
        self.exception_trap = None;
        self.apply_guest_debug()
    }

    /// Turns debugging off. int3s still written keep their #BPs trapped.
    /// The trap at the kernel's exception entry, whose register came after
    /// the debugger's traps as they were, goes too, as with new traps: the
    /// next single step chooses it again.
    pub fn clear_debug(&mut self) -> Result<()> {
        self.debugging = None;
        self.exception_trap = None;
        self.apply_guest_debug()
    }

    /// Tells KVM what to trap for the debugger, the monitor's steps toward
    /// a preemption point and the int3s together, after any of them
    /// changes.
    pub(crate) fn apply_guest_debug(&mut self) -> Result<()> {
        let monitor = match &self.work {
            Some(work) if work.stepping => Stepping::Yes,
            _ => Stepping::No,
        };
        let debug = guest_debug(
            self.debugging.as_ref(),
            monitor,
            self.approach_stepping(),
            &[self.crossing, self.exception_trap]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            &self.int3s,
        )?;
        Ok(self.vcpu.set_guest_debug(&debug)?)
    }

    /// Whether the machine single-steps toward a debugger's branch count.
    fn approach_stepping(&self) -> Stepping {
        match self.branch_stop {
            Some(BranchStop {
                approach: Approach::Stepping,
                ..
            }) => Stepping::Yes,
            _ => Stepping::No,
        }
    }

    /// Guest branches retired so far, exactly, under counter time; None
    /// under exit time, which counts none.
    pub fn branch_count(&self) -> Result<Option<u64>> {
        if !matches!(self.config.clock, crate::ClockSource::Branches(_)) {
            return Ok(None);
        }
        Ok(Some(match &self.work {
            Some(work) => work.count()?,
            None => self.work_base,
        }))
    }

    /// Stops `run` with [`DebugStop::Branches`] at the first instruction
    /// boundary where the guest has retired `target` branches, or, with
    /// None, no longer. The counter's interrupt is asked for `margin`
    /// branches short of the count, and the machine single-steps the
    /// rest; an interrupt later than that stops `run` with
    /// [`DebugStop::PassedBranches`]. Only under counter time, and not
    /// with preemption at branch counts, whose timer needs the interrupt.
    pub fn set_branch_stop(&mut self, target: Option<u64>, margin: u64) -> Result<()> {
        if target.is_some() {
            if !matches!(self.config.clock, crate::ClockSource::Branches(_)) {
                bail!("stopping at a branch count needs counter time");
            }
            if self.config.preemption == crate::Preemption::AtBranchCounts {
                bail!(
                    "stopping at a branch count needs the counter's interrupt, which preemption at branch counts takes"
                );
            }
        }
        self.branch_stop = target.map(|target| BranchStop {
            target,
            margin,
            approach: Approach::Far,
        });
        self.apply_guest_debug()
    }

    /// Closes in on the debugger's branch count before the guest runs:
    /// the stop once the count is reached or passed, else the counter's
    /// interrupt armed a margin short of it, each time the guest is about
    /// to run that far off, or single steps within the margin.
    pub(crate) fn close_in_on_branches(&mut self) -> Result<Option<DebugStop>> {
        let (Some(stop), Some(work)) = (self.branch_stop, &mut self.work) else {
            return Ok(None);
        };
        let count = work.count()?;
        if count >= stop.target {
            self.branch_stop = None;
            self.apply_guest_debug()?;
            return Ok(Some(if count == stop.target {
                DebugStop::Branches
            } else {
                DebugStop::PassedBranches
            }));
        }
        let left = stop.target - count;
        let (approach, arm_for) = stop.approach.closer(left, stop.margin);
        if let Some(branches) = arm_for {
            work.overflow.arm(branches)?;
        }
        if approach != stop.approach {
            self.branch_stop = Some(BranchStop { approach, ..stop });
            if approach == Approach::Stepping {
                self.apply_guest_debug()?;
            }
        }
        Ok(None)
    }

    /// The debugger's traps, one per debug register, or none.
    fn debugger_traps(&self) -> &[Trap] {
        self.debugging.as_ref().map_or(&[], |d| &d.traps)
    }

    /// Whether the machine single-steps, for the debugger or toward its
    /// branch count.
    fn single_stepping(&self) -> bool {
        let debugger = self
            .debugging
            .as_ref()
            .is_some_and(|d| d.stepping == Stepping::Yes);
        debugger || self.approach_stepping() == Stepping::Yes
    }

    /// Keeps single steps going across the kernel's boundary, before the
    /// vCPU runs. A `syscall` enters the kernel with the trap flag cleared,
    /// and so does every exception and interrupt, in user space or the
    /// kernel, so the kernel would run on unstepped, across exits and the
    /// steps they end, until something set the flag again. The kernel
    /// returns to user space with the flags its entry saved, which have no
    /// trap flag when the steps began in the kernel, so the user's code
    /// would run on unstepped. So at a `syscall` in user space a debug
    /// register traps the kernel's entry, and at a `sysretq` or an `iretq`
    /// to user space the address it returns to, where the step ends: the
    /// register after the debugger's, or one of theirs borrowed until then
    /// ([`entry_register`], [`return_register`]). Any instruction may fault
    /// or meet an interrupt, so the kernel's exception entry is trapped too
    /// ([`Machine::exception_entry`]), in a free register, else in one
    /// borrowed from the debugger ([`exception_register`]): a step then
    /// stops at the entry whether or not a register was free, as the
    /// replay that goes back over it, with every register free, does.
    /// With every register one the debugger cannot lend, the step runs on
    /// through the handler.
    pub(crate) fn step_across_the_boundary(&mut self) -> Result<()> {
        if !self.single_stepping() {
            return Ok(());
        }
        let regs = self.vcpu.get_regs()?;
        let sregs = self.vcpu.get_sregs()?;
        self.stepped_from = regs.rip;
        let mut changed = false;
        // A crossing takes the register after the debugger's, so the trap
        // at the exception entry, if one is left from a step that ended
        // without a #DB, is chosen again after it.
        if self.crossing.is_none() {
            self.crossing = if sregs.cs.selector & SELECTOR_RPL == USER_RPL {
                self.kernel_entry(&regs, &sregs)?
            } else {
                self.user_return(&regs)?
            };
            if self.crossing.is_some() {
                self.exception_trap = None;
                changed = true;
            }
        }
        if self.exception_trap.is_none() {
            self.exception_trap = self.exception_entry(&regs, &sregs)?;
            changed |= self.exception_trap.is_some();
        }
        if changed {
            self.apply_guest_debug()?;
        }

        // A write watch the trap borrowed from: its bytes before this step,
        // read anew for each step the trap stays set for.
        if let Some(mut borrowed) = self.exception_trap.and_then(|trap| trap.borrowed) {
            borrowed.cr3 = sregs.cr3;
            self.read_virtual(borrowed.address, &mut borrowed.before[..borrowed.len])?;
            if let Some(trap) = &mut self.exception_trap {
                trap.borrowed = Some(borrowed);
            }
        }
        Ok(())
    }

    /// Whether the stepped instruction wrote the bytes of the write watch
    /// the trap at the exception entry borrowed its register from, which
    /// the register missed: the bytes differ from before the step, read in
    /// the same page table. A step that moved to another page table, or
    /// whose bytes cannot be read, wrote nothing the watch can see.
    fn borrowed_watch_written(&self) -> Result<Option<(u64, usize)>> {
        let Some(trap) = &self.exception_trap else {
            return Ok(None);
        };
        let Some(borrowed) = &trap.borrowed else {
            return Ok(None);
        };
        if self.vcpu.get_sregs()?.cr3 != borrowed.cr3 {
            return Ok(None);
        }
        let mut after = [0u8; MAX_WATCH_LEN];
        if self.read_virtual(borrowed.address, &mut after[..borrowed.len])? < borrowed.len {
            return Ok(None);
        }
        if after[..borrowed.len] == borrowed.before[..borrowed.len] {
            return Ok(None);
        }
        Ok(Some((borrowed.address, trap.register)))
    }

    /// Where a single step from user space at `regs`' pc enters the kernel,
    /// when the instruction there is `syscall`: the address in LSTAR from
    /// 64-bit code, in CSTAR from 32-bit code.
    fn kernel_entry(&self, regs: &kvm_regs, sregs: &kvm_sregs) -> Result<Option<Crossing>> {
        let mut code = [0u8; SYSCALL.len()];
        self.read_virtual(regs.rip, &mut code)?;
        if code != SYSCALL {
            return Ok(None);
        }
        let Some(register) = entry_register(self.debugger_traps(), regs.rip) else {
            return Ok(None);
        };
        let msr = if sregs.cs.l != 0 {
            MSR_LSTAR
        } else {
            MSR_CSTAR
        };
        let address = self.read_msr(msr)?;
        Ok(Some(Crossing {
            address,
            register,
            restore: None,
            borrowed: None,
        }))
    }

    /// Where a single step enters the kernel when its instruction faults
    /// or an interrupt comes first: the entry the IDT's stubs share, found
    /// from the page fault's stub ([`common_entry`]), else that stub, in
    /// the register [`exception_register`] chooses. None with no register
    /// to be had, before the kernel has set up its IDT, or at the entry's
    /// first instruction, where the trap would come before the instruction
    /// runs, again and again.
    fn exception_entry(&self, regs: &kvm_regs, sregs: &kvm_sregs) -> Result<Option<Crossing>> {
        let crossing = self.crossing.map(|crossing| crossing.register);
        let Some(register) = exception_register(self.debugger_traps(), regs.rip, crossing) else {
            return Ok(None);
        };
        let Some(address) = self.common_entry_address(sregs)? else {
            return Ok(None);
        };
        if address == regs.rip {
            return Ok(None);
        }
        let borrowed = match self.debugger_traps().get(register) {
            Some(Trap::Watch {
                address,
                len,
                access: Access::Write,
            }) => Some(BorrowedWatch {
                address: *address,
                len: *len as usize,
                cr3: sregs.cr3,
                before: [0; MAX_WATCH_LEN],
            }),
            _ => None,
        };
        Ok(Some(Crossing {
            address,
            register,
            restore: None,
            borrowed,
        }))
    }

    /// The entry the IDT's stubs share, found from the page fault's stub
    /// ([`common_entry`]), else that stub. None before the kernel has set
    /// up its IDT.
    fn common_entry_address(&self, sregs: &kvm_sregs) -> Result<Option<u64>> {
        let end = (PF_VECTOR + 1) * GATE_SIZE as u64;
        if u64::from(sregs.idt.limit) < end - 1 {
            return Ok(None);
        }
        let mut gate = [0u8; GATE_SIZE];
        let at = sregs.idt.base + PF_VECTOR * GATE_SIZE as u64;
        if self.read_virtual(at, &mut gate)? < GATE_SIZE {
            return Ok(None);
        }
        let stub = gate_address(&gate);
        let mut code = [0u8; STUB_BYTES];
        let read = self.read_virtual(stub, &mut code)?;
        Ok(Some(common_entry(stub, &code[..read]).unwrap_or(stub)))
    }

    /// The kernel's common exception entry, as a debugger reading a
    /// thread's saved registers needs it, or None before the kernel has
    /// set up its IDT.
    pub fn exception_entry_point(&self) -> Result<Option<ExceptionEntry>> {
        let Some(address) = self.common_entry_address(&self.vcpu.get_sregs()?)? else {
            return Ok(None);
        };
        let mut code = [0u8; STUB_BYTES];
        let read = self.read_virtual(address, &mut code)?;
        Ok(Some(ExceptionEntry {
            address,
            rdi_saved_at: rdi_saved_at(address, &code[..read]),
        }))
    }

    /// The instructions the last single step ran without a trap of their
    /// own, in order: those of the IDT stub between an instruction that
    /// faulted or met an interrupt and the trap at the common entry,
    /// which the CPU ran with the trap flag it cleared. Empty for any
    /// other step.
    pub fn skipped(&self) -> &[u64] {
        &self.skipped
    }

    /// The stub instructions a step that the trap at the common entry
    /// ended ran unstepped: the stub's, found from the return address its
    /// call left on the stack, unless the step began in the stub, at its
    /// call. An external interrupt's vector stub runs first, pushing its
    /// vector where pt_regs keeps orig_rax and jumping to the common
    /// interrupt stub; the vector's gate in the IDT names it.
    fn stub_skipped(&self) -> Result<Vec<u64>> {
        let regs = self.vcpu.get_regs()?;
        let mut word = [0u8; WORD];
        if self.read_virtual(regs.rsp, &mut word)? < WORD {
            return Ok(Vec::new());
        }
        let returns_to = u64::from_le_bytes(word);
        let mut before = [0u8; STUB_BYTES];
        let start = returns_to - STUB_BYTES as u64;
        if self.read_virtual(start, &mut before)? < STUB_BYTES {
            return Ok(Vec::new());
        }
        let Some((instructions, target)) = stub_ending_at(returns_to, &before) else {
            return Ok(Vec::new());
        };
        let mut instructions = match self.vector_stub_before(regs.rsp, instructions[0])? {
            Some(mut vector) => {
                vector.extend(instructions);
                vector
            }
            None => instructions,
        };
        if target != regs.rip || instructions.contains(&self.stepped_from) {
            instructions.clear();
        }
        Ok(instructions)
    }

    /// The instructions of the vector stub that jumped to `common`, a stub
    /// that calls the common entry, from the vector it pushed, the word
    /// above the return address at `rsp`: the stub its gate in the IDT
    /// names, when that stub jumps to `common`.
    fn vector_stub_before(&self, rsp: u64, common: u64) -> Result<Option<Vec<u64>>> {
        let mut word = [0u8; WORD];
        if self.read_virtual(rsp + WORD as u64, &mut word)? < WORD {
            return Ok(None);
        }
        let vector = u64::from(u64::from_le_bytes(word) as u8);
        let sregs = self.vcpu.get_sregs()?;
        let end = (vector + 1) * GATE_SIZE as u64;
        if u64::from(sregs.idt.limit) < end - 1 {
            return Ok(None);
        }
        let mut gate = [0u8; GATE_SIZE];
        if self.read_virtual(sregs.idt.base + vector * GATE_SIZE as u64, &mut gate)? < GATE_SIZE {
            return Ok(None);
        }
        let stub = gate_address(&gate);
        let mut code = [0u8; STUB_BYTES];
        let read = self.read_virtual(stub, &mut code)?;
        Ok(vector_stub(stub, &code[..read], common))
    }

    /// Where a single step from the kernel at `regs`' pc returns to user
    /// space, when the instruction there is `sysretq`, which returns to
    /// RCX, or an `iretq` whose frame returns to user space. A fault's
    /// frame has the resume flag set, which would let the instruction it
    /// returns to run past the trap there; the flag is cleared in the
    /// frame, and put back once the step is done ([`Machine::end_crossing`]).
    /// The flag does nothing else: the guest's own debug registers are not
    /// in use while the debugger's are, and `pushf` never saves it.
    fn user_return(&mut self, regs: &kvm_regs) -> Result<Option<Crossing>> {
        let Some(register) = return_register(self.debugger_traps()) else {
            return Ok(None);
        };
        let mut code = [0u8; SYSRETQ.len()];
        self.read_virtual(regs.rip, &mut code)?;
        if code == SYSRETQ {
            return Ok(Some(Crossing {
                address: regs.rcx,
                register,
                restore: None,
                borrowed: None,
            }));
        }
        if code[..IRETQ.len()] != IRETQ {
            return Ok(None);
        }

        // The frame: the address, the code segment, then the flags.
        let mut frame = [0u8; IRET_FRAME_WORDS * WORD];
        if self.read_virtual(regs.rsp, &mut frame)? < frame.len() {
            return Ok(None);
        }
        let word = |i: usize| {
            u64::from_le_bytes(
                frame[i * WORD..(i + 1) * WORD]
                    .try_into()
                    .expect("one word"),
            )
        };
        let (address, cs, flags) = (word(0), word(1), word(2));
        if cs as u16 & SELECTOR_RPL != USER_RPL {
            return Ok(None);
        }
        let mut restore = None;
        if flags & RFLAGS_RF != 0 {
            let at = regs.rsp + (2 * WORD) as u64;
            self.write_virtual(at, &(flags & !RFLAGS_RF).to_le_bytes())?;
            restore = Some(SavedFlags { at, flags });
        }
        Ok(Some(Crossing {
            address,
            register,
            restore,
            borrowed: None,
        }))
    }

    /// Ends the crossing a single step went through, or no longer needs:
    /// its register is the debugger's again, and an `iretq` frame's flags
    /// are as the guest left them. A trap at the exception entry in the
    /// register after it moves down into its place: KVM is handed the
    /// traps in a row, so it would be there anyway, and a hit would set
    /// that register's bit of DR6, not the one after.
    fn end_crossing(&mut self) -> Result<()> {
        if let Some(SavedFlags { at, flags }) = self.crossing.take().and_then(|c| c.restore) {
            self.write_virtual(at, &flags.to_le_bytes())?;
        }
        let first_free = self.debugger_traps().len();
        if let Some(trap) = &mut self.exception_trap {
            trap.register = trap.register.min(first_free);
        }
        Ok(())
    }

    /// The vCPU's model-specific register `index`.
    fn read_msr(&self, index: u32) -> Result<u64> {
        let entry = kvm_msr_entry {
            index,
            ..Default::default()
        };
        let mut msrs = Msrs::from_entries(&[entry])?;
        if self.vcpu.get_msrs(&mut msrs)? != 1 {
            bail!("KVM did not read MSR {index:#x}");
        }
        Ok(msrs.as_slice()[0].data)
    }

    /// The counter's interrupt was disarmed, at a step or as `run` returns:
    /// closing in arms it again.
    pub(crate) fn branch_interrupt_disarmed(&mut self) {
        if let Some(stop) = &mut self.branch_stop
            && stop.approach == Approach::Armed
        {
            stop.approach = Approach::Far;
        }
    }

    /// What a debug exit means for the debugger, once the machine has done
    /// its own part. A #BP at an int3 the debugger wrote stops at it; any
    /// other #BP is the guest's own int3 and goes back to the guest. A
    /// single step puts a lifted int3 back. A stray single step's TF is
    /// cleared, so the guest runs on as it did when the run was recorded:
    /// KVM steps the vCPU by setting TF, and an instruction that saves
    /// RFLAGS while it is set, `syscall` into R11, `pushf`, or an
    /// interrupt's frame, keeps a copy that sets TF again when the guest
    /// restores it. The trap comes after the instruction has run, and KVM
    /// keeps it from the guest.
    pub(crate) fn debug_exit(
        &mut self,
        exception: u32,
        pc: u64,
        dr6: u64,
    ) -> Result<Option<DebugStop>> {
        if exception == BP_VECTOR {
            let written = self.translate(pc)?.is_some_and(|physical| {
                self.int3s.replaced.contains_key(&physical) && self.int3s.lifted != Some(physical)
            });
            if written {
                return Ok(Some(DebugStop::Breakpoint(pc)));
            }
            self.reinject_breakpoint()?;
            return Ok(None);
        }

        // A #DB: the machine's own steps first, toward a preemption point
        // or a debugger's branch count.
        let mut machine = match (&self.work, self.approach_stepping()) {
            (Some(work), _) if work.stepping => Stepping::Yes,
            (_, Stepping::Yes) => Stepping::Yes,
            _ => Stepping::No,
        };
        if dr6 & DR6_SINGLE_STEP != 0
            && let Some(physical) = self.int3s.lifted.take()
        {
            if self.int3s.replaced.contains_key(&physical) {
                self.dev.ram.write(physical, &[INT3])?;
            }
            self.apply_guest_debug()?;
            machine = Stepping::Yes;
        }

        // The kernel's entry, its exception entry or its return to user
        // space a step went through, trapped in a register after the
        // debugger's or one borrowed from them: the debugger's step ends
        // there, the registers are theirs again, and the machine's own
        // steps go on.
        let debugger = self.debugging.as_ref().map_or(Stepping::No, |d| d.stepping);
        let hit = |crossing: Option<Crossing>| {
            crossing.is_some_and(|crossing| dr6 & (1 << crossing.register) != 0)
        };
        if hit(self.crossing) || hit(self.exception_trap) {
            if hit(self.exception_trap) {
                self.skipped = self.stub_skipped()?;
            }
            self.end_crossing()?;
            self.exception_trap = None;
            self.apply_guest_debug()?;
            return Ok((debugger == Stepping::Yes).then_some(DebugStop::Step));
        }

        // A single step that ends a crossing it did not need: an
        // instruction that stepped with no exception or interrupt leaves
        // the trap at the exception entry unused, and an `iretq` whose frame
        // has the trap flag steps to where it returns before the trap there
        // can fire, which would then stop the next step at the same place.
        if dr6 & DR6_SINGLE_STEP != 0 {
            let returned = self.crossing.is_some_and(|crossing| crossing.address == pc);
            if returned {
                self.end_crossing()?;
            }
            if returned || self.exception_trap.take().is_some() {
                self.apply_guest_debug()?;
            }
        }

        // An instruction that loads RFLAGS, `iretq` back into the kernel
        // or `popf`, still traps once it has run, but leaves the flags it
        // loaded: with no trap flag when they were saved before the steps
        // began, as an interrupt's frame or irqsave's flags often are, the
        // instructions after it would run unstepped. So each step asks KVM
        // for steps again, which sets the flag where it is clear.
        let stepping = debugger == Stepping::Yes || machine == Stepping::Yes;
        if dr6 & DR6_SINGLE_STEP != 0 && stepping {
            self.apply_guest_debug()?;
        }

        match db_exit(dr6, self.debugger_traps(), debugger, machine) {
            // A step's instruction may have written what the watch whose
            // register the exception entry's trap borrowed watches.
            DbExit::Stop(DebugStop::Step) => {
                if let Some((address, register)) = self.borrowed_watch_written()? {
                    let mut traps = TrapSet::default();
                    traps.0 |= 1 << register;
                    return Ok(Some(DebugStop::Watchpoint { address, traps }));
                }
                Ok(Some(DebugStop::Step))
            }
            DbExit::Stop(stop) => Ok(Some(stop)),
            DbExit::Machine => Ok(None),
            DbExit::Stray => {
                let mut regs = self.vcpu.get_regs()?;
                regs.rflags &= !RFLAGS_TF;
                self.vcpu.set_regs(&regs)?;
                Ok(None)
            }
        }
    }

    /// Gives the guest the #BP of an int3 that is its own, which KVM keeps
    /// from it while the debugger has int3 written anywhere: the guest
    /// takes it as it would have without the debugger.
    fn reinject_breakpoint(&mut self) -> Result<()> {
        let mut events = self.vcpu.get_vcpu_events()?;
        events.exception.injected = 1;
        events.exception.nr = BP_VECTOR as u8;
        events.exception.has_error_code = 0;
        events.exception.error_code = 0;
        Ok(self.vcpu.set_vcpu_events(&events)?)
    }

    /// Writes int3 over the byte at physical address `physical`, for a
    /// breakpoint past the debug registers. The guest's #BP there stops
    /// `run` for the debugger, in whichever process reaches it, and reads
    /// of the machine's memory see the byte rather than int3.
    pub fn insert_int3(&mut self, physical: u64) -> Result<()> {
        if self.int3s.replaced.contains_key(&physical) {
            return Ok(());
        }
        let mut byte = [0u8];
        self.dev.ram.read(physical, &mut byte)?;
        self.dev.ram.write(physical, &[INT3])?;
        self.int3s.replaced.insert(physical, byte[0]);
        self.apply_guest_debug()
    }

    /// Puts back the byte int3 replaced at physical address `physical`.
    pub fn remove_int3(&mut self, physical: u64) -> Result<()> {
        let Some(byte) = self.int3s.replaced.remove(&physical) else {
            return Ok(());
        };
        if self.int3s.lifted == Some(physical) {
            self.int3s.lifted = None;
        }
        self.dev.ram.write(physical, &[byte])?;
        self.apply_guest_debug()
    }

    /// Lets the instruction at the breakpoint the machine stopped at run
    /// once without stopping there, as another process reaching it must:
    /// an int3 is lifted, its byte put back while the machine single-steps
    /// that instruction with interrupts held, then written again; a debug
    /// register's breakpoint is passed with RF.
    pub fn pass_breakpoint(&mut self) -> Result<()> {
        let mut regs = self.vcpu.get_regs()?;
        if let Some(physical) = self.translate(regs.rip)?
            && let Some(&byte) = self.int3s.replaced.get(&physical)
        {
            self.dev.ram.write(physical, &[byte])?;
            self.int3s.lifted = Some(physical);
            return self.apply_guest_debug();
        }
        // KVM hides the trap flag of its single steps from the flags read
        // above and puts it back only where the steps were asked for, so
        // the steps are asked for again here.
        regs.rflags |= RFLAGS_RF;
        self.vcpu.set_regs(&regs)?;
        self.apply_guest_debug()
    }

    /// The general purpose registers, the instruction pointer and flags.
    pub fn registers(&self) -> Result<kvm_regs> {
        Ok(self.vcpu.get_regs()?)
    }

    /// The general purpose registers, the instruction pointer and flags,
    /// by name, in the order KVM keeps them.
    pub fn named_registers(&self) -> Result<[(&'static str, u64); 18]> {
        let r = self.vcpu.get_regs()?;
        Ok([
            ("rax", r.rax),
            ("rbx", r.rbx),
            ("rcx", r.rcx),
            ("rdx", r.rdx),
            ("rsi", r.rsi),
            ("rdi", r.rdi),
            ("rsp", r.rsp),
            ("rbp", r.rbp),
            ("r8", r.r8),
            ("r9", r.r9),
            ("r10", r.r10),
            ("r11", r.r11),
            ("r12", r.r12),
            ("r13", r.r13),
            ("r14", r.r14),
            ("r15", r.r15),
            ("rip", r.rip),
            ("rflags", r.rflags),
        ])
    }

    pub fn set_registers(&mut self, regs: &kvm_regs) -> Result<()> {
        Ok(self.vcpu.set_regs(regs)?)
    }

    /// Segments, control registers and the like.
    pub fn special_registers(&self) -> Result<kvm_sregs> {
        Ok(self.vcpu.get_sregs()?)
    }

    /// Reads the VM's memory at a virtual address, as the VM's current page
    /// tables map it: the kernel's, and the user space of whichever process
    /// is running. Returns how many bytes could be read, which stops short
    /// at the first unmapped page.
    pub fn read_virtual(&self, address: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let Some(physical) = self.translate(address + done as u64)? else {
                break;
            };
            let n = page_remainder(address + done as u64).min(buf.len() - done);
            if self.read_ram(physical, &mut buf[done..done + n]).is_err() {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// Reads the VM's memory at a physical address as the guest wrote it:
    /// where the debugger wrote int3, the byte int3 replaced.
    fn read_ram(&self, physical: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.ram.read(physical, buf)?;
        self.int3s.shadow(physical, buf);
        Ok(())
    }

    /// Reads the VM's memory at a virtual address as the page table at
    /// physical address `page_table` maps it, whichever process is
    /// running: a process's memory while another runs. Returns how many
    /// bytes could be read, which stops short at the first unmapped page.
    pub fn read_virtual_in(&self, page_table: u64, address: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let at = address + done as u64;
            let Some(physical) = self.physical_in(page_table, at)? else {
                break;
            };
            let n = page_remainder(at).min(buf.len() - done);
            if self.read_ram(physical, &mut buf[done..done + n]).is_err() {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// The physical address behind a virtual one as the page table at
    /// physical address `page_table` maps it, if it is mapped.
    pub fn physical_in(&self, page_table: u64, address: u64) -> Result<Option<u64>> {
        let levels = if self.vcpu.get_sregs()?.cr4 & CR4_LA57 != 0 {
            Levels::Five
        } else {
            Levels::Four
        };
        let read = |physical: u64| {
            let mut entry = [0u8; PTE_SIZE as usize];
            self.dev.ram.read(physical, &mut entry).ok()?;
            Some(u64::from_le_bytes(entry))
        };
        Ok(walk(read, page_table, address, levels))
    }

    /// The user addresses the page table at physical address `page_table`
    /// maps to each of `frames`, physical page addresses, as (frame,
    /// virtual address) in address order: where in a process the guest
    /// memory at those frames is.
    pub fn mappings_in(
        &self,
        page_table: u64,
        frames: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<(u64, u64)>> {
        let levels = if self.vcpu.get_sregs()?.cr4 & CR4_LA57 != 0 {
            Levels::Five
        } else {
            Levels::Four
        };
        let read = |physical: u64| {
            let mut entry = [0u8; PTE_SIZE as usize];
            self.dev.ram.read(physical, &mut entry).ok()?;
            Some(u64::from_le_bytes(entry))
        };
        Ok(mappings(read, page_table, levels, frames))
    }

    /// Reads the VM's memory at a physical address, as the guest wrote it.
    pub fn read_physical(&self, physical: u64, buf: &mut [u8]) -> Result<()> {
        self.read_ram(physical, buf)
    }

    /// What the kernel published about its tasks at setup, or None from a
    /// kernel that predates it or before setup.
    pub fn task_layout(&self) -> Result<Option<crate::pv::TaskLayout>> {
        let Some(shared) = self.dev.shared else {
            return Ok(None);
        };
        let read = |at: u64, fields: &mut [u64]| -> Result<()> {
            for (i, field) in fields.iter_mut().enumerate() {
                let mut bytes = [0u8; 8];
                self.dev.ram.read(shared + at + 8 * i as u64, &mut bytes)?;
                *field = u64::from_le_bytes(bytes);
            }
            Ok(())
        };
        let mut fields = [0u64; crate::pv::TASK_LAYOUT_FIELDS];
        read(crate::pv::SHARED_TASKS, &mut fields)?;
        let mut bases = [0u64; crate::pv::TASK_BASES_FIELDS];
        read(crate::pv::SHARED_TASK_BASES, &mut bases)?;
        Ok(crate::pv::TaskLayout::from_fields(fields, bases))
    }

    /// Writes the VM's memory at a virtual address, all or nothing per page.
    /// A byte written where the debugger wrote int3 becomes the byte int3
    /// stands for, and int3 stays.
    pub fn write_virtual(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let physical = self
                .translate(address + done as u64)?
                .with_context(|| format!("{:#x} is not mapped", address + done as u64))?;
            let n = page_remainder(address + done as u64).min(data.len() - done);
            let mut chunk = data[done..done + n].to_vec();
            self.int3s.write_through(physical, &mut chunk);
            self.dev.ram.write(physical, &chunk)?;
            done += n;
        }
        Ok(())
    }

    /// The physical address behind a virtual one, if it is mapped, in the
    /// page tables the vCPU is on. The tables are walked here, where the
    /// walk asks only what is mapped. KVM_TRANSLATE walks them as a read
    /// by the kernel and applies the guest's SMAP to it: with SMAP on and
    /// RFLAGS.AC clear, as a guest shown the host's CPU runs nearly
    /// always, KVM translates no user page, whether the vCPU is in user
    /// space or the kernel. The fixed CPU models show the guest no SMAP.
    fn translate(&self, address: u64) -> Result<Option<u64>> {
        let sregs = self.vcpu.get_sregs()?;
        let paging = Paging {
            cr0: sregs.cr0,
            cr3: sregs.cr3,
            cr4: sregs.cr4,
        };
        let read = |physical: u64| {
            let mut entry = [0u8; PTE_SIZE as usize];
            self.dev.ram.read(physical, &mut entry).ok()?;
            Some(u64::from_le_bytes(entry))
        };
        Ok(paging.translate(read, address))
    }
}

/// The control registers that say how a vCPU translates an address.
#[derive(Clone, Copy, Debug)]
struct Paging {
    cr0: u64,
    cr3: u64,
    cr4: u64,
}

impl Paging {
    /// The physical address `address` maps to for a vCPU with these
    /// registers, reading each page table entry with `read`, or None where
    /// nothing is mapped: the address itself with paging off, else what
    /// the tables at CR3 map a canonical address to, whoever the mapping
    /// lets read it.
    fn translate(self, read: impl Fn(u64) -> Option<u64>, address: u64) -> Option<u64> {
        if self.cr0 & CR0_PG == 0 {
            return Some(address);
        }
        let levels = if self.cr4 & CR4_LA57 != 0 {
            Levels::Five
        } else {
            Levels::Four
        };
        if !levels.canonical(address) {
            return None;
        }
        walk(read, self.cr3, address, levels)
    }
}

/// DR7 for these traps, in registers 0 on. Refuses more traps than there
/// are registers, and a watch the CPU cannot make.
fn dr7(traps: &[Trap]) -> Result<u64> {
    if traps.len() > MAX_TRAPS {
        bail!(
            "at most {MAX_TRAPS} breakpoints and watchpoints, the CPU's debug registers; got {}",
            traps.len()
        );
    }
    let mut dr7 = 0;
    for (i, trap) in traps.iter().enumerate() {
        let condition = match *trap {
            Trap::Execute(_) => DR7_EXECUTE,
            Trap::Watch {
                address,
                len,
                access,
            } => {
                // DR7's length encoding: 1, 2, 8 and 4 bytes, in that order.
                let len_bits = match len {
                    1 => 0b00,
                    2 => 0b01,
                    8 => 0b10,
                    4 => 0b11,
                    _ => bail!("a watchpoint covers 1, 2, 4 or 8 bytes, not {len}"),
                };
                if !address.is_multiple_of(len) {
                    bail!("a watchpoint of {len} bytes must be aligned to {len}: {address:#x}");
                }
                let access_bits = match access {
                    Access::Write => DR7_WRITE,
                    Access::ReadWrite => DR7_READ_WRITE,
                };
                access_bits | len_bits << DR7_LEN_SHIFT
            }
        };
        dr7 |= DR7_LOCAL_ENABLE << (DR7_BITS_PER_ENABLE * i as u32);
        dr7 |= condition << (DR7_CONDITIONS + DR7_BITS_PER_CONDITION * i as u32);
    }
    Ok(dr7)
}

/// The entries of the top table that map user space: its lower half.
const USER_ENTRIES: u64 = 1 << (INDEX_BITS - 1);

/// Every user page the page tables rooted at `root` map to one of
/// `frames`, physical page addresses, as (frame, virtual address) in
/// address order, reading each entry with `read`. A large page maps each
/// of the frames inside it at its offset.
fn mappings(
    read: impl Fn(u64) -> Option<u64>,
    root: u64,
    levels: Levels,
    frames: &std::collections::BTreeSet<u64>,
) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    let top = levels.count() - 1;
    let mut tables = vec![(root & PTE_ADDRESS, top, 0u64, USER_ENTRIES)];

    // Depth first, each table's entries in order, so the addresses come
    // out in order.
    while let Some((table, level, base, entries)) = tables.pop() {
        let shift = PAGE_SHIFT + INDEX_BITS * level;
        let mut below = Vec::new();
        for index in 0..entries {
            let Some(entry) = read(table + index * PTE_SIZE) else {
                continue;
            };
            if entry & PTE_PRESENT == 0 {
                continue;
            }
            let address = base | (index << shift);
            let large = matches!(level, 1 | 2) && entry & PTE_LARGE != 0;
            if level > 0 && !large {
                below.push((entry & PTE_ADDRESS, level - 1, address, 1 << INDEX_BITS));
                continue;
            }

            // A page of 4 KiB, or a large one holding many frames.
            let size = 1u64 << shift;
            let start = entry & PTE_ADDRESS & !(size - 1);
            for frame in frames.range(start..start + size) {
                found.push((*frame, address + (frame - start)));
            }
        }
        tables.extend(below.into_iter().rev());
    }
    found.sort_by_key(|(_, address)| *address);
    found
}

/// How many bytes from `address` to the end of its page.
/// The physical address `address` maps to in the page tables rooted at
/// `root`, reading each entry with `read`, or None where it is not mapped.
/// Large pages end the walk early.
fn walk(read: impl Fn(u64) -> Option<u64>, root: u64, address: u64, levels: Levels) -> Option<u64> {
    let mut table = root & PTE_ADDRESS;
    for level in (0..levels.count()).rev() {
        let shift = PAGE_SHIFT + INDEX_BITS * level;
        let index = (address >> shift) & INDEX_MASK;
        let entry = read(table + index * PTE_SIZE)?;
        if entry & PTE_PRESENT == 0 {
            return None;
        }

        // A page: 4 KiB at the last level, or a large one a level or two
        // above it.
        let large = matches!(level, 1 | 2) && entry & PTE_LARGE != 0;
        if level == 0 || large {
            let size = 1u64 << shift;
            return Some((entry & PTE_ADDRESS & !(size - 1)) | (address & (size - 1)));
        }
        table = entry & PTE_ADDRESS;
    }
    None
}

fn page_remainder(address: u64) -> usize {
    (PAGE_SIZE - address % PAGE_SIZE) as usize
}

#[cfg(test)]
mod tests {
    // Reads and writes split at page boundaries, where the next page may
    // map somewhere else entirely.
    use super::*;

    #[test]
    fn an_early_counter_interrupt_is_armed_again() {
        // A stop 10,000 branches off with a margin of 512: far from it,
        // the interrupt is armed 512 short. An interrupt that came early,
        // with 6,000 still to go, as when the thread changed kind of core
        // on a hybrid host, is armed again 512 short, since the interrupt
        // fires once. Within the margin the machine single-steps, armed
        // or not, and keeps stepping.
        const MARGIN: u64 = 512;
        assert_eq!(
            Approach::Far.closer(10_000, MARGIN),
            (Approach::Armed, Some(9_488))
        );
        assert_eq!(
            Approach::Armed.closer(6_000, MARGIN),
            (Approach::Armed, Some(5_488))
        );
        assert_eq!(
            Approach::Armed.closer(MARGIN, MARGIN),
            (Approach::Stepping, None)
        );
        assert_eq!(Approach::Far.closer(1, MARGIN), (Approach::Stepping, None));
        assert_eq!(
            Approach::Stepping.closer(3, MARGIN),
            (Approach::Stepping, None)
        );
    }

    /// DR7 for a breakpoint and two watchpoints, encoded by hand from the
    /// Intel SDM's layout: each register's enable bit, then its condition
    /// and length in the upper half.
    #[test]
    fn dr7_enables_each_register_with_its_condition_and_length() {
        let traps = [
            Trap::Execute(0x1000),
            Trap::Watch {
                address: 0x2000,
                len: 8,
                access: Access::Write,
            },
            Trap::Watch {
                address: 0x3002,
                len: 2,
                access: Access::ReadWrite,
            },
        ];
        let enable = 0b01 | 0b01 << 2 | 0b01 << 4;
        // Register 0 executes, all zeros; 1 watches writes of 8 bytes; 2
        // watches reads and writes of 2.
        let conditions = 0b1001 << 20 | 0b0111 << 24;
        assert_eq!(dr7(&traps).unwrap(), enable | conditions);
    }

    /// The trap at the kernel's return to user space, which single steps
    /// across the return need, takes the register after the debugger's,
    /// and with all four the debugger's, the last that is not a read-write
    /// watch, whose trap is then off for the one instruction that returns.
    /// Checked on the registers KVM is handed for a breakpoint, then for
    /// three breakpoints and a watch, and four read-write watches, which
    /// leave none.
    #[test]
    fn the_return_to_user_space_takes_a_register_from_a_full_set() {
        const RETURN: u64 = 0x7f00_0000_1000;
        let breakpoint = Trap::Execute(0x40_1000);
        let watch = |access| Trap::Watch {
            address: 0x40_2000,
            len: 8,
            access,
        };
        let registers = |traps: &[Trap]| {
            let debugging = Debugging {
                stepping: Stepping::No,
                traps: traps.to_vec(),
            };
            let register = return_register(traps);
            let crossing = register.map(|register| Crossing {
                address: RETURN,
                register,
                restore: None,
                borrowed: None,
            });
            let debug = guest_debug(
                Some(&debugging),
                Stepping::No,
                Stepping::Yes,
                &crossing.into_iter().collect::<Vec<_>>(),
                &Int3s::default(),
            )
            .unwrap();
            (debug.arch.debugreg, register)
        };

        // A free register: the one after the debugger's.
        let (debugreg, register) = registers(&[breakpoint]);
        assert_eq!(register, Some(1));
        assert_eq!(debugreg[..2], [0x40_1000, RETURN]);

        // Four held: the watch's, the last, until the return.
        let full = [breakpoint, breakpoint, breakpoint, watch(Access::Write)];
        let (debugreg, register) = registers(&full);
        assert_eq!(register, Some(3));
        assert_eq!(debugreg[3], RETURN);
        assert_eq!(debugreg[DR7_INDEX], dr7(&[breakpoint; MAX_TRAPS]).unwrap());

        // Four read-write watches: none, and the debugger keeps them all.
        let (debugreg, register) = registers(&[watch(Access::ReadWrite); MAX_TRAPS]);
        assert_eq!(register, None);
        assert_eq!(debugreg[..MAX_TRAPS], [0x40_2000; MAX_TRAPS]);
    }

    /// The trap at the kernel's entry, which single steps across a
    /// `syscall` need, takes the register after the debugger's, and with
    /// all four the debugger's, the last that is not a breakpoint at the
    /// `syscall` itself, which still traps before the instruction runs. A
    /// watch of any kind is borrowed, since `syscall` touches no memory.
    /// Checked for a breakpoint, for three breakpoints and a watch, for
    /// four read-write watches, and for four registers whose last is a
    /// breakpoint at the `syscall`.
    #[test]
    fn the_kernel_entry_takes_a_register_from_a_full_set() {
        const SYSCALL_AT: u64 = 0x40_1893;
        let breakpoint = Trap::Execute(0x40_1000);
        let watch = |access| Trap::Watch {
            address: 0x40_2000,
            len: 8,
            access,
        };
        assert_eq!(entry_register(&[breakpoint], SYSCALL_AT), Some(1));
        let full = [breakpoint, breakpoint, breakpoint, watch(Access::Write)];
        assert_eq!(entry_register(&full, SYSCALL_AT), Some(3));
        let watches = [watch(Access::ReadWrite); MAX_TRAPS];
        assert_eq!(entry_register(&watches, SYSCALL_AT), Some(3));
        let here = [
            watch(Access::ReadWrite),
            breakpoint,
            watch(Access::Write),
            Trap::Execute(SYSCALL_AT),
        ];
        assert_eq!(entry_register(&here, SYSCALL_AT), Some(2));
    }

    /// The trap at the kernel's exception entry takes the first free
    /// register that is not the crossing's, else, with all four the
    /// debugger's, the last that is neither the crossing's, a read-write
    /// watch nor a breakpoint at the step's instruction. Checked for a
    /// breakpoint alone, with the register after it the crossing's, for
    /// three breakpoints and a write watch, for four read-write watches,
    /// and for a full set whose last is a breakpoint at the instruction
    /// and whose eligible register is the crossing's.
    #[test]
    fn the_exception_entry_takes_a_register_from_a_full_set() {
        const AT: u64 = 0x40_1893;
        let breakpoint = Trap::Execute(0x40_1000);
        let watch = |access| Trap::Watch {
            address: 0x40_2000,
            len: 8,
            access,
        };
        assert_eq!(exception_register(&[breakpoint], AT, None), Some(1));
        assert_eq!(exception_register(&[breakpoint], AT, Some(1)), Some(2));
        let full = [breakpoint, breakpoint, breakpoint, watch(Access::Write)];
        assert_eq!(exception_register(&full, AT, None), Some(3));
        assert_eq!(exception_register(&full, AT, Some(3)), None);
        let watches = [watch(Access::ReadWrite); MAX_TRAPS];
        assert_eq!(exception_register(&watches, AT, None), None);
        let here = [
            watch(Access::ReadWrite),
            breakpoint,
            watch(Access::ReadWrite),
            Trap::Execute(AT),
        ];
        assert_eq!(exception_register(&here, AT, None), Some(1));
        assert_eq!(exception_register(&here, AT, Some(1)), None);
    }

    /// An IDT gate's handler address is split in three: bits 0 to 15 in
    /// its first two bytes, 16 to 31 in bytes 6 and 7, and 32 to 63 in
    /// bytes 8 to 11; the selector, the gate's type and the reserved bytes
    /// between and after are not part of it. Checked on the gate Linux
    /// builds for asm_exc_page_fault at 0xffffffff81001280, with kernel
    /// code's selector 0x10 and an interrupt gate's type byte 0x8e.
    #[test]
    fn a_gate_holds_its_handler_in_three_pieces() {
        let gate = [
            0x80, 0x12, 0x10, 0x00, 0x00, 0x8e, 0x00, 0x81, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(gate_address(&gate), 0xffff_ffff_8100_1280);
    }

    /// Linux's IDT entry stubs share one entry, error_entry, which each
    /// calls after a short prologue: endbr64, a three-byte clac or nop, cld,
    /// and for an exception with no error code a push of -1. The call's
    /// target is found past the prologue from the stubs' bytes as the guest
    /// kernel has them, for a page fault, an invalid opcode and a common
    /// interrupt; a stub that calls nothing there is not decoded.
    #[test]
    fn the_stubs_share_their_call_to_the_common_entry() {
        const ERROR_ENTRY: u64 = 0xffff_ffff_8100_1810;
        let page_fault: [u8; 13] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x0f, 0x1f, 0x00, 0xfc, 0xe8, 0x83, 0x05, 0x00, 0x00,
        ];
        assert_eq!(
            common_entry(0xffff_ffff_8100_1280, &page_fault),
            Some(ERROR_ENTRY)
        );
        let invalid_op: [u8; 15] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x0f, 0x1f, 0x00, 0xfc, 0x6a, 0xff, 0xe8, 0xe1, 0x05, 0x00,
            0x00,
        ];
        assert_eq!(
            common_entry(0xffff_ffff_8100_1220, &invalid_op),
            Some(ERROR_ENTRY)
        );
        let clac: [u8; 13] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x0f, 0x01, 0xca, 0xfc, 0xe8, 0x83, 0x04, 0x00, 0x00,
        ];
        assert_eq!(
            common_entry(0xffff_ffff_8100_1380, &clac),
            Some(ERROR_ENTRY)
        );
        let other: [u8; 6] = [0xf3, 0x0f, 0x1e, 0xfa, 0x90, 0xc3];
        assert_eq!(common_entry(0xffff_ffff_8100_1380, &other), None);
    }

    /// A stub's instructions, each where it starts, come back in order
    /// with its call's target: the invalid opcode's has five, its push
    /// of -1 among them, and a stub that calls nothing has none. Ending at
    /// the call's return address, the stub is found from the bytes before
    /// it, the longest run of them that decodes as a stub; bytes of the
    /// stub before ending in its call decode as none.
    #[test]
    fn a_stub_is_its_instructions_in_order() {
        const STUB: u64 = 0xffff_ffff_8100_1220;
        let invalid_op: [u8; 15] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x0f, 0x1f, 0x00, 0xfc, 0x6a, 0xff, 0xe8, 0xe1, 0x05, 0x00,
            0x00,
        ];
        let (instructions, target) = stub_instructions(STUB, &invalid_op).unwrap();
        assert_eq!(
            instructions,
            [STUB, STUB + 4, STUB + 7, STUB + 8, STUB + 10]
        );
        assert_eq!(target, 0xffff_ffff_8100_1810);
        assert_eq!(stub_instructions(STUB, &[0x90, 0xc3]), None);

        let mut before = vec![0xe9, 0x00, 0x00, 0x00, 0x00, 0xcc];
        before.extend(invalid_op);
        let returns_to = STUB + invalid_op.len() as u64;
        let found = stub_ending_at(returns_to, &before);
        assert_eq!(found.map(|(i, _)| i[0]), Some(STUB));
        assert_eq!(stub_ending_at(returns_to, &invalid_op[..10]), None);
    }

    /// An external interrupt's vector stub pushes its vector and jumps to
    /// the common interrupt stub: its instructions, each where it starts,
    /// come back when the jump's target is that stub, by a 32-bit or an
    /// 8-bit displacement, and none when it jumps elsewhere or is no such
    /// stub. Checked on the guest kernel's stub for vector 0x20.
    #[test]
    fn a_vector_stub_jumps_to_the_common_interrupt() {
        const STUB: u64 = 0xffff_ffff_8100_0230;
        const COMMON: u64 = 0xffff_ffff_8100_1380;
        let vector: [u8; 11] = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x6a, 0x20, 0xe9, 0x45, 0x11, 0x00, 0x00,
        ];
        assert_eq!(
            vector_stub(STUB, &vector, COMMON),
            Some(vec![STUB, STUB + 4, STUB + 6])
        );
        assert_eq!(vector_stub(STUB, &vector, COMMON + 16), None);
        let short: [u8; 4] = [0x6a, 0x20, 0xeb, 0x10];
        assert_eq!(
            vector_stub(STUB, &short, STUB + 4 + 0x10),
            Some(vec![STUB, STUB + 2])
        );
        assert_eq!(vector_stub(STUB, &[0x90, 0xc3], COMMON), None);
    }

    /// error_entry begins by trading the stub's return address, which the
    /// call pushed where pt_regs keeps rdi, for rdi: push rsi, load the
    /// return address into rsi, store rdi over it. The store's end is
    /// where rdi's word in pt_regs is right again; other code is not
    /// decoded.
    #[test]
    fn error_entry_saves_rdi_three_instructions_in() {
        const ENTRY: u64 = 0xffff_ffff_8100_1810;
        let code = [
            0x56, 0x48, 0x8b, 0x74, 0x24, 0x08, 0x48, 0x89, 0x7c, 0x24, 0x08, 0x52,
        ];
        assert_eq!(rdi_saved_at(ENTRY, &code), Some(ENTRY + 11));
        assert_eq!(rdi_saved_at(ENTRY, &[0x90; 12]), None);
    }

    /// The CPU watches a power of two up to 8 bytes, aligned to itself,
    /// and has four registers; anything else is refused.
    #[test]
    fn a_watch_the_cpu_cannot_make_is_refused() {
        let watch = |address, len| Trap::Watch {
            address,
            len,
            access: Access::Write,
        };
        assert!(dr7(&[watch(0x2000, 3)]).is_err());
        assert!(dr7(&[watch(0x2004, 8)]).is_err());
        assert!(dr7(&[watch(0x2000, 16)]).is_err());
        assert!(dr7(&[watch(0x2000, 1); 5]).is_err());
        assert!(dr7(&[watch(0x2001, 1), watch(0x2004, 4)]).is_ok());
    }

    /// A #DB exit is the debugger's stop when one of its traps matched or
    /// it asked to step. A single step the monitor took toward a
    /// preemption point, or one a lifted int3 took, is the machine's own,
    /// and one nobody took is TF that a step left in a saved copy of
    /// RFLAGS. A watched access is reported even when a step trapped with
    /// it, since the step's instruction made it; a breakpoint only when no
    /// step did, as it traps again when the machine resumes.
    #[test]
    fn a_debug_trap_is_the_debuggers_the_machines_or_stray() {
        use Stepping::{No, Yes};
        let traps = [
            Trap::Execute(0x1000),
            Trap::Watch {
                address: 0x2000,
                len: 8,
                access: Access::Write,
            },
        ];
        let step = DR6_SINGLE_STEP;
        let watchpoint = DbExit::Stop(DebugStop::Watchpoint {
            address: 0x2000,
            traps: TrapSet(0b10),
        });
        assert_eq!(db_exit(0b10, &traps, No, No), watchpoint);
        assert_eq!(db_exit(0b10 | step, &traps, No, Yes), watchpoint);
        let breakpoint = DbExit::Stop(DebugStop::Breakpoint(0x1000));
        assert_eq!(db_exit(0b01, &traps, No, No), breakpoint);
        assert_eq!(
            db_exit(step, &traps, Yes, No),
            DbExit::Stop(DebugStop::Step)
        );
        assert_eq!(
            db_exit(step, &traps, Yes, Yes),
            DbExit::Stop(DebugStop::Step)
        );
        assert_eq!(
            db_exit(step | 0b01, &traps, Yes, No),
            DbExit::Stop(DebugStop::Step)
        );
        assert_eq!(db_exit(step | 0b01, &traps, No, Yes), DbExit::Machine);
        assert_eq!(db_exit(step, &traps, No, Yes), DbExit::Machine);
        assert_eq!(db_exit(step, &traps, No, No), DbExit::Stray);
    }

    /// One instruction that writes the bytes of two watches stops once
    /// and names both, by their place in the traps: a 64-bit store over
    /// two 32-bit watches sets both of their DR6 bits.
    #[test]
    fn a_stop_names_every_watch_one_access_matched() {
        let watch = |address| Trap::Watch {
            address,
            len: 4,
            access: Access::Write,
        };
        let traps = [Trap::Execute(0x1000), watch(0x2000), watch(0x2004)];
        let DbExit::Stop(DebugStop::Watchpoint {
            address,
            traps: hit,
        }) = db_exit(0b110, &traps, Stepping::No, Stepping::No)
        else {
            panic!("no watchpoint stop");
        };
        assert_eq!(address, 0x2000);
        let hit: Vec<usize> = (0..MAX_TRAPS).filter(|&i| hit.contains(i)).collect();
        assert_eq!(hit, [1, 2]);
    }

    /// KVM's debug settings combine the debugger's traps and steps, the
    /// monitor's steps toward a preemption point and the int3s written, so
    /// neither the monitor's steps nor a lifted int3 drop the debugger's
    /// traps, and no int3's #BP reaches the guest while one is written.
    /// Steps toward a debugger's branch count let interrupts through, and
    /// the user address they step toward takes the register after the
    /// debugger's.
    #[test]
    fn guest_debug_keeps_every_partys_traps() {
        use kvm_bindings::{
            KVM_GUESTDBG_BLOCKIRQ as BLOCKIRQ, KVM_GUESTDBG_ENABLE as ENABLE,
            KVM_GUESTDBG_SINGLESTEP as SINGLESTEP, KVM_GUESTDBG_USE_HW_BP as HW_BP,
            KVM_GUESTDBG_USE_SW_BP as SW_BP,
        };
        let debugger = Debugging {
            stepping: Stepping::No,
            traps: vec![Trap::Execute(0x1000)],
        };
        let stepping = Debugging {
            stepping: Stepping::Yes,
            traps: Vec::new(),
        };
        let none = Int3s::default();
        let mut written = Int3s::default();
        written.replaced.insert(0x5000, 0x55);
        let mut lifted = written.clone();
        lifted.lifted = Some(0x5000);
        let control = |d: Option<&Debugging>, monitor, int3s| {
            guest_debug(d, monitor, Stepping::No, &[], int3s)
                .unwrap()
                .control
        };

        assert_eq!(control(None, Stepping::No, &none), 0);
        assert_eq!(
            control(None, Stepping::Yes, &none),
            ENABLE | SINGLESTEP | BLOCKIRQ
        );
        assert_eq!(
            control(Some(&stepping), Stepping::No, &none),
            ENABLE | HW_BP | SINGLESTEP
        );
        assert_eq!(
            control(Some(&debugger), Stepping::No, &written),
            ENABLE | HW_BP | SW_BP
        );
        assert_eq!(
            control(Some(&debugger), Stepping::Yes, &written),
            ENABLE | HW_BP | SW_BP | SINGLESTEP | BLOCKIRQ
        );
        assert_eq!(
            control(Some(&debugger), Stepping::No, &lifted),
            ENABLE | HW_BP | SW_BP | SINGLESTEP | BLOCKIRQ
        );
        assert_eq!(
            guest_debug(Some(&debugger), Stepping::No, Stepping::Yes, &[], &none)
                .unwrap()
                .control,
            ENABLE | HW_BP | SINGLESTEP
        );
        let to_user = guest_debug(
            Some(&debugger),
            Stepping::No,
            Stepping::Yes,
            &[Crossing {
                address: 0x2000,
                register: 1,
                restore: None,
                borrowed: None,
            }],
            &none,
        )
        .unwrap();
        assert_eq!(to_user.arch.debugreg[1], 0x2000);
        assert_eq!(to_user.arch.debugreg[DR7_INDEX], 0b101);
        let d = guest_debug(Some(&debugger), Stepping::Yes, Stepping::No, &[], &written).unwrap();
        assert_eq!(d.arch.debugreg[0], 0x1000);
        assert_eq!(d.arch.debugreg[DR7_INDEX], 1);
    }

    /// Reads see the bytes int3 replaced, not int3, and a write over an
    /// int3 changes the byte kept for it while int3 stays in memory, or
    /// the byte itself while the int3 is lifted.
    #[test]
    fn reads_and_writes_see_the_bytes_int3_replaced() {
        let mut int3s = Int3s::default();
        int3s.replaced.insert(0x1002, 0x55);
        let mut read = [0x90, 0x90, INT3, 0x90];
        int3s.shadow(0x1000, &mut read);
        assert_eq!(read, [0x90, 0x90, 0x55, 0x90]);
        let mut beside = [0x90, 0x90];
        int3s.shadow(0x1003, &mut beside);
        assert_eq!(beside, [0x90, 0x90]);

        let mut write = [0x11, 0x22, 0x33];
        int3s.write_through(0x1001, &mut write);
        assert_eq!(write, [0x11, INT3, 0x33]);
        assert_eq!(int3s.replaced[&0x1002], 0x22);

        int3s.lifted = Some(0x1002);
        let mut write = [0x44];
        int3s.write_through(0x1002, &mut write);
        assert_eq!(write, [0x44]);
        assert_eq!(int3s.replaced[&0x1002], 0x44);
    }

    /// Page tables as a map from physical address to entry, built one
    /// entry at a time.
    #[derive(Default)]
    struct Tables(std::collections::HashMap<u64, u64>);

    impl Tables {
        /// Points entry `index` of the table at `table` to `to`, with
        /// `flags` besides present.
        fn entry(&mut self, table: u64, index: u64, to: u64, flags: u64) {
            self.0.insert(table + index * 8, to | flags | PTE_PRESENT);
        }

        fn read(&self, physical: u64) -> Option<u64> {
            self.0.get(&physical).copied()
        }
    }

    /// The index an address has at a level of the walk, 0 being the page
    /// table's.
    fn index(address: u64, level: u32) -> u64 {
        (address >> (PAGE_SHIFT + INDEX_BITS * level)) & INDEX_MASK
    }

    /// A 4 KiB page four levels down: the walk follows each table to the
    /// page and keeps the offset within it. An address whose table has no
    /// entry for it is not mapped.
    #[test]
    fn a_walk_follows_four_levels_to_a_page() {
        let address = 0x7fff_1234_5678;
        let mut t = Tables::default();
        t.entry(0x1000, index(address, 3), 0x2000, 0);
        t.entry(0x2000, index(address, 2), 0x3000, 0);
        t.entry(0x3000, index(address, 1), 0x4000, 0);
        t.entry(0x4000, index(address, 0), 0x9000, 0);
        let read = |p| t.read(p);
        assert_eq!(walk(read, 0x1000, address, Levels::Four), Some(0x9678));
        assert_eq!(walk(read, 0x1000, address + 0x1000, Levels::Four), None);
    }

    /// A 2 MiB page ends the walk at the page directory, and a 1 GiB one
    /// at the level above, each keeping the offset within its size. The
    /// root's low bits, a PCID in CR3, are not part of its address.
    #[test]
    fn a_walk_stops_at_a_large_page() {
        let address = 0x0000_4000_0012_3456;
        let mut t = Tables::default();
        t.entry(0x1000, index(address, 3), 0x2000, 0);
        t.entry(0x2000, index(address, 2), 0x3000, 0);
        t.entry(0x3000, index(address, 1), 0x4000_0000, PTE_LARGE);
        let read = |p| t.read(p);
        assert_eq!(
            walk(read, 0x1000 | 0x5, address, Levels::Four),
            Some(0x4012_3456)
        );

        let mut t = Tables::default();
        t.entry(0x1000, index(address, 3), 0x2000, 0);
        t.entry(0x2000, index(address, 2), 0x8000_0000, PTE_LARGE);
        let read = |p| t.read(p);
        assert_eq!(walk(read, 0x1000, address, Levels::Four), Some(0x8012_3456));
    }

    /// A user page, one whose every entry has the user bit, translates
    /// for a vCPU with paging on whose CR4 has SMAP: the translation asks
    /// only what is mapped. With paging off an address is its own
    /// physical address. An address that is not canonical maps nowhere,
    /// though its low bits index the same tables. CR4's LA57 picks five
    /// levels.
    #[test]
    fn a_translation_asks_only_what_is_mapped() {
        const PTE_USER: u64 = 1 << 2;
        const CR4_SMAP: u64 = 1 << 21;
        const ROOT: u64 = 0x1000;
        let address = 0x0000_0000_0040_18a0;
        let mut t = Tables::default();
        t.entry(ROOT, index(address, 3), 0x2000, PTE_USER);
        t.entry(0x2000, index(address, 2), 0x3000, PTE_USER);
        t.entry(0x3000, index(address, 1), 0x4000, PTE_USER);
        t.entry(0x4000, index(address, 0), 0x9000, PTE_USER);
        let read = |p| t.read(p);
        let paging = Paging {
            cr0: CR0_PG,
            cr3: ROOT,
            cr4: CR4_SMAP,
        };
        assert_eq!(paging.translate(read, address), Some(0x98a0));
        assert_eq!(paging.translate(read, address + PAGE_SIZE), None);
        assert_eq!(paging.translate(read, address | 1 << 62), None);

        let off = Paging { cr0: 0, ..paging };
        assert_eq!(off.translate(read, address), Some(address));

        let five = Paging {
            cr4: CR4_SMAP | CR4_LA57,
            ..paging
        };
        assert_eq!(five.translate(read, address), None);
    }

    /// With five levels the walk starts one table higher, indexed by the
    /// address's bits from 48.
    #[test]
    fn a_walk_with_five_levels_starts_one_higher() {
        let address = 0x0001_0000_0000_1234;
        let mut t = Tables::default();
        t.entry(0x1000, index(address, 4), 0x2000, 0);
        t.entry(0x2000, index(address, 3), 0x3000, 0);
        t.entry(0x3000, index(address, 2), 0x4000, 0);
        t.entry(0x4000, index(address, 1), 0x5000, 0);
        t.entry(0x5000, index(address, 0), 0x9000, 0);
        let read = |p| t.read(p);
        assert_eq!(walk(read, 0x1000, address, Levels::Five), Some(0x9234));
        assert_eq!(walk(read, 0x1000, address, Levels::Four), None);
    }

    /// The reverse of a walk: the user addresses that map each of a set of
    /// frames, through 4 KiB pages and a 2 MiB one, in address order.
    /// Frames nothing maps are not listed, and neither is the kernel's
    /// half of the table.
    #[test]
    fn the_user_addresses_that_map_a_frame_are_found() {
        let small = 0x40_5000;
        let large = 0x7f00_0020_0000;
        let mut t = Tables::default();
        t.entry(0x1000, index(small, 3), 0x2000, 0);
        t.entry(0x2000, index(small, 2), 0x3000, 0);
        t.entry(0x3000, index(small, 1), 0x4000, 0);
        t.entry(0x4000, index(small, 0), 0x9000, 0);
        t.entry(0x4000, index(small, 0) + 1, 0xa000, 0);
        t.entry(0x1000, index(large, 3), 0x5000, 0);
        t.entry(0x5000, index(large, 2), 0x6000, 0);
        t.entry(0x6000, index(large, 1), 0x20_0000, PTE_LARGE);
        t.entry(0x1000, 256, 0x7000, 0);
        t.entry(0x7000, 0, 0x8000, PTE_LARGE);
        let frames: std::collections::BTreeSet<u64> =
            [0x9000, 0x20_3000, 0xb000, 0x8000].into_iter().collect();
        let read = |p| t.read(p);
        assert_eq!(
            mappings(read, 0x1000, Levels::Four, &frames),
            vec![(0x9000, small), (0x20_3000, large + 0x3000)]
        );
    }

    #[test]
    fn a_page_remainder_runs_to_the_next_boundary() {
        assert_eq!(page_remainder(0x1000), 4096);
        assert_eq!(page_remainder(0x1ff0), 16);
        assert_eq!(page_remainder(0xffff_ffff_8128_5fff), 1);
    }
}
