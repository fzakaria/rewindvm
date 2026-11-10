//! Whether this host's performance counters can drive virtual time.
//!
//! Virtual time that follows the guest's work needs a counter of retired
//! conditional branches that is exact: the same count at the same exit on
//! every run. Intel's is. AMD's Zen cores overcount around lock-prefixed
//! instructions unless a bit in a model-specific register is set, the
//! workaround rr documents; setting it needs root and lasts until reboot.
//! [`selftest`] runs a workload that provokes the overcount twice and
//! compares every count, and checks probes whose counts are known by
//! construction. On AMD that is not enough: without the workaround
//! the two runs sometimes agree by chance, so counter time there also needs
//! the workaround to be known to be set, from the MSR itself or from the
//! marker `rewind pmu enable` leaves under /run.
//!
//! The explanation for users is docs/pmu.md, published at [`DOCS_URL`].

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rewind_init::{Job, Mark, Probe, Root, SELFTEST_ARG};
use rewind_trace::EventKind;
use rewind_vmm::CounterEvent as Event;
use rewind_vmm::pmu::{Counter, Modes, Pinned};
use rewind_vmm::{Machine, Observer, Outcome};

use crate::home::Guest;
use crate::run::{BASE_CMDLINE, DEFAULT_CORES, DEFAULT_QUANTUM, Spec, SpecExt};

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
/// whether the workaround is known, so setting it later tests again. A
/// self-test that could not run on every kind of core is not cached, since
/// a later process may run where this one could not.
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
    let (exact, cacheable) = match selftest(guest) {
        Ok(t) => (t.exact, t.covers_every_kind()),
        Err(_) => (false, true),
    };
    if cacheable {
        remember(home, known, exact)?;
    }
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

/// Why this boot's counter cannot drive the VM's clock, for a warning or a
/// refusal.
fn inexact_reason(vendor: &Vendor) -> &'static str {
    if vendor.needs_workaround() {
        return "this AMD CPU's branch counter is not exact until rr's workaround is set";
    }
    "this CPU's branch counter is not exact"
}

/// The warning a run made with exit time prints, and the app shows.
pub fn exit_time_warning(vendor: &Vendor) -> String {
    let why = inexact_reason(vendor);
    format!(
        "recording with exit time: {why}. Computation will not move the VM's clock, so a \
         timer set during a long computation fires late. `sudo rewind pmu enable` (until \
         reboot), then `rewind pmu status`, gives runs counter time, which makes the clock \
         follow computation; with either clock a thread that computes without system calls \
         keeps the CPU. Why: {DOCS_URL}"
    )
}

/// The error `--clock branches` gives where the counter is not exact: a run
/// counted on it would neither replay nor match the same inputs on another
/// host. On AMD Zen a Nix build sees the workaround only through its marker,
/// which the sandbox lacks unless the build machine adds it.
pub fn branch_clock_refusal(vendor: &Vendor) -> String {
    let why = inexact_reason(vendor);
    let sandbox = if vendor.needs_workaround() {
        format!(
            " In a Nix build, {WORKAROUND_MARKER} must be in the sandbox, as \
             programs.rewind.amdBranchCounterWorkaround puts it."
        )
    } else {
        String::new()
    };
    format!(
        "--clock branches needs an exact branch counter, and {why}.{sandbox} \
         Fix it with `sudo rewind pmu enable` (until reboot), then `rewind pmu status`, \
         or record with --clock exits. Why: {DOCS_URL}"
    )
}

/// What the self-test's workload counted, twice on each kind of core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTest {
    /// One per kind of core the test ran on: one, of no kind, on a host
    /// with one kind of core.
    pub runs: Vec<SelfTestRuns>,
    /// Whether every exit saw the same count in every run, and every probe
    /// counted its known answer.
    pub exact: bool,
    /// The kinds of core this process may not run on, which the test left
    /// out, as their PMU and CPUs.
    pub untested: Vec<String>,
}

impl SelfTest {
    /// Whether the test ran on every kind of core.
    pub fn covers_every_kind(&self) -> bool {
        self.untested.is_empty()
    }
}

/// The self-test's two runs on one kind of core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTestRuns {
    /// The kind of core, as its PMU and CPUs, on a hybrid host.
    pub kind: Option<String>,
    /// Final counts of the two runs.
    pub counts: [u64; 2],
    /// The first run's probe counts.
    pub probes: Vec<ProbeCount>,
}

