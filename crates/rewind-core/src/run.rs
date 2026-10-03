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

/// The CPUs the guest tells user space it has unless asked otherwise: the
/// VM's one vCPU.
pub const DEFAULT_CORES: u32 = 1;

/// The most CPUs the guest can tell user space of: its kernel's affinity
/// mask is one 64-bit word.
pub const MAX_CORES: u32 = 64;

/// The guest kernel's command line parameter for the CPU count.
const CORES_PARAM: &str = "rewind.cpus";

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
    /// Where the kernel's DWARF is, for `rewind gdb`: the kernel package's
    /// `debug` output, which may not be on this machine until it is
    /// fetched. Not an input: it is left out of the run's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_debug: Option<PathBuf>,
    /// The input image and its BLAKE3 hash; the hash is what makes the
    /// run's id, since the path can be reused.
    pub image: Option<PathBuf>,
    pub image_hash: Option<String>,
    pub mem_mib: u64,
    /// The CPUs the guest tells user space it has, through the affinity
    /// system calls, sysfs and /proc/cpuinfo, and a Nix build's
    /// NIX_BUILD_CORES. The VM has one vCPU whatever this is: threads
    /// sized by the count interleave on it.
    pub cores: u32,
    pub seed: u64,
    pub epoch: u64,
    pub quantum: u64,
    /// Perturbs where timer interrupts, and so preemptions, land; 0 for
    /// none.
    pub schedule: u64,
    /// The steps the schedule perturbation applies to, `from..until`.
    pub schedule_from: u64,
    pub schedule_until: u64,
    /// For a fork of a fork, the perturbations of the runs it came from,
    /// each over its window and all ending by `schedule_from`, so the fork
    /// is its parent up to its own step.
    pub inherited_schedules: Vec<ScheduleSegment>,
    /// The CPU the guest is shown.
    pub cpu: rewind_vmm::cpu::Model,
    /// What moves virtual time besides exits and idling.
    pub clock: rewind_vmm::ClockSource,
    /// Where a computing guest can be interrupted.
    pub preemption: rewind_vmm::Preemption,
    /// Whether the machine reserves the extras slot for `rewind shell
    /// --with`.
    pub extras: rewind_vmm::Extras,
    pub cmdline: String,
    pub job: Job,
}

/// One schedule seed over the steps `from..until`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSegment {
    pub seed: u64,
    pub from: u64,
    pub until: u64,
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
    /// Where the keyframes this run did not take itself are: another run's,
    /// up to the last step the two runs share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_keyframes: Option<crate::keyframes::Shared>,
    pub outcome: Option<RunOutcome>,
    /// The BLAKE3 hash of trace.bin in hex, once the run has finished. Runs
    /// with equal hashes did the same thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_hash: Option<String>,
    /// For a fork, the step where its trace first differs from its
    /// parent's; absent when the two are identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_difference: Option<u64>,
}

impl Spec {
    /// The run's id: the BLAKE3 hash of every input, short enough to type.
    /// Files count by their contents, not their paths, so a run keeps its
    /// id on another machine or after an import moves its inputs.
    pub fn id(&self) -> String {
        let mut inputs = self.clone();
        inputs.image = None;
        inputs.kernel_debug = None;
        let content =
            |p: &Path| crate::image::hash_file(p).unwrap_or_else(|_| p.display().to_string());
        inputs.kernel = PathBuf::from(content(&self.kernel));
        inputs.initrd = PathBuf::from(content(&self.initrd));
        let bytes = serde_json::to_vec(&inputs).expect("a spec always serializes");
        let hash = blake3::hash(&bytes).to_hex();
        hash[..16].to_string()
    }

    /// The spec of a fork of this run at `step` under schedule `seed`:
    /// this run's perturbations up to the step, the new one from it.
    pub fn fork(&self, step: u64, seed: u64) -> Spec {
        let own = ScheduleSegment {
            seed: self.schedule,
            from: self.schedule_from,
            until: self.schedule_until,
        };

        // Each earlier perturbation is cut off at the fork step, and one
        // that perturbs nothing before it is dropped.
        let inherited = self
            .inherited_schedules
            .iter()
            .chain(std::iter::once(&own))
            .map(|s| ScheduleSegment {
                until: s.until.min(step),
                ..s.clone()
            })
            .filter(|s| s.seed != 0 && s.from < s.until)
            .collect();
        Spec {
            schedule: seed,
            schedule_from: step,
            schedule_until: u64::MAX,
            inherited_schedules: inherited,
            ..self.clone()
        }
    }

