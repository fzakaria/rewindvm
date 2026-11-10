//! A content-addressed store of pages.
//!
//! Keyframes are snapshots of guest memory, and consecutive keyframes of a
//! run, keyframes of runs with the same inputs, and a fork and its parent
//! share most of their pages. So pages are stored once each, named by the
//! BLAKE3 hash of their contents, and a keyframe is a list of page numbers
//! and hashes. The all-zero page is never stored: its hash is
//! [`ZERO_PAGE`] and reading it back needs no I/O.
//!
//! On disk a store is a directory of pack files, each a run of compressed
//! pages appended one after another, an index that names every page's
//! pack, offset and length, and a file naming the store's format. Both only ever grow, so a crash can at worst
//! leave a partial last entry, which [`Store::open`] drops.
//!
//! The index can still name a page that is not in its pack: a crash after
//! the index reached the disk and before the pack did, or a full disk that
//! cut an index entry short, shifting every entry any writer appended
//! after it. A page named but not there is never put again, since the
//! run that would put it is deterministic and the store already names
//! its hash. So an opener with the store to itself verifies the entries
//! written since the index was last marked verified, by length and by
//! hash, drops the ones whose pages are not there, and moves the mark;
//! the last store to close marks the whole index once every pack is
//! synced. Verifying the whole index would read every pack: on a 20.7
//! million page store it took 55 s, and checking lengths alone 1.8 s,
//! both too long for every open.
//!
//! Any number of processes can have a store open at once. Each one that
//! writes appends to a pack no other is writing, which it holds a lock on,
//! and appends each index entry in a single write, so entries from
//! different writers never interleave. A store that misses a page rereads
//! the index for entries others added since.
//!
//! A store with many runs has millions of index entries, too many to read
//! into memory every time a store opens. So the index's entries are also
//! kept sorted by hash, in a few sorted segments, files searched where
//! they lie, each covering a stretch of the index: the first from its
//! start, each other from where the one before ends. An opener reads into
//! memory only the entries after the last segment. The index only grows,
//! so a segment never goes stale; an opener that finds many entries past
//! the last one sorts them into a new segment, under a name no other
//! writer has, and renames it into place. Openers take turns at that, so a
//! check's many threads sort the index once between them.
//!
//! A new segment takes the segments before it into itself, merging them as
//! it is written, while the newest of them covers no more than twice what
//! the merge covers after it. So each segment covers more than twice the
//! one after it, there are about log2 of the index's millions of them, and
//! an entry is rewritten only that many times, where one sorted copy of
//! all the index was rewritten whole for every million entries added. The
//! merge reads the segments where they lie and holds only the new entries
//! in memory.
//!
//! Pages no keyframe names any more are removed by a [`Collector`], which
//! needs the store to itself. It copies the pages that stay out of every
//! pack that holds anything else into new packs, writes a new index of
//! only those pages, and then deletes the old packs. Until the new index
//! is renamed over the old, the old one still names every page where it
//! was; after, the new one names every page where it is now.

// Only rewind-vmm and the command touch memory and the kernel directly;
// the store's one exception, mapping its sorted copy, is in mapping.rs.
#![deny(unsafe_code)]

mod mapping;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};

pub type Hash = [u8; 32];

/// The name the all-zero page goes by; it is never written.
pub const ZERO_PAGE: Hash = [0; 32];

/// The size of one index entry: hash, pack number, offset, length.
const INDEX_ENTRY: usize = 32 + 4 + 8 + 4;

/// The index, the directory of packs, and the file every open store holds
/// a shared lock on.
const INDEX: &str = "index";

/// The file that names a store's format: the version as a decimal line.
const FORMAT: &str = "format";
pub const STORE_VERSION: u32 = 1;
const PACKS: &str = "packs";
const LOCK: &str = "lock";

/// The file an opener holds an exclusive lock on while it sorts the
/// index, so the others wait for its copy rather than sort it too.
const SORTING_LOCK: &str = "sorting.lock";

/// A pack's file name after its number.
const PACK_SUFFIX: &str = ".pack";

/// The name a collector writes a new index under before renaming it over
/// the old, followed by its process id.
const COLLECTING: &str = "index.collecting";

/// How much of the index or a pack a collector reads at a time.
const COLLECT_BUFFER: usize = 1 << 20;

/// The sorted copy of the index's first entries that rewind 0.12 and
/// before wrote, read as a segment from the start of the index: a header of
/// a magic value, how many index entries it covers and how many it holds,
/// then those it holds, in the index's format, sorted by hash with each
/// hash once. Its name, followed by a dot, is also where every segment is
/// written before it is renamed into place, so an opener with the store
/// to itself removes what a crash left there, as rewind 0.12 does too.
const SORTED: &str = "index.sorted";
const SORTED_MAGIC: &[u8; 8] = b"rwsort02";
const SORTED_HEADER: usize = 24;

/// A hard link to the index the segments describe. A collector replaces
/// the index by rename, even in older builds, so the link identifies that
/// index until the segments go. Keeping the link also prevents the old
/// inode number from being reused while stale segments still name it.
const SEGMENT_INDEX: &str = "index.segments";

/// A sorted segment of the index's entries from `from` to `to`, named
/// `index.segment.<from>.<to>`: a header of a magic value, `from`, `to`,
/// how many entries it holds, and the index's entries at `from` and at
/// `to` less one as the index had them when the segment was written, then
/// the entries it holds, as in [`SORTED`]. A segment whose two entries are
/// not the index's at those places is not used. The endpoints alone cannot
/// detect a collector moving an interior page, so SEGMENT_INDEX must also
/// name the current index. Segments are written whole and never changed.
const SEGMENT: &str = "index.segment.";
const SEGMENT_MAGIC: &[u8; 8] = b"rwsort03";
const SEGMENT_HEADER: usize = 32 + 2 * INDEX_ENTRY;

/// A new segment takes in the newest segment before it while that segment
/// covers at most this many times what the new one covers so far.
const MERGE_FACTOR: u64 = 2;

/// How many times an opener lists the segments again when one it listed was
/// removed before it opened it, as a merge removes the segments it took in
/// once the segment that holds them has its name.
const LIST_ATTEMPTS: usize = 8;

/// How many index entries past the last segment an opener reads before it
/// writes a new segment that covers them too. Below this, reading them into
/// memory costs less than writing a segment.
#[cfg(not(test))]
const COMPACT_AT: usize = 1 << 20;
#[cfg(test)]
const COMPACT_AT: usize = 4;

/// What each thread reading pages keeps between them: the compressed
/// bytes' buffer and a decompression context. A context made for each
/// page, as `zstd::bulk::decompress_to_buffer` makes one, was a good part
/// of restoring 721,100 pages.
#[derive(Default)]
struct Reading {
    compressed: Vec<u8>,
    decompressor: Option<zstd::bulk::Decompressor<'static>>,
}

/// What each thread compressing a store's new pages keeps between batches:
/// a compression context, and its share of a batch compressed, page after
/// page, with each page's length. In the static musl build on a Zen 3
/// host, a context made for each page, as `zstd::bulk::compress` makes
/// one, took 80 µs a page and most of a keyframe's 200 ms; one context
/// kept takes 18 µs.
struct Writing {
    compressor: zstd::bulk::Compressor<'static>,
    compressed: Vec<u8>,
    lens: Vec<u32>,
}

impl Writing {
    fn new() -> Result<Writing> {
        Ok(Writing {
            compressor: zstd::bulk::Compressor::new(COMPRESSION_LEVEL)?,
            compressed: Vec::new(),
            lens: Vec::new(),
        })
    }

    /// Compresses `pages`, in place of the pages it held before.
    fn compress(&mut self, pages: &[(&[u8], Hash)]) -> Result<()> {
        self.compressed.clear();
        self.lens.clear();
        for (page, _) in pages {
            // The buffer holds the worst case past the pages before,
            // which compressing needs.
            let at = self.compressed.len();
            let bound = zstd::zstd_safe::compress_bound(page.len());
            self.compressed.resize(at + bound, 0);
            let len = self
                .compressor
                .compress_to_buffer(page, &mut self.compressed[at..])?;
            self.compressed.truncate(at + len);
            self.lens.push(u32::try_from(len)?);
        }
        Ok(())
    }
}

/// The most threads that hash or compress a store's pages at once, and
/// the fewest pages worth a thread of their own, a few times what starting
/// a thread costs. In GNU hello's nixpkgs build on a Zen 3 host, a
/// keyframe compresses about 2,400 new pages, 46 ms on one thread, 12 ms
/// on four and 7 ms on eight; sixteen saved about 2 ms more, and each
/// thread is a core taken from the other machines a check runs at once.
#[cfg(not(test))]
const PAGES_PER_WRITER: usize = 64;
#[cfg(test)]
const PAGES_PER_WRITER: usize = 2;
const MAX_PAGE_WRITERS: usize = 8;

/// How many new pages are compressed before any is written. A batch's
/// compressed pages are held in memory at once, each in room for the
/// worst case, a little over 4 KiB, so a full keyframe into a store that
/// has none of a machine's pages holds 4 MiB of them at a time and not
/// the machine's memory. Batches of 4096 pages, 16 MiB, took a first run
/// of echo to 93 MiB at its peak, against 85 MiB with one page at a time;
/// 1024 still give each of MAX_PAGE_WRITERS threads 128 pages a batch.
#[cfg(not(test))]
const COMPRESS_BATCH: usize = 1024;
#[cfg(test)]
const COMPRESS_BATCH: usize = 8;

/// How many threads share `pages` pages: as many as this process may run
/// at once, up to MAX_PAGE_WRITERS, and one for each PAGES_PER_WRITER.
/// The host's count is read once, since reading it reads the process's
/// CPU set and cgroup quota from /proc.
fn writers(pages: usize) -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cores = *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    cores
        .min(MAX_PAGE_WRITERS)
        .min(pages.div_ceil(PAGES_PER_WRITER))
        .max(1)
}

/// Runs `work` on each of `parts` at once, the first on the calling
/// thread and each other on a thread of its own, and returns the error of
/// the first part, in the parts' order, that failed.
fn at_once<P: Send>(
    parts: impl IntoIterator<Item = P>,
    work: impl Fn(P) -> Result<()> + Sync,
) -> Result<()> {
    let mut parts = parts.into_iter();
    let Some(first) = parts.next() else {
        return Ok(());
    };
    let work = &work;
    std::thread::scope(|scope| {
        let others: Vec<_> = parts.map(|part| scope.spawn(move || work(part))).collect();
        let mut result = work(first);
        for other in others {
            let done = other.join().expect("a page writer panicked");
            if result.is_ok() {
                result = done;
            }
        }
        result
    })
}

thread_local! {
    static READING: std::cell::RefCell<Reading> = std::cell::RefCell::default();
}

/// How many guesses a search of the sorted copy makes from where a key
/// should be, the hashes being uniform, before it halves the range instead.
const INTERPOLATIONS: u32 = 16;

/// Pack files roll over past this size.
#[cfg(not(test))]
const PACK_MAX: u64 = 1 << 30;
#[cfg(test)]
const PACK_MAX: u64 = 1 << 16;

/// zstd's fastest level: a keyframe is written while the guest waits.
const COMPRESSION_LEVEL: i32 = 1;

/// The bytes [`is_zero`] reads at a time: an OR over a block compiles to
/// a few vector instructions and one branch.
const ZERO_BLOCK: usize = 64;

/// Whether `data` is all zeros. A byte at a time, a 4 KiB page took
/// 0.86 µs on a Zen 4 host, 64 bytes at a time 0.08 µs. A full keyframe
/// tests every page of guest memory, 262,144 of them in 1 GiB, and
/// every keyframe each page written since the last.
pub fn is_zero(data: &[u8]) -> bool {
    let (blocks, rest) = data.as_chunks::<ZERO_BLOCK>();
    let block_is_zero = |block: &[u8; ZERO_BLOCK]| block.iter().fold(0, |or, b| or | b) == 0;
    blocks.iter().all(block_is_zero) && rest.iter().all(|b| *b == 0)
}

#[derive(Clone, Copy, Debug)]
struct Location {
    pack: u32,
    offset: u64,
    len: u32,
}

impl Location {
    /// The location an index entry names.
    fn of(entry: &[u8]) -> Location {
        Location {
            pack: u32::from_le_bytes(entry[32..36].try_into().expect("four bytes")),
            offset: u64::from_le_bytes(entry[36..44].try_into().expect("eight bytes")),
            len: u32::from_le_bytes(entry[44..48].try_into().expect("four bytes")),
        }
    }
}

/// An index entry as the index file holds it.
type Entry = [u8; INDEX_ENTRY];

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// A sorted segment of the index's entries, mapped into memory where it
/// lies (see [`rewind_mapped`]): a search reads several entries for each
/// page, and a system call for each was 3.3 million of them to restore
/// 721,100 pages.
struct Sorted {
    map: rewind_mapped::Mapped,
    /// How many bytes come before its entries.
    header: usize,
    /// How many entries it holds.
    entries: u64,
    /// The index entries it covers, from `from` up to `to`, duplicates
    /// included.
    from: u64,
    to: u64,
    /// Its file, which a merge that takes it in removes.
    path: PathBuf,
}

impl Sorted {
    /// The segment in the file at `path`, as a segment of the index `log`,
    /// or None when the file is not one. Fails only when the file cannot
    /// be opened, as when a merge has just removed it.
    fn open(path: &Path, log: &File) -> std::io::Result<Option<Sorted>> {
        let file = File::open(path)?;
        Ok(Sorted::of(file, path, log))
    }

