//! A run's keyframes, and bringing a machine to any step of a run.
//!
//! Keyframes are taken while a run executes, at an interval chosen so each
//! stretch between two of them is about [`TARGET`] of the guest's work, as
//! a reference host runs it. Seeking to a step restores the latest
//! keyframe at or before it and runs forward from there, so no seek costs
//! much more than one interval.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rewind_store::Store;
use rewind_vmm::snapshot::{Keyframe, KeyframeHead, Pages, ZERO_PAGE};
use rewind_vmm::{Machine, Observer, Outcome};
use serde::Deserialize;

/// The work one stretch between a recording's keyframes aims for, in the
/// time the reference host takes for it (see [`Progress::work_to`]).
pub const TARGET: Duration = Duration::from_millis(250);

/// The reference host's time for one step, and for each conditional
/// branch the guest retires in user mode: a least-squares fit of each
/// stretch's wall time to its steps and branches, in recordings of GNU
/// hello's nixpkgs build on a Zen 4 laptop, 49 stretches. The fit came
/// within 10% of 80% of the stretches; steps alone, or virtual time,
/// were off by more than 2x on a fifth of them. A Zen 3 host took
/// 45.5 µs and 1.62 ns, about twice each, so the two weigh the same there.
const NS_PER_STEP: u64 = 22_000;
const PS_PER_BRANCH: u64 = 750;
const PS_PER_NS: u64 = 1000;

/// The steps to the first stop, of a replay toward a step and of a
/// recording, and the most between any two stops.
const MIN_INTERVAL: u64 = 256;
const MAX_INTERVAL: u64 = 1 << 20;

pub use rewind_trace::manifest::KEYFRAMES_DIR as DIR;

/// The extension of a keyframe's file, which is named by its step.
pub const EXTENSION: &str = "kf";

/// The page store as the monitor sees it.
#[derive(Debug)]
pub struct StorePages<'a>(pub &'a mut Store);

impl Pages for StorePages<'_> {
    fn put_all(&mut self, pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        self.0.put_all(pages)
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
}

/// Read-only access, for restoring.
#[derive(Debug)]
pub struct ReadPages<'a>(pub &'a Store);

impl Pages for ReadPages<'_> {
    fn put_all(&mut self, _pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
        anyhow::bail!("restoring does not store pages")
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
}

/// What a keyframe file starts with: this magic, then the version of its
/// format as a little-endian u32. The keyframe follows, in bincode.
pub const KEYFRAME_MAGIC: &[u8; 8] = b"rwkeyfr\0";
pub const KEYFRAME_VERSION: u32 = 1;
const HEADER_LEN: usize = KEYFRAME_MAGIC.len() + 4;

/// A keyframe as its file holds it.
pub fn encode(kf: &Keyframe) -> Result<Vec<u8>> {
    let mut bytes = KEYFRAME_MAGIC.to_vec();
    bytes.extend(KEYFRAME_VERSION.to_le_bytes());
    bytes.extend(bincode::serialize(kf)?);
    Ok(bytes)
}

/// The keyframe a file holds, refusing one of another format and saying
/// which.
pub fn decode(bytes: &[u8]) -> Result<Keyframe> {
    if bytes.len() < HEADER_LEN || &bytes[..KEYFRAME_MAGIC.len()] != KEYFRAME_MAGIC {
        bail!(
            "not a keyframe this build of rewind reads: keyframes written before rewind \
             1.0 name no format"
        );
    }
    let version = &bytes[KEYFRAME_MAGIC.len()..HEADER_LEN];
    let version = u32::from_le_bytes(version.try_into().expect("four bytes"));
    if version != KEYFRAME_VERSION {
        bail!(
            "a keyframe of format {version}; this build of rewind reads format {KEYFRAME_VERSION}"
        );
    }

    // Everything before the pages through serde. The pages, most of the
    // bytes, are read straight from them: serde reads a hash a byte at a
    // time, which was half a second of each late restore.
    let body = &bytes[HEADER_LEN..];
    let mut rest = body;
    let head: KeyframeHead = bincode::deserialize_from(&mut rest)?;
    let pages = decode_pages(rest)?;
    Ok(Keyframe::from_head(head, pages))
}

/// The bytes bincode writes for one page: its number, then its hash.
const PAGE_ENTRY_LEN: usize = 4 + 32;

/// The pages bincode wrote for a `Vec<(u32, [u8; 32])>`, from `bytes`
/// that hold exactly them: a little-endian u64 count, then each page's
/// number, little-endian, and hash.
fn decode_pages(bytes: &[u8]) -> Result<Vec<(u32, [u8; 32])>> {
    let Some((count, entries)) = bytes.split_first_chunk::<8>() else {
        bail!("a keyframe cut short before its pages");
    };
    let count = usize::try_from(u64::from_le_bytes(*count))?;
    if entries.len()
        != count
            .checked_mul(PAGE_ENTRY_LEN)
            .context("too many pages")?
    {
        bail!(
            "a keyframe of {count} pages holds {} bytes of them",
            entries.len()
        );
    }
    let (entries, _) = entries.as_chunks::<PAGE_ENTRY_LEN>();
    Ok(entries
        .iter()
        .map(|entry| {
            let (page, hash) = entry.split_at(4);
            (
                u32::from_le_bytes(page.try_into().expect("four bytes")),
                hash.try_into().expect("32 bytes"),
            )
        })
        .collect())
}

fn path(run_dir: &Path, step: u64) -> PathBuf {
    run_dir.join(DIR).join(format!("{step:016}.{EXTENSION}"))
}

pub fn save(run_dir: &Path, kf: &Keyframe) -> Result<()> {
    fs::create_dir_all(run_dir.join(DIR))?;
    crate::image::write_atomic(&path(run_dir, kf.step), &encode(kf)?)
}

/// What a run's own keyframe directory holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Own {
    /// No keyframes.
    None,
    /// Keyframes that all read back.
    Readable,
    /// Keyframes of which at least one does not read back, such as ones
    /// written in an older format.
    Unreadable,
}

