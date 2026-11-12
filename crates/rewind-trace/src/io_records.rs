//! Read and write records: each write the guest made to a regular file,
//! with where in the file it went, and each open of a regular file, for
//! reading or for writing.
//!
//! The guest kernel appends them to a buffer in its memory without an exit
//! (see guest/linux/rewind-io.patch), and the monitor takes what the
//! buffer holds at every step. The records of one step go into the trace
//! as trace records of their own kind, [`KIND`], at that step, so a
//! replay that makes other records than its run made is caught as it is
//! for any other record.
//!
//! In the trace the records are encoded compactly, against what the
//! records before them named. Each file is given an id the first time a
//! record names its device and inode, and each path the first time one is
//! opened, stored as the bytes it shares with the path defined before it
//! and the rest. A write's position is stored as the distance from where
//! the file's last write ended, which is 0 for a file written front to
//! back, and a record's thread only when it is not the one before's. All
//! numbers are LEB128 varints. Decoding therefore reads a trace's records
//! in order from its first, and an encoder that goes on from a step of a
//! run, as a restored keyframe does, starts from the records up to it
//! ([`Encoder::resume`]).
//!
//! The guest's buffer also holds its wake records, each time a thread was
//! made runnable; the encoder hands them back to the monitor, which
//! stores them with the context switches ([`crate::switches`]).
//!
//! A record the guest's buffer had no room for is lost, and counted: the
//! trace says how many were lost at the step they would have been taken,
//! so a reader knows its view of that stretch is partial.

use std::collections::HashMap;

use crate::event::{HEADER_LEN, RECORD_MAX};
use crate::switches::{Wake, WakeHow};
use crate::{Divergence, Part};

/// The trace record kind read and write records are stored under. The
/// guest kernel reserves it and never sends it.
pub const KIND: u16 = 13;

/// The version of the encoding, in the `aux` word of each trace record of
/// [`KIND`].
pub const FORMAT: u32 = 1;

/// The length of the header of a record in the guest's buffer: the
/// record's length (u16), its kind (u8), a pad byte, the thread (u32), and
/// the file's inode (u64) and device (u32). Its data follows: the position
/// (u64) and count (u32) of a write, the path of an open.
pub const GUEST_HEADER_LEN: usize = 20;

/// The most payload one trace record of read and write records holds.
const PAYLOAD_MAX: usize = RECORD_MAX - HEADER_LEN;

/// A file, by the id the trace gave it.
pub type FileId = u32;

/// A path, by the id the trace gave it: an index into [`IoLog::paths`].
pub type PathId = u32;

/// Record kinds, as numbered in the guest's buffer and in the trace.
mod kind {
    pub(super) const WRITE: u8 = 1;
    pub(super) const READ_OPEN: u8 = 2;
    pub(super) const WRITE_OPEN: u8 = 3;
    /// The trace's alone: records the guest had no room for.
    pub(super) const LOST: u8 = 4;
    /// The guest's alone: a wake, which the trace keeps with the switches.
    pub(super) const WAKE: u8 = 5;
}

/// The bits of an encoded record's first byte: its kind, and whether its
/// thread is the one of the record before it, in which case the thread is
/// not stored.
const KIND_MASK: u8 = 0x07;
const SAME_THREAD: u8 = 0x08;

/// One read or write record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Io {
    /// Thread `tid` wrote `count` bytes at `pos` of `file`.
    Write {
        tid: u32,
        file: FileId,
        pos: u64,
        count: u32,
    },
    /// Thread `tid` opened the regular file at `path` for reading.
    ReadOpen { tid: u32, path: PathId },
    /// Thread `tid` opened the regular file at `path` for writing, and
    /// that file is `file`.
    WriteOpen {
        tid: u32,
        file: FileId,
        path: PathId,
    },
    /// `count` records the guest had no room for since the step before.
    Lost { count: u32 },
}

/// A read or write record and the step it was taken at: the guest made
/// it after the step before and before this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoRecord {
    pub step: u64,
    pub io: Io,
}

/// A run's read and write records, decoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoLog {
    pub records: Vec<IoRecord>,
    /// Every path the records name, by id.
    pub paths: Vec<String>,
}

