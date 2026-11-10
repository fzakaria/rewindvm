//! Checkpoints: a machine's state at steps it has passed, kept in memory,
//! so a debugger can rewind the machine to one of them in place.
//!
//! A fork from a keyframe builds a new VM and writes every page the
//! keyframes hold, which takes a good part of a second. Going back a few
//! instructions again and again, as a debugger running backwards does,
//! needs something cheaper. A checkpoint holds the vCPU, the interrupt
//! controllers and the monitor's own state, and only the pages written
//! since the checkpoint before it, its parent, which KVM's dirty log
//! names. The checkpoints form a tree whose root is the machine's origin:
//! the keyframes it was restored from, or boot.
//!
//! The machine's memory is always one checkpoint's, its anchor, with the
//! pages written since. Rewinding to another checkpoint writes back only
//! the pages that can differ: those written since the anchor, and those
//! either checkpoint's line of parents wrote below the checkpoint the two
//! lines share. Each page gets the contents the nearest checkpoint up the
//! target's line holds, else the origin's.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};

use crate::layout::PAGE_SIZE;
use crate::snapshot::{CpuState, DeviceState, Pages};
use crate::{Machine, Origin};

/// A checkpoint, by its place in the order they were taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckpointId(usize);

/// A guest page number.
type Page = u32;

/// The checkpoints' lines of parents and the pages each wrote, without
/// the pages' contents: which pages a rewind writes back, and where each
/// one's contents come from.
#[derive(Debug, Default)]
struct Tree {
    /// Each checkpoint's parent, None for the origin, by id.
    parents: Vec<Option<CheckpointId>>,
    /// The pages written between each checkpoint's parent and it, by id.
    written: Vec<BTreeSet<Page>>,
}

/// Where a page's contents come from on a rewind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The pages this checkpoint holds.
    Checkpoint(CheckpointId),
    /// The machine's origin.
    Origin,
}

impl Tree {
    /// Adds a checkpoint over `parent`, holding `written`.
    fn add(&mut self, parent: Option<CheckpointId>, written: BTreeSet<Page>) -> CheckpointId {
        self.parents.push(parent);
        self.written.push(written);
        CheckpointId(self.parents.len() - 1)
    }

    /// `id` and its line of parents, nearest first.
    fn line(&self, id: Option<CheckpointId>) -> Vec<CheckpointId> {
        let mut line = Vec::new();
        let mut at = id;
        while let Some(id) = at {
            line.push(id);
            at = self.parents[id.0];
        }
        line
    }

    /// The pages a rewind from `anchor`, with `since` written after it, to
    /// `target` writes back: those, and the pages each checkpoint wrote on
    /// either line of parents below the checkpoint the lines share.
    fn to_write_back(
        &self,
        anchor: Option<CheckpointId>,
        since: &BTreeSet<Page>,
        target: CheckpointId,
    ) -> BTreeSet<Page> {
        let from = self.line(anchor);
        let to = self.line(Some(target));
        let shared: BTreeSet<CheckpointId> =
            from.iter().filter(|id| to.contains(id)).copied().collect();
        let mut pages = since.clone();
        for id in from.iter().chain(&to).filter(|id| !shared.contains(id)) {
            pages.extend(&self.written[id.0]);
        }
        pages
    }

    /// Where page `page` of checkpoint `target` comes from: the nearest
    /// checkpoint up its line that wrote it, else the origin.
    fn source(&self, target: CheckpointId, page: Page) -> Source {
        self.line(Some(target))
            .into_iter()
            .find(|id| self.written[id.0].contains(&page))
            .map_or(Source::Origin, Source::Checkpoint)
    }
}

/// Pages' contents by page number, kept in one buffer: a checkpoint's
/// or the origin's. A checkpoint of a large guest keeps thousands of
/// pages, and freeing them one allocation each took 0.13 s at the end of
/// a command.
#[derive(Debug, Default)]
struct KeptPages {
    /// Where each page's contents start in `bytes`.
    at: HashMap<Page, usize>,
    bytes: Vec<u8>,
}

impl KeptPages {
    /// Room for `pages` pages without growing.
    fn with_capacity(pages: usize) -> KeptPages {
        KeptPages {
            at: HashMap::with_capacity(pages),
            bytes: Vec::with_capacity(pages * PAGE_SIZE),
        }
    }

