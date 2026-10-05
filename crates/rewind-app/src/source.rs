//! The source panel's model: where in the program's own code a thread was
//! at a step, as `rewind where --json` answers, and what the panel says
//! when there is no answer.
//!
//! The engine finds the answer on a fork of the run at the step: it walks
//! the thread's stack in gdb and picks the innermost frame that is the
//! program's own code, past the C library, Rust's standard library and
//! dependencies. That takes seconds, so the panel asks only once the
//! playhead rests. The answer carries every source file the frames are
//! in, whole, so showing another frame's needs no new answer.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::engine::EngineError;
use crate::sideways::widest_line;

/// One frame of the thread's stack, innermost first.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Frame {
    pub level: u32,
    pub function: Option<String>,
    /// The source file as the program's DWARF names it, and its line.
    pub file: Option<String>,
    /// The path the answer's files key the source file by.
    pub fullname: Option<String>,
    pub line: Option<u32>,
    /// The instruction, in hex.
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

    /// What the panel shows in place of the source of a frame without
    /// any: the frame's address and program, and a line saying so.
    pub fn without_source(&self) -> Vec<String> {
        let place = match &self.object {
            Some(object) => format!("{}  {object}", self.pc),
            None => self.pc.clone(),
        };
        vec![place, NO_SOURCE.to_string()]
    }
}

/// What the panel says of a frame the engine found no source for.
const NO_SOURCE: &str = "no source";

/// How much of a source file the answer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Extent {
    /// The whole file.
    Whole,
    /// The lines around its frames' lines, of a file too large to carry.
    Window,
}

/// A source file as the answer carries it, split into lines once.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "Carried")]
pub struct SourceFile {
    pub extent: Extent,
    /// The number of the first line.
    pub first: u32,
    pub lines: Vec<String>,
    /// The characters in the widest line, for scrolling sideways.
    pub widest: usize,
}

/// A source file as `rewind where --json` prints it: its text in one
/// string, which is shorter than a list of lines.
#[derive(Deserialize)]
struct Carried {
    extent: Extent,
    first: u32,
    text: String,
}

impl From<Carried> for SourceFile {
    fn from(carried: Carried) -> SourceFile {
        let lines: Vec<String> = carried.text.lines().map(str::to_string).collect();
        SourceFile {
            extent: carried.extent,
            first: carried.first,
            widest: widest_line(&lines),
            lines,
        }
    }
}

impl SourceFile {
    /// The index in `lines` of line `line`, counted from 1, when the
    /// file carries it.
    pub fn row_of(&self, line: u32) -> Option<usize> {
        let row = line.checked_sub(self.first)? as usize;
        (row < self.lines.len()).then_some(row)
    }

    /// The number of line `row` of `lines`.
    pub fn number_of(&self, row: usize) -> u32 {
        self.first + row as u32
    }

    /// The digits in the number of the file's last line, which the line
    /// numbers are padded to.
    pub fn digits(&self) -> usize {
        self.number_of(self.lines.len().saturating_sub(1))
            .to_string()
            .len()
    }

    /// What the panel says of a window of a large file: which lines it
    /// shows. None for a file carried whole.
    pub fn note(&self) -> Option<String> {
        match self.extent {
            Extent::Whole => None,
            Extent::Window => {
                let last = self.number_of(self.lines.len().saturating_sub(1));
                Some(format!(
                    "showing lines {}\u{2013}{last} of a large file",
                    self.first
                ))
            }
        }
    }
}

/// What `rewind where --json` prints.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Located {
    pub pid: u32,
    pub tid: u32,
    pub process: String,
    pub frames: Vec<Frame>,
    /// The index in `frames` of the innermost frame in the program's own
    /// code, when there is one.
    pub chosen: Option<usize>,
    /// Each source file the frames are in, by the path a frame's
    /// `fullname` gives it.
    pub files: BTreeMap<String, SourceFile>,
}

impl Located {
    /// The chosen frame.
    pub fn chosen_frame(&self) -> Option<&Frame> {
        self.frames.get(self.chosen?)
    }

