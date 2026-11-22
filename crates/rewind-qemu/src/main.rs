//! Replays a recorded run on Rewind's QEMU machine, without KVM, and
//! says whether its trace came out the same.
//!
//! The run's inputs are its manifest's spec, as for `rewind replay`. This
//! program builds what the spec's machine boots, starts
//! qemu-system-x86_64 with `-machine rewind` (qemu/ in this repository),
//! turns the machine's step log into a trace with rewind-trace's own
//! encoders, and compares it with the run's trace.bin: by hash, then, on
//! a difference, event by event, to name the first step where the two
//! differ.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use rewind_core::run::{Run, SpecExt};
use rewind_trace::machine::{ClockSource, CpuModel, Extras};
use rewind_trace::step_log::{self, Stop};

mod schedule;
use rewind_trace::{Event, Trace, TraceWriter};

#[derive(Parser, Debug)]
#[command(about = "Replay a Rewind VM run on the QEMU machine")]
struct Args {
    /// The run's directory, holding manifest.json and trace.bin.
    run: PathBuf,
    /// The qemu-system-x86_64 built with the rewind machine.
    #[arg(long, env = "REWIND_QEMU", default_value = "qemu-system-x86_64")]
    qemu: PathBuf,
    /// The CPU vendor the run was recorded on. The fixed CPU models keep
    /// the host's vendor and the manifest does not say it, so this
    /// defaults to this host's.
    #[arg(long, value_enum)]
    vendor: Option<Vendor>,
    /// Where to keep the replay's initrd, step log and trace; a
    /// directory of its own under the system's temporary directory when
    /// not given.
    #[arg(long)]
    work: Option<PathBuf>,
    /// Write what a browser build of the machine needs into this
    /// directory and stop: the kernel, the initrd, the image and the
    /// schedule, linked or written there, and args.json, QEMU's
    /// arguments with those files under /rewind.
    #[arg(long, conflicts_with = "log")]
    prepare: Option<PathBuf>,
    /// Compare this step log, from a machine run elsewhere such as a
    /// browser, instead of running QEMU.
    #[arg(long)]
    log: Option<PathBuf>,
    /// Start from the run's latest keyframe at or before this step, made
    /// by its recording or kept by a look, instead of from boot.
    #[arg(long)]
    from: Option<u64>,
    /// Read this file as it was at step --at, as `rewind cat` does: the
    /// machine runs to the step, asks the guest for the file and stops
    /// once the guest has answered, and the file goes to standard output.
    #[arg(long, requires = "at")]
    cat: Option<String>,
    /// The step to read --cat at.
    #[arg(long)]
    at: Option<u64>,
    /// Write what the browser viewer needs to read files at any step of
    /// the run into this directory and stop: each of the run's keyframes
    /// in portable form, the image and the schedule linked or written
    /// there, and manifest.json, which names them and holds QEMU's
    /// arguments with placeholders for the step.
    #[arg(long, conflicts_with_all = ["prepare", "log", "cat", "from"])]
    bundle: Option<PathBuf>,
}

/// The placeholder in a bundle's arguments for the step to read at.
const UNTIL_PLACEHOLDER: &str = "{until}";

/// The mark the guest's inspecting process ends its answer with
/// (rewind-init's Mark::InspectEnd), which stops the machine.
const INSPECT_END_MARK: &str = "rewind-inspect-end";

/// Where the browser build finds the files --prepare writes.
const BROWSER_DIR: &str = "/rewind";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Vendor {
    Amd,
    Intel,
}

