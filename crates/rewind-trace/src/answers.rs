//! Answers packed into an export, for a reader with no machine to replay
//! the run on, such as the browser viewer.
//!
//! `rewind export --with-answers` replays the run and keeps, at the step
//! the run failed on, at each assertion the job broke and at each
//! bookmark, where each live thread of the job was, as `rewind where`
//! says it, and the files the job had written by then. The export
//! carries them as `answers.json`, the index, and one member under
//! `answers/` for each file's bytes. Source and file contents say more of
//! a job than its trace does, so an export carries answers only when
//! asked; the member's name says they are inside.
//!
//! A file's bytes are kept up to [`FILE_BYTES`], and the export's files
//! up to [`TOTAL_BYTES`] between them. The index says which files were
//! cut and which were left out, and why.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::Trace;
use crate::located::Located;

/// The index of an export's answers, and the directory of its files'
/// bytes.
pub const ANSWERS: &str = "answers.json";
pub const ANSWERS_DIR: &str = "answers";

/// The most bytes of one file an export keeps: the first this many, as
/// many as the viewer shows of a file.
pub const FILE_BYTES: u64 = 1 << 20;

/// The most bytes of files an export keeps between them, counting each
/// distinct copy once. The browser viewer holds them in a 32-bit address
/// space beside the run's trace.
pub const TOTAL_BYTES: u64 = 32 << 20;

/// The most bytes of index a reader takes: the threads' places carry the
/// source files they are in, a few hundred kilobytes a thread at most.
pub const MAX_INDEX: u64 = 64 << 20;

/// The limits a packing keeps files within.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub file_bytes: u64,
    pub total_bytes: u64,
}

/// The limits exports are packed within.
pub const LIMITS: Limits = Limits {
    file_bytes: FILE_BYTES,
    total_bytes: TOTAL_BYTES,
};

/// What an export's `answers.json` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answers {
    pub limits: Option<Limits>,
    /// Each answered step, in step order.
    pub steps: Vec<Answered>,
}

/// What an export answers at one step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answered {
    pub step: u64,
    /// Why the step is answered: the failure, an assertion, a bookmark.
    pub why: Vec<Why>,
    /// Where each live thread of the job was.
    pub threads: Vec<ThreadAnswer>,
    /// The files the job had written by the step, with their bytes.
    pub files: Vec<FileAnswer>,
    /// The files the job had written that the export does not carry.
    pub left_out: Vec<LeftOut>,
}

/// Why a step is answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Why {
    /// The run failed at the step, as `rewind summary` names it.
    Failure,
    /// The job broke an assertion there, in its words.
    Assertion { text: String },
    /// The desktop app's bookmark there, with its note.
    Bookmark { note: String },
}

/// Where one thread was at a step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadAnswer {
    pub pid: u32,
    pub tid: u32,
    pub place: Place,
}

/// What `rewind where` said of a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    /// The thread's frames and their source, as `rewind where --json`
    /// prints them.
    Located { located: Box<Located> },
    /// Why `rewind where` found no place.
    Failed { error: String },
}

/// A file the export carries at a step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAnswer {
    pub path: String,
    /// The member holding its bytes, under [`ANSWERS_DIR`]: steps where
    /// a file did not change share one.
    pub member: String,
    /// Its length in the VM.
    pub size: u64,
    pub kept: Kept,
}

/// How much of a file an export keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    Whole,
    /// Its first [`Limits::file_bytes`] bytes.
    Start,
}

/// A file the job had written that an export does not carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftOut {
    pub path: String,
    pub why: LeftOutWhy,
}

/// Why an export does not carry a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum LeftOutWhy {
    /// The VM sent no bytes: the file was gone, was not a regular file, or
    /// was larger than the VM sends.
    NotRead,
    /// Reading the job's files at the step failed, for the reason given.
    ReadFailed { error: String },
    /// Its bytes would have taken the export's files past
    /// [`Limits::total_bytes`].
    OverTotal { size: u64 },
}

impl LeftOutWhy {
    /// Why the file is left out, in words for the viewer.
    pub fn words(&self, limits: Option<Limits>) -> String {
        match self {
            LeftOutWhy::NotRead => "the VM sent no bytes of it: it was gone, was not a regular \
                                    file, or was larger than the VM sends"
                .to_string(),
            LeftOutWhy::ReadFailed { error } => {
                format!("reading the job's files at the step failed: {error}")
            }
            LeftOutWhy::OverTotal { size } => {
                let total = limits.map_or(TOTAL_BYTES, |l| l.total_bytes);
                format!("its {size} bytes would have taken the export's files past {total} bytes")
            }
        }
    }
}

