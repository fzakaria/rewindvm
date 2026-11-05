//! Comparing a replay's whole machine with the recording at the run's
//! keyframes, rather than only the events the two make.
//!
//! An event can only differ at an exit, so a replay that makes the
//! recording's events can still be another machine: a value in memory or
//! a register that nothing has printed yet, or ever will. Keyframes hold
//! the rest. At each step the run has a keyframe at, the replay captures
//! its machine the way recording does, hashing the pages written since the
//! last capture without storing them, and compares page hashes, registers
//! and devices with the keyframe's. The first keyframe that differs, and
//! the last one before it that matched, bracket where the machine changed.
//!
//! The pages that differ are named where the replayed machine maps them:
//! the processes whose memory holds them, at which address, or the
//! kernel's image and the symbol there.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};
use rewind_store::Store;
use rewind_vmm::Machine;
use rewind_vmm::snapshot::{Keyframe, Pages, ZERO_PAGE};
use rewind_vmm::state::StateDifference;
use serde::Serialize;

use crate::keyframes::{Layers, Memory};

/// A page store that only hashes: a replay compares its pages' hashes with
/// the recording's, and keeps none of them.
#[derive(Debug)]
pub struct HashPages;

impl Pages for HashPages {
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        Ok(Store::hash_all(pages))
    }
    fn get(&self, _hash: &[u8; 32], _out: &mut [u8]) -> Result<()> {
        bail!("a replay's captured pages are hashes only")
    }
}

const PAGE_SIZE: usize = 4096;

/// How many differing pages are named and shown; the rest are counted.
pub const MAX_SHOWN_PAGES: usize = 16;

/// How many runs of differing bytes a page shows.
const MAX_BYTE_RUNS: usize = 4;

/// The most bytes a run of differing bytes shows.
const MAX_RUN_BYTES: usize = 16;

/// Differing bytes this close together are shown as one run.
const RUN_GAP: usize = 8;

/// Where the kernel's image is in its virtual memory: physical address
/// plus this, as x86-64 Linux maps it.
const START_KERNEL_MAP: u64 = 0xffff_ffff_8000_0000;

/// Where a page of guest memory is, in the replayed machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Place {
    /// In a process's memory, at this virtual address.
    Process {
        pid: u32,
        name: String,
        address: u64,
    },
    /// In the kernel's image, at this virtual address, and the symbol
    /// there when the kernel's symbol table has one.
    KernelImage {
        address: u64,
        symbol: Option<String>,
    },
    /// Elsewhere in the kernel's memory: page cache, slab, page tables.
    Kernel,
}

/// A stretch of bytes that differ within a page: where it starts, and the
/// bytes on each side. The recording's are absent when the page store no
/// longer holds its page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bytes {
    pub offset: usize,
    pub recorded: Option<Vec<u8>>,
    pub replayed: Vec<u8>,
}

/// A page whose contents differ at a keyframe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PageDifference {
    /// The guest page number: its physical address over the page size.
    pub page: u32,
    pub places: Vec<Place>,
    pub bytes: Vec<Bytes>,
}

/// Where a replay's machine first differs from the recording's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MachineDifference {
    /// The step of the first keyframe that differs.
    pub step: u64,
    /// The step of the last keyframe before it that matched, or None when
    /// the replay started at boot and no keyframe had matched yet.
    pub matched: Option<u64>,
    /// Up to [`MAX_SHOWN_PAGES`] differing pages, and how many differ.
    pub pages: Vec<PageDifference>,
    pub pages_differing: usize,
    /// Registers and other named state that differ, recorded on the left.
    pub state: Vec<StateDifference>,
}

/// What comparing a replay's machine at the run's keyframes found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Compared {
    /// The steps of the keyframes compared, in order.
    pub compared: Vec<u64>,
    /// The first that differed, if one did.
    pub first: Option<MachineDifference>,
}