    /// Keeps `contents` as page `page`'s, unless the page has some.
    fn insert(&mut self, page: Page, contents: &[u8]) {
        if self.at.contains_key(&page) {
            return;
        }
        self.at.insert(page, self.bytes.len());
        self.bytes.extend_from_slice(contents);
    }

    /// Page `page`'s contents, if kept.
    fn get(&self, page: Page) -> Option<&[u8]> {
        let at = *self.at.get(&page)?;
        Some(&self.bytes[at..at + PAGE_SIZE])
    }
}

/// One checkpoint's state, and the contents of the pages it wrote.
struct Checkpoint {
    step: u64,
    cpu: CpuState,
    irqchips: Vec<Vec<u8>>,
    devices: DeviceState,
    /// The read and write records' encoder, which goes on from the
    /// records made up to the checkpoint, and the switches' encoder, with
    /// the switches up to the checkpoint that no batch holds yet pending.
    io: Option<rewind_trace::io_records::Encoder>,
    switches: Option<rewind_trace::switches::Encoder>,
    pages: KeptPages,
}

/// A machine's checkpoints, and where its memory is relative to them.
#[derive(Default)]
pub(crate) struct Checkpoints {
    tree: Tree,
    taken: Vec<Checkpoint>,
    /// The checkpoint whose memory the machine's is, with `since` written
    /// over it; None for the origin.
    anchor: Option<CheckpointId>,
    /// The pages written since the anchor, by the guest as the dirty log
    /// names them, or by the monitor.
    pub(crate) since: BTreeSet<Page>,
    /// The origin's pages read so far, each once.
    origin_pages: KeptPages,
    /// Boot's writes, made again into memory of their own, for a machine
    /// that booted.
    pub(crate) booted: Option<crate::memory::Mapping>,
}

impl Machine {
    /// Reads KVM's dirty log, which it clears, and notes the pages it
    /// names as written since the anchor. Returns their numbers.
    pub(crate) fn dirty_pages(&mut self) -> Result<Vec<usize>> {
        let dirty = self
            .vm
            .get_dirty_log(crate::SLOT_RAM, self.dev.ram.len())
            .context("reading the dirty log")?;
        let pages: Vec<usize> = (0..self.dev.ram.len() / PAGE_SIZE)
            .filter(|&i| dirty[i / 64] & (1 << (i % 64)) != 0)
            .collect();
        self.checkpoints
            .since
            .extend(pages.iter().map(|&i| i as Page));
        for &page in &pages {
            self.dev.ram.mark_page(page);
        }
        Ok(pages)
    }

    /// The page the monitor writes into itself, which the dirty log does
    /// not see, once the guest has named it.
    fn shared_page(&self) -> Option<Page> {
        self.dev.shared.map(|s| (s as usize / PAGE_SIZE) as Page)
    }

    /// Keeps the machine as it is now, at a step's start, its pending exit
    /// completed, to rewind to later. The memory a debugger's int3 holds
    /// is kept as the guest wrote it.
    pub fn checkpoint(&mut self) -> Result<CheckpointId> {
        self.complete_pending_exit()?;
        self.dirty_pages()?;
        let mut written = std::mem::take(&mut self.checkpoints.since);
        written.extend(self.shared_page());

        let mut pages = KeptPages::with_capacity(written.len());
        let mut bytes = [0u8; PAGE_SIZE];
        for &page in &written {
            let at = page as u64 * PAGE_SIZE as u64;
            self.read_physical(at, &mut bytes)?;
            pages.insert(page, &bytes);
        }
        let (cpu, irqchips, devices) = self.state()?;
        let parent = self.checkpoints.anchor;
        let id = self.checkpoints.tree.add(parent, written);
        self.checkpoints.taken.push(Checkpoint {
            step: self.dev.step,
            cpu,
            irqchips,
            devices,
            io: self.dev.io.clone(),
            switches: self.dev.switches.clone(),
            pages,
        });
        self.checkpoints.anchor = Some(id);
        Ok(id)
    }

    /// The non-zero pages of the keyframes the machine was restored from,
    /// each with the hash of its contents, in page order; None for a
    /// machine that booted.
    pub fn restored_pages(&self) -> Option<&[(u32, [u8; 32])]> {
        match &self.origin {
            Origin::Restored(pages) => Some(pages),
            Origin::Boot => None,
        }
    }