impl IoLog {
    /// The path with id `id`.
    pub fn path(&self, id: PathId) -> &str {
        &self.paths[id as usize]
    }

    /// How many records the guest had no room for, in all.
    pub fn lost(&self) -> u64 {
        self.records
            .iter()
            .map(|r| match r.io {
                Io::Lost { count } => u64::from(count),
                _ => 0,
            })
            .sum()
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum IoError {
    #[error("read and write records at step {step} end in the middle of a record")]
    Truncated { step: u64 },
    #[error("read and write records at step {step} name {what} {id}, which none before defined")]
    Undefined {
        step: u64,
        what: &'static str,
        id: u64,
    },
    #[error("read and write records at step {step} hold a record of kind {kind}")]
    Kind { step: u64, kind: u8 },
    #[error(
        "read and write records at step {step} are in format {format}; this build reads {FORMAT}"
    )]
    Format { step: u64, format: u32 },
    #[error("read and write records at step {step} hold a {what} too large for its field")]
    OutOfRange { step: u64, what: &'static str },
}

/// Whether a trace record holds read and write records.
pub fn is_io(record: &[u8]) -> bool {
    record.len() >= HEADER_LEN && u16::from_le_bytes([record[4], record[5]]) == KIND
}

/// The read and write records among a trace file's records, as
/// [`crate::records`] reads them, decoded. A trace from before runs made
/// them has none.
pub fn decode(records: &[(u64, Vec<u8>)]) -> Result<IoLog, IoError> {
    let mut tables = Tables::default();
    let mut log = IoLog::default();
    for (step, record) in records.iter().filter(|(_, r)| is_io(r)) {
        tables.decode(*step, record, &mut log)?;
    }
    log.paths = tables.names;
    Ok(log)
}

/// The first place the read and write records among two trace files'
/// records differ, steps included, as [`crate::Trace::divergence`] finds
/// it for events: None when they are the same, which for a replay of a
/// run is the claim it makes. The index counts trace records of [`KIND`].
pub fn divergence(left: &[(u64, Vec<u8>)], right: &[(u64, Vec<u8>)]) -> Option<Divergence> {
    let left: Vec<&(u64, Vec<u8>)> = left.iter().filter(|(_, r)| is_io(r)).collect();
    let right: Vec<&(u64, Vec<u8>)> = right.iter().filter(|(_, r)| is_io(r)).collect();
    let n = left.len().min(right.len());
    let index = (0..n)
        .find(|&i| left[i] != right[i])
        .or((left.len() != right.len()).then_some(n))?;

    // The step of the record at `index`, or of the last one of a side that
    // ran out first.
    let step_of =
        |side: &[&(u64, Vec<u8>)]| side.get(index).or(side.last()).map_or(0, |(step, _)| *step);
    Some(Divergence {
        index,
        left_step: step_of(&left),
        right_step: step_of(&right),
        part: Part::IoRecords,
    })
}

/// Turns what the guest buffered between two steps into trace records.
#[derive(Clone, Debug, Default)]
pub struct Encoder {
    tables: Tables,
}

impl Encoder {
    /// An encoder for a machine that has just booted.
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// The encoder as it was once it had made the read and write records
    /// among `records`: the trace up to the step a machine is restored at.
    pub fn resume(records: &[(u64, Vec<u8>)]) -> Result<Encoder, IoError> {
        let mut tables = Tables::default();
        let mut log = IoLog::default();
        for (step, record) in records.iter().filter(|(_, r)| is_io(r)) {
            tables.decode(*step, record, &mut log)?;
        }
        Ok(Encoder { tables })
    }

