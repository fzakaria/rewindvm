//! ARM64 guest memory translation and debugging primitives for HVF.
//!
//! Includes a complete ARM64 4 KiB-granule page-table walker (`TTBR0_EL1` /
//! `TTBR1_EL1` + `TCR_EL1`) so virtual memory inspection and task-list walking
//! work on Apple Silicon. Full GDB remote protocol / hardware breakpoint
//! support is staged for a follow-up phase.

use anyhow::{Result, bail};

use crate::Machine;
use crate::arm64::layout::{PAGE_SIZE, RAM_BASE};
use crate::arm64::pagetable::walk_arm64;
use crate::hvf::sys;

/// Maximum hardware debug traps exposed by the debug interface.
pub const MAX_TRAPS: usize = 4;

/// Placeholder register structure matching the `rewind-core` debug interface.
#[allow(non_camel_case_types, reason = "matches kvm_bindings type name used by rewind-core")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct kvm_regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentStub {
    pub base: u64,
    pub selector: u16,
    pub dpl: u8,
}

#[allow(non_camel_case_types, reason = "matches kvm_bindings type name used by rewind-core")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct kvm_sregs {
    pub cs: SegmentStub,
    pub ds: SegmentStub,
    pub es: SegmentStub,
    pub fs: SegmentStub,
    pub gs: SegmentStub,
    pub ss: SegmentStub,
    pub tr: SegmentStub,
    pub cr3: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stepping {
    No,
    Yes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    Execute(u64),
    Watch {
        address: u64,
        len: u64,
        access: Access,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugStop {
    Step,
    Breakpoint(u64),
    Watchpoint { address: u64, traps: TrapSet },
    Branches,
    PassedBranches,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrapSet(u8);

impl TrapSet {
    pub fn contains(self, index: usize) -> bool {
        index < MAX_TRAPS && self.0 & (1 << index) != 0
    }
}

pub fn trap_set(dr6: u64, traps: &[Trap]) -> TrapSet {
    let mut set = TrapSet::default();
    for (i, trap) in traps.iter().enumerate().take(MAX_TRAPS) {
        if dr6 & 0xf & (1 << i) != 0 && matches!(trap, Trap::Watch { .. }) {
            set.0 |= 1 << i;
        }
    }
    set
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExceptionEntry {
    pub address: u64,
    pub rdi_saved_at: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Int3s {
    pub(crate) replaced: std::collections::HashMap<u64, u8>,
}

impl Int3s {
    #[allow(dead_code, reason = "matches KVM Int3s interface")]
    pub(crate) fn clear(&mut self) {
        self.replaced.clear();
    }
}

impl Machine {
    pub fn set_debug(&mut self, _stepping: Stepping, _traps: &[Trap]) -> Result<()> {
        bail!("interactive hardware debugging is not yet implemented on macOS ARM64 HVF")
    }

    pub fn clear_debug(&mut self) -> Result<()> {
        Ok(())
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

    pub fn insert_int3(&mut self, _physical: u64) -> Result<()> {
        bail!("software breakpoints are not yet implemented on macOS ARM64 HVF")
    }

    pub fn remove_int3(&mut self, _physical: u64) -> Result<()> {
        Ok(())
    }

    pub fn pass_breakpoint(&mut self) -> Result<()> {
        bail!("software breakpoints are not yet implemented on macOS ARM64 HVF")
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
            ("sp", self.vcpu.get_sys_reg(sys::HV_SYS_REG_SP_EL0)?),
            ("pc", self.vcpu.get_reg(sys::HV_REG_PC)?),
            ("cpsr", self.vcpu.get_reg(sys::HV_REG_CPSR)?),
        ])
    }

    pub fn set_registers(&mut self, _regs: &kvm_regs) -> Result<()> {
        bail!("modifying guest registers via GDB is not yet implemented on macOS ARM64 HVF")
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

    fn translate_active(&self, address: u64) -> Result<Option<(u64, u64)>> {
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
        let tcr = self.vcpu.get_sys_reg(sys::HV_SYS_REG_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        Ok(walk_arm64(&self.dev.ram, page_table, t0sz, address)?.map(|(pa, _)| pa))
    }

    pub fn mappings_in(
        &self,
        _page_table: u64,
        _frames: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<(u64, u64)>> {
        Ok(Vec::new())
    }

    pub fn read_physical(&self, physical: u64, buf: &mut [u8]) -> Result<()> {
        let Some(off) = physical.checked_sub(RAM_BASE) else {
            bail!("physical address {physical:#x} is below ARM64 RAM_BASE ({RAM_BASE:#x})");
        };
        self.dev.ram.read(off, buf)
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
            self.dev.ram.write(off, &data[done..done + chunk])?;
            done += chunk;
        }
        Ok(())
    }
}
