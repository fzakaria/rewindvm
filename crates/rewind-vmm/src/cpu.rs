//! The vCPU's starting state, and the CPUID it reports.
//!
//! CPUID is where determinism starts. Anything the guest can read that
//! differs between runs must either be hidden or replaced with a value the
//! monitor controls, and the instructions KVM cannot trap on our behalf
//! (RDTSC, RDRAND, RDSEED) are best kept out of the guest's reach by never
//! advertising them.

use anyhow::{Context, Result};
use kvm_bindings::{
    CpuId, Msrs, kvm_cpuid_entry2, kvm_fpu, kvm_msr_entry, kvm_regs, kvm_segment, kvm_sregs,
};
use kvm_ioctls::{Kvm, VcpuFd};

use crate::CpuModel;
use crate::layout::*;

/// The boot GDT: null, 64-bit code, data, and a TSS the vCPU's task
/// register must point at.
pub const GDT_ENTRIES: [u64; 4] = [
    gdt_entry(0, 0, 0),
    gdt_entry(0xa09b, 0, 0xfffff),
    gdt_entry(0xc093, 0, 0xfffff),
    gdt_entry(0x808b, 0, 0xfffff),
];

const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    (((base as u64) & 0xff00_0000) << (56 - 24))
        | (((flags as u64) & 0x0000_f0ff) << 40)
        | (((limit as u64) & 0x000f_0000) << (48 - 16))
        | (((base as u64) & 0x00ff_ffff) << 16)
        | ((limit as u64) & 0x0000_ffff)
}

fn segment(index: u16) -> kvm_segment {
    let entry = GDT_ENTRIES[index as usize];
    let g = ((entry >> 55) & 1) as u8;
    let raw_limit = (((entry >> 32) & 0x000f_0000) | (entry & 0xffff)) as u32;
    let present = ((entry >> 47) & 1) as u8;
    kvm_segment {
        base: ((entry >> 16) & 0x00ff_ffff) | (((entry >> 56) & 0xff) << 24),
        limit: if g == 1 {
            (raw_limit << 12) | 0xfff
        } else {
            raw_limit
        },
        selector: index * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present,
        dpl: ((entry >> 45) & 3) as u8,
        db: ((entry >> 54) & 1) as u8,
        s: ((entry >> 44) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        g,
        avl: ((entry >> 52) & 1) as u8,
        padding: 0,
        unusable: if present == 1 { 0 } else { 1 },
    }
}

/// Long mode with paging on, pointed at the boot page tables and GDT.
pub fn setup_sregs(vcpu: &VcpuFd) -> Result<()> {
    const CR0_PE: u64 = 1 << 0;
    const CR0_PG: u64 = 1 << 31;
    const CR4_PAE: u64 = 1 << 5;
    const EFER_LME: u64 = 1 << 8;
    const EFER_LMA: u64 = 1 << 10;

    let mut sregs: kvm_sregs = vcpu.get_sregs()?;
    let code = segment(1);
    let data = segment(2);
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment(3);
    sregs.gdt.base = GDT;
    sregs.gdt.limit = (GDT_ENTRIES.len() * 8 - 1) as u16;
    sregs.idt.base = IDT;
    sregs.idt.limit = 7;

    sregs.cr3 = PML4;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
    vcpu.set_sregs(&sregs)?;
    Ok(())
}

/// The 64-bit boot protocol's register contract: RSI holds the zero page.
pub fn setup_regs(vcpu: &VcpuFd, entry: u64) -> Result<()> {
    let regs = kvm_regs {
        rflags: 0x2,
        rip: entry,
        rsp: BOOT_STACK,
        rbp: BOOT_STACK,
        rsi: BOOT_PARAMS,
        ..Default::default()
    };
    vcpu.set_regs(&regs)?;
    Ok(())
}

pub fn setup_fpu(vcpu: &VcpuFd) -> Result<()> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu)?;
    Ok(())
}

pub fn setup_msrs(vcpu: &VcpuFd) -> Result<()> {
    const IA32_SYSENTER_CS: u32 = 0x174;
    const IA32_SYSENTER_ESP: u32 = 0x175;
    const IA32_SYSENTER_EIP: u32 = 0x176;
    const STAR: u32 = 0xc000_0081;
    const LSTAR: u32 = 0xc000_0082;
    const CSTAR: u32 = 0xc000_0083;
    const SYSCALL_MASK: u32 = 0xc000_0084;
    const KERNEL_GS_BASE: u32 = 0xc000_0102;
    const IA32_TSC: u32 = 0x10;
    const IA32_MISC_ENABLE: u32 = 0x1a0;
    const MTRR_DEF_TYPE: u32 = 0x2ff;

    const MISC_ENABLE_FAST_STRING: u64 = 1;
    const MTRR_ENABLE_WRITEBACK: u64 = (1 << 11) | 6;

    let entries = [
        (IA32_SYSENTER_CS, 0),
        (IA32_SYSENTER_ESP, 0),
        (IA32_SYSENTER_EIP, 0),
        (STAR, 0),
        (CSTAR, 0),
        (KERNEL_GS_BASE, 0),
        (SYSCALL_MASK, 0),
        (LSTAR, 0),
        (IA32_TSC, 0),
        (IA32_MISC_ENABLE, MISC_ENABLE_FAST_STRING),
        (MTRR_DEF_TYPE, MTRR_ENABLE_WRITEBACK),
    ]
    .map(|(index, data)| kvm_msr_entry {
        index,
        data,
        ..Default::default()
    });
    let msrs = Msrs::from_entries(&entries).context("building MSR list")?;
    vcpu.set_msrs(&msrs)?;
    Ok(())
}

/// IA32_PERF_CAPABILITIES, which tells a guest what its performance
/// counters can do.
pub(crate) const IA32_PERF_CAPABILITIES: u32 = 0x345;

/// IA32_ARCH_CAPABILITIES, which tells a guest the speculation bugs the
/// CPU does not have and the mitigations it does.
pub(crate) const IA32_ARCH_CAPABILITIES: u32 = 0x10a;