    /// The trace records, each at most [`RECORD_MAX`] bytes with its
    /// header, for `buffered`, the guest's buffer as it stood at `step`,
    /// and `lost`, how many records it had no room for since the step
    /// before. None when there is nothing to record. The buffer's wake
    /// records go to `wakes`, at `step` and with no time, which the
    /// monitor gives them, for the switches' encoder. A
    /// record the buffer holds that does not read, which only a broken
    /// guest writes, ends the reading and counts as lost with the rest.
    pub fn encode(
        &mut self,
        step: u64,
        buffered: &[u8],
        lost: u32,
        wakes: &mut Vec<Wake>,
    ) -> Vec<Vec<u8>> {
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        let mut entry = Vec::new();
        let mut lost = lost;

        // Each record in the buffer, encoded against the ones before it.
        let mut rest = buffered;
        while !rest.is_empty() {
            let Some(guest) = GuestRecord::parse(rest) else {
                lost = lost.saturating_add(1);
                break;
            };
            rest = &rest[guest.len..];
            if guest.kind == kind::WAKE {
                match guest.wake(step) {
                    Some(wake) => wakes.push(wake),
                    None => lost = lost.saturating_add(1),
                }
                continue;
            }
            entry.clear();
            if !self.tables.encode(&guest, &mut entry) {
                lost = lost.saturating_add(1);
                continue;
            }
            push(&mut payloads, &entry);
        }

        // The records lost since the step before, after the ones kept.
        if lost > 0 {
            entry.clear();
            entry.push(kind::LOST);
            put(&mut entry, u64::from(lost));
            push(&mut payloads, &entry);
        }
        payloads.into_iter().map(|p| record(&p)).collect()
    }
}

/// Appends an encoded record to the last payload, or to a new one when
/// the last has no room for it.
fn push(payloads: &mut Vec<Vec<u8>>, entry: &[u8]) {
    match payloads.last_mut() {
        Some(last) if last.len() + entry.len() <= PAYLOAD_MAX => last.extend_from_slice(entry),
        _ => payloads.push(entry.to_vec()),
    }
}

/// A trace record of [`KIND`] around `payload`.
fn record(payload: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + payload.len();
    let mut r = Vec::with_capacity(len);
    r.extend_from_slice(&(len as u32).to_le_bytes());
    r.extend_from_slice(&KIND.to_le_bytes());
    // Flags, pid and tid: the records carry their own threads.
    r.extend_from_slice(&[0u8; 10]);
    r.extend_from_slice(&FORMAT.to_le_bytes());
    r.extend_from_slice(payload);
    r
}

/// One record in the guest's buffer.
struct GuestRecord<'a> {
    len: usize,
    kind: u8,
    tid: u32,
    ino: u64,
    dev: u32,
    data: &'a [u8],
}

impl<'a> GuestRecord<'a> {
    /// A wake record's wake, taken at `step`: the woken thread, then the
    /// waker and how. None when it does not read.
    fn wake(&self, step: u64) -> Option<Wake> {
        const WAKE_DATA: usize = 5;
        if self.data.len() < WAKE_DATA {
            return None;
        }
        Some(Wake {
            step,
            ns: 0,
            tid: self.tid,
            waker: u32::from_le_bytes(self.data[0..4].try_into().expect("four bytes")),
            how: WakeHow::from_guest(self.data[4])?,
        })
    }