    /// The source file frame `frame` is in, when the answer carries it.
    pub fn file_of(&self, frame: usize) -> Option<&SourceFile> {
        let path = self.frames.get(frame)?.fullname.as_ref()?;
        self.files.get(path)
    }

    /// The row of frame `frame`'s file that holds the frame's line, which
    /// the panel marks and scrolls to the middle. None when the frame has
    /// no file, or its file does not carry the line.
    pub fn centred_row(&self, frame: usize) -> Option<usize> {
        let line = self.frames.get(frame)?.line?;
        self.file_of(frame)?.row_of(line)
    }
}

/// Whose stack the panel asks the engine for at a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Thread {
    /// The thread of the latest event at or before the step.
    Of { pid: u32, tid: u32 },
    /// The thread that was on the CPU, which the engine reads from the
    /// guest kernel: the latest event is the kernel's own, such as a
    /// console line printed while a thread of the job ran.
    OnTheCpu,
}

/// Whose stack the panel shows at a step whose latest event is `pid`'s
/// and `tid`'s.
pub fn target(pid: u32, tid: u32) -> Thread {
    const KERNEL_PID: u32 = 0;
    if pid == KERNEL_PID {
        return Thread::OnTheCpu;
    }
    Thread::Of { pid, tid }
}

/// What the panel shows for one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    /// An answer, with the frame whose source the panel shows: the
    /// chosen one until another frame is clicked.
    Located {
        step: u64,
        located: Located,
        selected: Option<usize>,
    },
    /// The step is before the job started, while the VM boots.
    Booting {
        step: u64,
        job_start: u64,
    },
    /// The engine cannot answer for this run at all; a fact about the
    /// run, not an error.
    Unreadable {
        step: u64,
        message: &'static str,
    },
    Failed {
        step: u64,
        message: String,
    },
}

impl Shown {
    /// The step the panel shows.
    pub fn step(&self) -> u64 {
        match self {
            Shown::Located { step, .. }
            | Shown::Booting { step, .. }
            | Shown::Unreadable { step, .. }
            | Shown::Failed { step, .. } => *step,
        }
    }

    /// The answer and the frame whose source the panel shows, when it
    /// shows an answer.
    pub fn located(&self) -> Option<(&Located, Option<usize>)> {
        match self {
            Shown::Located {
                located, selected, ..
            } => Some((located, *selected)),
            _ => None,
        }
    }

    /// Shows frame `frame`'s source in place of the frame shown. False,
    /// changing nothing, when the panel shows no answer with that frame.
    pub fn select_frame(&mut self, frame: usize) -> bool {
        let Shown::Located {
            located, selected, ..
        } = self
        else {
            return false;
        };
        if frame >= located.frames.len() {
            return false;
        }
        *selected = Some(frame);
        true
    }
}

/// Why a run's kernel cannot give a thread's stack: it was recorded
/// before the guest kernel listed its tasks.
pub const PREDATES_REASON: &str = "This run was recorded with a kernel from before Rewind could find a thread's stack at a step. Record it again to see its source here.";

/// What the engine says for such a run.
const PREDATES_MARKER: &str = "does not say where its tasks are";

/// How the rewind command starts its messages.
const ENGINE_PREFIX: &str = "rewind: ";

/// What the panel says while the engine looks, until the engine says
/// something of its own.
pub const LOOKING: &str = "Rewind is forking the run at this step and walking the thread's stack in gdb. This takes a few seconds.";

/// What the panel says while the engine looks: the latest of the engine's
/// own lines, once it has said one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    latest: Option<String>,
}

impl Progress {
    /// Takes `line`, which the engine said on its standard error; only
    /// rewind's own lines, not ones it passes on from gdb, are kept.
    pub fn said(&mut self, line: &str) {
        let Some(own) = line.strip_prefix(ENGINE_PREFIX) else {
            return;
        };
        self.latest = Some(own.trim().to_string());
    }

