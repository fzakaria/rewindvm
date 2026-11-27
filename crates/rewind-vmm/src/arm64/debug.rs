//! Shared ARM64 hardware & software debugging primitives used by both macOS
//! Hypervisor.framework and Linux ARM64 KVM.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

/// Maximum hardware debug traps exposed to `rewind-core`.
///
/// ARMv8 guarantees at least 2 (and Apple Silicon / server ARM64 provide 6+)
/// breakpoint registers and 4+ watchpoint registers; matching x86-64's 4 slots
/// lets breakpoints and watchpoints share `rewind-core`'s trap scheduler cleanly.
pub const MAX_TRAPS: usize = 4;

/// AArch64 `BRK #0` software breakpoint instruction (`0xd420_0000`, little-endian).
pub const BRK_INSN: [u8; 4] = 0xd420_0000u32.to_le_bytes();

/// ARM64 exception classes (`ESR_EL2.EC`, bits 31:26) for debug exits.
pub const EC_HW_BP_LOWER: u64 = 0x30;
pub const EC_SW_STEP_LOWER: u64 = 0x32;
pub const EC_WATCHPOINT_LOWER: u64 = 0x34;
pub const EC_BRK64: u64 = 0x3c;

/// `PSTATE.SS` (bit 21): single-step active-not-pending flag in CPSR/SPSR.
pub const PSTATE_SS: u64 = 1 << 21;

/// `MDSCR_EL1.SS` (bit 0): software single-step enable.
pub const MDSCR_SS: u64 = 1 << 0;

/// `MDSCR_EL1.MDE` (bit 15): monitor debug events (hardware breakpoints & watchpoints).
pub const MDSCR_MDE: u64 = 1 << 15;

/// Full AArch64 register file for GDB remote debugging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Arm64Regs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub cpsr: u32,
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
    pub tpidr: u64,
}

/// Compatibility register structure exposing `rip` (PC) and `rflags` (PSTATE)
/// for architecture-neutral code in `rewind-core`.
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

/// Compatibility special-register structure exposing privilege level (`cs`),
/// user TLS base (`fs.base` = `TPIDR_EL0`), and user page table (`cr3` = `TTBR0_EL1`).
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
pub struct TrapSet(pub(crate) u8);

impl TrapSet {
    pub fn contains(self, index: usize) -> bool {
        index < MAX_TRAPS && self.0 & (1 << index) != 0
    }

    pub fn insert(&mut self, index: usize) {
        if index < MAX_TRAPS {
            self.0 |= 1 << index;
        }
    }
}

