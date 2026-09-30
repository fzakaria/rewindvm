//! Guest memory: anonymous RAM, and the read-only input image.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};

use crate::layout::PAGE_SIZE;

/// A host mapping that backs a range of guest physical memory.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

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
            ptr: NonNull::new(ptr.cast()).unwrap(),
            len,
        })
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
        Ok(())
    }

    fn range(&self, offset: u64, len: usize) -> Result<std::ops::Range<usize>> {
        let start = usize::try_from(offset)?;
        let end = start.checked_add(len).context("guest address overflow")?;
        if end > self.len {
            bail!("guest range {start:#x}..{end:#x} is outside RAM");
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
