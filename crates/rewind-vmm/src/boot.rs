//! Loading a bzImage with the Linux x86 64-bit boot protocol.
//!
//! See Documentation/arch/x86/boot.rst in the kernel tree. The monitor does
//! what a bootloader does: copy the kernel's setup header into a zero page,
//! place the protected-mode kernel at 1 MiB, the initramfs high in RAM and
//! the command line low, describe memory with an e820 table, and jump to the
//! 64-bit entry point with the zero page in RSI.

use anyhow::{Context, Result, bail};

use crate::layout::*;
use crate::memory::Mapping;

/// Offsets into `struct boot_params` and its embedded `setup_header`.
mod bp {
    pub(super) const E820_ENTRIES: usize = 0x1e8;
    pub(super) const SETUP_SECTS: usize = 0x1f1;
    pub(super) const HEADER_MAGIC: usize = 0x202;
    pub(super) const HEADER_END_BYTE: usize = 0x201;
    pub(super) const VERSION: usize = 0x206;
    pub(super) const TYPE_OF_LOADER: usize = 0x210;
    pub(super) const LOADFLAGS: usize = 0x211;
    pub(super) const RAMDISK_IMAGE: usize = 0x218;
    pub(super) const RAMDISK_SIZE: usize = 0x21c;
    pub(super) const HEAP_END_PTR: usize = 0x224;
    pub(super) const CMD_LINE_PTR: usize = 0x228;
    pub(super) const INITRD_ADDR_MAX: usize = 0x22c;
    pub(super) const XLOADFLAGS: usize = 0x236;
    pub(super) const CMDLINE_SIZE: usize = 0x238;
    pub(super) const SETUP_DATA: usize = 0x250;
    pub(super) const INIT_SIZE: usize = 0x260;
    pub(super) const E820_TABLE: usize = 0x2d0;
    pub(super) const SIZE: usize = 4096;
}

/// "HdrS", the setup header's magic.
const HEADER_MAGIC: u32 = 0x5372_6448;

/// The oldest boot protocol with a 64-bit entry point (2.12).
const MIN_PROTOCOL: u16 = 0x020c;

/// loadflags: the kernel may use the heap up to heap_end_ptr.
const CAN_USE_HEAP: u8 = 0x80;

/// xloadflags: the kernel has a 64-bit entry at +0x200.
const XLF_KERNEL_64: u16 = 1 << 0;

/// type_of_loader: "undefined", which is what a bootloader with no
/// assigned ID reports.
const LOADER_UNDEFINED: u8 = 0xff;

/// setup_data type: a random seed the kernel credits to its RNG.
const SETUP_RNG_SEED: u32 = 9;

/// e820 entry types.
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum E820 {
    Ram = 1,
    Reserved = 2,
    /// Legacy persistent memory, which Linux exposes as /dev/pmemN.
    Pram = 12,
}

/// Everything the boot protocol needs from the monitor.
#[derive(Debug)]
pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: &'a str,
    pub seed: [u8; 32],
    pub pmem_len: u64,
    /// The extras slot's length, or 0 for a machine without one.
    pub extras_len: u64,
}

