//! ARM64 guest memory translation and hardware/software debugging on Linux KVM.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW, KVM_GUESTDBG_USE_SW_BP,
    kvm_guest_debug,
};

use crate::Machine;
pub use crate::arm64::debug::{
    Access, Arm64Regs, BRK_INSN, DebugStop, EC_BRK64, EC_HW_BP_LOWER, EC_SW_STEP_LOWER,
    EC_WATCHPOINT_LOWER, ExceptionEntry, MAX_TRAPS, MDSCR_MDE, MDSCR_SS, PSTATE_SS, SegmentStub,
    Stepping, Trap, TrapSet, kvm_regs, kvm_sregs, trap_set,
};
pub(crate) use crate::arm64::debug::{Debugging, Int3s};
use crate::arm64::debug::{encode_hw_bp, encode_hw_wp};
use crate::arm64::layout::{PAGE_SIZE, RAM_BASE};
use crate::arm64::pagetable::{mappings_in_arm64, walk_arm64};
use crate::kvm_arm64::cpu;

impl Machine {
    /// Enables hardware breakpoints, hardware watchpoints, and/or single-stepping
    /// on the ARM64 KVM vCPU via `KVM_SET_GUEST_DEBUG`.
    pub fn set_debug(&mut self, stepping: Stepping, traps: &[Trap]) -> Result<()> {
        if traps.len() > MAX_TRAPS {
            bail!(
                "ARM64 KVM supports at most {MAX_TRAPS} hardware debug traps (got {})",
                traps.len()
            );
        }
        self.debugging = Some(Debugging {
            stepping,
            traps: traps.to_vec(),
        });
        self.apply_guest_debug()
    }

    /// Disables hardware breakpoints, watchpoints, and single-stepping.
    /// Any software `BRK #0` breakpoints still installed keep `KVM_GUESTDBG_USE_SW_BP` active.
    pub fn clear_debug(&mut self) -> Result<()> {
        self.debugging = None;
        self.passing_breakpoint = false;
        self.watching_step = None;
        self.apply_guest_debug()
    }

    pub(crate) fn is_stepping(&self) -> bool {
        self.passing_breakpoint
            || self.watching_step.is_some()
            || self
                .debugging
                .as_ref()
                .is_some_and(|d| d.stepping == Stepping::Yes)
    }

    /// Applies the combined debugger, pass-breakpoint, watchpoint-step, and
    /// software-breakpoint configuration to KVM via `KVM_SET_GUEST_DEBUG`.
    pub(crate) fn apply_guest_debug(&mut self) -> Result<()> {
        let mut dbg = kvm_guest_debug::default();
        let suppress_hw = self.passing_breakpoint || self.watching_step.is_some();
        let traps: &[Trap] = if suppress_hw {
            &[]
        } else {
            self.debugging.as_ref().map_or(&[], |d| d.traps.as_slice())
        };

        let mut bp_idx = 0usize;
        let mut wp_idx = 0usize;
        for &trap in traps {
            match trap {
                Trap::Execute(addr) => {
                    let (bvr, bcr) = encode_hw_bp(addr);
                    dbg.arch.dbg_bvr[bp_idx] = bvr;
                    dbg.arch.dbg_bcr[bp_idx] = bcr;
                    bp_idx += 1;
                }
                Trap::Watch {
                    address,
                    len,
                    access,
                } => {
                    let (wvr, wcr) = encode_hw_wp(address, len, access)?;
                    dbg.arch.dbg_wvr[wp_idx] = wvr;
                    dbg.arch.dbg_wcr[wp_idx] = wcr;
                    wp_idx += 1;
                }
            }
        }

        let stepping = self.is_stepping();
        let has_hw = bp_idx > 0 || wp_idx > 0;
        let has_sw = !self.int3s.replaced.is_empty();

        if stepping || has_hw || has_sw {
            dbg.control |= KVM_GUESTDBG_ENABLE;
        }
        if stepping {
            dbg.control |= KVM_GUESTDBG_SINGLESTEP;
        }
        if has_hw {
            dbg.control |= KVM_GUESTDBG_USE_HW;
        }
        if has_sw {
            dbg.control |= KVM_GUESTDBG_USE_SW_BP;
        }

        self.vcpu
            .set_guest_debug(&dbg)
            .context("KVM_SET_GUEST_DEBUG")
    }

