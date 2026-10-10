//! The registers `rewind gdb`'s stub serves: gdb's x86-64 core and SSE
//! registers, as gdbstub_arch has them, then Linux's orig_rax and the FS
//! and GS bases. gdb finds thread-local variables through the FS base:
//! errno, C's `__thread`, Rust's `thread_local!`, and the thread state
//! Python's gdb extension reads its frames from.
//!
//! gdbstub_arch's x86-64 architecture describes only the SSE feature and
//! leaves gdb its built-in registers, which have no FS base. This one
//! describes every register, in the order gdb's own feature files list
//! them (gdb/features/i386/64bit-core.xml, 64bit-sse.xml, 64bit-linux.xml
//! and 64bit-segments.xml), so gdb numbers them as the register packet
//! sends them. gdb sets up its Linux support, shared libraries and
//! thread-local variables included, only for a description with the
//! Linux feature.

use std::num::NonZeroUsize;

use gdbstub::arch::{Arch, RegId, Registers};
use gdbstub_arch::x86::reg::X86_64CoreRegs;
use gdbstub_arch::x86::reg::id::X86_64CoreRegId;

/// gdb's numbers for orig_rax and the FS and GS bases: after the 57 core
/// and SSE registers, mxcsr the last of them.
const ORIG_RAX: usize = 57;
const FS_BASE: usize = 58;
const GS_BASE: usize = 59;

/// The bytes of each of the three in the register packet.
const EXTRA_LEN: usize = 8;

/// Where the three start in the register packet: after the core and SSE
/// registers, as gdbstub_arch serializes them.
const CORE_LEN: usize = 0x218;

/// x86-64 with the FS and GS bases.
pub enum X86_64WithBases {}

impl Arch for X86_64WithBases {
    type Usize = u64;
    type Registers = RegsWithBases;
    type RegId = RegIdWithBases;
    type BreakpointKind = usize;

    fn target_description_xml() -> Option<&'static str> {
        Some(TARGET_XML)
    }
}

/// The core and SSE registers, then orig_rax, the system call a thread
/// in the kernel entered it for, and the FS and GS bases, each None when
/// the thread's is not known, which gdb shows as unavailable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegsWithBases {
    pub core: X86_64CoreRegs,
    pub orig_rax: Option<u64>,
    pub fs_base: Option<u64>,
    pub gs_base: Option<u64>,
}

impl Registers for RegsWithBases {
    type ProgramCounter = u64;

    fn pc(&self) -> u64 {
        self.core.rip
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        self.core.gdb_serialize(&mut write_byte);

        // Each of the three whole, or each of its bytes unavailable.
        for extra in [self.orig_rax, self.fs_base, self.gs_base] {
            match extra {
                Some(value) => value
                    .to_le_bytes()
                    .iter()
                    .for_each(|b| write_byte(Some(*b))),
                None => (0..EXTRA_LEN).for_each(|_| write_byte(None)),
            }
        }
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.core.gdb_deserialize(bytes)?;

        // A packet that stops at the core registers leaves the rest alone.
        let extra = |i: usize| -> Option<u64> {
            let at = CORE_LEN + i * EXTRA_LEN;
            let chunk = bytes.get(at..at + EXTRA_LEN)?;
            Some(u64::from_le_bytes(chunk.try_into().ok()?))
        };
        let fields = [&mut self.orig_rax, &mut self.fs_base, &mut self.gs_base];
        for (i, field) in fields.into_iter().enumerate() {
            if let Some(value) = extra(i) {
                *field = Some(value);
            }
        }
        Ok(())
    }
}

/// A register by gdb's number: a core or SSE one, orig_rax, or a base.
#[derive(Clone, Copy, Debug)]
pub enum RegIdWithBases {
    Core(X86_64CoreRegId),
    OrigRax,
    FsBase,
    GsBase,
}

impl RegId for RegIdWithBases {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        let extra_len = NonZeroUsize::new(EXTRA_LEN);
        match id {
            ORIG_RAX => Some((RegIdWithBases::OrigRax, extra_len)),
            FS_BASE => Some((RegIdWithBases::FsBase, extra_len)),
            GS_BASE => Some((RegIdWithBases::GsBase, extra_len)),
            _ => X86_64CoreRegId::from_raw_id(id).map(|(r, len)| (RegIdWithBases::Core(r), len)),
        }
    }
}

