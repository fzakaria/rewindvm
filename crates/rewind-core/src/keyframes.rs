//! A run's keyframes, and bringing a machine to any step of a run.
//!
//! Keyframes are taken while a run executes, at an interval chosen so each
//! stretch between two of them takes about [`TARGET`] of wall time to run.
//! Seeking to a step restores the latest keyframe at or before it and runs
//! forward from there, so no seek costs much more than one interval.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rewind_store::Store;
use rewind_vmm::snapshot::{Keyframe, Pages};
use rewind_vmm::{Machine, Observer, Outcome};

/// The wall time one stretch between keyframes aims for.
pub const TARGET: Duration = Duration::from_millis(250);

/// Bounds on the interval, in steps.
const MIN_INTERVAL: u64 = 256;
const MAX_INTERVAL: u64 = 1 << 20;

pub const DIR: &str = "keyframes";

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
    run_dir.join(DIR).join(format!("{step:016}.kf"))
}

pub fn save(run_dir: &Path, kf: &Keyframe) -> Result<()> {
    fs::create_dir_all(run_dir.join(DIR))?;
    let bytes = bincode::serialize(kf)?;
    crate::image::write_atomic(&path(run_dir, kf.step), &bytes)
}

pub fn load(run_dir: &Path, step: u64) -> Result<Keyframe> {
    let p = path(run_dir, step);
    let bytes = fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(bincode::deserialize(&bytes)?)
}

/// The steps a run has keyframes at, in order.
pub fn steps(run_dir: &Path) -> Vec<u64> {
    let mut steps: Vec<u64> = fs::read_dir(run_dir.join(DIR))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".kf"))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    steps.sort_unstable();
    steps
}

/// The chain of keyframes that rebuilds the one at `step`, full one first.
pub fn chain(run_dir: &Path, step: u64) -> Result<Vec<Keyframe>> {
    let mut chain = vec![load(run_dir, step)?];
    while let Some(parent) = chain.last().unwrap().parent {
        chain.push(load(run_dir, parent)?);
    }
    chain.reverse();
    Ok(chain)
}

/// Runs a machine to the end, taking keyframes as it goes.
pub fn run_with_keyframes(
    machine: &mut Machine,
    obs: &mut dyn Observer,
    run_dir: &Path,
    store: &mut Store,
) -> Result<Outcome> {
    let mut interval = MIN_INTERVAL;
    let mut parent = None;
    loop {
        let started = Instant::now();
        let outcome = machine.run(Some(machine.step() + interval), obs)?;
        if let Outcome::Stopped(_) = outcome {
            return Ok(outcome);
        }
        let kf = machine.keyframe(&mut StorePages(store), parent)?;
        save(run_dir, &kf)?;
        parent = Some(kf.step);

        // Aim the next stretch at the target wall time.
        let took = started.elapsed().max(Duration::from_micros(1));
        let scaled = interval as f64 * TARGET.as_secs_f64() / took.as_secs_f64();
        interval = (scaled as u64).clamp(MIN_INTERVAL, MAX_INTERVAL);
    }
}
