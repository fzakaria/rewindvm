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
        let old = u32::from_le_bytes(bytes[reg..reg + 4].try_into().unwrap());
        let new = (old & !0x700) | (mode << 8);
        bytes[reg..reg + 4].copy_from_slice(&new.to_le_bytes());
    }
    vcpu.set_lapic(&lapic)?;
    Ok(())
}

/// The signature at leaf 0x40000000 the guest kernel looks for.
pub const SIGNATURE: &[u8; 12] = b"RewindRewind";

const HYPERVISOR_LEAF: u32 = 0x4000_0000;

/// The CPU a guest is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    /// The host's features, less the ones that break determinism. A run
    /// made this way replays only on the same CPU model: software picks
    /// code paths by the features it sees.
    Host,
    /// A fixed x86-64-v3 CPU: the same features, cache sizes, family and
    /// address widths on every host that supports them, so a run replays
    /// across machines.
    #[default]
    V3,
}

/// The host's CPUID shaped for a deterministic guest of the given model.
pub fn cpuid(kvm: &Kvm, model: Model) -> Result<CpuId> {
    let host = host_cpuid(kvm)?;
    match model {
        Model::Host => CpuId::from_entries(&host).context("building CPUID"),
        Model::V3 => {
            let entries = baseline(&host)?;
            CpuId::from_entries(&entries).context("building CPUID")
        }
    }
}

/// The feature bits the baseline keeps, by leaf, subleaf and register.
/// x86-64-v3, plus AES, PCLMULQDQ, ERMS, FSGSBASE and INVPCID, which
/// Haswell and Zen 2 onward all have.
mod v3 {
    // OSXSAVE (bit 27) is left out: KVM sets it itself from the guest's
    // CR4.
    pub const LEAF1_ECX: u32 = bits(&[0, 1, 9, 12, 13, 19, 20, 22, 23, 25, 26, 28, 29]);
    pub const LEAF1_EDX: u32 = bits(&[
        0, 1, 2, 3, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 17, 19, 23, 24, 25, 26,
    ]);
    pub const LEAF7_EBX: u32 = bits(&[0, 3, 5, 7, 8, 9, 10]);
    pub const EXT1_ECX: u32 = bits(&[0, 5, 8]);
    pub const EXT1_EDX: u32 = bits(&[11, 20, 26, 29]);

    /// XSAVE components: x87, SSE and AVX.
    pub const XCR0: u32 = 0b111;
    /// The standard XSAVE area with those three: 512 legacy bytes, a 64
    /// byte header, and 256 bytes of AVX upper halves.
    pub const XSAVE_SIZE: u32 = 832;
    /// XSAVEOPT only: compacted formats would change the kernel's layout.
    pub const XSAVE_LEAF1_EAX: u32 = 1;

    /// The highest basic and extended leaves the baseline describes.
    pub const MAX_BASIC: u32 = 0xd;
    pub const MAX_EXTENDED: u32 = 0x8000_0008;

    /// 39 physical and 48 virtual address bits, which every v3 CPU covers.
    pub const ADDRESS_BITS: u32 = (48 << 8) | 39;

    /// Family, model and stepping, per vendor: Zen 2 and Haswell, the
    /// first v3 cores of each.
    pub const SIGNATURE_AMD: u32 = 0x0083_0f10;
    pub const SIGNATURE_INTEL: u32 = 0x0003_06c3;

    /// Cache sizes, in AMD's leaf 0x80000005 and 0x80000006 formats, which
    /// glibc reads to size its copy strategies: 32 KiB 8-way L1s, a 512
    /// KiB 8-way L2 and a 16 MiB L3, all with 64 byte lines.
    pub const L1: u32 = (32 << 24) | (8 << 16) | (1 << 8) | 64;
    pub const L2: u32 = (512 << 16) | (0x6 << 12) | (1 << 8) | 64;
    pub const L3: u32 = (32 << 18) | (0x8 << 12) | (1 << 8) | 64;

    pub const BRAND: &[u8; 48] =
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

/// The baseline model from the host's shaped CPUID. Fails when the host
/// lacks a feature the baseline promises.
fn baseline(host: &[kvm_cpuid_entry2]) -> Result<Vec<kvm_cpuid_entry2>> {
    let find = |f: u32, i: u32| {
        host.iter()
            .find(|e| e.function == f && e.index == i)
            .copied()
    };
    let require = |have: u32, want: u32, what: &str| -> Result<()> {
        let missing = want & !have;
        if missing != 0 {
            anyhow::bail!(
                "this CPU lacks the x86-64-v3 baseline's {what} bits {missing:#x}; run with --cpu host"
            );
        }
        Ok(())
    };
    let leaf0 = find(0, 0).context("no CPUID leaf 0")?;
    let amd = leaf0.ebx == u32::from_le_bytes(*b"Auth");
    let leaf1 = find(1, 0).context("no CPUID leaf 1")?;
    let leaf7 = find(7, 0).unwrap_or_default();
    let ext1 = find(0x8000_0001, 0).unwrap_or_default();
    require(leaf1.ecx, v3::LEAF1_ECX, "leaf 1 ECX")?;
    require(leaf1.edx, v3::LEAF1_EDX, "leaf 1 EDX")?;
    require(leaf7.ebx, v3::LEAF7_EBX, "leaf 7 EBX")?;
    require(ext1.ecx, v3::EXT1_ECX, "leaf 0x80000001 ECX")?;
    require(ext1.edx, v3::EXT1_EDX, "leaf 0x80000001 EDX")?;

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
    let brand = |i: usize| u32::from_le_bytes(v3::BRAND[i * 4..i * 4 + 4].try_into().unwrap());

    let mut out = vec![
        entry(0, 0, v3::MAX_BASIC, leaf0.ebx, leaf0.ecx, leaf0.edx),
        // One logical processor, APIC ID 0, 64 byte cache lines.
        entry(
            1,
            0,
            signature,
            (1 << 16) | (8 << 8),
            v3::LEAF1_ECX | (1 << 31),
            v3::LEAF1_EDX,
        ),
        entry(7, 0, 0, v3::LEAF7_EBX, 0, 0),
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
        entry(0x8000_0001, 0, 0, 0, v3::EXT1_ECX, v3::EXT1_EDX),
        entry(0x8000_0002, 0, brand(0), brand(1), brand(2), brand(3)),
        entry(0x8000_0003, 0, brand(4), brand(5), brand(6), brand(7)),
        entry(0x8000_0004, 0, brand(8), brand(9), brand(10), brand(11)),
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
    let word = |i: usize| u32::from_le_bytes(sig[i * 4..i * 4 + 4].try_into().unwrap());
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