/// CPUID leaf 7 EDX bit 29: the CPU has IA32_ARCH_CAPABILITIES.
const ARCH_CAPABILITIES: u32 = 1 << 29;

/// The MSRs that describe the CPU to the guest, each with the value the
/// CPU that `cpuid` shows has, of the ones KVM lists in `listed`. A Linux
/// before 6.13 starts every one at the most the host supports, whatever
/// the VM was given, and a later one does so for an MSR CPUID shows. A
/// keyframe holds what KVM reads back, so without these values a keyframe
/// would hold the host's.
///
/// IA32_PERF_CAPABILITIES is 0: the VM has no counters (see
/// kvm::turn_off_counters). On an Intel host KVM starts the MSR at the
/// host's LBR format and full-width counter bits all the same, and a
/// Linux before 6.13 refuses to be given any value but 0 for a vCPU
/// without counters, the value it started the MSR at included. A
/// keyframe would hold that value, and no restore could set it.
///
/// IA32_ARCH_CAPABILITIES is 0 for a CPU whose CPUID hides the MSR, as
/// the v3 model's does. A Linux before 6.13 reads back the host's value
/// there, and a later one reads back 0 and takes nothing else, so a
/// keyframe from the first would not restore on the second. A CPU whose
/// CPUID shows the MSR, as the host model's can, keeps the host's value.
fn capability_msrs(listed: &[u32], cpuid: &[kvm_cpuid_entry2]) -> Vec<(u32, u64)> {
    const STRUCTURED_FEATURES: u32 = 7;

    let mut msrs = vec![(IA32_PERF_CAPABILITIES, 0)];
    let arch_capabilities_shown = cpuid.iter().any(|leaf| {
        leaf.function == STRUCTURED_FEATURES && leaf.index == 0 && leaf.edx & ARCH_CAPABILITIES != 0
    });
    if !arch_capabilities_shown {
        msrs.push((IA32_ARCH_CAPABILITIES, 0));
    }

    // A KVM that does not list an MSR has no such MSR to set.
    msrs.retain(|(index, _)| listed.contains(index));
    msrs
}

/// Sets the MSRs that describe the CPU to what the CPU that `cpuid` shows
/// has, on a vCPU that has not run: KVM takes no other value for them
/// once the vCPU has. A machine that boots and one restored from a
/// keyframe both start here, so a keyframe holds these values on every
/// host.
pub fn setup_capability_msrs(kvm: &Kvm, vcpu: &VcpuFd, cpuid: &[kvm_cpuid_entry2]) -> Result<()> {
    let listed = kvm.get_msr_index_list()?;
    let entries: Vec<kvm_msr_entry> = capability_msrs(listed.as_slice(), cpuid)
        .into_iter()
        .map(|(index, data)| kvm_msr_entry {
            index,
            data,
            ..Default::default()
        })
        .collect();
    if entries.is_empty() {
        return Ok(());
    }

    let msrs = Msrs::from_entries(&entries).context("building MSR list")?;
    let n = vcpu.set_msrs(&msrs)?;
    if n != entries.len() {
        anyhow::bail!(
            "KVM refused {} as {:#x} on a new vCPU",
            crate::state::msr_name(entries[n].index),
            entries[n].data
        );
    }
    Ok(())
}

/// LINT0 as ExtINT and LINT1 as NMI, the way firmware leaves the local APIC
/// for an operating system.
pub fn setup_lapic(vcpu: &VcpuFd) -> Result<()> {
    const LVT_LINT0: usize = 0x350;
    const LVT_LINT1: usize = 0x360;
    const DELIVERY_EXTINT: u32 = 7;
    const DELIVERY_NMI: u32 = 4;

    let mut lapic = vcpu.get_lapic()?;
    for (reg, mode) in [(LVT_LINT0, DELIVERY_EXTINT), (LVT_LINT1, DELIVERY_NMI)] {
        let bytes: &mut [u8] =
            // SAFETY: kvm_lapic_state.regs is a plain 1 KiB register page.
            unsafe { std::slice::from_raw_parts_mut(lapic.regs.as_mut_ptr().cast(), 1024) };
        let old = u32::from_le_bytes(bytes[reg..reg + 4].try_into().expect("four bytes"));
        let new = (old & !0x700) | (mode << 8);
        bytes[reg..reg + 4].copy_from_slice(&new.to_le_bytes());
    }
    vcpu.set_lapic(&lapic)?;
    Ok(())
}

/// The signature at leaf 0x40000000 the guest kernel looks for.
pub const SIGNATURE: &[u8; 12] = b"RewindRewind";

const HYPERVISOR_LEAF: u32 = 0x4000_0000;

/// The host's CPUID shaped for a deterministic guest of the given model.
/// A new run's model is checked with [`check`] before its machine is
/// built, so a refusal here is of a recorded run's model, and says where
/// the run replays rather than which --cpu to run with.
pub fn cpuid(kvm: &Kvm, model: CpuModel) -> Result<CpuId> {
    let host = host_cpuid(kvm)?;
    let entries = match model {
        CpuModel::Host => host,
        CpuModel::V3 | CpuModel::V3Compat => baseline(&host, model)?,
    };
    CpuId::from_entries(&entries).context("building CPUID")
}

/// The model a run that asks for none gets on a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultModel {
    /// The host has the whole x86-64-v3 baseline.
    V3,
    /// The host lacks ERMS, PREFETCHW or both, and nothing else.
    /// `lacking` names the bits the host lacks, as "ERMS (leaf 7 EBX bit
    /// 9)", joined by "and" where the host lacks both.
    V3Compat { lacking: String },
    /// The host lacks a bit that v3-compat shows the guest, so only
    /// `--cpu host` records there.
    /// `lacking` names the missing bits, as "leaf 7 EBX bits 0x220".
    HostOnly { lacking: String },
}