    /// The segment `file` holds, or None when it is not a segment of the
    /// index `log`: not one at all, cut short, covering more than the
    /// index holds, or describing another index.
    fn of(file: File, path: &Path, log: &File) -> Option<Sorted> {
        let log_entries = log.metadata().ok()?.len() / INDEX_ENTRY as u64;
        let mut magic = [0u8; 8];
        file.read_exact_at(&mut magic, 0).ok()?;
        let (header, from, to, entries) = if &magic == SORTED_MAGIC {
            let mut header = [0u8; SORTED_HEADER];
            file.read_exact_at(&mut header, 0).ok()?;
            (SORTED_HEADER, 0, u64_at(&header, 8), u64_at(&header, 16))
        } else if &magic == SEGMENT_MAGIC {
            if !segments_match_index(path.parent()?, log).ok()? {
                return None;
            }
            let mut header = [0u8; SEGMENT_HEADER];
            file.read_exact_at(&mut header, 0).ok()?;
            let (from, to) = (u64_at(&header, 8), u64_at(&header, 16));
            if from >= to || to > log_entries {
                return None;
            }
            let ends = header[32..].as_chunks::<INDEX_ENTRY>().0;
            let mut entry = [0u8; INDEX_ENTRY];
            for (at, end) in [from, to - 1].into_iter().zip(ends) {
                log.read_exact_at(&mut entry, at * INDEX_ENTRY as u64)
                    .ok()?;
                if entry != *end {
                    return None;
                }
            }
            (SEGMENT_HEADER, from, to, u64_at(&header, 24))
        } else {
            return None;
        };
        if to > log_entries || entries > to - from {
            return None;
        }
        let body = file.metadata().ok()?.len().checked_sub(header as u64)?;
        if body != entries.checked_mul(INDEX_ENTRY as u64)? {
            return None;
        }
        let len = usize::try_from(body).ok()? + header;
        let map = mapping::sorted(&file, len).ok()?;
        Some(Sorted {
            map,
            header,
            entries,
            from,
            to,
            path: path.to_path_buf(),
        })
    }

    /// How many index entries it covers.
    fn covers(&self) -> u64 {
        self.to - self.from
    }

    fn entry(&self, i: u64) -> Option<Entry> {
        let at = self.header + usize::try_from(i).ok()? * INDEX_ENTRY;
        self.map.bytes().get(at..at + INDEX_ENTRY)?.try_into().ok()
    }

    /// Where a page is, if the segment names it. The hashes are uniform, so
    /// the search first guesses where in the range a key falls from its
    /// leading bytes, which takes a few reads at any size, and halves the
    /// range once guessing has not found it.
    fn find(&self, hash: &Hash) -> Option<Location> {
        let key = u64::from_be_bytes(hash[..8].try_into().expect("eight bytes"));
        let (mut lo, mut hi) = (0u64, self.entries);
        let (mut lo_key, mut hi_key) = (0u64, u64::MAX);
        let mut guesses = 0;
        while lo < hi {
            let mid = if guesses < INTERPOLATIONS && lo_key < hi_key {
                guesses += 1;
                let span = (hi - lo) as u128;
                let into = (key.saturating_sub(lo_key)) as u128 * span / (hi_key - lo_key) as u128;
                lo + (into as u64).min(hi - lo - 1)
            } else {
                lo + (hi - lo) / 2
            };
            let entry = self.entry(mid)?;
            match entry[..32].cmp(&hash[..]) {
                std::cmp::Ordering::Equal => return Some(Location::of(&entry)),
                std::cmp::Ordering::Less => {
                    lo = mid + 1;
                    lo_key = u64::from_be_bytes(entry[..8].try_into().expect("eight bytes"));
                }
                std::cmp::Ordering::Greater => {
                    hi = mid;
                    hi_key = u64::from_be_bytes(entry[..8].try_into().expect("eight bytes"));
                }
            }
        }
        None
    }

    /// Every entry, in order, where it lies.
    fn all(&self) -> &[Entry] {
        self.map.bytes()[self.header..].as_chunks::<INDEX_ENTRY>().0
    }
}

/// Where a store's segments end in its index: past the last of them.
fn covered(segments: &[Arc<Sorted>]) -> u64 {
    segments.last().map_or(0, |r| r.to)
}

/// The index entries from `from` to `to` a segment is named for, if `name`
/// is a segment's.
fn segment_range(name: &str) -> Option<(u64, u64)> {
    let (from, to) = name.strip_prefix(SEGMENT)?.split_once('.')?;
    Some((from.parse().ok()?, to.parse().ok()?))
}

/// The files in `dir` that hold or may hold segments: every segment's, and
/// the sorted copy older builds wrote.
fn segment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let is_segment = name
            .to_str()
            .is_some_and(|n| n == SORTED || segment_range(n).is_some());
        if is_segment {
            files.push(entry.path());
        }
    }
    Ok(files)
}

/// Whether the segment generation still names the open index. Appends
/// keep the identity; a collector's replacement changes it.
fn segments_match_index(dir: &Path, log: &File) -> Result<bool> {
    let generation = match fs::metadata(dir.join(SEGMENT_INDEX)) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let index = log.metadata()?;
    Ok((generation.dev(), generation.ino()) == (index.dev(), index.ino()))
}

/// Binds new segments to the current index. The caller holds the sorting
/// lock or the store exclusively, so no other segment writer can bind a
/// generation meanwhile. Existing readers keep their mapped files.
fn bind_segments(dir: &Path, log: &File) -> Result<()> {
    if segments_match_index(dir, log)? {
        return Ok(());
    }

    // Discard stale segments before the new generation can validate any
    // segment name. The legacy sorted copy belongs to the current index.
    for path in segment_files(dir)? {
        if path.file_name().is_some_and(|name| name == SORTED) {
            continue;
        }
        remove_if_there(&path)?;
    }
    File::open(dir)?.sync_all()?;
    remove_if_there(&dir.join(SEGMENT_INDEX))?;
    fs::hard_link(dir.join(INDEX), dir.join(SEGMENT_INDEX))?;
    Ok(())
}

/// The segments in `dir` that cover the index `log` from its start, each
/// from where the one before ends, oldest first, and the files there that
/// hold no segment of them: a segment another covers, as a crash between a
/// merge's rename and its removals leaves, or one that is not a segment of
/// this index. Of two segments from the same entry, the one that covers
/// more is taken.
fn chain(dir: &Path, log: &File) -> Result<(Vec<Arc<Sorted>>, Vec<PathBuf>)> {
    let mut attempt = 1;
    loop {
        let mut segments = Vec::new();
        let mut unused = Vec::new();
        let mut vanished = false;
        for path in segment_files(dir)? {
            match Sorted::open(&path, log) {
                Ok(Some(segment)) => {
                    // A segment of the new format holds the entries it is
                    // named for; the sorted copy is named for none.
                    let named = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(segment_range);
                    if named.is_none_or(|range| range == (segment.from, segment.to)) {
                        segments.push(segment);
                    } else {
                        unused.push(path);
                    }
                }
                Ok(None) => unused.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => vanished = true,
                Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
            }
        }
        // A merge renames its segment into place before it removes the
        // segments it took in, so listing again finds the segment that
        // holds them.
        if vanished && attempt < LIST_ATTEMPTS {
            attempt += 1;
            continue;
        }

        segments.sort_by_key(|r| (r.from, std::cmp::Reverse(r.to)));
        let mut chain = Vec::new();
        let mut at = 0;
        for segment in segments {
            if segment.from == at {
                at = segment.to;
                chain.push(Arc::new(segment));
            } else {
                unused.push(segment.path);
            }
        }
        return Ok((chain, unused));
    }
}

/// Removes the file at `path`, if it is still there.
fn remove_if_there(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The mark that says how far the index has been verified against the
/// packs: a line of how many of its first entries name pages whose bytes
/// are in their packs and hash to their names, then the index's device and
/// inode numbers, since a collector replaces the index by another file. An
/// opener with the store to itself verifies the entries after the mark and
/// moves it; the last store to close moves it to the end once every pack
/// is synced, so a session that ends well leaves nothing to verify.
const VERIFIED: &str = "index.verified";

/// The flag a writer raises when an index write came up short, as a full
/// disk leaves it: the entries written after the part it left are shifted
/// and read as nothing, so no close may mark them verified. The next
/// opener with the store to itself verifies them, drops them, and lowers
/// the flag.
const TORN: &str = "index.torn";

/// How many of the index `log`'s first entries the mark in `dir` says
/// are verified: none without a mark, or with a mark of another index.
fn verified(dir: &Path, log: &File) -> Result<Option<u64>> {
    let text = match fs::read_to_string(dir.join(VERIFIED)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", dir.join(VERIFIED).display()));
        }
    };
    let mut fields = text.split_whitespace().map(|f| f.parse::<u64>().ok());
    let (Some(Some(entries)), Some(Some(dev)), Some(Some(ino))) =
        (fields.next(), fields.next(), fields.next())
    else {
        return Ok(None);
    };
    let index = log.metadata()?;
    if (dev, ino) != (index.dev(), index.ino()) {
        return Ok(None);
    }
    Ok(Some(entries))
}

/// Marks the first `entries` entries of the index `log` verified, under a
/// name of this writer's own until whole.
fn mark_verified(dir: &Path, log: &File, entries: u64) -> Result<()> {
    static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = log.metadata()?;
    let tmp = dir.join(format!("{VERIFIED}.{}.{n}", std::process::id()));
    fs::write(&tmp, format!("{entries} {} {}\n", index.dev(), index.ino()))?;
    fs::rename(&tmp, dir.join(VERIFIED))?;
    Ok(())
}

/// Whether the page an index entry names is in its pack: within the
/// pack's length, and bytes that decompress and hash to the entry's name.
/// `packs` holds the packs this thread has opened, by number.
fn page_is_there(
    dir: &Path,
    entry: &Entry,
    lengths: &HashMap<u32, u64>,
    packs: &mut HashMap<u32, File>,
    decompressor: &mut zstd::bulk::Decompressor<'static>,
    compressed: &mut Vec<u8>,
    page: &mut Vec<u8>,
) -> Result<bool> {
    let loc = Location::of(entry);
    let end = loc.offset + u64::from(loc.len);
    if loc.len == 0 || lengths.get(&loc.pack).is_none_or(|len| end > *len) {
        return Ok(false);
    }
    let pack = match packs.entry(loc.pack) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(File::open(Store::pack_path(dir, loc.pack))?)
        }
    };
    compressed.resize(loc.len as usize, 0);
    pack.read_exact_at(compressed, loc.offset)?;

    // Bytes that are not a frame, or not one of the size it says, are not
    // the page.
    let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(compressed) else {
        return Ok(false);
    };
    let Ok(size) = usize::try_from(size) else {
        return Ok(false);
    };
    page.clear();
    page.reserve(size);
    let Ok(n) = decompressor.decompress_to_buffer(compressed, page) else {
        return Ok(false);
    };
    Ok(n == size && Store::hash(page) == entry[..32])
}

/// Verifies the entries of the index `log` after the mark against the
/// packs, with the store to the caller alone, and moves the mark to the
/// end. An entry whose page is not in its pack, because a crash kept the
/// pack from reaching the disk while the index did or an index write came
/// up short and shifted every entry after it, is dropped: the index is cut
/// off before the first such entry and the entries after it that do name
/// their pages are appended again, and the segments that covered the cut
/// go, since a segment names its entries where the index had them. A
/// dropped page is then absent, and the next keyframe that has it puts it
/// again.
fn verify_index(dir: &Path, log: &File) -> Result<()> {
    let entries = log.metadata()?.len() / INDEX_ENTRY as u64;
    let from = verified(dir, log)?.unwrap_or(0).min(entries);
    if from == entries {
        remove_if_there(&dir.join(TORN))?;
        return Ok(());
    }

    // The entries after the mark, and each pack's length.
    let mut tail = vec![[0u8; INDEX_ENTRY]; usize::try_from(entries - from)?];
    log.read_exact_at(tail.as_flattened_mut(), from * INDEX_ENTRY as u64)?;
    let mut lengths = HashMap::new();
    for id in Store::pack_ids(dir)? {
        lengths.insert(id, file_len(&Store::pack_path(dir, id))?);
    }

    // Whether each entry's page is there, on threads, as a keyframe's
    // pages are hashed.
    let mut there = vec![true; tail.len()];
    let share = tail.len().div_ceil(writers(tail.len())).max(1);
    at_once(
        tail.chunks(share).zip(there.chunks_mut(share)),
        |(part, out)| {
            let mut packs = HashMap::new();
            let mut decompressor = zstd::bulk::Decompressor::new()?;
            let (mut compressed, mut page) = (Vec::new(), Vec::new());
            for (entry, ok) in part.iter().zip(out) {
                *ok = page_is_there(
                    dir,
                    entry,
                    &lengths,
                    &mut packs,
                    &mut decompressor,
                    &mut compressed,
                    &mut page,
                )?;
            }
            Ok(())
        },
    )?;
    let Some(first) = there.iter().position(|ok| !ok) else {
        mark_verified(dir, log, entries)?;
        remove_if_there(&dir.join(TORN))?;
        return Ok(());
    };

    // The segments that cover the cut go first, so a crash leaves none
    // beside an index whose entries they do not describe.
    let cut = from + first as u64;
    for path in segment_files(dir)? {
        if let Some(Some(segment)) = Sorted::open(&path, log).ok()
            && segment.to <= cut
        {
            continue;
        }
        remove_if_there(&path)?;
    }
    File::open(dir)?.sync_all()?;

    // The index cut off, with the entries after the cut that name their
    // pages appended again, durable before the mark says they are.
    let mut kept = Vec::with_capacity((tail.len() - first) * INDEX_ENTRY);
    for (entry, ok) in tail[first..].iter().zip(&there[first..]) {
        if *ok {
            kept.extend_from_slice(entry);
        }
    }
    log.set_len(cut * INDEX_ENTRY as u64)?;
    log.write_all_at(&kept, cut * INDEX_ENTRY as u64)?;
    log.sync_data()?;
    mark_verified(dir, log, cut + (kept.len() / INDEX_ENTRY) as u64)?;
    remove_if_there(&dir.join(TORN))?;
    Ok(())
}