/// The file in a run's keyframe directory whose presence says the run took
/// its keyframes as it executed, from where it started to where it ended.
/// A run without it has keyframes only where something looked inside it,
/// as `rewind check`'s runs have where their job exited.
pub const TAKEN: &str = "taken";

/// Records that the run in `run_dir` took its keyframes as it executed.
pub fn mark_taken(run_dir: &Path) -> Result<()> {
    fs::create_dir_all(run_dir.join(DIR))?;
    crate::image::write_atomic(&run_dir.join(DIR).join(TAKEN), &[])
}

/// Whether the run in `run_dir` took its keyframes as it executed.
pub fn taken(run_dir: &Path) -> bool {
    run_dir.join(DIR).join(TAKEN).exists()
}

/// Whether the keyframes in a run's own directory read back.
pub fn own_state(run_dir: &Path) -> Own {
    let steps = own_steps(run_dir);
    if steps.is_empty() {
        return Own::None;
    }
    let reads = |step: &u64| {
        fs::read(path(run_dir, *step))
            .ok()
            .and_then(|b| decode(&b).ok())
            .is_some()
    };
    if steps.iter().all(reads) {
        Own::Readable
    } else {
        Own::Unreadable
    }
}

/// A run's own keyframe steps, in order.
fn own_steps(run_dir: &Path) -> Vec<u64> {
    let mut steps: Vec<u64> = fs::read_dir(run_dir.join(DIR))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(EXTENSION)?.strip_suffix('.'))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    steps.sort_unstable();
    steps
}

pub use rewind_trace::manifest::SharedKeyframes as Shared;

/// The one field of a manifest that says where else a run's keyframes are.
#[derive(Deserialize)]
struct Sharing {
    shared_keyframes: Option<Shared>,
}

/// One directory a run's keyframes come from, for steps up to `through`.
#[derive(Debug)]
struct Layer {
    dir: PathBuf,
    through: u64,
}

/// Where a run's keyframes are: its own directory, then the directory of
/// the run it shares keyframes with, and so on back to a run that shares
/// none. Runs that share keyframes sit side by side in one runs directory.
#[derive(Debug)]
pub struct Layers {
    layers: Vec<Layer>,
}

impl Layers {
    /// The layers of the run in `run_dir`, whose manifest names `shared`.
    pub fn open(run_dir: &Path, shared: Option<&Shared>) -> Result<Layers> {
        let mut layers = vec![Layer {
            dir: run_dir.to_path_buf(),
            through: u64::MAX,
        }];
        let runs = run_dir.parent().unwrap_or(Path::new("."));
        let mut next = shared.cloned();
        while let Some(share) = next {
            let from = layers.last().expect("layers start with the run's own");
            let dir = runs.join(&share.run);
            let manifest = dir.join(crate::run::MANIFEST);

            // A run whose keyframes another needs may have been deleted or
            // never imported here.
            let bytes = fs::read(&manifest).with_context(|| {
                format!(
                    "run {} reads its keyframes up to step {} from run {}, which is not in {}; \
                     import that run, or export this one with --replayable where both are",
                    from.dir.file_name().unwrap_or_default().to_string_lossy(),
                    share.through,
                    share.run,
                    runs.display()
                )
            })?;
            let sharing: Sharing = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", manifest.display()))?;

            // Sharing that comes back to a run already read would never end.
            if layers.iter().any(|l| l.dir == dir) {
                bail!("runs share keyframes in a cycle through run {}", share.run);
            }
            let through = share.through.min(from.through);
            layers.push(Layer { dir, through });
            next = sharing.shared_keyframes;
        }
        Ok(Layers { layers })
    }

    /// The steps the run has keyframes at, its own and shared, in order.
    pub fn steps(&self) -> Vec<u64> {
        let mut steps: Vec<u64> = self
            .layers
            .iter()
            .flat_map(|l| {
                own_steps(&l.dir)
                    .into_iter()
                    .filter(move |s| *s <= l.through)
            })
            .collect();
        steps.sort_unstable();
        steps.dedup();
        steps
    }

    /// The file holding the keyframe at `step`, if the run has one there.
    pub fn path(&self, step: u64) -> Option<PathBuf> {
        self.layers
            .iter()
            .filter(|l| step <= l.through)
            .map(|l| path(&l.dir, step))
            .find(|p| p.exists())
    }

    /// The directory of the run that owns `step`: the earliest run that
    /// every run sharing it reads that step from. A keyframe taken there
    /// serves every fork made at that point.
    pub fn owner(&self, step: u64) -> &Path {
        let layer = self
            .layers
            .iter()
            .rfind(|l| step <= l.through)
            .expect("the run's own layer covers every step");
        &layer.dir
    }

    /// Whether the run that owns `step` (see [`Layers::owner`]) reads the
    /// keyframe at `parent`, so a keyframe kept there may name it as its
    /// parent; None, boot, has nothing to read. The run reads its own
    /// keyframes and those of the runs it shares with, never those of a
    /// fork of it, which a fork may hold for a shared step when it could
    /// not write to its parent's directory.
    pub fn owner_reads(&self, step: u64, parent: Option<u64>) -> bool {
        let Some(parent) = parent else {
            return true;
        };
        let layer_of = |dir: &Path| self.layers.iter().position(|l| l.dir == dir);
        let owner = layer_of(self.owner(step));
        let holder = self
            .path(parent)
            .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
            .and_then(|dir| layer_of(&dir));
        matches!((owner, holder), (Some(owner), Some(holder)) if holder >= owner)
    }

    pub fn load(&self, step: u64) -> Result<Keyframe> {
        let p = self.path(step).with_context(|| {
            format!(
                "run {} has no keyframe at step {step}",
                self.layers[0].dir.display()
            )
        })?;
        let bytes = fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        decode(&bytes).with_context(|| format!("reading {}", p.display()))
    }

