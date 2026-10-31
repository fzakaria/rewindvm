//! Counting the guest's work with the host's performance counters.
//!
//! Virtual time that moves only at exits makes computation free: a thread
//! that computes for a millisecond between two exits sees no time pass,
//! and neither does the scheduler. A hardware counter of retired branches,
//! restricted to guest mode, measures the work between two exits, and
//! because it is read only at exits, which are fixed points in the guest's
//! instruction stream, the count read there is the same on every run, as
//! long as the counter itself is exact. rr relies on the same property of
//! the same counter.
//!
//! A hybrid Intel CPU has more than one kind of core, and Linux registers a
//! PMU for each (see [`kinds_of_core`]). A counter on a thread counts only
//! while the thread is on a core of its own PMU, so on such a host there is
//! one counter per kind of core and a read adds them up. The sum is exact
//! wherever the host runs the thread only if every kind of core counts the
//! event alike, which the PMU self-test checks on each kind before runs use
//! counter time. The kinds do not run every instruction alike: they can
//! leave the flags Intel's manual calls undefined, as after `imul`, set
//! differently, which reaches memory wherever RFLAGS is saved. A run that
//! moves between kinds of core relies on no program and no saved RFLAGS
//! depending on one; docs/design.md has what that leaves open.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::CounterEvent;

/// perf_event_attr, as far as this monitor uses it.
#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved_2: u16,
    aux_sample_size: u32,
    reserved_3: u32,
    sig_data: u64,
    config3: u64,
}

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_RAW: u32 = 4;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;

/// Flag bits in perf_event_attr.flags.
const FLAG_DISABLED: u64 = 1 << 0;
/// Always on the hardware while enabled: the kernel never takes a pinned
/// event off to share the counters, and puts one that cannot fit in an
/// error state, where a read gives nothing, rather than counting short.
const FLAG_PINNED: u64 = 1 << 2;
const FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
const FLAG_EXCLUDE_HV: u64 = 1 << 6;
const FLAG_EXCLUDE_HOST: u64 = 1 << 19;

/// Bits in perf_event_attr.read_format: a read gives, after the count,
/// how long the event was enabled and how long it was on the hardware.
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;

const PERF_EVENT_IOC_ENABLE: u64 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u64 = 0x2401;
const PERF_EVENT_IOC_REFRESH: u64 = 0x2402;
const PERF_EVENT_IOC_RESET: u64 = 0x2403;
/// _IOW('$', 4, u64)
const PERF_EVENT_IOC_PERIOD: u64 = 0x4008_2404;

/// struct f_owner_ex and the fcntl that takes it, from linux/fcntl.h, which
/// the libc crate does not carry.
#[repr(C)]
struct FOwnerEx {
    type_: i32,
    pid: i32,
}
const F_SETOWN_EX: i32 = 15;
const F_OWNER_TID: i32 = 0;
const F_SETSIG: i32 = 10;

/// The signal a counter overflow sends the vCPU thread. Any signal pulls
/// the thread out of KVM_RUN; this one is otherwise unused.
pub(crate) fn overflow_signal() -> i32 {
    libc::SIGRTMIN() + 3
}

/// Whether guest kernel mode is counted too. Counting it needs
/// perf_event_paranoid at 1 or lower, or CAP_PERFMON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modes {
    UserOnly,
    UserAndKernel,
}

#[derive(Debug)]
pub struct Counter {
    /// One per kind of core, enabled in this order, and disabled and read
    /// in the reverse, which [`total`] relies on.
    fds: Vec<i32>,
}

impl Counter {
    /// A counter of guest-mode events on the calling thread, which must be
    /// the vCPU's: one per kind of core on a hybrid host, added up when
    /// read.
    pub fn open(event: CounterEvent, modes: Modes) -> Result<Counter> {
        let mut counter = Counter { fds: Vec::new() };
        for kind in each_kind()? {
            counter
                .fds
                .push(open_event(event, kind.as_ref(), modes, 0)?);
        }
        counter
            .enable()
            .map_err(|e| anyhow::anyhow!("enabling the counter: {e}"))?;
        Ok(counter)
    }

    pub fn read(&self) -> Result<u64> {
        let readings = self
            .fds
            .iter()
            .rev()
            .map(|&fd| read_reading(fd))
            .collect::<Result<Vec<_>>>()?;
        total(&readings)
    }

