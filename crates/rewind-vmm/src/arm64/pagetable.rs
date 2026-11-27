//! ARM64 4 KiB-granule translation table walker (3-level and 4-level).

use anyhow::Result;

use crate::arm64::layout::{PAGE_SIZE, RAM_BASE};
use crate::memory::Mapping;

/// Translates `vaddr` through the 4 KiB-granule ARM64 page tables rooted at
/// `ttbr` with address width `64 - txsz` bits. Returns `(gpa, remaining_bytes)`
/// in the mapped page or block, or `Ok(None)` if unmapped.
pub fn walk_arm64(
    ram: &Mapping,
    ttbr: u64,
    txsz: u64,
    vaddr: u64,
) -> Result<Option<(u64, u64)>> {
    let va_bits = 64u64.saturating_sub(txsz);
    // 4 KiB granule: 12 page offset bits, 9 bits per level.
    let start_level: u32 = if va_bits > 39 {
        0
    } else if va_bits > 30 {
        1
    } else {
        2
    };
    let mut table_pa = ttbr & 0x0000_ffff_ffff_f000;
    for level in start_level..=3 {
        let shift = 12 + 9 * (3 - level);
        let idx = (vaddr >> shift) & 0x1ff;
        let desc_pa = table_pa + idx * 8;
        let Some(off) = desc_pa.checked_sub(RAM_BASE) else {
            return Ok(None);
        };
        let mut bytes = [0u8; 8];
        if ram.read(off, &mut bytes).is_err() {
            return Ok(None);
        }
        let desc = u64::from_le_bytes(bytes);
        if desc & 1 == 0 {
            return Ok(None);
        }
        let is_table = (desc & 2) != 0;
        if level == 3 {
            if !is_table {
                return Ok(None);
            }
            let page_pa = desc & 0x0000_ffff_ffff_f000;
            let page_off = vaddr & 0xfff;
            return Ok(Some((page_pa + page_off, PAGE_SIZE as u64 - page_off)));
        }
        if !is_table {
            // Block descriptor at level 1 (1 GiB) or level 2 (2 MiB).
            let block_size = 1u64 << shift;
            let block_mask = block_size - 1;
            let block_pa = desc & 0x0000_ffff_ffff_f000 & !block_mask;
            let block_off = vaddr & block_mask;
            return Ok(Some((block_pa + block_off, block_size - block_off)));
        }
        table_pa = desc & 0x0000_ffff_ffff_f000;
    }
    Ok(None)
}
