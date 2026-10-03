//! Debugging a forked machine: its registers, its memory as the VM's own
//! page tables map it, breakpoints and single steps. `rewind gdb` serves
//! these over the GDB remote protocol.
//!
//! Breakpoints are the CPU's four debug address registers rather than int3
//! written into memory, so nothing in the VM changes. A debug trap is a
//! VM exit the VM never sees, and it is not a step: a machine runs the
//! same whether or not it is being debugged.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW_BP, kvm_guest_debug,
    kvm_regs, kvm_sregs,
};

use crate::Machine;

/// The CPU has four breakpoint address registers, DR0 to DR3.
pub const MAX_BREAKPOINTS: usize = 4;

/// DR7: the local enable bit of breakpoint i is bit 2i; its condition and
/// length bits, all zero, mean "break on executing this address".
const DR7_LOCAL_ENABLE: u64 = 1;
const DR7_BITS_PER_BREAKPOINT: u32 = 2;
const DR7_INDEX: usize = 7;

/// DR6 on a debug trap: which breakpoint matched (bits 0 to 3), or BS
/// (bit 14) for a single step.
const DR6_HIT_MASK: u64 = 0xf;
const DR6_SINGLE_STEP: u64 = 1 << 14;

/// RFLAGS' trap flag, which KVM sets to single-step the vCPU.
const RFLAGS_TF: u64 = 1 << 8;

const PAGE_SIZE: u64 = 4096;

/// Whether a debugger runs the machine one instruction at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stepping {
    No,
    Yes,
}

/// What a debugger has asked of the machine.
pub(crate) struct Debugging {
    stepping: Stepping,
    breakpoints: Vec<u64>,
}

/// Why a debugged machine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugStop {
    /// One instruction ran.
    Step,
    /// Execution reached the breakpoint at this address.
    Breakpoint(u64),
}

impl Machine {
    /// Turns debugging on with these breakpoint addresses, stepping one
    /// instruction at a time when asked. With debugging on, `run` returns
    /// [`crate::Outcome::Debug`] at every trap that is the debugger's.
    ///
    /// Interrupts are not held off while stepping, though a step then
    /// lands in an interrupt handler now and then: one held off is taken
    /// later than when the run was recorded, and the machine goes another
    /// way from there.
    pub fn set_debug(&mut self, stepping: Stepping, breakpoints: &[u64]) -> Result<()> {
        if breakpoints.len() > MAX_BREAKPOINTS {
            bail!(
                "at most {MAX_BREAKPOINTS} breakpoints, the CPU's debug registers; got {}",
                breakpoints.len()
            );
        }
        let mut debug = kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP,
            ..Default::default()
        };
        if stepping == Stepping::Yes {
            debug.control |= KVM_GUESTDBG_SINGLESTEP;
        }
        for (i, address) in breakpoints.iter().enumerate() {
            debug.arch.debugreg[i] = *address;
            debug.arch.debugreg[DR7_INDEX] |=
                DR7_LOCAL_ENABLE << (DR7_BITS_PER_BREAKPOINT * i as u32);
        }
        self.vcpu.set_guest_debug(&debug)?;
        self.debugging = Some(Debugging {
            stepping,
            breakpoints: breakpoints.to_vec(),
        });
        Ok(())
    }

    /// Turns debugging off.
    pub fn clear_debug(&mut self) -> Result<()> {
        self.vcpu.set_guest_debug(&kvm_guest_debug::default())?;
        self.debugging = None;
        Ok(())
    }

    /// What a debug trap with this DR6 means, given the breakpoints set.
    pub(crate) fn debug_stop(&self, dr6: u64) -> DebugStop {
        let breakpoints = self
            .debugging
            .as_ref()
            .map_or(&[][..], |d| &d.breakpoints[..]);
        if dr6 & DR6_SINGLE_STEP == 0 {
            let hit = (dr6 & DR6_HIT_MASK).trailing_zeros() as usize;
            if let Some(address) = breakpoints.get(hit) {
                return DebugStop::Breakpoint(*address);
            }
        }
        DebugStop::Step
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

/// Whether a debug trap with this DR6 is a single step the debugger did
/// not ask for.
fn stray_trap(dr6: u64, stepping: Stepping) -> bool {
    dr6 & DR6_SINGLE_STEP != 0 && dr6 & DR6_HIT_MASK == 0 && stepping == Stepping::No
}

/// How many bytes from `address` to the end of its page.
fn page_remainder(address: u64) -> usize {
    (PAGE_SIZE - address % PAGE_SIZE) as usize
}

#[cfg(test)]
mod tests {
    // Reads and writes split at page boundaries, where the next page may
    // map somewhere else entirely.
    use super::*;

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

    #[test]
    fn a_page_remainder_runs_to_the_next_boundary() {
        assert_eq!(page_remainder(0x1000), 4096);
        assert_eq!(page_remainder(0x1ff0), 16);
        assert_eq!(page_remainder(0xffff_ffff_8128_5fff), 1);
    }
}