impl DefaultModel {
    /// The model a run gets, or for a host that leaves only the host model
    /// a refusal naming the missing bits.
    pub fn model(&self) -> Result<CpuModel> {
        match self {
            DefaultModel::V3 => Ok(CpuModel::V3),
            DefaultModel::V3Compat { .. } => Ok(CpuModel::V3Compat),
            DefaultModel::HostOnly { lacking } => {
                anyhow::bail!("this CPU lacks the x86-64-v3 model's {lacking}; run with --cpu host")
            }
        }
    }
}

/// The model a run that asks for none gets on this host.
pub fn default_for(kvm: &Kvm) -> Result<DefaultModel> {
    classify(&host_cpuid(kvm)?)
}

/// The model a run shows the guest when none is asked for: v3 on a host
/// with the whole x86-64-v3 baseline, or v3-compat on one that lacks only
/// ERMS or PREFETCHW. Fails, naming the missing bits, on any other host.
pub fn default_model(kvm: &Kvm) -> Result<CpuModel> {
    default_for(kvm)?.model()
}

/// Fails, naming the missing bits and the models that would run, when this
/// host cannot show a new run `model`.
pub fn check(kvm: &Kvm, model: CpuModel) -> Result<()> {
    supports(&host_cpuid(kvm)?, model, RunKind::New)
}

/// The bits of `lacking` that v3-compat hides, as a refusal names them:
/// the short names, as "ERMS and PREFETCHW", and the names with the bits,
/// as "ERMS (leaf 7 EBX bit 9)".
fn compat_named(lacking: Features) -> (String, String) {
    let all = [
        ("ERMS", "ERMS (leaf 7 EBX bit 9)", lacking.leaf7_ebx & ERMS),
        (
            "PREFETCHW",
            "PREFETCHW (leaf 0x80000001 ECX bit 8)",
            lacking.ext1_ecx & PREFETCHW,
        ),
    ];
    let have: Vec<_> = all.iter().filter(|(_, _, bit)| *bit != 0).collect();
    let short = have.iter().map(|(s, _, _)| *s).collect::<Vec<_>>();
    let long = have.iter().map(|(_, l, _)| *l).collect::<Vec<_>>();
    (short.join(" and "), long.join(" and "))
}

/// The run a model is asked for: a new one, which can take another model
/// with --cpu, or a recorded one, whose model is part of its inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunKind {
    New,
    Recorded,
}

/// The feature bits the baseline keeps, by leaf, subleaf and register.
/// x86-64-v3, plus AES, PCLMULQDQ, ERMS, PREFETCHW, FSGSBASE and INVPCID.
/// Not every host that has x86-64-v3 shows ERMS and PREFETCHW: an EPYC
/// 7763 (Zen 3) host was seen without ERMS, and Haswell hosts do not show
/// PREFETCHW. The v3-compat model is for those hosts.
mod v3 {
    // OSXSAVE (bit 27) is left out: KVM sets it itself from the guest's
    // CR4.
    pub(super) const LEAF1_ECX: u32 = bits(&[0, 1, 9, 12, 13, 19, 20, 22, 23, 25, 26, 28, 29]);
    pub(super) const LEAF1_EDX: u32 = bits(&[
        0, 1, 2, 3, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 17, 19, 23, 24, 25, 26,
    ]);
    pub(super) const LEAF7_EBX: u32 = bits(&[0, 3, 5, 7, 8, 9, 10]);
    pub(super) const EXT1_ECX: u32 = bits(&[0, 5, 8]);
    pub(super) const EXT1_EDX: u32 = bits(&[11, 20, 26, 29]);

    /// XSAVE components: x87, SSE and AVX.
    pub(super) const XCR0: u32 = 0b111;
    /// The standard XSAVE area with those three: 512 legacy bytes, a 64
    /// byte header, and 256 bytes of AVX upper halves.
    pub(super) const XSAVE_SIZE: u32 = 832;
    /// XSAVEOPT only: compacted formats would change the kernel's layout.
    pub(super) const XSAVE_LEAF1_EAX: u32 = 1;

    /// The highest basic and extended leaves the baseline describes.
    pub(super) const MAX_BASIC: u32 = 0xd;
    pub(super) const MAX_EXTENDED: u32 = 0x8000_0008;

    /// 39 physical and 48 virtual address bits, which every v3 CPU covers.
    pub(super) const ADDRESS_BITS: u32 = (48 << 8) | 39;

    /// Family, model and stepping, per vendor: Zen 2 and Haswell, the
    /// first v3 cores of each.
    pub(super) const SIGNATURE_AMD: u32 = 0x0083_0f10;
    pub(super) const SIGNATURE_INTEL: u32 = 0x0003_06c3;

    /// Cache sizes, in AMD's leaf 0x80000005 and 0x80000006 formats, which
    /// glibc reads to size its copy strategies: 32 KiB 8-way L1s, a 512
    /// KiB 8-way L2 and a 16 MiB L3, all with 64 byte lines.
    pub(super) const L1: u32 = (32 << 24) | (8 << 16) | (1 << 8) | 64;
    pub(super) const L2: u32 = (512 << 16) | (0x6 << 12) | (1 << 8) | 64;
    pub(super) const L3: u32 = (32 << 18) | (0x8 << 12) | (1 << 8) | 64;

    pub(super) const BRAND: &[u8; 48] =
        b"Rewind VM x86-64-v3 virtual CPU\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

    const fn bits(list: &[u32]) -> u32 {
        let mut v = 0;
        let mut i = 0;
        while i < list.len() {
            v |= 1 << list[i];
            i += 1;
        }
        v
    }
}

/// What the v3-compat model shows the guest that the v3 model's constants
/// do not give: its brand string.
mod v3_compat {
    /// The brand string names the model, so that the guest, and anyone
    /// reading its /proc/cpuinfo or a log it wrote, can tell a v3-compat
    /// run from a v3 run. Sharing the v3 brand would buy no replayability:
    /// a v3-compat run already differs from a v3 run by ERMS and
    /// PREFETCHW. "x86-64-v3" stays in the name because the model is still
    /// x86-64-v3, a level that includes neither.
    pub(super) const BRAND: &[u8; 48] =
        b"Rewind VM x86-64-v3-compat virtual CPU\0\0\0\0\0\0\0\0\0\0";
}

