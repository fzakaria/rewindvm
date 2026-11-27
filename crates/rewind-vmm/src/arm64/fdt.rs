//! Minimal, deterministic Flattened Device Tree (FDT v17) writer for
//! booting an ARM64 Linux guest on Hypervisor.framework and ARM64 KVM.

use std::collections::HashMap;

use crate::arm64::layout::{REWIND_MMIO_BASE, REWIND_MMIO_SIZE};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_VERSION: u32 = 17;
const FDT_LAST_COMP_VERSION: u32 = 16;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

/// Deterministic builder for a Device Tree Blob (`.dtb`).
#[derive(Debug, Default)]
pub struct FdtBuilder {
    struct_block: Vec<u8>,
    strings_block: Vec<u8>,
    string_offsets: HashMap<String, u32>,
}

impl FdtBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_node(&mut self, name: &str) {
        self.push_u32(FDT_BEGIN_NODE);
        self.struct_block.extend_from_slice(name.as_bytes());
        self.struct_block.push(0);
        self.align4();
    }

    pub fn end_node(&mut self) {
        self.push_u32(FDT_END_NODE);
    }

    pub fn prop_bytes(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.intern_string(name);
        self.push_u32(FDT_PROP);
        self.push_u32(value.len() as u32);
        self.push_u32(nameoff);
        self.struct_block.extend_from_slice(value);
        self.align4();
    }

    pub fn prop_str(&mut self, name: &str, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop_bytes(name, &bytes);
    }

    pub fn prop_u32(&mut self, name: &str, value: u32) {
        self.prop_bytes(name, &value.to_be_bytes());
    }

    pub fn prop_u64(&mut self, name: &str, value: u64) {
        self.prop_bytes(name, &value.to_be_bytes());
    }

    pub fn prop_u64_pair(&mut self, name: &str, a: u64, b: u64) {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&a.to_be_bytes());
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        self.prop_bytes(name, &bytes);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.push_u32(FDT_END);

        // Header (40 bytes) + 8-byte alignment -> mem_rsvmap at offset 40.
        // Empty mem_rsvmap terminator: 16 bytes of zero.
        let header_size = 40u32;
        let off_mem_rsvmap = header_size;
        let rsvmap_size = 16u32;
        let off_dt_struct = off_mem_rsvmap + rsvmap_size;
        let size_dt_struct = self.struct_block.len() as u32;
        let off_dt_strings = off_dt_struct + size_dt_struct;
        let size_dt_strings = self.strings_block.len() as u32;
        let totalsize = (off_dt_strings + size_dt_strings).next_multiple_of(8);

        let mut out = Vec::with_capacity(totalsize as usize);
        for word in [
            FDT_MAGIC,
            totalsize,
            off_dt_struct,
            off_dt_strings,
            off_mem_rsvmap,
            FDT_VERSION,
            FDT_LAST_COMP_VERSION,
            0, // boot_cpuid_phys
            size_dt_strings,
            size_dt_struct,
        ] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.struct_block);
        out.extend_from_slice(&self.strings_block);
        out.resize(totalsize as usize, 0);
        out
    }

    fn push_u32(&mut self, value: u32) {
        self.struct_block.extend_from_slice(&value.to_be_bytes());
    }

    fn align4(&mut self) {
        while !self.struct_block.len().is_multiple_of(4) {
            self.struct_block.push(0);
        }
    }

    fn intern_string(&mut self, name: &str) -> u32 {
        if let Some(&off) = self.string_offsets.get(name) {
            return off;
        }
        let off = self.strings_block.len() as u32;
        self.strings_block.extend_from_slice(name.as_bytes());
        self.strings_block.push(0);
        self.string_offsets.insert(name.to_string(), off);
        off
    }
}

/// Parameters describing the ARM64 Rewind guest device tree.
#[derive(Debug)]
pub struct FdtConfig<'a> {
    pub ram_base: u64,
    pub ram_size: u64,
    pub cmdline: &'a str,
    pub initrd_start: u64,
    pub initrd_end: u64,
    pub seed: [u8; 32],
    pub pmem: Option<(u64, u64)>,
    pub extras: Option<(u64, u64)>,
}

/// Builds the complete DTB for an ARM64 Rewind guest.
pub fn build(cfg: &FdtConfig<'_>) -> Vec<u8> {
    let mut fdt = FdtBuilder::new();

    fdt.begin_node("");
    fdt.prop_str("compatible", "rewind,arm64-vm");
    fdt.prop_u32("#address-cells", 2);
    fdt.prop_u32("#size-cells", 2);

    // /chosen
    fdt.begin_node("chosen");
    fdt.prop_str("bootargs", cfg.cmdline);
    if cfg.initrd_end > cfg.initrd_start {
        fdt.prop_u64("linux,initrd-start", cfg.initrd_start);
        fdt.prop_u64("linux,initrd-end", cfg.initrd_end);
    }
    fdt.prop_bytes("rng-seed", &cfg.seed);
    fdt.end_node();

    // /memory
    fdt.begin_node(&format!("memory@{:x}", cfg.ram_base));
    fdt.prop_str("device_type", "memory");
    fdt.prop_u64_pair("reg", cfg.ram_base, cfg.ram_size);
    fdt.end_node();

    // /cpus
    fdt.begin_node("cpus");
    fdt.prop_u32("#address-cells", 1);
    fdt.prop_u32("#size-cells", 0);
    fdt.begin_node("cpu@0");
    fdt.prop_str("device_type", "cpu");
    fdt.prop_str("compatible", "arm,arm-v8");
    fdt.prop_u32("reg", 0);
    fdt.end_node();
    fdt.end_node();

    // /rewind: detected by the Rewind ARM64 Linux platform driver; exposes
    // the paravirtual MMIO register page used on both KVM and HVF.
    fdt.begin_node(&format!("rewind@{REWIND_MMIO_BASE:x}"));
    fdt.prop_str("compatible", "rewind,platform");
    fdt.prop_u64_pair("reg", REWIND_MMIO_BASE, REWIND_MMIO_SIZE);
    fdt.end_node();

    // Persistent memory regions exposed as /dev/pmem0 (and /dev/pmem1).
    if let Some((base, len)) = cfg.pmem {
        fdt.begin_node(&format!("pmem@{base:x}"));
        fdt.prop_str("compatible", "pmem-region");
        fdt.prop_u64_pair("reg", base, len);
        fdt.end_node();
    }
    if let Some((base, len)) = cfg.extras {
        fdt.begin_node(&format!("pmem@{base:x}"));
        fdt.prop_str("compatible", "pmem-region");
        fdt.prop_u64_pair("reg", base, len);
        fdt.end_node();
    }

    fdt.end_node();
    fdt.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdt_header_is_valid_and_deterministic() {
        let cfg = FdtConfig {
            ram_base: 0x4000_0000,
            ram_size: 256 << 20,
            cmdline: "console=rewind",
            initrd_start: 0x4800_0000,
            initrd_end: 0x4810_0000,
            seed: [7; 32],
            pmem: Some((0x1_0000_0000, 4 << 20)),
            extras: None,
        };
        let a = build(&cfg);
        let b = build(&cfg);
        assert_eq!(a, b);
        assert_eq!(u32::from_be_bytes(a[0..4].try_into().unwrap()), FDT_MAGIC);
        assert_eq!(
            u32::from_be_bytes(a[4..8].try_into().unwrap()) as usize,
            a.len()
        );
        assert!(a.windows(b"rewind,platform".len()).any(|w| w == b"rewind,platform"));
        assert!(a.windows(b"pmem-region".len()).any(|w| w == b"pmem-region"));
    }
}
