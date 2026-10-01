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

/// The kernel command line every guest boots with. The first two keep the
/// kernel from waiting on hardware time. loglevel=7 sends informational
/// messages to the console, and so into the trace, the segfault report
/// with its instruction pointer among them: a message reaches the console
/// only when its level is below the loglevel. mitigations=off because the
/// guest's processes need no protection from each other, KVM still guards
/// the host, and a kernel that picks mitigations by the CPU's bugs would
/// take different code paths on different hosts; with the fixed CPU model
/// the mitigations it picked more than doubled a build's time.
pub const BASE_CMDLINE: &str =
    "nolapic_timer lpj=1000000 panic=-1 rdinit=/init loglevel=7 mitigations=off";

/// Nanoseconds of virtual time per exit: about what a system call and a
/// context switch cost on current hardware, which is what an exit stands
/// for.
pub const DEFAULT_QUANTUM: u64 = 5000;

/// The guest's wall clock at boot when nothing else is asked for: the
/// start of the current day, UTC. Builds compare the clock against the
/// timestamps in source tarballs, so it must be later than those, and
/// certificates in test suites expire, so it should not be far later. The
/// value chosen is part of the run's inputs, so a replay uses it again.
pub fn default_epoch() -> u64 {
    const DAY: u64 = 24 * 60 * 60;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    now - now % DAY
}

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
    /// Perturbs where timer interrupts, and so preemptions, land; 0 for
    /// none.
    #[serde(default)]
    pub schedule: u64,
    /// The steps the schedule perturbation applies to, `from..until`.
    #[serde(default)]
    pub schedule_from: u64,
    #[serde(default = "forever")]
    pub schedule_until: u64,
    /// The CPU the guest is shown. Runs from before this field showed the
    /// host's.
    #[serde(default = "host_cpu")]
    pub cpu: rewind_vmm::cpu::Model,
    /// What moves virtual time besides exits and idling.
    #[serde(default)]
    pub clock: rewind_vmm::ClockSource,
    /// Where a computing guest can be interrupted.
    #[serde(default)]
    pub preemption: rewind_vmm::Preemption,
    pub cmdline: String,
    pub job: Job,
}

fn forever() -> u64 {
    u64::MAX
}

fn host_cpu() -> rewind_vmm::cpu::Model {
    rewind_vmm::cpu::Model::Host
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
    /// Files count by their contents, not their paths, so a run keeps its
    /// id on another machine or after an import moves its inputs.
    pub fn id(&self) -> String {
        let mut inputs = self.clone();
        inputs.image = None;
        let content =
            |p: &Path| crate::image::hash_file(p).unwrap_or_else(|_| p.display().to_string());
        inputs.kernel = PathBuf::from(content(&self.kernel));
        inputs.initrd = PathBuf::from(content(&self.initrd));
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
            schedule: rewind_vmm::pv::Schedule {
                seed: self.schedule,
                window: self.schedule_from..self.schedule_until,
            },
            cpu: self.cpu,
            clock: self.clock,
            preemption: self.preemption,
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

/// Whether a run takes keyframes as it executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyframes {
    /// Take them, so any step can be reached quickly later.
    Take,
    /// Skip them: the run is only being compared. Running the same spec
    /// again with [`Keyframes::Take`] adds them, since it is the same run.
    Skip,
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
        keyframes: Keyframes,
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
        let outcome = match keyframes {
            Keyframes::Take => {
                // Keyframes left from an earlier execution of this spec are
                // replaced with this one's.
                let _ = fs::remove_dir_all(dir.join(crate::keyframes::DIR));
                let mut store = rewind_store::Store::open(&home.store())?;
                let outcome = crate::keyframes::run_with_keyframes(
                    &mut machine,
                    &mut recorder,
                    &dir,
                    &mut store,
                )?;
                store.sync()?;
                outcome
            }
            Keyframes::Skip => machine.run(None, &mut recorder)?,
        };

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

    /// Whether the run has keyframes to seek from.
    pub fn has_keyframes(&self) -> bool {
        !crate::keyframes::steps(&self.dir).is_empty()
    }

    /// Executes this run's spec again, taking keyframes. The run is the
    /// same, so its trace and manifest come out as they were.
    pub fn add_keyframes(&self, home: &Home) -> Result<Run> {
        let m = &self.manifest;
        Run::execute(
            home,
            m.name.clone(),
            m.source.clone(),
            m.spec.clone(),
            m.parent.clone(),
            Echo::Quiet,
            Keyframes::Take,
        )
    }

    /// A machine at `step` of this run: the latest keyframe at or before
    /// it, restored, and run forward to the step. Events on the way go to
    /// `obs`.
    pub fn machine_at(&self, home: &Home, step: u64, obs: &mut dyn Observer) -> Result<Machine> {
        let config = self.manifest.spec.config()?;
        let from = crate::keyframes::steps(&self.dir)
            .into_iter()
            .rfind(|s| *s <= step);
        let mut machine = match from {
            Some(kf) => {
                let store = rewind_store::Store::open(&home.store())?;
                let chain = crate::keyframes::chain(&self.dir, kf)?;
                Machine::restore(&config, &chain, &crate::keyframes::ReadPages(&store))?
            }
            None => Machine::boot(&config)?,
        };
        machine.run(Some(step), obs)?;
        Ok(machine)
    }

    /// Restores the keyframe at or before `step` and runs to the end.
    /// Returns the keyframe's step, the original trace after it, and the
    /// new one, which should be the same: the claim every keyframe makes.
    pub fn replay_from(&self, home: &Home, step: u64) -> Result<(u64, Trace, Trace)> {
        let original = self.trace()?;
        let kf = crate::keyframes::steps(&self.dir)
            .into_iter()
            .rfind(|s| *s <= step)
            .context("the run has no keyframe at or before that step")?;
        let tmp = self.dir.join("replay-from.bin.tmp");
        let mut recorder = Recorder {
            trace: TraceWriter::new(fs::File::create(&tmp)?),
            echo: Echo::Quiet,
            status: None,
            error: None,
        };
        let mut machine = self.machine_at(home, kf, &mut recorder)?;
        machine.run(None, &mut recorder)?;
        recorder.trace.finish()?;
        let again = Trace::read(&tmp)?;
        fs::remove_file(&tmp)?;
        let suffix = Trace {
            events: original.events[original.index_after(kf)..].to_vec(),
        };
        Ok((kf, suffix, again))
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

    /// Finds a run by path, name, or id prefix. A run named exactly `what`
    /// wins over ids that start with it, so a run named `a` is found even
    /// when another run's id starts with an `a`.
    pub fn find(home: &Home, what: &str) -> Result<Run> {
        let path = Path::new(what);
        if path.join(MANIFEST).exists() {
            return Run::open(path);
        }
        let (named, others): (Vec<Run>, Vec<Run>) = Run::list(home)?
            .into_iter()
            .partition(|r| r.manifest.name == what);
        let matches: Vec<Run> = if named.is_empty() {
            others
                .into_iter()
                .filter(|r| r.manifest.id.starts_with(what))
                .collect()
        } else {
            named
        };
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
        Outcome::Debug(stop) => format!("stopped by the debugger: {stop:?}"),
    }
}
