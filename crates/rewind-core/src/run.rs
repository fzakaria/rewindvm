//! Runs: executing a machine and keeping what it did.
//!
//! A run is a directory holding `manifest.json`, which says exactly what
//! was run and how it ended, and `trace.bin`, every event with its step.
//! A run's id is the hash of its inputs, so running the same inputs twice
//! lands in the same directory, and replaying a run means running its
//! inputs again and checking the trace comes out the same.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rewind_init::Job;
use rewind_trace::schedule::Timer;
use rewind_trace::stop::StalledThread;
use rewind_trace::{Event, EventKind, Trace, TraceWriter};
use rewind_vmm::{Config, Machine, Observer, Outcome, Stop};
use serde::{Deserialize, Serialize};

use crate::cpio;
use crate::home::Home;

pub use rewind_trace::manifest::{
    BOOKMARKS, EXECUTING_LOCK, ID_LEN, MANIFEST, MANIFEST_VERSION,
    MANIFEST_VERSION_BEFORE_STALL_MAX, Manifest, Parent, RunId, RunOutcome, ScheduleSegment,
    Source, Spec, TRACE, executing,
};

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

/// What only the engine does with a spec: hash it into a run id, build
/// the machine it describes, and tell how far two specs run alike.
pub trait SpecExt {
    /// The run's id: the BLAKE3 hash of every input, short enough to type.
    /// Files count by their contents, not their paths, so a run keeps its
    /// id on another machine or after an import moves its inputs.
    fn id(&self) -> RunId;

    /// The 32 bytes the guest kernel seeds its RNG with.
    fn rng_seed(&self) -> [u8; 32];

    /// The kernel's command line: the spec's, then the CPU count.
    fn boot_cmdline(&self) -> String;

    /// The machine config: the base initramfs with the job appended.
    fn config(&self) -> Result<Config>;
}

impl SpecExt for Spec {
    fn id(&self) -> RunId {
        let mut inputs = self.clone();
        inputs.image = None;
        inputs.kernel_debug = None;
        let content =
            |p: &Path| crate::image::hash_file(p).unwrap_or_else(|_| p.display().to_string());
        inputs.kernel = PathBuf::from(content(&self.kernel));
        inputs.initrd = PathBuf::from(content(&self.initrd));
        let bytes = serde_json::to_vec(&inputs).expect("a spec always serializes");
        let hash = blake3::hash(&bytes).to_hex();
        RunId::of_hash(&hash).expect("a BLAKE3 hash in hex is longer than a run id")
    }

    fn rng_seed(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("rewind-vm guest rng seed");
        hasher.update(&self.seed.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    fn boot_cmdline(&self) -> String {
        format!("{} {CORES_PARAM}={}", self.cmdline, self.cores)
    }

    fn config(&self) -> Result<Config> {
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
    Fork { parent: RunId, step: u64 },
    /// At the latest keyframe of this run, by id, among the steps the two
    /// share, without being its fork: the runs `rewind check` makes match
    /// its unperturbed run up to where their schedules start.
    After(RunId),
}

/// How long a run may take on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeLimit {
    /// As long as it takes.
    None,
    /// Stop it after this much wall-clock time, however far it got. It
    /// then ends as [`Stop::TimedOut`], which is the host's doing: the same
    /// run may finish on a faster host.
    Wall(Duration),
}

/// How to execute a run, none of which changes what the run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Execution {
    pub echo: Echo,
    pub keyframes: Keyframes,
    pub limit: TimeLimit,
}

/// Whether a run takes keyframes as it executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyframes {
    /// Take them, so any step can be reached quickly later.
    Take,
    /// Skip them: the run is only being compared. Running the same spec
    /// again with [`Keyframes::Take`] adds them, since it is the same run.
    Skip,
    /// Take one only, of every page, where init reports the job's exit:
    /// the run is only being compared, by files it left that are read
    /// there afterwards, as `rewind check`'s junit and script properties
    /// read them. Reading them then restores that keyframe instead of
    /// replaying the run from boot.
    AtExit,
}

/// What happens to the guest's output while a run executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    /// Program output goes to the terminal as it arrives.
    Output,
    /// Nothing is printed.
    Quiet,
    /// No program output, but a status line on standard error with the
    /// step reached and the time taken, for a terminal that would
    /// otherwise show nothing while a long run executes.
    Progress,
}

/// How often the status line is drawn again, at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// What a terminal is sent to go back to the start of the line and clear
/// it, so the status line is drawn over itself and then taken away.
pub const CLEAR_LINE: &str = "\r\x1b[K";

/// The status line of an execution with `Echo::Progress`.
struct ProgressLine {
    started: Instant,
    drawn: Option<Instant>,
}

impl ProgressLine {
    fn new(started: Instant) -> ProgressLine {
        ProgressLine {
            started,
            drawn: None,
        }
    }

    /// The line to draw at `step` and `now`, or None when one was drawn
    /// less than PROGRESS_EVERY ago.
    fn at(&mut self, step: u64, now: Instant) -> Option<String> {
        if self
            .drawn
            .is_some_and(|drawn| now.duration_since(drawn) < PROGRESS_EVERY)
        {
            return None;
        }
        self.drawn = Some(now);
        let taken = now.duration_since(self.started).as_secs();
        Some(format!("rewind: step {step}, {taken}s"))
    }
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

    /// A replay that went another way stops there: what it does after is
    /// not the run's, and running on to the step asked for would only
    /// delay saying so.
    fn stop(&self) -> bool {
        self.differs_at.is_some() || self.inner.stop()
    }
}

/// Collects the trace as it arrives and echoes output if asked.
struct Recorder {
    trace: TraceWriter<fs::File>,
    echo: Echo,
    progress: ProgressLine,
    status: Option<i32>,
    error: Option<anyhow::Error>,
    /// Whether the machine pauses once init reports the job's exit.
    pause_at_exit: bool,
}

impl Observer for Recorder {
    fn record(&mut self, step: u64, record: &[u8]) {
        if let Err(e) = self.trace.record(step, record) {
            self.error.get_or_insert(e.into());
        }
        if self.echo == Echo::Progress
            && let Some(line) = self.progress.at(step, Instant::now())
        {
            eprint!("{CLEAR_LINE}{line}");
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
            EventKind::Mark { .. } => {
                if let Some(rewind_init::Mark::Exit { status }) = event.init_mark() {
                    self.status = Some(status);
                }
            }
            _ => {}
        }
    }

    fn stop(&self) -> bool {
        self.pause_at_exit && self.status.is_some()
    }
}

/// At which of a run's keyframes a replay compares the whole machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareMachine {
    /// At every keyframe after where the replay starts.
    Every,
    /// Only at the last: the replay takes no capture before it, so it
    /// runs as the recording would have without keyframes, which shows
    /// that taking them changes nothing the guest sees.
    Last,
    /// Nowhere: only the events and records are compared.
    Never,
}

/// A replay of a run, as [`Run::replay_machine`] made it.
#[derive(Debug)]
pub struct Replayed {
    /// The step of the keyframe it started from, or None from boot.
    pub keyframe: Option<u64>,
    /// The run's events from where the replay started, and the replay's.
    pub original: Trace,
    pub again: Trace,
    /// Where the replay first differed from the run, events or read and
    /// write records; None when it made the same records.
    pub divergence: Option<rewind_trace::Divergence>,
    /// What comparing the whole machine at the run's keyframes found.
    pub machine: crate::state::Compared,
}

/// A trace an execution writes beside the run's recorded one. The new
/// trace takes the recording's place only through `keep`, once the
/// execution has finished; dropped before then, as when the execution
/// fails, it is removed and the recording stays as it was.
struct NewTrace {
    path: PathBuf,
    tmp: PathBuf,
    kept: bool,
}

impl NewTrace {
    /// A new trace for the run in `dir`, and the file to write it to. The
    /// caller holds the run's executing lock, so a new trace already here
    /// is one an execution that was killed left, and goes.
    fn create(dir: &Path) -> Result<(NewTrace, fs::File)> {
        let path = dir.join(TRACE);
        for entry in fs::read_dir(dir)? {
            let left = entry?.path();
            if crate::image::is_temp_beside(&path, &left) {
                fs::remove_file(&left)?;
            }
        }

        let tmp = crate::image::temp_beside(&path);
        let file = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let new = NewTrace {
            path,
            tmp,
            kept: false,
        };
        Ok((new, file))
    }

