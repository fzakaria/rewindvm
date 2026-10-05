//! Searching a run: the build log's lines, kernel console lines among
//! them, the events the log does not show, and the paths of the files the
//! run wrote, for text typed into the search box.
//!
//! The index is built once per run, in the background, and each query
//! scans it: a match is the query anywhere in the text, ignoring case.

use std::collections::HashMap;

use rewind_trace::EventKind;

use crate::describe;
use crate::model::{LogFilter, Timeline};

/// Where a match was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// A line of the build log, or of the kernel's console.
    Log,
    /// A file the run wrote, removed or renamed, by its path, once.
    File,
    /// Any other event: a program run, a process started or ended, a
    /// signal, a file opened, removed or renamed.
    Event,
}

impl Found {
    pub fn label(self) -> &'static str {
        match self {
            Found::Log => "log",
            Found::File => "file",
            Found::Event => "event",
        }
    }
}

/// One match: the step it happened at, where it was found, and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub step: u64,
    pub found: Found,
    pub text: String,
    /// For a file, its path and the process that last wrote, removed or
    /// renamed it, for the file viewer.
    pub file: Option<(String, u32)>,
}

/// What a query matched: the first matches, up to the limit asked for,
/// in step order, and how many there are in all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hits {
    pub hits: Vec<Hit>,
    pub total: usize,
}

/// A searchable entry: its hit, and its text in lower case.
struct Entry {
    hit: Hit,
    lower: String,
}

/// A run's searchable text, in step order.
pub struct Index {
    entries: Vec<Entry>,
}

impl Index {
    /// Indexes the timeline's log lines, console lines included, and the
    /// events the log does not show.
    pub fn build(timeline: &Timeline) -> Index {
        let mut entries: Vec<Entry> = timeline
            .lines(LogFilter::WithConsole)
            .iter()
            .map(|line| entry(line.step, Found::Log, line.text.clone(), None))
            .collect();
        for event in &timeline.trace.events {
            // The log holds these already.
            let logged = matches!(
                event.kind,
                EventKind::Output { .. } | EventKind::Console { .. } | EventKind::Mark { .. }
            );
            if logged {
                continue;
            }
            let text = describe::describe(event).text;
            entries.push(entry(event.step, Found::Event, text, None));
        }

        // Each file the Files panel lists, once, at the step it was last
        // written, removed or renamed.
        let mut last: HashMap<&str, (u64, u32)> = HashMap::new();
        for file in &timeline.files {
            last.insert(file.path.as_str(), (file.step, file.pid));
        }
        for (path, (step, pid)) in last {
            let file = Some((path.to_string(), pid));
            entries.push(entry(step, Found::File, path.to_string(), file));
        }
        entries.sort_by_key(|e| (e.hit.step, e.hit.found == Found::File));
        Index { entries }
    }

    /// The entries `query` matches, ignoring case: the first `limit` of
    /// them and how many there are. An empty query matches nothing.
    pub fn find(&self, query: &str, limit: usize) -> Hits {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Hits::default();
        }
        let mut found = Hits::default();
        for e in self.entries.iter().filter(|e| e.lower.contains(&query)) {
            found.total += 1;
            if found.hits.len() < limit {
                found.hits.push(e.hit.clone());
            }
        }
        found
    }
}

fn entry(step: u64, found: Found, text: String, file: Option<(String, u32)>) -> Entry {
    Entry {
        lower: text.to_lowercase(),
        hit: Hit {
            step,
            found,
            text,
            file,
        },
    }
}

#[cfg(test)]
mod tests {
    // Queries against the index of a hand-built trace with output, a
    // console line, a program run and files.
    use super::*;
    use rewind_trace::{Event, Trace};

    fn timeline() -> Timeline {
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
                    5,
                    EventKind::Exec {
                        filename: "/bin/make".into(),
                        argv: vec!["make".into(), "check".into()],
                        old_pid: 5,
                    },
                ),
                event(
                    20,
                    5,
                    EventKind::Output {
                        fd: 1,
                        bytes: b"FAIL: test_pool_shutdown\n".to_vec(),
                    },
                ),
                event(
                    25,
                    0,
                    EventKind::Console {
                        text: "test_pool_shutd[174]: segfault at 108\n".into(),
                    },
                ),
                event(
                    30,
                    5,
                    EventKind::Open {
                        path: "/build/test-suite.log".into(),
                        flags: 0o1101,
                    },
                ),
            ],
        };
        Timeline::new(trace, None, None)
    }

    #[test]
    fn a_query_finds_log_lines_console_lines_files_and_events() {
        // "test" is in the failing line, the console's segfault, the log
        // file's path and the event that opened it; "make" is in the
        // program run. Case is ignored, and hits come in step order, the
        // file at the step it was last written.
        let index = Index::build(&timeline());
        let found = index.find("TEST", 10);
        let at: Vec<(u64, Found)> = found.hits.iter().map(|h| (h.step, h.found)).collect();
        assert_eq!(
            at,
            vec![
                (20, Found::Log),
                (25, Found::Log),
                (30, Found::Event),
                (30, Found::File),
            ]
        );
        assert_eq!(found.total, 4);
        let file = &found.hits[3];
        assert_eq!(file.text, "/build/test-suite.log");
        assert_eq!(file.file, Some(("/build/test-suite.log".to_string(), 5)));
        assert!(found.hits[2].text.starts_with("openat("));
        let make = index.find("make", 10);
        assert_eq!(make.hits.len(), 1);
        assert_eq!(make.hits[0].found, Found::Event);
        assert!(make.hits[0].text.contains("/bin/make"));
    }

    #[test]
    fn a_file_written_again_is_one_hit_at_its_last_write() {
        // A path opened for writing twice is one file, found at the
        // second write, with the process that made it.
        let mut t = timeline().trace;
        t.events.push(Event {
            step: 40,
            pid: 9,
            tid: 9,
            kind: EventKind::Open {
                path: "/build/test-suite.log".into(),
                flags: 0o1101,
            },
        });
        let index = Index::build(&Timeline::new(t, None, None));
        let found = index.find("suite", 10);
        let files: Vec<&Hit> = found
            .hits
            .iter()
            .filter(|h| h.found == Found::File)
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].step, 40);
        assert_eq!(
            files[0].file,
            Some(("/build/test-suite.log".to_string(), 9))
        );
    }

    #[test]
    fn the_limit_keeps_the_first_hits_and_counts_the_rest() {
        // Three matches with room for two: the first two by step, and a
        // total of three. Blank queries and queries that match nothing
        // find nothing.
        let index = Index::build(&timeline());
        let found = index.find("test", 2);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.hits[1].step, 25);
        assert_eq!(found.total, 4);
        assert_eq!(index.find("   ", 10), Hits::default());
        assert_eq!(index.find("nothing here", 10).total, 0);
    }
}
