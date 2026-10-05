//! Opening /dev/kvm, with what to do about it when that fails.

use anyhow::Result;
use kvm_ioctls::Kvm;

/// The device every VM is made through.
const DEVICE: &str = "/dev/kvm";

/// /dev/kvm, opened, or why not and what would let it open.
pub fn open() -> Result<Kvm> {
    Kvm::new().map_err(|e| match hint(e.errno()) {
        Some(hint) => anyhow::anyhow!("opening {DEVICE}: {e}; {hint}"),
        None => anyhow::anyhow!("opening {DEVICE}: {e}"),
    })
}

/// What to do when opening /dev/kvm failed with `errno`, when anything
/// is known to help.
fn hint(errno: i32) -> Option<&'static str> {
    match errno {
        libc::ENOENT => Some(
            "this machine has no KVM, or its module is not loaded: try `sudo modprobe \
             kvm_intel` or `sudo modprobe kvm_amd`, turn virtualization on in the firmware, \
             or inside a VM turn on nested virtualization",
        ),
        libc::EACCES | libc::EPERM => Some(
            "add your user to the group /dev/kvm belongs to, which `ls -l /dev/kvm` names and \
             is most often kvm, then log in again",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // The hint for each way opening /dev/kvm commonly fails.
    use super::*;

    #[test]
    fn a_missing_or_forbidden_device_says_what_would_help() {
        // No device: KVM is off or its module is not loaded. Not allowed:
        // the group. Anything else has no hint.
        assert!(hint(libc::ENOENT).unwrap().contains("kvm_intel"));
        assert!(hint(libc::EACCES).unwrap().contains("group"));
        assert!(hint(libc::EPERM).unwrap().contains("group"));
        assert_eq!(hint(libc::EBUSY), None);
    }
}
