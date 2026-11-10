//! A keyframe's machine state besides memory, by name: registers, extended
//! state, MSRs, the local APIC, pending events, the interrupt controllers
//! and the monitor's devices. Comparing two keyframes at the same step,
//! one recorded and one replayed, says which named piece of the machine
//! went another way, where comparing their bytes would only say that some
//! did.
//!
//! A few pieces change for reasons of the host's that the guest never sees,
//! and are left out of the comparison, each with its reason in [`IGNORED`].

use anyhow::Result;
use kvm_bindings::{
    kvm_debugregs, kvm_lapic_state, kvm_mp_state, kvm_regs, kvm_segment, kvm_sregs,
    kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use serde::Serialize;

use crate::snapshot::{Keyframe, unpod};

/// One named piece of machine state, and its bytes.
type Field = (String, Vec<u8>);

/// A named piece of machine state that differs between two keyframes, as
/// each holds it, in hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StateDifference {
    pub name: String,
    pub left: String,
    pub right: String,
}

/// State the guest cannot read that differs from run to run anyway, by
/// the start of its name, and why it is left out.
pub const IGNORED: &[(&str, &str)] = &[
    (
        "MSR 0x10 (TSC)",
        "the host's time stamp counter as KVM reads it: CPUID hides the TSC, CR4.TSD \
         faults RDTSC in user space, and the kernel reads the monitor's clock",
    ),
    (
        "MSR 0x40000010 (Hyper-V VP runtime)",
        "host time the vCPU has run, which KVM keeps for Hyper-V guests: the CPU model \
         shows no Hyper-V CPUID leaves, so the guest does not read it",
    ),
    (
        "MSR 0x40000020 (Hyper-V reference counter)",
        "host time since the VM was made, which KVM keeps for Hyper-V guests: the CPU \
         model shows no Hyper-V CPUID leaves, so the guest does not read it",
    ),
    (
        "MSR 0x40000022 (Hyper-V TSC frequency)",
        "the host's TSC frequency, which KVM keeps for Hyper-V guests: the CPU model shows \
         no Hyper-V CPUID leaves, so the guest does not read it",
    ),
    (
        "local APIC 0x390 (timer current count)",
        "KVM computes it from host time when it is read; the guest never arms the APIC timer",
    ),
];

/// The performance counters' MSRs, AMD's and Intel's, by range. The VM
/// has no counters (see kvm::turn_off_counters), so the guest reads these
/// as zero or faults, and which of them KVM lists depends on the host's
/// CPU: AMD's PerfMonV2 controls only where the host has PerfMonV2. They
/// are left out of the comparison.
const COUNTER_MSRS: &[std::ops::RangeInclusive<u32>] = &[
    0xc1..=0xc8,
    0x186..=0x18d,
    0x309..=0x30b,
    0x38d..=0x390,
    0xc000_0300..=0xc000_0303,
    0xc001_0000..=0xc001_0007,
    0xc001_0200..=0xc001_020b,
];

/// Whether an MSR is one of the performance counters'.
fn counter_msr(index: u32) -> bool {
    COUNTER_MSRS.iter().any(|range| range.contains(&index))
}

/// Whether an MSR is one the comparison leaves out because the guest
/// cannot read it: a performance counter's, or one of the host's time
/// that [`IGNORED`] names.
pub(crate) fn host_only_msr(index: u32) -> bool {
    counter_msr(index) || ignored(&msr_name(index))
}

/// The x86 MSRs a keyframe holds that have names worth showing.
const MSR_NAMES: &[(u32, &str)] = &[
    (0x10, "TSC"),
    (0x1b, "APIC_BASE"),
    (0x3a, "FEATURE_CONTROL"),
    (0x8b, "microcode revision"),
    (0x174, "SYSENTER_CS"),
    (0x175, "SYSENTER_ESP"),
    (0x176, "SYSENTER_EIP"),
    (0x277, "PAT"),
    (0x6e0, "TSC_DEADLINE"),
    (0xc000_0080, "EFER"),
    (0xc000_0081, "STAR"),
    (0xc000_0082, "LSTAR"),
    (0xc000_0083, "CSTAR"),
    (0xc000_0084, "SFMASK"),
    (0xc000_0100, "FS_BASE"),
    (0xc000_0101, "GS_BASE"),
    (0xc000_0102, "KERNEL_GS_BASE"),
    (0xc000_0103, "TSC_AUX"),
    (0x4000_0010, "Hyper-V VP runtime"),
    (0x4000_0020, "Hyper-V reference counter"),
    (0x4000_0022, "Hyper-V TSC frequency"),
];

