//! Searching a run: the build log's lines, kernel console lines among
//! them, the events the log does not show, and the paths of the files the
//! run wrote, for text typed into the search box.
//!
//! The index is built once per run, in the background, the first time the
//! search box opens on the run, and each query scans it, in the background
//! too: a match is the query anywhere in the text, ignoring case.
//! `Indexed` keeps the index with the run on screen.

use std::collections::HashMap;
use std::sync::Arc;

use rewind_trace::EventKind;

use crate::describe;
use crate::model::{LogFilter, Timeline};
use crate::request::{Request, Requests};

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

/// The matches Enter acts on: those of `typed`, the text in the field
/// when Enter is pressed. `shown` is the query the matches on screen are
/// for, and those matches. Queries run in the background, so the matches
/// on screen may be an earlier query's; `index` is then searched for
/// `typed` at once, for up to `limit` matches. None while there is no
/// index to search.
pub fn hits_for_enter(
    typed: &str,
    shown: (&str, &Hits),
    index: Option<&Index>,
    limit: usize,
) -> Option<Hits> {
    let (query, hits) = shown;
    if typed == query {
        return Some(hits.clone());
    }
    Some(index?.find(typed, limit))
}

/// The index of the run on screen, as far as it is built. The index
/// belongs to one run: showing another run drops it, and an index whose
/// build was started for a run shown before is never taken.
#[derive(Default)]
pub enum Indexed {
    /// Not asked for yet.
    #[default]
    No,
    /// Being built in the background, under this request's number.
    Building(Request),
    Ready(Arc<Index>),
}

impl Indexed {
    /// Asks for the index. Returns the number to build it under, or None
    /// when it is built or being built already.
    pub fn begin(&mut self, requests: &mut Requests) -> Option<Request> {
        if !matches!(self, Indexed::No) {
            return None;
        }
        let request = requests.issue();
        *self = Indexed::Building(request);
        Some(request)
    }

    /// Takes the index built under `request`. Returns whether it is the
    /// one waited on; any other is dropped.
    pub fn built(&mut self, request: Request, index: Index) -> bool {
        if !matches!(self, Indexed::Building(waited) if *waited == request) {
            return false;
        }
        *self = Indexed::Ready(Arc::new(index));
        true
    }

    /// Another run is on screen: the index, built or not, is not its.
    pub fn another_run(&mut self) {
        *self = Indexed::No;
    }

    /// The index, once built.
    pub fn ready(&self) -> Option<&Arc<Index>> {
        match self {
            Indexed::Ready(index) => Some(index),
            Indexed::No | Indexed::Building(_) => None,
        }
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
            ..Default::default()
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
    fn the_index_stays_with_the_run_it_was_built_for() {
        // The search box opens on a run and its index starts building;
        // opening the box again starts no second build. Another run is
        // shown before the build ends, and the box opens on it: the first
        // run's index arrives and is dropped, the second run's is taken.
        // Showing a third run drops the index that was ready.
        let mut requests = Requests::default();
        let mut indexed = Indexed::default();
        let first = indexed.begin(&mut requests).unwrap();
        assert_eq!(indexed.begin(&mut requests), None);
        assert!(indexed.ready().is_none());

        indexed.another_run();
        let second = indexed.begin(&mut requests).unwrap();
        assert!(!indexed.built(first, Index::build(&timeline())));
        assert!(indexed.ready().is_none());
        assert!(indexed.built(second, Index::build(&timeline())));
        assert_eq!(indexed.ready().unwrap().find("make", 10).total, 1);
        assert_eq!(indexed.begin(&mut requests), None);

        indexed.another_run();
        assert!(indexed.ready().is_none());
    }

    #[test]
    fn enter_acts_on_the_matches_of_the_text_in_the_field() {
        // The matches on screen are for "make" and the field now says
        // "test", whose search has not answered: Enter takes the matches
        // of "test". With the field still saying "make" it takes the ones
        // on screen. While the index is not built there are none to take
        // for a text that is not on screen.
        let index = Index::build(&timeline());
        let make = index.find("make", 10);
        let on_screen = ("make", &make);
        let taken = hits_for_enter("test", on_screen, Some(&index), 10).unwrap();
        assert_eq!(taken, index.find("test", 10));
        assert_eq!(taken.total, 4);
        assert_eq!(
            hits_for_enter("make", on_screen, Some(&index), 10),
            Some(make.clone())
        );
        assert_eq!(
            hits_for_enter("make", on_screen, None, 10),
            Some(make.clone())
        );
        assert_eq!(hits_for_enter("test", on_screen, None, 10), None);
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