    /// The chain of keyframes that rebuilds the one at `step`, full one
    /// first. The chain may cross from a run's own keyframes into shared
    /// ones. Each parent must be at an earlier step than the keyframe
    /// naming it, so a chain from an import that names itself or a later
    /// keyframe is refused rather than followed forever.
    pub fn chain(&self, step: u64) -> Result<Vec<Keyframe>> {
        let mut chain = vec![self.load(step)?];
        let mut at = step;
        while let Some(parent) = chain.last().and_then(|k| k.parent) {
            if parent >= at {
                bail!(
                    "the keyframe at step {at} of run {} names step {parent} as its parent, \
                     which is not before it",
                    self.layers[0].dir.display()
                );
            }
            chain.push(self.load(parent)?);
            at = parent;
        }
        chain.reverse();
        Ok(chain)
    }
}

/// Keyframes read from their files, by step, so restoring several from
/// one run reads each file once.
#[derive(Debug, Default)]
pub struct Loaded {
    by_step: HashMap<u64, Keyframe>,
}

impl Layers {
    /// The chain of keyframes that rebuilds the one at `step`, full one
    /// first, as [`Layers::chain`] reads it, reading only the keyframes
    /// `loaded` lacks and keeping them there.
    pub fn chain_loaded<'l>(&self, step: u64, loaded: &'l mut Loaded) -> Result<Vec<&'l Keyframe>> {
        // The steps of the chain, newest first, each keyframe read once.
        let mut steps = Vec::new();
        let mut at = Some(step);
        while let Some(this) = at {
            let parent = match loaded.by_step.entry(this) {
                std::collections::hash_map::Entry::Occupied(kf) => kf.get().parent,
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(self.load(this)?).parent
                }
            };
            if let Some(parent) = parent
                && parent >= this
            {
                bail!(
                    "the keyframe at step {this} of run {} names step {parent} as its parent, \
                     which is not before it",
                    self.layers[0].dir.display()
                );
            }
            steps.push(this);
            at = parent;
        }
        steps.reverse();
        Ok(steps.iter().map(|s| &loaded.by_step[s]).collect())
    }
}

/// The page hashes a chain of keyframes has set guest memory to, where they
/// are not zero.
#[derive(Default, Debug)]
pub struct Memory {
    pages: HashMap<u32, [u8; 32]>,
}

impl Memory {
    /// Memory as restoring `chain` leaves it.
    pub fn of(chain: &[Keyframe]) -> Memory {
        let mut memory = Memory::default();
        for kf in chain {
            memory.apply(kf.parent, &kf.pages);
        }
        memory
    }

    /// Memory whose non-zero pages are `pages`, as a machine restored
    /// from keyframes lists them.
    pub fn from_pages(pages: &[(u32, [u8; 32])]) -> Memory {
        Memory {
            pages: pages.iter().copied().collect(),
        }
    }

    /// Leaves out of a new keyframe's `pages` the ones whose contents are
    /// what memory already holds: pages the guest wrote back to the same
    /// bytes, often zero, since the keyframe at `parent`. Restoring sets
    /// only the pages a keyframe lists, so the rest keep the contents they
    /// had, which are these. A full keyframe, with no parent, is kept
    /// whole.
    pub fn trim(&mut self, parent: Option<u64>, pages: &mut Vec<(u32, [u8; 32])>) {
        if parent.is_some() {
            pages.retain(|(index, hash)| self.pages.get(index).unwrap_or(&ZERO_PAGE) != hash);
        }
        self.apply(parent, pages);
    }

    /// The hash of a page's contents: the zero page's for a page this
    /// memory does not hold.
    pub fn hash_of(&self, page: u32) -> [u8; 32] {
        *self.pages.get(&page).unwrap_or(&ZERO_PAGE)
    }

    /// The pages whose contents differ between this memory and `other`,
    /// in page order, with each side's hash: the zero page's where a
    /// side holds none.
    pub fn differing(&self, other: &Memory) -> Vec<(u32, [u8; 32], [u8; 32])> {
        let mut pages: Vec<u32> = self
            .pages
            .keys()
            .chain(other.pages.keys())
            .copied()
            .collect();
        pages.sort_unstable();
        pages.dedup();
        pages
            .into_iter()
            .filter_map(|page| {
                let ours = *self.pages.get(&page).unwrap_or(&ZERO_PAGE);
                let theirs = *other.pages.get(&page).unwrap_or(&ZERO_PAGE);
                (ours != theirs).then_some((page, ours, theirs))
            })
            .collect()
    }

    /// The pages among `pages` whose contents differ between this memory
    /// and `other`, as [`Memory::differing`] gives them. Two memories that
    /// were alike can differ since only at the pages either wrote, so a
    /// list of those pages finds every difference without reading the
    /// rest. `pages` may name a page twice and in any order.
    pub fn differing_among(&self, other: &Memory, pages: &[u32]) -> Vec<(u32, [u8; 32], [u8; 32])> {
        let mut pages = pages.to_vec();
        pages.sort_unstable();
        pages.dedup();
        pages
            .into_iter()
            .filter_map(|page| {
                let ours = self.hash_of(page);
                let theirs = other.hash_of(page);
                (ours != theirs).then_some((page, ours, theirs))
            })
            .collect()
    }

    /// Writes a keyframe's `pages` over this memory, as restoring the
    /// keyframe, whose parent is `parent`, would.
    pub fn apply(&mut self, parent: Option<u64>, pages: &[(u32, [u8; 32])]) {
        // A full keyframe starts from zeroed memory.
        if parent.is_none() {
            self.pages.clear();
        }
        for (index, hash) in pages {
            if *hash == ZERO_PAGE {
                self.pages.remove(index);
            } else {
                self.pages.insert(*index, *hash);
            }
        }
    }
}

/// How far past the nearest earlier keyframe, in steps, a fork must be
/// before it keeps a keyframe at its step. Keeping one costs about 10 ms
/// plus the pages written since that keyframe, and spares every later
/// fork at the step the replay to it. In a Nix build of a small C
/// library, replaying 200 steps took 12 ms and keeping a keyframe after
/// them 25 ms; 500 steps took 26 ms and 29 ms; 800 took 35 ms and 28 ms.
/// So from about 512 steps one later fork at the step, of the one or two
/// more `rewind where` makes, pays for the keyframe, and kept keyframes
/// are never closer together than that.
pub const KEEP_AFTER: u64 = 512;