impl Vendor {
    fn name(self) -> &'static str {
        match self {
            Vendor::Amd => "amd",
            Vendor::Intel => "intel",
        }
    }

    /// This host's vendor, from /proc/cpuinfo.
    fn of_host() -> Result<Vendor> {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").context("reading /proc/cpuinfo")?;
        if cpuinfo.contains("AuthenticAMD") {
            return Ok(Vendor::Amd);
        }
        if cpuinfo.contains("GenuineIntel") {
            return Ok(Vendor::Intel);
        }
        bail!("this host is neither AMD nor Intel; give --vendor")
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let run = Run::open(&args.run)?;
    let spec = &run.manifest.spec;

    // What the machine does not model yet.
    let model = match spec.cpu {
        CpuModel::V3 => "v3",
        CpuModel::V3Compat => "v3-compat",
        CpuModel::Host => {
            bail!("the run shows the guest the host's CPU; only v3 and v3-compat replay here")
        }
    };
    if spec.clock != ClockSource::Exits {
        bail!("the run's clock counts branches; only runs with --clock exits replay here");
    }
    let vendor = match args.vendor {
        Some(v) => v,
        None => Vendor::of_host()?,
    };

    // The initrd with the job appended, as the KVM machine boots it.
    let config = spec.config()?;
    if let Some(dir) = &args.bundle {
        return bundle(&run, &config, model, vendor, dir);
    }

    let work = match (&args.prepare, &args.work) {
        (Some(dir), _) | (None, Some(dir)) => dir.clone(),
        (None, None) => std::env::temp_dir().join(format!("rewind-qemu-{}", std::process::id())),
    };
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    std::fs::write(work.join(INITRD), &config.initrd)?;

    // The steps the schedule perturbs, through the recording's last.
    let last = run
        .manifest
        .outcome
        .as_ref()
        .map(|o| o.step)
        .context("the run has not ended; replay a run that has")?;
    let schedule = schedule::encode(&config.schedule, last);
    let perturbed = schedule.len() > schedule::MAGIC.len();
    if perturbed {
        std::fs::write(work.join(SCHEDULE), &schedule)?;
    }

    // A replay of a run the host's time limit stopped stops where it did,
    // and a read stops at its step to ask the guest.
    let until = match args.at {
        Some(at) => at,
        None => run
            .manifest
            .outcome
            .as_ref()
            .filter(|o| o.stop.timeout().is_some())
            .map_or(0, |o| o.step),
    };
    if let Some(path) = &args.cat {
        std::fs::write(
            work.join(REQUEST),
            rewind_trace::inspected::cat_request(None, path),
        )?;
    }
    // The keyframe to start from, in the form the QEMU machine loads.
    let resumed = match args.from {
        Some(step) => {
            let keyframes = run.keyframes()?;
            let at = keyframes
                .steps()
                .into_iter()
                .filter(|s| *s <= step)
                .max()
                .with_context(|| format!("the run has no keyframe at or before step {step}"))?;
            let home = rewind_core::home::Home::open()?;
            let store = rewind_core::run::Store::open(&home.store())?;
            let chain = keyframes.chain(at)?;
            let span = rewind_vmm::layout::ram_span(config.mem_bytes) as usize;
            let state = rewind_vmm::portable::encode(
                &chain,
                &rewind_core::keyframes::ReadPages(&store),
                span / rewind_vmm::layout::PAGE_SIZE,
            )?;
            std::fs::write(work.join(STATE), &state)?;
            eprintln!(
                "starting from the keyframe at step {at}, {} bytes",
                state.len()
            );
            Some(at)
        }
        None => None,
    };
    let launch = Launch {
        model,
        vendor,
        until: until.to_string(),
        perturbed,
        resumed: resumed.is_some(),
        inspects: args.cat.is_some(),
        config: &config,
    };

    // For a browser: the files beside its arguments, which name them
    // where the page puts them.
    if let Some(dir) = &args.prepare {
        if resumed.is_none() {
            link(&config.kernel, &dir.join(KERNEL))?;
        }
        if let Some(image) = &config.image {
            link(image, &dir.join(IMAGE))?;
        }
        let browser = launch.args(&|name: &str| format!("{BROWSER_DIR}/{name}"));
        std::fs::write(dir.join("args.json"), json_strings(&browser))?;
        println!("wrote {} for a browser", dir.display());
        return Ok(());
    }

    let log = match &args.log {
        Some(log) => log.clone(),
        None => {
            let qemu_args = launch.args(&|name: &str| {
                match (name, &config.image) {
                    (KERNEL, _) => config.kernel.clone(),
                    (IMAGE, Some(image)) => image.clone(),
                    _ => work.join(name),
                }
                .display()
                .to_string()
            });
            let started = Instant::now();
            let status = Command::new(&args.qemu)
                .args(&qemu_args)
                .status()
                .with_context(|| format!("starting {}", args.qemu.display()))?;
            let wall = started.elapsed();
            if !status.success() {
                bail!("{} failed: {status}", args.qemu.display());
            }
            eprintln!("QEMU ran {:.2} s", wall.as_secs_f64());
            work.join(LOG)
        }
    };

    // The trace, made by the encoders the KVM machine uses.
    let bytes = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
    let encoded = step_log::encode(&bytes, run.records()?)?;

    // A read answers with the file; its trace goes another way than the
    // recording's from the step on, so there is nothing to compare.
    if args.cat.is_some() {
        let answer = rewind_trace::inspected::answer(&encoded.records)
            .context("the machine stopped before the guest answered")?;
        if answer.status != rewind_init::InspectStatus::Done {
            bail!(
                "the guest could not read the file ({:?}): {}",
                answer.status,
                String::from_utf8_lossy(&answer.stderr).trim()
            );
        }
        use std::io::Write as _;
        std::io::stdout().write_all(&answer.stdout)?;
        return Ok(());
    }

    // From a keyframe, the replay's trace is the recording's up to it.
    let mut records = match resumed {
        Some(at) => {
            let recorded = run.records()?;
            recorded[..recorded.partition_point(|(s, _)| *s <= at)].to_vec()
        }
        None => Vec::new(),
    };
    records.extend(encoded.records.iter().cloned());
    let trace_path = work.join("trace.bin");
    let mut writer = TraceWriter::new(std::fs::File::create(&trace_path)?);
    for (step, record) in &records {
        writer.record(*step, record)?;
    }
    writer.finish()?;
    let hash = blake3::hash(&std::fs::read(&trace_path)?)
        .to_hex()
        .to_string();

    let (steps, stop) = encoded.end.unwrap_or((0, Stop::Paused));
    println!(
        "replayed {steps} steps, stopped: {stop:?}, virtual {} ns",
        encoded.ns
    );
    println!("trace {}", trace_path.display());
    match &run.manifest.trace_hash {
        Some(recorded) if *recorded == hash => {
            println!("trace_hash {hash} matches the recording");
            return Ok(());
        }
        Some(recorded) => println!("trace_hash {hash}, recording {recorded}"),
        None => println!("trace_hash {hash}; the recording has none"),
    }
    report_divergence(run.trace()?, run.records()?, &records)?;
    std::process::exit(1);
}

