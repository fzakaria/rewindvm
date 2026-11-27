//! Safe wrappers around Apple's Hypervisor.framework (ARM64).

pub mod cpu;
pub mod debug;
pub mod dirty;
pub mod kvm;
pub mod machine;
pub mod pmu;
pub mod snapshot;
pub mod state;
pub mod sys;

pub use crate::arm64::{boot, fdt, layout};

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

/// Tracks whether a VM instance already exists in this process (HVF allows
/// only one per process).
static VM_LIVE: AtomicBool = AtomicBool::new(false);

/// Turns an `hv_return_t` into a descriptive error.
pub fn check_hv(ret: sys::hv_return_t, what: &str) -> Result<()> {
    if ret == sys::HV_SUCCESS {
        return Ok(());
    }
    let name = match ret {
        sys::HV_ERROR => "HV_ERROR",
        sys::HV_BUSY => "HV_BUSY (Hypervisor.framework allows only one VM per process)",
        sys::HV_BAD_ARGUMENT => "HV_BAD_ARGUMENT",
        sys::HV_ILLEGAL_GUEST_STATE => "HV_ILLEGAL_GUEST_STATE",
        sys::HV_NO_RESOURCES => "HV_NO_RESOURCES",
        sys::HV_NO_DEVICE => "HV_NO_DEVICE",
        sys::HV_DENIED => {
            "HV_DENIED (binary must be codesigned with com.apple.security.hypervisor entitlement)"
        }
        sys::HV_FAULT => "HV_FAULT",
        sys::HV_UNSUPPORTED => "HV_UNSUPPORTED",
        _ => "unknown hv_return_t",
    };
    bail!("{what}: {name} ({:#x})", ret as u32);
}

/// The maximum intermediate physical address width this Apple Silicon host
/// supports, in bits.
pub fn max_ipa_bits() -> Result<u32> {
    let mut bits = 0u32;
    // SAFETY: `bits` is a valid out pointer.
    let ret = unsafe { sys::hv_vm_config_get_max_ipa_size(&mut bits) };
    check_hv(ret, "hv_vm_config_get_max_ipa_size")?;
    Ok(bits)
}

/// Owns the process-global HVF VM instance and destroys it on drop.
#[derive(Debug)]
pub struct HvfVm {
    ipa_bits: u32,
}

impl HvfVm {
    /// Creates the HVF VM with the widest supported IPA space (up to 40 bits).
    pub fn create() -> Result<Self> {
        if VM_LIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!(
                "cannot create a second VM in the same process: macOS Hypervisor.framework \
                 allows only one VM per process"
            );
        }
        let result = (|| -> Result<Self> {
            // SAFETY: creates a retained config object released before returning.
            let cfg = unsafe { sys::hv_vm_config_create() };
            let mut max_bits = 36u32;
            // SAFETY: valid out pointer.
            let _ = unsafe { sys::hv_vm_config_get_max_ipa_size(&mut max_bits) };
            let ipa_bits = max_bits.min(40);
            // SAFETY: valid config pointer and supported IPA size.
            let set_ret = unsafe { sys::hv_vm_config_set_ipa_size(cfg, ipa_bits) };
            if set_ret != sys::HV_SUCCESS {
                // Fall back to default IPA size if setting explicit size fails.
                let mut def = 36u32;
                // SAFETY: valid out pointer.
                let _ = unsafe { sys::hv_vm_config_get_default_ipa_size(&mut def) };
                // SAFETY: releases the config object we created.
                unsafe { sys::os_release(cfg) };
                // SAFETY: null uses default VM configuration.
                let ret = unsafe { sys::hv_vm_create(std::ptr::null_mut()) };
                check_hv(ret, "hv_vm_create")?;
                return Ok(HvfVm { ipa_bits: def });
            }
            // SAFETY: valid config pointer.
            let ret = unsafe { sys::hv_vm_create(cfg) };
            // SAFETY: releases the config object we created.
            unsafe { sys::os_release(cfg) };
            check_hv(ret, "hv_vm_create")?;
            Ok(HvfVm { ipa_bits })
        })();

        if result.is_err() {
            VM_LIVE.store(false, Ordering::SeqCst);
        }
        result
    }

    pub fn ipa_bits(&self) -> u32 {
        self.ipa_bits
    }

    /// Maps host memory into guest IPA space.
    ///
    /// # Safety
    /// `host_addr..host_addr + size` must remain valid and mapped for as long
    /// as this IPA range stays mapped in the VM, and both addresses and `size`
    /// must be multiples of the host page size (16 KiB on Apple Silicon).
    pub unsafe fn map(
        &self,
        host_addr: u64,
        ipa: u64,
        size: usize,
        flags: sys::hv_memory_flags_t,
    ) -> Result<()> {
        // SAFETY: caller upholds alignment and lifetime invariants.
        let ret = unsafe { sys::hv_vm_map(host_addr as *mut c_void, ipa, size, flags) };
        check_hv(ret, "hv_vm_map")
    }

    /// Unmaps a guest IPA range.
    pub fn unmap(&self, ipa: u64, size: usize) -> Result<()> {
        // SAFETY: valid IPA range previously mapped into this VM.
        let ret = unsafe { sys::hv_vm_unmap(ipa, size) };
        check_hv(ret, "hv_vm_unmap")
    }

    /// Changes stage-2 permissions on a mapped guest IPA range.
    pub fn protect(&self, ipa: u64, size: usize, flags: sys::hv_memory_flags_t) -> Result<()> {
        // SAFETY: `ipa` and `size` are page-aligned within a mapped region.
        let ret = unsafe { sys::hv_vm_protect(ipa, size, flags) };
        check_hv(ret, "hv_vm_protect")
    }
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        // SAFETY: all vCPUs are dropped before HvfVm (field order in Machine).
        let _ = unsafe { sys::hv_vm_destroy() };
        VM_LIVE.store(false, Ordering::SeqCst);
    }
}

