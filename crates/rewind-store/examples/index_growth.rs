//! How the page store's index costs as it grows.
//!
//! Grows a store's index to `--entries` entries, a million at a time, as
//! keyframes would, and opens the store after each million, which sorts
//! the entries past the sorted copies into the copies as a `rewind run`
//! or `check` opening the store would. Each open reports how long it took,
//! how many bytes it wrote, and the most anonymous memory it held. Then
//! it times looking up pages the store has, and pages it does not.
//!
//! The entries are written straight into the index file, in its format
//! (hash, pack, offset, length), with random hashes and no packs behind
//! them: opening and looking up read only the index, and writing real
//! pages to reach millions of entries would take most of an hour.
//!
//! ```text
//! cargo run --release -p rewind-store --example index_growth -- \
//!     --dir /tmp/store --entries 16777216
//! ```
//!
//! `--lookups-only` times only the lookups, in a store the benchmark grew
//! before.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rewind_store::{Hash, Store};

/// How many entries each round adds: as many as the store reads into
/// memory past its sorted copies before it sorts them.
const ROUND: u64 = 1 << 20;

/// How many lookups each of the two lookup timings makes.
const LOOKUPS: u64 = 1 << 20;

/// splitmix64, so every run of the benchmark writes the same hashes.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The hash of entry `i`, or for `absent`, a hash no entry has.
fn hash(i: u64, absent: bool) -> Hash {
    let salt = if absent { 0x5bd1_e995_u64 } else { 0 };
    let mut out = [0u8; 32];
    for (n, chunk) in out.chunks_mut(8).enumerate() {
        let word = mix(mix(i ^ salt).wrapping_add(n as u64));
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn field(name: &str, of: &str) -> Option<u64> {
    of.lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// The bytes this process has passed to write(2) so far.
fn written() -> Result<u64> {
    field("wchar:", &fs::read_to_string("/proc/self/io")?).context("no wchar in /proc/self/io")
}

/// The anonymous memory this process holds now, in bytes.
fn anon() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| field("RssAnon:", &s))
        .map_or(0, |kib| kib * 1024)
}

/// Runs `work`, sampling anonymous memory every millisecond, and returns
/// what it gave with the most anonymous memory held above where it began.
fn peak_anon<T>(work: impl FnOnce() -> T) -> (T, u64) {
    let start = anon();
    let peak = Arc::new(AtomicU64::new(start));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(anon(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let out = work();
    peak.fetch_max(anon(), Ordering::Relaxed);
    done.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    (out, peak.load(Ordering::Relaxed).saturating_sub(start))
}

/// The index's files besides the log: their count and size.
fn sorted_files(dir: &Path) -> Result<(usize, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("index.") && name != "index.collecting" {
            count += 1;
            bytes += entry.metadata()?.len();
        }
    }
    Ok((count, bytes))
}

fn append(dir: &Path, from: u64, to: u64) -> Result<()> {
    let index = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("index"))?;
    let mut out = BufWriter::with_capacity(1 << 20, index);
    for i in from..to {
        out.write_all(&hash(i, false))?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&(i * 2048).to_le_bytes())?;
        out.write_all(&2048u32.to_le_bytes())?;
    }
    out.flush()?;
    Ok(())
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut dir = PathBuf::from("index-growth-store");
    let mut entries: u64 = 16 << 20;
    let mut lookups_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lookups-only" => lookups_only = true,
            "--dir" => dir = args.next().context("--dir needs a path")?.into(),
            "--entries" => entries = args.next().context("--entries needs a count")?.parse()?,
            other => bail!("unknown argument {other}"),
        }
    }
    if lookups_only {
        return lookups(&dir, entries);
    }
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(dir.join("packs"))?;
    fs::write(dir.join("format"), "1\n")?;

    println!("entries   open_ms  written_MiB  peak_anon_MiB  files  files_MiB");
    let mut total_ms = 0.0;
    let mut total_written = 0;
    let mut worst_peak = 0;
    let mut at = 0;
    while at < entries {
        let next = (at + ROUND).min(entries);
        append(&dir, at, next)?;
        at = next;

        let before = written()?;
        let start = Instant::now();
        let (store, peak) = peak_anon(|| Store::open(&dir));
        let ms = start.elapsed().as_secs_f64() * 1e3;
        drop(store?);
        let wrote = written()? - before;
        let (files, bytes) = sorted_files(&dir)?;
        println!(
            "{at:>9} {ms:>9.1} {:>12.1} {:>14.1} {files:>6} {:>10.1}",
            mib(wrote),
            mib(peak),
            mib(bytes)
        );
        total_ms += ms;
        total_written += wrote;
        worst_peak = worst_peak.max(peak);
    }
    println!(
        "total: {:.1} s opening, {:.0} MiB written, {:.0} MiB peak anonymous memory",
        total_ms / 1e3,
        mib(total_written),
        mib(worst_peak)
    );

    lookups(&dir, entries)
}

/// Times lookups in the store in `dir`, opened afresh, of `entries`
/// entries, with its files in the page cache from the rounds before.
fn lookups(dir: &Path, entries: u64) -> Result<()> {
    let store = Store::open(dir)?;
    let present = |n: u64| hash(mix(n) % entries, false);
    for (name, of) in [
        ("present", &present as &dyn Fn(u64) -> Hash),
        ("absent", &|n| hash(n, true)),
    ] {
        let start = Instant::now();
        let mut found = 0u64;
        for n in 0..LOOKUPS {
            found += u64::from(store.contains(&of(n)));
        }
        let ns = start.elapsed().as_nanos() as f64 / LOOKUPS as f64;
        println!("lookup {name}: {ns:.0} ns each, {found} of {LOOKUPS} found");
    }
    Ok(())
}