/// Whether a fork at `step` of a run with keyframes at `steps` keeps a
/// keyframe there: when the nearest keyframe at or before the step, the
/// one the fork restored, is more than KEEP_AFTER steps back. Without an
/// earlier keyframe the fork booted, and boot counts as one at step 0, so
/// a run recorded without keyframes, as most of `rewind check`'s are,
/// gets a full one where it is first looked inside, and the next look
/// near there restores it instead of booting.
pub fn worth_keeping(steps: &[u64], step: u64) -> bool {
    let nearest = steps.iter().copied().rfind(|s| *s <= step).unwrap_or(0);
    step - nearest > KEEP_AFTER
}

/// How much of the guest's work a look inside a run, on its way to its
/// step, replays without keeping a keyframe, in the reference host's time
/// for it (see [`Progress::work_to`]): a later look anywhere on that
/// stretch then replays at most about this much. Keyframes kept this way
/// cost disk like any other, so the stretch is far longer than
/// [`TARGET`], which recording aims for.
///
/// A check run of a sagittarius build, 296,484 steps recorded without
/// keyframes, was read at step 290,000 and then at steps 100,000,
/// 175,000, 250,000 and 289,000, in a page store holding only its
/// schedule 0's pages. Keeping a keyframe only at each read's step, the
/// first read took 191 s, kept one keyframe of 6.2 MB and added 183 MB of
/// pages, and the later ones took 10, 75, 75 and 38 s. Keeping one every
/// 2 s of replay as well, the first read took 218 s and kept 48 of 47 MB
/// adding 829 MB, and the later ones took 3.7 to 5.3 s; every 5 s, 210 s,
/// 27 of 35 MB adding 645 MB, and 4.8 to 8.0 s; every 10 s, 203 s, 15 of
/// 24 MB adding 470 MB, and 3.1 to 10.5 s. Each keyframe holds the pages
/// written since the last, 14 to 21 MB of new pages here, however long
/// the stretch, so the cost goes with how many there are. 10 s leaves no
/// later read waiting more than about 10 s plus the restore, against
/// minutes before, for less than half of what 2 s adds past the first
/// read's own keyframe: 287 MB against 646 MB.
pub const KEEP_EVERY: Duration = Duration::from_secs(10);

/// How much of the guest's work a replay toward a step does between two
/// stops on the way, where it says how far it got and sees whether a
/// keyframe is due, in the reference host's time for it.
pub const STOP_EVERY: Duration = Duration::from_millis(250);

/// Paces a replay toward a step: where it stops on the way, and at which
/// of those stops a look inside the run keeps a keyframe. Like
/// [`Spacing`], it reads only the guest's progress, so a look keeps its
/// keyframes at the same steps on every host.
#[derive(Debug)]
pub struct Pace {
    /// Steps from one stop to the next, aimed at STOP_EVERY of work.
    stride: u64,
    /// The work since the keyframe the machine started from or last
    /// kept, in the reference host's nanoseconds.
    since: u64,
}

impl Default for Pace {
    fn default() -> Pace {
        Pace {
            stride: MIN_INTERVAL,
            since: 0,
        }
    }
}

impl Pace {
    /// The step of the next stop from `at` on the way to `to`.
    pub fn next_stop(&self, at: u64, to: u64) -> u64 {
        at.saturating_add(self.stride).min(to)
    }

    /// Takes the stretch just replayed, from `from` to `to`, and aims the
    /// next one at STOP_EVERY of the same work per step.
    pub fn ran(&mut self, from: Progress, to: Progress) {
        let work = from.work_to(to);
        self.since = self.since.saturating_add(work);
        if let Some(stride) = stride(from, to, STOP_EVERY.as_nanos()) {
            self.stride = stride;
        }
    }

    /// Whether a look stopped at `at` on its way to `to` keeps a keyframe
    /// there: once KEEP_EVERY of work has passed since the last one, and
    /// not within KEEP_AFTER of `to`, where the look keeps one of its own
    /// (see [`worth_keeping`]).
    pub fn keep_at(&self, at: u64, to: u64) -> bool {
        u128::from(self.since) >= KEEP_EVERY.as_nanos() && to - at > KEEP_AFTER
    }

    /// Notes that a keyframe was kept at the last stop.
    pub fn kept(&mut self) {
        self.since = 0;
    }
}

/// The steps of a stretch aimed at `aim` nanoseconds of the reference
/// host's work, at the work per step of the stretch from `from` to `to`,
/// or None for a stretch with no progress to scale by.
fn stride(from: Progress, to: Progress, aim: u128) -> Option<u64> {
    let work = from.work_to(to);
    if work == 0 {
        return None;
    }
    let steps = to.step.saturating_sub(from.step);
    let scaled = u128::from(steps) * aim / u128::from(work);
    let scaled = u64::try_from(scaled).unwrap_or(MAX_INTERVAL);
    Some(scaled.clamp(MIN_STRIDE, MAX_INTERVAL))
}

/// How far a guest has run: its step, and the conditional branches it has
/// retired in user mode. Both are the same wherever the run executes, so
/// keyframes spaced by them fall at the same steps on every host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub step: u64,
    pub branches: u64,
}

impl Progress {
    /// How far `machine` has run. Without a counter, under the exits
    /// clock, the branches stay where they are and steps alone count.
    pub fn of(machine: &Machine) -> Progress {
        Progress {
            step: machine.step(),
            branches: machine.branches(),
        }
    }

    /// The reference host's time, in nanoseconds, for the guest to run
    /// from here to `later`.
    pub fn work_to(self, later: Progress) -> u64 {
        let steps = later.step.saturating_sub(self.step);
        let branches = later.branches.saturating_sub(self.branches);
        steps
            .saturating_mul(NS_PER_STEP)
            .saturating_add(branches.saturating_mul(PS_PER_BRANCH) / PS_PER_NS)
    }
}