    /// The record at the start of `buf`, or None when its header is cut
    /// off or its length is not one a record has.
    fn parse(buf: &'a [u8]) -> Option<GuestRecord<'a>> {
        if buf.len() < GUEST_HEADER_LEN {
            return None;
        }
        let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if len < GUEST_HEADER_LEN || len > buf.len() {
            return None;
        }
        Some(GuestRecord {
            len,
            kind: buf[2],
            tid: u32::from_le_bytes(buf[4..8].try_into().expect("four bytes")),
            ino: u64::from_le_bytes(buf[8..16].try_into().expect("eight bytes")),
            dev: u32::from_le_bytes(buf[16..20].try_into().expect("four bytes")),
            data: &buf[GUEST_HEADER_LEN..len],
        })
    }
}

/// What a record is encoded against: the files and paths defined so far,
/// where each file's last write ended, the last path defined and the last
/// record's thread. An encoder and a decoder that have seen the same
/// records hold the same tables.
#[derive(Clone, Debug, Default)]
struct Tables {
    files: HashMap<(u32, u64), FileId>,
    ends: Vec<u64>,
    paths: HashMap<Vec<u8>, PathId>,
    names: Vec<String>,
    last_path: Vec<u8>,
    tid: u32,
}

impl Tables {
    /// Encodes one guest record onto `out`. False for a record of a kind
    /// this build does not know, or a write without its position.
    fn encode(&mut self, guest: &GuestRecord, out: &mut Vec<u8>) -> bool {
        // The kind and, when it changed, the thread.
        let known = [kind::WRITE, kind::READ_OPEN, kind::WRITE_OPEN];
        if !known.contains(&guest.kind) {
            return false;
        }
        const WRITE_DATA: usize = 12;
        if guest.kind == kind::WRITE && guest.data.len() < WRITE_DATA {
            return false;
        }

        if guest.tid == self.tid {
            out.push(guest.kind | SAME_THREAD);
        } else {
            out.push(guest.kind);
            put(out, zigzag(i64::from(guest.tid) - i64::from(self.tid)));
            self.tid = guest.tid;
        }

        // What the kind holds.
        match guest.kind {
            kind::WRITE => {
                let file = self.put_file(guest.dev, guest.ino, out);
                let pos = u64::from_le_bytes(guest.data[0..8].try_into().expect("eight bytes"));
                let count = u32::from_le_bytes(guest.data[8..12].try_into().expect("four bytes"));
                let end = &mut self.ends[file as usize];
                put(out, zigzag(pos.wrapping_sub(*end) as i64));
                put(out, u64::from(count));
                *end = pos.wrapping_add(u64::from(count));
            }
            kind::READ_OPEN => {
                self.put_path(guest.data, out);
            }
            _ => {
                self.put_file(guest.dev, guest.ino, out);
                self.put_path(guest.data, out);
            }
        }
        true
    }

    /// Encodes a reference to a file, defining it first if it is new.
    fn put_file(&mut self, dev: u32, ino: u64, out: &mut Vec<u8>) -> FileId {
        if let Some(&id) = self.files.get(&(dev, ino)) {
            put(out, u64::from(id));
            return id;
        }
        let id = self.define_file(dev, ino);
        put(out, u64::from(id));
        put(out, u64::from(dev));
        put(out, ino);
        id
    }

    /// Encodes a reference to a path, defining it first if it is new: the
    /// bytes it shares with the last path defined, then the rest.
    fn put_path(&mut self, path: &[u8], out: &mut Vec<u8>) {
        if let Some(&id) = self.paths.get(path) {
            put(out, u64::from(id));
            return;
        }
        let shared = self
            .last_path
            .iter()
            .zip(path)
            .take_while(|(a, b)| a == b)
            .count();
        put(out, self.paths.len() as u64);
        put(out, shared as u64);
        put(out, (path.len() - shared) as u64);
        out.extend_from_slice(&path[shared..]);
        self.define_path(path.to_vec());
    }

    fn define_file(&mut self, dev: u32, ino: u64) -> FileId {
        let id = self.ends.len() as FileId;
        self.files.insert((dev, ino), id);
        self.ends.push(0);
        id
    }

    fn define_path(&mut self, path: Vec<u8>) -> PathId {
        let id = self.names.len() as PathId;
        self.names.push(String::from_utf8_lossy(&path).into_owned());
        self.paths.insert(path.clone(), id);
        self.last_path = path;
        id
    }

    /// Decodes one trace record of [`KIND`] at `step` onto `log`.
    fn decode(&mut self, step: u64, record: &[u8], log: &mut IoLog) -> Result<(), IoError> {
        let format = u32::from_le_bytes(record[16..20].try_into().expect("four bytes"));
        if format != FORMAT {
            return Err(IoError::Format { step, format });
        }
        let mut r = Reader {
            bytes: &record[HEADER_LEN..],
            step,
        };
        while !r.bytes.is_empty() {
            let io = self.decode_one(&mut r)?;
            log.records.push(IoRecord { step, io });
        }
        Ok(())
    }