/// The local APIC's registers a keyframe holds, by offset. Each is 32
/// bits at the start of a 16-byte slot.
const APIC_NAMES: &[(usize, &str)] = &[
    (0x20, "ID"),
    (0x30, "version"),
    (0x80, "TPR"),
    (0x90, "APR"),
    (0xa0, "PPR"),
    (0xd0, "LDR"),
    (0xe0, "DFR"),
    (0xf0, "spurious vector"),
    (0x280, "error status"),
    (0x300, "ICR low"),
    (0x310, "ICR high"),
    (0x320, "LVT timer"),
    (0x330, "LVT thermal"),
    (0x340, "LVT performance counter"),
    (0x350, "LVT LINT0"),
    (0x360, "LVT LINT1"),
    (0x370, "LVT error"),
    (0x380, "timer initial count"),
    (0x390, "timer current count"),
    (0x3e0, "timer divide"),
];

/// The local APIC's eight-register banks, by the offset of the first.
const APIC_BANKS: &[(usize, &str)] = &[(0x100, "ISR"), (0x180, "TMR"), (0x200, "IRR")];
const APIC_SLOT: usize = 0x10;
const APIC_REGISTER: usize = 4;
const APIC_BANK_REGISTERS: usize = 8;

/// Where XSAVE's legacy area keeps each piece, and how long each is.
const XSAVE_X87_CONTROL: (usize, usize) = (0, 24);
const XSAVE_MXCSR: (usize, usize) = (24, 4);
const XSAVE_ST: usize = 32;
const XSAVE_ST_LEN: usize = 10;
const XSAVE_XMM: usize = 160;
const XSAVE_VECTOR: usize = 16;
const XSAVE_LEGACY_REGISTERS: usize = 8;
const XSAVE_VECTORS: usize = 16;
/// XSAVE's header, after the legacy area: XSTATE_BV, which says which
/// parts the area holds, then XCOMP_BV.
const XSAVE_XSTATE_BV: (usize, usize) = (512, 8);
const XSAVE_XCOMP_BV: (usize, usize) = (520, 8);
/// The upper halves of the AVX registers, in the standard format.
const XSAVE_YMM_HIGH: usize = 576;
/// The end of the x87, SSE and AVX state the CPU model shows the guest.
const XSAVE_END: usize = XSAVE_YMM_HIGH + XSAVE_VECTORS * XSAVE_VECTOR;

impl Keyframe {
    /// Where this keyframe's machine state besides memory differs from
    /// `other`'s, by name, in the order the machine holds it, less what
    /// [`IGNORED`] leaves out.
    pub fn state_differences(&self, other: &Keyframe) -> Result<Vec<StateDifference>> {
        let (left, right) = (self.fields()?, other.fields()?);
        let lookup = |fields: &[Field]| -> std::collections::HashMap<String, Vec<u8>> {
            fields.iter().cloned().collect()
        };
        let (ours, theirs) = (lookup(&left), lookup(&right));

        // Every name either side holds, this side's in order and then the
        // other's that this one lacks.
        let mut names: Vec<&str> = left.iter().map(|(name, _)| name.as_str()).collect();
        names.extend(
            right
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| !ours.contains_key(*name)),
        );
        let mut differences = Vec::new();
        for name in names {
            let (a, b) = (
                ours.get(name).map_or(&[][..], Vec::as_slice),
                theirs.get(name).map_or(&[][..], Vec::as_slice),
            );
            if a == b || ignored(name) {
                continue;
            }
            differences.push(StateDifference {
                name: name.to_string(),
                left: hex(a),
                right: hex(b),
            });
        }
        Ok(differences)
    }

    /// Every named piece of the machine's state besides memory.
    fn fields(&self) -> Result<Vec<Field>> {
        let c = &self.cpu;
        let mut fields = Vec::new();
        regs(&unpod::<kvm_regs>(&c.regs)?, &mut fields);
        sregs(&unpod::<kvm_sregs>(&c.sregs)?, &mut fields);
        xsave(&unpod::<kvm_xsave>(&c.xsave)?, &mut fields);
        xcrs(&unpod::<kvm_xcrs>(&c.xcrs)?, &mut fields);
        lapic(&unpod::<kvm_lapic_state>(&c.lapic)?, &mut fields);
        for (index, data) in c.msrs.iter().filter(|(i, _)| !counter_msr(*i)) {
            fields.push((msr_name(*index), data.to_le_bytes().to_vec()));
        }
        events(&unpod::<kvm_vcpu_events>(&c.events)?, &mut fields);
        let d = unpod::<kvm_debugregs>(&c.debugregs)?;
        for (i, db) in d.db.iter().enumerate() {
            fields.push((format!("DR{i}"), db.to_le_bytes().to_vec()));
        }
        fields.push(("DR6".into(), d.dr6.to_le_bytes().to_vec()));
        fields.push(("DR7".into(), d.dr7.to_le_bytes().to_vec()));
        let mp = unpod::<kvm_mp_state>(&c.mp_state)?;
        fields.push(("run state".into(), mp.mp_state.to_le_bytes().to_vec()));

        // The interrupt controllers, as KVM lays them out.
        for (chip, bytes) in ["PIC (master)", "PIC (slave)", "IO-APIC"]
            .iter()
            .zip(&self.irqchips)
        {
            fields.push((chip.to_string(), bytes.clone()));
        }

        let v = &self.devices;
        let devices: [(&str, u64); 5] = [
            ("monitor clock", v.now),
            ("monitor timer", v.deadline.unwrap_or(u64::MAX)),
            ("monitor shared page", v.shared.unwrap_or(u64::MAX)),
            ("monitor step", v.step),
            ("monitor branch count", v.branches),
        ];
        for (name, value) in devices {
            fields.push((name.to_string(), value.to_le_bytes().to_vec()));
        }
        Ok(fields)
    }
}