/// ERMS, fast REP MOVSB and STOSB: leaf 7 EBX bit 9. glibc and the kernel
/// choose how to copy and clear memory by it, so a guest shown ERMS takes
/// different steps from one that is not.
const ERMS: u32 = 1 << 9;

/// PREFETCHW, the prefetch for a write: leaf 0x80000001 ECX bit 8. Intel
/// shows it from Broadwell on, thus a Haswell host does not show it. The
/// kernel and glibc use it only as a hint, so a guest that is not shown it
/// prefetches less but takes the same steps otherwise.
const PREFETCHW: u32 = 1 << 8;

/// The leaves that hold the brand string, 16 bytes each.
const BRAND_FIRST: u32 = 0x8000_0002;
const BRAND_LAST: u32 = 0x8000_0004;

/// The leaf of AMD's extended features, which Intel also fills.
const EXT1: u32 = 0x8000_0001;

/// The registers a fixed model takes feature bits from, and the bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Features {
    leaf1_ecx: u32,
    leaf1_edx: u32,
    leaf7_ebx: u32,
    ext1_ecx: u32,
    ext1_edx: u32,
}

impl Features {
    /// No bits in any register.
    const NONE: Features = Features {
        leaf1_ecx: 0,
        leaf1_edx: 0,
        leaf7_ebx: 0,
        ext1_ecx: 0,
        ext1_edx: 0,
    };

    /// The v3 model's features.
    const V3: Features = Features {
        leaf1_ecx: v3::LEAF1_ECX,
        leaf1_edx: v3::LEAF1_EDX,
        leaf7_ebx: v3::LEAF7_EBX,
        ext1_ecx: v3::EXT1_ECX,
        ext1_edx: v3::EXT1_EDX,
    };

    /// The v3-compat model's features: the v3 model's, less ERMS and
    /// PREFETCHW.
    const V3_COMPAT: Features = Features {
        leaf7_ebx: v3::LEAF7_EBX & !ERMS,
        ext1_ecx: v3::EXT1_ECX & !PREFETCHW,
        ..Features::V3
    };

    /// ERMS and PREFETCHW: the bits of the v3 model that v3-compat hides,
    /// thus that a host the v3-compat model is for can lack.
    const COMPAT_HIDDEN: Features = Features {
        leaf7_ebx: ERMS,
        ext1_ecx: PREFETCHW,
        ..Features::NONE
    };

    /// The features a host's shaped CPUID shows.
    fn of(host: &[kvm_cpuid_entry2]) -> Result<Features> {
        let find = |function: u32| {
            host.iter()
                .find(|e| e.function == function && e.index == 0)
                .copied()
        };
        let leaf1 = find(1).context("no CPUID leaf 1")?;
        let leaf7 = find(7).unwrap_or_default();
        let ext1 = find(EXT1).unwrap_or_default();
        Ok(Features {
            leaf1_ecx: leaf1.ecx,
            leaf1_edx: leaf1.edx,
            leaf7_ebx: leaf7.ebx,
            ext1_ecx: ext1.ecx,
            ext1_edx: ext1.edx,
        })
    }

    /// The bits of `self` that `have` lacks.
    fn lacking(self, have: Features) -> Features {
        Features {
            leaf1_ecx: self.leaf1_ecx & !have.leaf1_ecx,
            leaf1_edx: self.leaf1_edx & !have.leaf1_edx,
            leaf7_ebx: self.leaf7_ebx & !have.leaf7_ebx,
            ext1_ecx: self.ext1_ecx & !have.ext1_ecx,
            ext1_edx: self.ext1_edx & !have.ext1_edx,
        }
    }

