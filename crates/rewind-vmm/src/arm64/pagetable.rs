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

/// Finds every virtual address in the 4 KiB-granule user page tables rooted at
/// `ttbr` (with `64 - txsz` VA bits) that maps a physical page frame in
/// `frames` (each frame aligned to 4 KiB). Returns `(virtual_page, physical_page)`
/// pairs in ascending virtual address order.
pub fn mappings_in_arm64(
    ram: &Mapping,
    ttbr: u64,
    txsz: u64,
    frames: &std::collections::BTreeSet<u64>,
) -> Result<Vec<(u64, u64)>> {
    if frames.is_empty() {
        return Ok(Vec::new());
    }
    let va_bits = 64u64.saturating_sub(txsz);
    let start_level: u32 = if va_bits > 39 {
        0
    } else if va_bits > 30 {
        1
    } else {
        2
    };
    let table_pa = ttbr & 0x0000_ffff_ffff_f000;
    let mut out = Vec::new();
    walk_mappings(ram, table_pa, start_level, 0, frames, &mut out);
    Ok(out)
}

fn walk_mappings(
    ram: &Mapping,
    table_pa: u64,
    level: u32,
    va_base: u64,
    frames: &std::collections::BTreeSet<u64>,
    out: &mut Vec<(u64, u64)>,
) {
    let Some(off) = table_pa.checked_sub(RAM_BASE) else {
        return;
    };
    let mut table = [0u8; PAGE_SIZE];
    if ram.read(off, &mut table).is_err() {
        return;
    }
    let shift = 12 + 9 * (3 - level);
    let entry_span = 1u64 << shift;
    for (idx, &chunk) in table.as_chunks::<8>().0.iter().enumerate() {
        let desc = u64::from_le_bytes(chunk);
        if desc & 1 == 0 {
            continue;
        }
        let is_table = (desc & 2) != 0;
        let va = va_base + (idx as u64) * entry_span;
        if level == 3 {
            if !is_table {
                continue;
            }
            let page_pa = desc & 0x0000_ffff_ffff_f000;
            if frames.contains(&page_pa) {
                out.push((va, page_pa));
            }
        } else if !is_table {
            let block_mask = entry_span - 1;
            let block_pa = desc & 0x0000_ffff_ffff_f000 & !block_mask;
            for &frame in frames.range(block_pa..block_pa.saturating_add(entry_span)) {
                out.push((va + (frame - block_pa), frame));
            }
        } else {
            let next_pa = desc & 0x0000_ffff_ffff_f000;
            walk_mappings(ram, next_pa, level + 1, va, frames, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn walks_4level_page_and_block_mappings() {
        let mut ram = Mapping::anonymous(64 * 1024).unwrap();
        let l0 = RAM_BASE;
        let l1 = RAM_BASE + 0x1000;
        let l2 = RAM_BASE + 0x2000;
        let l3 = RAM_BASE + 0x3000;
        let target_pa = RAM_BASE + 0x5000;

        // VA 0x0000_0000_0040_2000 -> l0[0], l1[0], l2[2], l3[2]
        ram.write(0, &(l1 | 0x3).to_le_bytes()).unwrap();
        ram.write(0x1000, &(l2 | 0x3).to_le_bytes()).unwrap();
        ram.write(0x2000 + 2 * 8, &(l3 | 0x3).to_le_bytes()).unwrap();
        ram.write(0x3000 + 2 * 8, &(target_pa | 0x3).to_le_bytes())
            .unwrap();

        let (pa, avail) = walk_arm64(&ram, l0, 16, 0x40_2123).unwrap().unwrap();
        assert_eq!(pa, target_pa + 0x123);
        assert_eq!(avail, 0x1000 - 0x123);

        let frames = BTreeSet::from([target_pa]);
        let mapped = mappings_in_arm64(&ram, l0, 16, &frames).unwrap();
        assert_eq!(mapped, vec![(0x40_2000, target_pa)]);
    }
}