    /// Stops counting until `resume`. The counter counts the thread's
    /// guest, whichever machine's that is, so a machine that is not
    /// running pauses its counter while another runs on the thread.
    pub fn pause(&self) -> Result<()> {
        for &fd in self.fds.iter().rev() {
            // SAFETY: an ioctl on an fd we own.
            if unsafe { libc::ioctl(fd, PERF_EVENT_IOC_DISABLE as _, 0) } != 0 {
                bail!("pausing the counter: {}", io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        self.enable()
            .map_err(|e| anyhow::anyhow!("resuming the counter: {e}"))
    }

    fn enable(&self) -> io::Result<()> {
        for &fd in &self.fds {
            // SAFETY: an ioctl on an fd we own.
            if unsafe { libc::ioctl(fd, PERF_EVENT_IOC_ENABLE as _, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        for &fd in &self.fds {
            // SAFETY: closing an fd we own.
            unsafe { libc::close(fd) };
        }
    }
}

/// A second counter of the same event, used only for its overflow: armed
/// to overflow after a number of events, it signals the vCPU thread, which
/// pulls the vCPU out of KVM_RUN a little after that point.
///
/// On a hybrid host there is one per kind of core, and each is armed with a
/// share of the events (see [`shares`]), so that one of them overflows by
/// the time the kinds between them have counted all of them.
#[derive(Debug)]
pub struct Overflow {
    /// One per kind of core, with the kind, which a host with one kind of
    /// core has none of.
    fds: Vec<(i32, Option<KindOfCore>)>,
}

impl Overflow {
    /// Must be called on the vCPU's thread, which the signal is sent to.
    pub fn open(event: CounterEvent, modes: Modes) -> Result<Overflow> {
        install_signal_handler();
        let mut overflow = Overflow { fds: Vec::new() };
        for kind in each_kind()? {
            // A period so large it never fires until armed.
            let fd = open_event(event, kind.as_ref(), modes, 1 << 62)?;
            overflow.fds.push((fd, kind));
            // SAFETY: fcntl on an fd we own, with the calling thread's id.
            unsafe {
                let owner = FOwnerEx {
                    type_: F_OWNER_TID,
                    pid: libc::gettid(),
                };
                if libc::fcntl(fd, F_SETOWN_EX, &owner) != 0
                    || libc::fcntl(fd, F_SETSIG, overflow_signal()) != 0
                    || libc::fcntl(fd, libc::F_SETFL, libc::O_ASYNC) != 0
                {
                    bail!("routing counter overflows: {}", io::Error::last_os_error());
                }
            }
        }
        Ok(overflow)
    }

    /// Signals the thread after `events` more events, once: on a hybrid
    /// host, after at most that many between the kinds of core, and sooner
    /// when the thread moves to another kind, to be armed again there.
    pub fn arm(&self, events: u64) -> Result<()> {
        // SAFETY: no arguments, no memory.
        let cpu = usize::try_from(unsafe { libc::sched_getcpu() }).ok();
        let here = self
            .fds
            .iter()
            .position(|(_, kind)| {
                kind.as_ref()
                    .is_some_and(|k| cpu.is_some_and(|cpu| k.cpus.contains(&cpu)))
            })
            .unwrap_or(0);
        for ((fd, _), share) in self.fds.iter().zip(shares(events, self.fds.len(), here)) {
            // SAFETY: ioctls on an fd we own.
            unsafe {
                if libc::ioctl(*fd, PERF_EVENT_IOC_DISABLE as _, 0) != 0
                    || libc::ioctl(*fd, PERF_EVENT_IOC_RESET as _, 0) != 0
                    || libc::ioctl(*fd, PERF_EVENT_IOC_PERIOD as _, &share) != 0
                    || libc::ioctl(*fd, PERF_EVENT_IOC_REFRESH as _, 1) != 0
                {
                    bail!("arming the overflow: {}", io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }

    pub fn disarm(&self) -> Result<()> {
        for (fd, _) in &self.fds {
            // SAFETY: an ioctl on an fd we own.
            if unsafe { libc::ioctl(*fd, PERF_EVENT_IOC_DISABLE as _, 0) } != 0 {
                bail!("disarming the overflow: {}", io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// How many events a kind of core's counter, other than the one the thread
/// is on, counts before it signals that the thread moved there.
const AWAY: u64 = 1024;

/// The periods `kinds` counters are armed with to overflow after `events`
/// between them, with the thread on kind `here`. Until one overflows, each
/// has counted less than its period, so periods adding up to `events` plus
/// one less than the number of kinds make one overflow by `events`. The
/// kind the thread is on gets nearly all of them, so a thread that stays
/// there is signalled once, as with one counter.
fn shares(events: u64, kinds: usize, here: usize) -> Vec<u64> {
    let events = events.max(1);
    let others = kinds as u64 - 1;
    let away = (events / kinds as u64).clamp(1, AWAY);
    (0..kinds)
        .map(|kind| {
            if kind == here {
                events - others * (away - 1)
            } else {
                away
            }
        })
        .collect()
}

impl Drop for Overflow {
    fn drop(&mut self) {
        for (fd, _) in &self.fds {
            // SAFETY: closing an fd we own.
            unsafe { libc::close(*fd) };
        }
    }
}

/// A handler that does nothing: the signal's only job is to interrupt
/// KVM_RUN, which a pending signal with a handler does.
pub(crate) fn install_signal_handler() {
    extern "C" fn nothing(_: i32) {}
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: installing a handler that touches no state.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = nothing as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(overflow_signal(), &action, std::ptr::null_mut());
        }
    });
}

/// Where Linux lists its performance monitoring units.
const EVENT_SOURCES: &str = "/sys/bus/event_source/devices";

/// The PMUs Linux registers for the kinds of core of a hybrid Intel CPU:
/// its performance cores, its efficient cores, and on some parts its
/// low-power efficient cores. A CPU with one kind of core has none of the
/// last two.
const HYBRID_PMUS: [&str; 3] = ["cpu_core", "cpu_atom", "cpu_lowpower"];

/// The PMU of a host with one kind of core.
const ONLY_PMU: &str = "cpu";

/// One kind of core of a hybrid host, and its PMU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindOfCore {
    /// The PMU's name, such as `cpu_atom`.
    pub pmu: String,
    /// The perf type Linux gave the PMU.
    type_: u32,
    /// Where the PMU puts an event's fields in its config.
    format: Format,
    /// Its CPUs, as the kernel lists them, such as `4-15`.
    pub cpu_list: String,
    pub cpus: Vec<usize>,
}

impl KindOfCore {
    /// Whether the PMU counts performance or efficient cores. Some parts
    /// have low-power efficient cores too, which count as efficient ones.
    fn class(&self) -> CoreClass {
        if self.pmu == "cpu_core" {
            CoreClass::Performance
        } else {
            CoreClass::Efficient
        }
    }
}

/// Which cores a counter counts on: on a hybrid host its performance or
/// its efficient cores, and otherwise the one kind of core there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoreClass {
    Performance,
    Efficient,
    Only,
}

/// Where a PMU puts an event's code and umask in perf_event_attr.config,
/// as its sysfs format directory says: `config:8-15,40-47` for the umask of
/// a PMU whose umasks run past 8 bits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Format {
    event: String,
    umask: String,
}

impl Format {
    /// The format of the PMU at `dir`, or x86's classic one, an 8-bit event
    /// code and an 8-bit umask, where it lists none.
    fn read(dir: &Path) -> Format {
        let field = |name: &str, classic: &str| {
            std::fs::read_to_string(dir.join("format").join(name))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| classic.to_string())
        };
        Format {
            event: field("event", "config:0-7"),
            umask: field("umask", "config:8-15"),
        }
    }

    /// The config for `event` with `umask`.
    fn config(&self, event: u64, umask: u64) -> Result<u64> {
        Ok(place(event, &self.event)? | place(umask, &self.umask)?)
    }
}

/// `value` laid out in a perf_event_attr.config by one field of a PMU's
/// format, such as `config:8-15,40-47`: its low bits fill the first range,
/// the bits after them the next.
fn place(value: u64, format: &str) -> Result<u64> {
    let ranges = format
        .strip_prefix("config:")
        .with_context(|| format!("PMU format {format:?} is not a field of config"))?;
    let mut placed = 0u64;
    let mut rest = value;
    for range in ranges.split(',') {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let bit = |n: &str| -> Result<u32> {
            n.trim()
                .parse()
                .with_context(|| format!("PMU format {format:?}"))
        };
        let (first, last) = (bit(first)?, bit(last)?);
        if last < first || last >= u64::BITS {
            bail!("PMU format {format:?} has a range out of order or past bit 63");
        }

        // The range takes as many of the value's remaining low bits as it
        // is wide.
        let width = last - first + 1;
        let mask = u64::MAX >> (u64::BITS - width);
        placed |= (rest & mask) << first;
        rest = rest.checked_shr(width).unwrap_or(0);
    }
    if rest != 0 {
        bail!("{value:#x} is wider than PMU format {format:?}");
    }
    Ok(placed)
}

/// Intel's retired conditional branches, BR_INST_RETIRED.COND, are event
/// 0xc4, with a umask that depends on the microarchitecture and the kind
/// of core.
const INTEL_BRANCHES_EVENT: u64 = 0xc4;

/// BR_INST_RETIRED.COND's umask from Nehalem through Skylake and Cascade
/// Lake, and on any Intel CPU [`INTEL_BRANCHES`] does not list.
const INTEL_BRANCHES_UMASK: u64 = 0x01;

/// The Intel microarchitectures whose BR_INST_RETIRED.COND has another
/// umask than [`INTEL_BRANCHES_UMASK`], by CPUID family and model, for
/// their performance cores and their efficient cores, as Linux's perf event
/// tables give it (tools/perf/pmu-events/arch/x86, each one's
/// pipeline.json, with mapfile.csv for the models). On these, umask 0x01
/// counts only some conditional branches: the taken ones, or on Panther
/// Lake the taken backward ones.
struct IntelBranches {
    name: &'static str,
    family: u32,
    models: &'static [u32],
    performance: Option<u64>,
    efficient: Option<u64>,
}

const INTEL_BRANCHES: &[IntelBranches] = &[
    IntelBranches {
        name: "icelake",
        family: 6,
        models: &[0x7d, 0x7e],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "icelakex",
        family: 6,
        models: &[0x6a, 0x6c],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "tigerlake",
        family: 6,
        models: &[0x8c, 0x8d],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "rocketlake",
        family: 6,
        models: &[0xa7],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "sapphirerapids",
        family: 6,
        models: &[0x8f],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "emeraldrapids",
        family: 6,
        models: &[0xcf],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "graniterapids",
        family: 6,
        models: &[0xad, 0xae],
        performance: Some(0x11),
        efficient: None,
    },
    IntelBranches {
        name: "alderlake",
        family: 6,
        models: &[0x97, 0x9a, 0xb7, 0xba, 0xbf],
        performance: Some(0x11),
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "meteorlake",
        family: 6,
        models: &[0xaa, 0xac, 0xb5],
        performance: Some(0x11),
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "lunarlake",
        family: 6,
        models: &[0xbd],
        performance: Some(0x111),
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "arrowlake",
        family: 6,
        models: &[0xc5, 0xc6],
        performance: Some(0x111),
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "pantherlake",
        family: 6,
        models: &[0xcc, 0xd5, 0xe5],
        performance: Some(0x07),
        efficient: Some(0x07),
    },
    IntelBranches {
        name: "alderlaken",
        family: 6,
        models: &[0xbe],
        performance: None,
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "sierraforest",
        family: 6,
        models: &[0xaf],
        performance: None,
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "grandridge",
        family: 6,
        models: &[0xb6],
        performance: None,
        efficient: Some(0x7e),
    },
    IntelBranches {
        name: "clearwaterforest",
        family: 6,
        models: &[0xdd],
        performance: None,
        efficient: Some(0x07),
    },
    // Linux's table gives Nova Lake's performance cores only.
    IntelBranches {
        name: "novalake",
        family: 18,
        models: &[0x01, 0x03],
        performance: Some(0x07),
        efficient: None,
    },
];

/// BR_INST_RETIRED.COND's umask on `class` cores of an Intel CPU of this
/// family and model; an error where Linux's tables give none.
fn intel_branches_umask(cpu: CpuModel, class: CoreClass) -> Result<u64> {
    let Some(known) = INTEL_BRANCHES
        .iter()
        .find(|b| b.family == cpu.family && b.models.contains(&cpu.model))
    else {
        return Ok(INTEL_BRANCHES_UMASK);
    };
    let umask = match class {
        CoreClass::Performance => known.performance,
        CoreClass::Efficient => known.efficient,
        // A part with one kind of core lists only that kind's umask.
        CoreClass::Only => known.performance.or(known.efficient),
    };
    umask.with_context(|| {
        format!(
            "Linux's perf tables give no umask of Intel's retired conditional branches for \
             {class:?} cores of {} (CPU family {} model {:#x})",
            known.name, cpu.family, cpu.model
        )
    })
}

/// A CPU's family and model, with CPUID's extended fields folded in, as
/// /proc/cpuinfo gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuModel {
    family: u32,
    model: u32,
}

impl CpuModel {
    /// This host's.
    fn host() -> Result<CpuModel> {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").context("reading /proc/cpuinfo")?;
        CpuModel::parse(&cpuinfo)
    }

    /// The first CPU's, from /proc/cpuinfo's text.
    fn parse(cpuinfo: &str) -> Result<CpuModel> {
        let field = |name: &str| -> Result<u32> {
            cpuinfo
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == name).then(|| value.trim().parse().ok())?
                })
                .with_context(|| format!("/proc/cpuinfo gives no {name}"))
        };
        Ok(CpuModel {
            family: field("cpu family")?,
            model: field("model")?,
        })
    }
}

/// This host's kinds of core, one per PMU; none on a host with one kind of
/// core, where one counter counts on every CPU.
pub fn kinds_of_core() -> Result<Vec<KindOfCore>> {
    kinds_in(Path::new(EVENT_SOURCES))
}

fn kinds_in(sources: &Path) -> Result<Vec<KindOfCore>> {
    let mut kinds = Vec::new();
    for pmu in HYBRID_PMUS {
        let dir = sources.join(pmu);
        if !dir.exists() {
            continue;
        }
        let read = |file: &str| -> Result<String> {
            let path = dir.join(file);
            Ok(std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .trim()
                .to_string())
        };
        let type_ = read("type")?
            .parse()
            .with_context(|| format!("{}/type", dir.display()))?;
        let cpu_list = read("cpus")?;
        let cpus = parse_cpu_list(&cpu_list)?;
        kinds.push(KindOfCore {
            pmu: pmu.to_string(),
            type_,
            format: Format::read(&dir),
            cpu_list,
            cpus,
        });
    }
    if kinds.len() < 2 {
        kinds.clear();
    }
    Ok(kinds)
}

/// What a counter is opened on, once per kind of core: each kind on a
/// hybrid host, and once, on whatever PMU the event names, otherwise.
fn each_kind() -> Result<Vec<Option<KindOfCore>>> {
    let kinds = kinds_of_core()?;
    if kinds.is_empty() {
        return Ok(vec![None]);
    }
    Ok(kinds.into_iter().map(Some).collect())
}

/// A kernel CPU list, such as `0-3,8,10-11`.
fn parse_cpu_list(list: &str) -> Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in list.split(',').filter(|p| !p.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let parse = |n: &str| {
            n.trim()
                .parse::<usize>()
                .with_context(|| format!("CPU list {list:?}"))
        };
        cpus.extend(parse(first)?..=parse(last)?);
    }
    Ok(cpus)
}

/// The calling thread kept to some of its CPUs, until this is dropped and
/// it gets back the CPUs it had: for the self-test, to test one kind of
/// core's counter alone.
pub struct Pinned {
    previous: libc::cpu_set_t,
}

// By hand: libc's cpu_set_t has Debug only with its extra_traits feature.
impl std::fmt::Debug for Pinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pinned")
    }
}

impl Pinned {
    /// Keeps the calling thread to those of `cpus` it may already use;
    /// `None`, leaving the thread as it was, when it may use none of them.
    pub fn to(cpus: &[usize]) -> Result<Option<Pinned>> {
        // SAFETY: cpu_set_t is plain data; zeroed is the empty set.
        let mut previous: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: the calling thread's mask, into a set of the size given.
        if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut previous) } != 0 {
            bail!("reading the thread's CPUs: {}", io::Error::last_os_error());
        }
        // SAFETY: as above.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let mut any = false;
        for &cpu in cpus {
            // SAFETY: `cpu` is below CPU_SETSIZE, so inside the set.
            if cpu < libc::CPU_SETSIZE as usize && unsafe { libc::CPU_ISSET(cpu, &previous) } {
                // SAFETY: as above.
                unsafe { libc::CPU_SET(cpu, &mut set) };
                any = true;
            }
        }
        if !any {
            return Ok(None);
        }
        // SAFETY: the calling thread, a set of the size given.
        if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) } != 0 {
            bail!(
                "keeping the thread to CPUs {cpus:?}: {}",
                io::Error::last_os_error()
            );
        }
        Ok(Some(Pinned { previous }))
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        // SAFETY: the calling thread, a set of the size given. Failing
        // leaves the thread on fewer CPUs, which is harmless.
        unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &self.previous) };
    }
}

