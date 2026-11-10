//! Who wrote a file's bytes in one run. The trace's events say which
//! processes opened the file for writing and when, following a rename
//! into place back to the file renamed. A run's read and write records
//! ([`crate::io_records`]) say which write gave a byte its value. `rewind
//! blame` and the desktop app's two-run file view both ask these
//! questions here.

use crate::io_records::{FileId, Io, IoLog};
use crate::{Event, EventKind, Process, Trace};

/// The open flag that empties a file as it is opened, as the kernel
/// numbers it.
pub const O_TRUNC: u32 = 0o1000;

/// How many writers of a file [`FileWrites::of`] goes back through, so a
/// file reopened many times without being emptied costs a bounded walk.
const MAX_WRITERS: usize = 64;

/// Programs that run a script named by their first argument, which names
/// the process better than they do.
const INTERPRETERS: &[&str] = &["sh", "bash", "perl", "ruby", "node"];

/// How many plain words after a program, such as meson's `setup`, name
/// what it was asked to do.
const LABEL_WORDS: usize = 1;

/// One process that wrote a file, as the trace has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    /// The path the bytes were written under, before any rename.
    pub path: String,
    pub process: Process,
    /// The step of the open for writing, and of the close that ended it.
    pub open: u64,
    pub close: u64,
    /// The flags the file was opened with.
    pub flags: u32,
}

/// The last process to write `path` before `before`, following a rename
/// into place back to the file that was renamed.
pub fn last_writer(trace: &Trace, path: &str, before: u64) -> Option<Writer> {
    let mut path = path.to_string();
    let processes = trace.processes();
    for (i, e) in trace.events.iter().enumerate().rev() {
        if e.step > before {
            continue;
        }
        match &e.kind {
            EventKind::Rename { from, to } if *to == path => {
                path = from.clone();
            }
            EventKind::Open {
                path: opened,
                flags,
            } if *opened == path => {
                // The guest gives a pid out again, so the writer is the
                // process that had the pid at the open.
                let process = crate::process_in(&processes, e.pid, e.step)?.as_of(e.step);
                let close = close_after(&trace.events[i + 1..], e, &path, &process);
                return Some(Writer {
                    path,
                    process,
                    open: e.step,
                    close,
                    flags: *flags,
                });
            }
            _ => {}
        }
    }
    None
}

/// The step the write that `open` started ended: the close of the file's
/// last reference, else the process's exit, and never a step before the
/// open. `rest` is the events after the open.
fn close_after(rest: &[Event], open: &Event, path: &str, process: &Process) -> u64 {
    rest.iter()
        .find(|e| matches!(&e.kind, EventKind::Close { path: p } if p == path))
        .map(|e| e.step)
        .or(process.end)
        .unwrap_or(open.step)
        .max(open.step)
}

/// The process thread `tid` belonged to when it made the write whose
/// record is at `step`, by the trace's forks; the thread itself when the
/// trace does not say.
pub fn pid_of(trace: &Trace, tid: u32, step: u64) -> u32 {
    pid_among(&trace.processes(), tid, step)
}

/// The step a write whose record is at `step` was made by: the one
/// before. The monitor takes a record at the step after its write, so a
/// write followed directly by its process's exit or exec has its record
/// at the step of that event, where the process is no longer alive or
/// already runs another program.
fn written_by(step: u64) -> u64 {
    step.saturating_sub(1)
}

/// The process thread `tid` belonged to among `processes` when it made
/// the write whose record is at `step`; the thread itself when none of
/// them has it.
fn pid_among(processes: &[Process], tid: u32, step: u64) -> u32 {
    let written = written_by(step);
    processes
        .iter()
        .filter(|p| p.alive_at(written))
        .find(|p| thread_of(p, tid))
        .map_or(tid, |p| p.pid)
}

/// Whether thread `tid` is one of `process`'s.
pub fn thread_of(process: &Process, tid: u32) -> bool {
    process.pid == tid || process.threads.iter().any(|t| t.0 == tid)
}

/// The file the writer opened: the first write-open of its path by one of
/// its threads from its open for writing to its close. The open's record
/// is taken at a step after the open's event, at the latest at the close.
pub fn file_of(log: &IoLog, writer: &Writer) -> Option<FileId> {
    log.records
        .iter()
        .filter(|r| r.step >= writer.open && r.step <= writer.close)
        .find_map(|r| match r.io {
            Io::WriteOpen { tid, file, path }
                if log.path(path) == writer.path && thread_of(&writer.process, tid) =>
            {
                Some(file)
            }
            _ => None,
        })
}

