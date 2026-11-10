//! Loading the kernel's vmlinux with the Linux x86 64-bit boot protocol.
//!
//! See Documentation/arch/x86/boot.rst in the kernel tree. The monitor does
//! what a bootloader does, less the decompression: copy vmlinux's loadable
//! segments to the physical addresses its ELF headers name, so the kernel
//! runs at its link address and relocates nothing, fill in a zero page,
//! place the initramfs high in RAM and the command line low, describe
//! memory with an e820 table, and jump to the kernel's entry point with
//! the zero page in RSI. A bzImage holds the same bytes compressed, and
//! spends most of a boot's host time unpacking them to the same place.

use anyhow::{Context, Result, bail};

use crate::layout::*;
use crate::memory::Mapping;

/// Offsets into `struct boot_params` and its embedded `setup_header`.
mod bp {
    pub(super) const E820_ENTRIES: usize = 0x1e8;
    pub(super) const BOOT_FLAG: usize = 0x1fe;
    pub(super) const HEADER_MAGIC: usize = 0x202;
    pub(super) const VERSION: usize = 0x206;
    pub(super) const TYPE_OF_LOADER: usize = 0x210;
    pub(super) const RAMDISK_IMAGE: usize = 0x218;
    pub(super) const RAMDISK_SIZE: usize = 0x21c;
    pub(super) const CMD_LINE_PTR: usize = 0x228;
    pub(super) const XLOADFLAGS: usize = 0x236;
    pub(super) const CMDLINE_SIZE: usize = 0x238;
    pub(super) const SETUP_DATA: usize = 0x250;
    pub(super) const E820_TABLE: usize = 0x2d0;
    pub(super) const SIZE: usize = 4096;
}

/// Offsets into a 64-bit ELF header and its program headers, and the
/// values a vmlinux has there.
mod elf {
    pub(super) const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    pub(super) const CLASS: usize = 4;
    pub(super) const CLASS_64: u8 = 2;
    pub(super) const DATA: usize = 5;
    pub(super) const DATA_LITTLE_ENDIAN: u8 = 1;
    pub(super) const TYPE: usize = 16;
    pub(super) const TYPE_EXEC: u16 = 2;
    pub(super) const MACHINE: usize = 18;
    pub(super) const MACHINE_X86_64: u16 = 62;
    pub(super) const ENTRY: usize = 24;
    pub(super) const PHOFF: usize = 32;
    pub(super) const PHENTSIZE: usize = 54;
    pub(super) const PHNUM: usize = 56;
    pub(super) const HEADER_SIZE: usize = 64;

    pub(super) const P_TYPE: usize = 0;
    pub(super) const PT_LOAD: u32 = 1;
    pub(super) const P_OFFSET: usize = 8;
    pub(super) const P_PADDR: usize = 24;
    pub(super) const P_FILESZ: usize = 32;
    pub(super) const P_MEMSZ: usize = 40;
    pub(super) const PHDR_SIZE: usize = 56;
}

/// 0xaa55 at the end of the boot sector, and "HdrS": what marks a zero
/// page as made by a bootloader that speaks the protocol.
const BOOT_FLAG: u16 = 0xaa55;
const HEADER_MAGIC: u32 = 0x5372_6448;

/// The boot protocol the zero page claims: 2.12, the first with a 64-bit
/// entry point. The kernel proper reads the version only to show it.
const PROTOCOL: u16 = 0x020c;

/// xloadflags: the kernel was entered in 64-bit mode.
const XLF_KERNEL_64: u16 = 1 << 0;

/// type_of_loader: "undefined", which is what a bootloader with no
/// assigned ID reports.
const LOADER_UNDEFINED: u8 = 0xff;

/// The highest address the kernel takes an initramfs at, from its own
/// setup header (arch/x86/boot/header.S), which vmlinux lacks.
const INITRD_ADDR_MAX: u64 = 0x7fff_ffff;

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

/// A loadable segment of the kernel's ELF: `filesz` bytes of the file at
/// `offset`, which go to physical address `paddr`, followed by zeros up
/// to `memsz`.
#[derive(Debug, PartialEq, Eq)]
struct Segment {
    offset: usize,
    paddr: u64,
    filesz: usize,
    memsz: u64,
}

/// What the loader reads from the kernel's ELF headers.
#[derive(Debug, PartialEq, Eq)]
struct Image {
    entry: u64,
    segments: Vec<Segment>,
}