    /// Where the new trace is until it is kept.
    fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// Puts the new trace in the recording's place.
    fn keep(mut self) -> Result<()> {
        fs::rename(&self.tmp, &self.path)
            .with_context(|| format!("renaming {} into place", self.tmp.display()))?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for NewTrace {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// What a replay's trace is named after in the run's directory, before
/// `temp_beside` makes the name its own.
const REPLAY_TRACE: &str = "replay.bin";

/// The file in a run's directory that says a replay found the run's
/// machine different from its recording at a keyframe.
const MACHINE_DIFFERS: &str = "machine-differs.json";

/// What [`MACHINE_DIFFERS`] holds: the step of the keyframe that differed,
/// and of the last one before it that matched.
#[derive(Debug, Serialize, Deserialize)]
struct MachineMark {
    step: u64,
    matched: Option<u64>,
}

/// A trace a replay writes to compare with the recording. Each replay
/// gets its own file, so replays of one run can run at once, and the
/// file is removed when the replay drops it, whether it finished or
/// failed.
struct ReplayTrace {
    path: PathBuf,
}

impl ReplayTrace {
    /// A new replay trace in the run directory `dir`, and the file to
    /// write it to.
    fn create(dir: &Path) -> Result<(ReplayTrace, fs::File)> {
        let path = crate::image::temp_beside(&dir.join(REPLAY_TRACE));
        let file =
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok((ReplayTrace { path }, file))
    }

    /// Where the replay's trace is.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ReplayTrace {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The recording an execution kept the keyframes of: its trace's hash and
/// the build of rewind that recorded it.
struct Kept<'a> {
    hash: &'a str,
    recorded_by: &'a str,
}

/// Whether a new execution of run `id` by the build `this_build`, its
/// trace hashed `new`, may replace the run's recording. An execution that
/// kept an earlier one's keyframes may replace it only with the same
/// trace: those keyframes are of the earlier execution, and seeking with
/// them in another trace goes another way. A different trace from the
/// build that made the recording means the run is not deterministic on
/// this host; from another build, the run or the builds may be the cause.
fn replaces(id: &RunId, kept: Option<Kept>, new: &str, this_build: &str) -> Result<()> {
    let Some(kept) = kept else {
        return Ok(());
    };
    if kept.hash == new {
        return Ok(());
    }
    let cause = another_way_cause(kept.recorded_by, this_build);
    let next = match Builds::of(kept.recorded_by, this_build) {
        Builds::Same => another_way_next(id, kept.recorded_by, this_build),
        Builds::Other => "Remove the run to record it with this build".to_string(),
    };
    bail!(
        "executing run {id}'s inputs again made a different trace than its recording: \
         {cause}. The recording and its keyframes stay as they were. {next}"
    )
}

/// Whether a new execution of a run keeps the keyframes of the recording
/// that ended as `previous`, and so must make the same trace. A recording
/// stopped at its wall-clock limit does not bind it: where it stopped was
/// the host's doing, and this execution may go further. Its keyframes are
/// of the same steps this one makes, and this one's land beside them.
fn keeps_recording(previous: Option<&RunOutcome>) -> bool {
    previous.is_some_and(|p| p.stop.timeout().is_none())
}

/// How long taking a run's executing lock waits out a hold before refusing.
/// A process spawned while the lock is held keeps a copy of it until it
/// starts its program, a moment after the holder gave it up; an execution
/// holds it far longer, and is still refused.
const LOCK_PATIENCE: Duration = Duration::from_millis(250);
const LOCK_RETRY: Duration = Duration::from_millis(1);

/// Takes `dir`'s executing lock, held until the file is dropped. Refused
/// while another execution of the same run holds it.
pub(crate) fn lock_executing(dir: &Path) -> Result<fs::File> {
    let path = dir.join(EXECUTING_LOCK);
    let file =
        rewind_store::open_lock(&path).with_context(|| format!("creating {}", path.display()))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_PATIENCE => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(fs::TryLockError::WouldBlock) => bail!(
                "run {} is executing in another process",
                dir.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// Whether bringing a machine to a step of a run keeps a keyframe there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Keep one when the keyframe restored is far enough back, for a fork
    /// that looks inside the run, which is often followed by more forks
    /// at the same step or near it.
    Keyframe,
    Nothing,
}

/// What the next keyframe a replay keeps is a delta from: the keyframe
/// the machine was restored from or last kept, or boot when None; the
/// memory that keyframe holds; and the page store, open from the restore
/// or the first keyframe kept until the replay is done.
struct Base {
    step: Option<u64>,
    /// Memory as of that keyframe, built from the machine's restored
    /// pages the first time a keyframe is kept: a map of every page,
    /// 0.36 s for a 3 GB guest, which most forks never need.
    memory: Option<crate::keyframes::Memory>,
    store: Option<rewind_store::Store>,
}

/// A run on disk.
#[derive(Debug)]
pub struct Run {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// The trace file's records and the trace decoded from them, read the
    /// first time either is asked for. A Run is opened on a finished run,
    /// or made when its execution has finished writing the file, so this
    /// process never sees the file change; another process executing the
    /// run meanwhile is seen as of the first read.
    records: OnceLock<Vec<(u64, Vec<u8>)>>,
    trace: OnceLock<Trace>,
}

impl Run {
    pub fn open(dir: &Path) -> Result<Run> {
        let path = dir.join(MANIFEST);

        // A manifest that does not read is, most often, one another build
        // of rewind wrote, in another format or with other fields.
        let manifest = Manifest::parse(
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
            records: OnceLock::new(),
            trace: OnceLock::new(),
        })
    }

    /// The run's last step: where the machine stopped, for a finished run,
    /// else its last event.
    pub fn last_step(&self) -> Result<u64> {
        match &self.manifest.outcome {
            Some(o) => Ok(o.step),
            None => Ok(self.trace()?.last_step()),
        }
    }

    /// `step`, when the run reached it. A step past the end is refused
    /// rather than taken as the end: a fork there would be the run again.
    pub fn check_step(&self, step: u64) -> Result<u64> {
        let last = self.last_step()?;
        if step > last {
            bail!(
                "run {} ends at step {last}, so step {step} is past its end",
                self.manifest.id
            );
        }
        Ok(step)
    }

    /// Whether a process is executing this run now. A run with no outcome
    /// that is not executing was interrupted.
    pub fn executing(&self) -> bool {
        executing(&self.dir)
    }

    /// The run's trace, read once.
    pub fn trace(&self) -> Result<&Trace> {
        if let Some(trace) = self.trace.get() {
            return Ok(trace);
        }
        let path = self.dir.join(TRACE);
        let trace = Trace::decode(self.records()?)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(self.trace.get_or_init(|| trace))
    }

    /// The step process `pid`'s memory began at by `step`: its last exec
    /// by then, else the step it was forked on. Before then its memory was
    /// another program's, so a search back for who wrote it stops there.
    pub fn process_began(&self, pid: u32, step: u64) -> Result<u64> {
        let process = self
            .trace()?
            .process_at(pid, step)
            .filter(|p| p.start <= step)
            .with_context(|| format!("the run has no process {pid} by step {step}"))?;
        Ok(process.memory_began(step))
    }

    /// The run's read and write records, decoded from its trace. A run
    /// whose guest kernel buffered none, as every run recorded before
    /// rewind made them, has an empty log.
    pub fn io(&self) -> Result<rewind_trace::io_records::IoLog> {
        let path = self.dir.join(TRACE);
        rewind_trace::io_records::decode(self.records()?)
            .with_context(|| format!("reading the read and write records of {}", path.display()))
    }

    /// The run's records as the guest wrote them, each with its step,
    /// read once.
    pub fn records(&self) -> Result<&[(u64, Vec<u8>)]> {
        if let Some(records) = self.records.get() {
            return Ok(records);
        }
        let path = self.dir.join(TRACE);
        let records =
            rewind_trace::records(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(self.records.get_or_init(|| records))
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
            Start::Fork { parent, step } => (Some(Parent { run: parent, step }), None),
            Start::After(run) => (None, Some(run)),
        };
        let manifest = Manifest {
            version: spec.manifest_version(),
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
            recorded_by: crate::VERSION.to_string(),
        };
        Run::execute_manifest(home, manifest, start_from.as_ref(), how)
    }

    /// Executes the run `manifest` describes, filling in how it ended,
    /// starting from `start_from`'s keyframes or else its parent's.
    fn execute_manifest(
        home: &Home,
        mut manifest: Manifest,
        start_from: Option<&RunId>,
        how: Execution,
    ) -> Result<Run> {
        let Execution {
            echo,
            keyframes,
            limit,
        } = how;
        let dir = home.runs().join(&manifest.id);

        // The machine is built before anything of the run is written: a
        // run whose config does not read, or whose machine this host
        // cannot boot or restore, as when it lacks the recorded CPU model
        // or a keyframe's pages, never started, and leaves no directory to
        // be listed as interrupted. Building reads only the spec, the run
        // it starts from and the page store, none of which the run's
        // executing lock guards, so a second execution of the same run is
        // still refused below, only after building its machine.
        let start = Instant::now();
        let config = manifest.spec.config()?;
        let shortcut = Shortcut::find(home, &manifest, start_from);

        // A run that shares its start with its parent is restored at the
        // parent's keyframe, and replays the parent's events up to there
        // into its trace further down. The restored machine's read and
        // write records go on from that same prefix of the parent's trace.
        // The store a restore opened stays open until this execution knows
        // whether it takes keyframes.
        let (mut machine, mut store, last_keyframe) = match &shortcut {
            Some(s) => {
                let store = rewind_store::Store::open(&home.store())?;
                let machine = Machine::restore(
                    &config,
                    &s.chain,
                    &crate::keyframes::ReadPages(&store),
                    &s.prefix,
                )?;
                (machine, Some(store), Some(s.base))
            }
            None => (Machine::boot(&config)?, None, None),
        };

        fs::create_dir_all(&dir)?;
        let _executing = lock_executing(&dir)?;
        let write_manifest = |m: &Manifest| -> Result<()> {
            crate::image::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(m)?)
        };

        // Keyframes a finished earlier execution of the same run took as
        // it executed stay, since other runs may read them, and this
        // execution takes none. Unreadable ones, from an older format, are
        // replaced. Ones left by an execution that never finished, or kept
        // where something looked inside the run, are as good as new ones,
        // which land beside them.
        let previous = Run::open(&dir).ok();
        let own = crate::keyframes::own_state(&dir);
        let keep = keyframes == Keyframes::Take
            && own == crate::keyframes::Own::Readable
            && crate::keyframes::taken(&dir)
            && keeps_recording(previous.as_ref().and_then(|p| p.manifest.outcome.as_ref()));
        if own == crate::keyframes::Own::Unreadable {
            fs::remove_dir_all(dir.join(crate::keyframes::DIR))?;
        }

        // A finished earlier execution's recording, trace and manifest,
        // stays in place until this execution finishes and replaces it. When
        // this one keeps that one's keyframes, its trace must hash the same.
        let recorded = previous
            .as_ref()
            .is_some_and(|p| p.manifest.outcome.is_some());
        let kept_by = previous.as_ref().filter(|_| keep).and_then(|p| {
            Some((
                p.manifest.trace_hash.clone()?,
                p.manifest.recorded_by.clone(),
            ))
        });

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

        // A run that takes no keyframes needed the store only to restore,
        // so the store closes now.
        if keyframes != Keyframes::Take {
            store = None;
        }

        // A run executing for the first time, or again after an execution
        // that never finished, is listed while it executes, with no outcome
        // yet. A run with a recording goes on showing that recording.
        manifest.outcome = None;
        manifest.trace_hash = None;
        if !recorded {
            write_manifest(&manifest)?;
        }

        let (new_trace, trace_file) = NewTrace::create(&dir)?;
        let mut recorder = Recorder {
            trace: TraceWriter::new(trace_file),
            echo,
            progress: ProgressLine::new(start),
            status: None,
            error: None,
            pause_at_exit: false,
        };

        // A run restored at its parent's keyframe begins its trace with
        // the parent's events up to the keyframe.
        if let Some(s) = &shortcut {
            for (step, record) in &s.prefix {
                recorder.record(*step, record);
            }
        }
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

                    // The pages first, durably, then the keyframe naming
                    // them.
                    store.sync()?;
                    if crate::keyframes::save(&s.owner, &kf).is_err() {
                        crate::keyframes::save(&dir, &kf)?;
                    }
                    parent = Some(kf.step);
                }

                // Keyframes spaced by the guest's work, and one more where
                // init reports the job's exit, so a replay compares the
                // whole machine after the job ran even when the job was too
                // short for a second keyframe by its work. A run whose job
                // exited before the shared step has passed its exit.
                let mut spacing = crate::keyframes::Spacing::default();
                recorder.pause_at_exit = recorder.status.is_none();
                let mut outcome = crate::keyframes::run_with_keyframes(
                    &mut machine,
                    &mut recorder,
                    &dir,
                    store,
                    &mut parent,
                    &mut spacing,
                    &mut memory,
                )?;
                if let Outcome::Paused = outcome {
                    recorder.pause_at_exit = false;
                    if parent != Some(machine.step()) {
                        crate::keyframes::take(
                            &mut machine,
                            &dir,
                            store,
                            &mut parent,
                            &mut memory,
                        )?;
                        spacing.kept();
                    }
                    outcome = crate::keyframes::run_with_keyframes(
                        &mut machine,
                        &mut recorder,
                        &dir,
                        store,
                        &mut parent,
                        &mut spacing,
                        &mut memory,
                    )?;
                }
                store.sync()?;
                crate::keyframes::mark_taken(&dir)?;
                outcome
            }
            Keyframes::Skip => machine.run(None, &mut recorder)?,

            // To the step where init reports the job's exit, a keyframe
            // there holding every page, and on to the end as the run would
            // have gone. A run that stops first takes none.
            Keyframes::AtExit => {
                recorder.pause_at_exit = true;
                match machine.run(None, &mut recorder)? {
                    Outcome::Paused => {
                        recorder.pause_at_exit = false;
                        let store = match &mut store {
                            Some(s) => s,
                            None => store.insert(rewind_store::Store::open(&home.store())?),
                        };
                        let kf =
                            machine.keyframe(&mut crate::keyframes::StorePages(store), None)?;

                        // The pages first, durably, then the keyframe naming
                        // them.
                        store.sync()?;
                        crate::keyframes::save(&dir, &kf)?;
                        machine.run(None, &mut recorder)?
                    }
                    stopped => stopped,
                }
            }
        };

