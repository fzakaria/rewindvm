//! Linux ARM64 boot protocol loader (`Documentation/arch/arm64/booting.rst`).
//!
//! Loads an uncompressed ARM64 `Image`, places the initramfs and a generated
//! Flattened Device Tree (`.dtb`) in RAM, and returns the kernel entry IPA and
//! DTB IPA (`X0`).

use anyhow::{Result, bail};

use crate::arm64::fdt::{self, FdtConfig};
use crate::arm64::layout::{EXTRAS_START, HOST_PAGE_SIZE, RAM_BASE, pmem_start};
use crate::memory::Mapping;

/// Magic at offset 0x38 of an ARM64 Linux `Image` ("ARM\x64" little-endian).
const ARM64_IMAGE_MAGIC: u32 = 0x644d_5241;
const IMAGE_HEADER_LEN: usize = 64;

/// Kernel placement alignment required by `booting.rst`: 2 MiB.
const KERNEL_ALIGN: u64 = 2 << 20;

/// FDT alignment required by `booting.rst`: 8 bytes (we use 64 KiB, well within
/// the 2 MiB maximum DTB size and aligned to host pages).
const FDT_ALIGN: u64 = 64 << 10;

/// Everything the boot protocol needs from the monitor.
#[derive(Debug)]
pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: &'a str,
    pub seed: [u8; 32],
    pub pmem_len: u64,
    pub extras_len: u64,
}

/// Entry addresses produced by [`load`].
#[derive(Clone, Copy, Debug)]
pub struct BootAddresses {
    pub kernel_entry: u64,
    pub dtb_gpa: u64,
}

/// Loads the ARM64 kernel `Image`, initrd, and FDT into `ram` (which backs
/// `RAM_BASE .. RAM_BASE + ram.len()`).
pub fn load(ram: &mut Mapping, spec: &BootSpec<'_>) -> Result<BootAddresses> {
    if spec.kernel.len() < IMAGE_HEADER_LEN {
        bail!("ARM64 kernel image is shorter than its 64-byte header");
    }
    let magic = u32::from_le_bytes(
        spec.kernel[0x38..0x3c]
            .try_into()
            .expect("four bytes at 0x38"),
    );
    if magic != ARM64_IMAGE_MAGIC {
        bail!(
            "kernel is not an uncompressed ARM64 Linux Image (expected magic {ARM64_IMAGE_MAGIC:#x}, \
             got {magic:#x}; pass arch/arm64/boot/Image)"
        );
    }

    let text_offset = u64::from_le_bytes(
        spec.kernel[0x08..0x10]
            .try_into()
            .expect("eight bytes at 0x08"),
    );
    let image_size = u64::from_le_bytes(
        spec.kernel[0x10..0x18]
            .try_into()
            .expect("eight bytes at 0x10"),
    );
    let effective_size = image_size.max(spec.kernel.len() as u64);

    // Per booting.rst, if image_size == 0, text_offset is assumed to be 0x80000.
    let offset = if image_size == 0 {
        0x80000
    } else {
        text_offset
    };

    // Place the kernel at a 2 MiB aligned base + text_offset.
    let kernel_offset = offset.min(KERNEL_ALIGN);
    let kernel_end = kernel_offset
        .checked_add(effective_size)
        .ok_or_else(|| anyhow::anyhow!("kernel size overflow"))?;
    let ram_len = ram.len() as u64;
    if kernel_end > ram_len {
        bail!("kernel ({effective_size} bytes) does not fit in {ram_len} bytes of RAM");
    }
    ram.write(kernel_offset, spec.kernel)?;
    let kernel_entry = RAM_BASE + kernel_offset;

    // Place initrd near the top of RAM, 16 KiB aligned.
    let initrd_len = spec.initrd.len() as u64;
    let initrd_offset = if initrd_len > 0 {
        let aligned = ram_len
            .checked_sub(initrd_len)
            .ok_or_else(|| anyhow::anyhow!("initrd does not fit in RAM"))?
            & !(HOST_PAGE_SIZE as u64 - 1);
        if aligned < kernel_end.next_multiple_of(KERNEL_ALIGN) {
            bail!("initrd overlaps kernel in RAM");
        }
        ram.write(aligned, spec.initrd)?;
        aligned
    } else {
        ram_len
    };
    let initrd_start = RAM_BASE + initrd_offset;
    let initrd_end = initrd_start + initrd_len;

    let pmem = (spec.pmem_len > 0).then(|| (pmem_start(ram_len), spec.pmem_len));
    let extras = (spec.extras_len > 0).then_some((EXTRAS_START, spec.extras_len));

    let dtb = fdt::build(&FdtConfig {
        ram_base: RAM_BASE,
        ram_size: ram_len,
        cmdline: spec.cmdline,
        initrd_start,
        initrd_end,
        seed: spec.seed,
        pmem,
        extras,
    });

    // Place the DTB directly below the initrd, aligned to FDT_ALIGN.
    let dtb_len = dtb.len() as u64;
    let dtb_offset = initrd_offset
        .checked_sub(dtb_len)
        .ok_or_else(|| anyhow::anyhow!("DTB does not fit below initrd"))?
        & !(FDT_ALIGN - 1);
    if dtb_offset < kernel_end {
        bail!("DTB overlaps kernel in RAM");
    }
    ram.write(dtb_offset, &dtb)?;
    let dtb_gpa = RAM_BASE + dtb_offset;

    Ok(BootAddresses {
        kernel_entry,
        dtb_gpa,
    })
}
