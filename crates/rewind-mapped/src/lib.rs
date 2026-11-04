//! A file mapped read-only into memory.
//!
//! The page store searches the sorted copy of its index where the file
//! lies, several entries for each page, and reading each with a system
//! call was 3.3 million of them to restore a 3 GB guest. Mapping the file
//! makes each read a memory read. The store forbids unsafe code, so the
//! mapping lives here, on its own.
//!
//! A mapping is sound only while nothing writes the file or cuts it
//! short: a read past a shortened file's end raises SIGBUS. The store
//! writes each sorted copy under a name of its own and renames it into
//! place, so a copy is never changed once it has its name, and a mapping
//! of the one replaced keeps its contents.

use std::fs::File;
use std::os::fd::AsRawFd;

/// The first `len` bytes of a file, mapped read-only, until this is
/// dropped.
pub struct Mapped {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is read-only, so threads only ever read it.
unsafe impl Send for Mapped {}
// SAFETY: as for Send: no thread writes the memory.
unsafe impl Sync for Mapped {}

impl std::fmt::Debug for Mapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mapped").field("len", &self.len).finish()
    }
}

impl Mapped {
    /// The first `len` bytes of `file`, mapped read-only. The file must
    /// be at least `len` bytes long and, while the mapping is kept, never
    /// written or cut short; one that is replaced is renamed over. Fails
    /// for `len` 0, which mmap refuses.
    pub fn file(file: &File, len: usize) -> std::io::Result<Mapped> {
        if len == 0 {
            return Err(std::io::Error::other("an empty file cannot be mapped"));
        }
        if file.metadata()?.len() < len as u64 {
            return Err(std::io::Error::other(
                "the file is shorter than the mapping",
            ));
        }
        // SAFETY: a fresh read-only mapping of a file this process holds
        // open, no longer than the file; the caller keeps the file as it is
        // while the mapping lives.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let ptr = std::ptr::NonNull::new(ptr.cast())
            .ok_or_else(|| std::io::Error::other("mmap returned a null mapping"))?;
        Ok(Mapped { ptr, len })
    }

    /// The mapped bytes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is live, `len` bytes long, and read-only.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping `file` made, unmapped once, with no slice of
        // it outliving `self`.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    // Mappings of temporary files: their bytes, refusals, and a mapping
    // kept across the file being replaced by a rename, as the store
    // replaces its sorted copy.
    use super::*;

    /// A fresh directory for one test.
    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-mapped-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mapping reads the file's first `len` bytes, and keeps them after
    /// another file is renamed over the name.
    #[test]
    fn a_mapping_keeps_the_file_it_was_made_of() {
        let dir = dir("keeps");
        let path = dir.join("sorted");
        std::fs::write(&path, b"abcdef").unwrap();
        let mapped = Mapped::file(&File::open(&path).unwrap(), 4).unwrap();
        assert_eq!(mapped.bytes(), b"abcd");

        let new = dir.join("sorted.new");
        std::fs::write(&new, b"zzzzzz").unwrap();
        std::fs::rename(&new, &path).unwrap();
        assert_eq!(mapped.bytes(), b"abcd");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An empty mapping, or one longer than the file, is refused rather
    /// than made.
    #[test]
    fn a_mapping_past_the_file_is_refused() {
        let dir = dir("refused");
        let path = dir.join("short");
        std::fs::write(&path, b"ab").unwrap();
        let file = File::open(&path).unwrap();
        assert!(Mapped::file(&file, 0).is_err());
        assert!(Mapped::file(&file, 3).is_err());
        assert!(Mapped::file(&file, 2).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