        let wall = start.elapsed();
        if echo == Echo::Progress {
            eprint!("{CLEAR_LINE}");
        }
        if let Some(e) = recorder.error {
            return Err(e.context("writing the trace"));
        }
        recorder.trace.finish()?.sync_all()?;

        // How the machine stopped, with the thread a run computing in user
        // space stopped in, read from the VM's kernel while the machine is
        // here.
        manifest.outcome = Some(RunOutcome {
            stop: stop_of(outcome, &manifest.spec.kernel, || thread_on_cpu(&machine))?,
            step: machine.step(),
            virtual_ns: machine.now(),
            status: recorder.status,
            wall_ms: wall.as_millis() as u64,
        });

        // The new trace replaces the recording, if it may.
        let trace_hash = crate::image::hash_file(new_trace.tmp())?;
        let kept = kept_by
            .as_ref()
            .map(|(hash, recorded_by)| Kept { hash, recorded_by });
        replaces(&manifest.id, kept, &trace_hash, crate::VERSION)?;
        new_trace.keep()?;

        // What a replay found of the recording this one replaces is not
        // this one's.
        if !keep {
            let _ = fs::remove_file(dir.join(MACHINE_DIFFERS));
        }
        manifest.trace_hash = Some(trace_hash);

        // Where a fork parts from its parent, while the parent is here to
        // compare with; otherwise what an earlier execution found stays.
        if let Some(parent) = manifest
            .parent
            .as_ref()
            .and_then(|p| Run::open(&home.runs().join(&p.run)).ok())
        {
            let ours = Trace::read(&dir.join(TRACE))?;
            manifest.first_difference = parent.trace()?.divergence(&ours).map(|d| d.right_step);
        }
        write_manifest(&manifest)?;
        Ok(Run {
            dir,
            manifest,
            records: OnceLock::new(),
            trace: OnceLock::new(),
        })
    }

    /// Runs the same inputs again and compares traces, events first and
    /// then read and write records. None when the new trace is identical,
    /// which is the claim every run makes.
    pub fn replay(&self) -> Result<Option<rewind_trace::Divergence>> {
        let original = self.trace()?;
        let (tmp, file) = ReplayTrace::create(&self.dir)?;
        let mut recorder = Recorder {
            trace: TraceWriter::new(file),
            echo: Echo::Quiet,
            progress: ProgressLine::new(Instant::now()),
            status: None,
            error: None,
            pause_at_exit: false,
        };
        let mut machine = Machine::boot(&self.manifest.spec.config()?)?;
        machine.run(self.replay_end(), &mut recorder)?;
        recorder.trace.finish()?;
        let records = rewind_trace::records(tmp.path())?;
        let again = Trace::decode(&records)?;
        Ok(original
            .divergence(&again)
            .or_else(|| rewind_trace::io_records::divergence(self.records().ok()?, &records)))
    }

    /// The step a replay of the run stops at: the step its recording
    /// stopped at when the wall-clock limit stopped it, since the guest
    /// went on only in the replay and the recording has nothing past that
    /// step to compare. None for a run whose guest stopped by itself.
    fn replay_end(&self) -> Option<u64> {
        self.manifest
            .outcome
            .as_ref()
            .filter(|o| o.stop.timeout().is_some())
            .map(|o| o.step)
    }

    /// Where the run's keyframes are, its own and the ones it shares.
    pub fn keyframes(&self) -> Result<crate::keyframes::Layers> {
        crate::keyframes::Layers::open(&self.dir, self.manifest.shared_keyframes.as_ref())
    }

    /// Whether the run has keyframes to seek from.
    pub fn has_keyframes(&self) -> bool {
        self.keyframes().is_ok_and(|k| !k.steps().is_empty())
    }

    /// Executes this run's spec again, taking keyframes, unless it has
    /// keyframes of its own already. The run is the same, so its trace and
    /// manifest come out as they were. A run that timed out gets as long as
    /// it had, and may stop at another step.
    pub fn add_keyframes(&self, home: &Home) -> Result<Run> {
        // A run that took readable keyframes as it executed has them
        // already, and executing it again would keep those and take none.
        // One with keyframes only where something looked inside it, as
        // `rewind check`'s runs have where their job exited, executes
        // again and takes the rest.
        if crate::keyframes::taken(&self.dir)
            && crate::keyframes::own_state(&self.dir) == crate::keyframes::Own::Readable
        {
            return Run::open(&self.dir);
        }

        let limit = match &self.manifest.outcome {
            Some(o) if o.stop.timeout().is_some() => {
                TimeLimit::Wall(Duration::from_millis(o.wall_ms))
            }
            _ => TimeLimit::None,
        };
        let manifest = Manifest {
            created: now(),
            recorded_by: crate::VERSION.to_string(),
            ..self.manifest.clone()
        };
        let how = Execution {
            echo: Echo::Quiet,
            keyframes: Keyframes::Take,
            limit,
        };
        Run::execute_manifest(home, manifest, None, how)
    }

    /// Whether the exit that made `step` armed the guest's timer, which
    /// the trace does not say: a schedule's late timer at that step changes
    /// the run only if it did. The run is brought to the step before and
    /// made to take that one exit, so the monitor sees it.
    pub fn armed_timer_at(&self, home: &Home, step: u64) -> Result<Timer> {
        let before = step.saturating_sub(1);
        let mut machine = self.machine_at(home, before, Keep::Nothing, &mut rewind_vmm::Ignore)?;
        machine.run(Some(step), &mut rewind_vmm::Ignore)?;
        Ok(if machine.timer_armed_at() == Some(step) {
            Timer::Armed
        } else {
            Timer::NotArmed
        })
    }

    /// What held the CPU at each step of `from..=to`, as the VM kernel's
    /// current_task names it, grouped into slices: the run is brought to
    /// `from` as a fork would be, then taken one step at a time. A window
    /// past the run's end stops there. The replay is checked against the
    /// run's records the whole way, as [`Run::machine_at`] checks it.
    ///
    /// Early in boot the kernel has not yet said where its tasks are, so
    /// a window that starts there starts at the first step it has.
    ///
    /// A step is an exit, so a thread that took and gave back the CPU
    /// between two exits is not seen.
    pub fn threads_through(
        &self,
        home: &Home,
        from: u64,
        to: u64,
    ) -> Result<Vec<crate::threads::Slice>> {
        if from > to {
            bail!("the window starts at step {from}, after its end at step {to}");
        }
        let to = to.min(self.last_step()?);
        let mut machine = self.machine_at(home, from, Keep::Nothing, &mut rewind_vmm::Ignore)?;

        // One step at a time, each checked against the run's records.
        let mut ignore = rewind_vmm::Ignore;
        let mut checked = Checked::new(&mut ignore, self.records_after(from)?);
        let mut step = from;

        // Through boot, until the kernel says where its tasks are.
        let layout = loop {
            if let Some(layout) = machine.task_layout()? {
                break layout;
            }
            let outcome = machine.run(Some(step + 1), &mut checked)?;
            if step == to || machine.step() != step + 1 || !matches!(outcome, Outcome::Paused) {
                bail!(
                    "run {}'s kernel does not say where its tasks are by step {step}",
                    self.manifest.id
                );
            }
            step += 1;
        };
        let mut held = Vec::new();
        loop {
            let (on, name) = crate::threads::Tasks::new(&machine, layout).held()?;
            held.push((step, on, name));
            if step == to {
                break;
            }

            // The last step is the one the guest stopped on, and a machine
            // that stopped short of the next step has none after it.
            let outcome = machine.run(Some(step + 1), &mut checked)?;
            if machine.step() != step + 1 {
                break;
            }
            step += 1;
            if !matches!(outcome, Outcome::Paused) {
                let (on, name) = crate::threads::Tasks::new(&machine, layout).held()?;
                held.push((step, on, name));
                break;
            }
        }
        if let Some(differs) = checked.differs_at {
            return Err(self.went_another_way(differs));
        }
        Ok(crate::threads::slices(held))
    }

    /// The error for a replay of this run that made a record other than
    /// the run's at step `differs`.
    fn went_another_way(&self, differs: u64) -> anyhow::Error {
        WentAnotherWay::new(&self.manifest.id, differs, &self.manifest.recorded_by).into()
    }

    /// Puts `machine`, restored from this run's keyframes, at the run's
    /// keyframe at `step`, in place (see [`Machine::restore_in_place`]),
    /// with keyframes read through `loaded` and pages from `store`. Much
    /// cheaper than a new machine there when the two keyframes are near.
    pub fn restore_in_place(
        &self,
        machine: &mut Machine,
        step: u64,
        store: &rewind_store::Store,
        loaded: &mut crate::keyframes::Loaded,
    ) -> Result<()> {
        let layers = self.keyframes()?;
        let chain = layers.chain_loaded(step, loaded)?;
        let records = self.records()?;
        let through = records.partition_point(|(s, _)| *s <= step);
        machine.restore_in_place(
            &chain,
            &crate::keyframes::ReadPages(store),
            &records[..through],
        )
    }

    /// A machine at `step` of this run: the latest keyframe at or before
    /// it, restored, and run forward to the step. Events on the way go to
    /// `obs`. With `Keep::Keyframe`, keyframes are kept on the way, each
    /// after [`crate::keyframes::KEEP_EVERY`] of the guest's work, so a
    /// later look anywhere on the stretch replays little of it, and one at
    /// the step when the last one is far enough back (see
    /// [`crate::keyframes::worth_keeping`]), so the next fork there
    /// restores it instead of replaying.
    pub fn machine_at(
        &self,
        home: &Home,
        step: u64,
        keep: Keep,
        obs: &mut dyn Observer,
    ) -> Result<Machine> {
        let config = self.manifest.spec.config()?;
        let keyframes = self.keyframes()?;
        let from = keyframes.steps().into_iter().rfind(|s| *s <= step);

        // The store stays open until a kept keyframe's pages are written
        // and the keyframe names them: `rewind gc` refuses while it is.
        let (mut machine, mut base) = match from {
            Some(kf) => {
                let store = rewind_store::Store::open(&home.store())?;
                let chain = keyframes.chain(kf)?;
                let records = self.records()?;
                let through = records.partition_point(|(s, _)| *s <= kf);
                let machine = Machine::restore(
                    &config,
                    &chain,
                    &crate::keyframes::ReadPages(&store),
                    &records[..through],
                )?;
                let base = Base {
                    step: Some(kf),
                    memory: None,
                    store: Some(store),
                };
                (machine, base)
            }
            None => {
                let base = Base {
                    step: None,
                    memory: Some(crate::keyframes::Memory::default()),
                    store: None,
                };
                (Machine::boot(&config)?, base)
            }
        };

        // On the way to the step, the replay must make the records the run
        // made after where it starts. One that differs means this build of
        // rewind runs the inputs another way than the build that recorded
        // them, and the machine at the step is not the run's.
        let made: Vec<(u64, Vec<u8>)> = self
            .records()?
            .iter()
            .filter(|(s, _)| from.is_none_or(|kf| *s > kf))
            .cloned()
            .collect();
        let mut checked = Checked::new(obs, made);

        // To the step in stretches, saying how far the replay got after
        // each, and keeping a keyframe at the end of one when enough
        // replay has passed since the last, when asked to.
        let mut pace = crate::keyframes::Pace::default();
        let mut reports = crate::progress::Reports::start(
            crate::progress::listener().map(|l| l.as_ref()),
            self.manifest.id.as_str(),
            (machine.step(), step),
            Instant::now(),
        );
        let outcome = loop {
            let at = crate::keyframes::Progress::of(&machine);
            let stop = pace.next_stop(at.step, step);
            let outcome = machine.run(Some(stop), &mut checked)?;
            pace.ran(at, crate::keyframes::Progress::of(&machine));
            if let Some(differs) = checked.differs_at {
                return Err(self.went_another_way(differs));
            }
            let paused = matches!(outcome, Outcome::Paused) && machine.step() == stop;
            if !paused || stop == step {
                reports.ended(machine.step(), Instant::now());
                break outcome;
            }
            reports.reached(stop, Instant::now());
            if keep == Keep::Keyframe
                && pace.keep_at(stop, step)
                && self.keep_keyframe(home, &mut machine, &mut base)?
            {
                pace.kept();
            }
        };

        // A keyframe at the step, when asked for, the machine got there,
        // and the keyframe it started from or last kept, or boot, is far
        // enough back.
        if keep == Keep::Nothing {
            return Ok(machine);
        }
        let reached = matches!(outcome, Outcome::Paused) && machine.step() == step;
        let last = base.step.map_or(Vec::new(), |s| vec![s]);
        if reached && crate::keyframes::worth_keeping(&last, step) {
            self.keep_keyframe(home, &mut machine, &mut base)?;
        }
        Ok(machine)
    }

    /// Saves a keyframe of `machine`, at a step of this run it reached
    /// from the keyframe `base` names, or from boot, in the directory of
    /// the run that owns the step, where every fork made there finds it
    /// (see [`crate::keyframes::Layers::owner`]). Its pages are the ones
    /// written since that keyframe, or from boot every non-zero page, put
    /// in the page store, which `base` opens if it has not yet. True when
    /// the keyframe was kept, and `base` then names it.
    ///
    /// The owner's executing lock is held meanwhile, so `rewind gc`, which
    /// refuses while a run is executing, does not collect the pages
    /// between their writing and the keyframe that names them, and two
    /// forks at nearby steps do not both keep one. Nothing is kept while
    /// another process holds the lock, executing the run or keeping a
    /// keyframe of its own, when a keyframe near the step appeared since
    /// this fork chose where to start, or when the owner cannot read the
    /// keyframe `base` names (see
    /// [`crate::keyframes::Layers::owner_reads`]). The machine's dirty log
    /// is read only for a keyframe that is kept, so the next delta from
    /// `base` still holds every page written since.
    ///
    /// A keyframe added to a run changes no fork's restored state. A fork
    /// sees a run's keyframes only up to the last step the two share,
    /// where the runs are the same machine, and each keyframe names the
    /// step of its parent, so the chains of keyframes already written
    /// resolve as before. Only the step a fork near the new keyframe
    /// replays from moves, as it does when an execution keeps the
    /// keyframe at the step where it parts from its parent.
    fn keep_keyframe(&self, home: &Home, machine: &mut Machine, base: &mut Base) -> Result<bool> {
        let step = machine.step();
        let layers = self.keyframes()?;
        let owner = layers.owner(step).to_path_buf();
        let Ok(_executing) = lock_executing(&owner) else {
            return Ok(false);
        };
        if !crate::keyframes::worth_keeping(&layers.steps(), step)
            || !layers.owner_reads(step, base.step)
        {
            return Ok(false);
        }

        // The pages first, durably, then the keyframe naming them.
        let store = match &mut base.store {
            Some(store) => store,
            None => base.store.insert(rewind_store::Store::open(&home.store())?),
        };
        let memory = base.memory.get_or_insert_with(|| {
            machine
                .restored_pages()
                .map(crate::keyframes::Memory::from_pages)
                .unwrap_or_default()
        });
        let mut kf = machine.keyframe(&mut crate::keyframes::StorePages(store), base.step)?;
        memory.trim(kf.parent, &mut kf.pages);
        store.sync()?;
        crate::keyframes::save(&owner, &kf)
            .with_context(|| format!("keeping a keyframe at step {step} in {}", owner.display()))?;
        base.step = Some(step);
        Ok(true)
    }

    /// The records the run made after `step`, with their steps: what a
    /// fork at `step` should make again.
    pub fn records_after(&self, step: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let records = self.records()?;
        let after = records.partition_point(|(s, _)| *s <= step);
        Ok(records[after..].to_vec())
    }

    /// Runs the same inputs again, from boot or from the keyframe at or
    /// before `from`, to where the recording ended, and compares: the
    /// events and read and write records with the run's, and at each
    /// keyframe of the run after the start the whole machine with the
    /// keyframe's (see [`crate::state`]). The claim every run and every
    /// keyframe makes is that both come out the same.
    pub fn replay_machine(
        &self,
        home: &Home,
        from: Option<u64>,
        compare: CompareMachine,
    ) -> Result<Replayed> {
        let original = self.trace()?;
        let layers = self.keyframes()?;
        let steps = layers.steps();
        let start = match from {
            None => None,
            Some(step) => Some(
                steps
                    .iter()
                    .copied()
                    .rfind(|s| *s <= step)
                    .context("the run has no keyframe at or before that step")?,
            ),
        };
        let (tmp, file) = ReplayTrace::create(&self.dir)?;
        let mut recorder = Recorder {
            trace: TraceWriter::new(file),
            echo: Echo::Quiet,
            progress: ProgressLine::new(Instant::now()),
            status: None,
            error: None,
            pause_at_exit: false,
        };
        let mut machine = match start {
            Some(kf) => self.machine_at(home, kf, Keep::Nothing, &mut recorder)?,
            None => Machine::boot(&self.manifest.spec.config()?)?,
        };

        // To each keyframe after the start, comparing the machine there,
        // then to the end. The recording's pages are read from the store
        // to show the bytes that differ, when it still has them.
        let store = rewind_store::Store::open(&home.store()).ok();
        let system_map =
            fs::read_to_string(self.manifest.spec.kernel.with_file_name(SYSTEM_MAP)).ok();
        let mut watch = crate::state::Watch::new(&layers, store.as_ref(), system_map, start)?;
        let after: Vec<u64> = steps
            .iter()
            .copied()
            .filter(|s| start.is_none_or(|kf| *s > kf))
            .collect();
        let at = match compare {
            CompareMachine::Every => after,
            CompareMachine::Last => after.last().copied().into_iter().collect(),
            CompareMachine::Never => Vec::new(),
        };
        for step in at {
            let outcome = machine.run(Some(step), &mut recorder)?;
            if !matches!(outcome, Outcome::Paused) || machine.step() != step {
                break;
            }
            watch.at_keyframe(&mut machine)?;
        }
        machine.run(self.replay_end(), &mut recorder)?;
        recorder.trace.finish()?;
        let records = rewind_trace::records(tmp.path())?;
        let again = Trace::decode(&records)?;
        let original = match start {
            Some(kf) => Trace {
                events: original.events[original.index_after(kf)..].to_vec(),
            },
            None => original.clone(),
        };

        // The events first, then the read and write records.
        let made = match start {
            Some(kf) => self.records_after(kf)?,
            None => self.records()?.to_vec(),
        };
        let divergence = original
            .divergence(&again)
            .or_else(|| rewind_trace::io_records::divergence(&made, &records));
        Ok(Replayed {
            keyframe: start,
            original,
            again,
            divergence,
            machine: watch.finish(),
        })
    }

    /// Notes in the run's directory that a replay found its machine
    /// different from the recording at the keyframe `difference` names,
    /// so a later look between the last keyframe that matched and that one
    /// warns (see [`Run::machine_warning`]).
    pub fn note_machine_difference(
        &self,
        difference: &crate::state::MachineDifference,
    ) -> Result<()> {
        let mark = MachineMark {
            step: difference.step,
            matched: difference.matched,
        };
        crate::image::write_atomic(
            &self.dir.join(MACHINE_DIFFERS),
            &serde_json::to_vec_pretty(&mark)?,
        )
    }

    /// What a look inside the run at `step` should warn of: a replay
    /// found the machine at a later keyframe different from the
    /// recording, and a look before that keyframe starts from an earlier
    /// one and replays the stretch where it changed, so it can show
    /// values the recording never had. None for a step no replay has
    /// found such a stretch around.
    pub fn machine_warning(&self, step: u64) -> Option<String> {
        let bytes = fs::read(self.dir.join(MACHINE_DIFFERS)).ok()?;
        let mark: MachineMark = serde_json::from_slice(&bytes).ok()?;
        let after = mark.matched.unwrap_or(0);
        if step <= after || step >= mark.step {
            return None;
        }
        let since = match mark.matched {
            Some(matched) => format!("matched at step {matched}"),
            None => "had matched at no keyframe before it".to_string(),
        };
        Some(format!(
            "a replay of run {} found its machine at the keyframe at step {} different from \
             the recording, and it {since}: step {step} is replayed through that stretch, and \
             what it shows there can be values the recording never had",
            self.manifest.id, mark.step
        ))
    }

    /// The runs in a home, newest first.
    pub fn list(home: &Home) -> Result<Vec<Run>> {
        Ok(Run::list_all(home)?.runs)
    }

    /// The runs in a home, newest first, and the directories beside them
    /// whose manifests do not read. A directory a process is executing a
    /// run into may not have its manifest yet, and is in neither.
    pub fn list_all(home: &Home) -> Result<Listing> {
        let mut listing = Listing::default();
        for dir in Run::dirs_starting(home, "")? {
            if !dir.is_dir() {
                continue;
            }
            match Run::open(&dir) {
                Ok(run) => listing.runs.push(run),
                Err(_) if executing(&dir) => {}
                Err(e) => listing.unreadable.push(Unreadable::of(&dir, &e)),
            }
        }
        listing
            .runs
            .sort_by_cached_key(|r| newest_first(r.manifest.created, &r.dir));
        listing
            .unreadable
            .sort_by_key(|u| std::cmp::Reverse(u.written));
        Ok(listing)
    }

    /// The directory whose manifest does not read that `what` names, by
    /// its id or the start of the one id it begins.
    pub fn find_unreadable(home: &Home, what: &str) -> Option<Unreadable> {
        let mut matches: Vec<Unreadable> = Run::list_all(home)
            .ok()?
            .unreadable
            .into_iter()
            .filter(|u| u.id.starts_with(what))
            .collect();
        if let Some(exact) = matches.iter().position(|u| u.id == what) {
            return Some(matches.swap_remove(exact));
        }
        match matches.len() {
            1 => matches.pop(),
            _ => None,
        }
    }

    /// Finds a run by path, id, `@`, name, or id prefix. A path to a run's
    /// directory, or a run's full id, opens that run without reading any
    /// other. `@` is the newest run, as `rewind ls` lists them first, and
    /// `@N` the Nth newest. Otherwise the newest run named exactly `what`
    /// wins over ids that start with it, so a run named `a` is found even
    /// when another run's id starts with an `a`; only `@` and names need
    /// every manifest read.
    pub fn find(home: &Home, what: &str) -> Result<Run> {
        let path = Path::new(what);
        if path.join(MANIFEST).exists() {
            return Run::open(path);
        }
        let dir = home.runs().join(what);
        let is_id = path.components().count() == 1 && dir.join(MANIFEST).exists();
        if is_id {
            return Run::open(&dir);
        }

        // `@` and `@N`: the runs in the order `rewind ls` prints them.
        if let Some(nth) = newest_index(what)? {
            let headers = Run::headers(home)?;
            let Some((dir, _)) = headers.get(nth - 1) else {
                bail!(
                    "there are {} runs, so {what} is past the oldest; see `rewind ls`",
                    headers.len()
                );
            };
            return Run::open(dir);
        }

        // The newest run named `what`.
        let named = Run::headers(home)?
            .into_iter()
            .find(|(_, header)| header.name == what);
        if let Some((dir, _)) = named {
            return Run::open(&dir);
        }

        // Else the runs whose id starts with `what`. Ones whose manifests
        // do not read are left out, as `Run::list` leaves them out.
        let mut matches: Vec<Run> = Run::dirs_starting(home, what)?
            .iter()
            .filter_map(|dir| Run::open(dir).ok())
            .collect();
        match matches.len() {
            0 => Err(Run::unreadable(home, what)
                .unwrap_or_else(|| anyhow::anyhow!("no run matches {what:?}; see `rewind ls`"))),
            1 => Ok(matches.remove(0)),
            n => {
                matches.sort_by_cached_key(|r| newest_first(r.manifest.created, &r.dir));
                let mut message = format!("{n} runs match {what:?}; give more of the id:");
                for run in matches.iter().take(CANDIDATES_SHOWN) {
                    message.push_str(&format!("\n  {}  {}", run.manifest.id, run.manifest.name));
                }
                if n > CANDIDATES_SHOWN {
                    message.push_str(&format!("\n  and {} more", n - CANDIDATES_SHOWN));
                }
                Err(anyhow::anyhow!(message))
            }
        }
    }

    /// Every readable run's directory and the parts of its manifest
    /// `find` matches on, newest first. Each manifest is read for those
    /// fields alone, which is faster than opening every run.
    fn headers(home: &Home) -> Result<Vec<(PathBuf, Header)>> {
        let mut headers = Vec::new();
        for dir in Run::dirs_starting(home, "")? {
            let Ok(bytes) = fs::read(dir.join(MANIFEST)) else {
                continue;
            };
            let Ok(header) = serde_json::from_slice::<Header>(&bytes) else {
                continue;
            };
            headers.push((dir, header));
        }
        headers.sort_by_cached_key(|(dir, header)| newest_first(header.created, dir));
        Ok(headers)
    }

    /// The directories in the home's runs directory whose names start with
    /// `prefix`, without reading what is in them.
    fn dirs_starting(home: &Home, prefix: &str) -> Result<Vec<PathBuf>> {
        let Ok(entries) = fs::read_dir(home.runs()) else {
            return Ok(Vec::new());
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(prefix) {
                continue;
            }
            dirs.push(entry.path());
        }
        Ok(dirs)
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

/// Every run in a home, and the directories beside them whose manifests
/// do not read.
#[derive(Default, Debug)]
pub struct Listing {
    pub runs: Vec<Run>,
    pub unreadable: Vec<Unreadable>,
}

/// A run directory whose manifest does not read, most often because
/// another build of rewind wrote it with other fields.
#[derive(Clone, Debug)]
pub struct Unreadable {
    /// The directory's name, which is the run's id.
    pub id: String,
    pub dir: PathBuf,
    /// Why, in the words of the manifest's parser or the file system.
    pub reason: String,
    /// When the directory last changed, in seconds since the Unix epoch.
    pub written: u64,
}

impl Unreadable {
    fn of(dir: &Path, error: &anyhow::Error) -> Unreadable {
        let written = fs::metadata(dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        Unreadable {
            id: dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            dir: dir.to_path_buf(),
            reason: error.root_cause().to_string(),
            written,
        }
    }
}

/// What [`Run::find`] reads of a manifest to order runs and match a name.
#[derive(Deserialize)]
struct Header {
    name: String,
    created: u64,
}

/// How many runs an ambiguous id prefix lists.
const CANDIDATES_SHOWN: usize = 5;

/// What a run argument starts with to name a run by how new it is.
const NEWEST: char = '@';

/// The order `rewind ls` lists runs in, as a sort key: the newest first,
/// and of runs made in the same second the one whose manifest was written
/// last, as when `rewind check` makes several at once.
fn newest_first(
    created: u64,
    dir: &Path,
) -> (std::cmp::Reverse<u64>, std::cmp::Reverse<SystemTime>) {
    let written = fs::metadata(dir.join(MANIFEST))
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    (std::cmp::Reverse(created), std::cmp::Reverse(written))
}

/// Which run, counting from the newest as 1, `@` (the newest) or `@N`
/// names; None when `what` is neither.
fn newest_index(what: &str) -> Result<Option<usize>> {
    let Some(count) = what.strip_prefix(NEWEST) else {
        return Ok(None);
    };
    if count.is_empty() {
        return Ok(Some(1));
    }
    if !count.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    match count.parse::<usize>() {
        Ok(nth) if nth >= 1 => Ok(Some(nth)),
        _ => bail!("{what} names no run: @ or @1 is the newest, @2 the one before it"),
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
    fn find(home: &Home, manifest: &Manifest, start_from: Option<&RunId>) -> Option<Shortcut> {
        let parent_id = start_from.or(manifest.parent.as_ref().map(|p| &p.run))?;
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

/// The thread on `machine`'s CPU, when its kernel says where its tasks
/// are.
fn thread_on_cpu(machine: &Machine) -> Option<StalledThread> {
    let layout = machine.task_layout().ok().flatten()?;
    let (pid, thread) = crate::threads::Tasks::new(machine, layout)
        .current_thread()
        .ok()?;
    Some(StalledThread {
        pid,
        tid: thread.tid,
        name: thread.name,
    })
}

/// The kernel's symbol table, beside its bzImage.
pub(crate) const SYSTEM_MAP: &str = "System.map";

/// `addr` as `symbol+0xoffset`, from the System.map text `map`: the symbol
/// at the highest address not above it.
pub(crate) fn kernel_symbol(map: &str, addr: u64) -> Option<String> {
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

/// A kernel address as `symbol+0xoffset`, from the System.map beside the
/// guest kernel `kernel`.
pub fn kernel_symbol_at(kernel: &Path, address: u64) -> Option<String> {
    let map = fs::read_to_string(kernel.with_file_name(SYSTEM_MAP)).ok()?;
    kernel_symbol(&map, address)
}

/// The address of the symbol `name` in the System.map text `map`.
pub(crate) fn kernel_symbol_address(map: &str, name: &str) -> Option<u64> {
    map.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let start = fields.next()?;
        (fields.nth(1)? == name).then(|| u64::from_str_radix(start, 16).ok())?
    })
}

/// How a run stopped, for its manifest. `kernel` is the guest kernel the
/// run booted, whose System.map names a kernel address a timeout stopped
/// at, and `thread` reads the thread on the CPU, asked only for a run that
/// timed out computing in user space.
fn stop_of(
    outcome: Outcome,
    kernel: &Path,
    thread: impl FnOnce() -> Option<StalledThread>,
) -> Result<rewind_trace::stop::Stop> {
    use rewind_trace::stop::{Doing, Stop as Stopped, Timeout};
    let Outcome::Stopped(stop) = outcome else {
        bail!("the machine did not stop: {outcome:?}");
    };
    Ok(match stop {
        Stop::Guest(rewind_vmm::pv::GuestExit::PowerOff) => Stopped::PoweredOff,
        Stop::Guest(rewind_vmm::pv::GuestExit::Restart) => Stopped::Restarted,
        Stop::Guest(rewind_vmm::pv::GuestExit::Halt) => Stopped::Halted,
        Stop::TripleFault => Stopped::TripleFault,
        Stop::Stalled => Stopped::Idle,
        Stop::TimedOut(stall) => {
            let doing = match stall.mode {
                _ if stall.since_exit < COMPUTING_AFTER => Doing::MakingExits,
                rewind_vmm::CpuMode::User => Doing::User {
                    rip: stall.rip,
                    thread: thread(),
                },
                rewind_vmm::CpuMode::Kernel => Doing::Kernel {
                    rip: stall.rip,
                    symbol: fs::read_to_string(kernel.with_file_name(SYSTEM_MAP))
                        .ok()
                        .and_then(|text| kernel_symbol(&text, stall.rip)),
                },
            };
            Stopped::TimedOut(Timeout {
                since_exit_ms: stall.since_exit.as_millis() as u64,
                doing,
            })
        }
    })
}

/// The build of rewind replaying a run, beside the one that recorded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Builds {
    /// The build that recorded the run: a replay that differs read
    /// something from the host.
    Same,
    /// Another build, which may run the same inputs another way.
    Other,
}

impl Builds {
    /// Whether `this_build` is the build `recorded_by` names.
    pub fn of(recorded_by: &str, this_build: &str) -> Builds {
        if recorded_by == this_build {
            return Builds::Same;
        }
        Builds::Other
    }
}

/// Why a replay by `this_build` of a run `recorded_by` recorded can
/// differ from the recording, for a person. The same build running the
/// same inputs another way means the run read something from this host
/// that its inputs do not decide. Under another build either the run or
/// the builds may be the cause, and only a replay by the recording build
/// tells which.
pub fn another_way_cause(recorded_by: &str, this_build: &str) -> String {
    match Builds::of(recorded_by, this_build) {
        Builds::Same => format!(
            "the same build of rewind, {this_build}, recorded the run, so the run is not \
             deterministic on this host: something it read came from the host rather than \
             from its inputs"
        ),
        Builds::Other => format!(
            "rewind {recorded_by} recorded the run, and this is rewind {this_build}; another \
             build may run the same inputs another way"
        ),
    }
}

/// What to do about a replay of run `id` that differs, for a person:
/// under the recording build, `rewind replay --bisect` finds the
/// instruction that read the host. Under another, --bisect would compare
/// two replays by this build, which agree when the builds are the cause,
/// so the reader is sent to the recording build instead.
pub fn another_way_next(id: &RunId, recorded_by: &str, this_build: &str) -> String {
    match Builds::of(recorded_by, this_build) {
        Builds::Same => format!(
            "`rewind replay {id} --bisect` finds the instruction that read it, and `rewind \
             bugreport {id}` gathers what a bug report needs"
        ),
        Builds::Other => format!(
            "Replay the run with rewind {recorded_by} to tell whether the run or the builds \
             differ, or record the run again with this one"
        ),
    }
}

/// A replay of a run that made a record other than its recording's: the
/// error the engine gives for it, which `rewind --errors json` reports by
/// its kind, so a program need not read its words.
#[derive(Debug)]
pub struct WentAnotherWay {
    pub run: RunId,
    /// The step the replay first made another record at.
    pub step: u64,
    words: String,
}

impl WentAnotherWay {
    /// The error for a replay by this build of run `run`, which
    /// `recorded_by` recorded, that went another way at `step`.
    pub fn new(run: &RunId, step: u64, recorded_by: &str) -> WentAnotherWay {
        WentAnotherWay {
            run: run.clone(),
            step,
            words: went_another_way_words(run, step, recorded_by, crate::VERSION),
        }
    }
}

impl std::fmt::Display for WentAnotherWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.words)
    }
}

