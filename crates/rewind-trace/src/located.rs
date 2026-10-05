//! What `rewind where --json` prints and the desktop app's source panel
//! reads: a thread's frames at a step, the one in the program's own code,
//! and the source files they are in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A thread's stack at a step of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Located {
    pub run: String,
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    pub process: String,
    /// The thread's frames, innermost first.
    pub frames: Vec<Frame>,
    /// The index in `frames` of the innermost frame in the program's own
    /// code, when there is one.
    pub chosen: Option<usize>,
    /// Each source file the frames are in, once, by the path the frames'
    /// `fullname` gives it. A frame without a source file and line, or
    /// whose file was not found, has none here.
    pub files: BTreeMap<String, SourceFile>,
}

/// One frame of a thread's stack, as gdb lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub level: u32,
    pub function: Option<String>,
    /// The source file as the program's DWARF names it, and where it was
    /// found here, which `Located::files` keys it by.
    pub file: Option<String>,
    pub fullname: Option<String>,
    pub line: Option<u32>,
    /// The instruction, in hex: a frame's pc, or a caller's return address.
    pub pc: String,
    /// The program or library the instruction is in, by its path in the VM.
    pub object: Option<String>,
}

impl Frame {
    /// The frame's function, or its address when it has no name.
    pub fn function_label(&self) -> String {
        self.function.clone().unwrap_or_else(|| self.pc.clone())
    }

    /// Where the frame is: its source line, else its program's file name.
    pub fn place_label(&self) -> String {
        match (&self.file, self.line, &self.object) {
            (Some(file), Some(line), _) => format!("{file}:{line}"),
            (_, _, Some(object)) => object.rsplit('/').next().unwrap_or(object).to_string(),
            _ => String::new(),
        }
    }
}

/// How much of a source file an answer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Extent {
    /// The whole file.
    Whole,
    /// The lines around its frames' lines, of a file too large to carry.
    Window,
}

/// A source file, or the part of it an answer carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub extent: Extent,
    /// The number of the first line of `text`: 1 for a whole file.
    pub first: u32,
    /// The file's text from line `first` on: the whole file as it reads,
    /// or a window's lines joined by newlines.
    pub text: String,
}

impl SourceFile {
    /// The whole of `text`.
    pub fn whole(text: &str) -> SourceFile {
        SourceFile {
            extent: Extent::Whole,
            first: 1,
            text: text.to_string(),
        }
    }
}