/// Loads the kernel and its inputs into RAM and returns the entry point.
pub fn load(ram: &mut Mapping, spec: &BootSpec) -> Result<u64> {
    let kernel = spec.kernel;
    if kernel.len() < 0x1000 {
        bail!("kernel image is too small to be a bzImage");
    }
    if read_u32(kernel, bp::HEADER_MAGIC) != HEADER_MAGIC {
        bail!("kernel image has no setup header; is it a bzImage?");
    }
    let version = read_u16(kernel, bp::VERSION);
    if version < MIN_PROTOCOL {
        bail!("boot protocol {version:#x} is older than {MIN_PROTOCOL:#x}");
    }
    if read_u16(kernel, bp::XLOADFLAGS) & XLF_KERNEL_64 == 0 {
        bail!("kernel has no 64-bit entry point");
    }

    // The zero page starts as a copy of the image's setup header.
    let mut params = vec![0u8; bp::SIZE];
    let header_end = 0x202 + kernel[bp::HEADER_END_BYTE] as usize;
    params[bp::SETUP_SECTS..header_end].copy_from_slice(&kernel[bp::SETUP_SECTS..header_end]);

    // The protected-mode kernel follows the real-mode setup sectors.
    let setup_sects = match kernel[bp::SETUP_SECTS] {
        0 => 4,
        n => n as usize,
    };
    let payload = &kernel[(setup_sects + 1) * 512..];
    ram.write(KERNEL_START, payload)?;

    // The kernel decompresses in place and needs init_size bytes from its
    // load address; the initramfs goes as high as the kernel allows, above
    // that.
    let init_size = read_u32(kernel, bp::INIT_SIZE) as u64;
    let initrd_max = (read_u32(kernel, bp::INITRD_ADDR_MAX) as u64).min(ram.len() as u64 - 1);
    let initrd_len = spec.initrd.len() as u64;
    if initrd_len > 0 {
        // An initramfs longer than everything below initrd_max has no
        // address at all, and one that ends up below the kernel's
        // init_size would be overwritten as the kernel decompresses.
        let initrd_addr = (initrd_max + 1)
            .checked_sub(initrd_len)
            .map(|top| top & !(PAGE_SIZE as u64 - 1))
            .filter(|addr| *addr >= KERNEL_START + init_size)
            .with_context(|| {
                format!("initrd of {initrd_len} bytes does not fit in RAM above the kernel")
            })?;
        ram.write(initrd_addr, spec.initrd)?;
        write_u32(&mut params, bp::RAMDISK_IMAGE, initrd_addr as u32);
        write_u32(&mut params, bp::RAMDISK_SIZE, initrd_len as u32);
    }

    // The command line, NUL terminated.
    if spec.cmdline.len() >= CMDLINE_MAX {
        bail!("kernel command line is longer than {CMDLINE_MAX} bytes");
    }
    let mut cmdline = spec.cmdline.as_bytes().to_vec();
    cmdline.push(0);
    ram.write(CMDLINE, &cmdline)?;
    write_u32(&mut params, bp::CMD_LINE_PTR, CMDLINE as u32);
    write_u32(&mut params, bp::CMDLINE_SIZE, cmdline.len() as u32);

    // The RNG seed, as the only entry of the setup_data chain. The kernel
    // credits it and wipes it, so the guest's randomness is a function of
    // the seed and nothing else.
    let mut setup = Vec::with_capacity(16 + spec.seed.len());
    setup.extend_from_slice(&0u64.to_le_bytes());
    setup.extend_from_slice(&SETUP_RNG_SEED.to_le_bytes());
    setup.extend_from_slice(&(spec.seed.len() as u32).to_le_bytes());
    setup.extend_from_slice(&spec.seed);
    ram.write(SETUP_DATA, &setup)?;
    params[bp::SETUP_DATA..bp::SETUP_DATA + 8].copy_from_slice(&SETUP_DATA.to_le_bytes());

    params[bp::TYPE_OF_LOADER] = LOADER_UNDEFINED;
    params[bp::LOADFLAGS] |= CAN_USE_HEAP;
    write_u16(&mut params, bp::HEAP_END_PTR, 0xfe00);

    // The memory map: conventional memory, RAM from 1 MiB up, and the
    // input image as persistent memory at 4 GiB.
    let mut e820 = vec![
        (0, EBDA_START, E820::Ram),
        (EBDA_START, KERNEL_START - EBDA_START, E820::Reserved),
        (KERNEL_START, ram.len() as u64 - KERNEL_START, E820::Ram),
    ];
    if spec.pmem_len > 0 {
        e820.push((PMEM_START, spec.pmem_len, E820::Pram));
    }
    if spec.extras_len > 0 {
        e820.push((EXTRAS_START, spec.extras_len, E820::Pram));
    }
    for (i, (addr, size, kind)) in e820.iter().enumerate() {
        let at = bp::E820_TABLE + i * 20;
        params[at..at + 8].copy_from_slice(&addr.to_le_bytes());
        params[at + 8..at + 16].copy_from_slice(&size.to_le_bytes());
        params[at + 16..at + 20].copy_from_slice(&(*kind as u32).to_le_bytes());
    }
    params[bp::E820_ENTRIES] = e820.len() as u8;

    ram.write(BOOT_PARAMS, &params)
        .context("writing the zero page")?;
    Ok(KERNEL_ENTRY_64)
}