    /// The step checkpoint `id` was taken at.
    pub fn checkpoint_step(&self, id: CheckpointId) -> u64 {
        self.checkpoints.taken[id.0].step
    }

    /// Puts the machine back as it was at checkpoint `id`, in place. The
    /// pages of a machine restored from keyframes come from `pages` when
    /// no checkpoint holds them. Takes out every int3 a debugger wrote,
    /// since the memory under them is the checkpoint's now, and leaves
    /// the debugger's traps to be set again.
    pub fn rewind(&mut self, id: CheckpointId, pages: &dyn Pages) -> Result<()> {
        self.complete_pending_exit()?;
        let int3s: Vec<u64> = self.int3s.replaced.keys().copied().collect();
        for physical in int3s {
            self.remove_int3(physical)?;
        }
        self.dirty_pages()?;

        // The pages that can differ, the shared page among them, now and
        // at the checkpoint, each with the checkpoint's contents.
        let mut back = self.checkpoints.tree.to_write_back(
            self.checkpoints.anchor,
            &self.checkpoints.since,
            id,
        );
        back.extend(self.shared_page());
        let target_shared = self.checkpoints.taken[id.0].devices.shared;
        back.extend(target_shared.map(|s| (s as usize / PAGE_SIZE) as Page));
        for page in back {
            let source = self.checkpoints.tree.source(id, page);
            if source == Source::Origin {
                self.read_origin_page(page, pages)?;
            }
            let kept = match source {
                Source::Checkpoint(from) => &self.checkpoints.taken[from.0].pages,
                Source::Origin => &self.checkpoints.origin_pages,
            };
            let contents = kept
                .get(page)
                .expect("the page is kept where its source says");
            let at = page as usize * PAGE_SIZE;
            self.dev.ram.as_mut_slice()[at..at + PAGE_SIZE].copy_from_slice(contents);
            self.dev.ram.mark_page(page as usize);
        }

        // The vCPU, the interrupt controllers and the monitor's state, and
        // a dirty log that starts afresh from the checkpoint's memory.
        let checkpoint = &self.checkpoints.taken[id.0];
        let (cpu, irqchips, devices, io, switches) = (
            checkpoint.cpu.clone(),
            checkpoint.irqchips.clone(),
            checkpoint.devices.clone(),
            checkpoint.io.clone(),
            checkpoint.switches.clone(),
        );
        self.apply_state(&cpu, &irqchips, &devices)?;
        self.dev.io = io;
        self.dev.switches = switches;
        self.dirty_pages()?;
        self.checkpoints.since.clear();
        self.checkpoints.anchor = Some(id);
        Ok(())
    }

