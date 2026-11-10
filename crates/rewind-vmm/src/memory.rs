//! Guest memory: anonymous RAM, and the read-only input image.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};

use crate::layout::PAGE_SIZE;

/// Whether the host took the advice to back a mapping with huge pages.
#[derive(Debug)]
pub enum HugePages {
    Advised,
    /// The host refused it, for this reason; the mapping works as before.
    Refused(std::io::Error),
}

/// What madvise's return `advised`, with `error` the errno it left, says
/// of the advice.
fn huge_pages(advised: libc::c_int, error: std::io::Error) -> HugePages {
    match advised {
        0 => HugePages::Advised,
        _ => HugePages::Refused(error),
    }
}

/// A host mapping that backs a range of guest physical memory.
#[derive(Debug)]
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    /// One bit for each page, set once anything has written the page
    /// since the mapping was made: the monitor through [`Mapping::write`],
    /// or whatever the caller of [`Mapping::mark`] names, the guest as
    /// KVM's dirty log reports it among them. A fresh mapping is zero, so
    /// a page with no bit set is zero still, and a full keyframe lists
    /// only the pages with a bit set rather than hashing all of memory.
    written: Vec<u64>,
}

/// The bits of one word of [`Mapping::written`].
const WORD_BITS: usize = u64::BITS as usize;

