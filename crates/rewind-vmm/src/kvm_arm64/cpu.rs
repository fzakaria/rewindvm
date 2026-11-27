//! ARM64 vCPU initial state, register ID helpers, and CPU model selection on Linux KVM.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    KVM_REG_ARM_CORE, KVM_REG_ARM64, KVM_REG_ARM64_SYSREG, KVM_REG_ARM64_SYSREG_CRM_SHIFT,
    KVM_REG_ARM64_SYSREG_CRN_SHIFT, KVM_REG_ARM64_SYSREG_OP0_SHIFT,
    KVM_REG_ARM64_SYSREG_OP1_SHIFT, KVM_REG_ARM64_SYSREG_OP2_SHIFT, KVM_REG_SIZE_U64,
};
use kvm_ioctls::{Cap, Kvm, VcpuFd};

use crate::CpuModel;

/// EL1h mode with D, A, I, F exceptions masked (`booting.rst` contract).
const PSTATE_EL1H_DAIF_MASKED: u64 = 0x3c5;

/// Clean little-endian `SCTLR_EL1` with MMU and caches off and ARMv8 RES1 bits set.
const SCTLR_EL1_RESET: u64 = 0x30c5_0830;

/// Constructs a 64-bit `KVM_REG_ARM_CORE` register ID from its 32-bit word
/// offset inside `struct kvm_regs` (`user_pt_regs` starts at offset 0).
pub const fn core_reg(u32_offset: u64) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | (KVM_REG_ARM_CORE as u64) | u32_offset
}

/// Constructs a 64-bit `KVM_REG_ARM64_SYSREG` register ID.
pub const fn sys_reg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64
        | KVM_REG_SIZE_U64
        | (KVM_REG_ARM64_SYSREG as u64)
        | (op0 << KVM_REG_ARM64_SYSREG_OP0_SHIFT)
        | (op1 << KVM_REG_ARM64_SYSREG_OP1_SHIFT)
        | (crn << KVM_REG_ARM64_SYSREG_CRN_SHIFT)
        | (crm << KVM_REG_ARM64_SYSREG_CRM_SHIFT)
        | (op2 << KVM_REG_ARM64_SYSREG_OP2_SHIFT)
}

pub const REG_X0: u64 = core_reg(0);
pub const REG_SP_EL0: u64 = core_reg(62);
pub const REG_PC: u64 = core_reg(64);
pub const REG_PSTATE: u64 = core_reg(66);

pub const SYS_SCTLR_EL1: u64 = sys_reg(3, 0, 1, 0, 0);
pub const SYS_TTBR0_EL1: u64 = sys_reg(3, 0, 2, 0, 0);
pub const SYS_TTBR1_EL1: u64 = sys_reg(3, 0, 2, 0, 1);
pub const SYS_TCR_EL1: u64 = sys_reg(3, 0, 2, 0, 2);
pub const SYS_CNTKCTL_EL1: u64 = sys_reg(3, 0, 14, 1, 0);
pub const SYS_CNTVCT_EL0: u64 = sys_reg(3, 3, 14, 0, 2);
pub const SYS_CNTPCT_EL0: u64 = sys_reg(3, 3, 14, 0, 1);
pub const SYS_CNTV_CTL_EL0: u64 = sys_reg(3, 3, 14, 3, 1);
pub const SYS_CNTV_CVAL_EL0: u64 = sys_reg(3, 3, 14, 3, 2);
pub const SYS_CNTP_CTL_EL0: u64 = sys_reg(3, 3, 14, 2, 1);
pub const SYS_CNTP_CVAL_EL0: u64 = sys_reg(3, 3, 14, 2, 2);

pub fn get_u64(vcpu: &VcpuFd, id: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    vcpu.get_one_reg(id, &mut buf)
        .with_context(|| format!("KVM_GET_ONE_REG {id:#x}"))?;
    Ok(u64::from_le_bytes(buf))
}

pub fn set_u64(vcpu: &VcpuFd, id: u64, val: u64) -> Result<()> {
    vcpu.set_one_reg(id, &val.to_le_bytes())
        .with_context(|| format!("KVM_SET_ONE_REG {id:#x}"))?;
    Ok(())
}

