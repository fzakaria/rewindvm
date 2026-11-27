//! Raw FFI bindings to Apple's Hypervisor.framework on ARM64 macOS.
//!
//! Signatures and enum discriminants match the macOS SDK headers
//! (`Hypervisor/hv_vm.h`, `hv_vm_config.h`, `hv_vcpu.h`, `hv_vcpu_types.h`,
//! `hv_vcpu_config.h`).

#![allow(non_camel_case_types, dead_code, reason = "C FFI matching Apple Hypervisor.framework headers")]

use std::ffi::c_void;

pub type hv_return_t = i32;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;
pub type hv_vcpu_t = u64;
pub type hv_vm_config_t = *mut c_void;
pub type hv_vcpu_config_t = *mut c_void;

pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_ERROR: hv_return_t = 0xfae9_4001u32 as i32;
pub const HV_BUSY: hv_return_t = 0xfae9_4002u32 as i32;
pub const HV_BAD_ARGUMENT: hv_return_t = 0xfae9_4003u32 as i32;
pub const HV_ILLEGAL_GUEST_STATE: hv_return_t = 0xfae9_4004u32 as i32;
pub const HV_NO_RESOURCES: hv_return_t = 0xfae9_4005u32 as i32;
pub const HV_NO_DEVICE: hv_return_t = 0xfae9_4006u32 as i32;
pub const HV_DENIED: hv_return_t = 0xfae9_4007u32 as i32;
pub const HV_FAULT: hv_return_t = 0xfae9_4008u32 as i32;
pub const HV_UNSUPPORTED: hv_return_t = 0xfae9_400fu32 as i32;

pub const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;
pub const HV_INTERRUPT_TYPE_FIQ: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: hv_ipa_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

/// 128-bit SIMD/FP register value (`hv_simd_fp_uchar16_t`).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct hv_simd_fp_uchar16_t(pub [u8; 16]);

/// General-purpose and special registers (`hv_reg_t`).
pub const HV_REG_X0: u32 = 0;
pub const HV_REG_X30: u32 = 30;
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_FPCR: u32 = 32;
pub const HV_REG_FPSR: u32 = 33;
pub const HV_REG_CPSR: u32 = 34;