/// Calls `each` with the entries of `inputs`, each sorted by hash with
/// each hash once, merged in hash order with each hash once: for a hash
/// more than one input has, the entry of the first that has it. Returns
/// how many entries it called `each` with. The inputs are read where they
/// lie, so a merge of segments holds none of them in memory.
fn merge(
    inputs: &[&[Entry]],
    mut each: impl FnMut(&Entry) -> std::io::Result<()>,
) -> std::io::Result<u64> {
    let mut at = vec![0usize; inputs.len()];
    let mut merged = 0u64;
    loop {
        let mut least: Option<(usize, &Entry)> = None;
        let mut left = 0;
        for (i, input) in inputs.iter().enumerate() {
            let Some(entry) = input.get(at[i]) else {
                continue;
            };
            left += 1;
            if least.is_none_or(|(_, l)| entry[..32] < l[..32]) {
                least = Some((i, entry));
            }
        }
        let Some((first, entry)) = least else {
            return Ok(merged);
        };

        // The last input with entries left has nothing to merge with.
        if left == 1 {
            for entry in &inputs[first][at[first]..] {
                each(entry)?;
            }
            return Ok(merged + (inputs[first].len() - at[first]) as u64);
        }
        each(entry)?;
        merged += 1;
        for (i, input) in inputs.iter().enumerate() {
            if input.get(at[i]).is_some_and(|e| e[..32] == entry[..32]) {
                at[i] += 1;
            }
        }
    }
}

/// Writes the entries of `inputs`, oldest first, merged as [`merge`]
/// merges them, as the segment of the index `log`'s entries from `from` to
/// `to`: to a file of this writer's own, then renamed into place. Returns
/// the segment written.
///
/// A process opens a store on each of its threads, as `rewind check`
/// does, and any of them may write a segment, so the file's name counts the
/// segments this process has written as well as naming the process, and is
/// created only if no file has it. A name two writers shared would be
/// truncated under one by the other.
fn write_segment(
    dir: &Path,
    log: &File,
    inputs: &[&[Entry]],
    from: u64,
    to: u64,
) -> Result<Sorted> {
    bind_segments(dir, log)?;
    #[cfg(test)]
    tests::SORTED_WRITES.lock().unwrap().push(dir.to_path_buf());
    static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{SORTED}.{}.{n}", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut out = BufWriter::with_capacity(COLLECT_BUFFER, file);
    out.write_all(&[0; SEGMENT_HEADER])?;
    let entries = merge(inputs, |entry| out.write_all(entry))?;
    let file = out.into_inner()?;

    // The header, now the count is known, with the index's entries at the
    // segment's two ends.
    let mut header = [0u8; SEGMENT_HEADER];
    header[..8].copy_from_slice(SEGMENT_MAGIC);
    header[8..16].copy_from_slice(&from.to_le_bytes());
    header[16..24].copy_from_slice(&to.to_le_bytes());
    header[24..32].copy_from_slice(&entries.to_le_bytes());
    let ends = header[32..].as_chunks_mut::<INDEX_ENTRY>().0;
    for (at, end) in [from, to - 1].into_iter().zip(ends) {
        log.read_exact_at(end, at * INDEX_ENTRY as u64)?;
    }
    file.write_all_at(&header, 0)?;
    file.sync_all()?;
    let path = dir.join(format!("{SEGMENT}{from}.{to}"));
    fs::rename(&tmp, &path)?;
    Sorted::of(file, &path, log).context("the segment just written does not read")
}

/// Hashes a page's key for the index's map by taking its first eight
/// bytes. The keys are BLAKE3 output, already uniform, and a store holds
/// millions of them: hashing each again with the standard hasher made
/// opening a large store take seconds.
#[derive(Default, Debug)]
pub struct PageHasher(u64);

impl Hasher for PageHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut first = [0u8; 8];
        let n = bytes.len().min(first.len());
        first[..n].copy_from_slice(&bytes[..n]);
        self.0 = u64::from_le_bytes(first);
    }

    // The length an array writes before its bytes says nothing.
    fn write_usize(&mut self, _: usize) {}

    fn finish(&self) -> u64 {
        self.0
    }
}

pub type PageHashes = BuildHasherDefault<PageHasher>;

/// A set of pages by hash, such as the ones a collection keeps.
pub type PageSet = HashSet<Hash, PageHashes>;

/// The index as far as this store has read it: the sorted segments of its
/// first entries, and the entries after those in memory.
struct Index {
    /// The segments, oldest first. Shared, so a lookup searches them
    /// without holding the index.
    segments: Arc<[Arc<Sorted>]>,
    pages: HashMap<Hash, Location, PageHashes>,
    /// How many bytes of the index file the segments and `pages` cover
    /// between them, a whole number of entries.
    read: u64,
    /// Whether `pages` holds the index past the segments yet.
    tail: Tail,
}

/// Where a page is in `segments`, the oldest first: the oldest, and
/// largest, holds most pages, and of two entries for one page the older is
/// the one a merge keeps.
fn find_in(segments: &[Arc<Sorted>], hash: &Hash) -> Option<Location> {
    segments.iter().find_map(|segment| segment.find(hash))
}

/// Whether a store has read the index past its segments. Reading it puts
/// every entry there in a map, up to COMPACT_AT of them: 70 ms for
/// 880,000. A store that only reads pages the segments name, as a fork of a
/// run recorded before the last sort does, never needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    Unread,
    Read,
}

impl Index {
    /// Where a page is, as far as this store has read the index.
    fn known(&self, hash: &Hash) -> Option<Location> {
        self.pages
            .get(hash)
            .copied()
            .or_else(|| find_in(&self.segments, hash))
    }
}

/// The pack this store appends to, locked against other writers while the
/// file is open.
struct Pack {
    file: File,
    id: u32,
    len: u64,
}

impl Pack {
    /// Appends `bytes`, and counts them in the pack's length once all of
    /// them are written.
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        tests::cut_short(&mut self.file, bytes)?;
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Pack `id`, opened as `file`, if no other writer holds it and it
    /// has room. The length is read once the lock is held: a writer that
    /// held the pack after `file` was opened may have appended to it
    /// before letting it go, and pages go in at the end it left.
    fn take(file: File, id: u32) -> Result<Option<Pack>> {
        if file.try_lock().is_err() {
            return Ok(None);
        }

        // Dropping the file lets a full pack go again.
        let len = file.metadata()?.len();
        if len >= PACK_MAX {
            return Ok(None);
        }
        Ok(Some(Pack { file, id, len }))
    }
}

pub struct Store {
    dir: PathBuf,
    index: Mutex<Index>,
    index_log: File,
    /// Claimed on the first page put, so a store only read takes none.
    pack: Option<Pack>,
    readers: Mutex<HashMap<u32, Arc<File>>>,
    /// One for each thread that has compressed pages, made as the first
    /// pages are put, as the pack is.
    writing: Vec<Writing>,
    /// Held shared for as long as the store is open, so an opener that
    /// gets it exclusively knows no one else has the store open.
    _lock: File,
}

/// The store's directory: its index holds every page it has, too many to
/// print.
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// How much a store holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: usize,
    pub stored_bytes: u64,
}

