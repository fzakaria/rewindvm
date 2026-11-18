//! A run's output as lines, reassembled from the writes that made them.
//!
//! A program writes a line in as many pieces as it likes, and a write may
//! end in the middle of a character. [`Lines`] keeps the bytes each
//! stream has written since its last newline, and makes a [`Line`] of
//! them when the newline arrives, when the process that wrote them exits,
//! or when the trace ends. The bytes become text once the line is whole.
//!
//! A line has one step, the step the line was made on, and lines are made
//! in the order of their steps. A playhead shows the lines made by its
//! step, so `rewind log` and the app, which read the lines in two ways,
//! show the same log at the same step.

use std::collections::BTreeMap;
use std::io;

use crate::{Event, EventKind, io_records, switches};

/// The byte that ends a line.
const NEWLINE: u8 = b'\n';

/// A line of output, reassembled from the writes that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The step the line was whole on: the write of its newline, or for
    /// a line no newline ended, the exit of the process that wrote the
    /// line, or the trace's last event when the trace ends first.
    pub step: u64,
    pub pid: u32,
    pub fd: u32,
    pub text: String,
}

/// Hands `emit` the lines of a trace file's records that are complete by
/// the step `until`, the lines [`crate::Trace::lines_until`] gives in the
/// same order, each as soon as the records complete it. No record is
/// taken after the first event past `until`, so the lines of a run's
/// first steps cost reading the run's first records and not its trace.
pub fn read(
    records: impl IntoIterator<Item = io::Result<(u64, Vec<u8>)>>,
    until: u64,
    emit: &mut impl FnMut(Line),
) -> io::Result<()> {
    let mut lines = Lines::default();
    let mut last_event = 0;
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
        last_event = step;
    }
    lines.end(last_event, emit);
    Ok(())
}

/// What one stream has written since its last newline.
#[derive(Debug, Default)]
struct Fragment {
    bytes: Vec<u8>,
    /// The place of the last write to the fragment among all the writes,
    /// which orders the fragments that become lines on one step.
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
            EventKind::Exit { thread: false, .. } => self.exit(event.step, event.pid, emit),
            _ => {}
        }
    }

    /// The trace has ended, its last event on `step`: hands `emit` what
    /// every stream had written since its last newline as a line of that
    /// step, in the order of the streams' last writes.
    pub fn end(self, step: u64, emit: &mut impl FnMut(Line)) {
        Self::flush(self.pending.into_iter().collect(), step, emit);
    }

    /// A write of `bytes`: every newline among them ends a line, and the
    /// bytes after the last newline wait for the rest of their line.
    fn write(&mut self, step: u64, pid: u32, fd: u32, bytes: &[u8], emit: &mut impl FnMut(Line)) {
        self.writes += 1;
        let fragment = self.pending.entry((pid, fd)).or_default();
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

    /// Process `pid` has exited on `step`, so nothing will finish its
    /// lines: what its streams had written since their last newlines are
    /// lines of that step.
    fn exit(&mut self, step: u64, pid: u32, emit: &mut impl FnMut(Line)) {
        let streams: Vec<(u32, u32)> = self
            .pending
            .range((pid, u32::MIN)..=(pid, u32::MAX))
            .map(|(stream, _)| *stream)
            .collect();
        let fragments = streams
            .into_iter()
            .filter_map(|stream| Some((stream, self.pending.remove(&stream)?)))
            .collect();
        Self::flush(fragments, step, emit);
    }

    /// Hands `emit` each fragment that holds anything as a line of
    /// `step`, in the order of the fragments' last writes.
    fn flush(mut fragments: Vec<((u32, u32), Fragment)>, step: u64, emit: &mut impl FnMut(Line)) {
        fragments.sort_by_key(|(_, fragment)| fragment.order);
        for ((pid, fd), fragment) in fragments {
            if fragment.bytes.is_empty() {
                continue;
            }
            emit(Line {
                step,
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
    use crate::{HEADER_LEN, Trace};

    /// The kinds of the records of a write to standard output or error,
    /// and of an exit.
    const OUTPUT_KIND: u16 = 2;
    const EXIT_KIND: u16 = 5;
    const STDOUT_FD: u32 = 1;
    /// The process the records are written by in the tests of one
    /// process.
    const PID: u32 = 7;

    /// A record of kind `kind` from process `pid`'s main thread.
    fn record(kind: u16, pid: u32, aux: u32, data: &[u8]) -> Vec<u8> {
        let len = (HEADER_LEN + data.len()) as u32;
        let mut record = Vec::new();
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&kind.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&pid.to_le_bytes());
        record.extend_from_slice(&pid.to_le_bytes());
        record.extend_from_slice(&aux.to_le_bytes());
        record.extend_from_slice(data);
        record
    }

    /// The record of process `pid` writing `text` to standard output.
    fn output_by(pid: u32, text: &str) -> Vec<u8> {
        record(OUTPUT_KIND, pid, STDOUT_FD, text.as_bytes())
    }

    /// The record of process [`PID`] writing `text` to standard output.
    fn output(text: &str) -> Vec<u8> {
        output_by(PID, text)
    }

    /// The record of process `pid` exiting with status 0.
    fn exit_of(pid: u32) -> Vec<u8> {
        record(EXIT_KIND, pid, 0, b"")
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

    // Tests that the lines read from a trace file's records and the lines
    // of the decoded trace are the same at every playhead, steps and
    // order included: the first is what `rewind log` prints and the
    // second what the app shows. Process 34 writes `first` with no
    // newline at step 1, process 35 writes a whole line at step 2, and
    // process 34 exits at step 3. `first` is a line from the exit on,
    // after the line of step 2, and has the exit's step.
    #[test]
    fn a_trace_and_its_records_give_the_same_lines_at_every_playhead() {
        const UNFINISHED: u32 = 34;
        const WHOLE: u32 = 35;
        let records = vec![
            (1, output_by(UNFINISHED, "first")),
            (2, output_by(WHOLE, "second\n")),
            (3, exit_of(UNFINISHED)),
        ];
        let trace = Trace::decode(&records).unwrap();
        let read_until = |until: u64| {
            let mut lines = Vec::new();
            read(
                records.iter().cloned().map(io::Result::Ok),
                until,
                &mut |line| lines.push(line),
            )
            .unwrap();
            lines
        };
        let shown = |lines: &[Line]| -> Vec<(u64, String)> {
            lines
                .iter()
                .map(|line| (line.step, line.text.clone()))
                .collect()
        };

        let expected: [(u64, &[(u64, &str)]); 4] = [
            (1, &[]),
            (2, &[(2, "second")]),
            (3, &[(2, "second"), (3, "first")]),
            (u64::MAX, &[(2, "second"), (3, "first")]),
        ];
        for (playhead, lines) in expected {
            let lines: Vec<(u64, String)> = lines
                .iter()
                .map(|(step, text)| (*step, text.to_string()))
                .collect();
            assert_eq!(shown(&read_until(playhead)), lines, "read to {playhead}");
            assert_eq!(
                shown(&trace.lines_until(playhead)),
                lines,
                "trace at {playhead}"
            );
        }
    }
}