/// Whether state of this name is left out of the comparison.
fn ignored(name: &str) -> bool {
    IGNORED.iter().any(|(prefix, _)| name.starts_with(prefix))
}

fn push(fields: &mut Vec<Field>, name: &str, value: u64) {
    fields.push((name.to_string(), value.to_le_bytes().to_vec()));
}

fn regs(r: &kvm_regs, fields: &mut Vec<Field>) {
    let named = [
        ("rax", r.rax),
        ("rbx", r.rbx),
        ("rcx", r.rcx),
        ("rdx", r.rdx),
        ("rsi", r.rsi),
        ("rdi", r.rdi),
        ("rsp", r.rsp),
        ("rbp", r.rbp),
        ("r8", r.r8),
        ("r9", r.r9),
        ("r10", r.r10),
        ("r11", r.r11),
        ("r12", r.r12),
        ("r13", r.r13),
        ("r14", r.r14),
        ("r15", r.r15),
        ("rip", r.rip),
        ("rflags", r.rflags),
    ];
    for (name, value) in named {
        push(fields, name, value);
    }
}

fn segment(name: &str, s: &kvm_segment, fields: &mut Vec<Field>) {
    push(fields, &format!("{name} base"), s.base);
    push(fields, &format!("{name} limit"), s.limit.into());
    push(fields, &format!("{name} selector"), s.selector.into());

    // An unusable segment, such as a null selector in ds in 64-bit mode,
    // has only its DPL defined: Intel saves the rest of its access rights
    // as whatever the CPU left there, and KVM reports them as they are.
    // The guest cannot read them, and on an Intel host a replay from a
    // keyframe saw ds's type as 1 where the recording had 0.
    let attributes = match s.unusable {
        0 => vec![
            s.type_, s.present, s.dpl, s.db, s.s, s.l, s.g, s.avl, s.unusable,
        ],
        _ => vec![s.dpl, s.unusable],
    };
    fields.push((format!("{name} attributes"), attributes));
}

fn sregs(s: &kvm_sregs, fields: &mut Vec<Field>) {
    let segments = [
        ("cs", &s.cs),
        ("ds", &s.ds),
        ("es", &s.es),
        ("fs", &s.fs),
        ("gs", &s.gs),
        ("ss", &s.ss),
        ("tr", &s.tr),
        ("ldt", &s.ldt),
    ];
    for (name, seg) in segments {
        segment(name, seg, fields);
    }
    push(fields, "gdt base", s.gdt.base);
    push(fields, "gdt limit", s.gdt.limit.into());
    push(fields, "idt base", s.idt.base);
    push(fields, "idt limit", s.idt.limit.into());
    let control = [
        ("cr0", s.cr0),
        ("cr2", s.cr2),
        ("cr3", s.cr3),
        ("cr4", s.cr4),
        ("cr8", s.cr8),
        ("efer", s.efer),
        ("apic base", s.apic_base),
    ];
    for (name, value) in control {
        push(fields, name, value);
    }
    for (i, word) in s.interrupt_bitmap.iter().enumerate() {
        push(fields, &format!("pending interrupts {i}"), *word);
    }
}