    /// The registers that hold any bits, with those bits, as a refusal
    /// names them: "leaf 1 ECX bits 0x1000, leaf 7 EBX bits 0x200".
    fn describe(self) -> String {
        let registers = [
            ("leaf 1 ECX", self.leaf1_ecx),
            ("leaf 1 EDX", self.leaf1_edx),
            ("leaf 7 EBX", self.leaf7_ebx),
            ("leaf 0x80000001 ECX", self.ext1_ecx),
            ("leaf 0x80000001 EDX", self.ext1_edx),
        ];
        registers
            .iter()
            .filter(|(_, bits)| *bits != 0)
            .map(|(register, bits)| format!("{register} bits {bits:#x}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A fixed model: the name a refusal gives it, the features it shows the
/// guest, and its brand string. Everything else, from the signature to
/// the cache sizes and the XSAVE layout, is the same for both fixed
/// models.
struct Fixed {
    name: &'static str,
    features: Features,
    brand: &'static [u8; 48],
}

impl Fixed {
    /// The fixed model `model` is, or None for the host model.
    fn of(model: CpuModel) -> Option<Fixed> {
        match model {
            CpuModel::Host => None,
            CpuModel::V3 => Some(Fixed {
                name: "x86-64-v3",
                features: Features::V3,
                brand: v3::BRAND,
            }),
            CpuModel::V3Compat => Some(Fixed {
                name: "v3-compat",
                features: Features::V3_COMPAT,
                brand: v3_compat::BRAND,
            }),
        }
    }
}

/// The model for a run that asks for none: v3 where the host has every v3
/// feature, v3-compat where it lacks only ERMS or PREFETCHW, and otherwise
/// only the host model.
fn classify(host: &[kvm_cpuid_entry2]) -> Result<DefaultModel> {
    let lacking = Features::V3.lacking(Features::of(host)?);
    if lacking == Features::NONE {
        return Ok(DefaultModel::V3);
    }
    if lacking.lacking(Features::COMPAT_HIDDEN) == Features::NONE {
        let (_, named) = compat_named(lacking);
        return Ok(DefaultModel::V3Compat { lacking: named });
    }

    // The host lacks a bit that neither fixed model hides.
    Ok(DefaultModel::HostOnly {
        lacking: lacking.describe(),
    })
}

/// Whether a host can show the guest `model` for `run`: a fixed model's
/// features must all be the host's, and the host model is always shown.
/// A refusal of a new run names the --cpu models that would record here,
/// and of a recorded run where the run replays.
fn supports(host: &[kvm_cpuid_entry2], model: CpuModel, run: RunKind) -> Result<()> {
    let Some(fixed) = Fixed::of(model) else {
        return Ok(());
    };
    let lacking = fixed.features.lacking(Features::of(host)?);
    if lacking == Features::NONE {
        return Ok(());
    }

    // Whether the v3 model lacks only bits that v3-compat hides here, where
    // v3-compat would record, and the model's name as --cpu takes it.
    let compat_only =
        model == CpuModel::V3 && lacking.lacking(Features::COMPAT_HIDDEN) == Features::NONE;
    let (short, named) = compat_named(lacking);
    let name = model.cli_name();

    // A recorded run cannot change its model: it replays where the host
    // has the features, and recording it again here is another run.
    if run == RunKind::Recorded {
        if compat_only {
            anyhow::bail!(
                "this {name} run needs a CPU with {named}, which this CPU lacks; replay it \
                 on a host with {short}. Recording it again here with --cpu v3-compat or --cpu host \
                 makes a different run"
            );
        }
        anyhow::bail!(
            "this {name} run needs the {name} model's {}, which this CPU lacks; replay it on a \
             host that has them. Recording it again here with --cpu host makes a different run",
            lacking.describe()
        );
    }

    // A new v3 run on a host that lacks only bits that v3-compat hides: the
    // same model with those bits hidden runs there.
    if compat_only {
        anyhow::bail!(
            "this CPU lacks {named}, which the x86-64-v3 model shows the guest; run with \
             --cpu v3-compat, the same model without ERMS and PREFETCHW, or --cpu host"
        );
    }
    anyhow::bail!(
        "this CPU lacks the {} model's {}; run with --cpu host",
        fixed.name,
        lacking.describe()
    )
}

/// The fixed model `model` from the host's shaped CPUID. Fails, as for a
/// recorded run, when the host lacks a feature the model shows the guest,
/// and for the host model, which is not fixed.
fn baseline(host: &[kvm_cpuid_entry2], model: CpuModel) -> Result<Vec<kvm_cpuid_entry2>> {
    let fixed = Fixed::of(model).context("the host CPU model has no baseline")?;
    supports(host, model, RunKind::Recorded)?;

    // The host's vendor, which the fixed models keep.
    let find = |f: u32, i: u32| {
        host.iter()
            .find(|e| e.function == f && e.index == i)
            .copied()
    };
    let leaf0 = find(0, 0).context("no CPUID leaf 0")?;
    let amd = leaf0.ebx == u32::from_le_bytes(*b"Auth");
    let features = fixed.features;

    let entry =
        |function: u32, index: u32, eax: u32, ebx: u32, ecx: u32, edx: u32| kvm_cpuid_entry2 {
            function,
            index,
            flags: find(function, index).map_or(0, |e| e.flags),
            eax,
            ebx,
            ecx,
            edx,
            ..Default::default()
        };
    let signature = if amd {
        v3::SIGNATURE_AMD
    } else {
        v3::SIGNATURE_INTEL
    };
    let brand = |i: usize| {
        u32::from_le_bytes(
            fixed.brand[i * 4..i * 4 + 4]
                .try_into()
                .expect("four bytes"),
        )
    };

    let mut out = vec![
        entry(0, 0, v3::MAX_BASIC, leaf0.ebx, leaf0.ecx, leaf0.edx),
        // One logical processor, APIC ID 0, 64 byte cache lines.
        entry(
            1,
            0,
            signature,
            (1 << 16) | (8 << 8),
            features.leaf1_ecx | (1 << 31),
            features.leaf1_edx,
        ),
        entry(7, 0, 0, features.leaf7_ebx, 0, 0),
        entry(0xd, 0, v3::XCR0, v3::XSAVE_SIZE, v3::XSAVE_SIZE, 0),
        entry(0xd, 1, v3::XSAVE_LEAF1_EAX, 0, 0, 0),
        entry(
            0x8000_0000,
            0,
            v3::MAX_EXTENDED,
            leaf0.ebx,
            leaf0.ecx,
            leaf0.edx,
        ),
        entry(EXT1, 0, 0, 0, features.ext1_ecx, features.ext1_edx),
        entry(BRAND_FIRST, 0, brand(0), brand(1), brand(2), brand(3)),
        entry(BRAND_FIRST + 1, 0, brand(4), brand(5), brand(6), brand(7)),
        entry(BRAND_LAST, 0, brand(8), brand(9), brand(10), brand(11)),
        entry(0x8000_0005, 0, 0, 0, v3::L1, v3::L1),
        entry(0x8000_0006, 0, 0, 0, v3::L2, v3::L3),
        entry(0x8000_0008, 0, v3::ADDRESS_BITS, 0, 0, 0),
    ];
    // The AVX component's size and offset, as the host reports them; they
    // are architectural.
    if let Some(avx) = find(0xd, 2) {
        out.push(avx);
    }
    out.extend(
        host.iter()
            .filter(|e| e.function >= HYPERVISOR_LEAF && e.function < 0x5000_0000),
    );
    Ok(out)
}

/// The physical address bits this host's memory slots can reach: the
/// PhysAddrSize KVM reports in leaf 0x80000008. Since Linux 5.18, KVM
/// refuses a slot that ends past them.
pub fn address_bits(kvm: &Kvm) -> Result<u32> {
    let supported = kvm.get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)?;
    let leaf = supported
        .as_slice()
        .iter()
        .find(|e| e.function == 0x8000_0008)
        .context("no CPUID leaf 0x80000008")?;
    Ok(leaf.eax & 0xff)
}

/// The host's CPUID, filtered down to what a deterministic guest may see.
fn host_cpuid(kvm: &Kvm) -> Result<Vec<kvm_cpuid_entry2>> {
    let supported = kvm.get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)?;
    let mut entries: Vec<kvm_cpuid_entry2> = supported
        .as_slice()
        .iter()
        .copied()
        // KVM's own paravirtual leaves would invite the guest to use
        // kvmclock, which reads host time.
        .filter(|e| !(0x4000_0000..0x5000_0000).contains(&e.function))
        // Topology and frequency leaves describe the host, not this one
        // vCPU, and the frequency leaves exist only to calibrate a TSC the
        // guest is not given.
        .filter(|e| !matches!(e.function, 0xb | 0x15 | 0x16 | 0x1f | 0x8000_001e))
        .collect();

    for e in entries.iter_mut() {
        match (e.function, e.index) {
            (0x1, _) => {
                // One logical processor, APIC ID 0.
                e.ebx &= 0x0000_ffff;
                e.ebx |= 1 << 16;
                e.edx &= !(1 << 28); // HTT
                e.edx &= !(1 << 4); // TSC
                e.ecx &= !(1 << 30); // RDRAND
                e.ecx &= !(1 << 24); // TSC deadline timer
                e.ecx &= !(1 << 21); // x2APIC
                e.ecx &= !(1 << 5); // VMX
                e.ecx &= !(1 << 3); // MONITOR/MWAIT
                e.ecx |= 1 << 31; // running under a hypervisor
            }
            (0x4, _) => {
                // One core and one thread per cache.
                e.eax &= 0x0000_3fff;
            }
            (0x6, _) => {
                // No thermal or power management.
                *e = kvm_cpuid_entry2 {
                    function: e.function,
                    index: e.index,
                    flags: e.flags,
                    ..Default::default()
                };
            }
            (0x7, 0) => {
                e.ebx &= !(1 << 18); // RDSEED
                e.ebx &= !(1 << 1); // TSC_ADJUST
                e.ecx &= !(1 << 5); // WAITPKG: TPAUSE counts in TSC cycles
                e.ecx &= !(1 << 22); // RDPID reads TSC_AUX
            }
            (0xa, _) => {
                // No performance counters: RDPMC and perf both read host
                // counts.
                *e = kvm_cpuid_entry2 {
                    function: e.function,
                    index: e.index,
                    flags: e.flags,
                    ..Default::default()
                };
            }
            (0x8000_0001, _) => {
                e.edx &= !(1 << 27); // RDTSCP
                e.ecx &= !(1 << 22); // TOPOEXT
                e.ecx &= !(1 << 2); // SVM
            }
            (0x8000_0007, _) => {
                e.edx &= !(1 << 8); // invariant TSC
            }
            (0x8000_0008, _) => {
                // One core.
                e.ecx &= !0xff;
            }
            _ => {}
        }
    }

    let sig = SIGNATURE;
    let word = |i: usize| u32::from_le_bytes(sig[i * 4..i * 4 + 4].try_into().expect("four bytes"));
    entries.push(kvm_cpuid_entry2 {
        function: HYPERVISOR_LEAF,
        eax: HYPERVISOR_LEAF + 1,
        ebx: word(0),
        ecx: word(1),
        edx: word(2),
        ..Default::default()
    });
    entries.push(kvm_cpuid_entry2 {
        function: HYPERVISOR_LEAF + 1,
        ..Default::default()
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    // The fixed CPU models and which one a host gets, from synthetic
    // hosts' shaped CPUID: no KVM is needed.
    use super::*;

    /// A host's shaped CPUID with every feature the x86-64-v3 model shows,
    /// less the bits in `lacking`, on an AMD CPU.
    fn host(lacking: Features) -> Vec<kvm_cpuid_entry2> {
        let word = |s: &[u8; 4]| u32::from_le_bytes(*s);
        let have = Features::V3.lacking(lacking);
        let leaf = |function: u32, ebx: u32, ecx: u32, edx: u32| kvm_cpuid_entry2 {
            function,
            ebx,
            ecx,
            edx,
            ..Default::default()
        };
        vec![
            leaf(0, word(b"Auth"), word(b"cAMD"), word(b"enti")),
            leaf(1, 0, have.leaf1_ecx, have.leaf1_edx),
            leaf(7, have.leaf7_ebx, 0, 0),
            leaf(0x8000_0001, 0, have.ext1_ecx, have.ext1_edx),
        ]
    }

    /// What a host with the whole baseline lacks: no bits at all.
    const NOTHING: Features = Features::NONE;

    /// A host that lacks ERMS alone, and one that lacks PREFETCHW alone.
    const ERMS_ONLY: Features = Features {
        leaf7_ebx: ERMS,
        ..NOTHING
    };
    const PREFETCHW_ONLY: Features = Features {
        ext1_ecx: PREFETCHW,
        ..NOTHING
    };

    #[test]
    fn a_vm_without_counters_reports_no_perf_capabilities() {
        // Where KVM lists IA32_PERF_CAPABILITIES, a new vCPU's is set to
        // 0, what a VM with no counters has. Where KVM does not list the
        // MSR, nothing is set.
        const LSTAR: u32 = 0xc000_0082;
        let cpuid = [arch_capabilities_leaf(Shown::Yes)];
        assert_eq!(
            capability_msrs(&[LSTAR, IA32_PERF_CAPABILITIES], &cpuid),
            [(IA32_PERF_CAPABILITIES, 0)]
        );
        assert_eq!(capability_msrs(&[LSTAR], &cpuid), []);
    }

    /// Whether a CPUID shows the guest IA32_ARCH_CAPABILITIES.
    enum Shown {
        Yes,
        No,
    }

    /// CPUID leaf 7 with or without the bit for IA32_ARCH_CAPABILITIES.
    fn arch_capabilities_leaf(shown: Shown) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function: 7,
            edx: match shown {
                Shown::Yes => ARCH_CAPABILITIES,
                Shown::No => 0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn arch_capabilities_are_zero_for_a_cpu_that_hides_them() {
        // A CPUID without the bit for IA32_ARCH_CAPABILITIES, as the v3
        // model's is, and one with no leaf 7 at all: the MSR is set to 0.
        // A CPUID with the bit, as the host model's can be: the MSR is
        // left as KVM started it, at the host's.
        let listed = [IA32_ARCH_CAPABILITIES, IA32_PERF_CAPABILITIES];
        let both = [(IA32_PERF_CAPABILITIES, 0), (IA32_ARCH_CAPABILITIES, 0)];
        assert_eq!(
            capability_msrs(&listed, &[arch_capabilities_leaf(Shown::No)]),
            both
        );
        assert_eq!(capability_msrs(&listed, &[]), both);
        assert_eq!(
            capability_msrs(&listed, &[arch_capabilities_leaf(Shown::Yes)]),
            [(IA32_PERF_CAPABILITIES, 0)]
        );
    }

    #[test]
    fn the_v3_model_is_pinned() {
        // Every recorded v3 run's id and steps rest on what the v3 model
        // shows the guest, so its CPUID is written out here bit by bit,
        // computed apart from the bit lists in `mod v3`, and then read
        // back from the leaves the v3 model builds on a host with the
        // whole baseline. A change to any of these values makes every v3
        // run so far unreplayable.
        assert_eq!(v3::LEAF1_ECX, 0x36d8_3203);
        assert_eq!(v3::LEAF1_EDX, 0x078b_fbef);
        assert_eq!(v3::LEAF7_EBX, 0x0000_07a9);
        assert_eq!(v3::EXT1_ECX, 0x0000_0121);
        assert_eq!(v3::EXT1_EDX, 0x2410_0800);
        assert_eq!(v3::XCR0, 0b111);
        assert_eq!(v3::XSAVE_SIZE, 832);
        assert_eq!(v3::XSAVE_LEAF1_EAX, 1);
        assert_eq!(v3::MAX_BASIC, 0xd);
        assert_eq!(v3::MAX_EXTENDED, 0x8000_0008);
        assert_eq!(v3::ADDRESS_BITS, 0x3027);
        assert_eq!(v3::SIGNATURE_AMD, 0x0083_0f10);
        assert_eq!(v3::SIGNATURE_INTEL, 0x0003_06c3);
        assert_eq!(v3::L1, 0x2008_0140);
        assert_eq!(v3::L2, 0x0200_6140);
        assert_eq!(v3::L3, 0x0080_8140);
        assert_eq!(&v3::BRAND[..31], b"Rewind VM x86-64-v3 virtual CPU");
        assert!(v3::BRAND[31..].iter().all(|&b| b == 0));
        assert_eq!(Features::V3.leaf7_ebx & ERMS, ERMS);

        // The leaves the v3 model builds carry those values.
        const HYPERVISOR_BIT: u32 = 1 << 31;
        let entries = baseline(&host(NOTHING), CpuModel::V3).unwrap();
        let leaf = |function: u32| {
            *entries
                .iter()
                .find(|e| e.function == function && e.index == 0)
                .unwrap()
        };
        assert_eq!(leaf(1).eax, 0x0083_0f10);
        assert_eq!(leaf(1).ecx, 0x36d8_3203 | HYPERVISOR_BIT);
        assert_eq!(leaf(1).edx, 0x078b_fbef);
        assert_eq!(leaf(7).ebx, 0x0000_07a9);
        assert_eq!((leaf(EXT1).ecx, leaf(EXT1).edx), (0x0000_0121, 0x2410_0800));
        let brand: Vec<u8> = (BRAND_FIRST..=BRAND_LAST)
            .flat_map(|function| {
                let e = leaf(function);
                [e.eax, e.ebx, e.ecx, e.edx]
            })
            .flat_map(u32::to_le_bytes)
            .collect();
        assert_eq!(&brand[..], &v3::BRAND[..]);
    }

    #[test]
    fn the_compat_model_is_v3_without_erms_and_prefetchw() {
        // Both models' CPUID on one host with the whole baseline, compared
        // leaf by leaf: leaf 7's EBX differs by ERMS alone, leaf
        // 0x80000001's ECX by PREFETCHW alone, the brand string names the
        // model, and every other entry is the same.
        let host = host(NOTHING);
        let v3 = baseline(&host, CpuModel::V3).unwrap();
        let compat = baseline(&host, CpuModel::V3Compat).unwrap();
        assert_eq!(v3.len(), compat.len());
        for (a, b) in v3.iter().zip(&compat) {
            assert_eq!((a.function, a.index), (b.function, b.index));
            match (a.function, a.index) {
                (7, 0) => {
                    assert_eq!(a.ebx & !ERMS, b.ebx);
                    assert_eq!(a.ebx & ERMS, ERMS);
                    assert_eq!((a.eax, a.ecx, a.edx), (b.eax, b.ecx, b.edx));
                }
                (EXT1, 0) => {
                    assert_eq!(a.ecx & !PREFETCHW, b.ecx);
                    assert_eq!(a.ecx & PREFETCHW, PREFETCHW);
                    assert_eq!((a.eax, a.ebx, a.edx), (b.eax, b.ebx, b.edx));
                }
                (BRAND_FIRST..=BRAND_LAST, 0) => {}
                _ => assert_eq!(a, b, "leaf {:#x}.{}", a.function, a.index),
            }
        }
        assert_eq!(
            &v3_compat::BRAND[..38],
            b"Rewind VM x86-64-v3-compat virtual CPU"
        );
    }

    #[test]
    fn the_default_model_follows_what_the_host_lacks() {
        // Hosts: one with the whole baseline gets v3. One without ERMS, one
        // without PREFETCHW and one without both get v3-compat, each with
        // the bits that host lacks named and no others. One without ERMS
        // and AVX2 gets only the host model, with both missing bits named.
        // A run that asks for no model is refused on that last host, which
        // says to use --cpu host.
        const AVX2: u32 = 1 << 5;
        assert_eq!(classify(&host(NOTHING)).unwrap(), DefaultModel::V3);
        let without_erms = classify(&host(ERMS_ONLY)).unwrap();
        assert_eq!(
            without_erms,
            DefaultModel::V3Compat {
                lacking: "ERMS (leaf 7 EBX bit 9)".into()
            }
        );
        assert_eq!(
            classify(&host(PREFETCHW_ONLY)).unwrap(),
            DefaultModel::V3Compat {
                lacking: "PREFETCHW (leaf 0x80000001 ECX bit 8)".into()
            }
        );
        assert_eq!(
            classify(&host(Features::COMPAT_HIDDEN)).unwrap(),
            DefaultModel::V3Compat {
                lacking: "ERMS (leaf 7 EBX bit 9) and PREFETCHW (leaf 0x80000001 ECX bit 8)".into()
            }
        );
        let lacking = Features {
            leaf7_ebx: ERMS | AVX2,
            ..NOTHING
        };
        let host_only = classify(&host(lacking)).unwrap();
        assert_eq!(
            host_only,
            DefaultModel::HostOnly {
                lacking: "leaf 7 EBX bits 0x220".into()
            }
        );
        assert_eq!(DefaultModel::V3.model().unwrap(), CpuModel::V3);
        assert_eq!(without_erms.model().unwrap(), CpuModel::V3Compat);
        let refused = host_only.model().unwrap_err().to_string();
        assert_eq!(
            refused,
            "this CPU lacks the x86-64-v3 model's leaf 7 EBX bits 0x220; run with --cpu host"
        );
    }

    #[test]
    fn v3_on_a_host_without_erms_names_v3_compat() {
        // Asking for v3 by name where only ERMS is missing still refuses,
        // and points at the two models that work there; v3-compat and the
        // host model are both fine on that host, and every model is fine
        // on a host with the whole baseline.
        let without_erms = host(ERMS_ONLY);
        let refused = supports(&without_erms, CpuModel::V3, RunKind::New)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("ERMS"), "{refused}");
        assert!(refused.contains("--cpu v3-compat"), "{refused}");
        assert!(refused.contains("--cpu host"), "{refused}");
        supports(&without_erms, CpuModel::V3Compat, RunKind::New).unwrap();
        supports(&without_erms, CpuModel::Host, RunKind::New).unwrap();
        for model in [CpuModel::V3, CpuModel::V3Compat, CpuModel::Host] {
            supports(&host(NOTHING), model, RunKind::New).unwrap();
        }
    }

    #[test]
    fn v3_on_a_host_without_prefetchw_names_v3_compat() {
        // A Haswell host shows no PREFETCHW. Asking for v3 there refuses,
        // names PREFETCHW, and points at v3-compat, which records there.
        let haswell = host(PREFETCHW_ONLY);
        let refused = supports(&haswell, CpuModel::V3, RunKind::New)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "this CPU lacks PREFETCHW (leaf 0x80000001 ECX bit 8), which the x86-64-v3 model \
             shows the guest; run with --cpu v3-compat, the same model without ERMS and \
             PREFETCHW, or --cpu host"
        );
        supports(&haswell, CpuModel::V3Compat, RunKind::New).unwrap();
        supports(&haswell, CpuModel::V3Compat, RunKind::Recorded).unwrap();
    }

    #[test]
    fn v3_on_a_host_without_either_names_both() {
        // A host that shows neither ERMS nor PREFETCHW. Asking for v3
        // there refuses and names both bits, joined by "and".
        let refused = supports(&host(Features::COMPAT_HIDDEN), CpuModel::V3, RunKind::New)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "this CPU lacks ERMS (leaf 7 EBX bit 9) and PREFETCHW (leaf 0x80000001 ECX bit 8), \
             which the x86-64-v3 model shows the guest; run with --cpu v3-compat, the same \
             model without ERMS and PREFETCHW, or --cpu host"
        );
    }

    #[test]
    fn a_recorded_v3_run_without_prefetchw_says_where_it_replays() {
        // A recorded v3 run's machine built on a host that lacks only
        // PREFETCHW, and on one that lacks ERMS too: the refusal names the
        // bits that host lacks, and says to replay on a host with them.
        let refused = baseline(&host(PREFETCHW_ONLY), CpuModel::V3)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "this v3 run needs a CPU with PREFETCHW (leaf 0x80000001 ECX bit 8), which this \
             CPU lacks; replay it on a host with PREFETCHW. Recording it again here with \
             --cpu v3-compat or --cpu host makes a different run"
        );
        let refused = baseline(&host(Features::COMPAT_HIDDEN), CpuModel::V3)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "this v3 run needs a CPU with ERMS (leaf 7 EBX bit 9) and PREFETCHW (leaf \
             0x80000001 ECX bit 8), which this CPU lacks; replay it on a host with ERMS and \
             PREFETCHW. Recording it again here with --cpu v3-compat or --cpu host makes a \
             different run"
        );
    }

