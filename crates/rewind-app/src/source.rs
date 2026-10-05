//! The source panel's model: where in the program's own code a thread was
//! at a step, as `rewind where --json` answers, and what the panel says
//! when there is no answer.
//!
//! The engine finds the answer on a fork of the run at the step: it walks
//! the thread's stack in gdb and picks the innermost frame that is the
//! program's own code, past the C library, Rust's standard library and
//! dependencies. That takes seconds, so the panel asks only once the
//! playhead rests. The answer carries every frame's source, so showing
//! another frame's needs no new answer.

use serde::Deserialize;

use crate::engine::EngineError;

/// One frame of the thread's stack, innermost first.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Frame {
    pub level: u32,
    pub function: Option<String>,
    /// The source file as the program's DWARF names it, and its line.
    pub file: Option<String>,
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

/// Lines of a frame's source file around its line.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Source {
    /// The number of the first line.
    pub first: u32,
    pub lines: Vec<String>,
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
    /// Each frame's source lines, in the order of `frames`: None for a
    /// frame the engine found no source for.
    pub sources: Vec<Option<Source>>,
}

/// How many lines either side of a frame's line the panel shows: the line
/// sits in the middle, with room for the frames below.
pub const PANEL_RADIUS: u32 = 8;

impl Located {
    /// The chosen frame.
    pub fn chosen_frame(&self) -> Option<&Frame> {
        self.frames.get(self.chosen?)
    }

    /// Frame `frame`'s source lines within `radius` of its line, and the
    /// number of the first of them.
    pub fn source_around(&self, frame: usize, radius: u32) -> Option<(u32, &[String])> {
        let source = self.sources.get(frame)?.as_ref()?;
        let line = self.frames.get(frame)?.line?;
        let first = line.saturating_sub(radius).max(source.first);
        let skip = (first - source.first) as usize;
        let take = (line + radius + 1).saturating_sub(first) as usize;
        let end = (skip + take).min(source.lines.len());
        let lines = source.lines.get(skip..end)?;
        Some((first, lines))
    }
}

/// The process and thread whose stack the panel shows at a step: the
/// thread of the latest event at or before it. None when that event is
/// the kernel's, in no process.
pub fn target(pid: u32, tid: u32) -> Option<(u32, u32)> {
    const KERNEL_PID: u32 = 0;
    if pid == KERNEL_PID {
        return None;
    }
    Some((pid, tid))
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
    /// The event at the step is the kernel's; there is no thread to show.
    Kernel {
        step: u64,
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
            | Shown::Kernel { step }
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

    /// The answer for the Nix tutorial's failing run at the crash, as
    /// `rewind where 8fd5378d 5060 --json` printed it, with the first
    /// frame's source cut to three lines and glibc's sources, in this
    /// machine's debuginfod cache, left out.
    const AT_THE_CRASH: &str = r#"{"run":"8fd5378ddf70075e","step":5060,"pid":166,"tid":174,"process":"test_pool_shutdown","frames":[{"level":0,"function":"worker","file":"src/pool.c","fullname":"/build/mylib/src/pool.c","line":77,"pc":"0x55bfaf437437","object":"/build/mylib/tests/test_pool_shutdown"},{"level":1,"function":"start_thread","file":"pthread_create.c","fullname":null,"line":454,"pc":"0x7f615854d7d1","object":"/nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6"},{"level":2,"function":"__GI___clone3","file":"../sysdeps/unix/sysv/linux/x86_64/clone3.S","fullname":null,"line":78,"pc":"0x7f61585d9b1c","object":"/nix/store/h4wfwic161kxrr74jlzla5lsm28hgary-glibc-2.44-25/lib/libc.so.6"}],"chosen":0,"sources":[{"first":76,"lines":["\t\t\tfflush(stdout);","\t\t\tp->queue->completed++;","\t\t}"]},null,null]}"#;

    /// The answer reads into the frames, the chosen one, and each frame's
    /// source, null where the engine had none; fields the panel does not
    /// use, such as fullname, are skipped.
    #[test]
    fn an_answer_reads_into_the_frames_and_their_sources() {
        let located: Located = serde_json::from_str(AT_THE_CRASH).unwrap();
        assert_eq!((located.pid, located.tid), (166, 174));
        assert_eq!(located.process, "test_pool_shutdown");
        assert_eq!(located.frames.len(), 3);
        assert_eq!(located.chosen, Some(0));
        let chosen = &located.frames[0];
        assert_eq!(chosen.function_label(), "worker");
        assert_eq!(chosen.place_label(), "src/pool.c:77");
        assert_eq!(located.sources.len(), 3);
        assert_eq!(located.sources[0].as_ref().unwrap().first, 76);
        assert_eq!(located.sources[1], None);
    }

    /// The panel's lines are a frame's source within a radius of its line,
    /// cut where the engine's lines end; a frame without source has none.
    #[test]
    fn the_panel_shows_the_lines_around_a_frame_s_line() {
        let located: Located = serde_json::from_str(AT_THE_CRASH).unwrap();
        let (first, lines) = located.source_around(0, 1).unwrap();
        assert_eq!(first, 76);
        assert_eq!(lines.len(), 3);
        let (first, lines) = located.source_around(0, 0).unwrap();
        assert_eq!((first, lines.len()), (77, 1));
        assert_eq!(lines[0], "\t\t\tp->queue->completed++;");
        let (first, lines) = located.source_around(0, PANEL_RADIUS).unwrap();
        assert_eq!((first, lines.len()), (76, 3));
        assert_eq!(located.source_around(1, PANEL_RADIUS), None);
        assert_eq!(located.source_around(3, PANEL_RADIUS), None);
    }

    /// A new answer shows its chosen frame. Clicking another frame shows
    /// that one instead, and a frame the answer does not have, or a click
    /// while the panel shows no answer, changes nothing.
    #[test]
    fn a_clicked_frame_is_shown_until_the_next_answer() {
        let located: Located = serde_json::from_str(AT_THE_CRASH).unwrap();
        let mut answer = shown(5060, Ok(located.clone()));
        assert_eq!(answer.located().map(|(_, at)| at), Some(Some(0)));

        assert!(answer.select_frame(2));
        let (_, at) = answer.located().unwrap();
        assert_eq!(at, Some(2));
        assert!(!answer.select_frame(3));
        assert_eq!(answer.located().map(|(_, at)| at), Some(Some(2)));

        let again = shown(5057, Ok(located));
        assert_eq!(again.located().map(|(_, at)| at), Some(Some(0)));

        let mut kernel = Shown::Kernel { step: 10 };
        assert!(!kernel.select_frame(0));
        assert_eq!(kernel.located(), None);
    }

    /// A frame without source is shown by its address and program, and a
    /// line saying it has no source.
    #[test]
    fn a_frame_without_source_says_where_it_is() {
        let located: Located = serde_json::from_str(AT_THE_CRASH).unwrap();
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
            line: None,
            pc: "0x401913".into(),
            object: Some("/newroot/src/big".into()),
        };
        assert_eq!(frame.function_label(), "0x401913");
        assert_eq!(frame.place_label(), "big");
    }

    /// The kernel's own events name no thread to show.
    #[test]
    fn a_kernel_event_has_no_thread() {
        assert_eq!(target(0, 0), None);
        assert_eq!(target(166, 174), Some((166, 174)));
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
