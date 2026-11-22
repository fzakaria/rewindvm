//! A keyframe in a form another machine can load: Rewind's QEMU machine
//! (qemu/ in this repository), which emulates the same machine without
//! KVM and starts a replay from it instead of from boot.
//!
//! KVM's structures are laid out for its ioctls; this form names each
//! field in the order the QEMU machine reads it, so QEMU needs none of
//! KVM's headers, as its WebAssembly build cannot have. All numbers are
//! little endian:
//!
//! - [`MAGIC`];
//! - the monitor: step, clock, whether a timer is armed and when it is
//!   due, whether the guest named its shared page and where, the epoch;
//! - rax, rbx, rcx, rdx, rsi, rdi, rsp, rbp, r8 to r15, rip and rflags;
//! - es, cs, ss, ds, fs, gs, ldt and tr, each its base, limit,
//!   selector and attributes in QEMU's descriptor layout ([`seg_flags`]);
//! - the GDT's and IDT's base and limit;
//! - cr0, cr2, cr3, cr4, cr8, EFER and the APIC base;
//! - XCR0, then dr0 to dr3, dr6 and dr7;
//! - whether interrupts are held for one instruction after STI or a
//!   move to SS;
//! - the local APIC's 1 KiB register page, and the 4 KiB XSAVE area;
//! - the MSRs, a count and then each index and value;
//! - memory, a count and then each page that is not zero, its number
//!   from guest physical address 0 and its 4096 bytes.

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    kvm_debugregs, kvm_lapic_state, kvm_mp_state, kvm_regs, kvm_segment, kvm_sregs,
    kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};

use crate::layout::PAGE_SIZE;
use crate::snapshot::{Keyframe, Pages, final_pages, read_pages, unpod};

/// The first bytes of a portable keyframe.
pub const MAGIC: &[u8; 8] = b"rwqsta1\0";

/// KVM_MP_STATE_RUNNABLE: a vCPU that is running, as every keyframe's is.
const MP_RUNNABLE: u32 = 0;

/// The interrupt shadow bits of kvm_vcpu_events: after STI, after a move
/// to SS.
const SHADOW_STI: u8 = 1;
const SHADOW_MOV_SS: u8 = 2;

/// XCR0's index among kvm_xcrs' entries.
const XCR_XCR0: u32 = 0;

/// QEMU's descriptor attribute bits (target/i386/cpu.h DESC_*_SHIFT).
const DESC_TYPE_SHIFT: u32 = 8;
const DESC_S_SHIFT: u32 = 12;
const DESC_DPL_SHIFT: u32 = 13;
const DESC_P_SHIFT: u32 = 15;
const DESC_AVL_SHIFT: u32 = 20;
const DESC_L_SHIFT: u32 = 21;
const DESC_B_SHIFT: u32 = 22;
const DESC_G_SHIFT: u32 = 23;

/// A segment's attributes as QEMU keeps them, from KVM's fields, as
/// QEMU's own KVM code converts them (target/i386/kvm/kvm.c get_seg).
fn seg_flags(s: &kvm_segment) -> u32 {
    let present = s.present != 0 && s.unusable == 0;
    (u32::from(s.type_) << DESC_TYPE_SHIFT)
        | (u32::from(s.s) << DESC_S_SHIFT)
        | (u32::from(s.dpl) << DESC_DPL_SHIFT)
        | (u32::from(present) << DESC_P_SHIFT)
        | (u32::from(s.avl) << DESC_AVL_SHIFT)
        | (u32::from(s.l) << DESC_L_SHIFT)
        | (u32::from(s.db) << DESC_B_SHIFT)
        | (u32::from(s.g) << DESC_G_SHIFT)
}

/// Writes little-endian numbers.
struct Out(Vec<u8>);

impl Out {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn seg(&mut self, s: &kvm_segment) {
        self.u64(s.base);
        self.u32(s.limit);
        self.u32(u32::from(s.selector));
        self.u32(seg_flags(s));
        self.u32(0);
    }
}

