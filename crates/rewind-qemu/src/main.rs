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
}

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
    let work = match &args.work {
        Some(w) => w.clone(),
        None => std::env::temp_dir().join(format!("rewind-qemu-{}", std::process::id())),
    };
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let initrd = work.join("initrd");
    std::fs::write(&initrd, &config.initrd)?;
    let log = work.join("steps.log");

    // A replay of a run the host's time limit stopped stops where it did.
    let until = run
        .manifest
        .outcome
        .as_ref()
        .filter(|o| o.stop.timeout().is_some())
        .map_or(0, |o| o.step);
    let seed: String = config.seed.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    let mut machine = format!(
        "rewind,seed={seed},epoch={},quantum={},cpu-model={model},vendor={},extras={},log={},until={until}",
        config.epoch,
        config.quantum,
        vendor.name(),
        if config.extras == Extras::Reserved {
            "on"
        } else {
            "off"
        },
        log.display(),
    );
    if let Some(image) = &config.image {
        write!(machine, ",image={}", image.display())?;
    }

    // The steps the schedule perturbs, through the recording's last.
    let last = run
        .manifest
        .outcome
        .as_ref()
        .map(|o| o.step)
        .context("the run has not ended; replay a run that has")?;
    let schedule = schedule::encode(&config.schedule, last);
    if schedule.len() > schedule::MAGIC.len() {
        let path = work.join("schedule");
        std::fs::write(&path, &schedule)?;
        write!(machine, ",schedule={}", path.display())?;
    }

    let started = Instant::now();
    let status = Command::new(&args.qemu)
        .args(["-machine", &machine, "-accel", "tcg", "-smp", "1"])
        .args(["-m", &format!("{}M", config.mem_bytes >> 20)])
        .arg("-kernel")
        .arg(&config.kernel)
        .arg("-initrd")
        .arg(&initrd)
        .args(["-append", &config.cmdline])
        .args(["-nodefaults", "-display", "none", "-no-reboot"])
        .status()
        .with_context(|| format!("starting {}", args.qemu.display()))?;
    let wall = started.elapsed();
    if !status.success() {
        bail!("{} failed: {status}", args.qemu.display());
    }

    // The trace, made by the encoders the KVM machine uses.
    let bytes = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
    let encoded = step_log::encode(&bytes)?;
    let trace_path = work.join("trace.bin");
    let mut writer = TraceWriter::new(std::fs::File::create(&trace_path)?);
    for (step, record) in &encoded.records {
        writer.record(*step, record)?;
    }
    writer.finish()?;
    let hash = blake3::hash(&std::fs::read(&trace_path)?)
        .to_hex()
        .to_string();

    let (steps, stop) = encoded.end.unwrap_or((0, Stop::Paused));
    println!(
        "replayed {} steps in {:.2} s ({:.0} steps/s), stopped: {stop:?}, virtual {} ns",
        steps,
        wall.as_secs_f64(),
        steps as f64 / wall.as_secs_f64(),
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
    report_divergence(run.trace()?, run.records()?, &encoded.records)?;
    std::process::exit(1);
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