/// The files a replay boots and writes, by name in its work directory.
const KERNEL: &str = "vmlinux";
const INITRD: &str = "initrd";
const IMAGE: &str = "image.erofs";
const SCHEDULE: &str = "schedule";
const LOG: &str = "steps.log";
const STATE: &str = "state";
const REQUEST: &str = "request";

/// What QEMU's arguments are made from.
struct Launch<'a> {
    model: &'static str,
    vendor: Vendor,
    /// The step to stop or inspect at, 0 for none, as the machine's
    /// argument takes it: a number, or a placeholder a page fills in.
    until: String,
    perturbed: bool,
    /// Whether the machine starts from a portable keyframe.
    resumed: bool,
    /// Whether the machine asks the guest for a file at the until step.
    inspects: bool,
    config: &'a rewind_vmm::Config,
}

impl Launch<'_> {
    /// qemu-system-x86_64's arguments, with each file named by `path`.
    fn args(&self, path: &dyn Fn(&str) -> String) -> Vec<String> {
        let config = self.config;
        let seed: String = config.seed.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
        let extras = match config.extras {
            Extras::Reserved => "on",
            Extras::Absent => "off",
        };
        let mut machine = format!(
            "rewind,seed={seed},epoch={},quantum={},cpu-model={},vendor={},extras={extras},log={},until={}",
            config.epoch,
            config.quantum,
            self.model,
            self.vendor.name(),
            path(LOG),
            self.until,
        );
        if config.image.is_some() {
            let _ = write!(machine, ",image={}", path(IMAGE));
        }
        if self.perturbed {
            let _ = write!(machine, ",schedule={}", path(SCHEDULE));
        }
        if self.inspects {
            let _ = write!(
                machine,
                ",inspect={},stop-mark={INSPECT_END_MARK}",
                path(REQUEST)
            );
        }
        // A machine from a keyframe boots nothing: its memory holds the
        // kernel and the initrd's files.
        let boot: Vec<String> = if self.resumed {
            let _ = write!(machine, ",state={}", path(STATE));
            Vec::new()
        } else {
            vec![
                "-kernel".into(),
                path(KERNEL),
                "-initrd".into(),
                path(INITRD),
                "-append".into(),
                config.cmdline.clone(),
            ]
        };
        let mut args: Vec<String> = [
            "-machine",
            &machine,
            "-accel",
            "tcg",
            "-smp",
            "1",
            "-m",
            &format!("{}M", config.mem_bytes >> 20),
            "-nodefaults",
            "-display",
            "none",
            "-no-reboot",
        ]
        .map(str::to_string)
        .to_vec();
        args.extend(boot);
        args
    }
}

