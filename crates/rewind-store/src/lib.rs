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

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

pub type Hash = [u8; 32];

/// The name the all-zero page goes by; it is never written.
pub const ZERO_PAGE: Hash = [0; 32];

/// The size of one index entry: hash, pack number, offset, length.
const INDEX_ENTRY: usize = 32 + 4 + 8 + 4;

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

/// Hashes a page's key for the index's map by taking its first eight
/// bytes. The keys are BLAKE3 output, already uniform, and a store holds
/// millions of them: hashing each again with the standard hasher made
/// opening a large store take seconds.
#[derive(Default)]
struct PageHasher(u64);

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

type PageHashes = BuildHasherDefault<PageHasher>;

/// The index as far as this store has read it.
struct Index {
    pages: HashMap<Hash, Location, PageHashes>,
    /// How many bytes of the index file `pages` holds, a whole number of
    /// entries.
    read: u64,
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
        fs::create_dir_all(dir.join("packs"))?;
        let index_path = dir.join("index");
        let index_log = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&index_path)?;

        // A partial last index entry can only be a crash's, and only an
        // opener with the store to itself may cut it off: with others open
        // it could be an entry still being written.
        let lock = File::create(dir.join("lock"))?;
        if lock.try_lock().is_ok() {
            let len = index_log.metadata()?.len();
            let whole = len / INDEX_ENTRY as u64 * INDEX_ENTRY as u64;
            if whole != len {
                index_log.set_len(whole)?;
            }
        }
        lock.lock_shared()
            .with_context(|| format!("locking {}", dir.display()))?;

        let store = Store {
            dir: dir.to_path_buf(),
            index: Mutex::new(Index {
                pages: HashMap::default(),
                read: 0,
            }),
            index_log,
            pack: None,
            readers: Mutex::new(HashMap::new()),
            _lock: lock,
        };
        store.refresh()?;
        Ok(store)
    }

    /// Reads the index entries written since this store last looked,
    /// by it or by any other.
    fn refresh(&self) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        let mut file = File::open(self.dir.join("index"))?;
        file.seek(SeekFrom::Start(index.read))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (entries, _) = bytes.as_chunks::<INDEX_ENTRY>();
        index.pages.reserve(entries.len());
        for entry in entries {
            let hash: Hash = entry[..32].try_into().unwrap();
            let pack = u32::from_le_bytes(entry[32..36].try_into().unwrap());
            let offset = u64::from_le_bytes(entry[36..44].try_into().unwrap());
            let len = u32::from_le_bytes(entry[44..48].try_into().unwrap());
            index.pages.insert(hash, Location { pack, offset, len });
        }
        index.read += (entries.len() * INDEX_ENTRY) as u64;
        Ok(())
    }

    fn pack_path(dir: &Path, id: u32) -> PathBuf {
        dir.join("packs").join(format!("{id:08}.pack"))
    }

    /// A pack to append to that no other writer holds: an existing one with
    /// room, or else a new one.
    fn claim_pack(&self) -> Result<Pack> {
        let mut ids: Vec<u32> = fs::read_dir(self.dir.join("packs"))?
            .filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_suffix(".pack")?
                    .parse()
                    .ok()
            })
            .collect();
        ids.sort_unstable();
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
        let known = self.index.lock().unwrap().pages.get(hash).copied();
        if known.is_some() {
            return known;
        }
        self.refresh().ok()?;
        self.index.lock().unwrap().pages.get(hash).copied()
    }

    /// Stores a page if it is new, and returns its hash either way. New
    /// means new to this store: it does not reread the index for every
    /// page, so two stores may both write a page another wrote meanwhile,
    /// and either copy serves.
    pub fn put(&mut self, data: &[u8]) -> Result<Hash> {
        let hash = Self::hash(data);
        if hash == ZERO_PAGE || self.index.lock().unwrap().pages.contains_key(&hash) {
            return Ok(hash);
        }
        let compressed = zstd::bulk::compress(data, COMPRESSION_LEVEL)?;
        let full = self
            .pack
            .as_ref()
            .is_none_or(|p| p.len + compressed.len() as u64 > PACK_MAX);
        if full {
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
    pub fn stats(&self) -> Stats {
        let index = self.index.lock().unwrap();
        Stats {
            pages: index.pages.len(),
            stored_bytes: index.pages.values().map(|l| l.len as u64).sum(),
        }
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
