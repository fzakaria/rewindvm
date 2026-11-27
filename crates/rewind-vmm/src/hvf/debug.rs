//! ARM64 guest memory translation and hardware/software debugging on macOS
//! Hypervisor.framework.

use anyhow::{Result, bail};

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
use crate::hvf::sys;

impl Machine {
    /// Enables hardware breakpoints, hardware watchpoints, and/or single-stepping
    /// on the HVF vCPU.
    pub fn set_debug(&mut self, stepping: Stepping, traps: &[Trap]) -> Result<()> {
        if traps.len() > MAX_TRAPS {
            bail!(
                "ARM64 HVF supports at most {MAX_TRAPS} hardware debug traps (got {})",
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
    /// Any software `BRK #0` breakpoints still installed keep debug exceptions trapped.
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

    /// Programs `DBGBVR/DBGBCR`, `DBGWVR/DBGWCR`, `MDSCR_EL1`, `CPSR.SS`, and
    /// `hv_vcpu_set_trap_debug_exceptions` to reflect the active debug state.
    pub(crate) fn apply_guest_debug(&mut self) -> Result<()> {
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
                    self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGBVR[bp_idx], bvr)?;
                    self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGBCR[bp_idx], bcr)?;
                    bp_idx += 1;
                }
                Trap::Watch {
                    address,
                    len,
                    access,
                } => {
                    let (wvr, wcr) = encode_hw_wp(address, len, access)?;
                    self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGWVR[wp_idx], wvr)?;
                    self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGWCR[wp_idx], wcr)?;
                    wp_idx += 1;
                }
            }
        }
        for i in bp_idx..MAX_TRAPS {
            self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGBCR[i], 0)?;
            self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGBVR[i], 0)?;
        }
        for i in wp_idx..MAX_TRAPS {
            self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGWCR[i], 0)?;
            self.vcpu.set_sys_reg(sys::HV_SYS_REG_DBGWVR[i], 0)?;
        }

        let stepping = self.is_stepping();
        let has_hw = bp_idx > 0 || wp_idx > 0;
        let trap_debug = stepping || has_hw || !self.int3s.replaced.is_empty();

        let mut mdscr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_MDSCR_EL1)?;
        if stepping {
            mdscr |= MDSCR_SS;
        } else {
            mdscr &= !MDSCR_SS;
        }
        if has_hw {
            mdscr |= MDSCR_MDE;
        } else {
            mdscr &= !MDSCR_MDE;
        }
        self.vcpu.set_sys_reg(sys::HV_SYS_REG_MDSCR_EL1, mdscr)?;

        let mut cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        if stepping {
            cpsr |= PSTATE_SS;
        } else {
            cpsr &= !PSTATE_SS;
        }
        self.vcpu.set_reg(sys::HV_REG_CPSR, cpsr)?;

        self.vcpu.set_trap_debug_exceptions(trap_debug)
    }

    /// Re-arms `CPSR.SS` and `MDSCR_EL1.SS` before vCPU entry while single-stepping,
    /// because taking an `EC_SW_STEP_LOWER` exception clears `CPSR.SS`.
    pub(crate) fn arm_single_step_if_needed(&self) -> Result<()> {
        if !self.is_stepping() {
            return Ok(());
        }
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        if cpsr & PSTATE_SS == 0 {
            self.vcpu.set_reg(sys::HV_REG_CPSR, cpsr | PSTATE_SS)?;
        }
        let mdscr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_MDSCR_EL1)?;
        if mdscr & MDSCR_SS == 0 {
            self.vcpu
                .set_sys_reg(sys::HV_SYS_REG_MDSCR_EL1, mdscr | MDSCR_SS)?;
        }
        Ok(())
    }

    /// Re-injects a guest-originated `BRK` synchronous exception into EL1 when
    /// debug exceptions are trapped for software breakpoints elsewhere.
    pub(crate) fn reinject_brk_el1(&self, syndrome: u64) -> Result<()> {
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        let vbar = self.vcpu.get_sys_reg(sys::HV_SYS_REG_VBAR_EL1)?;
        let offset = match cpsr & 0xf {
            0 => 0x400, // Lower EL using AArch64, Synchronous
            5 => 0x200, // Current EL with SP_ELx, Synchronous
            _ => 0x000, // Current EL with SP_EL0, Synchronous
        };
        self.vcpu.set_sys_reg(sys::HV_SYS_REG_ELR_EL1, pc)?;
        self.vcpu.set_sys_reg(sys::HV_SYS_REG_SPSR_EL1, cpsr)?;
        self.vcpu.set_sys_reg(sys::HV_SYS_REG_ESR_EL1, syndrome)?;
        self.vcpu
            .set_reg(sys::HV_REG_PC, vbar.wrapping_add(offset))?;
        // Mask DAIF and enter EL1h (0x3c5), preserving NZCV flags.
        let new_cpsr = (cpsr & 0xf000_0000) | 0x3c5;
        self.vcpu.set_reg(sys::HV_REG_CPSR, new_cpsr)
    }

    pub fn branch_count(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    pub fn set_branch_stop(&mut self, _target: Option<u64>, _margin: u64) -> Result<()> {
        bail!("branch-count breakpoints are not supported on macOS ARM64 HVF")
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
        let Some(off) = physical.checked_sub(RAM_BASE) else {
            bail!("physical address {physical:#x} is below ARM64 RAM_BASE ({RAM_BASE:#x})");
        };
        let mut orig = [0u8; 4];
        self.dev.ram.read(off, &mut orig)?;
        self.dev.ram.write(off, &BRK_INSN)?;
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
        if let Some(off) = physical.checked_sub(RAM_BASE) {
            self.dev.ram.write(off, &orig)?;
        }
        self.apply_guest_debug()
    }

    /// Lets the instruction at the current breakpoint execute once on the next
    /// `run()` without re-triggering that breakpoint.
    pub fn pass_breakpoint(&mut self) -> Result<()> {
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        if let Some((physical, _)) = self.translate_active(pc)?
            && let Some(&orig) = self.int3s.replaced.get(&physical)
            && let Some(off) = physical.checked_sub(RAM_BASE)
        {
            self.dev.ram.write(off, &orig)?;
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
            && let Some(off) = physical.checked_sub(RAM_BASE)
        {
            self.dev.ram.write(off, &BRK_INSN)?;
        }
        self.apply_guest_debug()
    }

    /// Reads the full AArch64 register file (GPRs, active SP, PC, CPSR, V0..V31,
    /// FPCR, FPSR, and TPIDR_EL0) for GDB.
    pub fn arm64_registers(&self) -> Result<Arm64Regs> {
        let mut x = [0u64; 31];
        for (i, slot) in x.iter_mut().enumerate() {
            *slot = self.vcpu.get_reg(i as u32)?;
        }
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        let sp_reg = if (cpsr & 0xf) == 5 {
            sys::HV_SYS_REG_SP_EL1
        } else {
            sys::HV_SYS_REG_SP_EL0
        };
        let sp = self.vcpu.get_sys_reg(sp_reg)?;
        let mut v = [0u128; 32];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = u128::from_le_bytes(self.vcpu.get_simd(i as u32)?);
        }
        let fpcr = self.vcpu.get_reg(sys::HV_REG_FPCR)? as u32;
        let fpsr = self.vcpu.get_reg(sys::HV_REG_FPSR)? as u32;
        let tpidr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TPIDR_EL0)?;
        Ok(Arm64Regs {
            x,
            sp,
            pc,
            cpsr: cpsr as u32,
            v,
            fpcr,
            fpsr,
            tpidr,
        })
    }

    /// Writes the AArch64 core and SIMD/FP registers from GDB.
    pub fn set_arm64_registers(&mut self, regs: &Arm64Regs) -> Result<()> {
        for (i, &val) in regs.x.iter().enumerate() {
            self.vcpu.set_reg(i as u32, val)?;
        }
        let sp_reg = if (regs.cpsr & 0xf) == 5 {
            sys::HV_SYS_REG_SP_EL1
        } else {
            sys::HV_SYS_REG_SP_EL0
        };
        self.vcpu.set_sys_reg(sp_reg, regs.sp)?;
        self.vcpu.set_reg(sys::HV_REG_PC, regs.pc)?;
        self.vcpu.set_reg(sys::HV_REG_CPSR, u64::from(regs.cpsr))?;
        for (i, &val) in regs.v.iter().enumerate() {
            self.vcpu.set_simd(i as u32, val.to_le_bytes())?;
        }
        self.vcpu.set_reg(sys::HV_REG_FPCR, u64::from(regs.fpcr))?;
        self.vcpu.set_reg(sys::HV_REG_FPSR, u64::from(regs.fpsr))?;
        Ok(())
    }

    pub fn registers(&self) -> Result<kvm_regs> {
        let pc = self.vcpu.get_reg(sys::HV_REG_PC)?;
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        Ok(kvm_regs {
            rip: pc,
            rflags: cpsr,
            ..Default::default()
        })
    }

    pub fn named_registers(&self) -> Result<[(&'static str, u64); 18]> {
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        let sp_reg = if (cpsr & 0xf) == 5 {
            sys::HV_SYS_REG_SP_EL1
        } else {
            sys::HV_SYS_REG_SP_EL0
        };
        Ok([
            ("x0", self.vcpu.get_reg(0)?),
            ("x1", self.vcpu.get_reg(1)?),
            ("x2", self.vcpu.get_reg(2)?),
            ("x3", self.vcpu.get_reg(3)?),
            ("x4", self.vcpu.get_reg(4)?),
            ("x5", self.vcpu.get_reg(5)?),
            ("x6", self.vcpu.get_reg(6)?),
            ("x7", self.vcpu.get_reg(7)?),
            ("x8", self.vcpu.get_reg(8)?),
            ("x9", self.vcpu.get_reg(9)?),
            ("x10", self.vcpu.get_reg(10)?),
            ("x11", self.vcpu.get_reg(11)?),
            ("x12", self.vcpu.get_reg(12)?),
            ("x29", self.vcpu.get_reg(29)?),
            ("x30", self.vcpu.get_reg(30)?),
            ("sp", self.vcpu.get_sys_reg(sp_reg)?),
            ("pc", self.vcpu.get_reg(sys::HV_REG_PC)?),
            ("cpsr", cpsr),
        ])
    }

    pub fn set_registers(&mut self, regs: &kvm_regs) -> Result<()> {
        self.vcpu.set_reg(sys::HV_REG_PC, regs.rip)?;
        self.vcpu.set_reg(sys::HV_REG_CPSR, regs.rflags)
    }

    pub fn special_registers(&self) -> Result<kvm_sregs> {
        let ttbr0 = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TTBR0_EL1)?;
        let cpsr = self.vcpu.get_reg(sys::HV_REG_CPSR)?;
        let is_user = (cpsr & 0xf) == 0;
        let tpidr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TPIDR_EL0)?;
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

    pub(crate) fn translate_active(&self, address: u64) -> Result<Option<(u64, u64)>> {
        let sctlr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_SCTLR_EL1)?;
        if sctlr & 1 == 0 {
            return Ok(Some((address, PAGE_SIZE as u64 - (address & 0xfff))));
        }
        let tcr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TCR_EL1)?;
        let upper = (address >> 55) & 1 != 0;
        let (ttbr, txsz) = if upper {
            (
                self.vcpu.get_sys_reg(sys::HV_SYS_REG_TTBR1_EL1)?,
                (tcr >> 16) & 0x3f,
            )
        } else {
            (
                self.vcpu.get_sys_reg(sys::HV_SYS_REG_TTBR0_EL1)?,
                tcr & 0x3f,
            )
        };
        walk_arm64(&self.dev.ram, ttbr, txsz, address)
    }

    pub fn read_virtual(&self, address: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let va = address.wrapping_add(done as u64);
            let Some((pa, avail)) = self.translate_active(va)? else {
                break;
            };
            let chunk = (buf.len() - done).min(avail as usize);
            if self.read_physical(pa, &mut buf[done..done + chunk]).is_err() {
                break;
            }
            done += chunk;
        }
        Ok(done)
    }

    pub fn read_virtual_in(&self, page_table: u64, address: u64, buf: &mut [u8]) -> Result<usize> {
        let tcr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        let mut done = 0;
        while done < buf.len() {
            let va = address.wrapping_add(done as u64);
            let Some((pa, avail)) = walk_arm64(&self.dev.ram, page_table, t0sz, va)? else {
                break;
            };
            let chunk = (buf.len() - done).min(avail as usize);
            if self.read_physical(pa, &mut buf[done..done + chunk]).is_err() {
                break;
            }
            done += chunk;
        }
        Ok(done)
    }

    pub fn physical_in(&self, page_table: u64, address: u64) -> Result<Option<u64>> {
        let sctlr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_SCTLR_EL1)?;
        if sctlr & 1 == 0 {
            return Ok(Some(address));
        }
        let tcr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TCR_EL1)?;
        if (address >> 55) & 1 != 0 {
            let ttbr1 = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TTBR1_EL1)?;
            let t1sz = (tcr >> 16) & 0x3f;
            return Ok(walk_arm64(&self.dev.ram, ttbr1, t1sz, address)?.map(|(pa, _)| pa));
        }
        let t0sz = tcr & 0x3f;
        Ok(walk_arm64(&self.dev.ram, page_table, t0sz, address)?.map(|(pa, _)| pa))
    }

    pub fn mappings_in(
        &self,
        page_table: u64,
        frames: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<(u64, u64)>> {
        let tcr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        mappings_in_arm64(&self.dev.ram, page_table, t0sz, frames)
    }

    pub fn read_physical(&self, physical: u64, buf: &mut [u8]) -> Result<()> {
        let Some(off) = physical.checked_sub(RAM_BASE) else {
            bail!("physical address {physical:#x} is below ARM64 RAM_BASE ({RAM_BASE:#x})");
        };
        self.dev.ram.read(off, buf)?;
        self.int3s.shadow(physical, buf);
        Ok(())
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

    pub fn write_virtual(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let va = address.wrapping_add(done as u64);
            let Some((pa, avail)) = self.translate_active(va)? else {
                bail!("virtual address {va:#x} is not mapped");
            };
            let chunk = (data.len() - done).min(avail as usize);
            let Some(off) = pa.checked_sub(RAM_BASE) else {
                bail!("physical address {pa:#x} is outside guest RAM");
            };
            let mut staged = data[done..done + chunk].to_vec();
            self.int3s.write_through(pa, &mut staged);
            self.dev.ram.write(off, &staged)?;
            done += chunk;
        }
        Ok(())
    }
}