/// Watches a replay's machine at each keyframe of the run it replays.
#[derive(Debug)]
pub struct Watch<'a> {
    layers: &'a Layers,
    store: Option<&'a Store>,
    /// The System.map of the run's kernel, which names kernel addresses.
    system_map: Option<String>,
    /// The keyframe the replay last captured, as recording names a
    /// keyframe's parent, and memory as the replay's captures leave it.
    parent: Option<u64>,
    replayed: Memory,
    /// The recording's memory at `recorded_at`, built from the keyframes
    /// read so far, each read once.
    recorded: Memory,
    recorded_at: Option<u64>,
    /// The pages either memory has written since the two were last
    /// compared and found alike, the only ones that can differ now.
    written: Written,
    loaded: HashMap<u64, Keyframe>,
    compared: Compared,
}

/// Which pages two memories compared at a keyframe can differ on.
#[derive(Debug)]
enum Written {
    /// Any page: the two have not been compared yet, or one was rebuilt
    /// from a full keyframe since.
    Any,
    /// Only these, the pages either wrote since they were last alike.
    Pages(Vec<u32>),
}

impl Written {
    /// Notes that a memory wrote `pages`.
    fn add(&mut self, pages: &[(u32, [u8; 32])]) {
        if let Written::Pages(written) = self {
            written.extend(pages.iter().map(|(page, _)| *page));
        }
    }
}

impl<'a> Watch<'a> {
    /// A watch over a replay started from the keyframe at `start`, or from
    /// boot. Pages the recording no longer has in `store` show without
    /// their recorded bytes.
    pub fn new(
        layers: &'a Layers,
        store: Option<&'a Store>,
        system_map: Option<String>,
        start: Option<u64>,
    ) -> Result<Watch<'a>> {
        let mut watch = Watch {
            layers,
            store,
            system_map,
            parent: start,
            replayed: Memory::default(),
            recorded: Memory::default(),
            recorded_at: None,
            written: Written::Any,
            loaded: HashMap::new(),
            compared: Compared::default(),
        };

