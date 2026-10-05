//! The engine's answers the app has had, kept to show again without
//! asking: what a file held at a step, and where a thread was.
//!
//! Each answer costs a fork of the run's VM, and a user stepping back and
//! forth asks the same questions again. A file's answer is keyed by the
//! version the trace says the file had at the step, so it serves every
//! step until the next event that changes the file; a file a running
//! process may still be writing is keyed by the step alone.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};

use rewind_trace::Trace;
use rewind_trace::contents::{Version, version};

/// At most this many answers of one kind are kept, the oldest going
/// first: a file's answer can be a mebibyte.
pub const KEPT: usize = 32;

/// Answers by key, the oldest dropped once there are `KEPT`.
pub struct Answers<K, V> {
    kept: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for Answers<K, V> {
    fn default() -> Self {
        Answers {
            kept: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> Answers<K, V> {
    pub fn get(&self, key: &K) -> Option<V> {
        self.kept.get(key).cloned()
    }

    pub fn insert(&mut self, key: K, answer: V) {
        if self.kept.insert(key.clone(), answer).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > KEPT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.kept.remove(&oldest);
        }
    }
}

/// What a file's answer is kept by: the run, the file, the process whose
/// view of paths it was read with, and the file's version at the step.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FileKey {
    run: PathBuf,
    path: String,
    pid: u32,
    version: VersionKey,
}

/// A file's version, as far as it keys an answer.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum VersionKey {
    Original,
    Since(u64),
    /// The contents at this one step, of a file a process that opened it
    /// for writing may still change with no event to say so.
    At(u64),
}

impl FileKey {
    /// The key for `path` at `step` of the run at `run`, whose trace is
    /// `trace`, read as process `pid` sees it.
    pub fn at(run: &Path, trace: &Trace, path: &str, pid: u32, step: u64) -> FileKey {
        let version = match version(trace, path, step) {
            Version::Original => VersionKey::Original,
            Version::Since(at) => VersionKey::Since(at),
            Version::Changing => VersionKey::At(step),
        };
        FileKey {
            run: run.to_path_buf(),
            path: path.to_string(),
            pid,
            version,
        }
    }
}

/// What a thread's place in its code is kept by: the run, the thread
/// asked for, and the step.
pub type PlaceKey = (PathBuf, crate::source::Thread, u64);

#[cfg(test)]
mod tests {
    // Keeping answers and keying files by their version, with hand-built
    // traces and no engine.
    use super::*;
    use rewind_trace::{Event, EventKind};

    #[test]
    fn the_oldest_answer_goes_first() {
        // KEPT answers and one more: the first is dropped, the rest are
        // kept, and an answer given again keeps its place.
        let mut answers: Answers<usize, usize> = Answers::default();
        for key in 0..=KEPT {
            answers.insert(key, key * 10);
        }
        assert_eq!(answers.get(&0), None);
        assert_eq!(answers.get(&1), Some(10));
        assert_eq!(answers.get(&KEPT), Some(KEPT * 10));
        answers.insert(1, 11);
        answers.insert(KEPT + 1, 0);
        assert_eq!(answers.get(&1), None);
        assert_eq!(answers.get(&2), Some(20));
    }

    #[test]
    fn a_file_is_keyed_by_the_version_it_had() {
        // make, process 7, opens out.o for writing at step 10 and exits at
        // 20: before 10 the file is the original, between 10 and 20 it is
        // changing and each step is a key of its own, and every step from
        // 20 on shares one key. Another process's view of it is another
        // key.
        let event = |step, pid, kind| Event {
            step,
            pid,
            tid: pid,
            kind,
        };
        let trace = Trace {
            events: vec![
                event(
                    10,
                    7,
                    EventKind::Open {
                        path: "/build/out.o".into(),
                        flags: 0o1101,
                    },
                ),
                event(
                    20,
                    7,
                    EventKind::Exit {
                        status: 0,
                        comm: "make".into(),
                        thread: false,
                    },
                ),
            ],
        };
        let run = Path::new("/runs/abc");
        let key = |step, pid| FileKey::at(run, &trace, "/build/out.o", pid, step);
        assert_ne!(key(5, 7), key(25, 7));
        assert_ne!(key(15, 7), key(16, 7));
        assert_eq!(key(25, 7), key(90, 7));
        assert_ne!(key(25, 7), key(25, 8));
    }
}
