//! Whether this host's performance counters can drive virtual time.
//!
//! Virtual time that follows the guest's work needs a counter of retired
//! conditional branches that is exact: the same count at the same exit on
//! every run. Intel's is. AMD's Zen cores overcount around lock-prefixed
//! instructions unless a bit in a model-specific register is set, the
//! workaround rr documents; setting it needs root and lasts until reboot.
//! Rather than trust any of that, [`selftest`] runs a workload that
//! provokes the overcount twice and compares every count.
//!
//! The explanation for users is docs/pmu.md, published at [`DOCS_URL`].

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rewind_init::{Job, Root};
use rewind_vmm::pmu::{Counter, Event, Modes};
use rewind_vmm::{Ignore, Machine};

use crate::home::Guest;
use crate::run::{BASE_CMDLINE, DEFAULT_QUANTUM, Spec};

/// Where to read about all this.
pub const DOCS_URL: &str = "https://rewindvm.dev/counter-time.html";

/// AMD's load-store configuration MSR and the bit rr sets in it
/// ("SpecLockMap" off), which makes Zen's branch counter exact.
pub const AMD_LS_CFG: u64 = 0xc001_1020;
pub const AMD_SPEC_LOCK_MAP_DISABLE: u64 = 1 << 54;

/// The first AMD family with Zen cores.
const AMD_ZEN_FAMILY: u32 = 0x17;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    /// AMD, with the CPU family, which says whether it is a Zen core.
    Amd {
        family: u32,
    },
    Other(String),
}

impl Vendor {
    pub fn detect() -> Result<Vendor> {
        let info = fs::read_to_string("/proc/cpuinfo")?;
        let field = |name: &str| {
            info.lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        Ok(match field("vendor_id").as_str() {
            "GenuineIntel" => Vendor::Intel,
            "AuthenticAMD" => Vendor::Amd {
                family: field("cpu family").parse().unwrap_or(0),
            },
            other => Vendor::Other(other.to_string()),
        })
    }

    /// The counter this vendor's cores count retired conditional branches
    /// with.
    pub fn event(&self) -> Option<Event> {
        match self {
            Vendor::Intel => Some(Event::IntelRetiredConditionalBranches),
            Vendor::Amd { family } if *family >= AMD_ZEN_FAMILY => {
                Some(Event::AmdRetiredConditionalBranches)
            }
            _ => None,
        }
    }

    /// Whether the counter needs rr's MSR workaround to be exact.
    pub fn needs_workaround(&self) -> bool {
        matches!(self, Vendor::Amd { family } if *family >= AMD_ZEN_FAMILY)
    }
}

/// The MSR device files, one per CPU.
fn msr_devices() -> Result<Vec<PathBuf>> {
    let mut devices: Vec<PathBuf> = fs::read_dir("/dev/cpu")?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("msr"))
        .filter(|p| p.exists())
        .collect();
    devices.sort();
    Ok(devices)
}

/// Whether the AMD workaround bit is set on every CPU. None when that
/// cannot be read, which without root is the usual case.
pub fn workaround_set() -> Option<bool> {
    let devices = msr_devices().ok()?;
    if devices.is_empty() {
        return None;
    }
    let mut all = true;
    for dev in devices {
        let f = fs::File::open(dev).ok()?;
        let mut value = [0u8; 8];
        f.read_exact_at(&mut value, AMD_LS_CFG).ok()?;
        all &= u64::from_le_bytes(value) & AMD_SPEC_LOCK_MAP_DISABLE != 0;
    }
    Some(all)
}

/// Sets the AMD workaround bit on every CPU. Needs root, and the msr
/// module, which it loads.
pub fn enable_workaround() -> Result<usize> {
    if msr_devices().map_or(true, |d| d.is_empty()) {
        let status = std::process::Command::new("modprobe")
            .arg("msr")
            .status()
            .context("running modprobe msr")?;
        if !status.success() {
            bail!("modprobe msr failed; is the msr module available?");
        }
    }
    let devices = msr_devices()?;
    if devices.is_empty() {
        bail!("no /dev/cpu/*/msr devices after loading the msr module");
    }
    for dev in &devices {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev)
            .with_context(|| format!("opening {}; this needs root", dev.display()))?;
        let mut value = [0u8; 8];
        f.read_exact_at(&mut value, AMD_LS_CFG)?;
        let value = u64::from_le_bytes(value) | AMD_SPEC_LOCK_MAP_DISABLE;
        f.write_all_at(&value.to_le_bytes(), AMD_LS_CFG)
            .with_context(|| format!("writing {}", dev.display()))?;
    }
    Ok(devices.len())
}

