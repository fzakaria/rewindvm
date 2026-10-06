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
//! pages appended one after another, and an index that names every page's
//! pack, offset and length. Both only ever grow, so a crash can at worst
//! leave a partial last entry, which [`Store::open`] drops.
//!
//! Any number of processes can have a store open at once. Each one that
//! writes appends to a pack no other is writing, which it holds a lock on,
//! and appends each index entry in a single write, so entries from
//! different writers never interleave. A store that misses a page rereads
//! the index for entries others added since.
//!
//! A store with many runs has millions of index entries, too many to read
//! into memory every time a store opens. So the index's first entries are
//! also kept sorted by hash, in a file searched where it lies, and an
//! opener reads into memory only the entries after the ones it covers.
//! The index only grows, so a sorted copy of its start never goes stale;
//! an opener that finds many entries past it writes a new one, under a
//! name no other writer has, and renames it over the old. Openers take
//! turns at that, so a check's many threads sort the index once between
//! them rather than each holding all of it in memory at once.
//!
//! Pages no keyframe names any more are removed by a [`Collector`], which
//! needs the store to itself. It copies the pages that stay out of every
//! pack that holds anything else into new packs, writes a new index of
//! only those pages, and then deletes the old packs. Until the new index
//! is renamed over the old, the old one still names every page where it
//! was; after, the new one names every page where it is now.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

pub type Hash = [u8; 32];

/// The name the all-zero page goes by; it is never written.
pub const ZERO_PAGE: Hash = [0; 32];

/// The size of one index entry: hash, pack number, offset, length.
const INDEX_ENTRY: usize = 32 + 4 + 8 + 4;

/// The index, the directory of packs, and the file every open store holds
/// a shared lock on.
const INDEX: &str = "index";
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

/// The sorted copy of the index's first entries: a header of a magic
/// value, how many index entries it covers and how many it holds, then
/// those it holds, in the index's format, sorted by hash with each hash
/// once. A copy whose magic or length disagrees with its header is not
/// used, and the store sorts again from the index, which has every entry.
const SORTED: &str = "index.sorted";
const SORTED_MAGIC: &[u8; 8] = b"rwsort02";
const SORTED_HEADER: usize = 24;

/// How many index entries past the sorted copy an opener reads before it
/// writes a new sorted copy that covers them too. Below this, reading them
/// into memory costs less than rewriting the copy.
#[cfg(not(test))]
const COMPACT_AT: usize = 1 << 20;
#[cfg(test)]
const COMPACT_AT: usize = 4;

/// How many guesses a search of the sorted copy makes from where a key
/// should be, the hashes being uniform, before it halves the range instead.
const INTERPOLATIONS: u32 = 16;

/// Pack files roll over past this size.
const PACK_MAX: u64 = 1 << 30;

/// zstd's fastest level: a keyframe is written while the guest waits.
const COMPRESSION_LEVEL: i32 = 1;

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
            pack: u32::from_le_bytes(entry[32..36].try_into().unwrap()),
            offset: u64::from_le_bytes(entry[36..44].try_into().unwrap()),
            len: u32::from_le_bytes(entry[44..48].try_into().unwrap()),
        }
    }
}

/// The sorted copy of the index's first entries, read where it lies.
struct Sorted {
    file: File,
    /// How many entries it holds.
    entries: u64,
    /// How many index entries it covers, duplicates included.
    covers: u64,
}

impl Sorted {
    /// The sorted copy in `dir`, or None when there is none or the file is
    /// not one.
    fn open(dir: &Path) -> Option<Sorted> {
        Sorted::of(File::open(dir.join(SORTED)).ok()?)
    }

    /// The sorted copy `file` holds, or None when it is not one.
    fn of(file: File) -> Option<Sorted> {
        let mut header = [0u8; SORTED_HEADER];
        file.read_exact_at(&mut header, 0).ok()?;
        if &header[..8] != SORTED_MAGIC {
            return None;
        }
        let covers = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let entries = u64::from_le_bytes(header[16..].try_into().unwrap());
        let body = file
            .metadata()
            .ok()?
            .len()
            .checked_sub(SORTED_HEADER as u64)?;
        if body != entries.checked_mul(INDEX_ENTRY as u64)? {
            return None;
        }
        (entries <= covers).then_some(Sorted {
            file,
            entries,
            covers,
        })
    }

