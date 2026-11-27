//! Shared ARM64 guest layout, device tree, Linux `Image` loader, and 4 KiB
//! page-table walker used by both macOS Hypervisor.framework and Linux ARM64 KVM.

pub mod boot;
pub mod fdt;
pub mod layout;
pub mod pagetable;