/// Identity page tables for the first gigabyte, in 2 MiB pages, and the
/// GDT the vCPU's segment registers point into. The kernel replaces both
/// within its first few thousand instructions.
pub fn write_tables(ram: &mut Mapping) -> Result<()> {
    const PRESENT_WRITABLE: u64 = 0x3;
    const HUGE: u64 = 0x80;

    ram.write(PML4, &(PDPT | PRESENT_WRITABLE).to_le_bytes())?;
    ram.write(PDPT, &(PD | PRESENT_WRITABLE).to_le_bytes())?;
    for i in 0..512u64 {
        let entry = (i << 21) | PRESENT_WRITABLE | HUGE;
        ram.write(PD + i * 8, &entry.to_le_bytes())?;
    }

    for (i, entry) in crate::cpu::GDT_ENTRIES.iter().enumerate() {
        ram.write(GDT + i as u64 * 8, &entry.to_le_bytes())?;
    }
    ram.write(IDT, &0u64.to_le_bytes())?;
    Ok(())
}

fn read_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(buf[at..at + 2].try_into().expect("two bytes"))
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("four bytes"))
}

fn write_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    // Loading a kernel: a made-up bzImage with only the setup header's
    // fields the loader reads, into a small RAM.
    use super::*;

    /// The guest's RAM in these tests.
    const RAM: usize = 16 << 20;

    /// A bzImage the loader takes: the header magic, a recent protocol, a
    /// 64-bit entry point, and an initramfs allowed anywhere in 4 GiB.
    fn kernel() -> Vec<u8> {
        let mut kernel = vec![0u8; 0x2000];
        kernel[bp::SETUP_SECTS] = 4;
        kernel[bp::HEADER_END_BYTE] = 0x80;
        write_u32(&mut kernel, bp::HEADER_MAGIC, HEADER_MAGIC);
        write_u16(&mut kernel, bp::VERSION, MIN_PROTOCOL);
        write_u16(&mut kernel, bp::XLOADFLAGS, XLF_KERNEL_64);
        write_u32(&mut kernel, bp::INITRD_ADDR_MAX, u32::MAX);
        write_u32(&mut kernel, bp::INIT_SIZE, 0x1000);
        kernel
    }

    fn spec<'a>(kernel: &'a [u8], initrd: &'a [u8]) -> BootSpec<'a> {
        BootSpec {
            kernel,
            initrd,
            cmdline: "console=ttyS0",
            seed: [0; 32],
            pmem_len: 0,
            extras_len: 0,
        }
    }

    /// An initramfs that fits goes at the top of RAM, page aligned, and
    /// the zero page names where and how long.
    #[test]
    fn an_initramfs_goes_at_the_top_of_ram() {
        let mut ram = Mapping::anonymous(RAM).unwrap();
        let kernel = kernel();
        let initrd = vec![7u8; 5000];
        load(&mut ram, &spec(&kernel, &initrd)).unwrap();

        let mut params = vec![0u8; bp::SIZE];
        ram.read(BOOT_PARAMS, &mut params).unwrap();
        let addr = read_u32(&params, bp::RAMDISK_IMAGE) as u64;
        assert_eq!(addr, (RAM as u64 - 5000) & !(PAGE_SIZE as u64 - 1));
        assert_eq!(read_u32(&params, bp::RAMDISK_SIZE), 5000);
    }

    /// An initramfs larger than RAM is refused as not fitting, rather
    /// than placed at an address computed by wrapping around.
    #[test]
    fn an_initramfs_larger_than_ram_does_not_fit() {
        let mut ram = Mapping::anonymous(RAM).unwrap();
        let kernel = kernel();
        let initrd = vec![7u8; RAM + 1];
        let err = load(&mut ram, &spec(&kernel, &initrd)).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not fit in RAM"),
            "{err:#}"
        );
    }
}