    /// The text in place of an answer: the latest line as a sentence, or
    /// LOOKING before the first.
    pub fn text(&self) -> String {
        let Some(latest) = &self.latest else {
            return LOOKING.to_string();
        };
        let mut chars = latest.chars();
        let Some(first) = chars.next() else {
            return LOOKING.to_string();
        };
        format!("{}{}.", first.to_uppercase(), chars.as_str())
    }
}

/// The panel's view of the engine's answer for `step`.
pub fn shown(step: u64, answer: Result<Located, EngineError>) -> Shown {
    match answer {
        Ok(located) => Shown::Located {
            step,
            selected: located.chosen,
            located,
        },
        Err(EngineError::Failed { message, .. }) if message.contains(PREDATES_MARKER) => {
            Shown::Unreadable {
                step,
                message: PREDATES_REASON,
            }
        }
        Err(e) if crate::engine::goes_another_way(&e) => Shown::Unreadable {
            step,
            message: crate::engine::REPLAYS_ANOTHER_WAY,
        },
        Err(EngineError::Failed { message, .. }) => Shown::Failed {
            step,
            message: format!(
                "Rewind could not find the thread's stack at this step: {}",
                message.trim_start_matches(ENGINE_PREFIX)
            ),
        },
        Err(e @ EngineError::Missing { .. }) => Shown::Failed {
            step,
            message: format!(
                "{e} Showing the source at a step needs the rewind command, which replays the run to that step."
            ),
        },
        Err(e) => Shown::Failed {
            step,
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    // `rewind where --json` answers as the engine prints them, read into
    // the panel's model, and the engine's refusals turned into what the
    // panel says.
    use super::*;
    use crate::selection::{Pos, Selection, Surface, Unit};
    use crate::viewer::TabbedLines;

    /// pool.c as the answer carries it, whole: 133 lines, with the lines
    /// around the crash's line 77 as the program has them, a tab-indented
    /// line far below it, and the rest numbered.
    fn pool_c() -> String {
        (1..=133)
            .map(|n| match n {
                76 => "\t\t\tfflush(stdout);\n".to_string(),
                77 => "\t\t\tp->queue->completed++;\n".to_string(),
                120 => "\tfree(p->queue);\n".to_string(),
                _ => format!("line {n}\n"),
            })
            .collect()
    }

    /// The answer for the Nix tutorial's failing run at the crash, as
    /// `rewind where 8fd5378d 5060 --json` printed it, with pool.c cut
    /// down as `pool_c` says, pthread_create.c's frame without a file,
    /// and clone3.S carried as a window of lines 70 to 86, as for a file
    /// too large to carry whole.
    fn at_the_crash() -> String {
        let clone3: Vec<String> = (70..=86).map(|n| format!("asm {n}")).collect();
        format!(
            r#"{{"run":"8fd5378ddf70075e","step":5060,"pid":166,"tid":174,"process":"test_pool_shutdown","frames":[{{"level":0,"function":"worker","file":"src/pool.c","fullname":"/build/mylib/src/pool.c","line":77,"pc":"0x55bfaf437437","object":"/build/mylib/tests/test_pool_shutdown"}},{{"level":1,"function":"start_thread","file":"pthread_create.c","fullname":null,"line":454,"pc":"0x7f615854d7d1","object":"/nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6"}},{{"level":2,"function":"__GI___clone3","file":"../sysdeps/unix/sysv/linux/x86_64/clone3.S","fullname":"/glibc/clone3.S","line":78,"pc":"0x7f61585d9b1c","object":"/nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6"}}],"chosen":0,"files":{{"/build/mylib/src/pool.c":{{"extent":"whole","first":1,"text":{pool}}},"/glibc/clone3.S":{{"extent":"window","first":70,"text":{clone3}}}}}}}"#,
            pool = serde_json::to_string(&pool_c()).unwrap(),
            clone3 = serde_json::to_string(&clone3.join("\n")).unwrap(),
        )
    }

    fn located() -> Located {
        serde_json::from_str(&at_the_crash()).unwrap()
    }

    /// The answer reads into the frames, the chosen one, and each source
    /// file once, split into lines; a frame finds its file by its
    /// fullname, and one without a fullname, or whose file the answer
    /// does not carry, has none.
    #[test]
    fn an_answer_reads_into_the_frames_and_their_files() {
        let located = located();
        assert_eq!((located.pid, located.tid), (166, 174));
        assert_eq!(located.process, "test_pool_shutdown");
        assert_eq!(located.frames.len(), 3);
        assert_eq!(located.chosen, Some(0));
        let chosen = &located.frames[0];
        assert_eq!(chosen.function_label(), "worker");
        assert_eq!(chosen.place_label(), "src/pool.c:77");

        assert_eq!(located.files.len(), 2);
        let pool = located.file_of(0).unwrap();
        assert_eq!((pool.extent, pool.first), (Extent::Whole, 1));
        assert_eq!(pool.lines.len(), 133);
        assert_eq!(pool.lines[76], "\t\t\tp->queue->completed++;");
        assert_eq!(pool.widest, "\t\t\tp->queue->completed++;".len() + 9);
        assert_eq!(located.file_of(1), None);
        let clone3 = located.file_of(2).unwrap();
        assert_eq!((clone3.extent, clone3.first), (Extent::Window, 70));
        assert_eq!(clone3.lines.len(), 17);
        assert_eq!(located.file_of(3), None);
    }

    /// The row a frame's file is scrolled to so the frame's line sits in
    /// the middle: the line's index among the lines the answer carries,
    /// counted from a window's first line. None for a frame without a
    /// file, or whose line the file does not have.
    #[test]
    fn the_frame_s_line_is_the_row_centred() {
        let mut located = located();
        assert_eq!(located.centred_row(0), Some(76));
        assert_eq!(located.centred_row(1), None);
        assert_eq!(located.centred_row(2), Some(8));
        assert_eq!(located.centred_row(3), None);

        located.frames[0].line = Some(134);
        assert_eq!(located.centred_row(0), None);
        located.frames[2].line = Some(69);
        assert_eq!(located.centred_row(2), None);
    }

    /// A file carried whole needs no note; a window of a large one says
    /// which lines it holds.
    #[test]
    fn a_window_of_a_large_file_says_which_lines_it_shows() {
        let located = located();
        assert_eq!(located.file_of(0).unwrap().note(), None);
        assert_eq!(
            located.file_of(2).unwrap().note().as_deref(),
            Some("showing lines 70\u{2013}86 of a large file")
        );
    }

    /// The panel's text is the whole file, so a selection reaches lines
    /// far from the frame's: lines 119 to 121 select and copy as the file
    /// has them, the tab included, though line 77 is the one marked.
    /// Selects over the file's lines as the panel reads them.
    #[test]
    fn a_selection_reaches_lines_far_from_the_frame_s() {
        let located = located();
        let lines = TabbedLines(&located.file_of(0).unwrap().lines);
        let mut selection = Selection::press(Surface::Source, Pos::new(118, 0), Unit::Char, &lines);
        selection.extend_to(Pos::new(120, 4));
        assert_eq!(selection.text(&lines), "line 119\n\tfree(p->queue);\nline");
    }

    /// A new answer shows its chosen frame. Clicking another frame shows
    /// that one instead, and a frame the answer does not have, or a click
    /// while the panel shows no answer, changes nothing.
    #[test]
    fn a_clicked_frame_is_shown_until_the_next_answer() {
        let located = located();
        let mut answer = shown(5060, Ok(located.clone()));
        assert_eq!(answer.located().map(|(_, at)| at), Some(Some(0)));

        assert!(answer.select_frame(2));
        let (_, at) = answer.located().unwrap();
        assert_eq!(at, Some(2));
        assert!(!answer.select_frame(3));
        assert_eq!(answer.located().map(|(_, at)| at), Some(Some(2)));

        let again = shown(5057, Ok(located));
        assert_eq!(again.located().map(|(_, at)| at), Some(Some(0)));

        let mut booting = Shown::Booting {
            step: 10,
            job_start: 900,
        };
        assert!(!booting.select_frame(0));
        assert_eq!(booting.located(), None);
    }

    /// A frame without source is shown by its address and program, and a
    /// line saying it has no source.
    #[test]
    fn a_frame_without_source_says_where_it_is() {
        let located = located();
        assert_eq!(
            located.frames[2].without_source(),
            vec![
                "0x7f61585d9b1c  /nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6"
                    .to_string(),
                "no source".to_string(),
            ]
        );
        let bare = Frame {
            object: None,
            ..located.frames[2].clone()
        };
        assert_eq!(
            bare.without_source(),
            vec!["0x7f61585d9b1c".to_string(), "no source".to_string()]
        );
    }

    /// A frame without a source line is placed by its program's file
    /// name, and one without a name by its address.
    #[test]
    fn a_frame_without_a_line_is_placed_by_its_program() {
        let frame = Frame {
            level: 3,
            function: None,
            file: None,
            fullname: None,
            line: None,
            pc: "0x401913".into(),
            object: Some("/newroot/src/big".into()),
        };
        assert_eq!(frame.function_label(), "0x401913");
        assert_eq!(frame.place_label(), "big");
    }

    /// A process's event names its thread; at the kernel's own events
    /// the engine looks for the thread on the CPU.
    #[test]
    fn a_kernel_event_asks_for_the_thread_on_the_cpu() {
        assert_eq!(target(0, 0), Thread::OnTheCpu);
        assert_eq!(target(166, 174), Thread::Of { pid: 166, tid: 174 });
    }

    /// A run recorded before its kernel listed its tasks says to record
    /// it again; any other refusal is shown with the engine's words, and
    /// a missing engine says what it is needed for.
    #[test]
    fn refusals_become_what_the_panel_says() {
        let failed = |message: &str| EngineError::Failed {
            command: "rewind where".into(),
            message: message.into(),
        };
        let predates = shown(
            10,
            Err(failed(
                "rewind: run 9d540a70's kernel does not say where its tasks are, so `rewind where` cannot find thread 34; record the run again",
            )),
        );
        assert_eq!(
            predates,
            Shown::Unreadable {
                step: 10,
                message: PREDATES_REASON
            }
        );

        let Shown::Failed { message, .. } = shown(10, Err(failed("rewind: no process 7"))) else {
            panic!("not a failure");
        };
        assert!(message.ends_with(": no process 7"));

        // A replay that went another way than the recording, in the
        // engine's words for it, is a fact about the run, not a failure.
        let another_way = format!(
            "rewind: replaying run 9d540a70 {} 506 than when it was recorded",
            rewind_trace::WENT_ANOTHER_WAY
        );
        assert_eq!(
            shown(10, Err(failed(&another_way))),
            Shown::Unreadable {
                step: 10,
                message: crate::engine::REPLAYS_ANOTHER_WAY
            }
        );

        let missing = EngineError::Missing {
            program: "rewind".into(),
        };
        let Shown::Failed { message, .. } = shown(10, Err(missing)) else {
            panic!("not a failure");
        };
        assert!(message.contains("needs the rewind command"));
    }

    /// The panel's loading text: the fixed sentence until the engine says
    /// a line of its own, then that line, the latest replacing the one
    /// before; lines without rewind's prefix change nothing. Feeds lines
    /// to a fresh Progress and reads its text after each.
    #[test]
    fn the_loading_text_is_the_engine_s_latest_line() {
        let mut progress = Progress::default();
        assert_eq!(progress.text(), LOOKING);

        progress.said("warning: something gdb said");
        assert_eq!(progress.text(), LOOKING);

        progress.said("rewind: walking thread 173's stack in gdb");
        assert_eq!(progress.text(), "Walking thread 173's stack in gdb.");

        progress.said("rewind: downloading debug info for libc.so.6; first time only");
        assert_eq!(
            progress.text(),
            "Downloading debug info for libc.so.6; first time only."
        );
    }
}