/// gdb's x86-64 core, SSE, Linux and segments features, copied from gdb
/// 16's feature files with their registers in the same order.
const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
<architecture>i386:x86-64</architecture>
<feature name="org.gnu.gdb.i386.core">
  <flags id="i386_eflags" size="4">
    <field name="CF" start="0" end="0"/>
    <field name="" start="1" end="1"/>
    <field name="PF" start="2" end="2"/>
    <field name="AF" start="4" end="4"/>
    <field name="ZF" start="6" end="6"/>
    <field name="SF" start="7" end="7"/>
    <field name="TF" start="8" end="8"/>
    <field name="IF" start="9" end="9"/>
    <field name="DF" start="10" end="10"/>
    <field name="OF" start="11" end="11"/>
    <field name="NT" start="14" end="14"/>
    <field name="RF" start="16" end="16"/>
    <field name="VM" start="17" end="17"/>
    <field name="AC" start="18" end="18"/>
    <field name="VIF" start="19" end="19"/>
    <field name="VIP" start="20" end="20"/>
    <field name="ID" start="21" end="21"/>
  </flags>
  <reg name="rax" bitsize="64" type="int64"/>
  <reg name="rbx" bitsize="64" type="int64"/>
  <reg name="rcx" bitsize="64" type="int64"/>
  <reg name="rdx" bitsize="64" type="int64"/>
  <reg name="rsi" bitsize="64" type="int64"/>
  <reg name="rdi" bitsize="64" type="int64"/>
  <reg name="rbp" bitsize="64" type="data_ptr"/>
  <reg name="rsp" bitsize="64" type="data_ptr"/>
  <reg name="r8" bitsize="64" type="int64"/>
  <reg name="r9" bitsize="64" type="int64"/>
  <reg name="r10" bitsize="64" type="int64"/>
  <reg name="r11" bitsize="64" type="int64"/>
  <reg name="r12" bitsize="64" type="int64"/>
  <reg name="r13" bitsize="64" type="int64"/>
  <reg name="r14" bitsize="64" type="int64"/>
  <reg name="r15" bitsize="64" type="int64"/>
  <reg name="rip" bitsize="64" type="code_ptr"/>
  <reg name="eflags" bitsize="32" type="i386_eflags"/>
  <reg name="cs" bitsize="32" type="int32"/>
  <reg name="ss" bitsize="32" type="int32"/>
  <reg name="ds" bitsize="32" type="int32"/>
  <reg name="es" bitsize="32" type="int32"/>
  <reg name="fs" bitsize="32" type="int32"/>
  <reg name="gs" bitsize="32" type="int32"/>
  <reg name="st0" bitsize="80" type="i387_ext"/>
  <reg name="st1" bitsize="80" type="i387_ext"/>
  <reg name="st2" bitsize="80" type="i387_ext"/>
  <reg name="st3" bitsize="80" type="i387_ext"/>
  <reg name="st4" bitsize="80" type="i387_ext"/>
  <reg name="st5" bitsize="80" type="i387_ext"/>
  <reg name="st6" bitsize="80" type="i387_ext"/>
  <reg name="st7" bitsize="80" type="i387_ext"/>
  <reg name="fctrl" bitsize="32" type="int" group="float"/>
  <reg name="fstat" bitsize="32" type="int" group="float"/>
  <reg name="ftag" bitsize="32" type="int" group="float"/>
  <reg name="fiseg" bitsize="32" type="int" group="float"/>
  <reg name="fioff" bitsize="32" type="int" group="float"/>
  <reg name="foseg" bitsize="32" type="int" group="float"/>
  <reg name="fooff" bitsize="32" type="int" group="float"/>
  <reg name="fop" bitsize="32" type="int" group="float"/>