/// What a replay found at one step, before packing: the threads' places,
/// and each file the job had written with its bytes, or why the replay
/// has none of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gathered {
    pub step: u64,
    pub why: Vec<Why>,
    pub threads: Vec<ThreadAnswer>,
    pub files: Vec<(String, Result<Vec<u8>, LeftOutWhy>)>,
}

/// Answers ready for an export: the index, and each member's name and
/// bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    pub answers: Answers,
    pub members: Vec<(String, Vec<u8>)>,
}

/// Packs what replays found at each step within `limits`: each file's
/// first `file_bytes` bytes, and no more than `total_bytes` between them,
/// the steps' files taken in order. Bytes two steps share are kept once.
pub fn pack(gathered: Vec<Gathered>, limits: Limits) -> Packed {
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stored: HashMap<Vec<u8>, String> = HashMap::new();
    let mut total = 0u64;
    let mut steps = Vec::with_capacity(gathered.len());
    for found in gathered {
        let mut files = Vec::new();
        let mut left_out = Vec::new();
        for (path, bytes) in found.files {
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(why) => {
                    left_out.push(LeftOut { path, why });
                    continue;
                }
            };

            // The file's start, past the limit of one file.
            let size = bytes.len() as u64;
            let (kept, bytes) = if size > limits.file_bytes {
                (Kept::Start, bytes[..limits.file_bytes as usize].to_vec())
            } else {
                (Kept::Whole, bytes)
            };

            // Bytes kept for another step already cost nothing more.
            if let Some(member) = stored.get(&bytes) {
                files.push(FileAnswer {
                    path,
                    member: member.clone(),
                    size,
                    kept,
                });
                continue;
            }
            let cost = bytes.len() as u64;
            if total + cost > limits.total_bytes {
                left_out.push(LeftOut {
                    path,
                    why: LeftOutWhy::OverTotal { size },
                });
                continue;
            }
            total += cost;
            let member = format!("{ANSWERS_DIR}/{}", members.len());
            stored.insert(bytes.clone(), member.clone());
            members.push((member.clone(), bytes));
            files.push(FileAnswer {
                path,
                member,
                size,
                kept,
            });
        }
        steps.push(Answered {
            step: found.step,
            why: found.why,
            threads: found.threads,
            files,
            left_out,
        });
    }
    steps.sort_by_key(|s| s.step);
    Packed {
        answers: Answers {
            limits: Some(limits),
            steps,
        },
        members,
    }
}

/// What an export's answers hold at a step for a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAt<'a> {
    /// The file's bytes, whole or its start.
    Kept { bytes: &'a [u8], kept: Kept },
    /// The job had written the file, and the export left it out.
    LeftOut(&'a LeftOutWhy),
}

/// An export's answers as a reader holds them: the index, and the bytes
/// of each member it names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carried {
    pub answers: Answers,
    pub contents: HashMap<String, Vec<u8>>,
}

impl Carried {
    /// The answers in `index`, the bytes of `answers.json`, with
    /// `contents`, each member under [`ANSWERS_DIR`] by its name.
    pub fn read(index: &[u8], contents: HashMap<String, Vec<u8>>) -> serde_json::Result<Carried> {
        let answers = serde_json::from_slice(index)?;
        Ok(Carried { answers, contents })
    }

    /// The steps answered, in step order.
    pub fn steps(&self) -> Vec<u64> {
        self.answers.steps.iter().map(|s| s.step).collect()
    }

    /// What the export answers at `step`.
    pub fn at(&self, step: u64) -> Option<&Answered> {
        self.answers.steps.iter().find(|s| s.step == step)
    }

    /// What the export holds of file `path` at `step`; None when the step
    /// is not answered or the job had not written the file by then. A
    /// file whose member is missing reads as left out, unread.
    pub fn file(&self, step: u64, path: &str) -> Option<FileAt<'_>> {
        let answered = self.at(step)?;
        if let Some(file) = answered.files.iter().find(|f| f.path == path) {
            return match self.contents.get(&file.member) {
                Some(bytes) => Some(FileAt::Kept {
                    bytes,
                    kept: file.kept,
                }),
                None => Some(FileAt::LeftOut(&LeftOutWhy::NotRead)),
            };
        }
        let left = answered.left_out.iter().find(|l| l.path == path)?;
        Some(FileAt::LeftOut(&left.why))
    }

    /// Where thread `tid` was at `step`, when the export answers it.
    pub fn place(&self, step: u64, tid: u32) -> Option<&Place> {
        let answered = self.at(step)?;
        let thread = answered.threads.iter().find(|t| t.tid == tid)?;
        Some(&thread.place)
    }
}