/// One write to a file, from any thread, as the records have it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalWrite {
    /// The step of the write's record, the step after the write.
    pub step: u64,
    pub tid: u32,
    pub pos: u64,
    pub count: u32,
}

impl FinalWrite {
    /// The step the write was made by: the one before its record's.
    pub fn made(&self) -> u64 {
        written_by(self.step)
    }

    /// Whether the write went over byte `offset` of its file.
    pub fn covers(&self, offset: u64) -> bool {
        (self.pos..self.pos + u64::from(self.count)).contains(&offset)
    }
}

/// The writes to `file` from step `from` to step `until`, in the order
/// they were made.
fn writes_to(log: &IoLog, file: FileId, from: u64, until: u64) -> impl Iterator<Item = FinalWrite> {
    log.records
        .iter()
        .filter(move |r| r.step >= from && r.step <= until)
        .filter_map(move |r| match r.io {
            Io::Write {
                tid,
                file: f,
                pos,
                count,
            } if f == file => Some(FinalWrite {
                step: r.step,
                tid,
                pos,
                count,
            }),
            _ => None,
        })
}

/// The last write a writer's file took at `offset` while the writer had
/// it open, from any thread: the one that gave the byte its final value.
pub fn final_write(log: &IoLog, writer: &Writer, offset: u64) -> Option<FinalWrite> {
    let file = file_of(log, writer)?;
    writes_to(log, file, writer.open, writer.close)
        .filter(|w| w.covers(offset))
        .last()
}

/// The last part of a path.
pub fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A process by its program, for a sentence: the program's file name, the
/// script's after an interpreter such as python3, and the plain word
/// after it, as in `meson setup`.
pub fn label(process: &Process) -> String {
    let argv = &process.argv;
    let Some(first) = argv.first() else {
        return format!("pid {}", process.pid);
    };
    let interpreted = |a: &str| {
        let base = base_name(a);
        base.starts_with("python") || INTERPRETERS.contains(&base)
    };
    let start = match argv.get(1) {
        Some(script) if interpreted(first) && script.contains('/') => 1,
        _ => 0,
    };
    let plain = |w: &&String| {
        w.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    let mut words = vec![base_name(&argv[start]).to_string()];
    words.extend(
        argv[start + 1..]
            .iter()
            .take_while(plain)
            .take(LABEL_WORDS)
            .cloned(),
    );
    words.join(" ")
}

/// Who gave one byte of a file its value: the step of the write, and the
/// process that made it, by its pid and its program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteWrite {
    pub step: u64,
    pub pid: u32,
    pub program: String,
}

impl ByteWrite {
    /// Who made `write`, among `processes`: the process its thread
    /// belonged to when the write was made, the step before its record's,
    /// by the program that process ran then. A process that opened a file
    /// and exec'd another program before writing it is named by the
    /// program that wrote.
    pub fn of(processes: &[Process], write: &FinalWrite) -> ByteWrite {
        let pid = pid_among(processes, write.tid, write.step);
        let written = write.made();
        let program = processes
            .iter()
            .filter(|p| p.alive_at(written))
            .find(|p| p.pid == pid)
            .map_or_else(|| format!("pid {pid}"), |p| label(&p.as_of(written)));
        ByteWrite {
            step: write.step,
            pid,
            program,
        }
    }
}

/// Every write that made a file as it was at one step of a run, oldest
/// first, each with who made it: the writes of its last writer, and of
/// the writers before it back to one that emptied the file as it opened
/// it. A byte's value at that step is the last of these to cover it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileWrites {
    writes: Vec<(FinalWrite, ByteWrite)>,
}

