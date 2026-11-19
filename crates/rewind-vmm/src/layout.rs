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

/// Where RAM resumes above the firmware hole. The kernel's segments go
/// above it, at the physical addresses its vmlinux names (16 MiB).
pub const RAM_START: u64 = 0x10_0000;

/// Low RAM stops below the 32-bit MMIO hole, where the local APIC and I/O
/// APIC live.
pub const RAM_MAX: u64 = 0xc000_0000;

/// Where RAM resumes above the 32-bit MMIO hole, for a guest with more
/// memory than fits below it.
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

/// The most memory a guest can have, so that the RAM and then the input
/// image stay well below the extras slot.
pub const MEM_MAX: u64 = 32 << 30;

/// The span of guest physical addresses that `mem` bytes of RAM take, from
/// 0: all of it below the MMIO hole when it fits, else the low part, the
/// hole, and the rest from 4 GiB. The monitor maps this span as one range
/// indexed by guest physical address; the hole in it is never touched, so
/// it costs no host memory.
pub const fn ram_span(mem: u64) -> u64 {
    if mem <= RAM_MAX {
        mem
    } else {
        HIGH_RAM_START + (mem - RAM_MAX)
    }
}

/// The guest RAM below and above the MMIO hole, as (start, length) pairs,
/// for RAM of span `span` ([`ram_span`]). The high part is empty unless
/// the RAM goes past [`RAM_MAX`].
pub const fn ram_parts(span: u64) -> [(u64, u64); 2] {
    if span <= HIGH_RAM_START {
        [(0, span), (HIGH_RAM_START, 0)]
    } else {
        [(0, RAM_MAX), (HIGH_RAM_START, span - HIGH_RAM_START)]
    }
}

/// Where the input image goes, mapped as legacy persistent memory: at 4 GiB
/// when the RAM stays below the hole, else at the first GiB above the RAM.
/// A run without high RAM thus keeps the layout it was recorded with.
pub const fn pmem_start(span: u64) -> u64 {
    if span <= HIGH_RAM_START {
        HIGH_RAM_START
    } else {
        span.next_multiple_of(1 << 30)
    }
}

/// The extras slot: persistent memory a run reserves at boot, empty, which
/// `rewind shell --with` fills in a fork with an image of more Nix
/// packages. Far above any input image, and inside the 39 physical address
/// bits the CPU model shows.
pub const EXTRAS_START: u64 = 0x40_0000_0000;
pub const EXTRAS_LEN: u64 = 64 << 30;

/// The physical address bits that reach the extras slot's last byte. KVM
/// refuses a memory slot that ends past what the host's CPU reaches.
pub const EXTRAS_ADDRESS_BITS: u32 = u64::BITS - (EXTRAS_START + EXTRAS_LEN - 1).leading_zeros();

/// The local APIC's architectural base, and the vector the monitor's
/// interrupt arrives on (Linux's HYPERVISOR_CALLBACK_VECTOR).
pub const APIC_BASE: u64 = 0xfee0_0000;
pub const CALLBACK_VECTOR: u32 = 0xf3;

/// Page size, for dirty logging and the page store.
pub const PAGE_SIZE: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    /// RAM that fits below the MMIO hole keeps the layout of before: one
    /// part, and the input image at 4 GiB.
    #[test]
    fn low_ram_stays_below_the_hole() {
        let span = ram_span(RAM_MAX);
        assert_eq!(span, RAM_MAX);
        assert_eq!(ram_parts(span), [(0, RAM_MAX), (HIGH_RAM_START, 0)]);
        assert_eq!(pmem_start(span), 0x1_0000_0000);
    }

    /// RAM past the hole resumes at 4 GiB, and the input image goes at
    /// the first GiB after it.
    #[test]
    fn high_ram_resumes_above_the_hole() {
        let mem = 8 << 30;
        let span = ram_span(mem);
        let [(low_start, low), (high_start, high)] = ram_parts(span);
        assert_eq!((low_start, low), (0, RAM_MAX));
        assert_eq!(high_start, HIGH_RAM_START);
        assert_eq!(low + high, mem);
        assert_eq!(span, HIGH_RAM_START + high);
        assert_eq!(pmem_start(span), span);
        assert_eq!(pmem_start(span + PAGE_SIZE as u64), span + (1 << 30));
    }

    /// The most memory, with an image of the same size, stays below the
    /// extras slot.
    #[test]
    fn the_most_memory_stays_below_the_extras_slot() {
        assert!(pmem_start(ram_span(MEM_MAX)) + MEM_MAX <= EXTRAS_START);
    }
}
