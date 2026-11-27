//! Hypervisor availability probe (HVF counterpart of Linux's `kvm::open`).

use anyhow::Result;

/// Handle proving macOS Hypervisor.framework is available and entitled.
#[derive(Debug)]
pub struct Kvm;

/// Verifies Hypervisor.framework can create a VM in this process.
pub fn open() -> Result<Kvm> {
    crate::hvf::check_available()?;
    Ok(Kvm)
}