        // A replay restored at a keyframe starts with that keyframe's
        // memory, which is the recording's there.
        if let Some(step) = start {
            watch.recorded_memory(step)?;
            for kf in watch.chain(step)? {
                let kf = &watch.loaded[&kf];
                watch.replayed.apply(kf.parent, &kf.pages);
            }
        }
        Ok(watch)
    }

    /// Captures `machine` at the keyframe step it has reached, as recording
    /// would, and compares it with the recorded keyframe there, unless an
    /// earlier keyframe differed already. The capture changes nothing the
    /// guest can see, so the replay goes on as it would have.
    pub fn at_keyframe(&mut self, machine: &mut Machine) -> Result<()> {
        let step = machine.step();
        let mut captured = machine.keyframe(&mut HashPages, self.parent)?;
        self.replayed.apply(captured.parent, &captured.pages);
        self.parent = Some(step);
        if self.compared.first.is_some() {
            return Ok(());
        }
        self.written.add(&captured.pages);
        captured.pages.clear();

        // The recording's memory and keyframe at the same step, compared
        // where either memory wrote since the two were last alike.
        self.recorded_memory(step)?;
        let recorded = &self.loaded[&step];
        let state = recorded.state_differences(&captured)?;
        let pages = match std::mem::replace(&mut self.written, Written::Pages(Vec::new())) {
            Written::Any => self.recorded.differing(&self.replayed),
            Written::Pages(written) => self.recorded.differing_among(&self.replayed, &written),
        };
        let matched = self.compared.compared.last().copied();
        self.compared.compared.push(step);
        if pages.is_empty() && state.is_empty() {
            return Ok(());
        }

        let shown: Vec<(u32, [u8; 32])> = pages
            .iter()
            .take(MAX_SHOWN_PAGES)
            .map(|(page, recorded, _)| (*page, *recorded))
            .collect();
        let pages_differing = pages.len();
        let pages = self.describe(machine, &shown)?;
        self.compared.first = Some(MachineDifference {
            step,
            matched,
            pages,
            pages_differing,
            state,
        });
        Ok(())
    }

    /// What the watch found.
    pub fn finish(self) -> Compared {
        self.compared
    }

    /// Each page of `pages`, with the hash the recording gives it, named
    /// where the replayed machine has it, with the bytes that differ.
    fn describe(
        &self,
        machine: &Machine,
        pages: &[(u32, [u8; 32])],
    ) -> Result<Vec<PageDifference>> {
        let frames: BTreeSet<u64> = pages
            .iter()
            .map(|(page, _)| *page as u64 * PAGE_SIZE as u64)
            .collect();
        let mut places: HashMap<u64, Vec<Place>> = HashMap::new();
        for (frame, place) in process_places(machine, &frames) {
            places.entry(frame).or_default().push(place);
        }

        let mut described = Vec::new();
        for (page, recorded_hash) in pages {
            let frame = *page as u64 * PAGE_SIZE as u64;
            let mut replayed = vec![0u8; PAGE_SIZE];
            machine.read_physical(frame, &mut replayed)?;
            let recorded = self.recorded_page(recorded_hash);
            let here = places
                .remove(&frame)
                .unwrap_or_else(|| vec![self.kernel_place(frame)]);
            described.push(PageDifference {
                page: *page,
                places: here,
                bytes: byte_runs(recorded.as_deref(), &replayed),
            });
        }
        Ok(described)
    }

    /// A page's recorded bytes, when the store has them.
    fn recorded_page(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        if *hash == ZERO_PAGE {
            return Some(vec![0; PAGE_SIZE]);
        }
        let store = self.store?;
        let mut page = vec![0u8; PAGE_SIZE];
        store.get(hash, &mut page).ok()?;
        Some(page)
    }

    /// Where a frame no process maps is in the kernel: in its image, at
    /// the symbol there, or elsewhere.
    fn kernel_place(&self, frame: u64) -> Place {
        let Some(map) = &self.system_map else {
            return Place::Kernel;
        };
        let address = frame + START_KERNEL_MAP;
        let bound = |name: &str| crate::run::kernel_symbol_address(map, name);
        match (bound("_text"), bound("_end")) {
            (Some(start), Some(end)) if (start..end).contains(&address) => Place::KernelImage {
                address,
                symbol: crate::run::kernel_symbol(map, address),
            },
            _ => Place::Kernel,
        }
    }

    /// Brings `recorded` to the recording's memory at the keyframe at
    /// `step`: from the memory it holds when that is the keyframe's
    /// parent, else from the chain's full keyframe.
    fn recorded_memory(&mut self, step: u64) -> Result<()> {
        let chain = self.chain(step)?;
        let parent = self.loaded[&step].parent;
        let from = match (parent, self.recorded_at) {
            (Some(p), Some(at)) if p == at => chain.len() - 1,
            _ => {
                self.recorded = Memory::default();
                self.written = Written::Any;
                0
            }
        };
        for kf in &chain[from..] {
            let kf = &self.loaded[kf];
            self.recorded.apply(kf.parent, &kf.pages);
            self.written.add(&kf.pages);
        }
        self.recorded_at = Some(step);
        Ok(())
    }

    /// The steps of the chain of keyframes that rebuilds the one at
    /// `step`, full one first, each loaded once.
    fn chain(&mut self, step: u64) -> Result<Vec<u64>> {
        let mut steps = vec![step];
        let mut at = step;
        loop {
            if !self.loaded.contains_key(&at) {
                let kf = self.layers.load(at)?;
                self.loaded.insert(at, kf);
            }
            let Some(parent) = self.loaded[&at].parent else {
                break;
            };
            if parent >= at {
                bail!("the keyframe at step {at} names step {parent} as its parent");
            }
            steps.push(parent);
            at = parent;
        }
        steps.reverse();
        Ok(steps)
    }
}

/// The processes that map each of `frames`, physical page addresses, in
/// `machine`, and where: every process the kernel lists, by its page
/// table. None before the kernel has said where its tasks are.
fn process_places(machine: &Machine, frames: &BTreeSet<u64>) -> Vec<(u64, Place)> {
    let Ok(Some(layout)) = machine.task_layout() else {
        return Vec::new();
    };
    let tasks = crate::threads::Tasks::new(machine, layout);
    let Ok(processes) = tasks.processes() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for task in processes {
        let Ok(Some(page_table)) = tasks.page_table(task) else {
            continue;
        };
        let (Ok(pid), Ok(name)) = (tasks.tgid(task), tasks.name(task)) else {
            continue;
        };
        let Ok(mapped) = machine.mappings_in(page_table, frames) else {
            continue;
        };
        for (frame, address) in mapped {
            found.push((
                frame,
                Place::Process {
                    pid,
                    name: name.clone(),
                    address,
                },
            ));
        }
    }
    found
}