/// What a read of a counter opened with [`PERF_FORMAT_TOTAL_TIME_ENABLED`]
/// and [`PERF_FORMAT_TOTAL_TIME_RUNNING`] gives.
#[repr(C)]
#[derive(Debug, Default)]
struct Reading {
    value: u64,
    enabled_ns: u64,
    running_ns: u64,
}

/// The count of a counter's readings, one per kind of core, when between
/// them they were on the hardware for all the time they were all enabled.
///
/// With one counter that is the whole time it was enabled. The kernel
/// takes a counter off the hardware to share it among more events than
/// fit, and a count from part of the time is short by work the guest did,
/// so a run read it would not replay.
///
/// On a hybrid host each kind's counter runs only while the thread is on
/// that kind of core, so each runs for less time than it is enabled and
/// their running times add up instead. They are enabled in one order, and
/// disabled and read in the reverse, so the one enabled last was enabled
/// the least time, and for all of it, up to the moment it was read, the
/// others were enabled too and still counting: wherever the thread ran
/// then, one of them was running, unless it was taken off the hardware or
/// the thread ran on a core no PMU covers. Read in the order they were
/// enabled, the others would stop short of it by the time between reads.
fn total(readings: &[Reading]) -> Result<u64> {
    let running: u64 = readings.iter().map(|r| r.running_ns).sum();
    let enabled = readings.iter().map(|r| r.enabled_ns).min().unwrap_or(0);
    if running < enabled {
        if readings.len() == 1 {
            bail!(
                "the host took the guest's counter off the hardware: it counted for {running} of \
                 {enabled} ns, so its count is short. Another program on the host is using the \
                 performance counters, as perf does; stop it, or run on another host"
            );
        }
        bail!(
            "the guest's counters, one per kind of core, counted for {running} of {enabled} ns \
             between them, so their count is short. Another program on the host is using the \
             performance counters, as perf does, or the vCPU ran on a core none of them covers; \
             stop the other program, or run on another host"
        );
    }
    Ok(readings.iter().map(|r| r.value).sum())
}