/// The machine `chain` restores to, its full keyframe first and the one
/// to restore last, in the portable form, with its memory read from
/// `pages`. `ram_pages` is the machine's memory span in pages. Refused
/// for a vCPU in the middle of delivering an event, which a keyframe
/// taken at a step never is.
pub fn encode(chain: &[Keyframe], pages: &(dyn Pages + Sync), ram_pages: usize) -> Result<Vec<u8>> {
    let last = chain.last().context("no keyframe to encode")?;
    let c = &last.cpu;
    let regs: kvm_regs = unpod(&c.regs)?;
    let sregs: kvm_sregs = unpod(&c.sregs)?;
    let xsave: kvm_xsave = unpod(&c.xsave)?;
    let xcrs: kvm_xcrs = unpod(&c.xcrs)?;
    let lapic: kvm_lapic_state = unpod(&c.lapic)?;
    let events: kvm_vcpu_events = unpod(&c.events)?;
    let debug: kvm_debugregs = unpod(&c.debugregs)?;
    let mp: kvm_mp_state = unpod(&c.mp_state)?;

    // What a keyframe at a step never holds, and QEMU's TCG could not
    // take over halfway.
    if mp.mp_state != MP_RUNNABLE {
        bail!("the vCPU at step {} is not running", last.step);
    }
    if events.exception.injected != 0
        || events.exception.pending != 0
        || events.interrupt.injected != 0
        || events.nmi.injected != 0
        || events.nmi.pending != 0
    {
        bail!("the vCPU at step {} is delivering an event", last.step);
    }

    let mut out = Out(MAGIC.to_vec());
    let d = &last.devices;
    for v in [
        d.step,
        d.now,
        u64::from(d.deadline.is_some()),
        d.deadline.unwrap_or(0),
        u64::from(d.shared.is_some()),
        d.shared.unwrap_or(0),
        d.epoch,
    ] {
        out.u64(v);
    }

    for v in [
        regs.rax,
        regs.rbx,
        regs.rcx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.rsp,
        regs.rbp,
        regs.r8,
        regs.r9,
        regs.r10,
        regs.r11,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
        regs.rip,
        regs.rflags,
    ] {
        out.u64(v);
    }
    for s in [
        &sregs.es, &sregs.cs, &sregs.ss, &sregs.ds, &sregs.fs, &sregs.gs, &sregs.ldt, &sregs.tr,
    ] {
        out.seg(s);
    }
    for t in [&sregs.gdt, &sregs.idt] {
        out.u64(t.base);
        out.u32(u32::from(t.limit));
        out.u32(0);
    }
    for v in [
        sregs.cr0,
        sregs.cr2,
        sregs.cr3,
        sregs.cr4,
        sregs.cr8,
        sregs.efer,
        sregs.apic_base,
    ] {
        out.u64(v);
    }
    let xcr0 = xcrs.xcrs[..xcrs.nr_xcrs as usize]
        .iter()
        .find(|x| x.xcr == XCR_XCR0)
        .map_or(1, |x| x.value);
    out.u64(xcr0);
    for v in [
        debug.db[0],
        debug.db[1],
        debug.db[2],
        debug.db[3],
        debug.dr6,
        debug.dr7,
    ] {
        out.u64(v);
    }
    let shadow = events.interrupt.shadow & (SHADOW_STI | SHADOW_MOV_SS) != 0;
    out.u32(u32::from(shadow));
    out.u32(0);

    out.0.extend(lapic.regs.iter().map(|b| *b as u8));
    for word in xsave.region {
        out.u32(word);
    }

    out.u32(c.msrs.len() as u32);
    for (index, value) in &c.msrs {
        out.u32(*index);
        out.u32(0);
        out.u64(*value);
    }

    // Memory: every page the chain leaves holding something.
    let restored = final_pages(chain, ram_pages)?;
    let mut ram = vec![0u8; ram_pages * PAGE_SIZE];
    read_pages(&mut ram, &restored, pages)?;
    let nonzero: Vec<u32> = restored
        .iter()
        .map(|(page, _)| *page)
        .filter(|&p| {
            let at = p as usize * PAGE_SIZE;
            ram[at..at + PAGE_SIZE].iter().any(|b| *b != 0)
        })
        .collect();
    out.u32(nonzero.len() as u32);
    for page in nonzero {
        let at = page as usize * PAGE_SIZE;
        out.u32(page);
        out.0.extend_from_slice(&ram[at..at + PAGE_SIZE]);
    }
    Ok(out.0)
}

