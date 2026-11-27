//! ARM64 vCPU initial state and CPU model selection on Hypervisor.framework.

use anyhow::{Result, bail};

use crate::CpuModel;
use crate::hvf::kvm::Kvm;
use crate::hvf::sys;
use crate::hvf::{HvfVcpu, max_ipa_bits};

/// EL1h mode with D, A, I, F exceptions masked (`booting.rst` contract).
const CPSR_EL1H_DAIF_MASKED: u64 = 0x3c5;

/// Clean little-endian `SCTLR_EL1` with MMU and caches off and ARMv8 RES1 bits set.
const SCTLR_EL1_RESET: u64 = 0x30c5_0830;

/// Configures a freshly created ARM64 vCPU for the Linux boot protocol:
/// `X0 = dtb_gpa`, `PC = kernel_entry`, `CPSR = EL1h (DAIF masked)`, MMU off,
/// and hides PMU / RNDR ID register bits where HVF allows writing them.
pub fn setup_boot_vcpu(vcpu: &HvfVcpu, kernel_entry: u64, dtb_gpa: u64) -> Result<()> {
    for reg in sys::HV_REG_X0..=sys::HV_REG_X30 {
        vcpu.set_reg(reg, 0)?;
    }
    vcpu.set_reg(sys::HV_REG_X0, dtb_gpa)?;
    vcpu.set_reg(sys::HV_REG_PC, kernel_entry)?;
    vcpu.set_reg(sys::HV_REG_CPSR, CPSR_EL1H_DAIF_MASKED)?;
    vcpu.set_reg(sys::HV_REG_FPCR, 0)?;
    vcpu.set_reg(sys::HV_REG_FPSR, 0)?;
    for q in 0..32 {
        vcpu.set_simd(q, [0u8; 16])?;
    }

    vcpu.set_sys_reg(sys::HV_SYS_REG_SCTLR_EL1, SCTLR_EL1_RESET)?;
    vcpu.set_sys_reg(sys::HV_SYS_REG_SP_EL0, 0)?;
    vcpu.set_sys_reg(sys::HV_SYS_REG_SP_EL1, 0)?;
    vcpu.set_sys_reg(sys::HV_SYS_REG_CNTKCTL_EL1, 0)?;
    vcpu.set_sys_reg(sys::HV_SYS_REG_CNTV_CTL_EL0, 0)?;
    vcpu.set_sys_reg(sys::HV_SYS_REG_CNTV_CVAL_EL0, 0)?;

    // Hide guest PMU (`ID_AA64DFR0_EL1.PMUVer`, bits 11:8) and hardware RNG
    // (`ID_AA64ISAR0_EL1.RNDR`, bits 63:60) where HVF permits overriding them.
    if let Ok(dfr0) = vcpu.get_sys_reg(sys::HV_SYS_REG_ID_AA64DFR0_EL1) {
        let _ = vcpu.set_sys_reg(sys::HV_SYS_REG_ID_AA64DFR0_EL1, dfr0 & !(0xf << 8));
    }
    if let Ok(isar0) = vcpu.get_sys_reg(sys::HV_SYS_REG_ID_AA64ISAR0_EL1) {
        let _ = vcpu.set_sys_reg(sys::HV_SYS_REG_ID_AA64ISAR0_EL1, isar0 & !(0xf << 60));
    }

    Ok(())
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

/// On Apple Silicon HVF, runs always use the host ARM64 CPU model.
pub fn default_for(_kvm: &Kvm) -> Result<DefaultModel> {
    Ok(DefaultModel::HostOnly {
        lacking: "x86-64 (Apple Silicon ARM64 uses --cpu host)".into(),
    })
}

pub fn default_model(_kvm: &Kvm) -> Result<CpuModel> {
    Ok(CpuModel::Host)
}

pub fn check(_kvm: &Kvm, model: CpuModel) -> Result<()> {
    match model {
        CpuModel::Host => Ok(()),
        CpuModel::V3 | CpuModel::V3Compat => bail!(
            "Apple Silicon (ARM64) runs use `--cpu host`; `{}` is an x86-64 CPU model",
            model.cli_name()
        ),
    }
}

pub fn address_bits(_kvm: &Kvm) -> Result<u32> {
    max_ipa_bits()
}
