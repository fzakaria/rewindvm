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

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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

pub struct Store {
    dir: PathBuf,
    index: HashMap<Hash, Location>,
    index_log: File,
    pack: File,
    pack_id: u32,
    pack_len: u64,
    readers: Mutex<HashMap<u32, File>>,
    /// Holds the store's lock for as long as the store is open.
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

        // One writer at a time: two processes appending to the same pack
        // would interleave their pages.
        let lock = File::create(dir.join("lock"))?;
        // SAFETY: flock on a file we own.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
            bail!(
                "locking {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            );
        }

        let index_path = dir.join("index");
        let mut bytes = Vec::new();
        if let Ok(mut f) = File::open(&index_path) {
            f.read_to_end(&mut bytes)?;
        }
        let whole = bytes.len() / INDEX_ENTRY * INDEX_ENTRY;
        let mut index = HashMap::with_capacity(whole / INDEX_ENTRY);
        for entry in bytes[..whole].chunks_exact(INDEX_ENTRY) {
            let hash: Hash = entry[..32].try_into().unwrap();
            let pack = u32::from_le_bytes(entry[32..36].try_into().unwrap());
            let offset = u64::from_le_bytes(entry[36..44].try_into().unwrap());
            let len = u32::from_le_bytes(entry[44..48].try_into().unwrap());
            index.insert(hash, Location { pack, offset, len });
        }
        let index_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?;
        if whole != bytes.len() {
            // A partial entry from an interrupted write.
            index_log.set_len(whole as u64)?;
        }

        let pack_id = index.values().map(|l| l.pack).max().unwrap_or(0);
        let (pack, pack_len) = Self::open_pack(dir, pack_id)?;
        let mut store = Store {
            dir: dir.to_path_buf(),
            index,
            index_log,
            pack,
            pack_id,
            pack_len,
            readers: Mutex::new(HashMap::new()),
            _lock: lock,
        };
        if store.pack_len >= PACK_MAX {
            store.roll()?;
        }
        Ok(store)
    }

    fn pack_path(dir: &Path, id: u32) -> PathBuf {
        dir.join("packs").join(format!("{id:08}.pack"))
    }

    fn open_pack(dir: &Path, id: u32) -> Result<(File, u64)> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(Self::pack_path(dir, id))?;
        let len = f.metadata()?.len();
        Ok((f, len))
    }

    fn roll(&mut self) -> Result<()> {
        self.pack_id += 1;
        let (pack, len) = Self::open_pack(&self.dir, self.pack_id)?;
        self.pack = pack;
        self.pack_len = len;
        Ok(())
    }

    pub fn hash(data: &[u8]) -> Hash {
        if data.iter().all(|b| *b == 0) {
            return ZERO_PAGE;
        }
        *blake3::hash(data).as_bytes()
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        *hash == ZERO_PAGE || self.index.contains_key(hash)
    }

    /// Stores a page if it is new, and returns its hash either way.
    pub fn put(&mut self, data: &[u8]) -> Result<Hash> {
        let hash = Self::hash(data);
        if self.contains(&hash) {
            return Ok(hash);
        }
        let compressed = zstd::bulk::compress(data, COMPRESSION_LEVEL)?;
        if self.pack_len + compressed.len() as u64 > PACK_MAX {
            self.roll()?;
        }
        let loc = Location {
            pack: self.pack_id,
            offset: self.pack_len,
            len: compressed.len() as u32,
        };
        self.pack.write_all(&compressed)?;
        self.pack_len += compressed.len() as u64;

        let mut entry = Vec::with_capacity(INDEX_ENTRY);
        entry.extend_from_slice(&hash);
        entry.extend_from_slice(&loc.pack.to_le_bytes());
        entry.extend_from_slice(&loc.offset.to_le_bytes());
        entry.extend_from_slice(&loc.len.to_le_bytes());
        self.index_log.write_all(&entry)?;
        self.index.insert(hash, loc);
        Ok(hash)
    }

    /// Reads a page back into `out`, which must be the page's size.
    pub fn get(&self, hash: &Hash, out: &mut [u8]) -> Result<()> {
        if *hash == ZERO_PAGE {
            out.fill(0);
            return Ok(());
        }
        let loc = *self
            .index
            .get(hash)
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
        self.pack.sync_data()?;
        self.index_log.sync_data()?;
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        Stats {
            pages: self.index.len(),
            stored_bytes: self.index.values().map(|l| l.len as u64).sum(),
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
}