#[cfg(test)]
mod tests {
    // The portable form of hand-made keyframes: the fields where the QEMU
    // machine reads them, and the refusals.
    use super::*;
    use crate::snapshot::{CpuState, DeviceState, ZERO_PAGE, pod};

    /// Pages kept in memory, by a hash that is the page's first byte.
    struct Fake;
    impl Pages for Fake {
        fn put_all(&mut self, _pages: &[&[u8]]) -> Result<Vec<[u8; 32]>> {
            unreachable!()
        }
        fn get(&self, hash: &[u8; 32], out: &mut [u8]) -> Result<()> {
            out.fill(hash[0]);
            Ok(())
        }
    }

    /// A keyframe at `step` of a running vCPU with `rip`, holding a page
    /// of 7s at page 3 and a zero page at page 5.
    fn keyframe(step: u64, rip: u64) -> Keyframe {
        let regs = kvm_regs {
            rip,
            rflags: 0x202,
            ..Default::default()
        };
        let mut sregs = kvm_sregs::default();
        sregs.cs.selector = 0x10;
        sregs.cs.type_ = 0xb;
        sregs.cs.s = 1;
        sregs.cs.present = 1;
        sregs.cs.l = 1;
        let mut seven = [0u8; 32];
        seven[0] = 7;
        Keyframe {
            step,
            parent: None,
            cpu: CpuState {
                regs: pod(&regs),
                sregs: pod(&sregs),
                xsave: pod(&kvm_xsave::default()),
                xcrs: pod(&kvm_xcrs::default()),
                lapic: pod(&kvm_lapic_state::default()),
                msrs: vec![(0xc000_0082, 0xffff_ffff_8100_0080)],
                events: pod(&kvm_vcpu_events::default()),
                debugregs: pod(&kvm_debugregs::default()),
                mp_state: pod(&kvm_mp_state::default()),
            },
            irqchips: Vec::new(),
            devices: DeviceState {
                now: 5000,
                deadline: Some(9000),
                shared: Some(0x2000),
                epoch: 1,
                step,
                branches: 0,
            },
            pages: vec![(3, seven), (5, ZERO_PAGE)],
        }
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// The monitor's fields and rip come first, and memory last, without
    /// the zero page.
    #[test]
    fn fields_and_pages_land_where_qemu_reads_them() {
        const PAGES: usize = 8;
        let out = encode(&[keyframe(42, 0x1234)], &Fake, PAGES).unwrap();
        let monitor = MAGIC.len();
        assert_eq!(u64_at(&out, monitor), 42);
        assert_eq!(u64_at(&out, monitor + 8), 5000);
        assert_eq!(u64_at(&out, monitor + 24), 9000);
        let regs = monitor + 7 * 8;
        assert_eq!(u64_at(&out, regs + 16 * 8), 0x1234);
        assert_eq!(u64_at(&out, regs + 17 * 8), 0x202);
        let cs = regs + 18 * 8 + 24;
        let flags = u32::from_le_bytes(out[cs + 16..cs + 20].try_into().unwrap());
        assert_eq!(flags, (0xb << 8) | (1 << 12) | (1 << 15) | (1 << 21));

        // One page, page 3, of 7s, and nothing after it.
        let tail = out.len() - (4 + 4 + PAGE_SIZE);
        assert_eq!(&out[tail..tail + 4], &1u32.to_le_bytes());
        assert_eq!(&out[tail + 4..tail + 8], &3u32.to_le_bytes());
        assert!(out[tail + 8..].iter().all(|b| *b == 7));
    }

    /// A vCPU delivering an exception does not encode.
    #[test]
    fn a_vcpu_delivering_an_event_is_refused() {
        let mut kf = keyframe(1, 0);
        let mut events = kvm_vcpu_events::default();
        events.exception.injected = 1;
        kf.cpu.events = pod(&events);
        let refused = encode(&[kf], &Fake, 8).unwrap_err().to_string();
        assert!(refused.contains("delivering an event"), "{refused}");
    }
}