    /// Puts the machine at the keyframe `chain` ends with, in place: the
    /// full keyframe first, as for [`Machine::restore`], and `trace` the
    /// run's records, which the encoders go on from at the last one's
    /// step. A machine restored from
    /// keyframes can differ from the last one only in the pages written
    /// since it was restored and the pages the two keyframes hold
    /// differently, so only those are written, from `pages`: about 3% of
    /// a large guest's memory for keyframes a few hundred steps apart,
    /// where a new machine reads every page. Checkpoints are dropped,
    /// since their pages are over the old origin, and the debugger's int3s
    /// taken out. Fails on a machine that booted, which has no keyframe
    /// memory to compare.
    pub fn restore_in_place(
        &mut self,
        chain: &[&crate::snapshot::Keyframe],
        pages: &(dyn Pages + Sync),
        trace: &[(u64, Vec<u8>)],
    ) -> Result<()> {
        let last = chain.last().context("restoring from no keyframes")?;
        if chain[0].parent.is_some() {
            anyhow::bail!("a keyframe chain must start with a full keyframe");
        }
        if !matches!(self.origin, Origin::Restored(_)) {
            anyhow::bail!("a machine that booted cannot be restored in place");
        }
        self.complete_pending_exit()?;
        let int3s: Vec<u64> = self.int3s.replaced.keys().copied().collect();
        for physical in int3s {
            self.remove_int3(physical)?;
        }
        self.dirty_pages()?;

        // Every page written since the old origin: by each checkpoint's
        // stretch, since the anchor, and by the monitor into the shared
        // page, now's and the keyframe's.
        let mut written = self.checkpoints.since.clone();
        for stretch in &self.checkpoints.tree.written {
            written.extend(stretch);
        }
        written.extend(self.shared_page());
        written.extend(
            last.devices
                .shared
                .map(|s| (s as usize / PAGE_SIZE) as Page),
        );

        // Those pages and the ones the keyframes hold differently, with
        // the new keyframe's contents, read on several threads.
        let ram_pages = self.dev.ram.len() / PAGE_SIZE;
        let new = crate::snapshot::final_pages(chain.iter().copied(), ram_pages)?;
        let Origin::Restored(old) = &self.origin else {
            unreachable!("checked to be restored above");
        };
        let rewrite = to_rewrite(old, &new, &written);
        for (page, _) in &rewrite {
            self.dev.ram.mark_page(*page as usize);
        }
        let ram = self.dev.ram.as_mut_slice();
        let mut read: Vec<(u32, &[u8; 32])> = Vec::new();
        for (page, hash) in &rewrite {
            match hash {
                Some(hash) => read.push((*page, hash)),
                None => {
                    let at = *page as usize * PAGE_SIZE;
                    ram[at..at + PAGE_SIZE].fill(0);
                }
            }
        }
        crate::snapshot::read_pages(ram, &read, pages)?;
        self.origin = Origin::Restored(new.into_iter().map(|(i, h)| (i, *h)).collect());

        // The checkpoints were over the old origin; the dirty log starts
        // afresh from the keyframe's memory.
        self.checkpoints = Checkpoints::default();
        self.vm.get_dirty_log(crate::SLOT_RAM, self.dev.ram.len())?;
        self.apply_state(&last.cpu, &last.irqchips, &last.devices)?;
        self.resume_records(trace, last.devices.step)?;
        Ok(())
    }

    /// Keeps page `page` as the machine's origin had it, once: from the
    /// keyframes it was restored from, read through `pages`, or as boot
    /// wrote it.
    fn read_origin_page(&mut self, page: Page, pages: &dyn Pages) -> Result<()> {
        if self.checkpoints.origin_pages.get(page).is_some() {
            return Ok(());
        }
        let mut contents = [0u8; PAGE_SIZE];
        if let Origin::Restored(hashes) = &self.origin {
            if let Ok(i) = hashes.binary_search_by_key(&page, |(p, _)| *p) {
                pages.get(&hashes[i].1, &mut contents)?;
            }
        } else {
            if self.checkpoints.booted.is_none() {
                self.write_boot(crate::BootInto::Scratch)?;
            }
            let booted = self
                .checkpoints
                .booted
                .as_ref()
                .expect("boot memory written above");
            let at = page as usize * PAGE_SIZE;
            contents.copy_from_slice(&booted.as_slice()[at..at + PAGE_SIZE]);
        }
        self.checkpoints.origin_pages.insert(page, &contents);
        Ok(())
    }
}