// SAFETY: the mapping is plain memory owned by this struct; the vCPU thread
// and the monitor thread never touch it at the same time, because the
// monitor only reads or writes guest memory while the vCPU is stopped in an
// exit.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Zeroed anonymous memory. MAP_NORESERVE so a large guest costs only
    /// what it touches.
    pub fn anonymous(len: usize) -> Result<Self> {
        // SAFETY: a fresh anonymous mapping aliases nothing.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!(
                "mmap of {len} bytes failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Mapping {
            ptr: NonNull::new(ptr.cast()).expect("mmap returns null only with MAP_FIXED"),
            len,
            written: vec![0; len.div_ceil(PAGE_SIZE).div_ceil(WORD_BITS)],
        })
    }

    /// Notes that the pages holding `len` bytes at `offset` were written,
    /// for a writer that goes through [`Mapping::as_mut_slice`].
    pub fn mark(&mut self, offset: u64, len: usize) {
        if len == 0 {
            return;
        }
        let Ok(start) = usize::try_from(offset) else {
            return;
        };
        let first = start / PAGE_SIZE;
        let last = start.saturating_add(len - 1) / PAGE_SIZE;
        for page in first..=last.min(self.len.saturating_sub(1) / PAGE_SIZE) {
            self.written[page / WORD_BITS] |= 1 << (page % WORD_BITS);
        }
    }

    /// Notes that page `page` was written, as [`Mapping::mark`] does.
    pub fn mark_page(&mut self, page: usize) {
        self.mark(page as u64 * PAGE_SIZE as u64, PAGE_SIZE);
    }

    /// The pages written since the mapping was made, in order, each once.
    /// Every other page is zero.
    pub fn written_pages(&self) -> Vec<usize> {
        let pages = self.len / PAGE_SIZE;
        let mut written = Vec::new();
        for (word, bits) in self.written.iter().enumerate() {
            let mut bits = *bits;
            while bits != 0 {
                let page = word * WORD_BITS + bits.trailing_zeros() as usize;
                if page < pages {
                    written.push(page);
                }
                bits &= bits - 1;
            }
        }
        written
    }

    /// A file mapped read-only into a zeroed region rounded up to `align`.
    ///
    /// The file alone is not mapped because the guest may read the tail of
    /// the region past the end of the file, and a mapping past end of file
    /// raises SIGBUS in the monitor. The anonymous region underneath reads
    /// as zeros there instead.
    pub fn file(path: &Path, align: usize) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let file_len = file.metadata()?.len() as usize;
        if file_len == 0 {
            bail!("{} is empty", path.display());
        }
        let len = file_len.div_ceil(align) * align;
        let region = Self::anonymous(len)?;

        // SAFETY: MAP_FIXED over the start of our own region, which nothing
        // else refers to yet.
        let ptr = unsafe {
            libc::mmap(
                region.ptr.as_ptr().cast(),
                file_len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!(
                "mmap of {} failed: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(region)
    }

    /// Asks the host to back the mapping with 2 MiB pages where it can,
    /// so filling or freeing it takes one page fault or free per 2 MiB
    /// rather than per 4 KiB page. The advice is optional: a host without
    /// transparent huge pages, or a sandbox that refuses the advice,
    /// leaves the mapping as it was, in 4 KiB pages.
    pub fn prefer_huge_pages(&self) -> HugePages {
        // SAFETY: advice on our own live mapping changes no contents.
        let advised =
            unsafe { libc::madvise(self.ptr.as_ptr().cast(), self.len, libc::MADV_HUGEPAGE) };
        huge_pages(advised, std::io::Error::last_os_error())
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn host_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is live and `len` bytes long.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is live, `len` bytes long, and we hold it
        // exclusively.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Reads guest memory at `offset`, failing rather than reading past the
    /// end: offsets come from the guest.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let range = self.range(offset, buf.len())?;
        buf.copy_from_slice(&self.as_slice()[range]);
        Ok(())
    }

    pub fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let range = self.range(offset, buf.len())?;
        self.as_mut_slice()[range].copy_from_slice(buf);
        self.mark(offset, buf.len());
        Ok(())
    }

    fn range(&self, offset: u64, len: usize) -> Result<std::ops::Range<usize>> {
        let start = usize::try_from(offset)?;
        let end = start.checked_add(len).context("VM address overflow")?;
        if end > self.len {
            bail!("VM range {start:#x}..{end:#x} is outside its RAM");
        }
        Ok(start..end)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: we own the mapping and nothing refers to it any more.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Huge pages are advice: a host that refuses MADV_HUGEPAGE, as one
    /// without transparent huge pages does with EINVAL, leaves a mapping
    /// that reads and writes as one that took it.
    #[test]
    fn refused_huge_page_advice_leaves_a_working_mapping() {
        let refused = huge_pages(-1, std::io::Error::from_raw_os_error(libc::EINVAL));
        assert!(matches!(refused, HugePages::Refused(_)));
        assert!(matches!(
            huge_pages(0, std::io::Error::from_raw_os_error(0)),
            HugePages::Advised
        ));

        let mut ram = Mapping::anonymous(2 * PAGE_SIZE).unwrap();
        let _advice = ram.prefer_huge_pages();
        ram.write(PAGE_SIZE as u64, &[7]).unwrap();
        let mut byte = [0];
        ram.read(PAGE_SIZE as u64, &mut byte).unwrap();
        assert_eq!(byte, [7]);
    }

    /// A mapping lists the pages written to it: none at first, each page
    /// a write touches, a write across a page boundary both of its pages,
    /// a page marked by a writer that went around `write`, and each once
    /// however often it was written, in page order.
    #[test]
    fn a_mapping_lists_the_pages_written_to_it() {
        let mut ram = Mapping::anonymous(8 * PAGE_SIZE).unwrap();
        assert!(ram.written_pages().is_empty());
        ram.write(5 * PAGE_SIZE as u64, &[1]).unwrap();
        ram.write(2 * PAGE_SIZE as u64 - 1, &[1, 2]).unwrap();
        ram.write(5 * PAGE_SIZE as u64 + 100, &[3]).unwrap();
        ram.mark_page(7);
        ram.mark(0, 0);
        assert_eq!(ram.written_pages(), [1, 2, 5, 7]);

        // A mark past the end names no page past the end.
        ram.mark(7 * PAGE_SIZE as u64, 3 * PAGE_SIZE);
        assert_eq!(ram.written_pages(), [1, 2, 5, 7]);
    }
}