pub fn trap_set(mask: u64, traps: &[Trap]) -> TrapSet {
    let mut set = TrapSet::default();
    for (i, trap) in traps.iter().enumerate().take(MAX_TRAPS) {
        if mask & 0xf & (1 << i) != 0 && matches!(trap, Trap::Watch { .. }) {
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

/// Active debugger configuration on an ARM64 `Machine`.
#[derive(Clone, Debug)]
pub(crate) struct Debugging {
    pub(crate) stepping: Stepping,
    pub(crate) traps: Vec<Trap>,
}

/// Software breakpoints (`BRK #0`, 4 bytes) written into guest RAM, keyed by
/// 4-byte-aligned guest physical address with the original 4-byte instruction.
#[derive(Clone, Debug, Default)]
pub(crate) struct Int3s {
    pub(crate) replaced: BTreeMap<u64, [u8; 4]>,
    /// Physical address whose original instruction is temporarily restored
    /// while single-stepping past the breakpoint.
    pub(crate) lifted: Option<u64>,
}

impl Int3s {
    /// Overlays the original bytes replaced by `BRK #0` onto `chunk`, just read
    /// from guest physical address `physical`, so memory reads never see `BRK #0`.
    pub(crate) fn shadow(&self, physical: u64, chunk: &mut [u8]) {
        if self.replaced.is_empty() || chunk.is_empty() {
            return;
        }
        let end = physical.saturating_add(chunk.len() as u64);
        let search_start = physical.saturating_sub(3);
        for (&at, orig) in self.replaced.range(search_start..end) {
            for (i, &byte) in orig.iter().enumerate() {
                let addr = at + i as u64;
                if addr >= physical && addr < end {
                    chunk[(addr - physical) as usize] = byte;
                }
            }
        }
    }

    /// Updates saved original bytes when the debugger writes to `physical..physical+data.len()`,
    /// keeping `BRK #0` in memory for any non-lifted software breakpoint overlapping the write.
    pub(crate) fn write_through(&mut self, physical: u64, data: &mut [u8]) {
        if self.replaced.is_empty() || data.is_empty() {
            return;
        }
        let end = physical.saturating_add(data.len() as u64);
        let search_start = physical.saturating_sub(3);
        let lifted = self.lifted;
        for (&at, orig) in self.replaced.range_mut(search_start..end) {
            for (i, slot) in orig.iter_mut().enumerate() {
                let addr = at + i as u64;
                if addr >= physical && addr < end {
                    let idx = (addr - physical) as usize;
                    *slot = data[idx];
                    if lifted != Some(at) {
                        data[idx] = BRK_INSN[i];
                    }
                }
            }
        }
    }
}

/// Encodes an ARM64 hardware execution breakpoint into `(DBGBVR_EL1, DBGBCR_EL1)`.
///
/// `DBGBCR_EL1`: `E = 1` (bit 0), `PMC = 0b11` (EL0 + EL1, bits 2:1),
/// `BAS = 0b1111` (all 4 bytes of the instruction, bits 8:5) -> `0x1e5`.
pub(crate) fn encode_hw_bp(address: u64) -> (u64, u64) {
    const DBGBCR_ENABLE_EL0_EL1: u64 = 1 | (0b11 << 1) | (0xf << 5);
    (address & !3, DBGBCR_ENABLE_EL0_EL1)
}

/// Encodes an ARM64 hardware data watchpoint into `(DBGWVR_EL1, DBGWCR_EL1)`.
///
/// `DBGWVR_EL1` is 8-byte aligned; `BAS` (bits 12:5 of `DBGWCR_EL1`) selects the
/// exact bytes within that 8-byte doubleword. `PAC = 0b11` watches both EL0 and EL1.
pub(crate) fn encode_hw_wp(address: u64, len: u64, access: Access) -> Result<(u64, u64)> {
    if !matches!(len, 1 | 2 | 4 | 8) {
        bail!("ARM64 hardware watchpoint length must be 1, 2, 4, or 8 bytes (got {len})");
    }
    if !address.is_multiple_of(len) {
        bail!("ARM64 hardware watchpoint at {address:#x} is not aligned to {len} bytes");
    }
    let aligned = address & !7;
    let byte_off = (address & 7) as u32;
    let bas = ((1u64 << len) - 1) << byte_off;
    let lsc = match access {
        Access::Write => 0b10u64,
        Access::ReadWrite => 0b11u64,
    };
    let wcr = 1 | (0b11 << 1) | (lsc << 3) | (bas << 5);
    Ok((aligned, wcr))
}

/// Matches a watchpoint fault address (`FAR`) against the debugger's active
/// `traps`, returning the first matched watch's start address and the set of
/// all matching trap indices.
pub(crate) fn match_watchpoints(far: u64, traps: &[Trap]) -> Option<(u64, TrapSet)> {
    let mut first = None;
    let mut set = TrapSet::default();
    for (i, trap) in traps.iter().enumerate().take(MAX_TRAPS) {
        if let Trap::Watch { address, len, .. } = *trap
            && (address..address.saturating_add(len)).contains(&far)
        {
            first = first.or(Some(address));
            set.insert(i);
        }
    }
    if let Some(addr) = first {
        return Some((addr, set));
    }
    // ARM64 allows FAR for a multi-byte access (e.g. STP/LDP or unaligned STR)
    // to report the lowest accessed address in the same 8-byte doubleword.
    let far_dw = far & !7;
    for (i, trap) in traps.iter().enumerate().take(MAX_TRAPS) {
        if let Trap::Watch { address, .. } = *trap
            && (address & !7) == far_dw
        {
            first = first.or(Some(address));
            set.insert(i);
        }
    }
    first.map(|addr| (addr, set))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_hw_breakpoints_and_watchpoints() {
        assert_eq!(encode_hw_bp(0x401004), (0x401004, 0x1e7));

        let (wvr, wcr) = encode_hw_wp(0x1004, 4, Access::Write).unwrap();
        assert_eq!(wvr, 0x1000);
        // E=1, PAC=0b11 (6), LSC=0b10 (0x10), BAS=0xf0 << 5 (0x1e00)
        assert_eq!(wcr, 1 | (3 << 1) | (2 << 3) | (0xf0 << 5));

        let (wvr, wcr) = encode_hw_wp(0x1000, 8, Access::ReadWrite).unwrap();
        assert_eq!(wvr, 0x1000);
        assert_eq!(wcr, 1 | (3 << 1) | (3 << 3) | (0xff << 5));

        assert!(encode_hw_wp(0x1001, 4, Access::Write).is_err());
        assert!(encode_hw_wp(0x1000, 3, Access::Write).is_err());
    }

    #[test]
    fn matches_watchpoints_exact_and_doubleword() {
        let traps = [
            Trap::Execute(0x4000),
            Trap::Watch {
                address: 0x2004,
                len: 4,
                access: Access::Write,
            },
            Trap::Watch {
                address: 0x2000,
                len: 8,
                access: Access::ReadWrite,
            },
        ];
        let (addr, set) = match_watchpoints(0x2006, &traps).unwrap();
        assert_eq!(addr, 0x2004);
        assert!(set.contains(1));
        assert!(set.contains(2));
        assert!(!set.contains(0));

        // Doubleword fallback when FAR is at the start of the 8-byte access.
        let single = [Trap::Watch {
            address: 0x3004,
            len: 4,
            access: Access::Write,
        }];
        let (addr, set) = match_watchpoints(0x3000, &single).unwrap();
        assert_eq!(addr, 0x3004);
        assert!(set.contains(0));
    }

    #[test]
    fn int3s_shadow_and_write_through_partial_overlaps() {
        let mut int3s = Int3s::default();
        int3s.replaced.insert(0x1004, [0x11, 0x22, 0x33, 0x44]);

        let mut buf = [0xaa; 8];
        buf[2..6].copy_from_slice(&BRK_INSN);
        int3s.shadow(0x1002, &mut buf);
        assert_eq!(buf, [0xaa, 0xaa, 0x11, 0x22, 0x33, 0x44, 0xaa, 0xaa]);

        // Write through updates saved bytes while keeping BRK_INSN in the buffer.
        let mut write = [0x99, 0x88, 0x77, 0x66];
        int3s.write_through(0x1006, &mut write);
        assert_eq!(int3s.replaced[&0x1004], [0x11, 0x22, 0x99, 0x88]);
        assert_eq!(write, [BRK_INSN[2], BRK_INSN[3], 0x77, 0x66]);
    }
}