    /// Re-injects a guest-originated `BRK` synchronous exception into EL1 when
    /// `KVM_GUESTDBG_USE_SW_BP` intercepted it.
    pub(crate) fn reinject_brk_el1(&self, syndrome: u64) -> Result<()> {
        let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let vbar = cpu::get_u64(&self.vcpu, cpu::SYS_VBAR_EL1)?;
        let offset = match pstate & 0xf {
            0 => 0x400,
            5 => 0x200,
            _ => 0x000,
        };
        cpu::set_u64(&self.vcpu, cpu::REG_ELR_EL1, pc)?;
        cpu::set_u64(&self.vcpu, cpu::REG_SPSR_EL1, pstate)?;
        cpu::set_u64(&self.vcpu, cpu::SYS_ESR_EL1, syndrome)?;
        cpu::set_u64(&self.vcpu, cpu::REG_PC, vbar.wrapping_add(offset))?;
        let new_pstate = (pstate & 0xf000_0000) | 0x3c5;
        cpu::set_u64(&self.vcpu, cpu::REG_PSTATE, new_pstate)
    }

    pub fn branch_count(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    pub fn set_branch_stop(&mut self, _target: Option<u64>, _margin: u64) -> Result<()> {
        bail!("branch-count breakpoints are not supported on ARM64 KVM")
    }

    pub fn exception_entry_point(&self) -> Result<Option<ExceptionEntry>> {
        Ok(None)
    }

    pub fn skipped(&self) -> &[u64] {
        &[]
    }

    /// Installs an ARM64 `BRK #0` software breakpoint at 4-byte-aligned guest
    /// physical address `physical`, remembering the replaced 4-byte instruction.
    pub fn insert_int3(&mut self, physical: u64) -> Result<()> {
        if !physical.is_multiple_of(4) {
            bail!("ARM64 software breakpoint address {physical:#x} is not 4-byte aligned");
        }
        if self.int3s.replaced.contains_key(&physical) {
            return Ok(());
        }
        let mut orig = [0u8; 4];
        self.dev.ram_read(physical, &mut orig)?;
        self.dev.ram_write(physical, &BRK_INSN)?;
        self.int3s.replaced.insert(physical, orig);
        self.apply_guest_debug()
    }

    /// Restores the original 4-byte instruction at `physical`.
    pub fn remove_int3(&mut self, physical: u64) -> Result<()> {
        let Some(orig) = self.int3s.replaced.remove(&physical) else {
            return Ok(());
        };
        if self.int3s.lifted == Some(physical) {
            self.int3s.lifted = None;
        }
        self.dev.ram_write(physical, &orig)?;
        self.apply_guest_debug()
    }

    /// Lets the instruction at the current breakpoint execute once on the next
    /// `run()` without re-triggering that breakpoint.
    pub fn pass_breakpoint(&mut self) -> Result<()> {
        let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
        if let Some((physical, _)) = self.translate(pc)?
            && let Some(&orig) = self.int3s.replaced.get(&physical)
        {
            self.dev.ram_write(physical, &orig)?;
            self.int3s.lifted = Some(physical);
        }
        self.passing_breakpoint = true;
        self.apply_guest_debug()
    }

    /// Re-inserts a lifted software breakpoint and clears `passing_breakpoint`.
    pub(crate) fn finish_pass_breakpoint(&mut self) -> Result<()> {
        self.passing_breakpoint = false;
        if let Some(physical) = self.int3s.lifted.take()
            && self.int3s.replaced.contains_key(&physical)
        {
            self.dev.ram_write(physical, &BRK_INSN)?;
        }
        self.apply_guest_debug()
    }

    /// Reads the full AArch64 register file (GPRs, active SP, PC, PSTATE,
    /// V0..V31, FPCR, FPSR, and TPIDR_EL0) for GDB.
    pub fn arm64_registers(&self) -> Result<Arm64Regs> {
        let mut x = [0u64; 31];
        for (i, slot) in x.iter_mut().enumerate() {
            *slot = cpu::get_u64(&self.vcpu, cpu::core_reg((i * 2) as u64))?;
        }
        let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let sp_id = if (pstate & 0xf) == 5 {
            cpu::REG_SP_EL1
        } else {
            cpu::REG_SP_EL0
        };
        let sp = cpu::get_u64(&self.vcpu, sp_id)?;
        let mut v = [0u128; 32];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = cpu::get_u128(&self.vcpu, cpu::reg_v(i))?;
        }
        let fpsr = cpu::get_u32(&self.vcpu, cpu::REG_FPSR)?;
        let fpcr = cpu::get_u32(&self.vcpu, cpu::REG_FPCR)?;
        let tpidr = cpu::get_u64(&self.vcpu, cpu::SYS_TPIDR_EL0)?;
        Ok(Arm64Regs {
            x,
            sp,
            pc,
            cpsr: pstate as u32,
            v,
            fpcr,
            fpsr,
            tpidr,
        })
    }