/// Makes sure the store in `dir` is in the format this build reads. A new
/// store, with nothing in its index yet, has the format written; one that
/// names another format, or has pages and names none, as a store written
/// before rewind 1.0 has, is refused.
fn check_format(dir: &Path) -> Result<()> {
    let path = dir.join(FORMAT);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let version: u32 = text
                .trim()
                .parse()
                .with_context(|| format!("reading {}", path.display()))?;
            if version != STORE_VERSION {
                bail!(
                    "{} is a page store of format {version}; this build of rewind reads \
                     format {STORE_VERSION}",
                    dir.display()
                );
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let indexed = fs::metadata(dir.join(INDEX)).map_or(0, |m| m.len());
            if indexed > 0 {
                bail!(
                    "{} is a page store from before rewind 1.0, which names no format, and \
                     this build cannot read it. Move it aside or remove it: the runs \
                     recorded before 1.0 that use it do not replay with this build either",
                    dir.display()
                );
            }

            // A new store's format, under a name of its own until whole.
            static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!("{FORMAT}.{}.{n}", std::process::id()));
            fs::write(&tmp, format!("{STORE_VERSION}\n"))?;
            fs::rename(&tmp, &path)?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl Store {
    /// The index, locked. Only a thread that panicked while holding the
    /// lock leaves it poisoned, and that panic is the failure to report.
    fn index(&self) -> MutexGuard<'_, Index> {
        self.index
            .lock()
            .expect("another thread panicked holding the store's index")
    }

    /// The open pack files, locked, as `index`.
    fn readers(&self) -> MutexGuard<'_, HashMap<u32, Arc<File>>> {
        self.readers
            .lock()
            .expect("another thread panicked holding the store's readers")
    }

    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir.join(PACKS))?;
        check_format(dir)?;
        let index_path = dir.join(INDEX);
        let open_index = || {
            OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&index_path)
        };

        // A partial last index entry can only be a crash's, and only an
        // opener with the store to itself may cut it off: with others open
        // it could be an entry still being written. The same goes for a
        // segment still being written.
        let lock = open_lock(&dir.join(LOCK))?;
        if lock.try_lock().is_ok() {
            let index = open_index()?;
            let len = index.metadata()?.len();
            let whole = len / INDEX_ENTRY as u64 * INDEX_ENTRY as u64;
            if whole != len {
                index.set_len(whole)?;
            }

            // A segment a crash left half written, under its writer's
            // name, is no one's now.
            let prefix = format!("{SORTED}.");
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                let stray = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix));
                if stray {
                    fs::remove_file(&path)?;
                }
            }

            // Nor are segments no store reads from: ones a merge took in
            // and a crash kept it from removing, or ones of another index.
            let (_, unused) = chain(dir, &index)?;
            for path in unused {
                remove_if_there(&path)?;
            }
            if !segments_match_index(dir, &index)? {
                remove_if_there(&dir.join(SEGMENT_INDEX))?;
            }

            // An entry a crash left naming a page that never reached its
            // pack, or an index write that came up short left shifted, is
            // dropped here, so the page is put again rather than trusted.
            verify_index(dir, &index)?;
        }
        lock.lock_shared()
            .with_context(|| format!("locking {}", dir.display()))?;

        // The index this store appends to, opened only under the shared
        // lock: a collector that had the store to itself until now may have
        // renamed a new index over the one there was before.
        let index_log = open_index()?;
        let past = |segments: &[Arc<Sorted>]| -> Result<u64> {
            Ok(index_log.metadata()?.len() / INDEX_ENTRY as u64 - covered(segments))
        };
        let (mut segments, _) = chain(dir, &index_log)?;

        // An index run long past its segments gets another, which takes the
        // entries past them in memory. Stores opened at once, as a check's
        // threads open them, take turns: the first sorts them, and the
        // others then find its segment and read only what came after.
        let mut sorting = None;
        if past(&segments)? >= COMPACT_AT as u64 {
            let lock = open_lock(&dir.join(SORTING_LOCK))?;
            lock.lock()
                .with_context(|| format!("waiting to sort {}", dir.display()))?;

            // Another opener may have sorted it while this one waited.
            segments = chain(dir, &index_log)?.0;
            if past(&segments)? >= COMPACT_AT as u64 {
                sorting = Some(lock);
            }
        }
        let read = covered(&segments) * INDEX_ENTRY as u64;

        let store = Store {
            dir: dir.to_path_buf(),
            index: Mutex::new(Index {
                segments: segments.into(),
                pages: HashMap::default(),
                read,
                tail: Tail::Unread,
            }),
            index_log,
            pack: None,
            readers: Mutex::new(HashMap::new()),
            writing: Vec::new(),
            _lock: lock,
        };

        // The entries past the segments go straight from the index file
        // into the new segment, rather than through the map first, which
        // would hold them twice.
        if sorting.is_some() {
            let len = store.index_log.metadata()?.len();
            store.index().read = len / INDEX_ENTRY as u64 * INDEX_ENTRY as u64;
            store.compact()?;
        }
        drop(sorting);

        // The index past the segments is read when a lookup misses them, or
        // before a page is put, so a page already stored is not stored
        // again.
        Ok(store)
    }

    /// Reads the index past the segments, unless this store has.
    fn read_tail(&self) -> Result<()> {
        if self.index().tail == Tail::Unread {
            self.refresh()?;
        }
        Ok(())
    }

    /// Writes a segment that covers the index from the last segment up to
    /// where this store has read it, taking in as many segments before it
    /// as [`MERGE_FACTOR`] says, and reads from the segments from now on.
    fn compact(&self) -> Result<()> {
        let mut index = self.index();
        let covered = covered(&index.segments);
        let read = index.read / INDEX_ENTRY as u64;

        // The entries after the last segment, as the index file has them,
        // then sorted. Stable, so of two entries for one hash the earlier
        // is kept.
        let mut tail: Vec<Entry> = vec![[0; INDEX_ENTRY]; (read - covered) as usize];
        self.index_log
            .read_exact_at(tail.as_flattened_mut(), covered * INDEX_ENTRY as u64)?;
        tail.sort_by(|a, b| a[..32].cmp(&b[..32]));
        tail.dedup_by(|a, b| a[..32] == b[..32]);

        // The segments the new one takes in: the newest while it covers at
        // most MERGE_FACTOR times what the new one covers without it.
        let mut take = index.segments.len();
        let mut covers = read - covered;
        while take > 0 && index.segments[take - 1].covers() <= MERGE_FACTOR * covers {
            take -= 1;
            covers += index.segments[take].covers();
        }
        let from = index.segments.get(take).map_or(covered, |r| r.from);
        let mut inputs: Vec<&[Entry]> = index.segments[take..].iter().map(|r| r.all()).collect();
        inputs.push(&tail);
        let segment = write_segment(&self.dir, &self.index_log, &inputs, from, read)?;

        // The segments taken in go once the new segment has its name; a
        // store that has one mapped keeps reading it.
        let merged: Vec<PathBuf> = index.segments[take..]
            .iter()
            .map(|r| r.path.clone())
            .collect();
        let mut segments: Vec<Arc<Sorted>> = index.segments[..take].to_vec();
        segments.push(Arc::new(segment));
        index.segments = segments.into();
        index.pages.clear();
        drop(index);
        for path in merged {
            remove_if_there(&path)?;
        }
        Ok(())
    }

    /// Reads the index entries written since this store last looked,
    /// by it or by any other.
    fn refresh(&self) -> Result<()> {
        let mut index = self.index();
        let mut file = File::open(self.dir.join(INDEX))?;
        file.seek(SeekFrom::Start(index.read))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (entries, _) = bytes.as_chunks::<INDEX_ENTRY>();
        index.pages.reserve(entries.len());
        for entry in entries {
            let hash: Hash = entry[..32].try_into().expect("32 bytes");
            index.pages.insert(hash, Location::of(entry));
        }
        index.read += (entries.len() * INDEX_ENTRY) as u64;
        index.tail = Tail::Read;
        Ok(())
    }

    fn pack_path(dir: &Path, id: u32) -> PathBuf {
        dir.join(PACKS).join(format!("{id:08}{PACK_SUFFIX}"))
    }

    /// The numbers of the packs in `dir`, in order.
    fn pack_ids(dir: &Path) -> Result<Vec<u32>> {
        let mut ids: Vec<u32> = fs::read_dir(dir.join(PACKS))?
            .filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_suffix(PACK_SUFFIX)?
                    .parse()
                    .ok()
            })
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// A pack to append to that no other writer holds: an existing one with
    /// room, or else a new one.
    fn claim_pack(&self) -> Result<Pack> {
        let ids = Self::pack_ids(&self.dir)?;
        for &id in ids.iter().rev() {
            let file = OpenOptions::new()
                .append(true)
                .open(Self::pack_path(&self.dir, id))?;
            #[cfg(test)]
            tests::append_before_lock(&Self::pack_path(&self.dir, id));
            if let Some(pack) = Pack::take(file, id)? {
                return Ok(pack);
            }
        }

        // Another writer may create the same new pack first, or take the
        // one this store created before this store locks it, so each tries
        // the next number until one is its own.
        let mut id = ids.last().map_or(0, |last| last + 1);
        loop {
            match OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(Self::pack_path(&self.dir, id))
            {
                Ok(file) => {
                    #[cfg(test)]
                    tests::append_before_lock(&Self::pack_path(&self.dir, id));
                    if let Some(pack) = Pack::take(file, id)? {
                        return Ok(pack);
                    }
                    id += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => id += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn hash(data: &[u8]) -> Hash {
        if is_zero(data) {
            return ZERO_PAGE;
        }
        *blake3::hash(data).as_bytes()
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        *hash == ZERO_PAGE || self.location(hash).is_some()
    }

    /// Where a page is, rereading the index once if this store has not
    /// seen it yet.
    fn location(&self, hash: &Hash) -> Option<Location> {
        let known = self.known(hash);
        if known.is_some() {
            return known;
        }
        self.refresh().ok()?;
        self.known(hash)
    }

    /// Where a page is, as far as this store has read the index. The
    /// segments are searched with the index let go, since a search reads
    /// their files and threads restoring a machine look pages up at once.
    fn known(&self, hash: &Hash) -> Option<Location> {
        let segments = {
            let index = self.index();
            if let Some(loc) = index.pages.get(hash) {
                return Some(*loc);
            }
            index.segments.clone()
        };
        find_in(&segments, hash)
    }

    /// The hash of each of `pages`, as [`Store::hash`] names it, in order,
    /// on up to MAX_PAGE_WRITERS threads: a full keyframe hashes every
    /// page of the machine's memory.
    pub fn hash_all(pages: &[&[u8]]) -> Vec<Hash> {
        let mut hashes = vec![ZERO_PAGE; pages.len()];
        let share = pages.len().div_ceil(writers(pages.len())).max(1);
        let parts = pages.chunks(share).zip(hashes.chunks_mut(share));
        at_once(parts, |(part, out)| {
            for (page, hash) in part.iter().zip(out) {
                *hash = Self::hash(page);
            }
            Ok(())
        })
        .expect("hashing a page cannot fail");
        hashes
    }

    /// Stores a page if it is new, and returns its hash either way. New
    /// means new to this store: it does not reread the index for every
    /// page, so two stores may both write a page another wrote meanwhile,
    /// and either copy serves.
    pub fn put(&mut self, data: &[u8]) -> Result<Hash> {
        Ok(self.put_all(&[data])?[0])
    }

    /// Stores each of `pages` that is new, as [`Store::put`] does, and
    /// returns the hash of each, in order. The pages are hashed, and the
    /// new ones compressed, on up to MAX_PAGE_WRITERS threads; then the
    /// calling thread writes them to the pack and the index in the order
    /// of `pages`, so the store's files hold the bytes a put of each page
    /// in turn writes. COMPRESS_BATCH new pages at a time are compressed
    /// and written.
    pub fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<Hash>> {
        let hashes = Self::hash_all(pages);

        // The new pages: not zero, not in the store, and not earlier in
        // `pages`.
        self.read_tail()?;
        let mut fresh = Vec::new();
        {
            let index = self.index();
            let mut queued = PageSet::default();
            for (page, hash) in pages.iter().zip(&hashes) {
                if *hash == ZERO_PAGE || index.known(hash).is_some() || !queued.insert(*hash) {
                    continue;
                }
                fresh.push((*page, *hash));
            }
        }

        // The contexts are taken out while pages go into a pack, which
        // claiming a new pack needs the store for, and put back after. A
        // put that fails makes new ones next time.
        let mut writing = std::mem::take(&mut self.writing);
        for batch in fresh.chunks(COMPRESS_BATCH) {
            self.put_batch(batch, &mut writing)?;
        }
        self.writing = writing;
        Ok(hashes)
    }

    /// Compresses `batch`, new pages and their hashes, on threads, one
    /// context of `writing` each, then appends the pages to the pack in
    /// order and their entries to the index.
    fn put_batch(&mut self, batch: &[(&[u8], Hash)], writing: &mut Vec<Writing>) -> Result<()> {
        let share = batch.len().div_ceil(writers(batch.len())).max(1);
        let parts = batch.len().div_ceil(share);
        while writing.len() < parts {
            writing.push(Writing::new()?);
        }
        at_once(batch.chunks(share).zip(writing.iter_mut()), |(part, w)| {
            w.compress(part)
        })?;

        // Each thread's pages go into the pack in one write, or in one
        // for each pack they fill, the next claimed as one fills up.
        let mut entries = Vec::with_capacity(batch.len() * INDEX_ENTRY);
        let mut locations = Vec::with_capacity(batch.len());
        for (part, w) in batch.chunks(share).zip(writing.iter()) {
            // The bytes of `w.compressed` that go into the current pack.
            let mut start = 0;
            let mut end = 0;
            for ((_, hash), &len) in part.iter().zip(&w.lens) {
                let pending = (end - start) as u64;
                let full = self
                    .pack
                    .as_ref()
                    .is_none_or(|p| p.len + pending + u64::from(len) > PACK_MAX);
                if full {
                    // A full pack gets the pages that fit and is synced as
                    // it is let go, since `sync` only reaches the pack
                    // this store is writing.
                    if let Some(mut old) = self.pack.take() {
                        old.append(&w.compressed[start..end])?;
                        old.file.sync_data()?;
                    }
                    start = end;
                    self.pack = Some(self.claim_pack()?);
                }
                let pack = self.pack.as_ref().expect("claimed above");
                let loc = Location {
                    pack: pack.id,
                    offset: pack.len + (end - start) as u64,
                    len,
                };
                end += len as usize;
                entries.extend_from_slice(hash);
                entries.extend_from_slice(&loc.pack.to_le_bytes());
                entries.extend_from_slice(&loc.offset.to_le_bytes());
                entries.extend_from_slice(&loc.len.to_le_bytes());
                locations.push((*hash, loc));
            }
            // A pack that took only some of the bytes is let go, so the
            // next put claims a pack and reads its length rather than
            // name pages where the length before the failure says.
            let pack = self.pack.as_mut().expect("claimed for the first page");
            if let Err(e) = pack.append(&w.compressed[start..end]) {
                self.pack = None;
                return Err(e.into());
            }
        }

        // The pages are in their packs before the entries naming them are
        // written, and the entries go in one write, whole, after everyone
        // else's.
        // A write that came up short, as a full disk leaves one, left part
        // of an entry that shifts whatever any writer appends after it; the
        // flag keeps every close from marking them verified.
        let written = match (&self.index_log).write(&entries) {
            Ok(written) if written == entries.len() => written,
            short => {
                let _ = File::create(self.dir.join(TORN));
                let written = short?;
                bail!(
                    "wrote {written} of {} index entries' {} bytes",
                    locations.len(),
                    entries.len()
                );
            }
        };
        debug_assert_eq!(written, entries.len());
        self.index().pages.extend(locations);
        Ok(())
    }

    /// Reads a page back into `out`, which must be the page's size.
    pub fn get(&self, hash: &Hash, out: &mut [u8]) -> Result<()> {
        if *hash == ZERO_PAGE {
            out.fill(0);
            return Ok(());
        }
        let loc = self
            .location(hash)
            .with_context(|| format!("page {} is not in the store", hex(hash)))?;
        // The pack's reader is shared, and read with the readers let go,
        // so threads restoring a machine read pages at once.
        let reader = {
            let mut readers = self.readers();
            let reader = match readers.entry(loc.pack) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(Arc::new(File::open(Self::pack_path(&self.dir, loc.pack))?))
                }
            };
            Arc::clone(reader)
        };
        let n = READING.with_borrow_mut(|reading| -> Result<usize> {
            reading.compressed.resize(loc.len as usize, 0);
            reader.read_exact_at(&mut reading.compressed, loc.offset)?;
            let decompressor = match &mut reading.decompressor {
                Some(decompressor) => decompressor,
                None => reading
                    .decompressor
                    .insert(zstd::bulk::Decompressor::new()?),
            };
            Ok(decompressor.decompress_to_buffer(&reading.compressed, out)?)
        })?;
        if n != out.len() {
            bail!("page {} is {n} bytes, expected {}", hex(hash), out.len());
        }

        // A page is named by its hash, so one whose bytes hash to anything
        // else, from a damaged pack or index, is refused rather than
        // restored into a machine as if it were the page.
        if Self::hash(out) != *hash {
            bail!(
                "page {} in the store is damaged: its bytes are another page's",
                hex(hash)
            );
        }
        Ok(())
    }

    /// Makes everything written so far durable.
    pub fn sync(&self) -> Result<()> {
        if let Some(pack) = &self.pack {
            pack.file.sync_data()?;
        }
        self.index_log.sync_data()?;
        Ok(())
    }

    /// Marks the whole index verified when this store is the last to
    /// close, once every pack and the index are durable, so the next
    /// opener with the store to itself verifies nothing a store that
    /// closed well wrote. A store whose index write came up short raised
    /// the flag, and the next such opener verifies instead. Taking the
    /// lock exclusively lets the shared hold go whether or not it is
    /// taken, which a closing store no longer needs.
    fn close(&mut self) -> Result<()> {
        #[cfg(test)]
        if tests::crashing() {
            return Ok(());
        }
        if self._lock.try_lock().is_err() || self.dir.join(TORN).exists() {
            return Ok(());
        }
        for id in Self::pack_ids(&self.dir)? {
            File::open(Self::pack_path(&self.dir, id))?.sync_data()?;
        }
        self.index_log.sync_data()?;
        let entries = self.index_log.metadata()?.len() / INDEX_ENTRY as u64;
        mark_verified(&self.dir, &self.index_log, entries)
    }
}

/// A close that fails to mark the index verified leaves the next opener
/// to verify it; there is no one to tell.
impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Store {
    /// How much the store holds, as far as this store has read the index.
    /// Reads every segment whole.
    pub fn stats(&self) -> Stats {
        // A store whose index cannot be read past the segments counts what
        // it has read.
        let _ = self.read_tail();
        let index = self.index();
        let mut stats = Stats::default();
        let segments: Vec<&[Entry]> = index.segments.iter().map(|r| r.all()).collect();
        let _ = merge(&segments, |entry| {
            stats.pages += 1;
            stats.stored_bytes += u64::from(Location::of(entry).len);
            Ok(())
        });
        for (hash, loc) in &index.pages {
            if find_in(&index.segments, hash).is_none() {
                stats.pages += 1;
                stats.stored_bytes += u64::from(loc.len);
            }
        }
        stats
    }
}

/// Whether `Collector::collect` changes the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Only count what would go.
    DryRun,
    Remove,
}

/// What a collection removed, or would.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Index entries dropped: one for each page that is not kept, and one
    /// for each second copy of a kept page that two writers both stored.
    pub pages: u64,
    /// How many bytes smaller the store is on disk afterwards.
    pub bytes: u64,
}

/// A pack on disk: its number, its size, and how many of its bytes hold
/// pages a collection keeps.
struct PackUse {
    id: u32,
    size: u64,
    kept: u64,
}

impl PackUse {
    /// Bytes of the pack that hold no kept page: removed pages, second
    /// copies, and what a crash left past the last entry.
    fn dead(&self) -> u64 {
        self.size.saturating_sub(self.kept)
    }
}

/// A store held by one process alone, which may remove pages from it. It
/// holds the store's lock exclusively, so a store opened meanwhile waits
/// for it to be dropped.
#[derive(Debug)]
pub struct Collector {
    dir: PathBuf,
    _lock: File,
}

