//! How [`Memory`], the page hashes a chain of keyframes leaves guest memory
//! with, costs for a large guest.
//!
//! Builds the memory of a 3 GiB guest, 786,432 pages, of which a full
//! keyframe lists 60% as not zero, then writes 60 deltas of 2,400 pages
//! each over it, about what a keyframe of GNU hello's nixpkgs build
//! writes, and times the operations recording, replay checking and
//! bisecting use.
//!
//! ```text
//! cargo run --release -p rewind-core --example memory_chain
//! ```

use std::time::Instant;

use anyhow::Result;
use rewind_core::keyframes::Memory;

const PAGES: u32 = 3 << 18;
const DELTAS: u64 = 60;
const DELTA_PAGES: u32 = 2400;

/// splitmix64, so every run builds the same memory.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn hash(seed: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (n, chunk) in out.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&mix(seed ^ (n as u64) << 56).to_le_bytes());
    }
    out
}

/// Runs `work` `times` times and prints the fastest, in milliseconds.
fn time<T>(name: &str, times: usize, mut work: impl FnMut() -> T) -> T {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..times {
        let start = Instant::now();
        out = Some(std::hint::black_box(work()));
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    println!("{name:<44} {best:>9.2} ms");
    out.expect("times is at least one")
}

fn main() -> Result<()> {
    let full: Vec<(u32, [u8; 32])> = (0..PAGES)
        .filter(|p| mix(u64::from(*p)) % 10 < 6)
        .map(|p| (p, hash(u64::from(p))))
        .collect();
    let deltas: Vec<Vec<(u32, [u8; 32])>> = (0..DELTAS)
        .map(|d| {
            (0..DELTA_PAGES)
                .map(|i| {
                    let page = (mix(d << 32 | u64::from(i)) % u64::from(PAGES)) as u32;
                    // One page in ten is written back to zero.
                    let to = if i % 10 == 0 {
                        [0; 32]
                    } else {
                        hash(d << 40 | u64::from(i))
                    };
                    (page, to)
                })
                .collect()
        })
        .collect();
    println!(
        "{} pages, {} not zero, {DELTAS} deltas of {DELTA_PAGES} pages",
        PAGES,
        full.len()
    );

    let base = time("from_pages (full keyframe)", 5, || {
        Memory::from_pages(&full, PAGES as usize)
    })?;
    time(
        "apply full + deltas (Memory::of a chain)",
        5,
        || -> Result<_> {
            let mut memory = Memory::new(PAGES as usize);
            memory.apply(None, &full)?;
            for (d, delta) in deltas.iter().enumerate() {
                memory.apply(Some(d as u64), delta)?;
            }
            Ok(memory)
        },
    )?;
    time("trim each delta against memory", 5, || -> Result<_> {
        let mut memory = Memory::from_pages(&full, PAGES as usize)?;
        let mut kept = 0;
        for (d, delta) in deltas.iter().enumerate() {
            let mut pages = delta.clone();
            memory.trim(Some(d as u64), &mut pages)?;
            kept += pages.len();
        }
        Ok(kept)
    })?;
    let mut other = Memory::from_pages(&full, PAGES as usize)?;
    other.apply(Some(0), &deltas[0])?;
    let differing = time("differing (two whole memories)", 5, || {
        base.differing(&other)
    });
    let written: Vec<u32> = deltas[0].iter().map(|(p, _)| *p).collect();
    let among = time("differing_among (a delta's pages)", 20, || {
        base.differing_among(&other, &written)
    });
    assert_eq!(differing, among);
    time("hash_of, 1M random pages", 5, || {
        let mut zero = 0u32;
        for i in 0..1u64 << 20 {
            let page = (mix(i) % u64::from(PAGES)) as u32;
            zero += u32::from(base.hash_of(page) == [0; 32]);
        }
        zero
    });
    Ok(())
}