    /// The perturbations the machine applies.
    fn schedule(&self) -> rewind_vmm::pv::Schedule {
        rewind_vmm::pv::Schedule {
            seed: self.schedule,
            window: self.schedule_from..self.schedule_until,
            earlier: self
                .inherited_schedules
                .iter()
                .map(|s| rewind_vmm::pv::Segment {
                    seed: s.seed,
                    window: s.from..s.until,
                })
                .collect(),
        }
    }

    /// The last step through which a run of this spec and a run of
    /// `other` are the same run: they differ only in their schedules, and
    /// the schedules perturb the same steps the same way up to it. None
    /// when anything else differs, u64::MAX when nothing does. A keyframe
    /// of one run at or before this step is a keyframe of the other.
    pub fn same_through(&self, other: &Spec) -> Option<u64> {
        // Everything but the schedule must match, compared the way the id
        // compares it.
        let unscheduled = |s: &Spec| Spec {
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            inherited_schedules: Vec::new(),
            image: None,
            kernel_debug: None,
            ..s.clone()
        };
        if unscheduled(self) != unscheduled(other) {
            return None;
        }
        Some(self.schedule().same_through(&other.schedule()))
    }

    /// The 32 bytes the guest kernel seeds its RNG with.
    pub fn rng_seed(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("rewind-vm guest rng seed");
        hasher.update(&self.seed.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// The kernel's command line: the spec's, then the CPU count.
    pub fn boot_cmdline(&self) -> String {
        format!("{} {CORES_PARAM}={}", self.cmdline, self.cores)
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
            cmdline: self.boot_cmdline(),
            seed: self.rng_seed(),
            epoch: self.epoch,
            quantum: self.quantum,
            schedule: self.schedule(),
            cpu: self.cpu,
            clock: self.clock,
            preemption: self.preemption,
            extras: self.extras,
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

/// Where a run starts executing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// At boot.
    Boot,
    /// As a fork of `parent` at `step`: the parent's run up to the step,
    /// from the parent's latest keyframe among the steps the two share.
    Fork { parent: String, step: u64 },
    /// At the latest keyframe of this run, by id, among the steps the two
    /// share, without being its fork: the runs `rewind check` makes match
    /// its unperturbed run up to where their schedules start.
    After(String),
}

/// How long a run may take on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeLimit {
    /// As long as it takes.
    None,
    /// Stop it after this much wall-clock time, however far it got. It
    /// then ends as [`TIMED_OUT`], which is the host's doing: the same run
    /// may finish on a faster host.
    Wall(std::time::Duration),
}

/// How to execute a run, none of which changes what the run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Execution {
    pub echo: Echo,
    pub keyframes: Keyframes,
    pub limit: TimeLimit,
}

/// How a run that reached its [`TimeLimit`] stopped, in its outcome.
pub const TIMED_OUT: &str = "timed out";

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

/// Passes a replay's records on to `inner` and compares each with the
/// next record the run made, keeping the step of the first that differs:
/// a record with other bytes, at another step, or that the run never made.
/// A replay that went another way reaches its step in a different machine.
struct Checked<'a> {
    inner: &'a mut dyn Observer,
    made: std::vec::IntoIter<(u64, Vec<u8>)>,
    differs_at: Option<u64>,
}

impl<'a> Checked<'a> {
    /// Checks against `made`, the run's records from where the replay
    /// starts.
    fn new(inner: &'a mut dyn Observer, made: Vec<(u64, Vec<u8>)>) -> Checked<'a> {
        Checked {
            inner,
            made: made.into_iter(),
            differs_at: None,
        }
    }
}

impl Observer for Checked<'_> {
    fn record(&mut self, step: u64, record: &[u8]) {
        self.inner.record(step, record);
        if self.differs_at.is_some() {
            return;
        }
        match self.made.next() {
            Some((made_step, made)) if made_step == step && made == record => {}
            _ => self.differs_at = Some(step),
        }
    }