/// Configures a freshly initialized ARM64 KVM vCPU for the Linux boot protocol:
/// `X0 = dtb_gpa`, `PC = kernel_entry`, `PSTATE = EL1h (DAIF masked)`, MMU off.
pub fn setup_boot_vcpu(vcpu: &VcpuFd, kernel_entry: u64, dtb_gpa: u64) -> Result<()> {
    for i in 0..31u64 {
        set_u64(vcpu, core_reg(i * 2), 0)?;
    }
    set_u64(vcpu, REG_X0, dtb_gpa)?;
    set_u64(vcpu, REG_SP_EL0, 0)?;
    set_u64(vcpu, REG_PC, kernel_entry)?;
    set_u64(vcpu, REG_PSTATE, PSTATE_EL1H_DAIF_MASKED)?;

    set_u64(vcpu, SYS_SCTLR_EL1, SCTLR_EL1_RESET)?;
    set_u64(vcpu, SYS_CNTKCTL_EL1, 0)?;
    let _ = set_u64(vcpu, SYS_CNTVCT_EL0, 0);
    let _ = set_u64(vcpu, SYS_CNTV_CTL_EL0, 0);
    let _ = set_u64(vcpu, SYS_CNTV_CVAL_EL0, 0);
    Ok(())
}

/// On heterogeneous ARM64 hosts (e.g. Apple Silicon P-cores + E-cores under
/// Linux KVM), `MIDR_EL1` reflects whichever physical CPU the vCPU runs on.
/// Pins the current thread to all online CPUs sharing `cpu0`'s `MIDR_EL1` so
/// every recording and replay sees the exact same CPU identification.
/// No-op on homogeneous hosts where all CPUs share one `MIDR_EL1`.
pub fn pin_homogeneous_cores() {
    let Ok(base_midr) =
        std::fs::read_to_string("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1")
    else {
        return;
    };
    let base_midr = base_midr.trim();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return;
    };
    let mut matching = Vec::new();
    let mut heterogeneous = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(cpu_str) = name.to_str().and_then(|s| s.strip_prefix("cpu")) else {
            continue;
        };
        let Ok(cpu_id) = cpu_str.parse::<usize>() else {
            continue;
        };
        let path = entry.path().join("regs/identification/midr_el1");
        let Ok(midr) = std::fs::read_to_string(path) else {
            continue;
        };
        if midr.trim() == base_midr {
            matching.push(cpu_id);
        } else {
            heterogeneous = true;
        }
    }
    if !heterogeneous || matching.is_empty() {
        return;
    }
    // SAFETY: `cpu_set_t` is zero-initialized and populated only with valid CPU indices
    // below `CPU_SETSIZE`, then passed to `sched_setaffinity` for the current thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for cpu in matching {
            if cpu < libc::CPU_SETSIZE as usize {
                libc::CPU_SET(cpu, &mut set);
            }
        }
        let _ = libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
    }
}

/// The model a run that asks for none gets on this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultModel {
    V3,
    V3Compat { lacking: String },
    HostOnly { lacking: String },
}

impl DefaultModel {
    pub fn model(&self) -> Result<CpuModel> {
        match self {
            DefaultModel::V3 => Ok(CpuModel::V3),
            DefaultModel::V3Compat { .. } => Ok(CpuModel::V3Compat),
            DefaultModel::HostOnly { .. } => Ok(CpuModel::Host),
        }
    }
}

/// On ARM64 KVM, runs use the host ARM64 CPU model.
pub fn default_for(_kvm: &Kvm) -> Result<DefaultModel> {
    Ok(DefaultModel::HostOnly {
        lacking: "x86-64 (ARM64 KVM uses --cpu host)".into(),
    })
}

pub fn default_model(_kvm: &Kvm) -> Result<CpuModel> {
    Ok(CpuModel::Host)
}

pub fn check(_kvm: &Kvm, model: CpuModel) -> Result<()> {
    match model {
        CpuModel::Host => Ok(()),
        CpuModel::V3 | CpuModel::V3Compat => bail!(
            "ARM64 runs use `--cpu host`; `{}` is an x86-64 CPU model",
            model.cli_name()
        ),
    }
}

/// The maximum IPA width supported by KVM on this ARM64 host, in bits.
pub fn address_bits(kvm: &Kvm) -> Result<u32> {
    let bits = kvm.check_extension_int(Cap::ArmVmIPASize);
    Ok(if bits > 0 { bits as u32 } else { 40 })
}