/// Writes the browser viewer's bundle for `run` into `dir`: the run's
/// keyframes in portable form, the image and schedule, and manifest.json
/// with QEMU's arguments, which start from a keyframe and read a file at
/// the step the page fills in.
fn bundle(
    run: &Run,
    config: &rewind_vmm::Config,
    model: &'static str,
    vendor: Vendor,
    dir: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let last = run
        .manifest
        .outcome
        .as_ref()
        .map(|o| o.step)
        .context("the run has not ended; bundle a run that has")?;
    let schedule = schedule::encode(&config.schedule, last);
    let perturbed = schedule.len() > schedule::MAGIC.len();
    if perturbed {
        std::fs::write(dir.join(SCHEDULE), &schedule)?;
    }
    if let Some(image) = &config.image {
        link(image, &dir.join(IMAGE))?;
    }

    // Every keyframe, the recording's and the ones looks kept.
    let keyframes = run.keyframes()?;
    let home = rewind_core::home::Home::open()?;
    let store = rewind_core::run::Store::open(&home.store())?;
    let span = rewind_vmm::layout::ram_span(config.mem_bytes) as usize;
    let mut steps = Vec::new();
    for step in keyframes.steps() {
        let chain = keyframes.chain(step)?;
        let state = rewind_vmm::portable::encode(
            &chain,
            &rewind_core::keyframes::ReadPages(&store),
            span / rewind_vmm::layout::PAGE_SIZE,
        )?;
        std::fs::write(dir.join(format!("{STATE}-{step}")), &state)?;
        eprintln!("keyframe at step {step}: {} bytes", state.len());
        steps.push(step);
    }

    let launch = Launch {
        model,
        vendor,
        until: UNTIL_PLACEHOLDER.to_string(),
        perturbed,
        resumed: true,
        inspects: true,
        config,
    };
    let args = launch.args(&|name: &str| format!("{BROWSER_DIR}/{name}"));
    let keyframes: Vec<String> = steps.iter().map(u64::to_string).collect();
    let manifest = format!(
        "{{\"args\":{},\"keyframes\":[{}],\"image\":{},\"schedule\":{}}}\n",
        json_strings(&args).trim_end(),
        keyframes.join(","),
        config.image.is_some(),
        perturbed,
    );
    std::fs::write(dir.join("manifest.json"), manifest)?;
    eprintln!("wrote {}", dir.display());
    Ok(())
}

/// Links `to` at `at`, replacing what was there.
fn link(to: &std::path::Path, at: &std::path::Path) -> Result<()> {
    let _ = std::fs::remove_file(at);
    std::os::unix::fs::symlink(to, at)
        .with_context(|| format!("linking {} at {}", to.display(), at.display()))
}

/// `strings` as a JSON array.
fn json_strings(strings: &[String]) -> String {
    let quoted: Vec<String> = strings
        .iter()
        .map(|s| {
            let mut q = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => q.push_str("\\\""),
                    '\\' => q.push_str("\\\\"),
                    c if (c as u32) < 0x20 => {
                        let _ = write!(q, "\\u{:04x}", c as u32);
                    }
                    c => q.push(c),
                }
            }
            q.push('"');
            q
        })
        .collect();
    format!("[{}]\n", quoted.join(","))
}

/// Prints where the replay's trace first differs from the recording's,
/// and the records at that step on each side.
fn report_divergence(
    original: &Trace,
    recorded: &[(u64, Vec<u8>)],
    replayed: &[(u64, Vec<u8>)],
) -> Result<()> {
    let again = Trace::decode(replayed)?;
    let divergence = original
        .divergence(&again)
        .or_else(|| rewind_trace::io_records::divergence(recorded, replayed));
    let Some(d) = divergence else {
        println!("the traces differ, but no event, read or write record does");
        return Ok(());
    };
    println!(
        "first difference ({:?}) at recorded step {}, replayed step {}",
        d.part, d.left_step, d.right_step
    );
    let around = |records: &[(u64, Vec<u8>)], step: u64, side: &str| {
        for (at, record) in records
            .iter()
            .filter(|(at, _)| at + 3 >= step && *at <= step)
        {
            println!("  {side} {at}: {}", describe(*at, record));
        }
    };
    around(recorded, d.left_step, "recorded");
    around(replayed, d.right_step, "replayed");
    Ok(())
}

/// One record, as an event when it is one.
fn describe(step: u64, record: &[u8]) -> String {
    match Event::decode(step, record) {
        Ok(event) => format!("{:?}", event.kind).chars().take(300).collect(),
        Err(_) => format!("record of {} bytes", record.len()),
    }
}
