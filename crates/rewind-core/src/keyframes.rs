//! A run's keyframes, and bringing a machine to any step of a run.
//!
//! Keyframes are taken while a run executes, at an interval chosen so each
//! stretch between two of them takes about [`TARGET`] of wall time to run.
//! Seeking to a step restores the latest keyframe at or before it and runs
//! forward from there, so no seek costs much more than one interval.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rewind_store::Store;
use rewind_vmm::snapshot::{Keyframe, Pages, ZERO_PAGE};
use rewind_vmm::{Machine, Observer, Outcome};
use serde::Deserialize;

/// The wall time one stretch between keyframes aims for.
pub const TARGET: Duration = Duration::from_millis(250);

/// Bounds on the interval, in steps.
const MIN_INTERVAL: u64 = 256;
const MAX_INTERVAL: u64 = 1 << 20;

pub const DIR: &str = "keyframes";

/// The extension of a keyframe's file, which is named by its step.
pub const EXTENSION: &str = "kf";

/// The page store as the monitor sees it.
pub struct StorePages<'a>(pub &'a mut Store);

impl Pages for StorePages<'_> {
    fn put(&mut self, page: &[u8]) -> Result<[u8; 32]> {
        self.0.put(page)
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
}

/// Read-only access, for restoring.
pub struct ReadPages<'a>(pub &'a Store);

impl Pages for ReadPages<'_> {
    fn put(&mut self, _page: &[u8]) -> Result<[u8; 32]> {
        anyhow::bail!("restoring does not store pages")
    }
    fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
        self.0.get(hash, out)
    }
}

fn path(run_dir: &Path, step: u64) -> PathBuf {
    run_dir.join(DIR).join(format!("{step:016}.{EXTENSION}"))
}

pub fn save(run_dir: &Path, kf: &Keyframe) -> Result<()> {
    fs::create_dir_all(run_dir.join(DIR))?;
    let bytes = bincode::serialize(kf)?;
    crate::image::write_atomic(&path(run_dir, kf.step), &bytes)
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

/// Whether the keyframes in a run's own directory read back.
pub fn own_state(run_dir: &Path) -> Own {
    let steps = own_steps(run_dir);
    if steps.is_empty() {
        return Own::None;
    }
    let reads = |step: &u64| {
        fs::read(path(run_dir, *step))
            .ok()
            .and_then(|b| bincode::deserialize::<Keyframe>(&b).ok())
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
struct Layer {
    dir: PathBuf,
    through: u64,
}

/// Where a run's keyframes are: its own directory, then the directory of
/// the run it shares keyframes with, and so on back to a run that shares
/// none. Runs that share keyframes sit side by side in one runs directory.
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

    pub fn load(&self, step: u64) -> Result<Keyframe> {
        let p = self.path(step).with_context(|| {
            format!(
                "run {} has no keyframe at step {step}",
                self.layers[0].dir.display()
            )
        })?;
        let bytes = fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        bincode::deserialize(&bytes).with_context(|| format!("reading {}", p.display()))
    }

    /// The chain of keyframes that rebuilds the one at `step`, full one
    /// first. The chain may cross from a run's own keyframes into shared
    /// ones. Each parent must be at an earlier step than the keyframe
    /// naming it, so a chain from an import that names itself or a later
    /// keyframe is refused rather than followed forever.
    pub fn chain(&self, step: u64) -> Result<Vec<Keyframe>> {
        let mut chain = vec![self.load(step)?];
        let mut at = step;
        while let Some(parent) = chain.last().unwrap().parent {
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

/// The page hashes a chain of keyframes has set guest memory to, where they
/// are not zero.
#[derive(Default)]
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

    fn apply(&mut self, parent: Option<u64>, pages: &[(u32, [u8; 32])]) {
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

/// Runs a machine to the end, taking keyframes as it goes. The first one
/// holds the pages written since the keyframe at `parent`, which the
/// machine was restored from or last took, or every page without one.
pub fn run_with_keyframes(
    machine: &mut Machine,
    obs: &mut dyn Observer,
    run_dir: &Path,
    store: &mut Store,
    mut parent: Option<u64>,
    memory: &mut Memory,
) -> Result<Outcome> {
    let mut interval = MIN_INTERVAL;
    loop {
        let started = Instant::now();
        let outcome = machine.run(Some(machine.step() + interval), obs)?;
        if let Outcome::Stopped(_) = outcome {
            return Ok(outcome);
        }
        let mut kf = machine.keyframe(&mut StorePages(store), parent)?;
        memory.trim(kf.parent, &mut kf.pages);

        // The pages first, durably, then the keyframe naming them.
        store.sync()?;
        save(run_dir, &kf)?;
        parent = Some(kf.step);

        // Aim the next stretch at the target wall time.
        let took = started.elapsed().max(Duration::from_micros(1));
        let scaled = interval as f64 * TARGET.as_secs_f64() / took.as_secs_f64();
        interval = (scaled as u64).clamp(MIN_INTERVAL, MAX_INTERVAL);
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
        let dir = runs.join(id);
        fs::create_dir_all(dir.join(DIR)).unwrap();
        for step in steps {
            fs::write(path(&dir, *step), b"").unwrap();
        }
        let manifest = serde_json::json!({ "id": id, "shared_keyframes": shared });
        fs::write(dir.join(crate::run::MANIFEST), manifest.to_string()).unwrap();
        dir
    }

    fn share(run: &str, through: u64) -> Option<Shared> {
        Some(Shared {
            run: run.into(),
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
    fn a_missing_parent_is_an_error_naming_it() {
        // A fork whose parent directory is gone cannot reach its shared
        // keyframes, and says which run it needs instead of panicking.
        let runs = runs("missing");
        let c = run(&runs, "c", &[3000], share("gone", 2400));
        let err = Layers::open(&c, share("gone", 2400).as_ref())
            .err()
            .expect("opening should fail");
        let message = format!("{err:#}");
        assert!(message.contains("gone"), "{message}");
        assert!(message.contains("2400"), "{message}");
    }
}