/// The pages restoring keyframe memory `new` over memory restored from
/// `old`, with `written` written since, can change, in page order, each
/// with its hash in `new`, None where `new` holds none and the page is
/// zero: the pages written since, and those the two hold differently.
/// Both lists are of non-zero pages in page order.
fn to_rewrite(
    old: &[(Page, [u8; 32])],
    new: &[(Page, &[u8; 32])],
    written: &BTreeSet<Page>,
) -> Vec<(Page, Option<[u8; 32]>)> {
    let mut pages: BTreeSet<Page> = written.clone();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        match (old.get(i), new.get(j)) {
            (Some((a, ha)), Some((b, hb))) if a == b => {
                if ha != *hb {
                    pages.insert(*a);
                }
                i += 1;
                j += 1;
            }
            (Some((a, _)), Some((b, _))) if a < b => {
                pages.insert(*a);
                i += 1;
            }
            (Some((a, _)), None) => {
                pages.insert(*a);
                i += 1;
            }
            (_, Some((b, _))) => {
                pages.insert(*b);
                j += 1;
            }
            (None, None) => unreachable!("the loop ends when both are done"),
        }
    }
    pages
        .into_iter()
        .map(|page| {
            let hash = new
                .binary_search_by_key(&page, |(p, _)| *p)
                .ok()
                .map(|k| *new[k].1);
            (page, hash)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // The checkpoints' bookkeeping on plain page numbers, with no VM: which
    // pages a rewind writes back, and where each one's contents come from.
    use super::*;

    fn pages(list: &[Page]) -> BTreeSet<Page> {
        list.iter().copied().collect()
    }

    /// A line of checkpoints over the origin, each over the one before:
    /// a rewind down the line writes back what was written since the
    /// anchor and what the checkpoints between wrote, and nothing the
    /// target's own line had written before it.
    #[test]
    fn a_rewind_down_a_line_writes_back_what_came_after() {
        let mut tree = Tree::default();
        let a = tree.add(None, pages(&[1, 2]));
        let b = tree.add(Some(a), pages(&[3]));
        let c = tree.add(Some(b), pages(&[4]));
        assert_eq!(
            tree.to_write_back(Some(c), &pages(&[5]), a),
            pages(&[3, 4, 5])
        );
        assert_eq!(tree.to_write_back(Some(a), &pages(&[]), a), pages(&[]));
        assert_eq!(tree.to_write_back(Some(a), &pages(&[]), c), pages(&[3, 4]));
    }

    /// Two branches over one checkpoint: a rewind from one to the other
    /// writes back the pages either branch wrote below the shared one.
    #[test]
    fn a_rewind_across_branches_writes_back_both() {
        let mut tree = Tree::default();
        let root = tree.add(None, pages(&[1]));
        let left = tree.add(Some(root), pages(&[2]));
        let right = tree.add(Some(root), pages(&[3]));
        assert_eq!(
            tree.to_write_back(Some(left), &pages(&[9]), right),
            pages(&[2, 3, 9])
        );
        assert_eq!(
            tree.to_write_back(None, &pages(&[9]), left),
            pages(&[1, 2, 9])
        );
    }

    /// Restoring a keyframe over memory restored from another rewrites
    /// the pages the two keyframes hold differently and the pages written
    /// since, each with the new keyframe's hash or zero where it holds
    /// none: page 1 is the same in both but written since, 2 changed, 3
    /// is only in the old keyframe, 4 only in the new, 5 written since
    /// and in neither, and 6 the same in both and not written.
    #[test]
    fn restoring_over_rewrites_what_can_differ() {
        let (a, b, c, d, x) = ([1; 32], [2; 32], [3; 32], [4; 32], [9; 32]);
        let old = [(1, a), (2, b), (3, c), (6, a)];
        let new = [(1, &a), (2, &x), (4, &d), (6, &a)];
        assert_eq!(
            to_rewrite(&old, &new, &pages(&[1, 5])),
            [
                (1, Some(a)),
                (2, Some(x)),
                (3, None),
                (4, Some(d)),
                (5, None)
            ]
        );
    }

    /// Pages kept in one buffer read back by number: two pages kept, one
    /// asked for that was not, and a page kept again keeps its first
    /// contents, as a page read once from the origin does.
    #[test]
    fn kept_pages_read_back_by_number() {
        let mut kept = KeptPages::with_capacity(2);
        kept.insert(7, &[7; PAGE_SIZE]);
        kept.insert(3, &[3; PAGE_SIZE]);
        kept.insert(7, &[9; PAGE_SIZE]);
        assert_eq!(kept.get(7), Some(&[7; PAGE_SIZE][..]));
        assert_eq!(kept.get(3), Some(&[3; PAGE_SIZE][..]));
        assert_eq!(kept.get(4), None);
    }

    /// A page's contents come from the nearest checkpoint up the target's
    /// line that wrote it, and from the origin when none did.
    #[test]
    fn a_page_comes_from_the_nearest_checkpoint_that_wrote_it() {
        let mut tree = Tree::default();
        let a = tree.add(None, pages(&[1, 2]));
        let b = tree.add(Some(a), pages(&[2]));
        let other = tree.add(Some(a), pages(&[3]));
        assert_eq!(tree.source(b, 2), Source::Checkpoint(b));
        assert_eq!(tree.source(b, 1), Source::Checkpoint(a));
        assert_eq!(tree.source(b, 3), Source::Origin);
        assert_eq!(tree.source(other, 3), Source::Checkpoint(other));
    }
}