/// How many times a recording stops between two keyframes to see whether
/// the next is due. A stretch between stops is sized from the one before,
/// so where the guest's steps start doing more work, as when a build turns
/// from starting processes to compiling, a keyframe comes at most one
/// stretch of the old steps late, an eighth of TARGET at their cost,
/// rather than a whole interval of them.
const STOPS_PER_KEYFRAME: u64 = 8;

/// The fewest steps between two stops: a step that alone is more work
/// than a stretch, as a compiler's can be, gets a stop of its own.
const MIN_STRIDE: u64 = 1;

/// Where a recording takes its keyframes: the first at the first stop,
/// MIN_INTERVAL steps in, then at the first stop TARGET of the guest's
/// work past the last. It reads nothing of the host, not the time a
/// stretch or a keyframe took, so a recording keyframes at the same steps
/// however fast the host runs it.
#[derive(Debug)]
pub struct Spacing {
    /// Steps from one stop to the next, aimed at TARGET / STOPS_PER_KEYFRAME
    /// of work.
    stride: u64,
    /// The work since the last keyframe, in the reference host's
    /// nanoseconds.
    since: u64,
    /// Whether the recording has taken a keyframe yet.
    kept_one: bool,
}

impl Default for Spacing {
    fn default() -> Spacing {
        Spacing {
            stride: MIN_INTERVAL,
            since: 0,
            kept_one: false,
        }
    }
}

impl Spacing {
    /// The step of the next stop after `at`.
    pub fn next_stop(&self, at: u64) -> u64 {
        at.saturating_add(self.stride)
    }

    /// Takes the stretch just run, from `from` to `to`, and aims the next
    /// one at the same work per step.
    pub fn ran(&mut self, from: Progress, to: Progress) {
        let work = from.work_to(to);
        self.since = self.since.saturating_add(work);
        let aim = TARGET.as_nanos() / u128::from(STOPS_PER_KEYFRAME);
        if let Some(stride) = stride(from, to, aim) {
            self.stride = stride;
        }
    }

    /// Whether the recording takes a keyframe at this stop.
    pub fn due(&self) -> bool {
        !self.kept_one || u128::from(self.since) >= TARGET.as_nanos()
    }

    /// Notes that a keyframe was taken at this stop.
    pub fn kept(&mut self) {
        self.since = 0;
        self.kept_one = true;
    }
}

/// Runs a machine to the end, taking keyframes as it goes, or until the
/// observer asks it to stop short of a stop, where it returns
/// [`Outcome::Paused`] and can be called again to run on. `parent` names
/// the keyframe the machine was restored from or last took, or none
/// before the first, which then holds every page; `spacing` says when the
/// next one is due. Both carry over from one call to the next.
pub fn run_with_keyframes(
    machine: &mut Machine,
    obs: &mut dyn Observer,
    run_dir: &Path,
    store: &mut Store,
    parent: &mut Option<u64>,
    spacing: &mut Spacing,
    memory: &mut Memory,
) -> Result<Outcome> {
    loop {
        // A stretch to the next stop, which takes a keyframe only once
        // one is due.
        let from = Progress::of(machine);
        let outcome = machine.run(Some(spacing.next_stop(from.step)), obs)?;
        if let Outcome::Stopped(_) = outcome {
            return Ok(outcome);
        }
        spacing.ran(from, Progress::of(machine));
        if obs.stop() {
            return Ok(outcome);
        }
        if !spacing.due() {
            continue;
        }
        spacing.kept();
        take(machine, run_dir, store, parent, memory)?;
    }
}

/// Takes a keyframe of the machine where it is, holding only the pages
/// whose contents changed since the keyframe at `parent`, saves it in
/// `run_dir` and names it the next keyframe's parent.
pub fn take(
    machine: &mut Machine,
    run_dir: &Path,
    store: &mut Store,
    parent: &mut Option<u64>,
    memory: &mut Memory,
) -> Result<()> {
    let mut kf = machine.keyframe(&mut StorePages(store), *parent)?;
    memory.trim(kf.parent, &mut kf.pages);

    // The pages first, durably, then the keyframe naming them.
    store.sync()?;
    save(run_dir, &kf)?;
    *parent = Some(kf.step);
    Ok(())
}

#[cfg(test)]
mod format_tests {
    // A keyframe file names its format before its contents: one of another
    // format, or one from before keyframes named theirs, is refused with
    // what it is.
    use super::*;

    #[test]
    fn a_keyframe_names_its_format_first() {
        let mut kf = Keyframe::default();
        kf.step = 4;
        let bytes = encode(&kf).unwrap();
        assert_eq!(&bytes[..KEYFRAME_MAGIC.len()], KEYFRAME_MAGIC);
        assert_eq!(
            bytes[KEYFRAME_MAGIC.len()..HEADER_LEN],
            KEYFRAME_VERSION.to_le_bytes()
        );
        assert_eq!(decode(&bytes).unwrap().step, 4);

        let mut other = bytes.clone();
        other[KEYFRAME_MAGIC.len()..HEADER_LEN]
            .copy_from_slice(&(KEYFRAME_VERSION + 1).to_le_bytes());
        let err = format!("{:#}", decode(&other).unwrap_err());
        assert!(
            err.contains(&format!("format {}", KEYFRAME_VERSION + 1)),
            "{err}"
        );

        let before = bincode::serialize(&kf).unwrap();
        let err = format!("{:#}", decode(&before).unwrap_err());
        assert!(err.contains("before"), "{err}");
    }

    /// A keyframe's pages read back as bincode wrote them, from a keyframe
    /// with a parent and three pages encoded and decoded again: the bytes
    /// of the decoded keyframe are the bytes of the original.
    #[test]
    fn a_keyframe_s_pages_read_back() {
        let mut kf = Keyframe::default();
        kf.step = 9;
        kf.parent = Some(3);
        kf.pages = vec![(0, [1; 32]), (7, [2; 32]), (u32::MAX, [3; 32])];
        let decoded = decode(&encode(&kf).unwrap()).unwrap();
        assert_eq!(decoded.pages, kf.pages);
        assert_eq!(
            bincode::serialize(&decoded).unwrap(),
            bincode::serialize(&kf).unwrap()
        );
    }

