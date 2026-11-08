//! Context switches: which task the CPU ran from each step on.
//!
//! The guest kernel writes a switch record at every context switch, so a
//! switch is an exit and a step of its own, and the record names the task
//! the CPU runs from that step: the thread of a process, one of the
//! kernel's own threads, or the idle task. The records are not events:
//! they say nothing a program did, so comparisons of what two runs did
//! leave them out, and `rewind events` does not list them. `rewind
//! threads` and the app's thread lanes are read from them.

use crate::event::{DecodeError, HEADER_LEN};

/// The record kind the guest numbers a switch, its REWIND_EV_SWITCH.
const KIND: u16 = 14;

/// The flag on a switch record for a task with no memory of its own: one of
/// the kernel's threads, or a process's thread exiting, past letting its
/// memory go. The guest's REWIND_FLAG_NO_MEMORY.
const FLAG_NO_MEMORY: u16 = 1 << 1;

/// The idle task's pid: the CPU had nothing to run.
const IDLE_PID: u32 = 0;

/// One context switch: from `step` on, the CPU ran this task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    pub step: u64,
    /// The task's thread group (process) id; 0 for the idle task.
    pub pid: u32,
    /// The task's thread id.
    pub tid: u32,
    /// Whether the task has memory of its own, and so a program.
    pub memory: bool,
    /// The kernel's name for the task, its comm.
    pub name: String,
}

impl Switch {
    /// Whether the switch is to the idle task: nothing was ready to run.
    pub fn is_idle(&self) -> bool {
        self.pid == IDLE_PID
    }
}

/// Whether a trace record is a switch record.
pub fn is_switch(record: &[u8]) -> bool {
    record.len() >= HEADER_LEN && u16::from_le_bytes([record[4], record[5]]) == KIND
}

/// Decodes a switch record as the guest wrote it: the header's pid and tid
/// name the task, its flags say whether the task has memory, and the
/// payload is the task's name.
pub fn decode(step: u64, record: &[u8]) -> Result<Switch, DecodeError> {
    if record.len() < HEADER_LEN {
        return Err(DecodeError::Short(record.len()));
    }
    let u32_at = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("four bytes"));
    let claimed = u32_at(0) as usize;
    if claimed != record.len() {
        return Err(DecodeError::Length {
            claimed,
            actual: record.len(),
        });
    }
    let flags = u16::from_le_bytes([record[6], record[7]]);
    let name = &record[HEADER_LEN..];
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    Ok(Switch {
        step,
        pid: u32_at(8),
        tid: u32_at(12),
        memory: flags & FLAG_NO_MEMORY == 0,
        name: String::from_utf8_lossy(&name[..end]).into_owned(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    // Decoding: switch records laid out as the guest lays them out come
    // back as the switches they describe, and other records are not taken
    // for one.
    use super::*;

    /// A switch record as the guest writes it, with `flags` and the task's
    /// name as the payload.
    pub(crate) fn record(flags: u16, pid: u32, tid: u32, name: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER_LEN + name.len()) as u32).to_le_bytes());
        r.extend_from_slice(&KIND.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(name);
        r
    }

    #[test]
    fn a_switch_to_a_thread_names_its_process_thread_and_program() {
        let switch = decode(40, &record(0, 80, 82, b"shuffle")).unwrap();
        assert_eq!(
            switch,
            Switch {
                step: 40,
                pid: 80,
                tid: 82,
                memory: true,
                name: "shuffle".into(),
            }
        );
        assert!(!switch.is_idle());
    }

    #[test]
    fn a_switch_to_a_kernel_thread_has_no_memory() {
        let switch = decode(41, &record(FLAG_NO_MEMORY, 11, 11, b"ksoftirqd/0")).unwrap();
        assert!(!switch.memory);
        assert_eq!(switch.name, "ksoftirqd/0");
        assert!(!switch.is_idle());
    }

    #[test]
    fn a_switch_to_pid_zero_is_to_the_idle_task() {
        let switch = decode(42, &record(FLAG_NO_MEMORY, 0, 0, b"swapper/0\0junk")).unwrap();
        assert!(switch.is_idle());
        assert_eq!(switch.name, "swapper/0");
    }

    #[test]
    fn only_records_of_the_switch_kind_are_switches() {
        assert!(is_switch(&record(0, 1, 1, b"init")));
        let mut other = record(0, 1, 1, b"init");
        other[4] = 5;
        assert!(!is_switch(&other));
        assert!(!is_switch(&[0; 4]));
    }

    #[test]
    fn a_record_shorter_than_its_header_or_its_claim_is_refused() {
        assert_eq!(decode(1, &[0; 8]), Err(DecodeError::Short(8)));
        let mut lying = record(0, 1, 1, b"init");
        lying[0] = 99;
        assert_eq!(
            decode(1, &lying),
            Err(DecodeError::Length {
                claimed: 99,
                actual: HEADER_LEN + 4,
            })
        );
    }
}