    fn decode_one(&mut self, r: &mut Reader) -> Result<Io, IoError> {
        let first = r.byte()?;
        let kind = first & KIND_MASK;
        if kind == kind::LOST {
            let count = r.u32("count")?;
            return Ok(Io::Lost { count });
        }

        // The thread, when it changed.
        if first & SAME_THREAD == 0 {
            let delta = unzigzag(r.varint()?);
            self.tid = i64::from(self.tid)
                .checked_add(delta)
                .and_then(|tid| u32::try_from(tid).ok())
                .ok_or(IoError::OutOfRange {
                    step: r.step,
                    what: "thread",
                })?;
        }
        let tid = self.tid;

        // What the kind holds.
        match kind {
            kind::WRITE => {
                let file = self.get_file(r)?;
                let delta = unzigzag(r.varint()?);
                let count = r.u32("count")?;
                let end = &mut self.ends[file as usize];
                let pos = end.wrapping_add(delta as u64);
                *end = pos.wrapping_add(u64::from(count));
                Ok(Io::Write {
                    tid,
                    file,
                    pos,
                    count,
                })
            }
            kind::READ_OPEN => {
                let path = self.get_path(r)?;
                Ok(Io::ReadOpen { tid, path })
            }
            kind::WRITE_OPEN => {
                let file = self.get_file(r)?;
                let path = self.get_path(r)?;
                Ok(Io::WriteOpen { tid, file, path })
            }
            other => Err(IoError::Kind {
                step: r.step,
                kind: other,
            }),
        }
    }

    /// Decodes a reference to a file, and its definition if it is new.
    fn get_file(&mut self, r: &mut Reader) -> Result<FileId, IoError> {
        let id = r.varint()?;
        let defined = self.ends.len() as u64;
        if id < defined {
            return Ok(id as FileId);
        }
        if id > defined {
            return Err(IoError::Undefined {
                step: r.step,
                what: "file",
                id,
            });
        }
        let dev = r.u32("device")?;
        let ino = r.varint()?;
        Ok(self.define_file(dev, ino))
    }

    /// Decodes a reference to a path, and its definition if it is new.
    fn get_path(&mut self, r: &mut Reader) -> Result<PathId, IoError> {
        let id = r.varint()?;
        let defined = self.names.len() as u64;
        if id < defined {
            return Ok(id as PathId);
        }
        if id > defined {
            return Err(IoError::Undefined {
                step: r.step,
                what: "path",
                id,
            });
        }
        let shared = r.varint()? as usize;
        let len = r.varint()? as usize;
        let suffix = r.take(len)?;
        let Some(prefix) = self.last_path.get(..shared) else {
            return Err(IoError::Truncated { step: r.step });
        };
        let mut path = prefix.to_vec();
        path.extend_from_slice(suffix);
        Ok(self.define_path(path))
    }
}

/// The payload of a trace record being decoded.
struct Reader<'a> {
    bytes: &'a [u8],
    step: u64,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, IoError> {
        let (&b, rest) = self
            .bytes
            .split_first()
            .ok_or(IoError::Truncated { step: self.step })?;
        self.bytes = rest;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], IoError> {
        if n > self.bytes.len() {
            return Err(IoError::Truncated { step: self.step });
        }
        let (taken, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(taken)
    }

    fn varint(&mut self) -> Result<u64, IoError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            if shift < u64::BITS {
                value |= u64::from(b & !VARINT_MORE) << shift;
            }
            if b & VARINT_MORE == 0 {
                return Ok(value);
            }
            shift += VARINT_BITS;
        }
    }

    /// A varint that holds a u32, `what` it is naming the field when it
    /// holds more.
    fn u32(&mut self, what: &'static str) -> Result<u32, IoError> {
        let value = self.varint()?;
        u32::try_from(value).map_err(|_| IoError::OutOfRange {
            step: self.step,
            what,
        })
    }
}

/// A LEB128 varint's bytes: seven bits of the number each, low bits first,
/// with the top bit set on every byte but the last.
const VARINT_BITS: u32 = 7;
const VARINT_LOW: u64 = (1 << VARINT_BITS) - 1;
const VARINT_MORE: u8 = 1 << VARINT_BITS;

/// Appends `value` as a LEB128 varint.
fn put(out: &mut Vec<u8>, mut value: u64) {
    while value > VARINT_LOW {
        out.push((value & VARINT_LOW) as u8 | VARINT_MORE);
        value >>= VARINT_BITS;
    }
    out.push(value as u8);
}