    /// Writes the AArch64 core and SIMD/FP registers from GDB.
    pub fn set_arm64_registers(&mut self, regs: &Arm64Regs) -> Result<()> {
        for (i, &val) in regs.x.iter().enumerate() {
            cpu::set_u64(&self.vcpu, cpu::core_reg((i * 2) as u64), val)?;
        }
        let sp_id = if (regs.cpsr & 0xf) == 5 {
            cpu::REG_SP_EL1
        } else {
            cpu::REG_SP_EL0
        };
        cpu::set_u64(&self.vcpu, sp_id, regs.sp)?;
        cpu::set_u64(&self.vcpu, cpu::REG_PC, regs.pc)?;
        cpu::set_u64(&self.vcpu, cpu::REG_PSTATE, u64::from(regs.cpsr))?;
        for (i, &val) in regs.v.iter().enumerate() {
            cpu::set_u128(&self.vcpu, cpu::reg_v(i), val)?;
        }
        cpu::set_u32(&self.vcpu, cpu::REG_FPSR, regs.fpsr)?;
        cpu::set_u32(&self.vcpu, cpu::REG_FPCR, regs.fpcr)?;
        Ok(())
    }

    pub fn registers(&self) -> Result<kvm_regs> {
        let pc = cpu::get_u64(&self.vcpu, cpu::REG_PC)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        Ok(kvm_regs {
            rip: pc,
            rflags: pstate,
            ..Default::default()
        })
    }

    pub fn named_registers(&self) -> Result<[(&'static str, u64); 18]> {
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let sp_id = if (pstate & 0xf) == 5 {
            cpu::REG_SP_EL1
        } else {
            cpu::REG_SP_EL0
        };
        Ok([
            ("x0", cpu::get_u64(&self.vcpu, cpu::core_reg(0))?),
            ("x1", cpu::get_u64(&self.vcpu, cpu::core_reg(2))?),
            ("x2", cpu::get_u64(&self.vcpu, cpu::core_reg(4))?),
            ("x3", cpu::get_u64(&self.vcpu, cpu::core_reg(6))?),
            ("x4", cpu::get_u64(&self.vcpu, cpu::core_reg(8))?),
            ("x5", cpu::get_u64(&self.vcpu, cpu::core_reg(10))?),
            ("x6", cpu::get_u64(&self.vcpu, cpu::core_reg(12))?),
            ("x7", cpu::get_u64(&self.vcpu, cpu::core_reg(14))?),
            ("x8", cpu::get_u64(&self.vcpu, cpu::core_reg(16))?),
            ("x9", cpu::get_u64(&self.vcpu, cpu::core_reg(18))?),
            ("x10", cpu::get_u64(&self.vcpu, cpu::core_reg(20))?),
            ("x11", cpu::get_u64(&self.vcpu, cpu::core_reg(22))?),
            ("x12", cpu::get_u64(&self.vcpu, cpu::core_reg(24))?),
            ("x29", cpu::get_u64(&self.vcpu, cpu::core_reg(58))?),
            ("x30", cpu::get_u64(&self.vcpu, cpu::core_reg(60))?),
            ("sp", cpu::get_u64(&self.vcpu, sp_id)?),
            ("pc", cpu::get_u64(&self.vcpu, cpu::REG_PC)?),
            ("pstate", pstate),
        ])
    }

    pub fn set_registers(&mut self, regs: &kvm_regs) -> Result<()> {
        cpu::set_u64(&self.vcpu, cpu::REG_PC, regs.rip)?;
        cpu::set_u64(&self.vcpu, cpu::REG_PSTATE, regs.rflags)
    }

    pub fn special_registers(&self) -> Result<kvm_sregs> {
        let ttbr0 = cpu::get_u64(&self.vcpu, cpu::SYS_TTBR0_EL1)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let is_user = (pstate & 0xf) == 0;
        let tpidr = cpu::get_u64(&self.vcpu, cpu::SYS_TPIDR_EL0)?;
        Ok(kvm_sregs {
            cs: SegmentStub {
                dpl: if is_user { 3 } else { 0 },
                selector: if is_user { 3 } else { 0 },
                base: 0,
            },
            fs: SegmentStub {
                base: tpidr,
                ..Default::default()
            },
            cr3: ttbr0 & 0x0000_ffff_ffff_f000,
            ..Default::default()
        })
    }

    pub fn read_physical(&self, address: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.ram_read(address, buf)?;
        self.int3s.shadow(address, buf);
        Ok(())
    }

    pub fn translate(&self, vaddr: u64) -> Result<Option<(u64, u64)>> {
        let sctlr = cpu::get_u64(&self.vcpu, cpu::SYS_SCTLR_EL1)?;
        if sctlr & 1 == 0 {
            return Ok(Some((vaddr, PAGE_SIZE as u64 - (vaddr & 0xfff))));
        }
        let tcr = cpu::get_u64(&self.vcpu, cpu::SYS_TCR_EL1)?;
        if (vaddr >> 55) & 1 != 0 {
            let ttbr1 = cpu::get_u64(&self.vcpu, cpu::SYS_TTBR1_EL1)?;
            let t1sz = (tcr >> 16) & 0x3f;
            walk_arm64(&self.dev.ram, ttbr1, t1sz, vaddr)
        } else {
            let ttbr0 = cpu::get_u64(&self.vcpu, cpu::SYS_TTBR0_EL1)?;
            let t0sz = tcr & 0x3f;
            walk_arm64(&self.dev.ram, ttbr0, t0sz, vaddr)
        }
    }

    pub fn read_virtual(&self, address: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let va = address.wrapping_add(done as u64);
            let Some((gpa, avail)) = self.translate(va)? else {
                break;
            };
            let take = (buf.len() - done).min(avail as usize);
            if self.read_physical(gpa, &mut buf[done..done + take]).is_err() {
                break;
            }
            done += take;
        }
        Ok(done)
    }