impl Image {
    /// The entry point and loadable segments of `kernel`, a vmlinux: a
    /// 64-bit little-endian x86-64 executable whose segments lie within
    /// the file.
    fn parse(kernel: &[u8]) -> Result<Image> {
        if kernel.len() < elf::HEADER_SIZE || kernel[..elf::MAGIC.len()] != elf::MAGIC {
            bail!("kernel image is not an ELF file; is it a vmlinux?");
        }
        if kernel[elf::CLASS] != elf::CLASS_64 || kernel[elf::DATA] != elf::DATA_LITTLE_ENDIAN {
            bail!("kernel image is not a 64-bit little-endian ELF");
        }
        if read_u16(kernel, elf::TYPE) != elf::TYPE_EXEC {
            bail!("kernel image is not an executable");
        }
        if read_u16(kernel, elf::MACHINE) != elf::MACHINE_X86_64 {
            bail!("kernel image is not for x86-64");
        }
        if read_u16(kernel, elf::PHENTSIZE) as usize != elf::PHDR_SIZE {
            bail!("kernel image has program headers of an unknown size");
        }

        // The program headers, each either a segment to load or something
        // the kernel describes about itself, which no loader needs.
        let phoff = read_u64(kernel, elf::PHOFF) as usize;
        let phnum = read_u16(kernel, elf::PHNUM) as usize;
        let headers = kernel
            .get(phoff..)
            .and_then(|rest| rest.get(..phnum * elf::PHDR_SIZE))
            .context("kernel image's program headers lie past its end")?;
        let mut segments = Vec::new();
        for header in headers.as_chunks::<{ elf::PHDR_SIZE }>().0 {
            if read_u32(header, elf::P_TYPE) != elf::PT_LOAD {
                continue;
            }
            let segment = Segment {
                offset: read_u64(header, elf::P_OFFSET) as usize,
                paddr: read_u64(header, elf::P_PADDR),
                filesz: read_u64(header, elf::P_FILESZ) as usize,
                memsz: read_u64(header, elf::P_MEMSZ),
            };
            if segment
                .offset
                .checked_add(segment.filesz)
                .is_none_or(|end| end > kernel.len())
            {
                bail!(
                    "kernel segment at {:#x} lies past the file's end",
                    segment.paddr
                );
            }
            if segment.memsz < segment.filesz as u64 {
                bail!(
                    "kernel segment at {:#x} is longer in the file than in memory",
                    segment.paddr
                );
            }
            segments.push(segment);
        }
        if segments.is_empty() {
            bail!("kernel image has no loadable segment");
        }
        Ok(Image {
            entry: read_u64(kernel, elf::ENTRY),
            segments,
        })
    }
}