impl Collector {
    /// The store in `dir` to this process alone, or None while another
    /// store has it open, since that store may be about to read a page or
    /// name one in a new keyframe.
    pub fn lock(dir: &Path) -> Result<Option<Collector>> {
        fs::create_dir_all(dir.join(PACKS))?;
        check_format(dir)?;
        let lock = open_lock(&dir.join(LOCK))?;
        match lock.try_lock() {
            Ok(()) => {
                // An entry naming a page that is not in its pack would fail
                // the collection when the page is moved; an opener with the
                // store to itself drops such entries, and so does this.
                let index = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .read(true)
                    .open(dir.join(INDEX))?;
                verify_index(dir, &index)?;
                Ok(Some(Collector {
                    dir: dir.to_path_buf(),
                    _lock: lock,
                }))
            }
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", dir.display()))
            }
        }
    }

    /// Removes every page not in `live`, or with `Act::DryRun` counts what
    /// that would remove. A pack holding only kept pages stays as it is;
    /// the kept pages of any other pack are copied into new packs, and the
    /// old pack is deleted once a new index names the copies.
    pub fn collect(&self, live: &PageSet, act: Act) -> Result<Collected> {
        let (mut kept, dropped) = self.kept_entries(live)?;

        // Each pack's size and the bytes its kept pages take. A pack with
        // anything else in it is rewritten; one with no kept page is only
        // deleted.
        let mut kept_bytes: HashMap<u32, u64> = HashMap::new();
        for loc in kept.values() {
            *kept_bytes.entry(loc.pack).or_default() += u64::from(loc.len);
        }
        let mut packs = Vec::new();
        for id in Store::pack_ids(&self.dir)? {
            let size = fs::metadata(Store::pack_path(&self.dir, id))?.len();
            let kept = kept_bytes.get(&id).copied().unwrap_or(0);
            packs.push(PackUse { id, size, kept });
        }
        let rewritten: Vec<&PackUse> = packs.iter().filter(|p| p.dead() > 0).collect();
        let moves = rewritten.iter().any(|p| p.kept > 0);

        // The index is written again when it loses an entry or a kept page
        // moves, along with one segment of all of it when it is as long as
        // an opener would sort.
        let reindex = dropped > 0 || moves;
        let mut index_now = file_len(&self.dir.join(INDEX))?;
        for segment in segment_files(&self.dir)? {
            index_now += file_len(&segment)?;
        }
        let entries_len = (kept.len() * INDEX_ENTRY) as u64;
        let sorted_len = if kept.len() >= COMPACT_AT {
            SEGMENT_HEADER as u64 + entries_len
        } else {
            0
        };
        let index_after = if reindex {
            entries_len + sorted_len
        } else {
            index_now
        };
        let packs_freed: u64 = rewritten.iter().map(|p| p.dead()).sum();
        let collected = Collected {
            pages: dropped,
            bytes: (packs_freed + index_now).saturating_sub(index_after),
        };
        if act == Act::DryRun || (rewritten.is_empty() && !reindex) {
            return Ok(collected);
        }

        // The kept pages of rewritten packs go into new packs, numbered
        // after every pack there is.
        let next = packs.last().map_or(0, |p| p.id + 1);
        let from: HashSet<u32> = rewritten.iter().map(|p| p.id).collect();
        self.move_pages(&mut kept, &from, next)?;
        if reindex {
            self.write_index(&kept)?;
        }

        // The new index names none of the old packs' pages.
        for pack in &rewritten {
            fs::remove_file(Store::pack_path(&self.dir, pack.id))?;
        }
        Ok(collected)
    }

    /// The first index entry for each page in `live`, and how many other
    /// entries there are. The index is read a buffer at a time, so a
    /// collection holds only the kept entries in memory. A partial last
    /// entry is a crash's, since no one else has the store open.
    fn kept_entries(&self, live: &PageSet) -> Result<(HashMap<Hash, Location, PageHashes>, u64)> {
        let mut kept: HashMap<Hash, Location, PageHashes> = HashMap::default();
        let mut dropped = 0u64;
        let index = match File::open(self.dir.join(INDEX)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((kept, dropped)),
            Err(e) => return Err(e.into()),
        };
        let mut reader = BufReader::with_capacity(COLLECT_BUFFER, index);
        let mut entry = [0u8; INDEX_ENTRY];
        loop {
            match reader.read_exact(&mut entry) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let hash: Hash = entry[..32].try_into().expect("32 bytes");
            if !live.contains(&hash) || kept.contains_key(&hash) {
                dropped += 1;
                continue;
            }
            kept.insert(hash, Location::of(&entry));
        }
        Ok((kept, dropped))
    }

    /// Copies the kept pages of the packs in `from` into new packs
    /// numbered from `next`, as they are stored, and points `kept` at the
    /// copies. Each new pack is synced before the index names it.
    fn move_pages(
        &self,
        kept: &mut HashMap<Hash, Location, PageHashes>,
        from: &HashSet<u32>,
        mut next: u32,
    ) -> Result<()> {
        let mut moving: Vec<(Hash, Location)> = kept
            .iter()
            .filter(|(_, loc)| from.contains(&loc.pack))
            .map(|(hash, loc)| (*hash, *loc))
            .collect();
        moving.sort_unstable_by_key(|(_, loc)| (loc.pack, loc.offset));

        let mut source: Option<(u32, File)> = None;
        let mut out: Option<Pack> = None;
        let mut page = Vec::new();
        for (hash, loc) in moving {
            // The old pack the page is in.
            if source.as_ref().is_none_or(|(id, _)| *id != loc.pack) {
                let file = File::open(Store::pack_path(&self.dir, loc.pack))?;
                source = Some((loc.pack, file));
            }
            let (_, file) = source.as_ref().expect("opened above");
            page.resize(loc.len as usize, 0);
            file.read_exact_at(&mut page, loc.offset)?;

            // A new pack when there is none yet or the page would not fit.
            let full = out
                .as_ref()
                .is_none_or(|p| p.len + u64::from(loc.len) > PACK_MAX);
            if full {
                if let Some(done) = out.take() {
                    done.file.sync_all()?;
                }
                let file = OpenOptions::new()
                    .append(true)
                    .create_new(true)
                    .open(Store::pack_path(&self.dir, next))?;
                out = Some(Pack {
                    file,
                    id: next,
                    len: 0,
                });
                next += 1;
            }
            let pack = out.as_mut().expect("opened above");
            pack.file.write_all(&page)?;
            kept.insert(
                hash,
                Location {
                    pack: pack.id,
                    offset: pack.len,
                    len: loc.len,
                },
            );
            pack.len += u64::from(loc.len);
        }
        if let Some(done) = out {
            done.file.sync_all()?;
        }
        Ok(())
    }

    /// Replaces the index with one entry for each page in `kept`, sorted,
    /// and one segment covering all of it when it is as long as an opener
    /// would sort. The old segments go first, so that no crash leaves them
    /// beside an index they do not describe; until the rename the old
    /// index stands, and after it the new one.
    fn write_index(&self, kept: &HashMap<Hash, Location, PageHashes>) -> Result<()> {
        let mut entries: Vec<[u8; INDEX_ENTRY]> = kept
            .iter()
            .map(|(hash, loc)| {
                let mut entry = [0u8; INDEX_ENTRY];
                entry[..32].copy_from_slice(hash);
                entry[32..36].copy_from_slice(&loc.pack.to_le_bytes());
                entry[36..44].copy_from_slice(&loc.offset.to_le_bytes());
                entry[44..48].copy_from_slice(&loc.len.to_le_bytes());
                entry
            })
            .collect();
        entries.sort_unstable_by(|a, b| a[..32].cmp(&b[..32]));

        // The new index, whole and synced, under a name of its own.
        let tmp = self
            .dir
            .join(format!("{COLLECTING}.{}", std::process::id()));
        let mut out = BufWriter::with_capacity(COLLECT_BUFFER, File::create(&tmp)?);
        for entry in &entries {
            out.write_all(entry)?;
        }
        out.into_inner()?.sync_all()?;

        for segment in segment_files(&self.dir)? {
            remove_if_there(&segment)?;
        }
        remove_if_there(&self.dir.join(SEGMENT_INDEX))?;
        fs::rename(&tmp, self.dir.join(INDEX))?;
        File::open(&self.dir)?.sync_all()?;

        // Every entry of the new index was verified when this collector
        // took the store, and names its page where the copy put it.
        let log = File::open(self.dir.join(INDEX))?;
        mark_verified(&self.dir, &log, entries.len() as u64)?;
        if entries.len() < COMPACT_AT {
            return Ok(());
        }
        write_segment(&self.dir, &log, &[&entries], 0, entries.len() as u64)?;
        Ok(())
    }
}