    fn serial(&mut self, step: u64, byte: u8) {
        self.inner.serial(step, byte);
    }
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

/// The file in a run's directory that the process executing the run holds
/// a lock on. The kernel drops the lock when that process dies, however it
/// dies, so a run with no outcome and no lock held was interrupted.
const EXECUTING_LOCK: &str = "executing.lock";

/// Takes `dir`'s executing lock, held until the file is dropped. Refused
/// while another execution of the same run holds it.
fn lock_executing(dir: &Path) -> Result<fs::File> {
    let path = dir.join(EXECUTING_LOCK);
    let file = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    if file.try_lock().is_err() {
        bail!(
            "run {} is executing in another process",
            dir.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(file)
}

/// Whether a process is executing the run in `dir` now.
fn executing(dir: &Path) -> bool {
    let Ok(file) = fs::File::open(dir.join(EXECUTING_LOCK)) else {
        return false;
    };
    // A shared lock is refused only while an execution holds its own, and
    // goes when the file is dropped.
    file.try_lock_shared().is_err()
}

/// A run on disk.
pub struct Run {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Run {
    pub fn open(dir: &Path) -> Result<Run> {
        let path = dir.join(MANIFEST);

        // A manifest that does not parse is, most often, one another build
        // of rewind wrote with other fields.
        let manifest = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| {
            format!(
                "run {} was recorded by another build of rewind, and this one cannot read \
                 its manifest; record it again with this build to replay it",
                dir.display()
            )
        })?;
        Ok(Run {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    /// Whether a process is executing this run now. A run with no outcome
    /// that is not executing was interrupted.
    pub fn executing(&self) -> bool {
        executing(&self.dir)
    }

    pub fn trace(&self) -> Result<Trace> {
        let path = self.dir.join(TRACE);
        Trace::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// Executes a spec, writing the run into the home's runs directory. A
    /// fork of a run on this machine starts from the parent's latest
    /// keyframe in the steps the two share, and reads the parent's
    /// keyframes for those steps instead of taking its own. A run started
    /// after another starts the same way without being its fork.
    pub fn execute(
        home: &Home,
        name: String,
        source: Source,
        spec: Spec,
        start: Start,
        how: Execution,
    ) -> Result<Run> {
        let (parent, start_from) = match start {
            Start::Boot => (None, None),
            Start::Fork { parent, step } => (Some((parent, step)), None),
            Start::After(run) => (None, Some(run)),
        };
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            id: spec.id(),
            name,
            created: now(),
            source,
            spec,
            parent,
            shared_keyframes: None,
            outcome: None,
            trace_hash: None,
            first_difference: None,
        };
        Run::execute_manifest(home, manifest, start_from.as_deref(), how)
    }

    /// Executes the run `manifest` describes, filling in how it ended,
    /// starting from `start_from`'s keyframes or else its parent's.
    fn execute_manifest(
        home: &Home,
        mut manifest: Manifest,
        start_from: Option<&str>,
        how: Execution,
    ) -> Result<Run> {
        let Execution {
            echo,
            keyframes,
            limit,
        } = how;
        let dir = home.runs().join(&manifest.id);
        fs::create_dir_all(&dir)?;
        let _executing = lock_executing(&dir)?;
        let write_manifest = |m: &Manifest| -> Result<()> {
            crate::image::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(m)?)
        };

        // Keyframes from a finished earlier execution of the same run stay,
        // since other runs may read them, and this execution takes none.
        // Unreadable ones, from an older format, are replaced. Ones left by
        // an execution that never finished are as good as new ones, which
        // land beside them.
        let previous = Run::open(&dir).ok();
        let own = crate::keyframes::own_state(&dir);
        let keep = keyframes == Keyframes::Take
            && own == crate::keyframes::Own::Readable
            && previous
                .as_ref()
                .is_some_and(|p| p.manifest.outcome.is_some());
        if own == crate::keyframes::Own::Unreadable {
            fs::remove_dir_all(dir.join(crate::keyframes::DIR))?;
        }
        let shortcut = Shortcut::find(home, &manifest, start_from);

        // A run that takes no keyframes of its own replays from boot, so it
        // reads none of another run's either: starting from them only made
        // this execution shorter, and removing that run takes nothing from
        // this one.
        let keyframes = if keep {
            manifest.shared_keyframes = previous.and_then(|p| p.manifest.shared_keyframes);
            Keyframes::Skip
        } else {
            manifest.shared_keyframes = shortcut
                .as_ref()
                .filter(|_| keyframes == Keyframes::Take)
                .map(|s| s.shared.clone());
            keyframes
        };
        manifest.outcome = None;
        manifest.trace_hash = None;
        write_manifest(&manifest)?;

        let mut recorder = Recorder {
            trace: TraceWriter::new(fs::File::create(dir.join(TRACE))?),
            echo,
            status: None,
            error: None,
        };
        let start = Instant::now();
        let config = manifest.spec.config()?;
        let store = || rewind_store::Store::open(&home.store());

        // A run that shares its start with its parent replays the parent's
        // events up to the parent's keyframe and goes on from there. A run
        // that takes no keyframes needs the store only to restore, so it
        // closes it then.
        let (mut machine, mut store, last_keyframe) = match &shortcut {
            Some(s) => {
                for (step, record) in &s.prefix {
                    recorder.record(*step, record);
                }
                let store = store()?;
                let machine =
                    Machine::restore(&config, &s.chain, &crate::keyframes::ReadPages(&store))?;
                let store = (keyframes == Keyframes::Take).then_some(store);
                (machine, store, Some(s.base))
            }
            None => (Machine::boot(&config)?, None, None),
        };
        if let TimeLimit::Wall(limit) = limit {
            machine.set_deadline(Some(start + limit));
        }

        let outcome = match keyframes {
            Keyframes::Take => {
                let store = match &mut store {
                    Some(s) => s,
                    None => store.insert(rewind_store::Store::open(&home.store())?),
                };
                let mut parent = last_keyframe;
                let mut memory = shortcut
                    .as_ref()
                    .map(|s| crate::keyframes::Memory::of(&s.chain))
                    .unwrap_or_default();

                // A keyframe at the last shared step goes to the run that
                // owns it, where every fork made at this step finds it, and
                // this run's own keyframes hold only what it wrote after.
                if let Some(s) = shortcut.as_ref().filter(|s| s.base < s.shared.through)
                    && let Outcome::Paused = machine.run(Some(s.shared.through), &mut recorder)?
                {
                    let mut kf =
                        machine.keyframe(&mut crate::keyframes::StorePages(store), parent)?;
                    memory.trim(kf.parent, &mut kf.pages);
                    if crate::keyframes::save(&s.owner, &kf).is_err() {
                        crate::keyframes::save(&dir, &kf)?;
                    }
                    parent = Some(kf.step);
                }
                let outcome = crate::keyframes::run_with_keyframes(
                    &mut machine,
                    &mut recorder,
                    &dir,
                    store,
                    parent,
                    &mut memory,
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
            stop: describe(outcome, &manifest.spec.kernel),
            step: machine.step(),
            virtual_ns: machine.now(),
            status: recorder.status,
            wall_ms: wall.as_millis() as u64,
        });
        manifest.trace_hash = Some(crate::image::hash_file(&dir.join(TRACE))?);

        // Where a fork parts from its parent, while the parent is here to
        // compare with; otherwise what an earlier execution found stays.
        if let Some(parent) = manifest
            .parent
            .as_ref()
            .and_then(|(id, _)| Run::open(&home.runs().join(id)).ok())
        {
            let ours = Trace::read(&dir.join(TRACE))?;
            manifest.first_difference = parent.trace()?.divergence(&ours).map(|d| d.right_step);
        }
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

    /// Where the run's keyframes are, its own and the ones it shares.
    pub fn keyframes(&self) -> Result<crate::keyframes::Layers> {
        crate::keyframes::Layers::open(&self.dir, self.manifest.shared_keyframes.as_ref())
    }

    /// Whether the run has keyframes to seek from.
    pub fn has_keyframes(&self) -> bool {
        self.keyframes().is_ok_and(|k| !k.steps().is_empty())
    }

    /// Executes this run's spec again, taking keyframes. The run is the
    /// same, so its trace and manifest come out as they were. A run that
    /// timed out gets as long as it had, and may stop at another step.
    pub fn add_keyframes(&self, home: &Home) -> Result<Run> {
        let limit = match &self.manifest.outcome {
            Some(o) if o.stop.starts_with(TIMED_OUT) => {
                TimeLimit::Wall(std::time::Duration::from_millis(o.wall_ms))
            }
            _ => TimeLimit::None,
        };
        let manifest = Manifest {
            created: now(),
            ..self.manifest.clone()
        };
        let how = Execution {
            echo: Echo::Quiet,
            keyframes: Keyframes::Take,
            limit,
        };
        Run::execute_manifest(home, manifest, None, how)
    }

    /// A machine at `step` of this run: the latest keyframe at or before
    /// it, restored, and run forward to the step. Events on the way go to
    /// `obs`.
    pub fn machine_at(&self, home: &Home, step: u64, obs: &mut dyn Observer) -> Result<Machine> {
        let config = self.manifest.spec.config()?;
        let keyframes = self.keyframes()?;
        let from = keyframes.steps().into_iter().rfind(|s| *s <= step);
        let mut machine = match from {
            Some(kf) => {
                let store = rewind_store::Store::open(&home.store())?;
                let chain = keyframes.chain(kf)?;
                Machine::restore(&config, &chain, &crate::keyframes::ReadPages(&store))?
            }
            None => Machine::boot(&config)?,
        };

        // On the way to the step, the replay must make the records the run
        // made after where it starts. One that differs means this build of
        // rewind runs the inputs another way than the build that recorded
        // them, and the machine at the step is not the run's.
        let path = self.dir.join(TRACE);
        let made: Vec<(u64, Vec<u8>)> = rewind_trace::records(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .into_iter()
            .filter(|(s, _)| from.is_none_or(|kf| *s > kf))
            .collect();
        let mut checked = Checked::new(obs, made);
        machine.run(Some(step), &mut checked)?;
        if let Some(differs) = checked.differs_at {
            bail!(
                "replaying run {} went another way at step {differs} than when it was \
                 recorded, so this build of rewind runs its inputs differently from the \
                 build that recorded it; use that build, or record the run again with this one",
                self.manifest.id
            );
        }
        Ok(machine)
    }

    /// The records the run made after `step`, with their steps: what a
    /// fork at `step` should make again.
    pub fn records_after(&self, step: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let path = self.dir.join(TRACE);
        Ok(rewind_trace::records(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .into_iter()
            .filter(|(s, _)| *s > step)
            .collect())
    }

    /// Restores the keyframe at or before `step` and runs to the end.
    /// Returns the keyframe's step, the original trace after it, and the
    /// new one, which should be the same: the claim every keyframe makes.
    pub fn replay_from(&self, home: &Home, step: u64) -> Result<(u64, Trace, Trace)> {
        let original = self.trace()?;
        let kf = self
            .keyframes()?
            .steps()
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
            0 => Err(Run::unreadable(home, what)
                .unwrap_or_else(|| anyhow::anyhow!("no run matches {what:?}; see `rewind ls`"))),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => bail!("{n} runs match {what:?}; give more of the id"),
        }
    }

    /// Why a run whose id starts with `what` could not be opened, when one
    /// is there that `Run::list` left out.
    fn unreadable(home: &Home, what: &str) -> Option<anyhow::Error> {
        let dir = fs::read_dir(home.runs())
            .ok()?
            .filter_map(|e| Some(e.ok()?.path()))
            .find(|d| {
                d.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(what))
            })?;
        Run::open(&dir).err()
    }
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// How a run starts at the keyframe of a run it shares its start with, its
/// parent or one it was told to start from, rather than at boot.
struct Shortcut {
    /// The parent's keyframes this run reads: up to the last step the two
    /// runs share.
    shared: crate::keyframes::Shared,
    /// The parent's latest keyframe at or before that step, and the chain
    /// that restores it.
    base: u64,
    chain: Vec<rewind_vmm::snapshot::Keyframe>,
    /// The parent's trace up to the keyframe, record by record.
    prefix: Vec<(u64, Vec<u8>)>,
    /// The directory a keyframe at the last shared step belongs in.
    owner: PathBuf,
}

impl Shortcut {
    /// The shortcut for the run `manifest` describes, when `start_from`,
    /// or else its parent, is here, shares steps with it, and has a
    /// keyframe among them. Anything missing or unreadable means starting
    /// from boot, which is never wrong.
    fn find(home: &Home, manifest: &Manifest, start_from: Option<&str>) -> Option<Shortcut> {
        let parent_id = start_from.or(manifest.parent.as_ref().map(|(id, _)| id.as_str()))?;
        if *parent_id == manifest.id {
            return None;
        }
        let parent = Run::open(&home.runs().join(parent_id)).ok()?;

        // A parent still running has a trace that may lag its keyframes.
        parent.manifest.outcome.as_ref()?;
        let through = parent.manifest.spec.same_through(&manifest.spec)?;

        // The same spec is the same run, whose keyframes are its own.
        if through == u64::MAX {
            return None;
        }
        let layers = parent.keyframes().ok()?;
        let base = layers.steps().into_iter().rfind(|s| *s <= through)?;
        let chain = layers.chain(base).ok()?;
        let prefix = rewind_trace::records(&parent.dir.join(TRACE))
            .ok()?
            .into_iter()
            .take_while(|(step, _)| *step <= base)
            .collect();
        Some(Shortcut {
            shared: crate::keyframes::Shared {
                run: parent.manifest.id.clone(),
                through,
            },
            base,
            chain,
            prefix,
            owner: layers.owner(through).to_path_buf(),
        })
    }
}

/// A guest that went this long without an exit before its time limit was
/// computing, not making system calls, and where it was is worth saying.
const COMPUTING_AFTER: Duration = Duration::from_secs(1);

/// A timeout in words: still making exits, or computing without them for
/// how long and at which instruction, named by its kernel symbol if it was
/// in the kernel.
fn describe_timeout(stall: &rewind_vmm::Stall, kernel: &Path) -> String {
    if stall.since_exit < COMPUTING_AFTER {
        return format!("{TIMED_OUT} while still making exits");
    }
    let place = match stall.mode {
        rewind_vmm::CpuMode::User => format!("in user space at {:#x}", stall.rip),
        rewind_vmm::CpuMode::Kernel => {
            let map = kernel.with_file_name(SYSTEM_MAP);
            let symbol = fs::read_to_string(map)
                .ok()
                .and_then(|text| kernel_symbol(&text, stall.rip));
            format!(
                "in the kernel at {}",
                symbol.unwrap_or_else(|| format!("{:#x}", stall.rip))
            )
        }
    };
    format!(
        "{TIMED_OUT} computing without exits for {:.1}s, {place}",
        stall.since_exit.as_secs_f64()
    )
}

/// The kernel's symbol table, beside its bzImage.
const SYSTEM_MAP: &str = "System.map";

/// `addr` as `symbol+0xoffset`, from the System.map text `map`: the symbol
/// at the highest address not above it.
fn kernel_symbol(map: &str, addr: u64) -> Option<String> {
    map.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start = u64::from_str_radix(fields.next()?, 16).ok()?;
            let name = fields.nth(1)?;
            Some((start, name))
        })
        .filter(|(start, _)| *start <= addr)
        .max_by_key(|(start, _)| *start)
        .map(|(start, name)| format!("{name}+{:#x}", addr - start))
}

/// How a run stopped, in words. `kernel` is the guest kernel the run
/// booted, whose System.map names a kernel address a timeout stopped at.
fn describe(outcome: Outcome, kernel: &Path) -> String {
    match outcome {
        Outcome::Paused => "paused".into(),
        Outcome::Stopped(Stop::Guest(exit)) => format!("{exit:?}").to_lowercase(),
        Outcome::Stopped(Stop::TripleFault) => "triple fault".into(),
        Outcome::Stopped(Stop::Stalled) => "stalled: idle with no timer armed".into(),
        Outcome::Stopped(Stop::TimedOut(stall)) => describe_timeout(&stall, kernel),
        Outcome::Debug(stop) => format!("stopped by the debugger: {stop:?}"),
    }
}

#[cfg(test)]
mod tests {
    // Specs and manifests without running anything: how far two schedules
    // agree, which decides what a fork may share with its parent, and the
    // manifest fields the desktop app reads by name.
    use super::*;

    #[test]
    fn a_replay_that_goes_another_way_is_caught() {
        // Records fed to the check as a replay would make them: the same
        // ones pass, and the first that differs in bytes or step, or that
        // the run never made, is where the replay went another way.
        let made = vec![(3, vec![1u8]), (5, vec![2]), (9, vec![3])];
        let feed = |replayed: &[(u64, Vec<u8>)]| {
            let mut ignore = rewind_vmm::Ignore;
            let mut check = Checked::new(&mut ignore, made.clone());
            for (step, record) in replayed {
                check.record(*step, record);
            }
            check.differs_at
        };
        assert_eq!(feed(&made), None);
        assert_eq!(feed(&made[..2]), None);
        assert_eq!(feed(&[(3, vec![1]), (5, vec![7])]), Some(5));
        assert_eq!(feed(&[(3, vec![1]), (4, vec![2])]), Some(4));
        assert_eq!(
            feed(&[(3, vec![1]), (5, vec![2]), (9, vec![3]), (11, vec![4])]),
            Some(11)
        );
    }

    #[test]
    fn a_timeout_says_where_the_guest_was() {
        // A run stopped while it was still making exits says only that. One
        // stopped after a long stretch without exits says for how long and
        // where the vCPU was: a user-space address as it is, a kernel one
        // by its symbol in the System.map beside the kernel.
        use rewind_vmm::{CpuMode, Stall};
        let dir = std::env::temp_dir().join(format!("rewind-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("System.map"),
            "ffffffff81000000 T _text\nffffffff81285080 t rewind_clock_read\nffffffff812850c0 T rewind_yield\n",
        )
        .unwrap();
        let kernel = dir.join("bzImage");
        let timed_out = |rip, mode, since_exit_ms| {
            describe(
                Outcome::Stopped(Stop::TimedOut(Stall {
                    rip,
                    mode,
                    since_exit: std::time::Duration::from_millis(since_exit_ms),
                })),
                &kernel,
            )
        };

        assert_eq!(
            timed_out(0x55ce_d9ab_18a7, CpuMode::User, 12_300),
            "timed out computing without exits for 12.3s, in user space at 0x55ced9ab18a7"
        );
        assert_eq!(
            timed_out(0xffff_ffff_8128_5085, CpuMode::Kernel, 2_000),
            "timed out computing without exits for 2.0s, in the kernel at rewind_clock_read+0x5"
        );
        assert_eq!(
            timed_out(0xffff_ffff_8128_5085, CpuMode::Kernel, 3),
            "timed out while still making exits"
        );
        assert!(timed_out(0x1000, CpuMode::User, 3).starts_with(TIMED_OUT));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_is_executing_while_its_lock_is_held() {
        // The lock an execution takes marks the run as executing until it
        // is dropped, as it is when the process dies; a second execution
        // of the same run meanwhile is refused. A run dir without the lock
        // file was never executed by this build, and is not executing.
        let dir = std::env::temp_dir().join(format!("rewind-executing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(!executing(&dir));

        let held = lock_executing(&dir).unwrap();
        assert!(executing(&dir));
        assert!(lock_executing(&dir).is_err());
        drop(held);
        assert!(!executing(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An unperturbed spec; tests change the schedule fields.
    fn spec() -> Spec {
        Spec {
            kernel: "/k".into(),
            initrd: "/i".into(),
            kernel_debug: None,
            image: None,
            image_hash: None,
            mem_mib: 1024,
            cores: DEFAULT_CORES,
            seed: 0,
            epoch: 0,
            quantum: DEFAULT_QUANTUM,
            schedule: 0,
            schedule_from: 0,
            schedule_until: u64::MAX,
            inherited_schedules: Vec::new(),
            cpu: rewind_vmm::cpu::Model::V3,
            clock: rewind_vmm::ClockSource::Exits,
            preemption: rewind_vmm::Preemption::AtExits,
            extras: rewind_vmm::Extras::Reserved,
            cmdline: BASE_CMDLINE.into(),
            job: Job {
                argv: vec!["true".into()],
                program: None,

                env: Vec::new(),
                cwd: "/".into(),
                uid: 0,
                gid: 0,
                hostname: "localhost".into(),
                root: rewind_init::Root::Initramfs,
                files: Vec::new(),
                outputs: Vec::new(),
            },
        }
    }

    /// `spec()` perturbed by `seed` over `from..until`.
    fn perturbed(seed: u64, from: u64, until: u64) -> Spec {
        Spec {
            schedule: seed,
            schedule_from: from,
            schedule_until: until,
            ..spec()
        }
    }

    #[test]
    fn a_fork_agrees_with_an_unperturbed_parent_until_its_step() {
        // The perturbation can act at the fork step itself, so the last
        // shared step is the one before.
        let parent = spec();
        let fork = perturbed(3, 4855, u64::MAX);
        assert_eq!(parent.same_through(&fork), Some(4854));
        assert_eq!(fork.same_through(&parent), Some(4854));
    }

    #[test]
    fn the_kernel_is_told_the_cpu_count() {
        // The guest kernel reads how many CPUs to report to user space
        // from rewind.cpus= on its command line, after the spec's own.
        let spec = Spec { cores: 4, ..spec() };
        assert_eq!(spec.boot_cmdline(), format!("{BASE_CMDLINE} rewind.cpus=4"));
    }

    #[test]
    fn a_fork_of_an_unperturbed_run_inherits_nothing() {
        // A first-level fork has no earlier perturbation to carry, so its
        // spec is the parent's with only its own schedule and window set.
        let fork = spec().fork(4855, 10);
        assert_eq!(fork, perturbed(10, 4855, u64::MAX));
    }

    #[test]
    fn a_fork_of_a_fork_keeps_its_parents_perturbation_until_its_step() {
        // The parent was forked at 2000 with seed 3; its fork at 4000 with
        // seed 5 carries seed 3 over 2000..4000, so the two are the same
        // run through 3999.
        let parent = spec().fork(2000, 3);
        let child = parent.fork(4000, 5);
        assert_eq!(
            child.inherited_schedules,
            vec![ScheduleSegment {
                seed: 3,
                from: 2000,
                until: 4000
            }]
        );
        assert_eq!(parent.same_through(&child), Some(3999));
        let grandchild = child.fork(6000, 9);
        assert_eq!(grandchild.inherited_schedules.len(), 2);
        assert_eq!(child.same_through(&grandchild), Some(5999));
        assert_eq!(parent.same_through(&grandchild), Some(3999));
    }

    #[test]
    fn a_fork_before_its_parents_window_inherits_nothing() {
        // Forked at 1000, before the parent's perturbation starts at 2000,
        // the child has no part of it; a parent's narrowed window that ended
        // before the fork step is carried whole.
        let parent = spec().fork(2000, 3);
        let early = parent.fork(1000, 5);
        assert!(early.inherited_schedules.is_empty());
        assert_eq!(parent.same_through(&early), Some(999));
        let narrowed = perturbed(10, 4855, 4918).fork(5000, 2);
        assert_eq!(
            narrowed.inherited_schedules,
            vec![ScheduleSegment {
                seed: 10,
                from: 4855,
                until: 4918
            }]
        );
    }

    #[test]
    fn a_fork_of_a_fork_agrees_only_until_either_is_perturbed() {
        // A fork of a fork at a later step runs unperturbed until its own
        // step, so it parts from its parent where the parent's
        // perturbation started.
        let parent = perturbed(3, 2000, u64::MAX);
        let later = perturbed(5, 4000, u64::MAX);
        assert_eq!(parent.same_through(&later), Some(1999));
        let earlier = perturbed(5, 1000, u64::MAX);
        assert_eq!(parent.same_through(&earlier), Some(999));
    }

    #[test]
    fn one_seed_over_windows_with_one_start_agrees_until_one_ends() {
        // A seed's choices are a function of the step, so two windows that
        // start together agree until the shorter one ends.
        let narrow = perturbed(3, 2000, 2500);
        let wide = perturbed(3, 2000, u64::MAX);
        assert_eq!(narrow.same_through(&wide), Some(2499));
    }

    #[test]
    fn schedules_that_perturb_nothing_agree_everywhere() {
        // Seed 0 and an empty window are both the unperturbed run.
        assert_eq!(spec().same_through(&spec()), Some(u64::MAX));
        assert_eq!(spec().same_through(&perturbed(3, 10, 10)), Some(u64::MAX));
    }

    #[test]
    fn other_inputs_never_agree() {
        // A different RNG seed is a different run from boot.
        let other = Spec { seed: 1, ..spec() };
        assert_eq!(spec().same_through(&other), None);
    }

    #[test]
    fn the_app_reads_trace_hash_and_first_difference_by_name() {
        // The fields sit at the top level of manifest.json under exactly
        // these names, are left out when unknown, and a manifest without
        // them parses back to the same value.
        let mut m = Manifest {
            version: MANIFEST_VERSION,
            id: "c".into(),
            name: "fork".into(),
            created: 0,
            source: Source::Image { root: "/".into() },
            spec: spec(),
            parent: Some(("p".into(), 4855)),
            shared_keyframes: None,
            outcome: None,
            trace_hash: None,
            first_difference: None,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert!(json.get("trace_hash").is_none());
        assert!(json.get("first_difference").is_none());
        assert!(json.get("shared_keyframes").is_none());
        let old: Manifest = serde_json::from_value(json).unwrap();
        assert_eq!(old, m);

        m.trace_hash = Some("ab".into());
        m.first_difference = Some(4872);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["trace_hash"], "ab");
        assert_eq!(json["first_difference"], 4872);
    }
}