/// One mark of the desktop app's `bookmarks.json`, as far as answering
/// reads it (crates/rewind-app/src/bookmarks.rs has the format).
#[derive(Deserialize)]
struct Bookmark {
    step: u64,
    note: String,
}

/// The format of `bookmarks.json` the steps are read from.
#[derive(Deserialize)]
struct BookmarksFile {
    version: u32,
    marks: Vec<Bookmark>,
}

/// The version of `bookmarks.json` this reads.
const BOOKMARKS_VERSION: u32 = 1;

/// The steps to answer at, each with why: the step the run failed on,
/// each assertion the job broke, and each bookmark in `bookmarks`, the
/// bytes of the run's `bookmarks.json` when it has one. Steps past `end`,
/// the run's last, are left out. Bookmarks in a format this does not read
/// are none.
pub fn steps_to_answer(
    failure: Option<u64>,
    assertions: &[crate::verdict::FailedAssertion],
    bookmarks: Option<&[u8]>,
    end: u64,
) -> BTreeMap<u64, Vec<Why>> {
    let mut steps: BTreeMap<u64, Vec<Why>> = BTreeMap::new();
    if let Some(step) = failure {
        steps.entry(step).or_default().push(Why::Failure);
    }
    for assertion in assertions {
        steps
            .entry(assertion.step)
            .or_default()
            .push(Why::Assertion {
                text: assertion.text.clone(),
            });
    }
    let marks = bookmarks
        .and_then(|bytes| serde_json::from_slice::<BookmarksFile>(bytes).ok())
        .filter(|file| file.version == BOOKMARKS_VERSION)
        .map(|file| file.marks)
        .unwrap_or_default();
    for mark in marks {
        steps
            .entry(mark.step)
            .or_default()
            .push(Why::Bookmark { note: mark.note });
    }
    steps.retain(|step, _| *step <= end);
    steps
}

/// The threads of the job to answer for at `step`: each thread of a
/// process alive then, and of one whose exit is at the step, which is
/// where a run that failed by an exit failed. Init and the kernel's own
/// threads are not the job's.
pub fn live_threads(trace: &Trace, step: u64) -> Vec<(u32, u32)> {
    let mut threads = Vec::new();
    for process in trace.processes() {
        let ends_here = process.end == Some(step);
        if !(process.alive_at(step) || ends_here) {
            continue;
        }
        let kernel = process.pid == crate::KTHREADD_PID || process.parent == crate::KTHREADD_PID;
        if process.pid == rewind_init::INIT_PID || kernel || process.argv.is_empty() {
            continue;
        }
        // The main thread while it lived, which may be less than the
        // process did: a main thread that called pthread_exit is gone.
        if process.main_end.is_none_or(|end| step <= end) {
            threads.push((process.pid, process.pid));
        }
        for &(tid, start, end) in &process.threads {
            let alive = start <= step && end.is_none_or(|end| step <= end);
            if alive {
                threads.push((process.pid, tid));
            }
        }
    }
    threads
}

#[cfg(test)]
mod tests {
    // Packing what replays found into an export's members and index, and
    // reading the index and members back, with small limits; and choosing
    // the steps and threads to answer, from traces built by hand.
    use super::*;
    use crate::{Event, EventKind};

    /// Limits for the tests: 4 bytes a file, 10 between them.
    const SMALL: Limits = Limits {
        file_bytes: 4,
        total_bytes: 10,
    };

    fn located(step: u64, tid: u32) -> Place {
        Place::Located {
            located: Box::new(Located {
                run: "0123456789abcdef".into(),
                step,
                pid: 7,
                tid,
                process: "pool".into(),
                frames: Vec::new(),
                chosen: None,
                files: BTreeMap::new(),
            }),
        }
    }

