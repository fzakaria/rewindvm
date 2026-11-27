//! ARM64 guest memory translation and debugging primitives for Linux KVM.

use anyhow::{Result, bail};

use crate::Machine;
use crate::arm64::layout::{PAGE_SIZE, RAM_BASE};
use crate::arm64::pagetable::walk_arm64;
use crate::kvm_arm64::cpu;

/// Maximum hardware debug traps exposed by the debug interface.
pub const MAX_TRAPS: usize = 4;

/// Placeholder register structure matching the `rewind-core` debug interface.
#[allow(
    non_camel_case_types,
    reason = "matches x86 kvm_bindings type name used by rewind-core"
)]
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

#[allow(
    non_camel_case_types,
    reason = "matches x86 kvm_bindings type name used by rewind-core"
)]
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
    #[allow(dead_code, reason = "matches x86 KVM Int3s interface")]
    pub(crate) fn clear(&mut self) {
        self.replaced.clear();
    }
}

impl Machine {
    pub fn set_debug(&mut self, _stepping: Stepping, _traps: &[Trap]) -> Result<()> {
        bail!("interactive hardware debugging is not yet implemented on ARM64 KVM")
    }

    pub fn clear_debug(&mut self) -> Result<()> {
        Ok(())
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

    pub fn insert_int3(&mut self, _physical: u64) -> Result<()> {
        bail!("software breakpoints are not yet implemented on ARM64 KVM")
    }

    pub fn remove_int3(&mut self, _physical: u64) -> Result<()> {
        Ok(())
    }

    pub fn pass_breakpoint(&mut self) -> Result<()> {
        bail!("software breakpoints are not yet implemented on ARM64 KVM")
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
            ("sp", cpu::get_u64(&self.vcpu, cpu::REG_SP_EL0)?),
            ("pc", cpu::get_u64(&self.vcpu, cpu::REG_PC)?),
            ("pstate", cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?),
        ])
    }

    pub fn set_registers(&mut self, _regs: &kvm_regs) -> Result<()> {
        bail!("modifying guest registers via GDB is not yet implemented on ARM64 KVM")
    }

    pub fn special_registers(&self) -> Result<kvm_sregs> {
        let ttbr0 = cpu::get_u64(&self.vcpu, cpu::SYS_TTBR0_EL1)?;
        let pstate = cpu::get_u64(&self.vcpu, cpu::REG_PSTATE)?;
        let is_user = (pstate & 0xf) == 0;
        Ok(kvm_sregs {
            cs: SegmentStub {
                dpl: if is_user { 3 } else { 0 },
                ..Default::default()
            },
            cr3: ttbr0,
            ..Default::default()
        })
    }

    pub fn debug_run(
        &mut self,
        _until: Option<u64>,
        _obs: &mut dyn crate::Observer,
    ) -> Result<crate::Outcome> {
        bail!("debug_run is not yet implemented on ARM64 KVM")
    }

    pub fn read_physical(&self, address: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.ram_read(address, buf)
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
            if self.dev.ram_read(gpa, &mut buf[done..done + take]).is_err() {
                break;
            }
            done += take;
        }
        Ok(done)
    }

    pub fn read_guest(&self, address: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.read_virtual(address, buf)?;
        if n > 0 && !self.int3s.replaced.is_empty() {
            for (i, b) in buf[..n].iter_mut().enumerate() {
                if let Some((gpa, _)) = self.translate(address.wrapping_add(i as u64))?
                    && let Some(&orig) = self.int3s.replaced.get(&gpa)
                {
                    *b = orig;
                }
            }
        }
        Ok(n)
    }

    pub fn write_virtual(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let va = address.wrapping_add(done as u64);
            let Some((gpa, avail)) = self.translate(va)? else {
                bail!("virtual address {va:#x} is not mapped");
            };
            let take = (data.len() - done).min(avail as usize);
            self.dev.ram_write(gpa, &data[done..done + take])?;
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
            if self.dev.ram_read(gpa, &mut buf[done..done + take]).is_err() {
                break;
            }
            done += take;
        }
        Ok(done)
    }

    pub fn physical_in(&self, page_table: u64, address: u64) -> Result<Option<u64>> {
        let tcr = cpu::get_u64(&self.vcpu, cpu::SYS_TCR_EL1)?;
        let t0sz = tcr & 0x3f;
        Ok(walk_arm64(&self.dev.ram, page_table, t0sz, address)?.map(|(pa, _)| pa))
    }

    pub fn mappings_in(
        &self,
        _root: u64,
        _wanted: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<(u64, u64)>> {
        Ok(Vec::new())
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