    fn entry(&self, i: u64) -> Option<[u8; INDEX_ENTRY]> {
        let mut entry = [0u8; INDEX_ENTRY];
        let at = SORTED_HEADER as u64 + i * INDEX_ENTRY as u64;
        self.file.read_exact_at(&mut entry, at).ok()?;
        Some(entry)
    }

    /// Where a page is, if the sorted copy names it. The hashes are
    /// uniform, so the search first guesses where in the range a key falls
    /// from its leading bytes, which takes a few reads at any size, and
    /// halves the range once guessing has not found it.
    fn find(&self, hash: &Hash) -> Option<Location> {
        let key = u64::from_be_bytes(hash[..8].try_into().unwrap());
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
                    lo_key = u64::from_be_bytes(entry[..8].try_into().unwrap());
                }
                std::cmp::Ordering::Greater => {
                    hi = mid;
                    hi_key = u64::from_be_bytes(entry[..8].try_into().unwrap());
                }
            }
        }
        None
    }

    /// Every entry, in order.
    fn all(&self) -> Result<Vec<[u8; INDEX_ENTRY]>> {
        let mut body = vec![0u8; (self.entries * INDEX_ENTRY as u64) as usize];
        self.file.read_exact_at(&mut body, SORTED_HEADER as u64)?;
        Ok(body.as_chunks::<INDEX_ENTRY>().0.to_vec())
    }
}

/// Writes `entries`, sorted and each hash once, as the sorted copy of the
/// index's first `covers` entries: to a file of this writer's own, then
/// renamed over the old, so a store reading the old one keeps reading it.
/// Returns the copy written, which another writer's may already have
/// replaced under the name.
///
/// A process opens a store on each of its threads, as `rewind check`
/// does, and any of them may write a sorted copy, so the file's name
/// counts the copies this process has written as well as naming the
/// process, and is created only if no file has it. A name two writers
/// shared would be truncated under one by the other.
fn write_sorted(dir: &Path, entries: &[[u8; INDEX_ENTRY]], covers: u64) -> Result<Sorted> {
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
    let mut out = std::io::BufWriter::new(file);
    out.write_all(SORTED_MAGIC)?;
    out.write_all(&covers.to_le_bytes())?;
    out.write_all(&(entries.len() as u64).to_le_bytes())?;
    for entry in entries {
        out.write_all(entry)?;
    }
    let file = out.into_inner()?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(SORTED))?;
    Sorted::of(file).context("the sorted copy just written does not read")
}

/// Hashes a page's key for the index's map by taking its first eight
/// bytes. The keys are BLAKE3 output, already uniform, and a store holds
/// millions of them: hashing each again with the standard hasher made
/// opening a large store take seconds.
#[derive(Default)]
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

/// The index as far as this store has read it: a sorted copy of its first
/// entries, and the entries after those in memory.
struct Index {
    sorted: Option<Sorted>,
    pages: HashMap<Hash, Location, PageHashes>,
    /// How many bytes of the index file the sorted copy and `pages` cover
    /// between them, a whole number of entries.
    read: u64,
}

impl Index {
    /// Where a page is, as far as this store has read the index.
    fn known(&self, hash: &Hash) -> Option<Location> {
        self.pages
            .get(hash)
            .copied()
            .or_else(|| self.sorted.as_ref()?.find(hash))
    }
}

/// The pack this store appends to, locked against other writers while the
/// file is open.
struct Pack {
    file: File,
    id: u32,
    len: u64,
}

pub struct Store {
    dir: PathBuf,
    index: Mutex<Index>,
    index_log: File,
    /// Claimed on the first page put, so a store only read takes none.
    pack: Option<Pack>,
    readers: Mutex<HashMap<u32, File>>,
    /// Held shared for as long as the store is open, so an opener that
    /// gets it exclusively knows no one else has the store open.
    _lock: File,
}

