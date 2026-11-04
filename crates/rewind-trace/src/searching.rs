//! How far a search back through a run for the accesses to a value has
//! got, as `rewind blame --at` says it on standard error and the desktop
//! app reads it: the window of steps being searched, newest first, and
//! what the search has seen so far.

use serde::{Deserialize, Serialize};

/// What a search is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Bringing a machine to the start of the window: a keyframe
    /// restored, and replay from it. Nothing is searched meanwhile.
    Restoring,
    /// Replaying the window with the value watched.
    Searching,
}

/// A search back for the accesses to a value, as far as it has got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Searching {
    pub phase: Phase,
    /// The step the search is before: where it was asked about.
    pub before: u64,
    /// The window being searched, from its first step to the step it ends
    /// before.
    pub window: (u64, u64),
    /// The step the replay of the window has reached.
    pub reached: u64,
    /// The steps searched so far, in this window and the later ones.
    pub steps: u64,
    /// The windows searched to their end so far.
    pub windows: u32,
    /// The writes and reads of the value seen so far.
    pub writes: u64,
    pub reads: u64,
    /// How long the search has taken so far, in milliseconds.
    pub elapsed_ms: u64,
}

/// The line a search's progress is printed as: `{"searching": {...}}`.
#[derive(Serialize, Deserialize)]
struct Line {
    searching: Searching,
}

impl Searching {
    /// The line of JSON the engine prints.
    pub fn line(&self) -> String {
        let line = Line {
            searching: self.clone(),
        };
        serde_json::to_string(&line).expect("a search's progress serializes")
    }

    /// The search a line the engine printed reports, if it reports one.
    pub fn parse(line: &str) -> Option<Searching> {
        let line: Line = serde_json::from_str(line.trim()).ok()?;
        Some(line.searching)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search's progress reads back from the line the engine prints,
    /// and a replay's line is not taken for one.
    #[test]
    fn a_search_s_line_reads_back() {
        let searching = Searching {
            phase: Phase::Searching,
            before: 4782,
            window: (4130, 4782),
            reached: 4500,
            steps: 370,
            windows: 0,
            writes: 2,
            reads: 0,
            elapsed_ms: 412,
        };
        assert_eq!(Searching::parse(&searching.line()), Some(searching));
        assert_eq!(
            Searching::parse(r#"{"replaying":{"run":"a","from":0,"at":1,"to":2,"elapsed_ms":0}}"#),
            None
        );
    }
}