fn xsave(x: &kvm_xsave, fields: &mut Vec<Field>) {
    let bytes: Vec<u8> = x.region.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut at = |name: String, (start, len): (usize, usize)| {
        fields.push((name, bytes[start..start + len].to_vec()));
    };
    at("x87 control and status".into(), XSAVE_X87_CONTROL);
    at("mxcsr".into(), XSAVE_MXCSR);
    for i in 0..XSAVE_LEGACY_REGISTERS {
        at(
            format!("st{i}"),
            (XSAVE_ST + i * XSAVE_VECTOR, XSAVE_ST_LEN),
        );
    }
    for i in 0..XSAVE_VECTORS {
        at(
            format!("xmm{i}"),
            (XSAVE_XMM + i * XSAVE_VECTOR, XSAVE_VECTOR),
        );
    }
    at("XSTATE_BV (XSAVE header)".into(), XSAVE_XSTATE_BV);
    at("XCOMP_BV (XSAVE header)".into(), XSAVE_XCOMP_BV);
    for i in 0..XSAVE_VECTORS {
        at(
            format!("ymm{i} upper half"),
            (XSAVE_YMM_HIGH + i * XSAVE_VECTOR, XSAVE_VECTOR),
        );
    }
    fields.push((
        "extended state past AVX".into(),
        bytes[XSAVE_END..].to_vec(),
    ));
}

fn xcrs(x: &kvm_xcrs, fields: &mut Vec<Field>) {
    for xcr in x.xcrs.iter().take(x.nr_xcrs as usize) {
        push(fields, &format!("XCR{}", xcr.xcr), xcr.value);
    }
}

fn lapic(l: &kvm_lapic_state, fields: &mut Vec<Field>) {
    let bytes: Vec<u8> = l.regs.iter().map(|b| *b as u8).collect();
    let register = |offset: usize| bytes[offset..offset + APIC_REGISTER].to_vec();
    for (offset, name) in APIC_NAMES {
        fields.push((
            format!("local APIC {offset:#x} ({name})"),
            register(*offset),
        ));
    }
    for (first, name) in APIC_BANKS {
        for i in 0..APIC_BANK_REGISTERS {
            let offset = first + i * APIC_SLOT;
            fields.push((
                format!("local APIC {offset:#x} ({name} {i})"),
                register(offset),
            ));
        }
    }
}

/// The vCPU's pending events. KVM leaves the vector and error code of the
/// last exception and interrupt it delivered in these fields after it
/// has, so where one is neither injected nor pending they say nothing the
/// guest will see, and differ only by what the vCPU last took: they are
/// compared as zero.
fn events(e: &kvm_vcpu_events, fields: &mut Vec<Field>) {
    let x = &e.exception;
    let exception = if x.injected != 0 || x.pending != 0 {
        (
            vec![x.injected, x.nr, x.has_error_code, x.pending],
            x.error_code,
        )
    } else {
        (vec![0; 4], 0)
    };
    fields.push(("pending exception".into(), exception.0));
    push(fields, "pending exception error code", exception.1.into());
    let i = &e.interrupt;
    let vector = if i.injected != 0 { i.nr } else { 0 };
    fields.push((
        "pending interrupt".into(),
        vec![i.injected, vector, i.soft, i.shadow],
    ));
    let n = &e.nmi;
    fields.push(("pending NMI".into(), vec![n.injected, n.pending, n.masked]));
    let s = &e.smi;
    fields.push((
        "SMM".into(),
        vec![s.smm, s.pending, s.smm_inside_nmi, s.latched_init],
    ));
    push(fields, "pending event flags", e.flags.into());
}

/// An MSR's name: its index, with what it is when that is known.
pub fn msr_name(index: u32) -> String {
    match MSR_NAMES.iter().find(|(i, _)| *i == index) {
        Some((_, name)) => format!("MSR {index:#x} ({name})"),
        None => format!("MSR {index:#x}"),
    }
}

