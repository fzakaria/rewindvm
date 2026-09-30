//! Runs: executing a machine and keeping what it did.
//!
//! A run is a directory holding `manifest.json`, which says exactly what
//! was run and how it ended, and `trace.bin`, every event with its step.
//! A run's id is the hash of its inputs, so running the same inputs twice
//! lands in the same directory, and replaying a run means running its
//! inputs again and checking the trace comes out the same.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rewind_init::{EXIT_MARK, Job};
use rewind_trace::{Event, EventKind, Trace, TraceWriter};
use rewind_vmm::{Config, Machine, Observer, Outcome, Stop};
use serde::{Deserialize, Serialize};

use crate::cpio;
use crate::home::Home;

pub const MANIFEST: &str = "manifest.json";
pub const TRACE: &str = "trace.bin";

/// The version of the manifest format.
const MANIFEST_VERSION: u32 = 1;

/// The kernel command line every guest boots with. Each argument keeps the
/// kernel from waiting on hardware time or probing hardware that is not
/// there.
pub const BASE_CMDLINE: &str = "nolapic_timer lpj=1000000 panic=-1 rdinit=/init quiet";

/// Nanoseconds of virtual time per exit.
pub const DEFAULT_QUANTUM: u64 = 1000;

/// The guest's wall clock at boot when nothing else is asked for: one
/// second past the epoch, as SOURCE_DATE_EPOCH is in nixpkgs.
pub const DEFAULT_EPOCH: u64 = 1;

/// Everything that determines a run. Two equal specs make equal runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// The input image and its BLAKE3 hash; the hash is what makes the
    /// run's id, since the path can be reused.
    pub image: Option<PathBuf>,
    pub image_hash: Option<String>,
    pub mem_mib: u64,
    pub seed: u64,
    pub epoch: u64,
    pub quantum: u64,
    pub cmdline: String,
    pub job: Job,
}

/// What kind of workload a run is, for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Source {
    /// A command in a root filesystem.
    Image { root: String },
    /// A Nix derivation.
    Nix { drv: String, outputs: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// How the machine stopped.
    pub stop: String,
    pub step: u64,
    pub virtual_ns: u64,
    /// The job's wait status, if init reported one.
    pub status: Option<i32>,
    pub wall_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub created: u64,
    pub source: Source,
    pub spec: Spec,
    /// The run this one was forked from, and the step it was forked at.
    pub parent: Option<(String, u64)>,
    pub outcome: Option<RunOutcome>,
}

impl Spec {
    /// The run's id: the BLAKE3 hash of every input, short enough to type.
    pub fn id(&self) -> String {
        let mut inputs = self.clone();
        // The image's path does not matter, only its contents.
        inputs.image = None;
        let bytes = serde_json::to_vec(&inputs).expect("a spec always serializes");
        let hash = blake3::hash(&bytes).to_hex();
        hash[..16].to_string()
    }