fn read_reading(fd: i32) -> Result<Reading> {
    let mut reading = Reading::default();
    let len = size_of::<Reading>();
    // SAFETY: reading at most `len` bytes into a repr(C) struct of u64s.
    let n = unsafe { libc::read(fd, (&mut reading as *mut Reading).cast(), len) };

    // A pinned counter that could not get the hardware reads as end of
    // file.
    if n == 0 {
        bail!(
            "the host gave the guest's counter no place on the hardware: another program on the \
             host is using the performance counters, as perf does; stop it, or run on another host"
        );
    }
    if n != len as isize {
        return Err(io::Error::last_os_error()).context("reading the counter");
    }
    Ok(reading)
}

/// The perf type and config for `event` on a `cpu`, on `kind`'s PMU on a
/// hybrid host and on the one PMU, laid out by `only`, otherwise. A raw
/// event is opened under the PMU's own type, with the encoding it has on
/// that microarchitecture and kind of core; a hardware event names the
/// PMU in its config's upper half.
fn event_attr(
    event: CounterEvent,
    kind: Option<&KindOfCore>,
    cpu: CpuModel,
    only: &Format,
) -> Result<(u32, u64)> {
    let raw = kind.map_or(PERF_TYPE_RAW, |k| k.type_);
    Ok(match event {
        CounterEvent::AmdRetiredConditionalBranches => (raw, 0xd1),
        CounterEvent::IntelRetiredConditionalBranches => {
            let (class, format) = match kind {
                Some(k) => (k.class(), &k.format),
                None => (CoreClass::Only, only),
            };
            let umask = intel_branches_umask(cpu, class)?;
            (raw, format.config(INTEL_BRANCHES_EVENT, umask)?)
        }
        CounterEvent::Instructions => (
            PERF_TYPE_HARDWARE,
            kind.map_or(0, |k| u64::from(k.type_) << 32) | PERF_COUNT_HW_INSTRUCTIONS,
        ),
    })
}

