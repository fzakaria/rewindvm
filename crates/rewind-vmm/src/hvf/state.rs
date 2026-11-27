//! Named ARM64 machine state for keyframe comparison on Hypervisor.framework.

use anyhow::Result;
use serde::Serialize;

use crate::hvf::snapshot::Keyframe;
use crate::hvf::sys;

type Field = (String, Vec<u8>);

/// A named piece of machine state that differs between two keyframes, in hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StateDifference {
    pub name: String,
    pub left: String,
    pub right: String,
}

/// Host-dependent state left out of keyframe comparisons.
pub const IGNORED: &[(&str, &str)] = &[
    (
        "CNTV_CTL_EL0",
        "virtual timer control register; the Rewind ARM64 guest uses its paravirtual timer \
         and masks the architectural vtimer",
    ),
    (
        "CNTV_CVAL_EL0",
        "virtual timer compare value; unused by the Rewind ARM64 guest",
    ),
];

/// Every architectural EL0/EL1 system register captured in an ARM64 keyframe,
/// in deterministic order.
pub(crate) const SAVED_SYS_REGS: &[(u16, &str)] = &[
    (sys::HV_SYS_REG_SCTLR_EL1, "SCTLR_EL1"),
    (sys::HV_SYS_REG_CPACR_EL1, "CPACR_EL1"),
    (sys::HV_SYS_REG_TTBR0_EL1, "TTBR0_EL1"),
    (sys::HV_SYS_REG_TTBR1_EL1, "TTBR1_EL1"),
    (sys::HV_SYS_REG_TCR_EL1, "TCR_EL1"),
    (sys::HV_SYS_REG_MAIR_EL1, "MAIR_EL1"),
    (sys::HV_SYS_REG_AMAIR_EL1, "AMAIR_EL1"),
    (sys::HV_SYS_REG_VBAR_EL1, "VBAR_EL1"),
    (sys::HV_SYS_REG_SP_EL0, "SP_EL0"),
    (sys::HV_SYS_REG_SP_EL1, "SP_EL1"),
    (sys::HV_SYS_REG_SPSR_EL1, "SPSR_EL1"),
    (sys::HV_SYS_REG_ELR_EL1, "ELR_EL1"),
    (sys::HV_SYS_REG_ESR_EL1, "ESR_EL1"),
    (sys::HV_SYS_REG_FAR_EL1, "FAR_EL1"),
    (sys::HV_SYS_REG_PAR_EL1, "PAR_EL1"),
    (sys::HV_SYS_REG_AFSR0_EL1, "AFSR0_EL1"),
    (sys::HV_SYS_REG_AFSR1_EL1, "AFSR1_EL1"),
    (sys::HV_SYS_REG_CONTEXTIDR_EL1, "CONTEXTIDR_EL1"),
    (sys::HV_SYS_REG_TPIDR_EL1, "TPIDR_EL1"),
    (sys::HV_SYS_REG_TPIDR_EL0, "TPIDR_EL0"),
    (sys::HV_SYS_REG_TPIDRRO_EL0, "TPIDRRO_EL0"),
    (sys::HV_SYS_REG_CNTKCTL_EL1, "CNTKCTL_EL1"),
    (sys::HV_SYS_REG_CSSELR_EL1, "CSSELR_EL1"),
    (sys::HV_SYS_REG_MDSCR_EL1, "MDSCR_EL1"),
    (sys::HV_SYS_REG_MDCCINT_EL1, "MDCCINT_EL1"),
    (sys::HV_SYS_REG_APIAKEYLO_EL1, "APIAKEYLO_EL1"),
    (sys::HV_SYS_REG_APIAKEYHI_EL1, "APIAKEYHI_EL1"),
    (sys::HV_SYS_REG_APIBKEYLO_EL1, "APIBKEYLO_EL1"),
    (sys::HV_SYS_REG_APIBKEYHI_EL1, "APIBKEYHI_EL1"),
    (sys::HV_SYS_REG_APDAKEYLO_EL1, "APDAKEYLO_EL1"),
    (sys::HV_SYS_REG_APDAKEYHI_EL1, "APDAKEYHI_EL1"),
    (sys::HV_SYS_REG_APDBKEYLO_EL1, "APDBKEYLO_EL1"),
    (sys::HV_SYS_REG_APDBKEYHI_EL1, "APDBKEYHI_EL1"),
    (sys::HV_SYS_REG_APGAKEYLO_EL1, "APGAKEYLO_EL1"),
    (sys::HV_SYS_REG_APGAKEYHI_EL1, "APGAKEYHI_EL1"),
];

pub fn sys_reg_name(id: u16) -> String {
    SAVED_SYS_REGS
        .iter()
        .find(|&&(r, _)| r == id)
        .map_or_else(|| format!("sysreg {id:#06x}"), |&(_, name)| name.to_string())
}

fn core_reg_name(i: usize) -> String {
    match i {
        0..=30 => format!("X{i}"),
        31 => "PC".into(),
        32 => "FPCR".into(),
        33 => "FPSR".into(),
        34 => "CPSR".into(),
        _ => format!("REG{i}"),
    }
}

fn ignored(name: &str) -> bool {
    IGNORED.iter().any(|(prefix, _)| name.starts_with(prefix))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Keyframe {
    /// Named state differences between this keyframe and `other`.
    pub fn state_differences(&self, other: &Keyframe) -> Result<Vec<StateDifference>> {
        let (left, right) = (self.fields(), other.fields());
        let lookup = |fields: &[Field]| -> std::collections::HashMap<String, Vec<u8>> {
            fields.iter().cloned().collect()
        };
        let (ours, theirs) = (lookup(&left), lookup(&right));

        let mut names: Vec<&str> = left.iter().map(|(n, _)| n.as_str()).collect();
        names.extend(
            right
                .iter()
                .map(|(n, _)| n.as_str())
                .filter(|n| !ours.contains_key(*n)),
        );
        let mut diffs = Vec::new();
        for name in names {
            let (a, b) = (
                ours.get(name).map_or(&[][..], Vec::as_slice),
                theirs.get(name).map_or(&[][..], Vec::as_slice),
            );
            if a == b || ignored(name) {
                continue;
            }
            diffs.push(StateDifference {
                name: name.to_string(),
                left: hex(a),
                right: hex(b),
            });
        }
        Ok(diffs)
    }

    fn fields(&self) -> Vec<Field> {
        let c = &self.cpu;
        let mut fields = Vec::new();
        for (i, &val) in c.regs.iter().enumerate() {
            fields.push((core_reg_name(i), val.to_le_bytes().to_vec()));
        }
        for (q, bytes) in c.simd.iter().enumerate() {
            fields.push((format!("Q{q}"), bytes.to_vec()));
        }
        for &(id, val) in &c.sys_regs {
            fields.push((sys_reg_name(id), val.to_le_bytes().to_vec()));
        }
        fields.push(("irq_pending".into(), vec![u8::from(c.irq_pending)]));

        let d = &self.devices;
        for (name, val) in [
            ("monitor clock", d.now),
            ("monitor timer", d.deadline.unwrap_or(u64::MAX)),
            ("monitor shared page", d.shared.unwrap_or(u64::MAX)),
            ("monitor step", d.step),
            ("monitor branch count", d.branches),
        ] {
            fields.push((name.to_string(), val.to_le_bytes().to_vec()));
        }
        fields
    }
}
