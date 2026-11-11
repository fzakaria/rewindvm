//! The answers an export carries, standing in for the engine.
//!
//! An export written with `rewind export --with-answers` carries, at a
//! few steps, where each live thread of the job was and the files it had
//! written (see [`rewind_trace::answers`]). The browser viewer has no
//! engine to ask, so for a run opened from such an export the panes that
//! read a file or locate a thread ask [`Answering`] instead: an
//! [`Engine`] that answers those two questions from the export, at the
//! steps it answers, and hands every other question, and every other run,
//! to the engine it wraps.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rewind_trace::answers::{Carried, FileAt, Kept, Place};

use crate::blame::{Ask, Blamed, Target};
use crate::describe::thousands;
use crate::engine::{
    Cancel, CommandLine, DirAtStep, Engine, EngineError, EngineResult, EngineVersion, FileAtStep,
    Forked, GdbAt, Imported, Outputs,
};
use crate::source::{AfterAccess, Located, Thread};
use crate::sweep::Checked;
use crate::viewer::FetchedAll;

/// What the viewer says, after the steps an export answers, of a look at
/// any other step.
const ELSEWHERE: &str = "At other steps, reading files and the source is in the desktop app.";

/// `steps` in words, as a sentence lists them: "step 719", "steps 719
/// and 731", "steps 1, 2 and 3".
fn steps_words(steps: &[u64]) -> String {
    let words: Vec<String> = steps.iter().map(|s| thousands(*s)).collect();
    match words.as_slice() {
        [] => "no step".to_string(),
        [one] => format!("step {one}"),
        [rest @ .., last] => format!("steps {} and {last}", rest.join(", ")),
    }
}

/// An engine that answers from an export's answers for one run.
pub struct Answering {
    /// The run the answers are of, by the path the app opened it as.
    run: PathBuf,
    answers: Arc<Carried>,
    /// The engine every other question goes to.
    inner: Arc<dyn Engine>,
}

impl Answering {
    pub fn new(run: PathBuf, answers: Arc<Carried>, inner: Arc<dyn Engine>) -> Answering {
        Answering {
            run,
            answers,
            inner,
        }
    }

    /// Whether a question about `run` is this engine's to answer.
    fn answers_for(&self, run: &Path) -> bool {
        run == self.run
    }

    /// The refusal of a question about `step` the export holds no answer
    /// for, which names the steps it answers.
    fn unanswered(&self, what: String) -> EngineError {
        EngineError::Unanswered {
            message: format!(
                "{what} The export answers at {}. {ELSEWHERE}",
                steps_words(&self.answers.steps())
            ),
        }
    }

    /// What the export holds of `path` at `step`, as a file the engine
    /// read.
    pub fn file(&self, step: u64, path: &str) -> EngineResult<FileAtStep> {
        let limits = self.answers.answers.limits;
        match self.answers.file(step, path) {
            Some(FileAt::Kept { bytes, kept }) => Ok(FileAtStep::Exists {
                bytes: bytes.to_vec(),
                complete: match kept {
                    Kept::Whole => FetchedAll::Yes,
                    Kept::Start => FetchedAll::No,
                },
            }),
            Some(FileAt::LeftOut(why)) => Err(EngineError::Unanswered {
                message: format!(
                    "The export left {path} out at step {}: {}.",
                    thousands(step),
                    why.words(limits)
                ),
            }),
            None if self.answers.at(step).is_some() => Err(self.unanswered(format!(
                "The export holds no copy of {path} at step {}: the job had not written it by then.",
                thousands(step)
            ))),
            None => Err(self.unanswered(format!(
                "The export holds no answers at step {}.",
                thousands(step)
            ))),
        }
    }

    /// Where `thread` was at `step`, as the export says.
    pub fn place(&self, step: u64, thread: Thread) -> EngineResult<Located> {
        let Thread::Of { tid, .. } = thread else {
            return Err(self.unanswered(format!(
                "The export does not say which thread was on the CPU at step {}.",
                thousands(step)
            )));
        };
        match self.answers.place(step, tid) {
            Some(Place::Located { located }) => Ok(Located::from((**located).clone())),
            Some(Place::Failed { error }) => Err(EngineError::Unanswered {
                message: format!(
                    "When the export was written, rewind where found no place for thread {tid} at step {}: {error}",
                    thousands(step)
                ),
            }),
            None => Err(self.unanswered(format!(
                "The export holds no place for thread {tid} at step {}.",
                thousands(step)
            ))),
        }
    }
}

impl Engine for Answering {
    fn fork(&self, run: &Path, step: u64, schedule: u64) -> EngineResult<Forked> {
        self.inner.fork(run, step, schedule)
    }