/// How much a store holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: usize,
    pub stored_bytes: u64,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir.join(PACKS))?;
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
        // sorted copy still being written.
        let lock = File::create(dir.join(LOCK))?;
        if lock.try_lock().is_ok() {
            let index = open_index()?;
            let len = index.metadata()?.len();
            let whole = len / INDEX_ENTRY as u64 * INDEX_ENTRY as u64;
            if whole != len {
                index.set_len(whole)?;
            }

            // A sorted copy a crash left half written, under its writer's
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
        }
        lock.lock_shared()
            .with_context(|| format!("locking {}", dir.display()))?;

        // The index this store appends to, opened only under the shared
        // lock: a collector that had the store to itself until now may have
        // renamed a new index over the one there was before.
        let index_log = open_index()?;

        // The sorted copy, unless it covers more than the index holds, which
        // only a hand could make.
        let sorted_copy = || -> Result<Option<Sorted>> {
            let len = index_log.metadata()?.len();
            Ok(Sorted::open(dir).filter(|s| s.covers * INDEX_ENTRY as u64 <= len))
        };
        let past = |sorted: &Option<Sorted>| -> Result<u64> {
            let covered = sorted.as_ref().map_or(0, |s| s.covers);
            Ok(index_log.metadata()?.len() / INDEX_ENTRY as u64 - covered)
        };
        let mut sorted = sorted_copy()?;

        // An index run long past its sorted copy is sorted again, which
        // takes all of it in memory. Stores opened at once, as a check's
        // threads open them, take turns: the first sorts it, and the others
        // then find its copy and read only what came after.
        let mut sorting = None;
        if past(&sorted)? >= COMPACT_AT as u64 {
            let lock = File::create(dir.join(SORTING_LOCK))?;
            lock.lock()
                .with_context(|| format!("waiting to sort {}", dir.display()))?;

            // Another opener may have sorted it while this one waited.
            sorted = sorted_copy()?;
            if past(&sorted)? >= COMPACT_AT as u64 {
                sorting = Some(lock);
            }
        }
        let read = sorted.as_ref().map_or(0, |s| s.covers * INDEX_ENTRY as u64);

        let store = Store {
            dir: dir.to_path_buf(),
            index: Mutex::new(Index {
                sorted,
                pages: HashMap::default(),
                read,
            }),
            index_log,
            pack: None,
            readers: Mutex::new(HashMap::new()),
            _lock: lock,
        };

        // The entries past the copy go straight from the index file into
        // the new one, rather than through the map first, which would hold
        // them all twice.
        if sorting.is_some() {
            let len = store.index_log.metadata()?.len();
            store.index.lock().unwrap().read = len / INDEX_ENTRY as u64 * INDEX_ENTRY as u64;
            store.compact()?;
        }
        drop(sorting);
        store.refresh()?;
        Ok(store)
    }

    /// Writes a new sorted copy that covers the index up to where this store
    /// has read it, and reads from it from now on.
    fn compact(&self) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        let covered = index.sorted.as_ref().map_or(0, |s| s.covers);
        let mut entries = match &index.sorted {
            Some(sorted) => sorted.all()?,
            None => Vec::new(),
        };

        // The entries after the old copy, as the index file has them.
        let tail = index.read - covered * INDEX_ENTRY as u64;
        let mut bytes = vec![0u8; tail as usize];
        File::open(self.dir.join(INDEX))?
            .read_exact_at(&mut bytes, covered * INDEX_ENTRY as u64)?;
        entries.extend_from_slice(bytes.as_chunks::<INDEX_ENTRY>().0);

        // Stable, so of two entries for one hash the earlier is kept.
        entries.sort_by(|a, b| a[..32].cmp(&b[..32]));
        entries.dedup_by(|a, b| a[..32] == b[..32]);
        // The copy this store wrote, which covers every entry it has read,
        // rather than whichever copy has the name by now.
        index.sorted = Some(write_sorted(
            &self.dir,
            &entries,
            index.read / INDEX_ENTRY as u64,
        )?);
        index.pages.clear();
        Ok(())
    }

    /// Reads the index entries written since this store last looked,
    /// by it or by any other.
    fn refresh(&self) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        let mut file = File::open(self.dir.join(INDEX))?;
        file.seek(SeekFrom::Start(index.read))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (entries, _) = bytes.as_chunks::<INDEX_ENTRY>();
        index.pages.reserve(entries.len());
        for entry in entries {
            let hash: Hash = entry[..32].try_into().unwrap();
            index.pages.insert(hash, Location::of(entry));
        }
        index.read += (entries.len() * INDEX_ENTRY) as u64;
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
            let len = file.metadata()?.len();
            if len < PACK_MAX && file.try_lock().is_ok() {
                return Ok(Pack { file, id, len });
            }
        }

        // Another writer may create the same new pack first, so each tries
        // the next number until one is its own.
        let mut id = ids.last().map_or(0, |last| last + 1);
        loop {
            match OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(Self::pack_path(&self.dir, id))
            {
                Ok(file) => {
                    file.lock()?;
                    return Ok(Pack { file, id, len: 0 });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => id += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn hash(data: &[u8]) -> Hash {
        if data.iter().all(|b| *b == 0) {
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
        let known = self.index.lock().unwrap().known(hash);
        if known.is_some() {
            return known;
        }
        self.refresh().ok()?;
        self.index.lock().unwrap().known(hash)
    }

    /// Stores a page if it is new, and returns its hash either way. New
    /// means new to this store: it does not reread the index for every
    /// page, so two stores may both write a page another wrote meanwhile,
    /// and either copy serves.
    pub fn put(&mut self, data: &[u8]) -> Result<Hash> {
        let hash = Self::hash(data);
        if hash == ZERO_PAGE || self.index.lock().unwrap().known(&hash).is_some() {
            return Ok(hash);
        }
        let compressed = zstd::bulk::compress(data, COMPRESSION_LEVEL)?;
        let full = self
            .pack
            .as_ref()
            .is_none_or(|p| p.len + compressed.len() as u64 > PACK_MAX);
        if full {
            // A full pack is synced as it is let go, since `sync` only
            // reaches the pack this store is writing.
            if let Some(old) = self.pack.take() {
                old.file.sync_data()?;
            }
            self.pack = Some(self.claim_pack()?);
        }
        let pack = self.pack.as_mut().expect("claimed above");
        let loc = Location {
            pack: pack.id,
            offset: pack.len,
            len: compressed.len() as u32,
        };
        pack.file.write_all(&compressed)?;
        pack.len += compressed.len() as u64;

        // The page is in its pack before the entry naming it is written,
        // and the entry goes in one write, whole, after everyone else's.
        let mut entry = Vec::with_capacity(INDEX_ENTRY);
        entry.extend_from_slice(&hash);
        entry.extend_from_slice(&loc.pack.to_le_bytes());
        entry.extend_from_slice(&loc.offset.to_le_bytes());
        entry.extend_from_slice(&loc.len.to_le_bytes());
        let written = (&self.index_log).write(&entry)?;
        if written != INDEX_ENTRY {
            bail!("wrote {written} of an index entry's {INDEX_ENTRY} bytes");
        }
        self.index.lock().unwrap().pages.insert(hash, loc);
        Ok(hash)
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
        let mut compressed = vec![0u8; loc.len as usize];
        {
            let mut readers = self.readers.lock().unwrap();
            let reader = match readers.entry(loc.pack) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(File::open(Self::pack_path(&self.dir, loc.pack))?)
                }
            };
            reader.read_exact_at(&mut compressed, loc.offset)?;
        }
        let n = zstd::bulk::decompress_to_buffer(&compressed, out)?;
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

    /// How much the store holds, as far as this store has read the index.
    /// Reads the whole sorted copy.
    pub fn stats(&self) -> Stats {
        let index = self.index.lock().unwrap();
        let sorted = index
            .sorted
            .as_ref()
            .and_then(|s| s.all().ok())
            .unwrap_or_default();
        let in_sorted = |hash: &Hash| index.sorted.as_ref().and_then(|s| s.find(hash)).is_some();
        let after: Vec<&Location> = index
            .pages
            .iter()
            .filter(|(hash, _)| !in_sorted(hash))
            .map(|(_, loc)| loc)
            .collect();
        Stats {
            pages: sorted.len() + after.len(),
            stored_bytes: sorted
                .iter()
                .map(|e| Location::of(e).len as u64)
                .sum::<u64>()
                + after.iter().map(|l| l.len as u64).sum::<u64>(),
        }
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
        let lock = File::create(dir.join(LOCK))?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Collector {
                dir: dir.to_path_buf(),
                _lock: lock,
            })),
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
        // moves, along with a sorted copy of all of it when it is as long
        // as an opener would sort.
        let reindex = dropped > 0 || moves;
        let index_now = file_len(&self.dir.join(INDEX))? + file_len(&self.dir.join(SORTED))?;
        let entries_len = (kept.len() * INDEX_ENTRY) as u64;
        let sorted_len = if kept.len() >= COMPACT_AT {
            SORTED_HEADER as u64 + entries_len
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
            let hash: Hash = entry[..32].try_into().unwrap();
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
    /// and a sorted copy covering all of it when it is as long as an opener
    /// would sort. The old sorted copy goes first, so that no crash leaves
    /// it beside an index it does not describe; until the rename the old
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

        match fs::remove_file(self.dir.join(SORTED)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        fs::rename(&tmp, self.dir.join(INDEX))?;
        File::open(&self.dir)?.sync_all()?;
        if entries.len() < COMPACT_AT {
            return Ok(());
        }
        write_sorted(&self.dir, &entries, entries.len() as u64)?;
        Ok(())
    }
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

    /// How many sorted copies were written in `dir`.
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
        let first = u64::from_le_bytes(key[..8].try_into().unwrap());
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

    /// Puts pages `0..n` with fills from `first` on.
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
        let sorted = Sorted::open(&dir).expect("a sorted copy was written");
        assert_eq!((sorted.entries, sorted.covers), (10, 10));
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
        assert_eq!(store.index.lock().unwrap().pages.len(), 2);
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
            let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
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
        let sorted = Sorted::open(&dir).expect("a sorted copy was written");
        assert_eq!(sorted.entries, hashes.len() as u64);
        let mut out = vec![0u8; PAGE];
        for (hash, fill) in hashes.iter().zip(1..) {
            store.get(hash, &mut out).unwrap();
            assert_eq!(out, page(fill));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_page_two_stores_wrote_is_sorted_once() {
        // Two stores open side by side both put the same page, so the log
        // names it twice; the sorted copy keeps one.
        let dir = tmp("twice");
        {
            let mut first = Store::open(&dir).unwrap();
            let mut second = Store::open(&dir).unwrap();
            first.put(&page(1)).unwrap();
            second.put(&page(1)).unwrap();
            put_pages(&mut first, 2, 4);
        }
        let store = Store::open(&dir).unwrap();
        let sorted = Sorted::open(&dir).unwrap();
        assert_eq!((sorted.entries, sorted.covers), (5, 6));
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
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
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
        let sorted = OpenOptions::new()
            .write(true)
            .open(dir.join(SORTED))
            .unwrap();
        let len = sorted.metadata().unwrap().len();
        sorted.set_len(len - INDEX_ENTRY as u64).unwrap();

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
        // A sorted copy that is not one, as a crash mid-rename cannot make
        // but a stray file could, is passed over for the log.
        let dir = tmp("damaged");
        let hash = {
            let mut store = Store::open(&dir).unwrap();
            store.put(&page(9)).unwrap()
        };
        fs::write(dir.join(SORTED), b"not a sorted index").unwrap();
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
        entries.sort_unstable_by(|a, b| a[..32].cmp(&b[..32]));
        write_sorted(&dir, &entries, 1000).unwrap();
        let sorted = Sorted::open(&dir).unwrap();
        for i in 0u32..1000 {
            let key = *blake3::hash(&i.to_le_bytes()).as_bytes();
            assert_eq!(sorted.find(&key).map(|l| l.pack), Some(i));
        }
        assert!(sorted.find(blake3::hash(b"absent").as_bytes()).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The set of `hashes`, as `Collector::collect` takes the pages to keep.
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
        let sorted = fs::metadata(dir.join(SORTED)).map_or(0, |m| m.len());
        packs + index + sorted
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
