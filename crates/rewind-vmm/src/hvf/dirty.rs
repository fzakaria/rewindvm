//! Dirty page tracking on Hypervisor.framework via stage-2 write protection.
//!
//! HVF does not expose a KVM-style dirty page log. Instead, when dirty logging
//! is enabled, guest RAM is mapped `HV_MEMORY_READ | HV_MEMORY_EXEC`. Each
//! guest write triggers a stage-2 data abort; we record the 16 KiB host page,
//! flip it to `RWX` with `hv_vm_protect`, and resume. On capture, only the
//! previously dirtied host pages are re-protected back to `RX` (coalesced into
//! contiguous runs) and expanded into 4 KiB guest page indices.

use anyhow::Result;

use crate::hvf::HvfVm;
use crate::hvf::layout::{HOST_PAGE_SIZE, PAGE_SIZE, RAM_BASE};
use crate::hvf::sys::{HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE};

const GUEST_PAGES_PER_HOST: usize = HOST_PAGE_SIZE / PAGE_SIZE;
const WORD_BITS: usize = u64::BITS as usize;

/// Bitmap of 16 KiB host pages written by the guest since the last drain.
#[derive(Debug)]
pub(crate) struct DirtyTracker {
    host_pages: usize,
    bits: Vec<u64>,
    enabled: bool,
}

impl DirtyTracker {
    pub(crate) fn new(ram_len: usize, enabled: bool) -> Self {
        let host_pages = ram_len.div_ceil(HOST_PAGE_SIZE);
        DirtyTracker {
            host_pages,
            bits: vec![0; host_pages.div_ceil(WORD_BITS)],
            enabled,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Write-protects all of guest RAM and clears the dirty bitmap.
    pub(crate) fn arm_all(&mut self, vm: &HvfVm) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.bits.fill(0);
        vm.protect(
            RAM_BASE,
            self.host_pages * HOST_PAGE_SIZE,
            HV_MEMORY_READ | HV_MEMORY_EXEC,
        )
    }

    /// Handles a stage-2 write fault at guest physical address `ipa`.
    /// Returns `Ok(true)` if `ipa` lies in RAM and was unprotected for retry.
    pub(crate) fn handle_write_fault(&mut self, vm: &HvfVm, ipa: u64) -> Result<bool> {
        if !self.enabled || ipa < RAM_BASE {
            return Ok(false);
        }
        let offset = (ipa - RAM_BASE) as usize;
        let host_page = offset / HOST_PAGE_SIZE;
        if host_page >= self.host_pages {
            return Ok(false);
        }
        self.bits[host_page / WORD_BITS] |= 1 << (host_page % WORD_BITS);
        let page_ipa = RAM_BASE + (host_page * HOST_PAGE_SIZE) as u64;
        vm.protect(
            page_ipa,
            HOST_PAGE_SIZE,
            HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
        )?;
        Ok(true)
    }

    /// Drains all dirtied pages (as 4 KiB guest page numbers relative to
    /// `RAM_BASE`), re-protects those host pages back to `RX`, and clears the
    /// bitmap.
    pub(crate) fn drain(&mut self, vm: &HvfVm, ram_len: usize) -> Result<Vec<usize>> {
        let max_guest_pages = ram_len / PAGE_SIZE;
        let mut dirty_hosts = Vec::new();
        for (word_idx, word) in self.bits.iter_mut().enumerate() {
            let mut bits = *word;
            if bits == 0 {
                continue;
            }
            *word = 0;
            while bits != 0 {
                let h = word_idx * WORD_BITS + bits.trailing_zeros() as usize;
                if h < self.host_pages {
                    dirty_hosts.push(h);
                }
                bits &= bits - 1;
            }
        }

        // Re-protect contiguous runs of host pages to minimize syscalls.
        let mut i = 0;
        while i < dirty_hosts.len() {
            let start = dirty_hosts[i];
            let mut end = start + 1;
            while i + 1 < dirty_hosts.len() && dirty_hosts[i + 1] == end {
                i += 1;
                end += 1;
            }
            let ipa = RAM_BASE + (start * HOST_PAGE_SIZE) as u64;
            let len = (end - start) * HOST_PAGE_SIZE;
            vm.protect(ipa, len, HV_MEMORY_READ | HV_MEMORY_EXEC)?;
            i += 1;
        }

        let mut guest_pages = Vec::with_capacity(dirty_hosts.len() * GUEST_PAGES_PER_HOST);
        for h in dirty_hosts {
            let first = h * GUEST_PAGES_PER_HOST;
            let last = (first + GUEST_PAGES_PER_HOST).min(max_guest_pages);
            guest_pages.extend(first..last);
        }
        Ok(guest_pages)
    }
}