impl FileWrites {
    /// The writes that made `path` as it was at step `at`, from `trace`
    /// and its run's read and write records `log`. None of them when the
    /// run has no records, or no process of it wrote the file.
    pub fn of(trace: &Trace, log: &IoLog, path: &str, at: u64) -> FileWrites {
        // A run without records names no write.
        if log.records.is_empty() {
            return FileWrites::default();
        }

        // The writers back from the last one before the step, through the
        // ones that kept the bytes before them, to one that emptied the
        // file.
        let mut writers = Vec::new();
        let mut path = path.to_string();
        let mut before = at;
        while writers.len() < MAX_WRITERS {
            let Some(writer) = last_writer(trace, &path, before) else {
                break;
            };
            let emptied = writer.flags & O_TRUNC != 0;
            let open = writer.open;
            path = writer.path.clone();
            writers.push(writer);
            if emptied || open == 0 {
                break;
            }
            before = open - 1;
        }

        // Each writer's writes up to the step, oldest first, with the
        // process that made each: the writer, or another process its file
        // was shared with.
        let processes = trace.processes();
        let mut writes = Vec::new();
        for writer in writers.iter().rev() {
            let Some(file) = file_of(log, writer) else {
                continue;
            };
            for write in writes_to(log, file, writer.open, writer.close.min(at)) {
                let by = ByteWrite::of(&processes, &write);
                writes.push((write, by));
            }
        }
        writes.sort_by_key(|(write, _)| write.step);
        FileWrites { writes }
    }

    /// Who gave byte `offset` its value, when the records say: the last
    /// write to cover it.
    pub fn byte(&self, offset: u64) -> Option<&ByteWrite> {
        self.writes
            .iter()
            .rev()
            .find(|(write, _)| write.covers(offset))
            .map(|(_, by)| by)
    }

    /// Whether any write made the file, as far as the records say.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    // Small traces and records written out in each test: a build's
    // processes opening a file for writing, renaming it and closing it,
    // and the writes the records hold. Each test asks which write gave a
    // byte its value, and checks the step and the process.
    use super::*;
    use crate::io_records::{IoRecord, PathId};

    /// The paths the records below name, by id.
    const TMP: PathId = 0;
    const OUT: PathId = 1;
    const PATHS: [&str; 2] = ["/build/out.tmp", "/out/lib.so"];

    /// The file ids the records below give the files.
    const FIRST: FileId = 0;
    const SECOND: FileId = 1;

