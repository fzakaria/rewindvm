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
    /// The function's arguments, in the order it declares them, as gdb's
    /// `info args` lists them; empty without the function's DWARF.
    pub arguments: Vec<Variable>,
    /// The locals in scope at the frame's instruction, the innermost
    /// block's first, as gdb's `info locals` lists them.
    pub locals: Vec<Variable>,
    /// The expressions on the source lines around the frame's line, a
    /// few above it in its function and a couple below, each with what
    /// it is in the frame.
    pub expressions: Vec<Expression>,
    /// What each expression a command asked about is in the frame, in the
    /// order asked; None for one gdb could not evaluate there.
    pub evaluated: Vec<Option<Evaluated>>,
}

/// An argument or local of a frame, with its value as gdb prints it on one
/// line, cut short when long, and where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub location: Location,
}

/// An expression in a frame: its text, its value as gdb prints it, and
/// where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluated {
    pub text: String,
    pub value: String,
    pub location: Location,
}

/// An expression on a line of source: what it is in the frame, and where
/// on the line it is, in characters from the line's start: the
/// expression from `start`, and its last name, the one a person points
/// at, from `name_start` to `end`. `p->queue` in `p->queue->completed` is
/// an expression of its own, its last name `queue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expression {
    pub text: String,
    pub value: String,
    pub location: Location,
    pub line: u32,
    pub start: u32,
    pub name_start: u32,
    pub end: u32,
}

/// Where a value is, which decides whether the CPU can watch it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Location {
    /// In the process's memory: `size` bytes at `address`, in hex, few
    /// enough to watch, of a type named `type`.
    Memory {
        address: String,
        size: u64,
        class: ValueClass,
        #[serde(rename = "type")]
        type_name: String,
    },
    /// In memory, but more bytes than the debug registers watch at once.
    TooLarge {
        address: String,
        size: u64,
        class: ValueClass,
        #[serde(rename = "type")]
        type_name: String,
    },
    /// In a register at the frame's instruction, by name when gdb says.
    Register {
        register: Option<String>,
    },
    OptimizedOut,
    /// At an address the process cannot read, such as through a null
    /// pointer.
    Unreadable,
}

/// How a value's bytes print.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueClass {
    Pointer,
    Signed,
    Unsigned,
    Bool,
    Float,
    Bytes,
}

impl Location {
    /// The address and size of a value the CPU can watch.
    pub fn watchable(&self) -> Option<(u64, u64)> {
        match self {
            Location::Memory { address, size, .. } => {
                let address = u64::from_str_radix(address.trim_start_matches("0x"), 16).ok()?;
                Some((address, *size))
            }
            _ => None,
        }
    }

    /// Why a value cannot be watched, in words, or None when it can.
    pub fn why_not(&self) -> Option<String> {
        match self {
            Location::Memory { .. } => None,
            Location::TooLarge { size, .. } => Some(format!(
                "{size} bytes, past the 32 the CPU's debug registers watch at once; \
                 pick a field"
            )),
            Location::Register {
                register: Some(register),
            } => Some(format!(
                "in register {register} at this instruction; the CPU watches memory"
            )),
            Location::Register { register: None } => {
                Some("in a register at this instruction; the CPU watches memory".into())
            }
            Location::OptimizedOut => Some("optimized out: there is no value to watch".into()),
            Location::Unreadable => Some("at an address the process cannot read".into()),
        }
    }
}

impl ValueClass {
    /// `bytes`, a value of this class, as a person reads it: a pointer in
    /// hex, an integer in decimal, the rest as bytes in hex.
    pub fn show(self, bytes: &[u8]) -> String {
        self.shown(bytes).unwrap_or_else(|| {
            bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
    }

    /// `bytes` as [`ValueClass::show`] has them, or None to show them as
    /// bytes.
    fn shown(self, bytes: &[u8]) -> Option<String> {
        if bytes.is_empty() || bytes.len() > WORD {
            return None;
        }
        let mut word = [0u8; WORD];
        word[..bytes.len()].copy_from_slice(bytes);
        let unsigned = u64::from_le_bytes(word);
        let bits = 8 * bytes.len() as u32;
        Some(match self {
            ValueClass::Pointer => format!("{unsigned:#x}"),
            ValueClass::Unsigned => unsigned.to_string(),
            ValueClass::Signed => {
                let shift = u64::BITS - bits;
                (((unsigned << shift) as i64) >> shift).to_string()
            }
            ValueClass::Bool => (unsigned != 0).to_string(),
            ValueClass::Float => match bytes.len() {
                4 => f32::from_bits(unsigned as u32).to_string(),
                WORD => f64::from_bits(unsigned).to_string(),
                _ => return None,
            },
            ValueClass::Bytes => return None,
        })
    }
}

/// The bytes of the widest value shown as a number.
const WORD: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    /// A value's bytes print by their class: a pointer in hex, a signed
    /// int with its sign, a bool as a word, and a struct as bytes.
    #[test]
    fn values_show_by_their_class() {
        assert_eq!(ValueClass::Pointer.show(&[0; 8]), "0x0");
        assert_eq!(
            ValueClass::Pointer.show(&0x5556_f833_d2a0u64.to_le_bytes()),
            "0x5556f833d2a0"
        );
        assert_eq!(ValueClass::Signed.show(&(-3i32).to_le_bytes()), "-3");
        assert_eq!(ValueClass::Unsigned.show(&[0xff]), "255");
        assert_eq!(ValueClass::Bool.show(&[1]), "true");
        assert_eq!(ValueClass::Bytes.show(&[1, 0xab]), "01 ab");
    }

    /// Locations read from what the gdb script prints, and only one in
    /// memory and small enough can be watched.
    #[test]
    fn a_location_in_memory_is_watchable() {
        let memory: Location = serde_json::from_str(
            r#"{"kind":"memory","address":"0x5556f833c068","size":8,"class":"pointer","type":"struct queue *"}"#,
        )
        .unwrap();
        assert_eq!(memory.watchable(), Some((0x5556_f833_c068, 8)));
        assert_eq!(memory.why_not(), None);
        let register: Location =
            serde_json::from_str(r#"{"kind":"register","register":"rbx"}"#).unwrap();
        assert_eq!(register.watchable(), None);
        assert!(register.why_not().unwrap().contains("rbx"));
        let gone: Location = serde_json::from_str(r#"{"kind":"optimized-out"}"#).unwrap();
        assert_eq!(gone, Location::OptimizedOut);
    }
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