</feature>
<feature name="org.gnu.gdb.i386.sse">
  <vector id="v8bf16" type="bfloat16" count="8"/>
  <vector id="v8h" type="ieee_half" count="8"/>
  <vector id="v4f" type="ieee_single" count="4"/>
  <vector id="v2d" type="ieee_double" count="2"/>
  <vector id="v16i8" type="int8" count="16"/>
  <vector id="v8i16" type="int16" count="8"/>
  <vector id="v4i32" type="int32" count="4"/>
  <vector id="v2i64" type="int64" count="2"/>
  <union id="vec128">
    <field name="v8_bfloat16" type="v8bf16"/>
    <field name="v8_half" type="v8h"/>
    <field name="v4_float" type="v4f"/>
    <field name="v2_double" type="v2d"/>
    <field name="v16_int8" type="v16i8"/>
    <field name="v8_int16" type="v8i16"/>
    <field name="v4_int32" type="v4i32"/>
    <field name="v2_int64" type="v2i64"/>
    <field name="uint128" type="uint128"/>
  </union>
  <flags id="i386_mxcsr" size="4">
    <field name="IE" start="0" end="0"/>
    <field name="DE" start="1" end="1"/>
    <field name="ZE" start="2" end="2"/>
    <field name="OE" start="3" end="3"/>
    <field name="UE" start="4" end="4"/>
    <field name="PE" start="5" end="5"/>
    <field name="DAZ" start="6" end="6"/>
    <field name="IM" start="7" end="7"/>
    <field name="DM" start="8" end="8"/>
    <field name="ZM" start="9" end="9"/>
    <field name="OM" start="10" end="10"/>
    <field name="UM" start="11" end="11"/>
    <field name="PM" start="12" end="12"/>
    <field name="FZ" start="15" end="15"/>
  </flags>
  <reg name="xmm0" bitsize="128" type="vec128" regnum="40"/>
  <reg name="xmm1" bitsize="128" type="vec128"/>
  <reg name="xmm2" bitsize="128" type="vec128"/>
  <reg name="xmm3" bitsize="128" type="vec128"/>
  <reg name="xmm4" bitsize="128" type="vec128"/>
  <reg name="xmm5" bitsize="128" type="vec128"/>
  <reg name="xmm6" bitsize="128" type="vec128"/>
  <reg name="xmm7" bitsize="128" type="vec128"/>
  <reg name="xmm8" bitsize="128" type="vec128"/>
  <reg name="xmm9" bitsize="128" type="vec128"/>
  <reg name="xmm10" bitsize="128" type="vec128"/>
  <reg name="xmm11" bitsize="128" type="vec128"/>
  <reg name="xmm12" bitsize="128" type="vec128"/>
  <reg name="xmm13" bitsize="128" type="vec128"/>
  <reg name="xmm14" bitsize="128" type="vec128"/>
  <reg name="xmm15" bitsize="128" type="vec128"/>
  <reg name="mxcsr" bitsize="32" type="i386_mxcsr" group="vector"/>
</feature>
<feature name="org.gnu.gdb.i386.linux">
  <reg name="orig_rax" bitsize="64" type="int" regnum="57"/>
</feature>
<feature name="org.gnu.gdb.i386.segments">
  <reg name="fs_base" bitsize="64" type="int" regnum="58"/>
  <reg name="gs_base" bitsize="64" type="int"/>
</feature>
</target>"#;

#[cfg(test)]
mod tests {
    // The register packet against the target description: every byte the
    // packet sends belongs to a register gdb numbers, in order, with the
    // bases last.
    use super::*;

    /// The packet's bytes, None for an unavailable one.
    fn packet(regs: &RegsWithBases) -> Vec<Option<u8>> {
        let mut bytes = Vec::new();
        regs.gdb_serialize(|b| bytes.push(b));
        bytes
    }

    /// The packet is as long as the registers gdb numbers from 0 add up
    /// to, and the description lists as many registers as there are
    /// numbers, orig_rax at 57 and the bases at 58 and 59.
    #[test]
    fn the_packet_matches_the_registers_gdb_numbers() {
        let mut total = 0;
        let mut count = 0;
        while let Some((_, len)) = RegIdWithBases::from_raw_id(count) {
            total += len.unwrap().get();
            count += 1;
        }
        assert_eq!(packet(&RegsWithBases::default()).len(), total);
        assert_eq!(count, GS_BASE + 1);
        assert_eq!(TARGET_XML.matches("<reg ").count(), count);
        assert!(matches!(
            RegIdWithBases::from_raw_id(FS_BASE),
            Some((RegIdWithBases::FsBase, _))
        ));
    }

    /// orig_rax and the bases follow the core registers, a known one
    /// whole and an unknown one as unavailable bytes; a packet gdb writes
    /// back keeps them.
    #[test]
    fn the_bases_follow_the_core_registers() {
        let regs = RegsWithBases {
            orig_rax: None,
            fs_base: Some(0x7f00_0000_1234),
            gs_base: None,
            ..Default::default()
        };
        let bytes = packet(&regs);
        let at = |i: usize| &bytes[CORE_LEN + i * EXTRA_LEN..CORE_LEN + (i + 1) * EXTRA_LEN];
        assert!(at(0).iter().all(Option::is_none));
        let fs: Vec<u8> = at(1).iter().map(|b| b.unwrap()).collect();
        assert_eq!(fs, 0x7f00_0000_1234u64.to_le_bytes());
        assert!(at(2).iter().all(Option::is_none));

        let written: Vec<u8> = bytes.iter().map(|b| b.unwrap_or(0)).collect();
        let mut back = RegsWithBases::default();
        back.gdb_deserialize(&written).unwrap();
        assert_eq!(back.fs_base, Some(0x7f00_0000_1234));
        assert_eq!(back.gs_base, Some(0));
    }
}