    /// A keyframe cut short in its pages is refused rather than read as
    /// fewer pages.
    #[test]
    fn a_keyframe_cut_short_is_refused() {
        let mut kf = Keyframe::default();
        kf.pages = vec![(1, [1; 32]), (2, [2; 32])];
        let bytes = encode(&kf).unwrap();
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }
}

#[cfg(test)]
mod memory_tests {
    // Memory as a chain of keyframes leaves it, built two ways.
    use super::*;

    /// Memory built from the non-zero pages a restore leaves, in page
    /// order, is memory built from the chain: a full keyframe of pages 1
    /// and 2, then a delta that rewrites 2, zeroes 1 and adds 3.
    #[test]
    fn memory_from_restored_pages_is_memory_from_the_chain() {
        let mut full = Keyframe::default();
        full.pages = vec![(1, [1; 32]), (2, [2; 32])];
        let mut delta = Keyframe::default();
        delta.parent = Some(0);
        delta.pages = vec![(1, ZERO_PAGE), (2, [5; 32]), (3, [3; 32])];
        let of_chain = Memory::of(&[full, delta]);
        let restored = Memory::from_pages(&[(2, [5; 32]), (3, [3; 32])]);
        for page in 0..5 {
            assert_eq!(
                restored.hash_of(page),
                of_chain.hash_of(page),
                "page {page}"
            );
        }
        assert!(restored.differing(&of_chain).is_empty());
    }
}

#[cfg(test)]
mod tests {
    // Layers: where each of a run's keyframes is read from. The tests lay
    // out run directories with empty .kf files and manifests holding only
    // the field Layers reads, and check the steps and paths it resolves.
    use super::*;