impl std::error::Error for WentAnotherWay {}

/// Why a replay of run `id` made a record other than the run's at step
/// `differs`, by the build `this_build`, of a run `recorded_by` recorded,
/// and what to do about it.
pub fn went_another_way_words(
    id: &RunId,
    differs: u64,
    recorded_by: &str,
    this_build: &str,
) -> String {
    let went = rewind_trace::WENT_ANOTHER_WAY;
    format!(
        "replaying run {id} {went} {differs} than when it was recorded: {}. {}",
        another_way_cause(recorded_by, this_build),
        another_way_next(id, recorded_by, this_build)
    )
}

#[cfg(test)]
pub(crate) mod tests {
    // Specs and manifests without running anything: how far two schedules
    // agree, which decides what a fork may share with its parent, and the
    // manifest fields the desktop app reads by name.
    use super::*;
    use rewind_trace::schedule::StallMax;

    #[test]
    fn a_lock_given_up_is_taken_again_while_processes_are_spawned() {
        // A process another thread spawns holds a copy of every open file
        // until it starts its program, a held executing lock among them, so
        // the lock outlasts its File by a moment. Taking it again right
        // after giving it up waits that out rather than refusing, the way
        // an import of a run already here takes it twice.
        let dir = std::env::temp_dir().join(format!("rewind-relock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawning = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::process::Command::new("true").status().unwrap();
                }
            })
        };
        let refused = (0..20_000)
            .filter(|_| lock_executing(&dir).is_err())
            .count();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spawning.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(refused, 0);
    }

    #[test]
    fn a_guest_that_powered_off_stopped_cleanly() {
        // A guest that powered off is the clean stop the CLI and the app
        // recognize; a machine that never stopped has no stop to record.
        let stopped = Outcome::Stopped(Stop::Guest(rewind_vmm::pv::GuestExit::PowerOff));
        let stop = stop_of(stopped, Path::new("/k"), || None).unwrap();
        assert!(stop.is_clean());
        assert!(stop_of(Outcome::Paused, Path::new("/k"), || None).is_err());
    }

    #[test]
    fn a_status_line_is_drawn_a_few_times_a_second() {
        // The first record draws the line; one PROGRESS_EVERY later draws
        // it again with the step and the whole seconds taken; any sooner
        // draws nothing.
        let started = Instant::now();
        let mut line = ProgressLine::new(started);
        assert_eq!(line.at(10, started).as_deref(), Some("rewind: step 10, 0s"));
        assert_eq!(line.at(20, started + PROGRESS_EVERY / 2), None);
        let later = started + Duration::from_secs(3);
        assert_eq!(line.at(30, later).as_deref(), Some("rewind: step 30, 3s"));
    }

    #[test]
    fn a_new_trace_replaces_the_recording_only_when_kept() {
        // A run directory with a recorded trace, one a killed execution
        // left beside it, and a new trace written there: the killed one's
        // goes when the new one starts; the new one dropped, as when its
        // execution fails, goes too and the recording stays byte for byte;
        // kept, it takes the recording's place and nothing else is left in
        // the directory.
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("rewind-new-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(TRACE), b"recorded").unwrap();
        fs::write(crate::image::temp_beside(&dir.join(TRACE)), b"killed").unwrap();
        let names = || {
            let mut names: Vec<String> = fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        let (new, mut file) = NewTrace::create(&dir).unwrap();
        file.write_all(b"half").unwrap();
        drop(new);
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), b"recorded");
        assert_eq!(names(), [TRACE]);

        let (new, mut file) = NewTrace::create(&dir).unwrap();
        file.write_all(b"whole").unwrap();
        file.sync_all().unwrap();
        new.keep().unwrap();
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), b"whole");
        assert_eq!(names(), [TRACE]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replays_of_one_run_write_their_own_traces() {
        // Two replays of a run at once each get their own trace file, so
        // neither truncates or removes the other's, and each file goes
        // when its replay is done with it.
        let dir = std::env::temp_dir().join(format!("rewind-replay-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let (first, _) = ReplayTrace::create(&dir).unwrap();
        let (second, _) = ReplayTrace::create(&dir).unwrap();
        assert_ne!(first.path(), second.path());

        let (first_path, second_path) = (first.path().to_owned(), second.path().to_owned());
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_trace_that_differs_from_its_kept_keyframes_recording_is_refused() {
        // A new execution that kept the keyframes of an earlier one may
        // replace that one's trace only with the same trace; with none
        // kept, any trace replaces it. A different trace from the build
        // that recorded the run blames the host, and from another build
        // blames the builds.
        let id = id("a");
        let by = |recorded_by| Kept {
            hash: "aa",
            recorded_by,
        };
        assert!(replaces(&id, Some(by("0.9.0 (x)")), "aa", "0.9.0 (x)").is_ok());
        assert!(replaces(&id, None, "bb", "0.9.0 (x)").is_ok());
        let same = replaces(&id, Some(by("0.9.0 (x)")), "bb", "0.9.0 (x)")
            .unwrap_err()
            .to_string();
        assert!(same.contains("not deterministic on this host"), "{same}");
        assert!(same.contains("--bisect"), "{same}");
        let other = replaces(&id, Some(by("0.8.0 (y)")), "bb", "0.9.0 (x)")
            .unwrap_err()
            .to_string();
        assert!(
            other.contains("Remove the run to record it with this build"),
            "{other}"
        );
    }

    #[test]
    fn only_a_recording_that_ended_by_itself_binds_the_next_execution() {
        // A run executed again keeps an earlier recording's keyframes, and
        // must make the same trace, only when that recording ended by
        // itself. One that reached its wall-clock limit stopped where this
        // host's speed left it: the same run with another limit, as
        // `rewind check` gives schedule 0 after `rewind run --timeout`,
        // goes on further, and that is no sign the run is not
        // deterministic.
        let ended = |stop| RunOutcome {
            stop,
            step: 9,
            virtual_ns: 0,
            status: None,
            wall_ms: 0,
        };
        let timed_out = rewind_trace::stop::Stop::TimedOut(rewind_trace::stop::Timeout {
            since_exit_ms: 0,
            doing: rewind_trace::stop::Doing::MakingExits,
        });
        assert!(keeps_recording(Some(&ended(
            rewind_trace::stop::Stop::PoweredOff
        ))));
        assert!(!keeps_recording(Some(&ended(timed_out))));
        assert!(!keeps_recording(None));
    }

    #[test]
    fn a_replay_that_goes_another_way_is_caught() {
        // Records fed to the check as a replay would make them: the same
        // ones pass, and the first that differs in bytes or step, or that
        // the run never made, is where the replay went another way, and
        // where the check asks the machine to stop.
        let made = vec![(3, vec![1u8]), (5, vec![2]), (9, vec![3])];
        let feed = |replayed: &[(u64, Vec<u8>)]| {
            let mut ignore = rewind_vmm::Ignore;
            let mut check = Checked::new(&mut ignore, made.clone());
            for (step, record) in replayed {
                check.record(*step, record);
            }
            assert_eq!(check.stop(), check.differs_at.is_some());
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
        // where the vCPU was: a user-space address with the thread on the
        // CPU, a kernel one by its symbol in the System.map beside the
        // kernel.
        use rewind_trace::stop::{Doing, Stop as Stopped, Timeout};
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
        let worker = StalledThread {
            pid: 34,
            tid: 36,
            name: "worker".into(),
        };
        let timed_out = |rip, mode, since_exit_ms| {
            let stall = Stall {
                rip,
                mode,
                since_exit: Duration::from_millis(since_exit_ms),
            };
            stop_of(Outcome::Stopped(Stop::TimedOut(stall)), &kernel, || {
                Some(worker.clone())
            })
            .unwrap()
        };
        let timeout = |since_exit_ms, doing| {
            Stopped::TimedOut(Timeout {
                since_exit_ms,
                doing,
            })
        };

        assert_eq!(
            timed_out(0x55ce_d9ab_18a7, CpuMode::User, 12_300),
            timeout(
                12_300,
                Doing::User {
                    rip: 0x55ce_d9ab_18a7,
                    thread: Some(worker.clone()),
                }
            )
        );
        assert_eq!(
            timed_out(0xffff_ffff_8128_5085, CpuMode::Kernel, 2_000),
            timeout(
                2_000,
                Doing::Kernel {
                    rip: 0xffff_ffff_8128_5085,
                    symbol: Some("rewind_clock_read+0x5".into()),
                }
            )
        );
        assert_eq!(
            timed_out(0xffff_ffff_8128_5085, CpuMode::Kernel, 3),
            timeout(3, Doing::MakingExits)
        );
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

        // A program another test started may hold a copy of the lock until
        // it execs, so the run stops executing soon after the drop rather
        // than at once (see `crate::settle`).
        crate::settle::settle("the run to stop executing", || {
            (!executing(&dir)).then_some(())
        });
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An unperturbed spec; tests change the schedule fields.
    pub(crate) fn spec() -> Spec {
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
            stall_max: StallMax::default(),
            inherited_schedules: Vec::new(),
            reseeds: Vec::new(),
            cpu: rewind_vmm::CpuModel::V3,
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
                output: rewind_init::Output::Plain,
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
                until: 4000,
                stall_max: StallMax::default(),
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
                until: 4918,
                stall_max: StallMax::default(),
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
    fn runs_without_a_stall_max_keep_their_ids() {
        // The ids of an unperturbed run, a fork and a fork of a fork,
        // pinned as they were before a spec could carry a longest stall:
        // a spec that leaves the default serializes, and so hashes, to the
        // same bytes it always did.
        let fork = spec().fork(4855, 10);
        let fork_of_fork = fork.fork(6000, 3);
        assert_eq!(spec().id(), "9501ba2bef8ad19c");
        assert_eq!(fork.id(), "bce6e984a58c78bc");
        assert_eq!(fork_of_fork.id(), "687617f510a03fa4");
    }

    #[test]
    fn a_spec_without_a_stall_max_writes_back_unchanged() {
        // A spec at the default longest stall, a fork's included, writes
        // no stall_max_ns, reads back as itself and writes back as the
        // same bytes, and names the manifest format from before the field.
        let fork = spec().fork(4855, 10).fork(6000, 3);
        for s in [spec(), fork] {
            let bytes = serde_json::to_vec_pretty(&s).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("stall_max"));
            let read: Spec = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(read, s);
            assert_eq!(serde_json::to_vec_pretty(&read).unwrap(), bytes);
            assert_eq!(read.manifest_version(), MANIFEST_VERSION_BEFORE_STALL_MAX);
        }
    }

    #[test]
    fn a_spec_with_a_stall_max_round_trips_and_forks_inherit_it() {
        // A perturbed spec given a longest stall writes it in nanoseconds,
        // reads back as itself, has another id and the newer manifest
        // format. Schedule 0 never stalls, so an unperturbed spec stays at
        // the default. A fork's own window takes the default unless given
        // another, as its seed does, while the window it inherits keeps
        // its maximum, and two forks at one step under different maxima
        // part where their windows start.
        let max: StallMax = "50ms".parse().unwrap();
        let long = perturbed(3, 2000, u64::MAX).with_stall_max(max);
        let json = serde_json::to_value(&long).unwrap();
        assert_eq!(json["stall_max_ns"], 50_000_000);
        let read: Spec = serde_json::from_value(json).unwrap();
        assert_eq!(read, long);
        assert_ne!(long.id(), perturbed(3, 2000, u64::MAX).id());
        assert_eq!(long.manifest_version(), MANIFEST_VERSION);
        assert!(spec().with_stall_max(max).stall_max.is_default());

        // A fork inherits the parent's window with its maximum.
        let short = long.fork(4000, 5);
        assert!(short.stall_max.is_default());
        assert_eq!(short.inherited_schedules[0].stall_max, max);
        assert_eq!(short.manifest_version(), MANIFEST_VERSION);
        assert_eq!(long.same_through(&short), Some(3999));
        let json = serde_json::to_value(&short).unwrap();
        assert_eq!(json["inherited_schedules"][0]["stall_max_ns"], 50_000_000);
        assert_eq!(serde_json::from_value::<Spec>(json).unwrap(), short);

        // Forks at one step under two maxima part there.
        let also_long = long.fork(4000, 5).with_stall_max(max);
        assert_eq!(also_long.stall_max, max);
        assert_eq!(short.same_through(&also_long), Some(3999));
    }

    #[test]
    fn other_inputs_never_agree() {
        // A different RNG seed is a different run from boot.
        let other = Spec { seed: 1, ..spec() };
        assert_eq!(spec().same_through(&other), None);
    }

    /// The id a test gives the run it calls `name`: `name` itself when it
    /// is an id, else its bytes in hex, padded to an id's length.
    pub(crate) fn id(name: &str) -> RunId {
        RunId::parse(name).unwrap_or_else(|| {
            let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
            RunId::parse(&format!("{hex:0>width$}", width = ID_LEN))
                .expect("a test run's name is at most 8 bytes")
        })
    }

    /// A manifest for run `id` named `name`, made at `created`.
    pub(crate) fn manifest(id: &str, name: &str, created: u64) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            id: self::id(id),
            name: name.into(),
            created,
            source: Source::Image { root: "/".into() },
            spec: spec(),
            parent: None,
            shared_keyframes: None,
            outcome: None,
            trace_hash: None,
            first_difference: None,
            recorded_by: crate::VERSION.into(),
        }
    }

    /// A run directory under `runs` holding `manifest` and a trace of one
    /// write of `text` at each of `steps`.
    fn write_run(runs: &Path, manifest: &Manifest, steps: &[u64], text: &[u8]) -> PathBuf {
        let dir = runs.join(&manifest.id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST), serde_json::to_vec(manifest).unwrap()).unwrap();
        write_trace(&dir, steps, text);
        dir
    }

    /// Writes `dir`'s trace: one write of `text` at each of `steps`.
    fn write_trace(dir: &Path, steps: &[u64], text: &[u8]) {
        const OUTPUT: u16 = 2;
        const STDOUT: u32 = 1;
        let len = (rewind_trace::HEADER_LEN + text.len()) as u32;
        let mut record = Vec::new();
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&OUTPUT.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&7u32.to_le_bytes());
        record.extend_from_slice(&7u32.to_le_bytes());
        record.extend_from_slice(&STDOUT.to_le_bytes());
        record.extend_from_slice(text);
        let mut writer = TraceWriter::new(fs::File::create(dir.join(TRACE)).unwrap());
        for step in steps {
            writer.record(*step, &record).unwrap();
        }
        writer.finish().unwrap();
    }

    /// A fresh runs directory for one test.
    fn runs_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rewind-run-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A finished run in a fresh home named `name`, with a keyframe of its
    /// own at step 4, and the home.
    fn run_with_a_keyframe(name: &str) -> (PathBuf, Home, PathBuf, Manifest) {
        let root = runs_dir(name);
        let home = Home::at(root.clone()).unwrap();
        let mut m = manifest("abc", "r", 0);
        m.outcome = Some(RunOutcome {
            stop: rewind_trace::stop::Stop::PoweredOff,
            step: 9,
            virtual_ns: 0,
            status: Some(0),
            wall_ms: 0,
        });
        let dir = write_run(&home.runs(), &m, &[3, 5, 9], b"a");
        let mut kf = rewind_vmm::snapshot::Keyframe::default();
        kf.step = 4;
        crate::keyframes::save(&dir, &kf).unwrap();
        (root, home, dir, m)
    }

    #[test]
    fn keyframes_are_added_only_to_a_run_without_them() {
        // A finished run that took keyframes as it executed: adding
        // keyframes runs nothing, which this spec, with no kernel to boot,
        // would fail at, and leaves the run as it was.
        let (root, home, dir, m) = run_with_a_keyframe("add-keyframes");
        crate::keyframes::mark_taken(&dir).unwrap();
        let trace = fs::read(dir.join(TRACE)).unwrap();

        let run = Run::open(&dir).unwrap().add_keyframes(&home).unwrap();
        assert_eq!(run.manifest, m);
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), trace);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn keyframes_are_added_to_a_run_with_one_kept_where_it_was_looked_inside() {
        // A finished run whose one keyframe was kept where something looked
        // inside it, as at a check run's job exit: adding keyframes
        // executes the run again, which this spec, with no kernel to boot,
        // fails at, and the run's trace stays as it was.
        let (root, home, dir, _) = run_with_a_keyframe("add-keyframes-kept");
        let trace = fs::read(dir.join(TRACE)).unwrap();

        assert!(Run::open(&dir).unwrap().add_keyframes(&home).is_err());
        assert_eq!(fs::read(dir.join(TRACE)).unwrap(), trace);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_run_reads_its_trace_once() {
        // A run's trace file is read the first time trace(), records() or
        // records_after() needs it. Writes a trace of three events, reads
        // it, replaces the file with another, and checks the open Run
        // still answers from the first while a Run opened anew reads the
        // second.
        let runs = runs_dir("trace-once");
        let dir = write_run(&runs, &manifest("r", "r", 0), &[3, 5, 9], b"a");
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.trace().unwrap().events.len(), 3);
        assert_eq!(run.records_after(4).unwrap().len(), 2);

        write_trace(&dir, &[1], b"b");
        assert_eq!(run.trace().unwrap().events.len(), 3);
        assert_eq!(run.records().unwrap().len(), 3);
        assert_eq!(run.records_after(4).unwrap().len(), 2);
        assert_eq!(Run::open(&dir).unwrap().trace().unwrap().events.len(), 1);
        fs::remove_dir_all(&runs).unwrap();
    }

    #[test]
    fn a_run_is_found_by_path_id_prefix_or_name() {
        // find() over a home of runs: 8fd5 named mylib, 8f00 named 8f,
        // a1b2 named a, and b000 named with 8fd5's id. A path, a full id or a unique prefix finds
        // its run; a name finds its run over the ids it starts; a prefix
        // two ids share, or one nothing has, is an error. A full id finds
        // its own run even when b000 is named with it.
        let root = runs_dir("find");
        let home = Home::at(root.clone()).unwrap();
        let runs = home.runs();
        let id = |run: Result<Run>| run.unwrap().manifest.id;
        write_run(&runs, &manifest("8fd5378ddf70075e", "mylib", 1), &[3], b"a");
        write_run(&runs, &manifest("8f00aaaaaaaaaaaa", "8f", 2), &[3], b"a");
        write_run(&runs, &manifest("a1b2c3d4e5f60718", "a", 3), &[3], b"a");
        write_run(
            &runs,
            &manifest("b000000000000000", "8fd5378ddf70075e", 4),
            &[3],
            b"a",
        );

        let dir = runs.join("8fd5378ddf70075e");
        assert_eq!(
            id(Run::find(&home, dir.to_str().unwrap())),
            "8fd5378ddf70075e"
        );
        assert_eq!(id(Run::find(&home, "8fd5378ddf70075e")), "8fd5378ddf70075e");
        assert_eq!(id(Run::find(&home, "8fd5")), "8fd5378ddf70075e");
        assert_eq!(id(Run::find(&home, "mylib")), "8fd5378ddf70075e");
        assert_eq!(id(Run::find(&home, "8f")), "8f00aaaaaaaaaaaa");
        assert_eq!(id(Run::find(&home, "a")), "a1b2c3d4e5f60718");
        assert_eq!(id(Run::find(&home, "8f0")), "8f00aaaaaaaaaaaa");
        assert_eq!(
            Run::find(&home, "c").err().unwrap().to_string(),
            "no run matches \"c\"; see `rewind ls`"
        );
        write_run(&runs, &manifest("8f01bbbbbbbbbbbb", "other", 5), &[3], b"a");
        assert_eq!(
            Run::find(&home, "8f0").err().unwrap().to_string(),
            "2 runs match \"8f0\"; give more of the id:\n  \
             8f01bbbbbbbbbbbb  other\n  \
             8f00aaaaaaaaaaaa  8f"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_newest_runs_are_found_by_at_and_a_name_by_its_newest() {
        // find() over a home of three runs: two named mylib, made at 1 and
        // 3, and one named other, made at 2. `@` and `@1` are the newest,
        // `@2` and `@3` the ones before it, and past the oldest is an
        // error. A name two runs share finds the newer of them.
        let root = runs_dir("newest");
        let home = Home::at(root.clone()).unwrap();
        let runs = home.runs();
        let id = |what: &str| Run::find(&home, what).unwrap().manifest.id;
        write_run(&runs, &manifest("aaaa000000000000", "mylib", 1), &[3], b"a");
        write_run(&runs, &manifest("bbbb000000000000", "other", 2), &[3], b"a");
        write_run(&runs, &manifest("cccc000000000000", "mylib", 3), &[3], b"a");

        assert_eq!(id("@"), "cccc000000000000");
        assert_eq!(id("@1"), "cccc000000000000");
        assert_eq!(id("@2"), "bbbb000000000000");
        assert_eq!(id("@3"), "aaaa000000000000");
        assert_eq!(
            Run::find(&home, "@4").err().unwrap().to_string(),
            "there are 3 runs, so @4 is past the oldest; see `rewind ls`"
        );
        assert_eq!(
            Run::find(&home, "@0").err().unwrap().to_string(),
            "@0 names no run: @ or @1 is the newest, @2 the one before it"
        );
        assert_eq!(id("mylib"), "cccc000000000000");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn runs_whose_manifests_do_not_read_are_listed_apart() {
        // A home with one run, a directory whose manifest another build
        // wrote with other fields, and one with no manifest at all: the
        // run is listed, the two others are unreadable with the parser's
        // reason, and either is found by its id or a prefix of it. A
        // directory a process is executing into has no manifest yet and
        // is neither.
        let root = runs_dir("unreadable");
        let home = Home::at(root.clone()).unwrap();
        let runs = home.runs();
        write_run(&runs, &manifest("aaaa000000000000", "good", 1), &[3], b"a");
        fs::create_dir_all(runs.join("bbbb000000000000")).unwrap();
        fs::write(runs.join("bbbb000000000000").join(MANIFEST), b"{}").unwrap();
        fs::create_dir_all(runs.join("cccc000000000000")).unwrap();
        let making = runs.join("dddd000000000000");
        fs::create_dir_all(&making).unwrap();
        let _executing = lock_executing(&making).unwrap();

        let listing = Run::list_all(&home).unwrap();
        let ids: Vec<&str> = listing
            .runs
            .iter()
            .map(|r| r.manifest.id.as_str())
            .collect();
        assert_eq!(ids, vec!["aaaa000000000000"]);
        let mut unreadable: Vec<&str> = listing.unreadable.iter().map(|u| u.id.as_str()).collect();
        unreadable.sort();
        assert_eq!(unreadable, vec!["bbbb000000000000", "cccc000000000000"]);
        let bad = listing
            .unreadable
            .iter()
            .find(|u| u.id == "bbbb000000000000")
            .unwrap();
        assert!(bad.reason.contains("missing field"), "{}", bad.reason);

        assert_eq!(
            Run::find_unreadable(&home, "bbbb000000000000").unwrap().id,
            "bbbb000000000000"
        );
        assert_eq!(
            Run::find_unreadable(&home, "ccc").unwrap().id,
            "cccc000000000000"
        );
        assert!(Run::find_unreadable(&home, "aaaa").is_none());
        assert!(Run::find_unreadable(&home, "dddd").is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_step_past_the_end_of_a_run_is_refused() {
        // A finished run ends at its outcome's step, after its last event;
        // a run that never finished, at its last event. Steps up to the
        // end are taken, and the one after it refused with both named.
        let root = runs_dir("past-end");
        let home = Home::at(root.clone()).unwrap();
        let mut finished = manifest("aaaa000000000000", "finished", 1);
        finished.outcome = Some(RunOutcome {
            stop: rewind_trace::stop::Stop::PoweredOff,
            step: 12,
            virtual_ns: 0,
            status: Some(0),
            wall_ms: 0,
        });
        let dir = write_run(&home.runs(), &finished, &[3, 5, 9], b"a");
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.last_step().unwrap(), 12);
        assert_eq!(run.check_step(12).unwrap(), 12);
        assert_eq!(
            run.check_step(13).unwrap_err().to_string(),
            "run aaaa000000000000 ends at step 12, so step 13 is past its end"
        );

        let dir = write_run(
            &home.runs(),
            &manifest("bbbb000000000000", "cut", 2),
            &[3, 5, 9],
            b"a",
        );
        let run = Run::open(&dir).unwrap();
        assert_eq!(run.last_step().unwrap(), 9);
        assert!(run.check_step(10).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_fork_whose_machine_does_not_build_leaves_no_run_behind() {
        // A fork that fails before its machine runs never started, so it
        // must not be listed as an interrupted run. Forks a finished parent
        // twice with specs no machine is built from: one whose initramfs is
        // not there, so its config does not read, and one whose initramfs
        // reads but whose kernel is not there, so the boot fails, whether
        // or not this host has KVM. Each execution must fail and leave the
        // runs directory holding the parent alone.
        const FORK_STEP: u64 = 5;
        let root = runs_dir("unbuilt-fork");
        let home = Home::at(root.clone()).unwrap();
        let mut parent = manifest("aaaa000000000000", "parent", 1);
        parent.outcome = Some(RunOutcome {
            stop: rewind_trace::stop::Stop::PoweredOff,
            step: 12,
            virtual_ns: 0,
            status: Some(0),
            wall_ms: 0,
        });
        write_run(&home.runs(), &parent, &[3, 5, 9], b"a");
        let initrd = root.join("initrd");
        fs::write(&initrd, b"").unwrap();
        let unbootable = [
            perturbed(3, FORK_STEP, u64::MAX),
            Spec {
                initrd,
                ..perturbed(4, FORK_STEP, u64::MAX)
            },
        ];

        // Each fork is refused, and the runs directory still lists only
        // the parent.
        for spec in unbootable {
            let fork = Run::execute(
                &home,
                "fork".into(),
                parent.source.clone(),
                spec,
                Start::Fork {
                    parent: parent.id.clone(),
                    step: FORK_STEP,
                },
                Execution {
                    echo: Echo::Quiet,
                    keyframes: Keyframes::Take,
                    limit: TimeLimit::None,
                },
            );
            assert!(fork.is_err());
            let runs: Vec<_> = fs::read_dir(home.runs())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(runs, vec![std::ffi::OsString::from(parent.id.as_str())]);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_ambiguous_prefix_lists_the_newest_five_matches() {
        // Seven runs whose ids start with 8f, made at 0 to 6: the error
        // names the five newest, newest first, and counts the rest.
        let root = runs_dir("ambiguous");
        let home = Home::at(root.clone()).unwrap();
        for i in 0..7u64 {
            let id = format!("8f{i:014}");
            write_run(
                &home.runs(),
                &manifest(&id, &format!("r{i}"), i),
                &[3],
                b"a",
            );
        }
        assert_eq!(
            Run::find(&home, "8f").err().unwrap().to_string(),
            "7 runs match \"8f\"; give more of the id:\n  \
             8f00000000000006  r6\n  \
             8f00000000000005  r5\n  \
             8f00000000000004  r4\n  \
             8f00000000000003  r3\n  \
             8f00000000000002  r2\n  \
             and 2 more"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_replay_by_the_recording_build_blames_the_host() {
        // A replay that went another way under the build that recorded
        // the run says the run is not deterministic on this host and how
        // to find out why; under another build it blames the builds.
        let id = id("a");
        let same = went_another_way_words(&id, 311, "0.9.0 (abc)", "0.9.0 (abc)");
        assert!(same.contains(rewind_trace::WENT_ANOTHER_WAY), "{same}");
        assert!(same.contains("not deterministic on this host"), "{same}");
        assert!(same.contains("--bisect"), "{same}");
        assert!(same.contains("rewind bugreport"), "{same}");
        assert!(!same.contains("this build"), "{same}");
        let other = went_another_way_words(&id, 311, "0.8.1 (def)", "0.9.0 (abc)");
        assert!(other.contains(rewind_trace::WENT_ANOTHER_WAY), "{other}");
        assert!(other.contains("0.8.1 (def)"), "{other}");
        assert!(other.contains("record the run again"), "{other}");
    }

    #[test]
    fn a_replay_by_another_build_names_both_and_blames_neither() {
        // Under another build a replay that differs may be the run's
        // doing or the builds': the words name both builds, say how to
        // tell which, and neither call the run not deterministic nor send
        // the reader to --bisect, which compares two replays by this build.
        let id = id("a");
        assert_eq!(Builds::of("0.9.0 (abc)", "0.9.0 (abc)"), Builds::Same);
        assert_eq!(Builds::of("0.8.1 (def)", "0.9.0 (abc)"), Builds::Other);
        let cause = another_way_cause("0.8.1 (def)", "0.9.0 (abc)");
        assert!(
            cause.contains("rewind 0.8.1 (def) recorded the run"),
            "{cause}"
        );
        assert!(cause.contains("this is rewind 0.9.0 (abc)"), "{cause}");
        assert!(!cause.contains("not deterministic"), "{cause}");
        let next = another_way_next(&id, "0.8.1 (def)", "0.9.0 (abc)");
        assert!(
            next.contains("Replay the run with rewind 0.8.1 (def)"),
            "{next}"
        );
        assert!(next.contains("record the run again"), "{next}");
        assert!(!next.contains("--bisect"), "{next}");
        let other = went_another_way_words(&id, 311, "0.8.1 (def)", "0.9.0 (abc)");
        assert!(other.contains(&cause) && other.contains(&next), "{other}");
        assert!(!other.contains("--bisect"), "{other}");
        let same = another_way_next(&id, "0.9.0 (abc)", "0.9.0 (abc)");
        assert!(same.contains("--bisect"), "{same}");
    }

    #[test]
    fn a_manifest_names_the_rewind_that_recorded_it() {
        // A run's manifest names the rewind that recorded it by its
        // version and the commit it was built from; one without that name
        // does not read.
        assert!(crate::VERSION.starts_with(env!("CARGO_PKG_VERSION")));
        let m = manifest("c", "r", 0);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["recorded_by"], crate::VERSION);

        let mut older = json;
        older.as_object_mut().unwrap().remove("recorded_by");
        assert!(serde_json::from_value::<Manifest>(older).is_err());
    }
}