/// A probe's known answer, and what the counter made of it in one run:
/// None when the run did not write all four of the probe's marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeCount {
    pub probe: Probe,
    pub expected: u64,
    pub counted: Option<u64>,
}

impl ProbeCount {
    pub fn matches(&self) -> bool {
        self.counted == Some(self.expected)
    }
}

/// Every probe's count, from the counter's value at each probe mark in the
/// order the run wrote them: four a probe, around its short run and its
/// long one. The long run's count less the short run's is the branches of
/// the extra iterations, since all else between the marks is the same.
fn probe_counts(marks: &[(Probe, u64)]) -> Vec<ProbeCount> {
    Probe::ALL
        .into_iter()
        .map(|probe| {
            let at: Vec<u64> = marks
                .iter()
                .filter(|(p, _)| *p == probe)
                .map(|(_, count)| *count)
                .collect();
            let counted = match at.as_slice() {
                [s1, e1, s2, e2] => e2
                    .checked_sub(*s2)
                    .zip(e1.checked_sub(*s1))
                    .and_then(|(long, short)| long.checked_sub(short)),
                _ => None,
            };
            ProbeCount {
                probe,
                expected: probe.branches_per_iteration() * probe.iterations(),
                counted,
            }
        })
        .collect()
}

/// Pauses the self-test's machine at each probe mark, where the counter
/// has counted everything the guest did before the mark's write. The
/// self-test is the job, not init, so its marks are not init's.
#[derive(Default)]
struct ProbeMarks {
    seen: Option<Probe>,
}

impl Observer for ProbeMarks {
    fn record(&mut self, step: u64, record: &[u8]) {
        let Ok(event) = rewind_trace::Event::decode(step, record) else {
            return;
        };
        let EventKind::Mark { text } = &event.kind else {
            return;
        };
        if let Some(Mark::Probe(probe)) = Mark::parse(text) {
            self.seen = Some(probe);
        }
    }

    fn stop(&self) -> bool {
        self.seen.is_some()
    }
}

/// Runs the self-test workload twice with the branch counter attached, and
/// compares the count at every exit. On a hybrid host it runs twice on each
/// kind of core, kept there, since a run adds up one counter per kind and
/// is exact only if every kind counts alike. The kinds this process may
/// not run on are left out and named in `untested`: the verdict holds for
/// this process's runs, which never count there, but not for the boot.
pub fn selftest(guest: &Guest) -> Result<SelfTest> {
    let event = Vendor::detect()?
        .event()
        .context("this CPU has no retired conditional branch counter rewind knows")?;
    let config = selftest_spec(guest).config()?;
    let kinds = rewind_vmm::pmu::kinds_of_core()?;
    let mut runs = Vec::new();
    let mut counts = Vec::new();
    let mut hashes = Vec::new();
    let mut probes = Vec::new();
    let mut run_twice = |kind: Option<String>| -> Result<()> {
        let mut these = [0u64; 2];
        let mut first = None;
        for count in &mut these {
            let mut machine = Machine::boot(&config)?;
            machine.pmu = Some((
                Counter::open(event, Modes::UserOnly)?,
                0xcbf2_9ce4_8422_2325,
            ));

            // Run from one probe mark to the next, reading the counter at
            // each, until the guest stops.
            let mut observer = ProbeMarks::default();
            let mut marks = Vec::new();
            while let Outcome::Paused = machine.run(None, &mut observer)? {
                let (counter, _) = machine.pmu.as_ref().expect("set before the run");
                let probe = observer.seen.take().expect("paused only at a probe mark");
                marks.push((probe, counter.read()?));
            }

            let (counter, hash) = machine.pmu.as_ref().expect("set before the run");
            *count = counter.read()?;
            hashes.push(*hash);
            let these_probes = probe_counts(&marks);
            probes.extend(these_probes.iter().cloned());
            first.get_or_insert(these_probes);
        }
        counts.extend(these);
        runs.push(SelfTestRuns {
            kind,
            counts: these,
            probes: first.unwrap_or_default(),
        });
        Ok(())
    };
    if kinds.is_empty() {
        run_twice(None)?;
    }
    let mut untested = Vec::new();
    for kind in &kinds {
        let name = format!("{} (CPUs {})", kind.pmu, kind.cpu_list);

        // This thread runs the vCPU, so kept to one kind of core it tests
        // that kind's counter alone.
        let Some(_kept) = Pinned::to(&kind.cpus)? else {
            untested.push(name);
            continue;
        };
        run_twice(Some(name))?;
    }
    Ok(SelfTest {
        runs,
        exact: verdict(&counts, &hashes, &probes),
        untested,
    })
}

