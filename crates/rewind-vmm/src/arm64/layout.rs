//! ARM64 guest physical memory map shared by Hypervisor.framework and ARM64 KVM.
//!
//! Guest RAM starts at 1 GiB (`0x4000_0000`, matching QEMU `virt` and Linux
//! arm64 conventions). The Rewind paravirtual MMIO register page lives at
//! `0x0900_0000` (4 KiB). Persistent-memory devices (input image and optional
//! extras slot) are placed above RAM and advertised in the FDT as
//! `compatible = "pmem-region"` nodes.

/// Guest architectural page size (Linux arm64 4 KiB granule).
pub const PAGE_SIZE: usize = 4096;

/// Alignment required for host memory mappings (16 KiB covers both 4 KiB and
/// 16 KiB ARM64 hosts, including Apple Silicon macOS and Asahi Linux).
pub const HOST_PAGE_SIZE: usize = 16384;

/// Guest physical address of RAM byte 0 (1 GiB).
pub const RAM_BASE: u64 = 0x4000_0000;

/// Guest physical address of the Rewind paravirtual MMIO register page.
pub const REWIND_MMIO_BASE: u64 = 0x0900_0000;

/// Length of the Rewind paravirtual MMIO register page.
pub const REWIND_MMIO_SIZE: u64 = PAGE_SIZE as u64;

/// 8-byte aligned paravirtual MMIO register offsets inside `REWIND_MMIO_BASE`.
pub const MMIO_EMIT: u16 = 0x00;
pub const MMIO_CLOCK: u16 = 0x08;
pub const MMIO_TIMER: u16 = 0x10;
pub const MMIO_IDLE: u16 = 0x18;
pub const MMIO_EXIT: u16 = 0x20;
pub const MMIO_SETUP: u16 = 0x28;
pub const MMIO_IRQ_ACK: u16 = 0x30;
pub const MMIO_SERIAL: u16 = 0x38;
pub const MMIO_NET_TX: u16 = 0x40;

/// Maps an ARM64 Rewind MMIO register offset to the shared `pv::PORT_*` /
/// `pv::HVC_IRQ_ACK` constant.
pub const fn mmio_offset_to_port(offset: u16) -> Option<u16> {
    use crate::pv;
    match offset {
        MMIO_EMIT => Some(pv::PORT_EMIT),
        MMIO_CLOCK => Some(pv::PORT_CLOCK),
        MMIO_TIMER => Some(pv::PORT_TIMER),
        MMIO_IDLE => Some(pv::PORT_IDLE),
        MMIO_EXIT => Some(pv::PORT_EXIT),
        MMIO_SETUP => Some(pv::PORT_SETUP),
        MMIO_IRQ_ACK => Some(pv::HVC_IRQ_ACK),
        MMIO_SERIAL => Some(pv::PORT_COM1),
        MMIO_NET_TX => Some(pv::PORT_NET_TX),
        _ => None,
    }
}

/// Maximum guest RAM supported on ARM64 (16 GiB).
pub const MEM_MAX: u64 = 16 << 30;

/// Guest physical address reserved for the extras image slot (32 GiB).
/// Fits comfortably within a 36-bit IPA space (`0 .. 64 GiB`).
pub const EXTRAS_START: u64 = 0x8_0000_0000;

/// Size of the reserved extras slot (16 GiB).
pub const EXTRAS_LEN: u64 = 16 << 30;

/// Minimum IPA width needed to reach the end of the extras slot (`32 + 16 = 48 GiB < 64 GiB`).
pub const EXTRAS_ADDRESS_BITS: u32 = 36;

/// Guest physical address where the input image (`pmem0`) starts, 2 MiB aligned
/// above RAM.
pub const fn pmem_start(ram_len: u64) -> u64 {
    let align = 2 << 20;
    let end = RAM_BASE + ram_len;
    (end + align - 1) & !(align - 1)
}

/// Length of the RAM backing mapping for `mem_bytes` of guest RAM.
pub const fn ram_span(mem_bytes: u64) -> u64 {
    mem_bytes
}