/// A signed number as an unsigned one that is small when the number is
/// near 0 either way.
fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> (i64::BITS - 1))) as u64
}

fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

#[cfg(test)]
mod tests {
    // Encoding and decoding: guest buffers built the way the kernel lays
    // records out go through an encoder into trace records, and decode
    // back to the records they held, in order and at their step.
    use super::*;

    const DEV: u32 = 0x16;

    /// A record as the guest lays it out in its buffer.
    fn guest(kind: u8, tid: u32, ino: u64, data: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((GUEST_HEADER_LEN + data.len()) as u16).to_le_bytes());
        r.push(kind);
        r.push(0);
        r.extend_from_slice(&tid.to_le_bytes());
        r.extend_from_slice(&ino.to_le_bytes());
        r.extend_from_slice(&DEV.to_le_bytes());
        r.extend_from_slice(data);
        r
    }

    fn write(tid: u32, ino: u64, pos: u64, count: u32) -> Vec<u8> {
        let mut data = pos.to_le_bytes().to_vec();
        data.extend_from_slice(&count.to_le_bytes());
        guest(kind::WRITE, tid, ino, &data)
    }

    fn read_open(tid: u32, ino: u64, path: &str) -> Vec<u8> {
        guest(kind::READ_OPEN, tid, ino, path.as_bytes())
    }

    fn write_open(tid: u32, ino: u64, path: &str) -> Vec<u8> {
        guest(kind::WRITE_OPEN, tid, ino, path.as_bytes())
    }

    /// Encodes each buffer at its step with one encoder, as the monitor
    /// does over a run, into the trace's records.
    fn encode_all(drains: &[(u64, Vec<u8>)]) -> Vec<(u64, Vec<u8>)> {
        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        let mut wakes = Vec::new();
        for (step, buffered) in drains {
            for r in encoder.encode(*step, buffered, 0, &mut wakes) {
                out.push((*step, r));
            }
        }
        out
    }

    #[test]
    fn a_buffer_decodes_to_the_records_it_held() {
        // A compiler opens its source, opens its output and writes it in
        // two pieces; the decoded records name the same paths, the file
        // the output was opened as, and the writes' positions.
        let buffered = [
            read_open(40, 7, "/build/a.c"),
            write_open(40, 9, "/build/a.o"),
            write(40, 9, 0, 100),
            write(41, 9, 100, 28),
        ]
        .concat();
        let log = decode(&encode_all(&[(12, buffered)])).unwrap();
        assert_eq!(log.paths, vec!["/build/a.c", "/build/a.o"]);
        let ios: Vec<Io> = log.records.iter().map(|r| r.io).collect();
        assert_eq!(
            ios,
            vec![
                Io::ReadOpen { tid: 40, path: 0 },
                Io::WriteOpen {
                    tid: 40,
                    file: 0,
                    path: 1
                },
                Io::Write {
                    tid: 40,
                    file: 0,
                    pos: 0,
                    count: 100
                },
                Io::Write {
                    tid: 41,
                    file: 0,
                    pos: 100,
                    count: 28
                },
            ]
        );
        assert!(log.records.iter().all(|r| r.step == 12));
    }

    fn wake(tid: u32, waker: u32, how: u8) -> Vec<u8> {
        let mut data = waker.to_le_bytes().to_vec();
        data.push(how);
        guest(kind::WAKE, tid, 0, &data)
    }

    #[test]
    fn wake_records_go_to_the_switches_not_the_trace() {
        // A worker wakes main through a futex between two writes, and a
        // timer wakes a sleeper: the wakes come back, at the step, for the
        // switches' encoder, and the writes alone are trace records. A
        // how the guest does not define is counted as lost.
        let buffered = [
            wake(41, 42, 0),
            write(40, 9, 0, 10),
            wake(43, 0, 1),
            wake(44, 0, 9),
        ]
        .concat();
        let mut wakes = Vec::new();
        let records = Encoder::new().encode(7, &buffered, 0, &mut wakes);
        assert_eq!(
            wakes,
            vec![
                Wake {
                    step: 7,
                    ns: 0,
                    tid: 41,
                    waker: 42,
                    how: WakeHow::Task
                },
                Wake {
                    step: 7,
                    ns: 0,
                    tid: 43,
                    waker: 0,
                    how: WakeHow::Hardirq
                },
            ]
        );
        let log = decode(&records.into_iter().map(|r| (7, r)).collect::<Vec<_>>()).unwrap();
        assert_eq!(log.records.len(), 2);
        assert_eq!(log.lost(), 1);
    }

    #[test]
    fn a_path_is_stored_once() {
        // The same path opened at two steps: its bytes appear once in
        // the trace, and both records decode to it.
        let path = "/nix/store/0123456789abcdef-glibc/lib/libc.so.6";
        let records = encode_all(&[(3, read_open(1, 5, path)), (8, read_open(2, 5, path))]);
        let all: Vec<u8> = records.iter().flat_map(|(_, r)| r.clone()).collect();
        let found = all
            .windows(path.len())
            .filter(|w| *w == path.as_bytes())
            .count();
        assert_eq!(found, 1);
        let log = decode(&records).unwrap();
        assert_eq!(log.records[1].io, Io::ReadOpen { tid: 2, path: 0 });
        assert_eq!(log.records[1].step, 8);
    }

    #[test]
    fn records_the_guest_lost_are_counted() {
        // A buffer that overflowed: the records it kept, then a mark of
        // how many it lost, at the same step.
        let mut encoder = Encoder::new();
        let records: Vec<(u64, Vec<u8>)> = encoder
            .encode(0, &write(1, 2, 0, 4), 3, &mut Vec::new())
            .into_iter()
            .map(|r| (20, r))
            .collect();
        let log = decode(&records).unwrap();
        assert_eq!(log.records.last().unwrap().io, Io::Lost { count: 3 });
        assert_eq!(log.lost(), 3);
    }

    #[test]
    fn an_empty_buffer_makes_no_record() {
        // A step the guest buffered nothing before, and lost nothing,
        // adds nothing to the trace.
        assert!(Encoder::new().encode(0, &[], 0, &mut Vec::new()).is_empty());
    }

    #[test]
    fn a_resumed_encoder_encodes_as_the_first_did() {
        // An encoder resumed from the records of the first two steps, as
        // a machine restored there resumes, makes the third step's
        // records byte for byte as the encoder that ran all three did.
        let drains = [
            (
                5,
                [
                    read_open(1, 3, "/build/x.h"),
                    write_open(1, 4, "/build/x.o"),
                ]
                .concat(),
            ),
            (9, write(1, 4, 0, 64)),
            (
                14,
                [read_open(1, 3, "/build/x.h"), write(2, 4, 64, 8)].concat(),
            ),
        ];
        let whole = encode_all(&drains);
        let prefix: Vec<(u64, Vec<u8>)> = whole.iter().filter(|(s, _)| *s <= 9).cloned().collect();
        let mut resumed = Encoder::resume(&prefix).unwrap();
        let tail = resumed.encode(drains[2].0, &drains[2].1, 0, &mut Vec::new());
        let expected: Vec<Vec<u8>> = whole
            .iter()
            .filter(|(s, _)| *s == 14)
            .map(|(_, r)| r.clone())
            .collect();
        assert_eq!(tail, expected);
    }

    #[test]
    fn a_large_buffer_splits_into_records_that_fit() {
        // Ten thousand writes at one step, each to a new file so none
        // encodes small: every trace record fits the trace's limit, and
        // all the writes decode back.
        const WRITES: u64 = 10_000;
        let buffered: Vec<u8> = (0..WRITES).flat_map(|i| write(7, 1000 + i, i, 1)).collect();
        let records = encode_all(&[(30, buffered)]);
        assert!(records.len() > 1);
        assert!(records.iter().all(|(_, r)| r.len() <= RECORD_MAX));
        let log = decode(&records).unwrap();
        assert_eq!(log.records.len() as u64, WRITES);
        assert_eq!(
            log.records.last().unwrap().io,
            Io::Write {
                tid: 7,
                file: (WRITES - 1) as FileId,
                pos: WRITES - 1,
                count: 1
            }
        );
    }

    #[test]
    fn a_buffer_that_does_not_read_counts_the_rest_as_lost() {
        // A record whose length runs past the buffer's end: the records
        // before it are kept, and it is counted as lost.
        let mut buffered = write(1, 2, 0, 4);
        let mut broken = write(1, 2, 4, 4);
        broken.truncate(GUEST_HEADER_LEN + 2);
        buffered.extend_from_slice(&broken);
        let records = encode_all(&[(4, buffered)]);
        let log = decode(&records).unwrap();
        assert_eq!(log.records.len(), 2);
        assert_eq!(log.lost(), 1);
    }

    #[test]
    fn records_in_another_format_are_refused() {
        // A trace record whose aux names an encoding this build does not
        // read fails to decode with what it is, rather than decoding
        // wrong.
        let mut records = encode_all(&[(2, write(1, 2, 0, 4))]);
        records[0].1[16..20].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        assert_eq!(
            decode(&records),
            Err(IoError::Format {
                step: 2,
                format: FORMAT + 1
            })
        );
    }

    #[test]
    fn the_first_differing_record_is_found() {
        // Two traces whose records match at step 3 and part at step 6,
        // where one wrote 4 bytes and the other 5: the first difference
        // is the second record, at step 6 in both. Each is identical to
        // itself.
        let left = encode_all(&[(3, write(1, 2, 0, 4)), (6, write(1, 2, 4, 4))]);
        let right = encode_all(&[(3, write(1, 2, 0, 4)), (6, write(1, 2, 4, 5))]);
        assert_eq!(divergence(&left, &left), None);
        let d = divergence(&left, &right).unwrap();
        assert_eq!((d.index, d.left_step, d.right_step), (1, 6, 6));
        assert_eq!(d.part, Part::IoRecords);

        // One that ran out first parts at its end.
        let d = divergence(&left, &left[..1]).unwrap();
        assert_eq!((d.index, d.left_step, d.right_step), (1, 6, 3));
    }

    #[test]
    fn a_trace_without_records_has_none() {
        // A trace from before runs made records: its guest records hold
        // none, and decoding gives an empty log.
        let mut console = Vec::new();
        console.extend_from_slice(&((HEADER_LEN + 2) as u32).to_le_bytes());
        console.extend_from_slice(&1u16.to_le_bytes());
        console.extend_from_slice(&[0u8; 14]);
        console.extend_from_slice(b"ok");
        assert!(!is_io(&console));
        assert_eq!(decode(&[(1, console)]).unwrap(), IoLog::default());
    }

    /// A read open by a thread `delta` from the last, of a new path "a".
    fn read_open_entry(payload: &mut Vec<u8>, delta: i64) {
        payload.push(kind::READ_OPEN);
        put(payload, zigzag(delta));
        // Path 0, new: nothing shared with the last path, one byte more.
        put(payload, 0);
        put(payload, 0);
        put(payload, 1);
        payload.push(b'a');
    }

    #[test]
    fn a_thread_out_of_range_is_an_error() {
        // A corrupt record moves the thread past what a u32 holds; decoding
        // refuses it rather than overflowing (found by fuzz/trace).
        let mut payload = Vec::new();
        read_open_entry(&mut payload, 1);
        read_open_entry(&mut payload, i64::MAX);
        let err = decode(&[(4, record(&payload))]).unwrap_err();
        assert_eq!(
            err,
            IoError::OutOfRange {
                step: 4,
                what: "thread"
            }
        );
    }

    #[test]
    fn a_count_out_of_range_is_an_error() {
        // Lost and written byte counts are u32s; a corrupt one past that is
        // refused rather than cut to its low bits.
        let mut payload = vec![kind::LOST];
        put(&mut payload, u64::from(u32::MAX) + 1);
        let err = decode(&[(4, record(&payload))]).unwrap_err();
        assert_eq!(
            err,
            IoError::OutOfRange {
                step: 4,
                what: "count"
            }
        );
    }
}