/// Owns an HVF vCPU bound to the thread that created it.
pub struct HvfVcpu {
    id: sys::hv_vcpu_t,
    exit: *const sys::hv_vcpu_exit_t,
}

impl std::fmt::Debug for HvfVcpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HvfVcpu").field("id", &self.id).finish()
    }
}

// SAFETY: HVF vCPUs are accessed by one thread at a time (as Machine enforces).
unsafe impl Send for HvfVcpu {}

impl HvfVcpu {
    pub fn create() -> Result<Self> {
        let mut id: sys::hv_vcpu_t = 0;
        let mut exit: *const sys::hv_vcpu_exit_t = std::ptr::null();
        // SAFETY: valid out pointers; null uses default vCPU config.
        let ret = unsafe { sys::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) };
        check_hv(ret, "hv_vcpu_create")?;
        let vcpu = HvfVcpu { id, exit };
        // Mask the host virtual timer so it never injects asynchronous exits.
        vcpu.set_vtimer_mask(true)?;
        vcpu.set_vtimer_offset(0)?;
        Ok(vcpu)
    }

    pub fn id(&self) -> sys::hv_vcpu_t {
        self.id
    }

    pub fn run(&self) -> Result<sys::hv_vcpu_exit_t> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        let ret = unsafe { sys::hv_vcpu_run(self.id) };
        check_hv(ret, "hv_vcpu_run")?;
        // SAFETY: `self.exit` is populated by `hv_vcpu_create` and valid until `hv_vcpu_destroy`.
        Ok(unsafe { *self.exit })
    }

    pub fn get_reg(&self, reg: u32) -> Result<u64> {
        let mut val = 0u64;
        // SAFETY: valid vCPU and out pointer.
        let ret = unsafe { sys::hv_vcpu_get_reg(self.id, reg, &mut val) };
        check_hv(ret, "hv_vcpu_get_reg")?;
        Ok(val)
    }

    pub fn set_reg(&self, reg: u32, val: u64) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret = unsafe { sys::hv_vcpu_set_reg(self.id, reg, val) };
        check_hv(ret, "hv_vcpu_set_reg")
    }

    pub fn get_simd(&self, reg: u32) -> Result<[u8; 16]> {
        let mut val = sys::hv_simd_fp_uchar16_t::default();
        // SAFETY: valid vCPU and aligned out pointer.
        let ret = unsafe { sys::hv_vcpu_get_simd_fp_reg(self.id, reg, &mut val) };
        check_hv(ret, "hv_vcpu_get_simd_fp_reg")?;
        Ok(val.0)
    }

    pub fn set_simd(&self, reg: u32, bytes: [u8; 16]) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret =
            unsafe { sys::hv_vcpu_set_simd_fp_reg(self.id, reg, sys::hv_simd_fp_uchar16_t(bytes)) };
        check_hv(ret, "hv_vcpu_set_simd_fp_reg")
    }

    pub fn get_sys_reg(&self, reg: u16) -> Result<u64> {
        let mut val = 0u64;
        // SAFETY: valid vCPU and out pointer.
        let ret = unsafe { sys::hv_vcpu_get_sys_reg(self.id, reg, &mut val) };
        check_hv(ret, "hv_vcpu_get_sys_reg")?;
        Ok(val)
    }

    pub fn set_sys_reg(&self, reg: u16, val: u64) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret = unsafe { sys::hv_vcpu_set_sys_reg(self.id, reg, val) };
        check_hv(ret, "hv_vcpu_set_sys_reg")
    }

    pub fn set_pending_irq(&self, pending: bool) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret = unsafe {
            sys::hv_vcpu_set_pending_interrupt(self.id, sys::HV_INTERRUPT_TYPE_IRQ, pending)
        };
        check_hv(ret, "hv_vcpu_set_pending_interrupt")
    }

    pub fn set_vtimer_mask(&self, masked: bool) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret = unsafe { sys::hv_vcpu_set_vtimer_mask(self.id, masked) };
        check_hv(ret, "hv_vcpu_set_vtimer_mask")
    }

    pub fn set_vtimer_offset(&self, offset: u64) -> Result<()> {
        // SAFETY: valid vCPU.
        let ret = unsafe { sys::hv_vcpu_set_vtimer_offset(self.id, offset) };
        check_hv(ret, "hv_vcpu_set_vtimer_offset")
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: `self.id` is valid and owned by this struct.
        let _ = unsafe { sys::hv_vcpu_destroy(self.id) };
    }
}

/// Checks whether Hypervisor.framework is usable and the process holds the
/// `com.apple.security.hypervisor` entitlement by querying HVF or briefly
/// creating and destroying a VM if none is live.
pub fn check_available() -> Result<()> {
    if VM_LIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    let _vm = HvfVm::create()?;
    Ok(())
}
