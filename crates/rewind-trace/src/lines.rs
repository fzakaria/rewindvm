//! A run's output as lines, reassembled from the writes that made them.
//!
//! A program writes a line in as many pieces as it likes, and a write may
//! end in the middle of a character. [`Lines`] keeps the bytes each
//! stream has written since its last newline, and makes a [`Line`] of
//! them when the newline arrives, when the process that wrote them exits,
//! or when the trace ends. The bytes become text once the line is whole.

use std::collections::BTreeMap;

use crate::{Event, EventKind};

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