/// Bytes as hex: a little-endian number when there are at most eight of
/// them, else the bytes in order.
fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "none".into();
    }
    if bytes.len() <= 8 {
        let mut word = [0u8; 8];
        word[..bytes.len()].copy_from_slice(bytes);
        return format!("{:#x}", u64::from_le_bytes(word));
    }
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    // Keyframes made from KVM structures without a VM, compared by name.
    use super::*;
    use crate::snapshot::tests::with_cpu;

    #[test]
    fn a_register_that_differs_is_named_with_both_values() {
        // Two keyframes whose rax differs, and whose TSC MSR differs too:
        // rax is named with both values, and the TSC is left out as host
        // time.
        let mut regs = kvm_regs::default();
        let left = with_cpu(&regs, &[(0x10, 1)]);
        regs.rax = 0xd1f3;
        let right = with_cpu(&regs, &[(0x10, 2)]);
        let differences = left.state_differences(&right).unwrap();
        assert_eq!(
            differences,
            vec![StateDifference {
                name: "rax".into(),
                left: "0x0".into(),
                right: "0xd1f3".into(),
            }]
        );
    }

    #[test]
    fn an_exception_left_from_before_is_not_a_difference() {
        // Two keyframes whose exception fields hold the vector and error
        // code of the last exception the vCPU took, #PF in one and #GP in
        // the other, with nothing injected or pending: no difference. The
        // same vectors pending do differ.
        let with_exception = |nr: u8, error_code: u32, pending: u8| {
            let mut kf = with_cpu(&kvm_regs::default(), &[]);
            let mut events = kvm_vcpu_events::default();
            events.exception.nr = nr;
            events.exception.has_error_code = 1;
            events.exception.error_code = error_code;
            events.exception.pending = pending;
            kf.cpu.events = crate::snapshot::pod(&events);
            kf
        };
        let (left, right) = (with_exception(14, 6, 0), with_exception(13, 0, 0));
        assert!(left.state_differences(&right).unwrap().is_empty());
        let (left, right) = (with_exception(14, 6, 1), with_exception(13, 0, 1));
        let names: Vec<String> = left
            .state_differences(&right)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["pending exception", "pending exception error code"]);
    }

    #[test]
    fn an_unusable_segment_s_undefined_attributes_are_not_a_difference() {
        // Two keyframes whose ds is unusable, a null selector in 64-bit
        // mode, with type 0 in one and 1 in the other, as an Intel host
        // left them in checks.state-replay: no difference, since VMX
        // leaves those bits undefined and the guest cannot read them. The
        // same types on a usable ds do differ.
        let with_ds = |type_: u8, unusable: u8| {
            let mut kf = with_cpu(&kvm_regs::default(), &[]);
            let mut sregs = kvm_sregs::default();
            sregs.ds.type_ = type_;
            sregs.ds.db = 1;
            sregs.ds.unusable = unusable;
            sregs.ds.present = 1 - unusable;
            kf.cpu.sregs = crate::snapshot::pod(&sregs);
            kf
        };
        let (left, right) = (with_ds(0, 1), with_ds(1, 1));
        assert!(left.state_differences(&right).unwrap().is_empty());
        let (left, right) = (with_ds(0, 0), with_ds(1, 0));
        let names: Vec<String> = left
            .state_differences(&right)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["ds attributes"]);
    }

    #[test]
    fn host_only_msrs_are_left_out() {
        // A keyframe from a host whose KVM lists AMD's PerfMonV2 controls
        // and one from a host whose KVM does not, with different Hyper-V
        // TSC frequencies: no difference. A difference in LSTAR is one.
        let regs = kvm_regs::default();
        let zen4 = with_cpu(&regs, &[(0xc000_0300, 0), (0x4000_0022, 3_294_861_576)]);
        let zen3 = with_cpu(&regs, &[(0x4000_0022, 2_445_412_000)]);
        assert!(zen4.state_differences(&zen3).unwrap().is_empty());
        let other = with_cpu(&regs, &[(0xc000_0082, 1)]);
        let names: Vec<String> = zen3
            .state_differences(&other)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["MSR 0xc0000082 (LSTAR)"]);
    }

    #[test]
    fn the_same_state_has_no_differences() {
        // A keyframe compared with itself differs nowhere.
        let kf = with_cpu(&kvm_regs::default(), &[(0x8b, 7)]);
        assert!(kf.state_differences(&kf).unwrap().is_empty());
    }

    #[test]
    fn msrs_and_values_are_named_for_a_reader() {
        // A known MSR carries its name; an unknown one only its index. A
        // short value reads as a number, a long one as bytes.
        assert_eq!(msr_name(0xc000_0082), "MSR 0xc0000082 (LSTAR)");
        assert_eq!(msr_name(0x1234), "MSR 0x1234");
        assert_eq!(hex(&[0x2a, 0x01]), "0x12a");
        assert_eq!(hex(&[0; 10]), "00 00 00 00 00 00 00 00 00 00");
        assert_eq!(hex(&[]), "none");
    }
}