/// Opens a guest-mode counter on the calling thread, disabled; with a
/// sample period, it overflows every that many events.
fn open_event(
    event: CounterEvent,
    kind: Option<&KindOfCore>,
    modes: Modes,
    sample_period: u64,
) -> Result<i32> {
    {
        let only = Format::read(&Path::new(EVENT_SOURCES).join(ONLY_PMU));
        let (type_, config) = event_attr(event, kind, CpuModel::host()?, &only)?;
        let mut flags = FLAG_DISABLED | FLAG_PINNED | FLAG_EXCLUDE_HV | FLAG_EXCLUDE_HOST;
        if modes == Modes::UserOnly {
            flags |= FLAG_EXCLUDE_KERNEL;
        }
        let attr = PerfEventAttr {
            type_,
            size: size_of::<PerfEventAttr>() as u32,
            config,
            flags,
            sample_period,
            read_format: PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING,
            ..Default::default()
        };
        // SAFETY: a valid attr for the duration of the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                0,  // this thread
                -1, // any CPU
                -1, // no group
                0,
            )
        } as i32;
        if fd < 0 {
            let err = io::Error::last_os_error();
            let on = kind.map(|k| format!(" on {}", k.pmu)).unwrap_or_default();
            bail!(
                "perf_event_open for {event:?}{on}: {err}; see /proc/sys/kernel/perf_event_paranoid"
            );
        }
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(value: u64, enabled_ns: u64, running_ns: u64) -> Reading {
        Reading {
            value,
            enabled_ns,
            running_ns,
        }
    }

    #[test]
    fn a_count_off_the_hardware_part_of_the_time_is_refused() {
        // A reading whose counter ran the whole time it was enabled gives
        // its count, as does one never enabled. One the kernel took off
        // the hardware for part of that time, as it does to share the
        // counters among more events than fit, is short and is refused.
        assert_eq!(total(&[reading(1234, 5000, 5000)]).unwrap(), 1234);
        assert_eq!(total(&[Reading::default()]).unwrap(), 0);

        let err = total(&[reading(1000, 5000, 4000)]).unwrap_err().to_string();
        assert!(err.contains("4000 of 5000 ns"), "{err}");
    }

    #[test]
    fn counts_split_across_kinds_of_core_add_up_unless_short() {
        // A thread that spent 600 ns on a performance core and 390 on an
        // efficient one: the efficient cores' counter, enabled second, was
        // enabled 10 ns less, and the two ran for all of that.
        assert_eq!(
            total(&[reading(60, 1000, 600), reading(39, 990, 390)]).unwrap(),
            99
        );
        // Taken off the hardware for 90 ns while both were enabled.
        let err = total(&[reading(60, 1000, 600), reading(30, 990, 300)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("900 of 990 ns"), "{err}");
    }

    #[test]
    fn one_of_the_shared_overflows_fires_by_the_count_wherever_the_thread_runs() {
        // Each counter has counted less than its period until it
        // overflows, so the periods may add up to one less than the
        // number of kinds past the count, and no more.
        for (events, kinds, here) in [(1, 2, 0), (3, 3, 2), (1_000_000, 2, 1), (5_000, 3, 0)] {
            let shares = shares(events, kinds, here);
            assert_eq!(shares.len(), kinds);
            assert!(shares.iter().all(|&s| s >= 1), "{shares:?}");
            assert_eq!(shares.iter().sum::<u64>(), events + kinds as u64 - 1);
        }
        // The kind the thread is on gets all but a little.
        assert_eq!(shares(1_000_000, 2, 1), [AWAY, 1_000_000 - AWAY + 1]);
        assert_eq!(shares(1_000_000, 1, 0), [1_000_000]);
    }

    #[test]
    fn kernel_cpu_lists_parse() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), [0, 1, 2, 3]);
        assert_eq!(
            parse_cpu_list("0-3,8,10-11").unwrap(),
            [0, 1, 2, 3, 8, 10, 11]
        );
        assert_eq!(parse_cpu_list("5").unwrap(), [5]);
        assert!(parse_cpu_list("").unwrap().is_empty());
        assert!(parse_cpu_list("0-x").is_err());
    }

    #[test]
    fn only_a_host_with_two_kinds_of_core_counts_on_each() {
        // As on an Intel Core Ultra: performance cores 0-3 under cpu_core,
        // efficient cores 4-15 under cpu_atom, at perf types 4 and 10, each
        // with the umask format of a PMU whose umasks run past 8 bits.
        let sources = std::env::temp_dir().join(format!("rewind-pmu-{}", std::process::id()));
        let pmu = |name: &str, type_: &str, cpus: &str| {
            let dir = sources.join(name);
            std::fs::create_dir_all(dir.join("format")).unwrap();
            std::fs::write(dir.join("type"), format!("{type_}\n")).unwrap();
            std::fs::write(dir.join("cpus"), format!("{cpus}\n")).unwrap();
            std::fs::write(dir.join("format/event"), "config:0-7\n").unwrap();
            std::fs::write(dir.join("format/umask"), "config:8-15,40-47\n").unwrap();
        };
        pmu("cpu_core", "4", "0-3");
        assert!(kinds_in(&sources).unwrap().is_empty());

        pmu("cpu_atom", "10", "4-15");
        let kinds = kinds_in(&sources).unwrap();
        std::fs::remove_dir_all(&sources).unwrap();
        assert_eq!(
            kinds
                .iter()
                .map(|k| (k.pmu.as_str(), k.cpus.len()))
                .collect::<Vec<_>>(),
            [("cpu_core", 4), ("cpu_atom", 12)]
        );

        // Each kind's counter is opened on its own PMU, with that kind's
        // umask: Panther Lake's 0x07 on both, Alder Lake's 0x11 and 0x7e,
        // and Lunar Lake's 0x111 on its performance cores, whose ninth bit
        // goes to config bit 40.
        let (core, atom) = (Some(&kinds[0]), Some(&kinds[1]));
        let only = Format::read(&sources);
        let attr = |kind, family, model| {
            event_attr(
                CounterEvent::IntelRetiredConditionalBranches,
                kind,
                CpuModel { family, model },
                &only,
            )
            .unwrap()
        };
        assert_eq!(attr(core, 6, 0xcc), (4, 0x07c4));
        assert_eq!(attr(atom, 6, 0xcc), (10, 0x07c4));
        assert_eq!(attr(core, 6, 0x97), (4, 0x11c4));
        assert_eq!(attr(atom, 6, 0x97), (10, 0x7ec4));
        assert_eq!(attr(core, 6, 0xbd), (4, (1 << 40) | 0x11c4));
        assert_eq!(
            event_attr(
                CounterEvent::Instructions,
                atom,
                CpuModel {
                    family: 6,
                    model: 0xcc
                },
                &only
            )
            .unwrap(),
            (PERF_TYPE_HARDWARE, (10 << 32) | PERF_COUNT_HW_INSTRUCTIONS)
        );
    }

    #[test]
    fn a_value_is_laid_out_by_the_pmus_format() {
        // A format's ranges take the value's bits in order, low bits
        // first; a value wider than them all is refused.
        assert_eq!(place(0xc4, "config:0-7").unwrap(), 0xc4);
        assert_eq!(place(0x11, "config:8-15").unwrap(), 0x1100);
        assert_eq!(
            place(0x111, "config:8-15,40-47").unwrap(),
            (1 << 40) | 0x1100
        );
        assert_eq!(place(1, "config:21").unwrap(), 1 << 21);
        assert!(place(0x111, "config:8-15").is_err());
        assert!(place(1, "config1:0-7").is_err());
    }

    #[test]
    fn intel_conditional_branches_take_each_microarchitectures_umask() {
        // From Linux's perf tables: 0x01 up to Skylake and on any model
        // they leave out, 0x11 from Ice Lake on, and the hybrid parts'
        // performance and efficient cores apart. A part with one kind of
        // core takes that kind's umask.
        let umask =
            |family, model, class| intel_branches_umask(CpuModel { family, model }, class).ok();
        use CoreClass::{Efficient, Only, Performance};
        assert_eq!(umask(6, 0x5e, Only), Some(0x01)); // Skylake
        assert_eq!(umask(6, 0x55, Only), Some(0x01)); // Skylake-X, Cascade Lake
        assert_eq!(umask(6, 0x7e, Only), Some(0x11)); // Ice Lake
        assert_eq!(umask(6, 0x8f, Only), Some(0x11)); // Sapphire Rapids
        assert_eq!(umask(6, 0x97, Performance), Some(0x11)); // Alder Lake
        assert_eq!(umask(6, 0x97, Efficient), Some(0x7e));
        assert_eq!(umask(6, 0xb7, Efficient), Some(0x7e)); // Raptor Lake
        assert_eq!(umask(6, 0xbd, Performance), Some(0x111)); // Lunar Lake
        assert_eq!(umask(6, 0xc6, Efficient), Some(0x7e)); // Arrow Lake
        assert_eq!(umask(6, 0xcc, Performance), Some(0x07)); // Panther Lake
        assert_eq!(umask(6, 0xcc, Efficient), Some(0x07));
        assert_eq!(umask(6, 0xaf, Only), Some(0x7e)); // Sierra Forest
        assert_eq!(umask(6, 0xdd, Only), Some(0x07)); // Clearwater Forest
        assert_eq!(umask(18, 0x01, Performance), Some(0x07)); // Nova Lake
        assert_eq!(umask(18, 0x01, Efficient), None);
    }

    #[test]
    fn the_cpu_family_and_model_come_from_cpuinfo() {
        // /proc/cpuinfo gives them in decimal with the extended fields
        // folded in: a Core Ultra X9 388H is family 6, model 204, 0xcc.
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\n\
                       model\t\t: 204\nmodel name\t: Intel(R) Core(TM) Ultra X9 388H\n";
        assert_eq!(
            CpuModel::parse(cpuinfo).unwrap(),
            CpuModel {
                family: 6,
                model: 0xcc
            }
        );
        assert!(CpuModel::parse("vendor_id\t: GenuineIntel\n").is_err());
    }
}