    fn event(step: u64, pid: u32, kind: EventKind) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind,
        }
    }

    /// Process `pid` forked by init at `step`, and running `argv` from
    /// the step after.
    fn started(step: u64, pid: u32, argv: &[&str]) -> [Event; 2] {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        [
            event(
                step,
                1,
                EventKind::Fork {
                    child: pid,
                    thread: false,
                },
            ),
            event(
                step + 1,
                pid,
                EventKind::Exec {
                    filename: argv[0].clone(),
                    argv,
                    old_pid: pid,
                },
            ),
        ]
    }

    fn open(step: u64, pid: u32, path: &str, flags: u32) -> Event {
        event(
            step,
            pid,
            EventKind::Open {
                path: path.into(),
                flags,
            },
        )
    }

    fn close(step: u64, pid: u32, path: &str) -> Event {
        event(step, pid, EventKind::Close { path: path.into() })
    }

    /// Process `pid` exiting at `step`.
    fn exited(step: u64, pid: u32) -> Event {
        event(
            step,
            pid,
            EventKind::Exit {
                status: 0,
                comm: String::new(),
                thread: false,
            },
        )
    }

    fn log(records: Vec<(u64, Io)>) -> IoLog {
        IoLog {
            records: records
                .into_iter()
                .map(|(step, io)| IoRecord { step, io })
                .collect(),
            paths: PATHS.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn write_open(tid: u32, file: FileId, path: PathId) -> Io {
        Io::WriteOpen { tid, file, path }
    }

    fn write(tid: u32, file: FileId, pos: u64, count: u32) -> Io {
        Io::Write {
            tid,
            file,
            pos,
            count,
        }
    }

    /// The step and pid of the write that set byte `offset`.
    fn set_by(writes: &FileWrites, offset: u64) -> Option<(u64, u32)> {
        writes.byte(offset).map(|b| (b.step, b.pid))
    }

    /// A linker that empties /out/lib.so, writes it front to back, then
    /// patches bytes 50 to 59: byte 55 took its value in the patch, byte
    /// 120 in the second write, and byte 500 in none.
    #[test]
    fn a_byte_is_set_by_the_last_write_that_covers_it() {
        let mut events = started(5, 10, &["/nix/store/x-binutils/bin/ld", "-o", "lib.so"]).to_vec();
        events.push(open(10, 10, "/out/lib.so", O_TRUNC));
        events.push(close(16, 10, "/out/lib.so"));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let log = log(vec![
            (11, write_open(10, FIRST, OUT)),
            (12, write(10, FIRST, 0, 100)),
            (13, write(10, FIRST, 100, 50)),
            (14, write(10, FIRST, 50, 10)),
        ]);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 20);
        assert_eq!(set_by(&writes, 55), Some((14, 10)));
        assert_eq!(set_by(&writes, 120), Some((13, 10)));
        assert_eq!(set_by(&writes, 500), None);
        assert_eq!(writes.byte(55).unwrap().program, "ld");
    }

    /// A second writer that opens the file without emptying it and
    /// patches bytes 50 to 59: byte 55 is the second writer's, and byte
    /// 10 still the first's, whose writes the second kept.
    #[test]
    fn a_writer_that_kept_the_file_leaves_other_bytes_to_the_one_before() {
        let mut events = started(5, 10, &["ld"]).to_vec();
        events.extend(started(
            6,
            20,
            &[
                "/nix/store/y-python3/bin/python3",
                "/nix/store/z-meson/bin/meson",
                "rewrite",
            ],
        ));
        events.push(open(10, 10, "/out/lib.so", O_TRUNC));
        events.push(close(13, 10, "/out/lib.so"));
        events.push(open(20, 20, "/out/lib.so", 0));
        events.push(close(23, 20, "/out/lib.so"));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let log = log(vec![
            (11, write_open(10, FIRST, OUT)),
            (12, write(10, FIRST, 0, 100)),
            (21, write_open(20, FIRST, OUT)),
            (22, write(20, FIRST, 50, 10)),
        ]);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 30);
        assert_eq!(set_by(&writes, 55), Some((22, 20)));
        assert_eq!(writes.byte(55).unwrap().program, "meson rewrite");
        assert_eq!(set_by(&writes, 10), Some((12, 10)));
    }

    /// The file as it was at step 12, while the first writer still had it
    /// open: only the writes made by then count, and the second writer
    /// had not opened it yet.
    #[test]
    fn only_writes_made_by_the_step_count() {
        let mut events = started(5, 10, &["ld"]).to_vec();
        events.extend(started(6, 20, &["strip"]));
        events.push(open(10, 10, "/out/lib.so", O_TRUNC));
        events.push(close(15, 10, "/out/lib.so"));
        events.push(open(20, 20, "/out/lib.so", 0));
        events.push(close(25, 20, "/out/lib.so"));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let log = log(vec![
            (11, write_open(10, FIRST, OUT)),
            (12, write(10, FIRST, 0, 100)),
            (14, write(10, FIRST, 0, 100)),
            (21, write_open(20, FIRST, OUT)),
            (22, write(20, FIRST, 0, 100)),
        ]);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 12);
        assert_eq!(set_by(&writes, 0), Some((12, 10)));
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 30);
        assert_eq!(set_by(&writes, 0), Some((22, 20)));
    }

    /// A file written under a temporary name and renamed into place: its
    /// bytes are the temporary file's writes. A write by the same thread
    /// to another file does not count.
    #[test]
    fn a_file_renamed_into_place_is_found_by_its_first_name() {
        let mut events = started(5, 10, &["install"]).to_vec();
        events.push(open(10, 10, "/build/out.tmp", O_TRUNC));
        events.push(close(14, 10, "/build/out.tmp"));
        events.push(event(
            15,
            10,
            EventKind::Rename {
                from: "/build/out.tmp".into(),
                to: "/out/lib.so".into(),
            },
        ));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let log = log(vec![
            (11, write_open(10, FIRST, TMP)),
            (12, write(10, SECOND, 0, 64)),
            (13, write(10, FIRST, 0, 64)),
        ]);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 20);
        assert_eq!(set_by(&writes, 3), Some((13, 10)));
    }

    /// A write record is taken at the step after its write. A linker
    /// whose thread 11 writes and which then exits has the record at the
    /// step of its exit, and a shell that writes and then execs another
    /// program has the record at the step of the exec. Each write is
    /// still its process's, by the program that process ran when it
    /// wrote.
    #[test]
    fn a_write_taken_at_the_writer_s_exit_is_the_writer_s() {
        const LINKER_THREAD: u32 = 11;
        let mut events = started(5, 10, &["ld"]).to_vec();
        events.push(event(
            7,
            10,
            EventKind::Fork {
                child: LINKER_THREAD,
                thread: true,
            },
        ));
        events.push(open(10, 10, "/out/lib.so", O_TRUNC));
        events.push(exited(13, 10));
        events.extend(started(20, 30, &["sh"]));
        events.push(open(22, 30, "/build/out.tmp", O_TRUNC));
        events.push(event(
            24,
            30,
            EventKind::Exec {
                filename: "cat".into(),
                argv: vec!["cat".into()],
                old_pid: 30,
            },
        ));
        events.push(exited(28, 30));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let log = log(vec![
            (11, write_open(10, FIRST, OUT)),
            (13, write(LINKER_THREAD, FIRST, 0, 100)),
            (23, write_open(30, SECOND, TMP)),
            (24, write(30, SECOND, 0, 100)),
        ]);
        assert_eq!(pid_of(&trace, LINKER_THREAD, 13), 10);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 40);
        assert_eq!(set_by(&writes, 0), Some((13, 10)));
        assert_eq!(writes.byte(0).unwrap().program, "ld");
        let writes = FileWrites::of(&trace, &log, "/build/out.tmp", 40);
        assert_eq!(set_by(&writes, 0), Some((24, 30)));
        assert_eq!(writes.byte(0).unwrap().program, "sh");
    }

    /// The guest gives a pid out again: process 10 runs `true` and exits,
    /// and a later process 10 runs `ld` and writes /out/lib.so until it
    /// exits, without a close in the trace. The writer is the process
    /// alive at the open, by its own program, and its write ends at its
    /// own exit, so the write between is found.
    #[test]
    fn a_reused_pid_names_the_writer_alive_at_the_open() {
        let mut events = started(2, 10, &["true"]).to_vec();
        events.push(exited(4, 10));
        events.extend(started(20, 10, &["ld"]));
        events.push(open(25, 10, "/out/lib.so", O_TRUNC));
        events.push(exited(30, 10));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let writer = last_writer(&trace, "/out/lib.so", 40).unwrap();
        assert_eq!(writer.process.argv, ["ld"]);
        assert_eq!((writer.open, writer.close), (25, 30));

        let log = log(vec![
            (26, write_open(10, FIRST, OUT)),
            (27, write(10, FIRST, 0, 100)),
        ]);
        let writes = FileWrites::of(&trace, &log, "/out/lib.so", 40);
        assert_eq!(set_by(&writes, 0), Some((27, 10)));
        assert_eq!(writes.byte(0).unwrap().program, "ld");
    }

    /// A run recorded without read and write records: the writer is known
    /// from the trace, but no write is, so no byte has one.
    #[test]
    fn a_run_without_records_names_no_write() {
        let mut events = started(5, 10, &["ld"]).to_vec();
        events.push(open(10, 10, "/out/lib.so", O_TRUNC));
        let trace = Trace {
            events,
            ..Default::default()
        };
        let writes = FileWrites::of(&trace, &IoLog::default(), "/out/lib.so", 20);
        assert!(writes.is_empty());
        assert_eq!(set_by(&writes, 0), None);
    }

    /// A compiler by its file name, a Python program by its script and its
    /// subcommand rather than by python3, and a process that never exec'd
    /// by its pid.
    #[test]
    fn a_process_is_named_by_its_program() {
        let process = |pid: u32, argv: &[&str]| {
            let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            Process {
                pid,
                parent: 1,
                argv: argv.clone(),
                execd: true,
                forked_argv: Vec::new(),
                execs: vec![(1, argv)],
                start: 1,
                end: None,
                status: None,
                threads: Vec::new(),
            }
        };
        let cc1 = process(4, &["/nix/store/x-gcc/libexec/cc1", "-quiet", "a.c"]);
        assert_eq!(label(&cc1), "cc1");
        let meson = process(
            56,
            &[
                "/nix/store/y-python3/bin/python3.11",
                "/nix/store/z-meson/bin/meson",
                "setup",
                "build",
            ],
        );
        assert_eq!(label(&meson), "meson setup");
        let forked = process(7, &[]);
        assert_eq!(label(&forked), "pid 7");
    }
}
