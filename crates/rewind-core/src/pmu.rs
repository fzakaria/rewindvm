//! Whether this host's performance counters can drive virtual time.
//!
//! Virtual time that follows the guest's work needs a counter of retired
//! conditional branches that is exact: the same count at the same exit on
//! every run. Intel's is. AMD's Zen cores overcount around lock-prefixed
//! instructions unless a bit in a model-specific register is set, the
//! workaround rr documents; setting it needs root and lasts until reboot.
//! [`selftest`] runs a workload that provokes the overcount twice and
//! compares every count. On AMD that is not enough: without the workaround
//! the two runs sometimes agree by chance, so counter time there also needs
//! the workaround to be known to be set, from the MSR itself or from the
//! marker `rewind pmu enable` leaves under /run.
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
use crate::run::{BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM, Spec};

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

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Vendor::Intel => write!(f, "Intel"),
            Vendor::Amd { family } if *family >= AMD_ZEN_FAMILY => {
                write!(f, "AMD Zen (family {family})")
            }
            Vendor::Amd { family } => write!(f, "AMD (family {family})"),
            Vendor::Other(name) => write!(f, "{name}"),
        }
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

/// Where `rewind pmu enable` records that it set the AMD workaround, with
/// the boot's id. /run is cleared at boot, like the MSR bit, and anyone can
/// read it, while the MSR can be read only by root.
pub const WORKAROUND_MARKER: &str = "/run/rewind/amd-branch-workaround";

/// This boot's id, which names the boot in caches and markers.
fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Whether the AMD workaround is known to be set on every CPU this boot:
/// read from the MSRs as root, or from the marker otherwise.
pub fn workaround_known() -> bool {
    if let Some(set) = workaround_set() {
        return set;
    }
    fs::read_to_string(WORKAROUND_MARKER).is_ok_and(|text| text.trim() == boot_id())
}

/// Whether runs may use counter time: the self-test found the counter
/// exact, and on a CPU that needs the workaround, the workaround is known
/// to be set, since there the self-test can pass by chance.
pub fn counter_usable(needs_workaround: bool, workaround_known: bool, exact: bool) -> bool {
    exact && (workaround_known || !needs_workaround)
}

/// Sets the AMD workaround bit on every CPU and leaves the marker. Needs
/// root, and the msr module, which it loads.
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

    // The marker, for runs as users who cannot read the MSRs.
    let marker = PathBuf::from(WORKAROUND_MARKER);
    if let Some(dir) = marker.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&marker, format!("{}\n", boot_id()))
        .with_context(|| format!("writing {}", marker.display()))?;
    Ok(devices.len())
}

pub fn perf_event_paranoid() -> Option<i32> {
    fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether runs may use counter time this boot. On a CPU that needs the
/// workaround and does not have it, no: the self-test is not run at all.
/// Otherwise the self-test's verdict, cached in the home under the boot and
/// whether the workaround is known, so setting it later tests again.
pub fn counter_usable_this_boot(
    home: &crate::Home,
    guest: &Guest,
    vendor: &Vendor,
) -> Result<bool> {
    let needs = vendor.needs_workaround();
    let known = needs && workaround_known();
    if needs && !known {
        return Ok(false);
    }

    let cache = home.root().join(SELFTEST_CACHE);
    if let Some(verdict) = fs::read_to_string(&cache)
        .ok()
        .and_then(|text| text.strip_prefix(&cache_key(known)).map(str::to_string))
    {
        return Ok(counter_usable(needs, known, verdict.trim() == EXACT));
    }
    let exact = selftest(guest).map(|t| t.exact).unwrap_or(false);
    remember(home, known, exact)?;
    Ok(counter_usable(needs, known, exact))
}

/// The file in the home that caches the self-test's verdict, and the words
/// for it.
const SELFTEST_CACHE: &str = "pmu-selftest";
const EXACT: &str = "exact";
const INEXACT: &str = "inexact";

/// What a cached verdict is filed under: this boot, and whether the
/// workaround was known to be set when the test ran.
fn cache_key(workaround_known: bool) -> String {
    let workaround = if workaround_known { "+workaround" } else { "" };
    format!("{}{workaround} ", boot_id())
}

/// Caches a self-test verdict for this boot.
pub fn remember(home: &crate::Home, workaround_known: bool, exact: bool) -> Result<()> {
    let verdict = if exact { EXACT } else { INEXACT };
    fs::write(
        home.root().join(SELFTEST_CACHE),
        format!("{}{verdict}\n", cache_key(workaround_known)),
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
        kernel_debug: None,
        image: None,
        image_hash: None,
        mem_mib: 256,
        cores: DEFAULT_CORES,
        seed: 0,
        epoch: 1,
        quantum: DEFAULT_QUANTUM,
        schedule: 0,
        schedule_from: 0,
        schedule_until: u64::MAX,
        inherited_schedules: Vec::new(),
        cpu: rewind_vmm::cpu::Model::default(),
        clock: rewind_vmm::ClockSource::Exits,
        preemption: rewind_vmm::Preemption::AtExits,
        extras: rewind_vmm::Extras::Absent,
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

#[cfg(test)]
mod tests {
    // The decision to use counter time, from the three facts it rests on.
    use super::*;

    #[test]
    fn amd_needs_the_workaround_even_when_the_self_test_passes() {
        // The self-test agreeing by chance is not enough on AMD.
        assert!(!counter_usable(true, false, true));
        assert!(counter_usable(true, true, true));
        assert!(!counter_usable(true, true, false));
    }

    #[test]
    fn intel_needs_only_the_self_test() {
        // No workaround exists, so the self-test decides.
        assert!(counter_usable(false, false, true));
        assert!(!counter_usable(false, false, false));
    }
}