    pub fn write_virtual(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let va = address.wrapping_add(done as u64);
            let Some((gpa, avail)) = self.translate(va)? else {
                bail!("virtual address {va:#x} is not mapped");
            };
            let take = (data.len() - done).min(avail as usize);
            let mut staged = data[done..done + take].to_vec();
            self.int3s.write_through(gpa, &mut staged);
            self.dev.ram_write(gpa, &staged)?;
            done += take;
        }
        Ok(())
    }

    pub fn read_virtual_in(&self, ttbr0: u64, address: u64, buf: &mut [u8]) -> Result<usize> {
        let tcr = cpu::get_u64(&self.vcpu, cpu::SYS_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        let mut done = 0;
        while done < buf.len() {
            let va = address.wrapping_add(done as u64);
            let Some((gpa, avail)) = walk_arm64(&self.dev.ram, ttbr0, t0sz, va)? else {
                break;
            };
            let take = (buf.len() - done).min(avail as usize);
            if self.read_physical(gpa, &mut buf[done..done + take]).is_err() {
                break;
            }
            done += take;
        }
        Ok(done)
    }

    pub fn physical_in(&self, page_table: u64, address: u64) -> Result<Option<u64>> {
        let sctlr = cpu::get_u64(&self.vcpu, cpu::SYS_SCTLR_EL1)?;
        if sctlr & 1 == 0 {
            return Ok(Some(address));
        }
        let tcr = cpu::get_u64(&self.vcpu, cpu::SYS_TCR_EL1)?;
        if (address >> 55) & 1 != 0 {
            let ttbr1 = cpu::get_u64(&self.vcpu, cpu::SYS_TTBR1_EL1)?;
            let t1sz = (tcr >> 16) & 0x3f;
            return Ok(walk_arm64(&self.dev.ram, ttbr1, t1sz, address)?.map(|(pa, _)| pa));
        }
        let t0sz = tcr & 0x3f;
        Ok(walk_arm64(&self.dev.ram, page_table, t0sz, address)?.map(|(pa, _)| pa))
    }

    pub fn mappings_in(
        &self,
        root: u64,
        wanted: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<(u64, u64)>> {
        let tcr = cpu::get_u64(&self.vcpu, cpu::SYS_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        mappings_in_arm64(&self.dev.ram, root, t0sz, wanted)
    }

    pub fn task_layout(&self) -> Result<Option<crate::pv::TaskLayout>> {
        let Some(shared) = self.dev.shared else {
            return Ok(None);
        };
        let mut words = [0u64; crate::pv::TASK_LAYOUT_FIELDS];
        let mut bytes = [0u8; crate::pv::TASK_LAYOUT_FIELDS * 8];
        self.dev
            .ram_read(shared + crate::pv::SHARED_TASKS, &mut bytes)?;
        for (i, w) in words.iter_mut().enumerate() {
            *w = u64::from_le_bytes(bytes[i * 8..(i + 1) * 8].try_into().expect("eight bytes"));
        }
        if words[0] == 0 {
            return Ok(None);
        }
        let mut bases = [0u8; crate::pv::TASK_BASES_FIELDS * 8];
        self.dev
            .ram_read(shared + crate::pv::SHARED_TASK_BASES, &mut bases)?;
        let fsbase = u64::from_le_bytes(bases[0..8].try_into().expect("eight bytes"));
        let gsbase = u64::from_le_bytes(bases[8..16].try_into().expect("eight bytes"));
        Ok(crate::pv::TaskLayout::from_fields(words, [fsbase, gsbase]))
    }

    pub fn ram_offset_of(&self, gpa: u64) -> Option<u64> {
        gpa.checked_sub(RAM_BASE)
    }
}