    fn found(step: u64, files: &[(&str, Result<&[u8], LeftOutWhy>)]) -> Gathered {
        Gathered {
            step,
            why: vec![Why::Failure],
            threads: vec![ThreadAnswer {
                pid: 7,
                tid: 8,
                place: located(step, 8),
            }],
            files: files
                .iter()
                .map(|(path, bytes)| (path.to_string(), bytes.clone().map(<[u8]>::to_vec)))
                .collect(),
        }
    }

    /// Why a file is left out when reading the job's files failed.
    fn failed_read() -> LeftOutWhy {
        LeftOutWhy::ReadFailed {
            error: "the fork did not answer".into(),
        }
    }

    /// The members of `packed` as a reader holds them.
    fn unpacked(packed: &Packed) -> Carried {
        let index = serde_json::to_vec(&packed.answers).unwrap();
        let contents = packed.members.iter().cloned().collect();
        Carried::read(&index, contents).unwrap()
    }

    #[test]
    fn files_are_kept_within_their_limits_and_the_rest_is_said() {
        // Step 5 has a file of 3 bytes, one of 6 that is cut to its first
        // 4, and one the VM did not send; step 9 has the first file
        // unchanged, which costs nothing more, two new ones, the second of
        // which would pass the total of 10, and one whose read failed,
        // left out with the read's error.
        let packed = pack(
            vec![
                found(
                    5,
                    &[
                        ("/b/small", Ok(b"abc")),
                        ("/b/large", Ok(b"123456")),
                        ("/b/gone", Err(LeftOutWhy::NotRead)),
                    ],
                ),
                found(
                    9,
                    &[
                        ("/b/small", Ok(b"abc")),
                        ("/b/new", Ok(b"xyz")),
                        ("/b/over", Ok(b"!")),
                        ("/b/unread", Err(failed_read())),
                    ],
                ),
            ],
            SMALL,
        );
        let names: Vec<&str> = packed.members.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["answers/0", "answers/1", "answers/2"]);
        let stored: u64 = packed.members.iter().map(|(_, b)| b.len() as u64).sum();
        assert_eq!(stored, 10);