/// The stretches of bytes that differ between a page as recorded, when
/// known, and as replayed: up to [`MAX_BYTE_RUNS`], each up to
/// [`MAX_RUN_BYTES`] long, differing bytes within [`RUN_GAP`] of each
/// other joined. Without the recorded page, the page's first bytes.
fn byte_runs(recorded: Option<&[u8]>, replayed: &[u8]) -> Vec<Bytes> {
    let Some(recorded) = recorded else {
        return vec![Bytes {
            offset: 0,
            recorded: None,
            replayed: replayed[..MAX_RUN_BYTES].to_vec(),
        }];
    };
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for i in (0..replayed.len()).filter(|&i| recorded[i] != replayed[i]) {
        match runs.last_mut() {
            Some((_, end)) if i <= *end + RUN_GAP => *end = i,
            _ => runs.push((i, i)),
        }
    }
    runs.into_iter()
        .take(MAX_BYTE_RUNS)
        .map(|(start, end)| {
            let end = (end + 1).min(start + MAX_RUN_BYTES);
            Bytes {
                offset: start,
                recorded: Some(recorded[start..end].to_vec()),
                replayed: replayed[start..end].to_vec(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // The pieces of a machine comparison that need no VM: which pages two
    // memories disagree on, and which bytes of a page differ.
    use super::*;

    #[test]
    fn memories_differ_where_a_page_hash_differs_or_one_side_has_none() {
        // Page 1 has other contents on each side, page 2 only on the left,
        // and page 3 is the same on both: pages 1 and 2 differ, page 2
        // with the zero page's hash on the right.
        let mut left = Memory::default();
        left.apply(None, &[(1, [1; 32]), (2, [2; 32]), (3, [3; 32])]);
        let mut right = Memory::default();
        right.apply(None, &[(1, [9; 32]), (3, [3; 32])]);
        assert_eq!(
            left.differing(&right),
            vec![(1, [1; 32], [9; 32]), (2, [2; 32], ZERO_PAGE)]
        );
        assert!(left.differing(&left).is_empty());
    }

    #[test]
    fn memories_that_matched_differ_only_among_the_pages_written_since() {
        // Two memories alike, then the left writes pages 5 and 7 and the
        // right pages 6 and 7, both 7 with the same contents. Among the
        // pages written, 5 and 6 differ, which is what comparing every page
        // finds; among a list without them, none do; and the list may name
        // a page twice and out of order.
        let mut left = Memory::default();
        left.apply(None, &[(1, [1; 32]), (2, [2; 32])]);
        let mut right = Memory::default();
        right.apply(None, &[(1, [1; 32]), (2, [2; 32])]);
        left.apply(Some(1), &[(5, [5; 32]), (7, [7; 32])]);
        right.apply(Some(1), &[(6, [6; 32]), (7, [7; 32])]);
        let differing = vec![(5, [5; 32], ZERO_PAGE), (6, ZERO_PAGE, [6; 32])];
        assert_eq!(left.differing_among(&right, &[7, 6, 5, 7]), differing);
        assert_eq!(left.differing(&right), differing);
        assert!(left.differing_among(&right, &[1, 2, 7]).is_empty());
    }

    #[test]
    fn differing_bytes_are_shown_in_runs() {
        // Bytes 0x170 to 0x177 differ, and byte 0x300: two runs, the
        // first eight bytes long. Without the recorded page, the page's
        // first bytes show as replayed.
        let recorded = vec![0u8; PAGE_SIZE];
        let mut replayed = recorded.clone();
        for b in &mut replayed[0x170..0x178] {
            *b = 0xaa;
        }
        replayed[0x300] = 1;
        let runs = byte_runs(Some(&recorded), &replayed);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].offset, 0x170);
        assert_eq!(runs[0].replayed, vec![0xaa; 8]);
        assert_eq!(runs[0].recorded, Some(vec![0; 8]));
        assert_eq!(runs[1].offset, 0x300);
        let unknown = byte_runs(None, &replayed);
        assert_eq!(unknown[0].recorded, None);
        assert_eq!(unknown[0].replayed.len(), MAX_RUN_BYTES);
    }
}