/// Opens the lock file at `path`, creating it if there is none, for
/// reading and writing. Linux's NFS client emulates flock(2) with POSIX
/// byte-range locks, which take a shared lock only on a file open for
/// reading and an exclusive one only on a file open for writing: a lock
/// file opened write-only, as `File::create` opens it, refuses a shared
/// lock there with EBADF.
pub fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// The size of the file at `path`, or 0 when there is none.
fn file_len(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

pub fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    // The store against a temporary directory: pages come back as they
    // went in, a page is stored once however often it is put, zero pages
    // cost nothing, and everything survives reopening.
    use super::*;

    const PAGE: usize = 4096;

    /// Every directory a sorted copy was written in, once per copy, by any
    /// test of this process.
    pub(super) static SORTED_WRITES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    thread_local! {
        /// Whether the next append to a pack on this thread writes half of
        /// its bytes and fails, as a disk that fills up does.
        static CUT_SHORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

        /// Whether the next store dropped on this thread goes without
        /// marking the index verified, as a store whose process died does.
        static CRASH_ON_DROP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Whether the store being dropped on this thread is to go as a
    /// killed process's would.
    pub(super) fn crashing() -> bool {
        CRASH_ON_DROP.replace(false)
    }

    /// How many entries the mark in `dir` says are verified, of the index
    /// there now.
    fn verified(dir: &Path) -> Option<u64> {
        let log = File::open(dir.join(INDEX)).unwrap();
        super::verified(dir, &log).unwrap()
    }

    /// Writes half of `bytes` to `file` and fails, if a test on this
    /// thread asked for that.
    pub(super) fn cut_short(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
        if !CUT_SHORT.replace(false) {
            return Ok(());
        }
        file.write_all(&bytes[..bytes.len() / 2])?;
        Err(std::io::Error::other("the disk is full"))
    }

    /// Every store directory where the next pack a store claims gets
    /// bytes from another writer, once per entry, by any test of this
    /// process.
    static APPEND_BEFORE_LOCK: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Bytes the writer that `APPEND_BEFORE_LOCK` stands in for appends.
    const OTHER_WRITERS_BYTES: &[u8] = b"another writer's page";

    /// Appends another writer's bytes to `pack` if a test asked for that
    /// in the pack's store: a writer that held the pack, and let it go
    /// after this store opened it and before this store locked it.
    pub(super) fn append_before_lock(pack: &Path) {
        let dir = pack.parent().and_then(Path::parent).unwrap();
        let mut asked = APPEND_BEFORE_LOCK.lock().unwrap();
        let Some(i) = asked.iter().position(|d| d == dir) else {
            return;
        };
        asked.remove(i);
        let mut other = OpenOptions::new().append(true).open(pack).unwrap();
        other.write_all(OTHER_WRITERS_BYTES).unwrap();
    }

    /// The segments a store opened in `dir` reads, oldest first, and the
    /// files there that hold no segment of them.
    fn listed(dir: &Path) -> (Vec<Arc<Sorted>>, Vec<PathBuf>) {
        let log = File::open(dir.join(INDEX)).unwrap();
        chain(dir, &log).unwrap()
    }

    /// The segments a store opened in `dir` reads, oldest first.
    fn segments(dir: &Path) -> Vec<Arc<Sorted>> {
        listed(dir).0
    }

    /// The one segment a store opened in `dir` reads.
    fn only_segment(dir: &Path) -> Arc<Sorted> {
        let mut segments = segments(dir);
        assert_eq!(segments.len(), 1, "one segment");
        segments.remove(0)
    }

    /// How many segments were written in `dir`.
    fn sorted_copies_written(dir: &Path) -> usize {
        SORTED_WRITES
            .lock()
            .unwrap()
            .iter()
            .filter(|d| *d == dir)
            .count()
    }

    /// The index's map hashes a page's key to the key's first eight bytes,
    /// which BLAKE3 already makes uniform, rather than hashing 32 bytes
    /// again; two keys that differ there land apart.
    #[test]
    fn a_page_key_hashes_to_its_first_eight_bytes() {
        use std::hash::BuildHasher;
        let build = PageHashes::default();
        let key = Store::hash(b"some page");
        let first = u64::from_le_bytes(key[..8].try_into().expect("eight bytes"));
        assert_eq!(build.hash_one(key), first);
        assert_ne!(build.hash_one(Store::hash(b"another page")), first);
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn page(fill: u8) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[..64].fill(fill);
        p
    }

    #[test]
    fn one_set_byte_anywhere_makes_a_page_not_zero() {
        // A page of zeros is zero and hashes to ZERO_PAGE. Setting any one
        // byte of it, at each offset in turn, makes it not zero, as does
        // one in the bytes past the last whole block of a length that is
        // not a multiple of the block.
        let mut bytes = vec![0u8; PAGE];
        assert!(is_zero(&bytes));
        assert_eq!(Store::hash(&bytes), ZERO_PAGE);
        for at in 0..PAGE {
            bytes[at] = 1;
            assert!(!is_zero(&bytes), "byte {at} set");
            assert_ne!(Store::hash(&bytes), ZERO_PAGE, "byte {at} set");
            bytes[at] = 0;
        }
        let short = PAGE - 1;
        bytes[short - 1] = 0x80;
        assert!(!is_zero(&bytes[..short]));
        assert!(is_zero(&bytes[..short - 1]));
        assert!(is_zero(&[]));
    }

    /// A store names its format in a file of its own, written when the
    /// store is new and read at every open. A store of another format is
    /// refused, naming it, and so is one with pages but no format named,
    /// as a store written before rewind 1.0 is; a collector refuses both
    /// too, since it rewrites the index.
    #[test]
    fn a_store_names_its_format() {
        let dir = tmp("format");
        let hash = Store::open(&dir).unwrap().put(&page(1)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(FORMAT)).unwrap(),
            format!("{STORE_VERSION}\n")
        );
        let mut out = vec![0u8; PAGE];
        Store::open(&dir).unwrap().get(&hash, &mut out).unwrap();

        fs::write(dir.join(FORMAT), format!("{}\n", STORE_VERSION + 1)).unwrap();
        let err = format!("{:#}", Store::open(&dir).err().unwrap());
        assert!(
            err.contains(&format!("format {}", STORE_VERSION + 1)),
            "{err}"
        );
        assert!(Collector::lock(&dir).is_err());

        fs::remove_file(dir.join(FORMAT)).unwrap();
        let err = format!("{:#}", Store::open(&dir).err().unwrap());
        assert!(err.contains("before rewind 1.0"), "{err}");
        assert!(Collector::lock(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trips_and_deduplicates() {
        let dir = tmp("dedup");
        let mut store = Store::open(&dir).unwrap();
        let a = store.put(&page(1)).unwrap();
        let b = store.put(&page(2)).unwrap();
        assert_eq!(store.put(&page(1)).unwrap(), a);
        assert_eq!(store.stats().pages, 2);

        let mut out = vec![0u8; PAGE];
        store.get(&b, &mut out).unwrap();
        assert_eq!(out, page(2));

        assert_eq!(store.put(&vec![0u8; PAGE]).unwrap(), ZERO_PAGE);
        assert_eq!(store.stats().pages, 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A page of bytes from a xorshift generator seeded with `seed`, which
    /// zstd cannot shrink, so a few of them fill a pack.
    fn noise(seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut p = Vec::with_capacity(PAGE);
        while p.len() < PAGE {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            p.extend_from_slice(&state.to_le_bytes());
        }
        p
    }

    /// How many pages `a_keyframes_pages` holds before the ones it
    /// repeats: enough to fill several packs of the tests' PACK_MAX and
    /// several batches of COMPRESS_BATCH.
    const KEYFRAME_PAGES: u64 = 96;

    /// The pages a keyframe might hold, in its order: zeros, pages that
    /// compress well and pages that do not, some of them twice.
    fn a_keyframes_pages() -> Vec<Vec<u8>> {
        let mut pages = Vec::new();
        for i in 0..KEYFRAME_PAGES {
            match i % 4 {
                0 => pages.push(vec![0u8; PAGE]),
                1 => pages.push(page(i as u8)),
                _ => pages.push(noise(i)),
            }
            if i % 5 == 0 {
                pages.push(noise(i / 2));
            }
        }
        pages
    }

    /// `pages` as the slices `put_all` takes.
    fn slices(pages: &[Vec<u8>]) -> Vec<&[u8]> {
        pages.iter().map(Vec::as_slice).collect()
    }

    /// Every file of the store in `dir` and its bytes, by path.
    fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(d) = dirs.pop() {
            for entry in fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let bytes = fs::read(&path).unwrap();
                files.push((path.strip_prefix(dir).unwrap().to_path_buf(), bytes));
            }
        }
        files.sort();
        files
    }

    #[test]
    fn pages_put_at_once_are_stored_as_put_one_at_a_time() {
        // The same pages, a third of them already stored, go into one
        // store a page at a time and into another all at once, which
        // hashes and compresses them on several threads. The hashes agree,
        // and the two stores' packs and index hold the same bytes, so a
        // keyframe writes what it wrote before its pages were spread over
        // threads.
        let one = tmp("batch-one");
        let all = tmp("batch-all");
        let pages = a_keyframes_pages();
        let stored = &pages[..pages.len() / 3];

        let mut by_one = Store::open(&one).unwrap();
        let hashes_one: Vec<Hash> = pages.iter().map(|p| by_one.put(p).unwrap()).collect();
        by_one.sync().unwrap();

        let mut by_all = Store::open(&all).unwrap();
        by_all.put_all(&slices(stored)).unwrap();
        let hashes_all = by_all.put_all(&slices(&pages)).unwrap();
        by_all.sync().unwrap();

        assert_eq!(hashes_all, hashes_one);
        let packs = fs::read_dir(all.join(PACKS)).unwrap().count();
        assert!(packs > 2, "the pages fill only {packs} packs");
        assert_eq!(files(&all), files(&one));

        // Every page reads back from the store that took them at once.
        let mut out = vec![0u8; PAGE];
        for (p, hash) in pages.iter().zip(&hashes_all) {
            by_all.get(hash, &mut out).unwrap();
            assert_eq!(&out, p);
        }
        fs::remove_dir_all(&one).unwrap();
        fs::remove_dir_all(&all).unwrap();
    }

    #[test]
    fn pages_put_after_a_pack_took_only_some_read_back() {
        // A put whose pack takes half of its pages' bytes fails. The same
        // store then puts the pages again, and each reads back as it went
        // in: the pages that follow the cut-short bytes are named where
        // they are, not where the pack's length before the failure said.
        let dir = tmp("cut-short");
        let pages = a_keyframes_pages();
        let mut store = Store::open(&dir).unwrap();
        store.put(&noise(u64::MAX)).unwrap();

        CUT_SHORT.set(true);
        assert!(store.put_all(&slices(&pages)).is_err());
        let hashes = store.put_all(&slices(&pages)).unwrap();
        let mut out = vec![0u8; PAGE];
        for (p, hash) in pages.iter().zip(&hashes) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(&out, p);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pages_hashed_at_once_hash_as_one_at_a_time() {
        // Hashing a keyframe's pages on several threads names each page
        // as hashing it alone does, in the pages' order.
        let pages = a_keyframes_pages();
        let one: Vec<Hash> = pages.iter().map(|p| Store::hash(p)).collect();
        assert_eq!(Store::hash_all(&slices(&pages)), one);
    }

    #[test]
    fn survives_reopening_and_a_torn_index() {
        let dir = tmp("reopen");
        let a = {
            let mut store = Store::open(&dir).unwrap();
            let a = store.put(&page(7)).unwrap();
            store.sync().unwrap();
            a
        };
        // A partial entry, as a crash mid-write would leave.
        OpenOptions::new()
            .append(true)
            .open(dir.join("index"))
            .unwrap()
            .write_all(&[1, 2, 3])
            .unwrap();

        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        store.get(&a, &mut out).unwrap();
        assert_eq!(out, page(7));
        assert_eq!(
            fs::metadata(dir.join("index")).unwrap().len(),
            INDEX_ENTRY as u64
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The entries of the index in `dir`, in order.
    fn index_entries(dir: &Path) -> Vec<Entry> {
        fs::read(dir.join(INDEX))
            .unwrap()
            .as_chunks::<INDEX_ENTRY>()
            .0
            .to_vec()
    }

    /// Lets `store` go as a process that was killed, or lost power, lets
    /// it go: without marking the index verified.
    fn crash(store: Store) {
        CRASH_ON_DROP.set(true);
        drop(store);
    }

    #[test]
    fn pages_past_a_packs_end_are_absent_after_reopening() {
        // Six pages are put, and the store's process dies before its pack
        // reached the disk: the pack is cut off after the third page while
        // the index names all six, as a power loss after the index was
        // synced and before the pack was leaves them. The next opener with
        // the store to itself drops the three entries whose pages are not
        // there, so the pages are absent and a keyframe puts them again,
        // and the three that are there are found. Opening again finds the
        // index verified and leaves it as it is.
        let dir = tmp("pack-cut-off");
        let mut store = Store::open(&dir).unwrap();
        let hashes = put_pages(&mut store, 1, 6);
        let entries = index_entries(&dir);
        let third = Location::of(&entries[2]);
        crash(store);
        OpenOptions::new()
            .write(true)
            .open(Store::pack_path(&dir, third.pack))
            .unwrap()
            .set_len(third.offset + u64::from(third.len))
            .unwrap();

        let mut store = Store::open(&dir).unwrap();
        for (hash, fill) in hashes.iter().zip(1..) {
            assert_eq!(store.contains(hash), fill <= 3, "page {fill}");
        }
        assert_eq!(index_entries(&dir), entries[..3]);
        assert_eq!(put_pages(&mut store, 4, 3), hashes[3..]);
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        drop(store);
        let before = index_entries(&dir);
        assert_eq!(before.len(), 6);
        drop(Store::open(&dir).unwrap());
        assert_eq!(index_entries(&dir), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_page_whose_bytes_changed_is_absent_after_reopening() {
        // The bytes of the second of three pages are overwritten in the
        // pack, as a disk that lost a write leaves them, and the store's
        // process dies. The next opener finds the page's bytes hash to
        // something else and drops its entry, keeping the other two.
        let dir = tmp("pack-damaged");
        let mut store = Store::open(&dir).unwrap();
        let hashes = put_pages(&mut store, 1, 3);
        let second = Location::of(&index_entries(&dir)[1]);
        crash(store);
        let pack = OpenOptions::new()
            .write(true)
            .open(Store::pack_path(&dir, second.pack))
            .unwrap();
        pack.write_all_at(&vec![0xffu8; second.len as usize], second.offset)
            .unwrap();

        let store = Store::open(&dir).unwrap();
        assert!(store.contains(&hashes[0]));
        assert!(!store.contains(&hashes[1]));
        assert!(store.contains(&hashes[2]));
        assert_eq!(index_entries(&dir).len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_after_a_short_index_write_are_dropped() {
        // A writer's disk fills while it writes an index entry, leaving
        // part of one, and another writer, open at the same time, appends
        // three whole entries after it, each shifted by the part. The next
        // opener with the store to itself cuts the index off before the
        // part, since nothing after it reads as the entry it was: the
        // pages named after the part are absent, put again, and read
        // back.
        const PART: usize = 20;
        let dir = tmp("short-index-write");
        let mut first = Store::open(&dir).unwrap();
        let before = put_pages(&mut first, 1, 3);
        OpenOptions::new()
            .append(true)
            .open(dir.join(INDEX))
            .unwrap()
            .write_all(&[7u8; PART])
            .unwrap();
        let mut second = Store::open(&dir).unwrap();
        let after = put_pages(&mut second, 4, 3);
        crash(second);
        crash(first);

        let mut store = Store::open(&dir).unwrap();
        for hash in &before {
            assert!(store.contains(hash));
        }
        for hash in &after {
            assert!(!store.contains(hash));
        }
        assert_eq!(index_entries(&dir).len(), 3);
        assert_eq!(put_pages(&mut store, 4, 3), after);
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in before.iter().chain(&after).zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_segment_naming_a_dropped_entry_goes_with_it() {
        // Ten pages put by one store, and sorted into a segment by a
        // second opened beside it, which verifies nothing, having the
        // store shared. Both die, and the pack is cut off before the last
        // page. The segment covers the dropped entry, so it goes too, and
        // the nine pages that are there are found through the index and
        // sorted again.
        let dir = tmp("segment-dropped");
        let mut store = Store::open(&dir).unwrap();
        let hashes = put_pages(&mut store, 1, 10);
        let sorter = Store::open(&dir).unwrap();
        let segment = only_segment(&dir).path.clone();
        crash(sorter);
        crash(store);
        let last = Location::of(index_entries(&dir).last().unwrap());
        OpenOptions::new()
            .write(true)
            .open(Store::pack_path(&dir, last.pack))
            .unwrap()
            .set_len(last.offset)
            .unwrap();

        let store = Store::open(&dir).unwrap();
        assert!(!segment.exists());
        assert!(!store.contains(&hashes[9]));
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes[..9].iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        let segment = only_segment(&dir);
        assert_eq!((segment.from, segment.to), (0, 9));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_store_closed_last_marks_the_index_verified() {
        // The last store to close has the store to itself, and marks the
        // whole index verified, so the next opener reads none of it again.
        // A store that crashes marks nothing, and the next opener verifies
        // what was written since the last mark.
        let dir = tmp("verified-mark");
        let mut store = Store::open(&dir).unwrap();
        put_pages(&mut store, 1, 3);
        drop(store);
        assert_eq!(verified(&dir).unwrap(), 3);

        let mut store = Store::open(&dir).unwrap();
        put_pages(&mut store, 4, 2);
        crash(store);
        assert_eq!(verified(&dir).unwrap(), 3);
        drop(Store::open(&dir).unwrap());
        assert_eq!(verified(&dir).unwrap(), 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_collection_verifies_the_index_first_and_marks_the_new_one() {
        // A collector takes the store to itself as an opener does, so it
        // verifies the index first: an entry whose page is not in its pack
        // is dropped before pages are moved, rather than failing the
        // collection. The index it writes is marked verified.
        let dir = tmp("collect-verifies");
        let mut store = Store::open(&dir).unwrap();
        let hashes = put_pages(&mut store, 1, 3);
        let last = Location::of(index_entries(&dir).last().unwrap());
        crash(store);
        OpenOptions::new()
            .write(true)
            .open(Store::pack_path(&dir, last.pack))
            .unwrap()
            .set_len(last.offset)
            .unwrap();

        let collector = Collector::lock(&dir).unwrap().unwrap();
        assert_eq!(verified(&dir).unwrap(), 2);
        collector.collect(&live(&hashes[1..]), Act::Remove).unwrap();
        assert_eq!(verified(&dir).unwrap(), 1);
        drop(collector);
        let store = Store::open(&dir).unwrap();
        assert!(!store.contains(&hashes[0]));
        assert!(store.contains(&hashes[1]));
        assert!(!store.contains(&hashes[2]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stores_open_side_by_side_share_pages() {
        // Two stores open on one directory at once, as two rewind
        // processes would have them: opening the second does not wait for
        // the first to close, each writes to a pack of its own, and a page
        // one puts is there for the other to read, and for any store
        // opened later.
        let dir = tmp("side-by-side");
        let mut first = Store::open(&dir).unwrap();
        let opened = {
            let dir = dir.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || tx.send(Store::open(&dir).unwrap()).unwrap());
            // Generous, so a loaded build machine does not fail it; a store
            // that waited for the first to close would wait forever.
            rx.recv_timeout(std::time::Duration::from_secs(60))
        };
        let mut second = opened.expect("the second store waited for the first to close");

        let a = first.put(&page(1)).unwrap();
        let b = second.put(&page(2)).unwrap();
        let c = first.put(&page(3)).unwrap();
        let mut out = vec![0u8; PAGE];
        second.get(&a, &mut out).unwrap();
        assert_eq!(out, page(1));
        first.get(&b, &mut out).unwrap();
        assert_eq!(out, page(2));

        let packs = fs::read_dir(dir.join("packs")).unwrap().count();
        assert_eq!(packs, 2);
        drop((first, second));

        let later = Store::open(&dir).unwrap();
        for (hash, fill) in [(a, 1), (b, 2), (c, 3)] {
            later.get(&hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pack_another_writer_appended_to_before_the_lock_keeps_its_pages() {
        // Another writer appends to a pack after this store opened it and
        // before this store locks it: to an existing pack let go by the
        // store that wrote it, and to a new pack this store just created.
        // The pages this store puts after those bytes read back as they
        // went in from a store opened after.
        let dir = tmp("append-before-lock");
        let first = Store::open(&dir).unwrap().put(&page(1)).unwrap();
        APPEND_BEFORE_LOCK.lock().unwrap().push(dir.clone());
        let second = Store::open(&dir).unwrap().put(&page(2)).unwrap();

        let mut out = vec![0u8; PAGE];
        let store = Store::open(&dir).unwrap();
        for (hash, fill) in [(first, 1), (second, 2)] {
            store.get(&hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        drop(store);
        fs::remove_dir_all(&dir).unwrap();

        let dir = tmp("append-before-lock-new");
        APPEND_BEFORE_LOCK.lock().unwrap().push(dir.clone());
        let only = Store::open(&dir).unwrap().put(&page(3)).unwrap();
        Store::open(&dir).unwrap().get(&only, &mut out).unwrap();
        assert_eq!(out, page(3));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Puts pages `0..n` with fills from `first` on.
    /// A store opened after another wrote pages finds them and stores
    /// none of them again: its index has them past the sorted copy, which
    /// it reads only when a lookup needs it.
    #[test]
    fn a_page_past_the_sorted_copy_is_found_and_not_stored_again() {
        let dir = tmp("tail-dedupe");
        let hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, 3);
        let index_len = || fs::metadata(dir.join(INDEX)).unwrap().len();
        let before = index_len();

        let mut store = Store::open(&dir).unwrap();
        assert_eq!(store.put(&page(2)).unwrap(), hashes[1]);
        assert_eq!(index_len(), before);
        let mut out = vec![0u8; PAGE];
        store.get(&hashes[2], &mut out).unwrap();
        assert_eq!(out, page(3));
        assert!(store.contains(&hashes[0]));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn put_pages(store: &mut Store, first: u8, n: u8) -> Vec<Hash> {
        (first..first + n)
            .map(|f| store.put(&page(f)).unwrap())
            .collect()
    }

    #[test]
    fn a_long_index_is_sorted_on_opening() {
        // Ten entries are past COMPACT_AT, so reopening writes them to the
        // sorted copy, reads nothing into the map, and still finds and
        // counts every page.
        let dir = tmp("sorted");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 10)
        };
        let store = Store::open(&dir).unwrap();
        let segment = only_segment(&dir);
        assert_eq!((segment.entries, segment.from, segment.to), (10, 0, 10));
        assert!(store.index.lock().unwrap().pages.is_empty());
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        assert_eq!(store.stats().pages, 10);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pages_after_the_sorted_copy_are_read_from_the_log() {
        // A store with a sorted copy puts two more pages, fewer than
        // COMPACT_AT: a store opened after finds all twelve, ten in the
        // sorted copy and two in the log after it.
        let dir = tmp("tail");
        let mut hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 10)
        };
        {
            let mut store = Store::open(&dir).unwrap();
            hashes.extend(put_pages(&mut store, 11, 2));
        }
        let store = Store::open(&dir).unwrap();
        assert!(store.index.lock().unwrap().pages.is_empty());
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        assert_eq!(store.stats().pages, 12);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stores_opening_at_once_on_many_threads_keep_every_page() {
        // `rewind check` opens a store on each worker thread. Each round a
        // writer puts more pages than COMPACT_AT, then sixteen threads of
        // this one process open the store at once, so all of them write a
        // sorted copy together. Every open succeeds, and every page is
        // still found, through the sorted copy, by a store opened after.
        const THREADS: usize = 16;
        const ROUNDS: u8 = 12;
        const PER_ROUND: u8 = 5;
        let dir = tmp("threads");
        let mut writer = Store::open(&dir).unwrap();
        let mut hashes = Vec::new();
        for round in 0..ROUNDS {
            hashes.extend(put_pages(&mut writer, 1 + round * PER_ROUND, PER_ROUND));
            writer.sync().unwrap();
            let start = Arc::new(std::sync::Barrier::new(THREADS));
            let opens: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (dir, start) = (dir.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        Store::open(&dir).map(drop).map_err(|e| format!("{e:#}"))
                    })
                })
                .collect();
            for open in opens {
                open.join().unwrap().unwrap();
            }
        }
        drop(writer);

        let store = Store::open(&dir).unwrap();
        let segments = segments(&dir);
        let held: u64 = segments.iter().map(|r| r.entries).sum();
        assert_eq!(held, hashes.len() as u64);
        assert_eq!(covered(&segments), hashes.len() as u64);
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_page_two_stores_wrote_is_sorted_once() {
        // Two stores open side by side, each having read the index as it
        // put a page of its own, both put the same page, so the log names
        // it twice; the sorted copy keeps one.
        let dir = tmp("twice");
        {
            let mut first = Store::open(&dir).unwrap();
            let mut second = Store::open(&dir).unwrap();
            first.put(&page(2)).unwrap();
            second.put(&page(3)).unwrap();
            first.put(&page(1)).unwrap();
            second.put(&page(1)).unwrap();
            put_pages(&mut first, 4, 2);
        }
        let store = Store::open(&dir).unwrap();
        let segment = only_segment(&dir);
        assert_eq!((segment.entries, segment.to), (5, 6));
        assert_eq!(store.stats().pages, 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stores_opening_at_once_sort_the_index_once() {
        // Sixteen threads open a store whose index has run long past its
        // sorted copy, as a `rewind check` does at its start. One of them
        // sorts it; the others wait and use that copy, rather than every
        // one of them holding and sorting the whole index at once.
        const THREADS: usize = 16;
        let dir = tmp("sort-once");
        {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 20);
        }
        let start = Arc::new(std::sync::Barrier::new(THREADS));
        let opens: Vec<_> = (0..THREADS)
            .map(|_| {
                let (dir, start) = (dir.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    drop(Store::open(&dir).unwrap());
                })
            })
            .collect();
        for open in opens {
            open.join().unwrap();
        }
        assert_eq!(sorted_copies_written(&dir), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sorted_copy_cut_short_is_ignored() {
        // A sorted copy missing its last entry names fewer pages than its
        // header says. It is passed over for the log, which has every page,
        // so the page it lost is found.
        let dir = tmp("short");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 10)
        };
        drop(Store::open(&dir).unwrap());
        let path = only_segment(&dir).path.clone();
        let segment = OpenOptions::new().write(true).open(&path).unwrap();
        let len = segment.metadata().unwrap().len();
        segment.set_len(len - INDEX_ENTRY as u64).unwrap();
        assert!(segments(&dir).is_empty());

        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_sorted_copy_is_ignored() {
        // A sorted copy or segment that is not one, as a crash mid-rename
        // cannot make but a stray file could, is passed over for the log.
        let dir = tmp("damaged");
        let hash = {
            let mut store = Store::open(&dir).unwrap();
            store.put(&page(9)).unwrap()
        };
        fs::write(dir.join(SORTED), b"not a sorted index").unwrap();
        fs::write(dir.join(format!("{SEGMENT}0.1")), b"not a segment either").unwrap();
        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        store.get(&hash, &mut out).unwrap();
        assert_eq!(out, page(9));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sorted_copy_finds_every_key_and_no_other() {
        // A thousand keys written sorted, searched for one by one, and a
        // key that is not there.
        let dir = tmp("search");
        fs::create_dir_all(&dir).unwrap();
        let mut entries: Vec<[u8; INDEX_ENTRY]> = (0u32..1000)
            .map(|i| {
                let mut e = [0u8; INDEX_ENTRY];
                e[..32].copy_from_slice(blake3::hash(&i.to_le_bytes()).as_bytes());
                e[32..36].copy_from_slice(&i.to_le_bytes());
                e
            })
            .collect();
        // The index has them in the order they were made.
        fs::write(dir.join(INDEX), entries.as_flattened()).unwrap();
        let log = File::open(dir.join(INDEX)).unwrap();
        entries.sort_unstable_by(|a, b| a[..32].cmp(&b[..32]));
        let sorted = write_segment(&dir, &log, &[&entries], 0, 1000).unwrap();
        for i in 0u32..1000 {
            let key = *blake3::hash(&i.to_le_bytes()).as_bytes();
            assert_eq!(sorted.find(&key).map(|l| l.pack), Some(i));
        }
        assert!(sorted.find(blake3::hash(b"absent").as_bytes()).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_segment_leaves_a_larger_one_as_it_is() {
        // Twenty pages make one segment. Four more, COMPACT_AT of them,
        // are sorted into a segment of their own on the next open, and the
        // segment of twenty is neither rewritten nor read into memory;
        // every page is still found, and counted once.
        let dir = tmp("leave-larger");
        let mut hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, 20);
        drop(Store::open(&dir).unwrap());
        let first = only_segment(&dir);
        let written = sorted_copies_written(&dir);

        hashes.extend(put_pages(&mut Store::open(&dir).unwrap(), 21, 4));
        let store = Store::open(&dir).unwrap();
        assert_eq!(sorted_copies_written(&dir), written + 1);
        let segments = segments(&dir);
        let ranges: Vec<(u64, u64)> = segments.iter().map(|r| (r.from, r.to)).collect();
        assert_eq!(ranges, vec![(0, 20), (20, 24)]);
        assert_eq!(segments[0].path, first.path);
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        assert_eq!(store.stats().pages, 24);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn segments_stay_few_as_the_index_grows() {
        // Forty rounds of COMPACT_AT new pages, the store opened after
        // each: each segment covers more than MERGE_FACTOR times the
        // segment after it, so there are about log2 of the rounds of them,
        // and every page is found through them.
        let dir = tmp("grow");
        let mut hashes = Vec::new();
        for round in 0..40u8 {
            hashes.extend(put_pages(&mut Store::open(&dir).unwrap(), 1 + round * 4, 4));
            drop(Store::open(&dir).unwrap());
            let segments = segments(&dir);
            assert_eq!(covered(&segments), hashes.len() as u64);
            for pair in segments.windows(2) {
                assert!(pair[0].covers() > MERGE_FACTOR * pair[1].covers());
            }
            assert!(
                segments.len() <= 4,
                "{} segments after round {round}",
                segments.len()
            );
        }
        let store = Store::open(&dir).unwrap();
        for hash in &hashes {
            assert!(store.contains(hash));
        }
        assert_eq!(store.stats().pages, hashes.len());
        assert!(listed(&dir).1.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_page_two_segments_hold_is_counted_once() {
        // A store that read the index before another stored a page stores
        // it again, and here the two entries fall in segments a merge does
        // not join: the page is counted once, and found.
        let dir = tmp("two-segments");
        put_pages(&mut Store::open(&dir).unwrap(), 1, 20);
        drop(Store::open(&dir).unwrap());
        let mut late = Store::open(&dir).unwrap();
        late.put(&page(40)).unwrap();
        put_pages(&mut Store::open(&dir).unwrap(), 60, 12);
        drop(Store::open(&dir).unwrap());
        put_pages(&mut late, 60, 1);
        put_pages(&mut late, 50, 3);
        drop(late);

        let store = Store::open(&dir).unwrap();
        let ranges: Vec<(u64, u64)> = segments(&dir).iter().map(|r| (r.from, r.to)).collect();
        assert_eq!(ranges, vec![(0, 33), (33, 37)]);
        assert_eq!(store.stats().pages, 36);
        assert!(store.contains(&Store::hash(&page(60))));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Writes the sorted copy rewind 0.12 writes of the index in `dir`.
    fn write_old_sorted_copy(dir: &Path) {
        let log = fs::read(dir.join(INDEX)).unwrap();
        let mut entries: Vec<Entry> = log.as_chunks::<INDEX_ENTRY>().0.to_vec();
        let covers = entries.len() as u64;
        entries.sort_by(|a, b| a[..32].cmp(&b[..32]));
        entries.dedup_by(|a, b| a[..32] == b[..32]);
        let mut bytes = SORTED_MAGIC.to_vec();
        bytes.extend_from_slice(&covers.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        bytes.extend_from_slice(entries.as_flattened());
        fs::write(dir.join(SORTED), bytes).unwrap();
    }

    #[test]
    fn a_sorted_copy_from_an_older_build_is_read_as_a_segment() {
        // A store rewind 0.12 sorted: its sorted copy is read as the first
        // segment, not sorted again, until a merge takes it in and removes
        // it.
        let dir = tmp("older-copy");
        let mut hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, 10);
        write_old_sorted_copy(&dir);
        let store = Store::open(&dir).unwrap();
        assert_eq!(sorted_copies_written(&dir), 0);
        let segment = only_segment(&dir);
        assert_eq!(
            (segment.from, segment.to, segment.path.clone()),
            (0, 10, dir.join(SORTED))
        );
        assert!(store.index.lock().unwrap().pages.is_empty());
        drop((store, segment));

        hashes.extend(put_pages(&mut Store::open(&dir).unwrap(), 11, 5));
        let store = Store::open(&dir).unwrap();
        let segment = only_segment(&dir);
        assert_eq!((segment.from, segment.to), (0, 15));
        assert!(!dir.join(SORTED).exists());
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_segment_of_another_index_is_not_read() {
        // A collector of rewind 0.12 rewrites the index and removes only
        // the sorted copy it knows, leaving a segment beside an index it
        // does not describe. The segment is passed over and removed, and
        // the pages are found through the index.
        let dir = tmp("other-index");
        let hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, 10);
        drop(Store::open(&dir).unwrap());
        let path = only_segment(&dir).path.clone();

        let log = fs::read(dir.join(INDEX)).unwrap();
        let mut entries: Vec<Entry> = log.as_chunks::<INDEX_ENTRY>().0.to_vec();
        entries.reverse();
        fs::write(dir.join(INDEX), entries.as_flattened()).unwrap();
        let (segments, unused) = listed(&dir);
        assert!(segments.is_empty());
        assert_eq!(unused, vec![path.clone()]);

        let store = Store::open(&dir).unwrap();
        for hash in &hashes {
            assert!(store.contains(hash));
        }
        assert_eq!(only_segment(&dir).to, 10);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_segment_with_unchanged_ends_does_not_survive_reindexing() {
        // A legacy collector can move an interior page without changing
        // either endpoint. Reject the old segment before choosing a chain.
        const PAGES: u8 = 8;
        const INTERIOR: usize = 3;
        let dir = tmp("reindexed-middle");
        let mut stores: Vec<_> = (0..PAGES).map(|_| Store::open(&dir).unwrap()).collect();
        let hashes: Vec<_> = stores
            .iter_mut()
            .zip(1..=PAGES)
            .map(|(store, fill)| store.put(&page(fill)).unwrap())
            .collect();
        drop(stores);
        let collector = Collector::lock(&dir).unwrap().unwrap();
        let (mut kept, _) = collector.kept_entries(&live(&hashes)).unwrap();
        collector.write_index(&kept).unwrap();
        let segment = only_segment(&dir).path.clone();
        let bytes = fs::read(dir.join(INDEX)).unwrap();
        let before = bytes.as_chunks::<INDEX_ENTRY>().0;
        let mut entries = before.to_vec();
        let hash: Hash = entries[INTERIOR][..32].try_into().unwrap();
        let old = Location::of(&entries[INTERIOR]);

        // Move one page as gc does for a pack with an unfinished append.
        // Replace only the index, leaving the segment an old gc ignores.
        let next = Store::pack_ids(&dir).unwrap().last().unwrap() + 1;
        collector
            .move_pages(&mut kept, &HashSet::from([old.pack]), next)
            .unwrap();
        let new = kept[&hash];
        entries[INTERIOR][32..36].copy_from_slice(&new.pack.to_le_bytes());
        entries[INTERIOR][36..44].copy_from_slice(&new.offset.to_le_bytes());
        entries[INTERIOR][44..48].copy_from_slice(&new.len.to_le_bytes());
        assert_eq!(entries.first(), before.first());
        assert_eq!(entries.last(), before.last());
        let replacement = dir.join(COLLECTING);
        fs::write(&replacement, entries.as_flattened()).unwrap();
        fs::rename(replacement, dir.join(INDEX)).unwrap();
        fs::remove_file(Store::pack_path(&dir, old.pack)).unwrap();
        drop(collector);
        let log = File::open(dir.join(INDEX)).unwrap();
        assert!(Sorted::open(&segment, &log).unwrap().is_none());

        // A legacy sorted copy must win regardless of directory ordering,
        // and the relocated page must be read from its new pack.
        write_old_sorted_copy(&dir);
        let store = Store::open(&dir).unwrap();
        let mut out = vec![0; PAGE];
        store.get(&hash, &mut out).unwrap();
        assert_eq!(Store::hash(&out), hash);
        assert!(!segment.exists());
        assert!(!dir.join(SEGMENT_INDEX).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_opener_rebinds_segments_after_a_replacement() {
        // A shared opener cannot clean the store exclusively. Sorting
        // must discard the old generation before publishing new segments.
        const PAGES: u8 = 8;
        let dir = tmp("rebind-segments");
        let hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, PAGES);
        drop(Store::open(&dir).unwrap());
        let old = File::open(dir.join(INDEX)).unwrap();
        let written = sorted_copies_written(&dir);
        let replacement = dir.join(COLLECTING);
        fs::copy(dir.join(INDEX), &replacement).unwrap();
        fs::rename(replacement, dir.join(INDEX)).unwrap();

        // Stand in for another reader holding the store after the old gc.
        let reader = open_lock(&dir.join(LOCK)).unwrap();
        reader.lock_shared().unwrap();
        let store = Store::open(&dir).unwrap();
        assert_eq!(sorted_copies_written(&dir), written + 1);
        assert!(segments_match_index(&dir, &store.index_log).unwrap());
        assert!(!segments_match_index(&dir, &old).unwrap());
        let mut out = vec![0; PAGE];
        for hash in hashes {
            store.get(&hash, &mut out).unwrap();
            assert_eq!(Store::hash(&out), hash);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn segments_without_a_generation_are_rebuilt() {
        // A copied store that lost hard links, or a crash before the link
        // persisted, must rebuild its cache from the append-only index.
        const PAGES: u8 = 8;
        let dir = tmp("missing-generation");
        let hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, PAGES);
        drop(Store::open(&dir).unwrap());
        let written = sorted_copies_written(&dir);
        fs::remove_file(dir.join(SEGMENT_INDEX)).unwrap();
        let store = Store::open(&dir).unwrap();
        assert_eq!(sorted_copies_written(&dir), written + 1);
        assert!(segments_match_index(&dir, &store.index_log).unwrap());
        for hash in hashes {
            assert!(store.contains(&hash));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn segments_a_merge_left_behind_are_removed() {
        // A crash after a merge renamed its segment into place and before
        // it removed the segments it took in leaves them beside it. Stores
        // read the merged segment, and one with the store to itself
        // removes them.
        let dir = tmp("left-behind");
        put_pages(&mut Store::open(&dir).unwrap(), 1, 10);
        drop(Store::open(&dir).unwrap());
        let taken = only_segment(&dir).path.clone();
        let kept = fs::read(&taken).unwrap();
        put_pages(&mut Store::open(&dir).unwrap(), 11, 10);
        drop(Store::open(&dir).unwrap());
        assert!(!taken.exists());
        fs::write(&taken, kept).unwrap();

        let (segments, unused) = listed(&dir);
        assert_eq!(segments.len(), 1);
        assert_eq!((segments[0].from, segments[0].to), (0, 20));
        assert_eq!(unused, vec![taken.clone()]);
        drop(segments);
        drop(Store::open(&dir).unwrap());
        assert!(!taken.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_collection_leaves_one_segment() {
        // Segments of the old index go with it: the collector writes one
        // segment of the new index, which openers read.
        let dir = tmp("collect-segments");
        let mut hashes = put_pages(&mut Store::open(&dir).unwrap(), 1, 20);
        drop(Store::open(&dir).unwrap());
        hashes.extend(put_pages(&mut Store::open(&dir).unwrap(), 21, 4));
        drop(Store::open(&dir).unwrap());
        assert_eq!(segments(&dir).len(), 2);

        let collector = Collector::lock(&dir).unwrap().unwrap();
        collector.collect(&live(&hashes[2..]), Act::Remove).unwrap();
        drop(collector);
        assert_eq!(segment_files(&dir).unwrap().len(), 1);
        let segment = only_segment(&dir);
        assert_eq!((segment.from, segment.to, segment.entries), (0, 22, 22));
        let store = Store::open(&dir).unwrap();
        assert!(!store.contains(&hashes[0]));
        for hash in &hashes[2..] {
            assert!(store.contains(hash));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_merge_keeps_each_hash_once_from_the_oldest_input() {
        let entry = |key: u8, pack: u32| {
            let mut e = [0u8; INDEX_ENTRY];
            e[0] = key;
            e[32..36].copy_from_slice(&pack.to_le_bytes());
            e
        };
        let old = [entry(1, 0), entry(3, 0), entry(5, 0)];
        let new = [entry(2, 1), entry(3, 1), entry(6, 1)];
        let newest = [entry(5, 2), entry(7, 2)];
        let mut out = Vec::new();
        let n = merge(&[&old, &new, &newest], |e| {
            out.push((e[0], Location::of(e).pack));
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 6);
        assert_eq!(out, vec![(1, 0), (2, 1), (3, 0), (5, 0), (6, 1), (7, 2)]);
    }

    /// The set of `hashes`, as `Collector::collect` takes the pages to
    /// keep.
    fn live(hashes: &[Hash]) -> PageSet {
        hashes.iter().copied().collect()
    }

    /// The bytes of every pack, the index and its sorted copy in `dir`.
    fn on_disk(dir: &Path) -> u64 {
        let packs: u64 = fs::read_dir(dir.join("packs"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        let index = fs::metadata(dir.join("index")).map_or(0, |m| m.len());
        let segments: u64 = segment_files(dir)
            .unwrap()
            .iter()
            .map(|r| fs::metadata(r).unwrap().len())
            .sum();
        packs + index + segments
    }

    #[test]
    fn collecting_keeps_the_live_pages_and_removes_the_rest() {
        // Six pages in one pack, two of them live. A dry run counts four
        // pages and the bytes they and their index entries take, and
        // changes nothing; collecting then frees exactly that, and a store
        // opened after reads the two live pages and has no other.
        let dir = tmp("collect");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 6)
        };
        let keep = live(&[hashes[1], hashes[4]]);
        let before = on_disk(&dir);

        let collector = Collector::lock(&dir)
            .unwrap()
            .expect("no one has the store open");
        let counted = collector.collect(&keep, Act::DryRun).unwrap();
        assert_eq!(counted.pages, 4);
        assert!(counted.bytes > 0);
        assert_eq!(on_disk(&dir), before);

        let collected = collector.collect(&keep, Act::Remove).unwrap();
        assert_eq!(collected, counted);
        assert_eq!(on_disk(&dir), before - collected.bytes);
        drop(collector);

        let store = Store::open(&dir).unwrap();
        assert_eq!(store.stats().pages, 2);
        let mut out = vec![0u8; PAGE];
        for (i, hash) in hashes.iter().enumerate() {
            if keep.contains(hash) {
                store.get(hash, &mut out).unwrap();
                assert_eq!(out, page(i as u8 + 1));
            } else {
                assert!(!store.contains(hash));
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_collected_store_takes_new_pages() {
        // After a collection, a writer appends to the store as before, and
        // the pages it adds and the ones kept all read back.
        let dir = tmp("collect-then-put");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 3)
        };
        let collector = Collector::lock(&dir).unwrap().unwrap();
        collector.collect(&live(&hashes[..1]), Act::Remove).unwrap();
        drop(collector);

        let more = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 10, 2)
        };
        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in [(hashes[0], 1), (more[0], 10), (more[1], 11)] {
            store.get(&hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        assert_eq!(store.stats().pages, 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_page_that_reads_back_wrong_is_refused() {
        // Two pages, then an index whose entry for the first names where
        // the second is, as a damaged index or a torn write could leave
        // it: reading the first fails, saying the page is damaged, rather
        // than handing back the second's bytes.
        let dir = tmp("damaged-page");
        let (a, b) = {
            let mut store = Store::open(&dir).unwrap();
            let hashes = put_pages(&mut store, 1, 2);
            (hashes[0], hashes[1])
        };
        let mut index = fs::read(dir.join(INDEX)).unwrap();
        let entry_of = |index: &[u8], hash: &Hash| {
            index
                .chunks(INDEX_ENTRY)
                .position(|e| e[..32] == hash[..])
                .unwrap()
                * INDEX_ENTRY
        };
        let (at_a, at_b) = (entry_of(&index, &a), entry_of(&index, &b));
        let b_location = index[at_b + 32..at_b + INDEX_ENTRY].to_vec();
        index[at_a + 32..at_a + INDEX_ENTRY].copy_from_slice(&b_location);
        fs::write(dir.join(INDEX), &index).unwrap();

        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        let err = store.get(&a, &mut out).unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        store.get(&b, &mut out).unwrap();
        assert_eq!(out, page(2));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_store_opened_during_a_collection_writes_to_the_new_index() {
        // A store opened while a collector has the store to itself waits
        // for the collection to end, then appends to the index the
        // collector left, not the one it renamed over: a page it puts reads
        // back from a store opened afterwards. The opener starts before the
        // collection, and the sleep gives it time to reach the lock.
        let dir = tmp("open-during-collect");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 3)
        };
        let collector = Collector::lock(&dir).unwrap().unwrap();
        let opener = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&dir).unwrap();
                put_pages(&mut store, 20, 1)[0]
            })
        };
        std::thread::sleep(OPENER_WAIT);
        collector.collect(&live(&hashes[..1]), Act::Remove).unwrap();
        drop(collector);
        let added = opener.join().unwrap();

        let store = Store::open(&dir).unwrap();
        let mut out = vec![0u8; PAGE];
        store.get(&added, &mut out).unwrap();
        assert_eq!(out, page(20));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// How long a test gives a thread opening a store to reach its lock.
    const OPENER_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

    #[test]
    fn a_store_whose_pages_all_live_is_left_as_it_is() {
        // Nothing to remove: no page counted, no byte freed, no file
        // rewritten.
        let dir = tmp("collect-nothing");
        let hashes = {
            let mut store = Store::open(&dir).unwrap();
            put_pages(&mut store, 1, 3)
        };
        let before = fs::read(dir.join("index")).unwrap();
        let collector = Collector::lock(&dir).unwrap().unwrap();
        let collected = collector.collect(&live(&hashes), Act::Remove).unwrap();
        assert_eq!(collected, Collected::default());
        assert_eq!(fs::read(dir.join("index")).unwrap(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pack_no_entry_names_is_removed() {
        // A pack a crash left behind, which no index entry names, holds
        // nothing to keep: collecting deletes it and counts its bytes.
        let dir = tmp("collect-orphan");
        let hash = {
            let mut store = Store::open(&dir).unwrap();
            store.put(&page(1)).unwrap()
        };
        let orphan = Store::pack_path(&dir, 7);
        fs::write(&orphan, [9u8; 100]).unwrap();
        let collector = Collector::lock(&dir).unwrap().unwrap();
        let collected = collector.collect(&live(&[hash]), Act::Remove).unwrap();
        assert_eq!(
            collected,
            Collected {
                pages: 0,
                bytes: 100
            }
        );
        assert!(!orphan.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_store_open_elsewhere_is_not_collected() {
        // While any store has the directory open, no collector gets it,
        // since that store may be about to read or name a page.
        let dir = tmp("collect-busy");
        let store = Store::open(&dir).unwrap();
        assert!(Collector::lock(&dir).unwrap().is_none());
        drop(store);
        assert!(Collector::lock(&dir).unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_store_only_read_writes_nothing() {
        // Opening a store to read pages creates no pack.
        let dir = tmp("read-only");
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.stats().pages, 0);
        assert_eq!(fs::read_dir(dir.join("packs")).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
