//! Debugging a forked machine: its registers, its memory as the VM's own
//! page tables map it, breakpoints, watchpoints and single steps. `rewind
//! gdb` serves these over the GDB remote protocol.
//!
//! Breakpoints and watchpoints are the CPU's four debug address registers
//! rather than int3 written into memory or a value compared after every
//! step, so nothing in the VM changes and it runs at full speed. A debug
//! trap is a VM exit the VM never sees, and it is not a step: a machine
//! runs the same whether or not it is being debugged.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW_BP, kvm_guest_debug,
    kvm_regs, kvm_sregs,
};

use crate::Machine;

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
pub(crate) struct Debugging {
    stepping: Stepping,
    traps: Vec<Trap>,
}

/// Why a debugged machine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugStop {
    /// One instruction ran.
    Step,
    /// Execution reached the breakpoint at this address.
    Breakpoint(u64),
    /// An instruction accessed the watched bytes at this address, and has
    /// run.
    Watchpoint(u64),
}

impl Machine {
    /// Turns debugging on with these traps, one per debug address register,
    /// stepping one instruction at a time when asked. With debugging on,
    /// `run` returns [`crate::Outcome::Debug`] at every trap that is the
    /// debugger's.
    ///
    /// Interrupts are not held off while stepping, though a step then
    /// lands in an interrupt handler now and then: one held off is taken
    /// later than when the run was recorded, and the machine goes another
    /// way from there.
    pub fn set_debug(&mut self, stepping: Stepping, traps: &[Trap]) -> Result<()> {
        let mut debug = kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP,
            ..Default::default()
        };
        if stepping == Stepping::Yes {
            debug.control |= KVM_GUESTDBG_SINGLESTEP;
        }
        debug.arch.debugreg[DR7_INDEX] = dr7(traps)?;
        for (i, trap) in traps.iter().enumerate() {
            debug.arch.debugreg[i] = match trap {
                Trap::Execute(address) | Trap::Watch { address, .. } => *address,
            };
        }
        self.vcpu.set_guest_debug(&debug)?;
        self.debugging = Some(Debugging {
            stepping,
            traps: traps.to_vec(),
        });
        Ok(())
    }

    /// Turns debugging off.
    pub fn clear_debug(&mut self) -> Result<()> {
        self.vcpu.set_guest_debug(&kvm_guest_debug::default())?;
        self.debugging = None;
        Ok(())
    }

    /// What a debug trap with this DR6 means, given the traps set.
    pub(crate) fn debug_stop(&self, dr6: u64) -> DebugStop {
        let traps = self.debugging.as_ref().map_or(&[][..], |d| &d.traps[..]);
        stop_for(dr6, traps)
    }

    /// Whether a debug trap is stray, and if so clears the TF behind it, so
    /// the guest runs on as it did when the run was recorded. KVM steps
    /// the vCPU by setting TF, and an instruction that saves RFLAGS while
    /// it is set, `syscall` into R11, `pushf`, or an interrupt's frame,
    /// keeps a copy that sets TF again when the guest restores it, after
    /// the debugger has stopped stepping. The trap comes after the
    /// instruction has run, and KVM keeps it from the guest.
    pub(crate) fn clear_stray_trap(&mut self, dr6: u64) -> Result<bool> {
        let stepping = self.debugging.as_ref().map_or(Stepping::No, |d| d.stepping);
        if !stray_trap(dr6, stepping) {
            return Ok(false);
        }
        let mut regs = self.vcpu.get_regs()?;
        regs.rflags &= !RFLAGS_TF;
        self.vcpu.set_regs(&regs)?;
        Ok(true)
    }

    /// The general purpose registers, the instruction pointer and flags.
    pub fn registers(&self) -> Result<kvm_regs> {
        Ok(self.vcpu.get_regs()?)
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
            if self
                .dev
                .ram
                .read(physical, &mut buf[done..done + n])
                .is_err()
            {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// Reads the VM's memory at a virtual address as the page table at
    /// physical address `page_table` maps it, whichever process is
    /// running: a process's memory while another runs. Returns how many
    /// bytes could be read, which stops short at the first unmapped page.
    pub fn read_virtual_in(&self, page_table: u64, address: u64, buf: &mut [u8]) -> Result<usize> {
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
        let mut done = 0;
        while done < buf.len() {
            let at = address + done as u64;
            let Some(physical) = walk(read, page_table, at, levels) else {
                break;
            };
            let n = page_remainder(at).min(buf.len() - done);
            if self
                .dev
                .ram
                .read(physical, &mut buf[done..done + n])
                .is_err()
            {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// What the kernel published about its tasks at setup, or None from a
    /// kernel that predates it or before setup.
    pub fn task_layout(&self) -> Result<Option<crate::pv::TaskLayout>> {
        let Some(shared) = self.dev.shared else {
            return Ok(None);
        };
        let mut fields = [0u64; crate::pv::TASK_LAYOUT_FIELDS];
        for (i, field) in fields.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            self.dev
                .ram
                .read(shared + crate::pv::SHARED_TASKS + 8 * i as u64, &mut bytes)?;
            *field = u64::from_le_bytes(bytes);
        }
        Ok(crate::pv::TaskLayout::from_fields(fields))
    }

    /// Writes the VM's memory at a virtual address, all or nothing per page.
    pub fn write_virtual(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let physical = self
                .translate(address + done as u64)?
                .with_context(|| format!("{:#x} is not mapped", address + done as u64))?;
            let n = page_remainder(address + done as u64).min(data.len() - done);
            self.dev.ram.write(physical, &data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// The physical address behind a virtual one, if it is mapped.
    fn translate(&self, address: u64) -> Result<Option<u64>> {
        let t = self.vcpu.translate_gva(address)?;
        Ok((t.valid != 0).then_some(t.physical_address))
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

/// What a debug trap with this DR6 means. A watched access is reported
/// even when a step trapped with it, since the step's instruction made
/// it; a breakpoint only when no step did.
fn stop_for(dr6: u64, traps: &[Trap]) -> DebugStop {
    let hits = (0..MAX_TRAPS)
        .filter(|i| dr6 & DR6_HIT_MASK & (1 << i) != 0)
        .filter_map(|i| traps.get(i));
    let mut breakpoint = None;
    for trap in hits {
        match *trap {
            Trap::Watch { address, .. } => return DebugStop::Watchpoint(address),
            Trap::Execute(address) => breakpoint = breakpoint.or(Some(address)),
        }
    }
    match breakpoint {
        Some(address) if dr6 & DR6_SINGLE_STEP == 0 => DebugStop::Breakpoint(address),
        _ => DebugStop::Step,
    }
}

/// Whether a debug trap with this DR6 is a single step the debugger did
/// not ask for.
fn stray_trap(dr6: u64, stepping: Stepping) -> bool {
    dr6 & DR6_SINGLE_STEP != 0 && dr6 & DR6_HIT_MASK == 0 && stepping == Stepping::No
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

    /// A trap names the register that matched in DR6: a watched address
    /// stops as a watchpoint even when a step trapped with it, an executed
    /// one as a breakpoint, and a step alone as a step.
    #[test]
    fn a_trap_stops_for_the_register_that_matched() {
        let traps = [
            Trap::Execute(0x1000),
            Trap::Watch {
                address: 0x2000,
                len: 8,
                access: Access::Write,
            },
        ];
        assert_eq!(stop_for(0b10, &traps), DebugStop::Watchpoint(0x2000));
        assert_eq!(
            stop_for(0b10 | DR6_SINGLE_STEP, &traps),
            DebugStop::Watchpoint(0x2000)
        );
        assert_eq!(stop_for(0b01, &traps), DebugStop::Breakpoint(0x1000));
        assert_eq!(stop_for(DR6_SINGLE_STEP, &traps), DebugStop::Step);
    }

    /// A single-step trap is the debugger's only when it asked to step;
    /// otherwise it is TF that one of its steps left in a saved copy of
    /// RFLAGS. A breakpoint's trap is always the debugger's.
    #[test]
    fn a_single_step_trap_the_debugger_did_not_ask_for_is_stray() {
        assert!(stray_trap(DR6_SINGLE_STEP, Stepping::No));
        assert!(!stray_trap(DR6_SINGLE_STEP, Stepping::Yes));
        assert!(!stray_trap(1, Stepping::No));
        assert!(!stray_trap(DR6_SINGLE_STEP | 1, Stepping::No));
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

    #[test]
    fn a_page_remainder_runs_to_the_next_boundary() {
        assert_eq!(page_remainder(0x1000), 4096);
        assert_eq!(page_remainder(0x1ff0), 16);
        assert_eq!(page_remainder(0xffff_ffff_8128_5fff), 1);
    }
}
