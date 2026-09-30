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

/// The host's CPUID, filtered down to what a deterministic guest may see.
pub fn cpuid(kvm: &Kvm) -> Result<CpuId> {
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

    CpuId::from_entries(&entries).context("building CPUID")
}