    /// A fresh runs directory for one test.
    fn runs(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-layers-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A run directory with empty keyframe files at `steps` and a manifest
    /// that shares keyframes with `shared`.
    fn run(runs: &Path, id: &str, steps: &[u64], shared: Option<Shared>) -> PathBuf {
        let dir = runs.join(crate::run::tests::id(id));
        fs::create_dir_all(dir.join(DIR)).unwrap();
        for step in steps {
            fs::write(path(&dir, *step), b"").unwrap();
        }
        let manifest =
            serde_json::json!({ "id": crate::run::tests::id(id), "shared_keyframes": shared });
        fs::write(dir.join(crate::run::MANIFEST), manifest.to_string()).unwrap();
        dir
    }

    /// A chain read through a cache of loaded keyframes reads each file
    /// once: a run with a full keyframe at 10 and deltas over it at 20 and
    /// 30, whose chain to 30 is read, then the keyframe file at 10 removed,
    /// still gives the chains to 20 and to a new 40 over 10, and the chain
    /// to a keyframe over one never read, with no file, fails.
    #[test]
    fn a_cached_chain_reads_each_keyframe_once() {
        let runs = runs("cached-chain");
        let dir = run(&runs, "a", &[], None);
        let keyframe = |step: u64, parent: Option<u64>| {
            let mut kf = Keyframe::default();
            kf.step = step;
            kf.parent = parent;
            kf.pages = vec![(step as u32, [step as u8; 32])];
            save(&dir, &kf).unwrap();
        };
        keyframe(10, None);
        keyframe(20, Some(10));
        keyframe(30, Some(20));
        let layers = Layers::open(&dir, None).unwrap();
        let mut loaded = Loaded::default();

        let steps = |chain: Vec<&Keyframe>| chain.iter().map(|k| k.step).collect::<Vec<_>>();
        assert_eq!(
            steps(layers.chain_loaded(30, &mut loaded).unwrap()),
            [10, 20, 30]
        );
        fs::remove_file(path(&dir, 10)).unwrap();
        assert_eq!(
            steps(layers.chain_loaded(20, &mut loaded).unwrap()),
            [10, 20]
        );

        keyframe(40, Some(10));
        assert_eq!(
            steps(layers.chain_loaded(40, &mut loaded).unwrap()),
            [10, 40]
        );
        keyframe(50, Some(45));
        assert!(layers.chain_loaded(50, &mut loaded).is_err());
        fs::remove_dir_all(&runs).unwrap();
    }

    fn share(run: &str, through: u64) -> Option<Shared> {
        Some(Shared {
            run: crate::run::tests::id(run),
            through,
        })
    }

    #[test]
    fn a_delta_lists_only_pages_whose_contents_changed() {
        // Memory follows the pages a chain has set. A delta's page that the
        // guest wrote back to what it was, zero included, is left out; a
        // changed one stays, and Memory learns it for the next delta.
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut memory = Memory::default();
        let mut full = vec![(1, a), (2, b)];
        memory.trim(None, &mut full);
        assert_eq!(full, vec![(1, a), (2, b)]);

        let mut delta = vec![(1, a), (2, a), (3, ZERO_PAGE), (4, b)];
        memory.trim(Some(256), &mut delta);
        assert_eq!(delta, vec![(2, a), (4, b)]);

        let mut next = vec![(2, a), (4, ZERO_PAGE)];
        memory.trim(Some(512), &mut next);
        assert_eq!(next, vec![(4, ZERO_PAGE)]);
    }

    #[test]
    fn a_chain_whose_parents_do_not_go_back_is_refused() {
        // Keyframes saved with their parents: 0 full, 256 over 0, and 512
        // naming itself, as an imported keyframe could. The chain to 256
        // goes back to 0; the one to 512 is refused instead of looping,
        // as is one through a keyframe naming a later step as its parent.
        let runs = runs("chain");
        let dir = run(&runs, "a", &[], None);
        let at = |step, parent| {
            let mut kf = Keyframe::default();
            kf.step = step;
            kf.parent = parent;
            kf
        };
        for kf in [at(0, None), at(256, Some(0)), at(512, Some(512))] {
            save(&dir, &kf).unwrap();
        }
        let layers = Layers::open(&dir, None).unwrap();
        let steps = |c: Vec<Keyframe>| c.iter().map(|k| k.step).collect::<Vec<_>>();
        assert_eq!(steps(layers.chain(256).unwrap()), vec![0, 256]);
        assert!(layers.chain(512).is_err());

        save(&dir, &at(768, Some(1024))).unwrap();
        save(&dir, &at(1024, Some(768))).unwrap();
        assert!(layers.chain(1024).is_err());
    }

    #[test]
    fn a_run_that_shares_nothing_has_its_own_keyframes() {
        // Without a Shared entry, the steps are exactly the run's own files.
        let runs = runs("own");
        let dir = run(&runs, "a", &[512, 256], None);
        let layers = Layers::open(&dir, None).unwrap();
        assert_eq!(layers.steps(), vec![256, 512]);
        assert_eq!(layers.path(256), Some(path(&dir, 256)));
        assert_eq!(layers.path(300), None);
    }

    #[test]
    fn a_fork_reads_its_parents_keyframes_up_to_the_shared_step() {
        // A fork sharing through 2400 sees its parent's keyframes at 256
        // and 1200 but not 2500, which is past the point the two runs
        // part, and reads its own after that from its own directory.
        let runs = runs("fork");
        let parent = run(&runs, "p", &[256, 1200, 2500], None);
        let fork = run(&runs, "c", &[3000], share("p", 2400));
        let layers = Layers::open(&fork, share("p", 2400).as_ref()).unwrap();
        assert_eq!(layers.steps(), vec![256, 1200, 3000]);
        assert_eq!(layers.path(1200), Some(path(&parent, 1200)));
        assert_eq!(layers.path(2500), None);
        assert_eq!(layers.path(3000), Some(path(&fork, 3000)));
    }

    #[test]
    fn a_fork_of_a_fork_reads_through_both() {
        // c shares through 2400 with p, which shares through 1500 with g:
        // c sees g's keyframes up to 1500 only, p's up to 2400, and its own.
        let runs = runs("grandparent");
        let g = run(&runs, "g", &[256, 1200, 1800], None);
        let p = run(&runs, "p", &[2000, 2600], share("g", 1500));
        let c = run(&runs, "c", &[3000], share("p", 2400));
        let layers = Layers::open(&c, share("p", 2400).as_ref()).unwrap();
        assert_eq!(layers.steps(), vec![256, 1200, 2000, 3000]);
        assert_eq!(layers.path(256), Some(path(&g, 256)));
        assert_eq!(layers.path(2000), Some(path(&p, 2000)));
    }

    #[test]
    fn a_shared_step_belongs_to_the_run_that_first_had_it() {
        // owner() names the directory a new keyframe at a shared step goes
        // to, so every fork at that step finds it: the deepest run whose
        // share covers the step.
        let runs = runs("owner");
        let g = run(&runs, "g", &[256], None);
        let p = run(&runs, "p", &[], share("g", 1500));
        let c = run(&runs, "c", &[], share("p", 2400));
        let layers = Layers::open(&c, share("p", 2400).as_ref()).unwrap();
        assert_eq!(layers.owner(1000), g.as_path());
        assert_eq!(layers.owner(2000), p.as_path());
        assert_eq!(layers.owner(3000), c.as_path());
    }

    #[test]
    fn a_keyframe_kept_for_a_parent_names_one_the_parent_reads() {
        // c shares through 2400 with p. A keyframe kept at 2000 goes to p,
        // which owns that step: one whose parent is p's 1200 is fine, one
        // whose parent is a keyframe at 1800 in c's own directory, which
        // p cannot read, is not, and boot, with no parent, always is.
        let runs = runs("parent-reads");
        run(&runs, "p", &[1200], None);
        let c = run(&runs, "c", &[1800, 3000], share("p", 2400));
        let layers = Layers::open(&c, share("p", 2400).as_ref()).unwrap();
        assert!(layers.owner_reads(2000, Some(1200)));
        assert!(!layers.owner_reads(2000, Some(1800)));
        assert!(layers.owner_reads(2000, None));
        assert!(layers.owner_reads(3500, Some(3000)));
        assert!(layers.owner_reads(3500, Some(1200)));
    }

    #[test]
    fn a_lookup_keeps_a_keyframe_only_far_from_the_one_it_restored() {
        // worth_keeping() over lists of keyframe steps: a step more than
        // KEEP_AFTER past the nearest earlier keyframe is worth one, a
        // step at or nearer to it is not, keyframes after the step do not
        // count, and without an earlier keyframe boot counts as one at
        // step 0.
        let steps = [256, 512, 1439];
        assert!(worth_keeping(&steps, 1439 + KEEP_AFTER + 1));
        assert!(worth_keeping(&steps, 5060));
        assert!(!worth_keeping(&steps, 1439 + KEEP_AFTER));
        assert!(!worth_keeping(&steps, 1439));
        assert!(!worth_keeping(&steps, 600));
        assert!(worth_keeping(&[256, 9000], 256 + KEEP_AFTER + 1));
        assert!(worth_keeping(&[], 5060));
        assert!(worth_keeping(&[9000], 5060));
        assert!(!worth_keeping(&[], KEEP_AFTER));
    }

    #[test]
    fn a_long_look_keeps_keyframes_along_the_way() {
        // Pace over the made-up guest of keyframes_are_a_target_of_the_
        // guests_work_apart: a look stops about every STOP_EVERY of the
        // guest's work on the way, keeps a keyframe at the first stop
        // KEEP_EVERY of work past the last, and keeps none within
        // KEEP_AFTER of the step it goes to, where the look keeps its own.
        // It reads only the guest's progress, so a look on any host keeps
        // them at the same steps.
        let looks = || {
            let mut pace = Pace::default();
            let mut at = guest(0);
            let mut kept = Vec::new();
            while at.step < GUEST_STEPS {
                let stop = pace.next_stop(at.step, GUEST_STEPS);
                assert!(stop > at.step && stop <= GUEST_STEPS);
                pace.ran(at, guest(stop));
                at = guest(stop);
                if pace.keep_at(at.step, GUEST_STEPS) {
                    kept.push(at.step);
                    pace.kept();
                }
            }
            kept
        };
        let kept = looks();
        assert_eq!(kept, looks());
        let every = KEEP_EVERY.as_nanos() as u64;
        let total = guest(0).work_to(guest(GUEST_STEPS));
        assert!(kept.len() as u64 >= total / every - 2, "{kept:?}");
        let compute = guest(GUEST_SYSCALLS).work_to(guest(GUEST_SYSCALLS + 1));
        let stretch = STOP_EVERY.as_nanos() as u64;
        for (before, after) in std::iter::once(&0).chain(&kept).zip(&kept) {
            let work = guest(*before).work_to(guest(*after));
            assert!(work >= every, "{before}..{after}: {work} ns");
            assert!(
                work <= every + stretch + 2 * compute,
                "{before}..{after}: {work} ns"
            );
        }
        assert!(GUEST_STEPS - kept.last().unwrap() > KEEP_AFTER);

        // A look a few hundred steps past its keyframe keeps none on the
        // way, however much work those steps are.
        let mut short = Pace::default();
        let heavy = Progress {
            step: KEEP_AFTER,
            branches: u64::MAX / 2,
        };
        short.ran(Progress::default(), heavy);
        assert!(!short.keep_at(KEEP_AFTER, KEEP_AFTER * 2));
    }

    /// A made-up guest's progress at `step`: GUEST_SYSCALLS steps of
    /// few branches each, as a build's many small processes make, then
    /// computation of many branches a step.
    fn guest(step: u64) -> Progress {
        let calls = step.min(GUEST_SYSCALLS);
        let computing = step - calls;
        Progress {
            step,
            branches: calls * GUEST_SYSCALL_BRANCHES + computing * GUEST_COMPUTE_BRANCHES,
        }
    }
    const GUEST_SYSCALLS: u64 = 200_000;
    const GUEST_SYSCALL_BRANCHES: u64 = 2_000;
    const GUEST_COMPUTE_BRANCHES: u64 = 300_000;
    const GUEST_STEPS: u64 = 300_000;

    /// The steps a recording of [`guest`] takes its keyframes at.
    fn keyframe_steps() -> Vec<u64> {
        let mut spacing = Spacing::default();
        let mut at = guest(0);
        let mut steps = Vec::new();
        while at.step < GUEST_STEPS {
            let stop = guest(spacing.next_stop(at.step).min(GUEST_STEPS));
            spacing.ran(at, stop);
            at = stop;
            if spacing.due() {
                steps.push(at.step);
                spacing.kept();
            }
        }
        steps
    }

    #[test]
    fn keyframes_are_a_target_of_the_guests_work_apart() {
        // A recording of a made-up guest, in two phases whose steps each
        // do the same work, the second's ten times the first's. The first
        // keyframe is at MIN_INTERVAL, and each later one at least TARGET
        // of work past the one before. Within a phase each is at most one
        // stop's stretch, TARGET / STOPS_PER_KEYFRAME, past TARGET, and
        // where the phases meet, at most one stretch of the first phase's
        // steps at the second's cost. The guest's progress is all the
        // spacing reads, so a host of any speed keyframes the guest at
        // these same steps.
        let steps = keyframe_steps();
        assert_eq!(steps, keyframe_steps());
        assert_eq!(steps[0], MIN_INTERVAL);
        let target = TARGET.as_nanos() as u64;
        let stretch = target / STOPS_PER_KEYFRAME;
        let syscall = guest(0).work_to(guest(1));
        let compute = guest(GUEST_SYSCALLS).work_to(guest(GUEST_SYSCALLS + 1));
        assert_eq!((syscall, compute), (23_500, 247_000));

        let mut checked = 0;
        for pair in steps.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let work = guest(from).work_to(guest(to));
            assert!(work >= target, "{from}..{to}: {work} ns");
            let most = match (to <= GUEST_SYSCALLS, from >= GUEST_SYSCALLS) {
                (true, _) => target + stretch + syscall,
                (_, true) => target + stretch + compute,
                _ => target + stretch / syscall * compute + compute,
            };
            assert!(work <= most, "{from}..{to}: {work} ns, more than {most}");
            checked += 1;
        }
        assert!(checked > 100, "{steps:?}");
    }