pub fn perf_event_paranoid() -> Option<i32> {
    fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The self-test's verdict for this boot, cached in the home: the counter's
/// exactness does not change until the workaround is set or the machine
/// reboots, and `rewind pmu status` runs the test again.
pub fn exact_this_boot(home: &crate::Home, guest: &Guest) -> Result<bool> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_string();
    let cache = home.root().join("pmu-selftest");
    if let Ok(text) = fs::read_to_string(&cache) {
        if let Some(verdict) = text.strip_prefix(&format!("{boot} ")) {
            return Ok(verdict.trim() == "exact");
        }
    }
    let exact = selftest(guest).map(|t| t.exact).unwrap_or(false);
    remember(home, exact)?;
    Ok(exact)
}

/// Caches a self-test verdict for this boot.
pub fn remember(home: &crate::Home, exact: bool) -> Result<()> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_string();
    let verdict = if exact { "exact" } else { "inexact" };
    fs::write(
        home.root().join("pmu-selftest"),
        format!("{boot} {verdict}\n"),
    )?;
    Ok(())
}

/// The warning a run made with exit time prints, and the app shows.
pub fn exit_time_warning(vendor: &Vendor) -> String {
    let why = if vendor.needs_workaround() {
        "this AMD CPU's branch counter is not exact until rr's workaround is set"
    } else {
        "this CPU's branch counter is not exact"
    };
    format!(
        "recording with exit time: {why}. Computation will not move the VM's clock, \
         and a thread that computes without system calls is not preempted. \
         Fix it with `sudo rewind pmu enable` (until reboot), then `rewind pmu status`. \
         Why: {DOCS_URL}"
    )
}

/// What two runs of the self-test's workload counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTest {
    /// Final counts of the two runs.
    pub counts: [u64; 2],
    /// Whether every exit saw the same count in both runs.
    pub exact: bool,
}

/// Runs the self-test workload twice with the branch counter attached, and
/// compares the count at every exit.
pub fn selftest(guest: &Guest) -> Result<SelfTest> {
    let event = Vendor::detect()?
        .event()
        .context("this CPU has no retired conditional branch counter rewind knows")?;
    let spec = Spec {
        kernel: guest.kernel.clone(),
        initrd: guest.initrd.clone(),
        image: None,
        image_hash: None,
        mem_mib: 256,
        seed: 0,
        epoch: 1,
        quantum: DEFAULT_QUANTUM,
        schedule: 0,
        schedule_from: 0,
        schedule_until: u64::MAX,
        cpu: rewind_vmm::cpu::Model::default(),
        clock: rewind_vmm::ClockSource::Exits,
        preemption: rewind_vmm::Preemption::AtExits,
        cmdline: BASE_CMDLINE.to_string(),
        job: Job {
            argv: vec!["/init".into(), "--selftest".into()],
            env: Vec::new(),
            cwd: "/".into(),
            uid: 0,
            gid: 0,
            hostname: "localhost".into(),
            root: Root::Initramfs,
            files: Vec::new(),
            outputs: Vec::new(),
        },
    };
    let config = spec.config()?;
    let mut counts = [0u64; 2];
    let mut hashes = [0u64; 2];
    for i in 0..2 {
        let mut machine = Machine::boot(&config)?;
        machine.pmu = Some((
            Counter::open(event, Modes::UserOnly)?,
            0xcbf2_9ce4_8422_2325,
        ));
        machine.run(None, &mut Ignore)?;
        let (counter, hash) = machine.pmu.as_ref().unwrap();
        counts[i] = counter.read()?;
        hashes[i] = *hash;
    }
    Ok(SelfTest {
        counts,
        exact: hashes[0] == hashes[1],
    })
}