    fn export(&self, run: &Path, out: &Path) -> EngineResult<()> {
        self.inner.export(run, out)
    }

    fn import(&self, file: &Path) -> EngineResult<Imported> {
        self.inner.import(file)
    }

    fn prune_identical(&self, run: &Path) -> EngineResult<Vec<String>> {
        self.inner.prune_identical(run)
    }

    fn remove(&self, run: &Path) -> EngineResult<Vec<String>> {
        self.inner.remove(run)
    }

    fn version(&self) -> EngineResult<EngineVersion> {
        self.inner.version()
    }

    fn stop_all(&self) {
        self.inner.stop_all();
    }

    fn shell_command(&self, run: &Path, step: u64, pid: Option<u32>) -> CommandLine {
        self.inner.shell_command(run, step, pid)
    }

    fn gdb_command(&self, run: &Path, step: u64, at: Option<GdbAt>) -> CommandLine {
        self.inner.gdb_command(run, step, at)
    }

    fn cat(
        &self,
        run: &Path,
        step: u64,
        pid: Option<u32>,
        path: &str,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<FileAtStep> {
        if !self.answers_for(run) {
            return self.inner.cat(run, step, pid, path, cancel, progress);
        }
        self.file(step, path)
    }

    fn list(
        &self,
        run: &Path,
        step: u64,
        pid: Option<u32>,
        path: &str,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<DirAtStep> {
        if !self.answers_for(run) {
            return self.inner.list(run, step, pid, path, cancel, progress);
        }
        Err(self.unanswered(format!(
            "The export holds the files the job wrote, not a listing of {path}."
        )))
    }

    fn differing_outputs(
        &self,
        this: &Path,
        other: &Path,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Outputs> {
        self.inner.differing_outputs(this, other, cancel, progress)
    }

    fn locate(
        &self,
        run: &Path,
        step: u64,
        thread: Thread,
        after: Option<AfterAccess>,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Located> {
        if !self.answers_for(run) {
            return self
                .inner
                .locate(run, step, thread, after, cancel, progress);
        }
        if after.is_some() {
            return Err(self.unanswered(format!(
                "The export holds where threads were at the start of step {}, not after an access in it.",
                thousands(step)
            )));
        }
        self.place(step, thread)
    }

    fn blame(
        &self,
        run: &Path,
        target: &Target,
        ask: Ask,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Blamed> {
        self.inner.blame(run, target, ask, cancel, progress)
    }

    fn check_from(
        &self,
        run: &Path,
        step: u64,
        schedules: u64,
        seeds: u64,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str),
    ) -> EngineResult<Checked> {
        self.inner
            .check_from(run, step, schedules, seeds, cancel, progress)
    }
}

#[cfg(test)]
mod tests {
    // Answers packed by hand, as an export with answers carries them,
    // asked for through the engine trait: a file and a thread at an
    // answered step come back as the engine would give them, and anything
    // else is refused in words that name the answered steps. Another
    // run's questions go to the engine it wraps.
    use super::*;
    use crate::engine::CliEngine;
    use rewind_trace::answers::{Gathered, LIMITS, LeftOutWhy, ThreadAnswer, Why, pack};
    use std::collections::BTreeMap;

    const STEP: u64 = 5_775;
    const PID: u32 = 166;

    /// Why reading the job's files failed, as the export says it.
    const READ_ERROR: &str = "the fork did not answer";

    fn packed() -> Carried {
        let located = rewind_trace::located::Located {
            run: "0123456789abcdef".into(),
            step: STEP,
            pid: PID,
            tid: PID,
            process: "test_pool".into(),
            frames: Vec::new(),
            chosen: None,
            files: BTreeMap::new(),
        };
        let packed = pack(
            vec![Gathered {
                step: STEP,
                why: vec![Why::Failure],
                threads: vec![
                    ThreadAnswer {
                        pid: PID,
                        tid: PID,
                        place: Place::Located {
                            located: Box::new(located),
                        },
                    },
                    ThreadAnswer {
                        pid: PID,
                        tid: PID + 1,
                        place: Place::Failed {
                            error: "no symbols".into(),
                        },
                    },
                ],
                files: vec![
                    ("/build/out.log".into(), Ok(b"ok\n".to_vec())),
                    ("/build/gone".into(), Err(LeftOutWhy::NotRead)),
                    (
                        "/build/unread".into(),
                        Err(LeftOutWhy::ReadFailed {
                            error: READ_ERROR.into(),
                        }),
                    ),
                ],
            }],
            LIMITS,
        );
        let index = serde_json::to_vec(&packed.answers).unwrap();
        Carried::read(&index, packed.members.into_iter().collect()).unwrap()
    }

    fn answering() -> Answering {
        Answering::new(
            PathBuf::from("run.rwd"),
            Arc::new(packed()),
            Arc::new(CliEngine::new("/nonexistent/rewind")),
        )
    }

    fn cat(engine: &Answering, run: &str, step: u64, path: &str) -> EngineResult<FileAtStep> {
        engine.cat(
            Path::new(run),
            step,
            None,
            path,
            &Cancel::default(),
            &mut |_| {},
        )
    }

    fn locate(engine: &Answering, step: u64, tid: u32) -> EngineResult<Located> {
        let thread = Thread::Of { pid: PID, tid };
        engine.locate(
            Path::new("run.rwd"),
            step,
            thread,
            None,
            &Cancel::default(),
            &mut |_| {},
        )
    }

    #[test]
    fn an_answered_step_reads_its_file_and_its_thread() {
        // The file comes back whole; the thread comes back as `rewind
        // where --json` would have said it.
        let engine = answering();
        assert_eq!(
            cat(&engine, "run.rwd", STEP, "/build/out.log"),
            Ok(FileAtStep::Exists {
                bytes: b"ok\n".to_vec(),
                complete: FetchedAll::Yes
            })
        );
        let located = locate(&engine, STEP, PID).unwrap();
        assert_eq!((located.pid, located.tid), (PID, PID));
        assert_eq!(located.process, "test_pool");
    }

    #[test]
    fn what_the_export_lacks_is_said_with_the_steps_it_answers() {
        // A file left out says why, and a file whose read failed says the
        // read's error; a file never written, a step not answered and a
        // thread not answered each name the answered step; a thread rewind
        // where could not locate says what it said.
        let engine = answering();
        let words = |result: EngineResult<FileAtStep>| result.unwrap_err().to_string();
        assert!(
            words(cat(&engine, "run.rwd", STEP, "/build/gone")).contains("left /build/gone out")
        );
        let unread = words(cat(&engine, "run.rwd", STEP, "/build/unread"));
        assert!(unread.contains(READ_ERROR), "{unread}");
        let never = words(cat(&engine, "run.rwd", STEP, "/etc/hosts"));
        assert!(never.contains("at step 5,775"), "{never}");
        let elsewhere = words(cat(&engine, "run.rwd", STEP + 1, "/build/out.log"));
        assert!(
            elsewhere.contains("no answers at step 5,776"),
            "{elsewhere}"
        );
        assert!(elsewhere.contains(ELSEWHERE), "{elsewhere}");
        let failed = locate(&engine, STEP, PID + 1).unwrap_err().to_string();
        assert!(failed.contains("no symbols"), "{failed}");
        let absent = locate(&engine, STEP, PID + 2).unwrap_err().to_string();
        assert!(
            absent.contains("The export answers at step 5,775"),
            "{absent}"
        );
    }

    /// An export of the failing example's manifest and trace with the
    /// answers of [`packed`], in the order `rewind export` writes them.
    fn export_with_answers() -> Vec<u8> {
        let example = crate::archive::read_bytes(crate::examples::FAILING).unwrap();
        let answers = packed();
        let index = serde_json::to_vec(&answers.answers).unwrap();
        let mut entries: Vec<(String, Vec<u8>)> = vec![
            ("manifest.json".into(), example.manifest),
            ("trace.bin".into(), example.trace),
            (rewind_trace::answers::ANSWERS.into(), index),
        ];
        entries.extend(answers.contents);
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in &entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, &data[..]).unwrap();
        }
        let level = rewind_trace::export::COMPRESSION_LEVEL;
        zstd::encode_all(&tar.into_inner().unwrap()[..], level).unwrap()
    }

    #[test]
    fn answers_come_with_a_run_opened_from_memory() {
        // As the browser viewer opens an export: the bytes into a run,
        // which carries the answers at their step and none at another,
        // and the engine the panes are given reads the file there.
        use crate::run::{InMemoryOrigin, Run};
        let run =
            Run::open_bytes("run.rwd", &export_with_answers(), InMemoryOrigin::Export).unwrap();
        assert!(run.answers_at(STEP) && !run.answers_at(STEP + 1));
        let engine = Answering::new(
            run.path.clone(),
            run.answers.clone().unwrap(),
            Arc::new(CliEngine::new("/nonexistent/rewind")),
        );
        assert_eq!(
            cat(&engine, "run.rwd", STEP, "/build/out.log"),
            Ok(FileAtStep::Exists {
                bytes: b"ok\n".to_vec(),
                complete: FetchedAll::Yes
            })
        );

        // An export without answers opens with none.
        let plain = Run::open_bytes(
            "plain.rwd",
            crate::examples::FAILING,
            InMemoryOrigin::Export,
        )
        .unwrap();
        assert!(plain.answers.is_none());
    }

    #[test]
    fn answers_come_with_a_run_opened_from_a_file() {
        // As the desktop app opens an export: the file is unpacked into
        // the cache and the run opened from there, and the trace-only
        // export's answers come with it, as they do in the browser.
        use crate::run::Run;
        let cache = crate::archive::test_cache();
        let dir = cache.join(format!("answers-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("run.rwd");
        std::fs::write(&file, export_with_answers()).unwrap();
        let run = Run::open(&file).unwrap();
        assert!(run.answers_at(STEP) && !run.answers_at(STEP + 1));

        // An export without answers opens with none.
        let plain = dir.join("plain.rwd");
        std::fs::write(&plain, crate::examples::FAILING).unwrap();
        assert!(Run::open(&plain).unwrap().answers.is_none());
    }

    /// What checks.answers-app hands the test below: an export `rewind
    /// export --with-answers` wrote of a job that wrote a file and then
    /// failed, the file's path and bytes, and the function a bookmarked
    /// thread was in.
    const EXPORT_ENV: &str = "REWIND_ANSWERS_EXPORT";
    const PATH_ENV: &str = "REWIND_ANSWERS_PATH";
    const BYTES_ENV: &str = "REWIND_ANSWERS_BYTES";
    const FUNCTION_ENV: &str = "REWIND_ANSWERS_FUNCTION";

    #[test]
    #[ignore = "needs an export the engine wrote with answers: checks.answers-app"]
    fn an_engines_export_with_answers_reads_back_through_the_app() {
        // The export opens as the browser viewer opens one. At the step
        // the app marks as the failure, the file the job wrote reads back
        // whole through the engine the scrubber gives its panes; at the
        // bookmarked step, some thread is located in the function named.
        use crate::run::{InMemoryOrigin, Run};
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"));
        let bytes = std::fs::read(env(EXPORT_ENV)).unwrap();
        let run = Run::open_bytes("report.rwd", &bytes, InMemoryOrigin::Export).unwrap();
        let failure = run.timeline.failure.expect("the run failed").step;
        assert!(run.answers_at(failure), "no answers at step {failure}");
        let answers = run.answers.clone().unwrap();
        let engine = Answering::new(
            run.path.clone(),
            answers.clone(),
            Arc::new(CliEngine::new("/nonexistent/rewind")),
        );
        let path = env(PATH_ENV);
        assert_eq!(
            engine.cat(
                &run.path,
                failure,
                None,
                &path,
                &Cancel::default(),
                &mut |_| {}
            ),
            Ok(FileAtStep::Exists {
                bytes: env(BYTES_ENV).into_bytes(),
                complete: FetchedAll::Yes
            })
        );

        let function = env(FUNCTION_ENV);
        let marked = answers
            .answers
            .steps
            .iter()
            .find(|s| s.why.iter().any(|w| matches!(w, Why::Bookmark { .. })))
            .expect("no bookmarked step is answered");
        let found = marked.threads.iter().any(|t| {
            let thread = Thread::Of {
                pid: t.pid,
                tid: t.tid,
            };
            let located = engine.locate(
                &run.path,
                marked.step,
                thread,
                None,
                &Cancel::default(),
                &mut |_| {},
            );
            located.is_ok_and(|l| {
                l.chosen_frame()
                    .and_then(|f| f.function.as_deref())
                    .is_some_and(|f| f == function)
            })
        });
        assert!(found, "no thread at step {} is in {function}", marked.step);
    }

    #[test]
    fn the_answered_steps_are_listed_as_a_sentence_lists_them() {
        // One step is "step N"; several are "steps A, B and C".
        assert_eq!(steps_words(&[5_775]), "step 5,775");
        assert_eq!(steps_words(&[719, 731]), "steps 719 and 731");
        assert_eq!(steps_words(&[1, 2, 3]), "steps 1, 2 and 3");
    }

    #[test]
    fn another_runs_questions_go_to_the_engine_it_wraps() {
        // The wrapped engine is not installed, so its refusal says so.
        let engine = answering();
        let other = cat(&engine, "other.rwd", STEP, "/build/out.log");
        assert!(
            matches!(other, Err(EngineError::Missing { .. })),
            "{other:?}"
        );
    }
}
