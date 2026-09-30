//! Where things live in guest physical memory.
//!
//! The low structures follow the layout Firecracker and most minimal
//! monitors use for the 64-bit Linux boot protocol, so the addresses are
//! ones the kernel has been booted from many times before.

/// The global descriptor table the vCPU starts with.
pub const GDT: u64 = 0x500;

/// An empty interrupt descriptor table; the kernel installs its own before
/// it enables interrupts.
pub const IDT: u64 = 0x520;

/// The boot stack pointer handed to the kernel's 64-bit entry.
pub const BOOT_STACK: u64 = 0x8ff0;

/// The zero page: `struct boot_params`.
pub const BOOT_PARAMS: u64 = 0x7000;

/// Identity page tables for the first gigabyte: one PML4, one PDPT, one page
/// directory of 2 MiB pages.
pub const PML4: u64 = 0x9000;
pub const PDPT: u64 = 0xa000;
pub const PD: u64 = 0xb000;

/// The kernel command line.
pub const CMDLINE: u64 = 0x20000;
pub const CMDLINE_MAX: usize = 4096;

/// The `setup_data` chain: the RNG seed the kernel credits at boot.
pub const SETUP_DATA: u64 = 0x21000;

/// End of conventional memory. The kernel treats the range above it, up to
/// 1 MiB, as firmware.
pub const EBDA_START: u64 = 0x9fc00;

/// Where the protected-mode kernel is loaded, and its 64-bit entry point.
pub const KERNEL_START: u64 = 0x10_0000;
pub const KERNEL_ENTRY_64: u64 = KERNEL_START + 0x200;

/// RAM stops below the 32-bit MMIO hole, where the local APIC and I/O APIC
/// live. Guests are small, so no RAM is placed above 4 GiB.
pub const RAM_MAX: u64 = 0xc000_0000;

/// The input image, mapped as legacy persistent memory at 4 GiB.
pub const PMEM_START: u64 = 0x1_0000_0000;

/// The local APIC's architectural base, and the vector the monitor's
/// interrupt arrives on (Linux's HYPERVISOR_CALLBACK_VECTOR).
pub const APIC_BASE: u64 = 0xfee0_0000;
pub const CALLBACK_VECTOR: u32 = 0xf3;

/// Page size, for dirty logging and the page store.
pub const PAGE_SIZE: usize = 4096;
