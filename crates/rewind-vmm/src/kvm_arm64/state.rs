//! Named ARM64 KVM machine state for keyframe comparison.

use anyhow::Result;
use kvm_bindings::{
    KVM_REG_ARM_CORE, KVM_REG_ARM_COPROC_MASK, KVM_REG_ARM64_SYSREG,
    KVM_REG_ARM64_SYSREG_CRM_MASK, KVM_REG_ARM64_SYSREG_CRM_SHIFT, KVM_REG_ARM64_SYSREG_CRN_MASK,
    KVM_REG_ARM64_SYSREG_CRN_SHIFT, KVM_REG_ARM64_SYSREG_OP0_MASK, KVM_REG_ARM64_SYSREG_OP0_SHIFT,
    KVM_REG_ARM64_SYSREG_OP1_MASK, KVM_REG_ARM64_SYSREG_OP1_SHIFT, KVM_REG_ARM64_SYSREG_OP2_MASK,
    KVM_REG_ARM64_SYSREG_OP2_SHIFT,
};
use serde::Serialize;

use crate::kvm_arm64::cpu;
use crate::kvm_arm64::snapshot::Keyframe;

type Field = (String, Vec<u8>);

/// A named piece of machine state that differs between two keyframes, in hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StateDifference {
    pub name: String,
    pub left: String,
    pub right: String,
}

/// Host-dependent architectural timer counters left out of keyframe comparisons.
pub const IGNORED: &[(&str, &str)] = &[
    (
        "CNTVCT_EL0",
        "architectural virtual counter; the Rewind ARM64 guest uses its paravirtual clock",
    ),
    (
        "CNTPCT_EL0",
        "architectural physical counter; unused by the Rewind ARM64 guest",
    ),
    (
        "CNTV_CTL_EL0",
        "virtual timer control register; unused by the Rewind ARM64 guest",
    ),
    (
        "CNTV_CVAL_EL0",
        "virtual timer compare value; unused by the Rewind ARM64 guest",
    ),
    (
        "CNTP_CTL_EL0",
        "physical timer control register; unused by the Rewind ARM64 guest",
    ),
    (
        "CNTP_CVAL_EL0",
        "physical timer compare value; unused by the Rewind ARM64 guest",
    ),
];

/// Returns true for architectural counter/timer registers whose values reflect
/// host wall time rather than deterministic guest state.
pub(crate) fn is_host_timer_reg(id: u64) -> bool {
    matches!(
        id,
        cpu::SYS_CNTVCT_EL0
            | cpu::SYS_CNTPCT_EL0
            | cpu::SYS_CNTV_CTL_EL0
            | cpu::SYS_CNTV_CVAL_EL0
            | cpu::SYS_CNTP_CTL_EL0
            | cpu::SYS_CNTP_CVAL_EL0
    )
}

/// Returns true for ARM64 CPU ID registers (`op0=3, op1=0, crn=0`) which are
/// read-only on KVM once a vCPU is initialized.
pub(crate) fn is_id_sysreg(id: u64) -> bool {
    let coproc = (id as u32) & KVM_REG_ARM_COPROC_MASK;
    if coproc != KVM_REG_ARM64_SYSREG {
        return false;
    }
    let op0 = ((id as u32) & KVM_REG_ARM64_SYSREG_OP0_MASK) >> KVM_REG_ARM64_SYSREG_OP0_SHIFT;
    let op1 = ((id as u32) & KVM_REG_ARM64_SYSREG_OP1_MASK) >> KVM_REG_ARM64_SYSREG_OP1_SHIFT;
    let crn = ((id as u32) & KVM_REG_ARM64_SYSREG_CRN_MASK) >> KVM_REG_ARM64_SYSREG_CRN_SHIFT;
    op0 == 3 && op1 == 0 && crn == 0
}

pub fn reg_name(id: u64) -> String {
    match id {
        cpu::REG_SP_EL0 => return "SP_EL0".into(),
        cpu::REG_PC => return "PC".into(),
        cpu::REG_PSTATE => return "PSTATE".into(),
        cpu::SYS_SCTLR_EL1 => return "SCTLR_EL1".into(),
        cpu::SYS_TTBR0_EL1 => return "TTBR0_EL1".into(),
        cpu::SYS_TTBR1_EL1 => return "TTBR1_EL1".into(),
        cpu::SYS_TCR_EL1 => return "TCR_EL1".into(),
        cpu::SYS_CNTKCTL_EL1 => return "CNTKCTL_EL1".into(),
        cpu::SYS_CNTVCT_EL0 => return "CNTVCT_EL0".into(),
        cpu::SYS_CNTPCT_EL0 => return "CNTPCT_EL0".into(),
        cpu::SYS_CNTV_CTL_EL0 => return "CNTV_CTL_EL0".into(),
        cpu::SYS_CNTV_CVAL_EL0 => return "CNTV_CVAL_EL0".into(),
        cpu::SYS_CNTP_CTL_EL0 => return "CNTP_CTL_EL0".into(),
        cpu::SYS_CNTP_CVAL_EL0 => return "CNTP_CVAL_EL0".into(),
        _ => {}
    }
    let coproc = (id as u32) & KVM_REG_ARM_COPROC_MASK;
    if coproc == KVM_REG_ARM_CORE {
        let off = id & 0xffff;
        if off < 62 && off.is_multiple_of(2) {
            return format!("X{}", off / 2);
        }
        return format!("core+{off:#x}");
    }
    if coproc == KVM_REG_ARM64_SYSREG {
        let op0 = ((id as u32) & KVM_REG_ARM64_SYSREG_OP0_MASK) >> KVM_REG_ARM64_SYSREG_OP0_SHIFT;
        let op1 = ((id as u32) & KVM_REG_ARM64_SYSREG_OP1_MASK) >> KVM_REG_ARM64_SYSREG_OP1_SHIFT;
        let crn = ((id as u32) & KVM_REG_ARM64_SYSREG_CRN_MASK) >> KVM_REG_ARM64_SYSREG_CRN_SHIFT;
        let crm = ((id as u32) & KVM_REG_ARM64_SYSREG_CRM_MASK) >> KVM_REG_ARM64_SYSREG_CRM_SHIFT;
        let op2 = ((id as u32) & KVM_REG_ARM64_SYSREG_OP2_MASK) >> KVM_REG_ARM64_SYSREG_OP2_SHIFT;
        return format!("S{op0}_{op1}_C{crn}_C{crm}_{op2}");
    }
    format!("reg {id:#018x}")
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
        let mut fields = Vec::new();
        for (id, bytes) in &self.cpu.regs {
            if is_host_timer_reg(*id) {
                continue;
            }
            fields.push((reg_name(*id), bytes.clone()));
        }
        fields.push(("irq_pending".into(), vec![u8::from(self.cpu.irq_pending)]));

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
