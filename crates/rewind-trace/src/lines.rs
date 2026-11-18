//! A run's output as lines, reassembled from the writes that made them.
//!
//! A program writes a line in as many pieces as it likes, and a write may
//! end in the middle of a character. [`Lines`] keeps the bytes each
//! stream has written since its last newline, and makes a [`Line`] of
//! them when the newline arrives, when the process that wrote them exits,
//! or when the trace ends. The bytes become text once the line is whole.

use std::collections::BTreeMap;
use std::io;

use crate::{Event, EventKind, io_records, switches};

/// The byte that ends a line.
const NEWLINE: u8 = b'\n';

/// A line of output, reassembled from the writes that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The step of the write that ended the line: the write of its
    /// newline, or for a line no newline ended, the last write to it.
    pub step: u64,
    pub pid: u32,
    pub fd: u32,
    pub text: String,
}

/// Hands `emit` the lines of a trace file's records that are complete by
/// the step `until`, as [`crate::Trace::lines_until`] counts them, each
/// as soon as the records complete it. No record is taken after the first
/// event past `until`, so the lines of a run's first steps cost reading
/// the run's first records and not its trace. A line no newline ended
/// comes when its writer exits or the records run out, which may be after
/// lines with later steps.
pub fn read(
    records: impl IntoIterator<Item = io::Result<(u64, Vec<u8>)>>,
    until: u64,
    emit: &mut impl FnMut(Line),
) -> io::Result<()> {
    let mut lines = Lines::default();
    for record in records {
        let (step, record) = record?;

        // The read and write records and the batches of switches are not
        // events, and hold no output.
        if io_records::is_io(&record) || switches::is_batch(&record) {
            continue;
        }
        if step > until {
            return Ok(());
        }
        let event = Event::decode(step, &record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        lines.event(&event, emit);
    }
    lines.end(emit);
    Ok(())
}

/// What one stream has written since its last newline.
#[derive(Debug, Default)]
struct Fragment {
    bytes: Vec<u8>,
    /// The step of the last write to the fragment.
    step: u64,
    /// The place of that write among all the writes, which orders the
    /// fragments that are left when their writers are gone.
    order: u64,
}

/// Makes lines of a trace's writes, taken in the order of the trace.
#[derive(Debug, Default)]
pub struct Lines {
    /// The unfinished line of each stream, by its process and file
    /// descriptor.
    pending: BTreeMap<(u32, u32), Fragment>,
    /// How many writes there have been.
    writes: u64,
}

impl Lines {
    /// Takes the trace's next event, and hands `emit` each line the event
    /// finishes: the lines a write's newlines end, and for a process's
    /// exit, what the process had written since its last newline.
    pub fn event(&mut self, event: &Event, emit: &mut impl FnMut(Line)) {
        match &event.kind {
            EventKind::Output { fd, bytes } => self.write(event.step, event.pid, *fd, bytes, emit),
            EventKind::Exit { thread: false, .. } => self.exit(event.pid, emit),
            _ => {}
        }
    }

    /// The trace has ended: hands `emit` what every stream had written
    /// since its last newline, in the order of the streams' last writes.
    pub fn end(self, emit: &mut impl FnMut(Line)) {
        Self::flush(self.pending.into_iter().collect(), emit);
    }

    /// A write of `bytes`: every newline among them ends a line, and the
    /// bytes after the last newline wait for the rest of their line.
    fn write(&mut self, step: u64, pid: u32, fd: u32, bytes: &[u8], emit: &mut impl FnMut(Line)) {
        self.writes += 1;
        let fragment = self.pending.entry((pid, fd)).or_default();
        fragment.step = step;
        fragment.order = self.writes;

        let mut rest = bytes;
        while let Some(newline) = rest.iter().position(|&byte| byte == NEWLINE) {
            fragment.bytes.extend_from_slice(&rest[..newline]);
            emit(Line {
                step,
                pid,
                fd,
                text: String::from_utf8_lossy(&fragment.bytes).into_owned(),
            });
            fragment.bytes.clear();
            rest = &rest[newline + 1..];
        }
        fragment.bytes.extend_from_slice(rest);
    }

    /// Process `pid` has exited, so nothing will finish its lines: what
    /// its streams had written since their last newlines are lines now.
    fn exit(&mut self, pid: u32, emit: &mut impl FnMut(Line)) {
        let streams: Vec<(u32, u32)> = self
            .pending
            .range((pid, u32::MIN)..=(pid, u32::MAX))
            .map(|(stream, _)| *stream)
            .collect();
        let fragments = streams
            .into_iter()
            .filter_map(|stream| Some((stream, self.pending.remove(&stream)?)))
            .collect();
        Self::flush(fragments, emit);
    }

    /// Hands `emit` each fragment that holds anything as a line, in the
    /// order of the fragments' last writes.
    fn flush(mut fragments: Vec<((u32, u32), Fragment)>, emit: &mut impl FnMut(Line)) {
        fragments.sort_by_key(|(_, fragment)| fragment.order);
        for ((pid, fd), fragment) in fragments {
            if fragment.bytes.is_empty() {
                continue;
            }
            emit(Line {
                step: fragment.step,
                pid,
                fd,
                text: String::from_utf8_lossy(&fragment.bytes).into_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    // Lines read from a trace file's records one at a time, as `rewind
    // log` reads them: reading stops at the first event past the step
    // asked for, and a line no newline ended is read when the records run
    // out and not before.
    use std::cell::Cell;
    use std::io;

    use super::*;
    use crate::HEADER_LEN;

    /// The kind of the record of a write to standard output or error.
    const OUTPUT_KIND: u16 = 2;
    const STDOUT_FD: u32 = 1;
    /// The process every record in these tests is written by.
    const PID: u32 = 7;

    /// The record of process [`PID`] writing `text` to standard output.
    fn output(text: &str) -> Vec<u8> {
        let len = (HEADER_LEN + text.len()) as u32;
        let mut record = Vec::new();
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&OUTPUT_KIND.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&PID.to_le_bytes());
        record.extend_from_slice(&PID.to_le_bytes());
        record.extend_from_slice(&STDOUT_FD.to_le_bytes());
        record.extend_from_slice(text.as_bytes());
        record
    }

    /// The texts of the lines `read` makes of `records` up to `until`.
    fn texts(records: impl IntoIterator<Item = (u64, Vec<u8>)>, until: u64) -> Vec<String> {
        let mut texts = Vec::new();
        read(
            records.into_iter().map(io::Result::Ok),
            until,
            &mut |line| texts.push(line.text),
        )
        .unwrap();
        texts
    }

    // Tests that reading lines up to a step takes no record after the
    // first event past the step. A thousand one-line writes are handed
    // out by an iterator that counts what is taken from it: asking for
    // the lines up to step 2 gives two lines and takes three records.
    #[test]
    fn reading_up_to_a_step_stops_at_the_first_later_event() {
        const WRITES: u64 = 1000;
        const UNTIL: u64 = 2;
        let taken = Cell::new(0u64);
        let records = (1..=WRITES).map(|step| {
            taken.set(taken.get() + 1);
            (step, output(&format!("line {step}\n")))
        });

        assert_eq!(texts(records, UNTIL), ["line 1", "line 2"]);
        assert_eq!(taken.get(), UNTIL + 1);
    }

    // Tests that a line no newline ended is read when the records run
    // out, and is not read by a step before the last write. The records
    // are one whole line and then a write without a newline.
    #[test]
    fn an_unfinished_line_is_read_once_the_records_run_out() {
        let records = || [(1, output("first\n")), (2, output("reason: out of space"))];

        assert_eq!(texts(records(), 1), ["first"]);
        assert_eq!(texts(records(), 2), ["first", "reason: out of space"]);
        assert_eq!(
            texts(records(), u64::MAX),
            ["first", "reason: out of space"]
        );
    }
}