    /// The 32 bytes the guest kernel seeds its RNG with.
    pub fn rng_seed(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("rewind-vm guest rng seed");
        hasher.update(&self.seed.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// The machine config: the base initramfs with the job appended.
    pub fn config(&self) -> Result<Config> {
        let mut initrd =
            fs::read(&self.initrd).with_context(|| format!("reading {}", self.initrd.display()))?;
        initrd.extend_from_slice(&job_archive(&self.job)?);
        Ok(Config {
            kernel: self.kernel.clone(),
            initrd,
            image: self.image.clone(),
            mem_bytes: self.mem_mib << 20,
            cmdline: self.cmdline.clone(),
            seed: self.rng_seed(),
            epoch: self.epoch,
            quantum: self.quantum,
        })
    }
}

/// The per-run archive: the job itself, and any files it brings into
/// /build (Nix's passAsFile attributes).
fn job_archive(job: &Job) -> Result<Vec<u8>> {
    let mut a = cpio::Archive::new();
    a.dir("/rewind", 0o755);
    a.file(
        rewind_init::JOB_PATH,
        0o644,
        &serde_json::to_vec_pretty(job)?,
    );
    Ok(a.finish())
}

/// What happens to the guest's output while a run executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    /// Program output goes to the terminal as it arrives.
    Output,
    /// Nothing is printed.
    Quiet,
}

/// Collects the trace as it arrives and echoes output if asked.
struct Recorder {
    trace: TraceWriter<fs::File>,
    echo: Echo,
    status: Option<i32>,
    error: Option<anyhow::Error>,
}

impl Observer for Recorder {
    fn record(&mut self, step: u64, record: &[u8]) {
        if let Err(e) = self.trace.record(step, record) {
            self.error.get_or_insert(e.into());
        }
        let Ok(event) = Event::decode(step, record) else {
            return;
        };
        match &event.kind {
            EventKind::Output { fd, bytes } if self.echo == Echo::Output => {
                use std::io::Write;
                let _ = match fd {
                    2 => std::io::stderr().write_all(bytes),
                    _ => std::io::stdout().write_all(bytes),
                };
            }
            EventKind::Mark { text } => {
                if let Some(status) = text.strip_prefix(EXIT_MARK) {
                    self.status = status.trim().parse().ok();
                }
            }
            _ => {}
        }
    }
}

/// A run on disk.
pub struct Run {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Run {
    pub fn open(dir: &Path) -> Result<Run> {
        let path = dir.join(MANIFEST);
        let manifest = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Run {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    pub fn trace(&self) -> Result<Trace> {
        let path = self.dir.join(TRACE);
        Trace::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// Executes a spec, writing the run into the home's runs directory.
    pub fn execute(
        home: &Home,
        name: String,
        source: Source,
        spec: Spec,
        parent: Option<(String, u64)>,
        echo: Echo,
    ) -> Result<Run> {
        let id = spec.id();
        let dir = home.runs().join(&id);
        fs::create_dir_all(&dir)?;

        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let mut manifest = Manifest {
            version: MANIFEST_VERSION,
            id,
            name,
            created,
            source,
            spec,
            parent,
            outcome: None,
        };
        let write_manifest = |m: &Manifest| -> Result<()> {
            crate::image::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(m)?)
        };
        write_manifest(&manifest)?;

        let mut recorder = Recorder {
            trace: TraceWriter::new(fs::File::create(dir.join(TRACE))?),
            echo,
            status: None,
            error: None,
        };
        let start = Instant::now();
        let mut machine = Machine::boot(&manifest.spec.config()?)?;
        let outcome = machine.run(None, &mut recorder)?;
        let wall = start.elapsed();
        if let Some(e) = recorder.error {
            return Err(e.context("writing the trace"));
        }
        recorder.trace.finish()?;

        manifest.outcome = Some(RunOutcome {
            stop: describe(outcome),
            step: machine.step(),
            virtual_ns: machine.now(),
            status: recorder.status,
            wall_ms: wall.as_millis() as u64,
        });
        write_manifest(&manifest)?;
        Ok(Run { dir, manifest })
    }

    /// Runs the same inputs again and compares traces. None when the new
    /// trace is identical, which is the claim every run makes.
    pub fn replay(&self) -> Result<Option<rewind_trace::Divergence>> {
        let original = self.trace()?;
        let tmp = self.dir.join("replay.bin.tmp");
        let mut recorder = Recorder {
            trace: TraceWriter::new(fs::File::create(&tmp)?),
            echo: Echo::Quiet,
            status: None,
            error: None,
        };
        let mut machine = Machine::boot(&self.manifest.spec.config()?)?;
        machine.run(None, &mut recorder)?;
        recorder.trace.finish()?;
        let again = Trace::read(&tmp)?;
        fs::remove_file(&tmp)?;
        Ok(original.divergence(&again))
    }

    /// The runs in a home, newest first.
    pub fn list(home: &Home) -> Result<Vec<Run>> {
        let mut runs = Vec::new();
        let Ok(entries) = fs::read_dir(home.runs()) else {
            return Ok(runs);
        };
        for entry in entries {
            let dir = entry?.path();
            if let Ok(run) = Run::open(&dir) {
                runs.push(run);
            }
        }
        runs.sort_by_key(|r| std::cmp::Reverse(r.manifest.created));
        Ok(runs)
    }

    /// Finds a run by id prefix, name, or path.
    pub fn find(home: &Home, what: &str) -> Result<Run> {
        let path = Path::new(what);
        if path.join(MANIFEST).exists() {
            return Run::open(path);
        }
        let matches: Vec<Run> = Run::list(home)?
            .into_iter()
            .filter(|r| r.manifest.id.starts_with(what) || r.manifest.name == what)
            .collect();
        match matches.len() {
            0 => bail!("no run matches {what:?}; see `rewind ls`"),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => bail!("{n} runs match {what:?}; give more of the id"),
        }
    }
}

fn describe(outcome: Outcome) -> String {
    match outcome {
        Outcome::Paused => "paused".into(),
        Outcome::Stopped(Stop::Guest(exit)) => format!("{exit:?}").to_lowercase(),
        Outcome::Stopped(Stop::TripleFault) => "triple fault".into(),
        Outcome::Stopped(Stop::Stalled) => "stalled: idle with no timer armed".into(),
    }
}