    #[test]
    fn a_recorded_v3_run_without_erms_says_where_it_replays() {
        // A recorded v3 run's machine built on a host that lacks only
        // ERMS: the refusal names the run's model and the missing ERMS,
        // says to replay where ERMS is, and offers recording again here
        // only as a different run, never as a --cpu to run with.
        let refused = baseline(&host(ERMS_ONLY), CpuModel::V3)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "this v3 run needs a CPU with ERMS (leaf 7 EBX bit 9), which this CPU lacks; \
             replay it on a host with ERMS. Recording it again here with --cpu v3-compat or \
             --cpu host makes a different run"
        );
    }

    #[test]
    fn a_recorded_run_missing_more_says_where_it_replays() {
        // Recorded runs of both fixed models on a host without AVX2: the
        // refusal names the run's model and the missing bits, says to
        // replay where they are, and offers only --cpu host for a new run.
        const AVX2: u32 = 1 << 5;
        let lacking = Features {
            leaf7_ebx: AVX2,
            ..NOTHING
        };
        for (model, name) in [(CpuModel::V3, "v3"), (CpuModel::V3Compat, "v3-compat")] {
            let refused = baseline(&host(lacking), model).unwrap_err().to_string();
            assert_eq!(
                refused,
                format!(
                    "this {name} run needs the {name} model's leaf 7 EBX bits 0x20, which this \
                     CPU lacks; replay it on a host that has them. Recording it again here with \
                     --cpu host makes a different run"
                )
            );
        }
    }
}