/// Whether the self-test's runs show an exact counter: the same count at
/// every exit in every run, on every kind of core, and a count at all. The
/// workload certainly branches, so a counter that read 0 every time
/// counted nothing, and agreeing on 0 says nothing about exactness. A
/// counter can also agree with itself and be wrong, as one that counts only
/// taken branches is, so every probe must count its known answer too.
fn verdict(counts: &[u64], hashes: &[u64], probes: &[ProbeCount]) -> bool {
    !hashes.is_empty()
        && hashes.iter().all(|&hash| hash == hashes[0])
        && counts.iter().all(|&count| count > 0)
        && probes.iter().all(ProbeCount::matches)
}

/// The machine the self-test boots: the self-test's workload on the bare
/// guest, with exit time. The guest is shown the host's CPU, since the
/// test is of the host's branch counter, and the fixed x86-64-v3 model
/// would refuse a host that lacks part of its baseline.
fn selftest_spec(guest: &Guest) -> Spec {
    Spec {
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
        stall_max: rewind_trace::schedule::StallMax::default(),
        inherited_schedules: Vec::new(),
        reseeds: Vec::new(),
        cpu: rewind_vmm::CpuModel::Host,
        clock: rewind_vmm::ClockSource::Exits,
        preemption: rewind_vmm::Preemption::AtExits,
        extras: rewind_vmm::Extras::Absent,
        cmdline: BASE_CMDLINE.to_string(),
        job: Job {
            program: None,
            argv: vec!["/init".into(), SELFTEST_ARG.into()],
            env: Vec::new(),
            cwd: "/".into(),
            uid: 0,
            gid: 0,
            hostname: "localhost".into(),
            root: Root::Initramfs,
            files: Vec::new(),
            outputs: Vec::new(),
            output: rewind_init::Output::Plain,
        },
    }
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
    fn the_self_test_boots_the_host_cpu() {
        // The self-test measures the host's branch counter, so it shows the
        // guest the host's CPU: a host without the x86-64-v3 baseline, such
        // as a Zen 3 without ERMS, must still be able to run it.
        let guest = Guest {
            kernel: PathBuf::from("bzImage"),
            initrd: PathBuf::from("initrd"),
            kernel_debug: None,
        };
        assert_eq!(selftest_spec(&guest).cpu, rewind_vmm::CpuModel::Host);
    }

    #[test]
    fn intel_needs_only_the_self_test() {
        // No workaround exists, so the self-test decides.
        assert!(counter_usable(false, false, true));
        assert!(!counter_usable(false, false, false));
    }

    #[test]
    fn the_exit_time_warning_offers_what_the_command_gives() {
        // `sudo rewind pmu enable` gives counter time, which makes the
        // clock follow computation; it does not preempt a thread that
        // computes without system calls, so the warning must not offer
        // the command as a fix for that. Builds the message for AMD.
        let warning = exit_time_warning(&Vendor::Amd { family: 25 });
        assert!(warning.contains("sudo rewind pmu enable"), "{warning}");
        assert!(warning.contains("counter time"), "{warning}");
        assert!(!warning.contains("not preempted"), "{warning}");
        assert!(warning.contains("either clock"), "{warning}");
    }

    #[test]
    fn an_explicit_branch_clock_refusal_names_the_cause_and_the_ways_out() {
        // On AMD Zen the refusal names the workaround and where a Nix build
        // must find its marker; on Intel only the counter. Both offer the
        // fix and exit time. Builds the messages for each vendor.
        let amd = branch_clock_refusal(&Vendor::Amd { family: 25 });
        assert!(amd.contains("until rr's workaround is set"), "{amd}");
        assert!(amd.contains(WORKAROUND_MARKER), "{amd}");
        assert!(amd.contains("sudo rewind pmu enable"), "{amd}");
        assert!(amd.contains("--clock exits"), "{amd}");

        let intel = branch_clock_refusal(&Vendor::Intel);
        assert!(
            intel.contains("this CPU's branch counter is not exact"),
            "{intel}"
        );
        assert!(!intel.contains(WORKAROUND_MARKER), "{intel}");
        assert!(intel.contains("--clock exits"), "{intel}");
    }

    /// The counter's value at each probe mark of a run whose counter counts
    /// `per_iteration(probe)` branches an iteration, with 500 branches of
    /// the run between each pair of marks besides the probe's loop.
    fn marks_counting(per_iteration: impl Fn(Probe) -> u64) -> Vec<(Probe, u64)> {
        let mut count = 1_000;
        let mut marks = Vec::new();
        for probe in Probe::ALL {
            for iterations in [probe.iterations(), 2 * probe.iterations()] {
                marks.push((probe, count));
                count += 500 + per_iteration(probe) * iterations;
                marks.push((probe, count));
                count += 70;
            }
        }
        marks
    }

    /// Every probe's known answer, as an exact counter counts it.
    fn known_answers() -> Vec<ProbeCount> {
        probe_counts(&marks_counting(Probe::branches_per_iteration))
    }

    #[test]
    fn a_probe_counts_its_loop_alone() {
        // The branches around a probe's loop are the same at both lengths,
        // so the difference of its two runs is the loop's extra iterations
        // alone, which an exact counter counts as the known answer.
        let counts = known_answers();
        assert_eq!(counts.len(), Probe::ALL.len());
        for count in &counts {
            assert_eq!(
                count.counted,
                Some(count.probe.branches_per_iteration() * count.probe.iterations())
            );
            assert!(count.matches(), "{count:?}");
        }
    }

    #[test]
    fn a_counter_that_counts_only_taken_branches_misses_the_known_answer() {
        // Intel's umask 0x01 on newer cores counts only taken branches,
        // the same short count every run: deterministic, so the hashes
        // agree, but wrong, which the branches probe sees.
        let taken_only = |probe| match probe {
            Probe::Branches => 2,
            probe => Probe::branches_per_iteration(probe),
        };
        let counts = probe_counts(&marks_counting(taken_only));
        assert!(!counts.iter().all(ProbeCount::matches));
        assert!(!verdict(&[20_001_750, 20_001_750], &[7, 7], &counts));
    }

    #[test]
    fn a_run_without_every_probe_mark_is_not_exact() {
        // A guest that wrote no marks, or stopped partway, leaves probes
        // with no count, and an unknown answer is not a right one.
        let counts = probe_counts(&[]);
        assert_eq!(counts.len(), Probe::ALL.len());
        assert!(counts.iter().all(|c| c.counted.is_none()));
        assert!(!verdict(&[20_001_750, 20_001_750], &[7, 7], &counts));

        let mut marks = marks_counting(Probe::branches_per_iteration);
        marks.pop();
        assert!(!probe_counts(&marks).iter().all(ProbeCount::matches));
    }

    #[test]
    fn a_self_test_that_counted_nothing_is_not_exact() {
        // Seen with the counter of an Intel Core Ultra's performance cores
        // on its efficient cores, where it does not count: 0 and 0
        // branches, equal hashes.
        let known = known_answers();
        assert!(!verdict(&[0, 0], &[7, 7], &known));
        assert!(verdict(&[20_001_750, 20_001_750], &[7, 7], &known));
        assert!(!verdict(&[20_001_750, 20_001_750], &[7, 8], &known));
    }

    #[test]
    fn every_kind_of_core_must_count_alike() {
        // Each kind exact on its own is not enough: a run that moves
        // between kinds adds up their counts, so they must agree.
        let known = known_answers();
        assert!(verdict(&[20_001_750; 4], &[7; 4], &known));
        assert!(!verdict(
            &[20_001_750, 20_001_750, 20_001_751, 20_001_751],
            &[7, 7, 8, 8],
            &known
        ));
    }

    #[test]
    fn a_self_test_that_left_out_a_kind_of_core_does_not_cover_every_kind() {
        // A process kept to the performance cores, as with `taskset -c
        // 0-3`, tests only their counter. Its verdict holds for its own
        // runs, which never reach the other cores, but not for a later
        // process that may run anywhere.
        let partial = SelfTest {
            runs: Vec::new(),
            exact: true,
            untested: vec!["cpu_atom (CPUs 4-15)".to_string()],
        };
        assert!(!partial.covers_every_kind());
        let whole = SelfTest {
            untested: Vec::new(),
            ..partial
        };
        assert!(whole.covers_every_kind());
    }
}