    #[test]
    fn a_step_of_more_work_than_a_keyframe_is_due_keyframes_each_stop() {
        // Steps of a second of work each: a stop after every step, and
        // each a keyframe. A stretch with no progress at all, as a run
        // that stopped where it started, leaves the stride as it was
        // rather than dividing by zero.
        let mut spacing = Spacing::default();
        spacing.kept();
        let slow = |step| Progress {
            step,
            branches: step * 1_400_000_000,
        };
        let mut at = 0;
        for _ in 0..4 {
            let stop = spacing.next_stop(at);
            spacing.ran(slow(at), slow(stop));
            assert!(spacing.due(), "at {stop}");
            spacing.kept();
            at = stop;
        }
        assert_eq!(spacing.next_stop(at), at + MIN_STRIDE);
        spacing.ran(slow(at), slow(at));
        assert_eq!(spacing.next_stop(at), at + MIN_STRIDE);
        assert!(!spacing.due());
    }

    #[test]
    fn a_missing_parent_is_an_error_naming_it() {
        // A fork whose parent directory is gone cannot reach its shared
        // keyframes, and says which run it needs instead of panicking.
        let runs = runs("missing");
        let c = run(&runs, "c", &[3000], share("gone", 2400));
        let err = Layers::open(&c, share("gone", 2400).as_ref()).expect_err("opening should fail");
        let message = format!("{err:#}");
        assert!(
            message.contains(crate::run::tests::id("gone").as_str()),
            "{message}"
        );
        assert!(message.contains("2400"), "{message}");
    }
}