/// Loads the kernel and its inputs into RAM and returns the entry point.
pub fn load(ram: &mut Mapping, spec: &BootSpec) -> Result<u64> {
    let image = Image::parse(spec.kernel)?;

    // Each segment goes where the kernel was linked to run, which must be
    // RAM; the kernel's zero-filled tail is written as zeros, since the
    // RAM a rewind restores from may hold an earlier boot there.
    let mut kernel_end = 0;
    for segment in &image.segments {
        let end = segment
            .paddr
            .checked_add(segment.memsz)
            .filter(|end| segment.paddr >= RAM_START && *end <= ram.len() as u64)
            .with_context(|| {
                format!(
                    "kernel segment at {:#x} of {} bytes does not fit in RAM of {} bytes",
                    segment.paddr,
                    segment.memsz,
                    ram.len()
                )
            })?;
        ram.write(
            segment.paddr,
            &spec.kernel[segment.offset..segment.offset + segment.filesz],
        )?;
        let zeros = segment.memsz - segment.filesz as u64;
        if zeros > 0 {
            ram.write(
                segment.paddr + segment.filesz as u64,
                &vec![0u8; zeros as usize],
            )?;
        }
        kernel_end = kernel_end.max(end);
    }

    // The zero page: the setup header fields a bootloader fills in, with
    // no bzImage header to copy the rest from, which only the real-mode
    // setup code and the decompressor read.
    let mut params = vec![0u8; bp::SIZE];
    write_u16(&mut params, bp::BOOT_FLAG, BOOT_FLAG);
    write_u32(&mut params, bp::HEADER_MAGIC, HEADER_MAGIC);
    write_u16(&mut params, bp::VERSION, PROTOCOL);
    write_u16(&mut params, bp::XLOADFLAGS, XLF_KERNEL_64);
    params[bp::TYPE_OF_LOADER] = LOADER_UNDEFINED;

    // The initramfs goes as high as the kernel allows, above the kernel.
    let initrd_max = INITRD_ADDR_MAX.min(ram.len() as u64 - 1);
    let initrd_len = spec.initrd.len() as u64;
    if initrd_len > 0 {
        // An initramfs longer than everything below initrd_max has no
        // address at all, and one that ends up below the kernel's end
        // would overwrite the kernel.
        let initrd_addr = (initrd_max + 1)
            .checked_sub(initrd_len)
            .map(|top| top & !(PAGE_SIZE as u64 - 1))
            .filter(|addr| *addr >= kernel_end)
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

    // The memory map: conventional memory, RAM from 1 MiB up, and the
    // input image as persistent memory at 4 GiB.
    let mut e820 = vec![
        (0, EBDA_START, E820::Ram),
        (EBDA_START, RAM_START - EBDA_START, E820::Reserved),
        (RAM_START, ram.len() as u64 - RAM_START, E820::Ram),
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
    Ok(image.entry)
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

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("eight bytes"))
}

fn write_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    // Loading a kernel: a made-up vmlinux of one loadable segment, into a
    // small RAM.
    use super::*;

    fn write_u64(buf: &mut [u8], at: usize, value: u64) {
        buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// The guest's RAM in these tests.
    const RAM: usize = 16 << 20;

    /// Where the made-up kernel is linked to run, and its entry point
    /// inside the segment.
    const LINK: u64 = 2 << 20;
    const ENTRY: u64 = LINK + 0x10;

    /// A vmlinux the loader takes: the ELF header, one program header
    /// for a segment of `text` at LINK, followed by `tail` zero bytes
    /// in memory, and the segment's bytes.
    fn kernel_with(text: &[u8], tail: u64) -> Vec<u8> {
        let offset = elf::HEADER_SIZE + elf::PHDR_SIZE;
        let mut kernel = vec![0u8; offset + text.len()];
        kernel[..4].copy_from_slice(&elf::MAGIC);
        kernel[elf::CLASS] = elf::CLASS_64;
        kernel[elf::DATA] = elf::DATA_LITTLE_ENDIAN;
        write_u16(&mut kernel, elf::TYPE, elf::TYPE_EXEC);
        write_u16(&mut kernel, elf::MACHINE, elf::MACHINE_X86_64);
        write_u64(&mut kernel, elf::ENTRY, ENTRY);
        write_u64(&mut kernel, elf::PHOFF, elf::HEADER_SIZE as u64);
        write_u16(&mut kernel, elf::PHENTSIZE, elf::PHDR_SIZE as u16);
        write_u16(&mut kernel, elf::PHNUM, 1);

        let header = elf::HEADER_SIZE;
        write_u32(&mut kernel, header + elf::P_TYPE, elf::PT_LOAD);
        write_u64(&mut kernel, header + elf::P_OFFSET, offset as u64);
        write_u64(&mut kernel, header + elf::P_PADDR, LINK);
        write_u64(&mut kernel, header + elf::P_FILESZ, text.len() as u64);
        write_u64(&mut kernel, header + elf::P_MEMSZ, text.len() as u64 + tail);
        kernel[offset..].copy_from_slice(text);
        kernel
    }

    fn kernel() -> Vec<u8> {
        kernel_with(&[0x90; 0x100], 0)
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

    /// The segment's bytes land at its physical address, its zero tail
    /// is written over whatever RAM held, and the entry point is the
    /// ELF's.
    #[test]
    fn the_kernel_lands_at_its_link_address() {
        let mut ram = Mapping::anonymous(RAM).unwrap();
        let text = [0x42u8; 0x100];
        let kernel = kernel_with(&text, 0x20);
        ram.write(LINK + 0x100, &[0xffu8; 0x20]).unwrap();
        let entry = load(&mut ram, &spec(&kernel, &[])).unwrap();
        assert_eq!(entry, ENTRY);

        let mut loaded = [0u8; 0x120];
        ram.read(LINK, &mut loaded).unwrap();
        assert_eq!(loaded[..0x100], text);
        assert_eq!(loaded[0x100..], [0u8; 0x20]);
    }

    /// A segment that reaches past RAM is refused, naming its address.
    #[test]
    fn a_kernel_linked_past_ram_does_not_fit() {
        let mut ram = Mapping::anonymous(RAM).unwrap();
        let kernel = kernel_with(&[0x90; 0x100], RAM as u64);
        let err = load(&mut ram, &spec(&kernel, &[])).unwrap_err();
        assert!(
            format!("{err:#}").contains("segment at 0x200000"),
            "{err:#}"
        );
    }

    /// A file that is no ELF, as a bzImage is, is refused as such.
    #[test]
    fn a_bzimage_is_not_a_kernel_image() {
        let mut ram = Mapping::anonymous(RAM).unwrap();
        let mut kernel = kernel();
        kernel[0] = 0xeb;
        let err = load(&mut ram, &spec(&kernel, &[])).unwrap_err();
        assert!(format!("{err:#}").contains("not an ELF"), "{err:#}");
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
        assert_eq!(read_u32(&params, bp::HEADER_MAGIC), HEADER_MAGIC);
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