        let first = &packed.answers.steps[0];
        assert_eq!(first.files[1].kept, Kept::Start);
        assert_eq!(first.files[1].size, 6);
        assert_eq!(
            first.left_out,
            [LeftOut {
                path: "/b/gone".into(),
                why: LeftOutWhy::NotRead
            }]
        );
        let second = &packed.answers.steps[1];
        assert_eq!(second.files[0].member, first.files[0].member);
        assert_eq!(
            second.left_out,
            [
                LeftOut {
                    path: "/b/over".into(),
                    why: LeftOutWhy::OverTotal { size: 1 }
                },
                LeftOut {
                    path: "/b/unread".into(),
                    why: failed_read()
                }
            ]
        );
        let words = second.left_out[1].why.words(packed.answers.limits);
        assert!(words.contains("the fork did not answer"), "{words}");
        assert_eq!(packed.answers.limits, Some(SMALL));
    }

    #[test]
    fn packed_answers_read_back_by_step_path_and_thread() {
        // The index written as an export writes it and the members beside
        // it: each file reads back at its step, cut where it was cut; a
        // file left out says why; a step or a file not answered is None,
        // and so is a thread not answered. A member the index names that
        // is missing reads as left out.
        let packed = pack(
            vec![found(
                5,
                &[("/b/small", Ok(b"abc")), ("/b/large", Ok(b"123456"))],
            )],
            SMALL,
        );
        let read = unpacked(&packed);
        assert_eq!(read.steps(), [5]);
        assert_eq!(
            read.file(5, "/b/small"),
            Some(FileAt::Kept {
                bytes: b"abc",
                kept: Kept::Whole
            })
        );
        assert_eq!(
            read.file(5, "/b/large"),
            Some(FileAt::Kept {
                bytes: b"1234",
                kept: Kept::Start
            })
        );
        assert_eq!(read.file(6, "/b/small"), None);
        assert_eq!(read.file(5, "/b/other"), None);
        assert_eq!(read.place(5, 8), Some(&located(5, 8)));
        assert_eq!(read.place(5, 9), None);

        let mut missing = read.clone();
        missing.contents.clear();
        assert_eq!(
            missing.file(5, "/b/small"),
            Some(FileAt::LeftOut(&LeftOutWhy::NotRead))
        );
        assert!(Carried::read(b"not json", HashMap::new()).is_err());
    }

    #[test]
    fn steps_are_the_failure_the_assertions_and_the_bookmarks() {
        // The failure at 40, an assertion at 12 and one at 40, and
        // bookmarks at 12 and 900 in a run that ends at 100: three
        // reasons at 40 and two at 12, in that order, and nothing past
        // the end. Bookmarks of another format are none.
        let assertion = |step| crate::verdict::FailedAssertion {
            step,
            pid: 7,
            tid: 7,
            text: format!("always \"x{step}\" failed"),
        };
        let marks = br#"{"version": 1, "marks": [{"step": 12, "note": "here"}, {"step": 900, "note": "late"}]}"#;
        let steps = steps_to_answer(Some(40), &[assertion(12), assertion(40)], Some(marks), 100);
        assert_eq!(steps.keys().copied().collect::<Vec<_>>(), [12, 40]);
        assert_eq!(
            steps[&12],
            [
                Why::Assertion {
                    text: "always \"x12\" failed".into()
                },
                Why::Bookmark {
                    note: "here".into()
                }
            ]
        );
        assert_eq!(steps[&40][0], Why::Failure);
        let other = br#"{"version": 2, "marks": [{"step": 3, "note": "?"}]}"#;
        assert!(steps_to_answer(None, &[], Some(other), 100).is_empty());
    }

    #[test]
    fn the_threads_answered_are_the_jobs_alive_or_ending_at_the_step() {
        // Init forks the job, 7, which starts thread 8, ended at step 6,
        // and thread 9, and forks 10, which exits at step 8. At step 8:
        // the job's main thread and thread 9, and 10, whose exit is the
        // step; not init, not thread 8, and not kthreadd's child 3.
        let event = |step, pid, tid, kind| Event {
            step,
            pid,
            tid,
            kind,
        };
        let fork =
            |step, pid, child, thread| event(step, pid, pid, EventKind::Fork { child, thread });
        let exec = |step, pid, name: &str| {
            event(
                step,
                pid,
                pid,
                EventKind::Exec {
                    filename: format!("/bin/{name}"),
                    argv: vec![name.into()],
                    old_pid: pid,
                },
            )
        };
        let exit = |step, pid, tid, thread| {
            event(
                step,
                pid,
                tid,
                EventKind::Exit {
                    status: 0,
                    comm: "x".into(),
                    thread,
                },
            )
        };
        let trace = Trace {
            events: vec![
                exec(0, 1, "init"),
                fork(1, crate::KTHREADD_PID, 3, false),
                fork(2, 1, 7, false),
                exec(3, 7, "pool"),
                fork(4, 7, 8, true),
                fork(5, 7, 9, true),
                exit(6, 7, 8, true),
                fork(7, 7, 10, false),
                exit(8, 10, 10, false),
            ],
            ..Default::default()
        };
        assert_eq!(live_threads(&trace, 8), [(7, 7), (7, 9), (10, 10)]);
    }

    #[test]
    fn a_main_thread_gone_before_its_process_is_not_answered_for() {
        // The job, 7, starts threads 8 and 9 and its main thread exits at
        // step 4, as after pthread_exit; thread 9 exits at 6 and thread 8
        // ends the process at 9. At step 5 both workers are answered for
        // and the main thread is not; at 9, the process's exit, thread 8
        // alone is. Builds the trace by hand.
        let event = |step, tid, kind| Event {
            step,
            pid: 7,
            tid,
            kind,
        };
        let thread = |step, child| {
            event(
                step,
                7,
                EventKind::Fork {
                    child,
                    thread: true,
                },
            )
        };
        let exit = |step, tid, thread| {
            event(
                step,
                tid,
                EventKind::Exit {
                    status: 0,
                    comm: "pexit".into(),
                    thread,
                },
            )
        };
        let trace = Trace {
            events: vec![
                Event {
                    step: 1,
                    pid: 1,
                    tid: 1,
                    kind: EventKind::Fork {
                        child: 7,
                        thread: false,
                    },
                },
                event(
                    2,
                    7,
                    EventKind::Exec {
                        filename: "/bin/pexit".into(),
                        argv: vec!["pexit".into()],
                        old_pid: 7,
                    },
                ),
                thread(3, 8),
                thread(3, 9),
                exit(4, 7, true),
                exit(6, 9, true),
                exit(9, 8, false),
            ],
            ..Default::default()
        };
        assert_eq!(live_threads(&trace, 5), [(7, 8), (7, 9)]);
        assert_eq!(live_threads(&trace, 9), [(7, 8)]);
    }
}
