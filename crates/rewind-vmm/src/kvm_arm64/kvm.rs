//! Opening `/dev/kvm` on Linux ARM64 with actionable error messages.

use std::io::ErrorKind;

use anyhow::{Context, Result, bail};
use kvm_ioctls::Kvm;

/// Opens `/dev/kvm`, explaining missing device or permissions clearly.
pub fn open() -> Result<Kvm> {
    match Kvm::new() {
        Ok(kvm) => Ok(kvm),
        Err(e) => {
            let err = std::io::Error::from_raw_os_error(e.errno());
            match err.kind() {
                ErrorKind::NotFound => bail!(
                    "/dev/kvm does not exist; enable KVM in the host kernel or run on a host with \
                     ARM64 virtualization support"
                ),
                ErrorKind::PermissionDenied => bail!(
                    "cannot open /dev/kvm: permission denied; add your user to the `kvm` group"
                ),
                _ => Err(err).context("opening /dev/kvm"),
            }
        }
    }
}
