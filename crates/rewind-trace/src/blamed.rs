//! What `rewind blame --at --json` prints and the desktop app reads: the
//! accesses to values in a process's memory before a step, each with the
//! thread and source line that made it, and where the process's other
//! threads were at the last write.

use serde::{Deserialize, Serialize};

use crate::located::ValueClass;

/// The accesses to values before a step of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blamed {
    pub run: String,
    /// The step asked about: the accesses are before its start.
    pub step: u64,
    pub pid: u32,
    /// The thread whose frame the values were named in, and that frame's
    /// place.
    pub tid: u32,
    pub place: Option<SourcePlace>,
    pub values: Vec<Watched>,
    /// The accesses found, oldest first.
    pub accesses: Vec<BlamedAccess>,
    /// Where the process's other threads were at the latest write's step,
    /// the writer left out; empty without a write.
    pub threads: Vec<ThreadAt>,
    /// The earliest step searched from, and the step no search goes
    /// before: the process's start or its last exec, or what was asked.
    pub searched_from: u64,
    pub since: u64,
    pub windows: u32,
    /// How many accesses past the ones listed the search saw, when it
    /// stopped at its limit.
    pub dropped: u64,
    pub elapsed_ms: u64,
}

/// A value watched: the expression, or address, it was asked as, where
/// it is, and what it holds at the step and held where the search began.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    pub text: String,
    pub address: String,
    pub size: u64,
    pub class: ValueClass,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub now: Option<String>,
    pub initial: Option<String>,
}

/// Whether an access read the value or wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessKind {
    Read,
    Write,
}

/// One access to a watched value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlamedAccess {
    /// The step it happened in: a fork at this step is just before it.
    pub step: u64,
    /// The index in `values` of the value accessed.
    pub value: usize,
    pub kind: AccessKind,
    pub tid: u32,
    pub thread: String,
    /// The value before the access and after it, shown by its class.
    pub before: Option<String>,
    pub after: Option<String>,
    /// The instruction that made the access, in hex, for one in user
    /// space; for one the kernel made, its symbol and the system call it
    /// was in, by number.
    pub pc: Option<String>,
    pub kernel: Option<String>,
    pub syscall: Option<u64>,
    pub place: Option<SourcePlace>,
    /// Where it falls among the process's accesses to the value in its
    /// step, from 1: among the writes for a write, among every access for
    /// a read. `rewind where --after-write` or `--after-access` with it
    /// looks at the program just after this access.
    pub hit: u32,
}

/// A place in a program's source: its function, file and line, and the
/// line's text when the file was found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePlace {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub text: Option<String>,
}

/// Where a thread was at a step: its frame in the program's own code,
/// else its innermost, and the function it was in at the top of its
/// stack, such as the C library's printf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadAt {
    pub tid: u32,
    pub place: Option<SourcePlace>,
    pub innermost: Option<String>,
    /// Why gdb could not walk the thread, when it could not.
    pub error: Option<String>,
}

impl Blamed {
    /// The latest write the answer holds.
    pub fn last_write(&self) -> Option<&BlamedAccess> {
        self.accesses
            .iter()
            .rev()
            .find(|a| a.kind == AccessKind::Write)
    }
}