/// System registers (`hv_sys_reg_t`).
pub const HV_SYS_REG_MDCCINT_EL1: u16 = 0x8010;
pub const HV_SYS_REG_MDSCR_EL1: u16 = 0x8012;
pub const HV_SYS_REG_MIDR_EL1: u16 = 0xc000;
pub const HV_SYS_REG_MPIDR_EL1: u16 = 0xc005;
pub const HV_SYS_REG_ID_AA64PFR0_EL1: u16 = 0xc020;
pub const HV_SYS_REG_ID_AA64PFR1_EL1: u16 = 0xc021;
pub const HV_SYS_REG_ID_AA64DFR0_EL1: u16 = 0xc028;
pub const HV_SYS_REG_ID_AA64DFR1_EL1: u16 = 0xc029;
pub const HV_SYS_REG_ID_AA64ISAR0_EL1: u16 = 0xc030;
pub const HV_SYS_REG_ID_AA64ISAR1_EL1: u16 = 0xc031;
pub const HV_SYS_REG_ID_AA64MMFR0_EL1: u16 = 0xc038;
pub const HV_SYS_REG_ID_AA64MMFR1_EL1: u16 = 0xc039;
pub const HV_SYS_REG_ID_AA64MMFR2_EL1: u16 = 0xc03a;
pub const HV_SYS_REG_SCTLR_EL1: u16 = 0xc080;
pub const HV_SYS_REG_CPACR_EL1: u16 = 0xc082;
pub const HV_SYS_REG_TTBR0_EL1: u16 = 0xc100;
pub const HV_SYS_REG_TTBR1_EL1: u16 = 0xc101;
pub const HV_SYS_REG_TCR_EL1: u16 = 0xc102;
pub const HV_SYS_REG_APIAKEYLO_EL1: u16 = 0xc108;
pub const HV_SYS_REG_APIAKEYHI_EL1: u16 = 0xc109;
pub const HV_SYS_REG_APIBKEYLO_EL1: u16 = 0xc10a;
pub const HV_SYS_REG_APIBKEYHI_EL1: u16 = 0xc10b;
pub const HV_SYS_REG_APDAKEYLO_EL1: u16 = 0xc110;
pub const HV_SYS_REG_APDAKEYHI_EL1: u16 = 0xc111;
pub const HV_SYS_REG_APDBKEYLO_EL1: u16 = 0xc112;
pub const HV_SYS_REG_APDBKEYHI_EL1: u16 = 0xc113;
pub const HV_SYS_REG_APGAKEYLO_EL1: u16 = 0xc118;
pub const HV_SYS_REG_APGAKEYHI_EL1: u16 = 0xc119;
pub const HV_SYS_REG_SPSR_EL1: u16 = 0xc200;
pub const HV_SYS_REG_ELR_EL1: u16 = 0xc201;
pub const HV_SYS_REG_SP_EL0: u16 = 0xc208;
pub const HV_SYS_REG_AFSR0_EL1: u16 = 0xc288;
pub const HV_SYS_REG_AFSR1_EL1: u16 = 0xc289;
pub const HV_SYS_REG_ESR_EL1: u16 = 0xc290;
pub const HV_SYS_REG_FAR_EL1: u16 = 0xc300;
pub const HV_SYS_REG_PAR_EL1: u16 = 0xc3a0;
pub const HV_SYS_REG_MAIR_EL1: u16 = 0xc510;
pub const HV_SYS_REG_AMAIR_EL1: u16 = 0xc518;
pub const HV_SYS_REG_VBAR_EL1: u16 = 0xc600;
pub const HV_SYS_REG_CONTEXTIDR_EL1: u16 = 0xc681;
pub const HV_SYS_REG_TPIDR_EL1: u16 = 0xc684;
pub const HV_SYS_REG_CNTKCTL_EL1: u16 = 0xc708;
pub const HV_SYS_REG_CSSELR_EL1: u16 = 0xd000;
pub const HV_SYS_REG_TPIDR_EL0: u16 = 0xde82;
pub const HV_SYS_REG_TPIDRRO_EL0: u16 = 0xde83;
pub const HV_SYS_REG_CNTV_CTL_EL0: u16 = 0xdf19;
pub const HV_SYS_REG_CNTV_CVAL_EL0: u16 = 0xdf1a;
pub const HV_SYS_REG_SP_EL1: u16 = 0xe208;

/// Feature registers (`hv_feature_reg_t`).
pub const HV_FEATURE_REG_ID_AA64DFR0_EL1: u32 = 0;
pub const HV_FEATURE_REG_ID_AA64ISAR0_EL1: u32 = 2;

unsafe extern "C" {
    pub fn os_release(object: *mut c_void);

    pub fn hv_vm_config_create() -> hv_vm_config_t;
    pub fn hv_vm_config_get_max_ipa_size(ipa_bit_length: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_get_default_ipa_size(ipa_bit_length: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_set_ipa_size(config: hv_vm_config_t, ipa_bit_length: u32) -> hv_return_t;

    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(
        addr: *mut c_void,
        ipa: hv_ipa_t,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    pub fn hv_vcpu_config_create() -> hv_vcpu_config_t;
    pub fn hv_vcpu_config_get_feature_reg(
        config: hv_vcpu_config_t,
        feature_reg: u32,
        value: *mut u64,
    ) -> hv_return_t;

    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpus_exit(vcpus: *mut hv_vcpu_t, vcpu_count: u32) -> hv_return_t;

    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;

    pub fn hv_vcpu_get_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *mut hv_simd_fp_uchar16_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: hv_simd_fp_uchar16_t,
    ) -> hv_return_t;

    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;

    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: hv_vcpu_t,
        intr_type: u32,
        pending: bool,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, vtimer_is_masked: bool) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, vtimer_offset: u64) -> hv_return_t;

    pub fn hv_vcpu_set_trap_debug_exceptions(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
    pub fn hv_vcpu_set_trap_debug_reg_accesses(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
}
