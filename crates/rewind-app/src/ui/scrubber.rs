//! The scrubber view's state and behavior: the playhead, where it can go,
//! the forks made from it, the notices shown over it, and opening runs.
//! Drawing lives in `render`.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    ClipboardItem, Context, FocusHandle, PathPromptOptions, SharedString, UniformListScrollHandle,
    Window,
};

use crate::describe::{self, thousands};
use crate::engine::{Engine, EngineResult};
use crate::model::{LogFilter, Motion};
use crate::run::Session;
use crate::ui::Launch;
use crate::ui::widgets::Fonts;

/// How long a notice stays up.
const NOTICE_DURATION: Duration = Duration::from_secs(8);

/// Where a fork made from the playhead stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkState {
    /// The engine is working on it.
    Pending,
    /// The engine made it, in this directory.
    Created(PathBuf),
    /// The engine could not make it.
    Failed(String),
}

/// A fork made from the playhead, marked on the timeline.
#[derive(Clone, Debug)]
pub struct ForkMark {
    pub step: u64,
    pub seed: u64,
    pub state: ForkState,
}

/// How a notice is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeTone {
    Info,
    Error,
}

/// A message over the bottom right corner that goes away on its own.
#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u64,
    pub tone: NoticeTone,
    pub title: SharedString,
    pub body: SharedString,
}

/// What the "Open" prompt asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKind {
    RunDirectory,
    TraceFile,
}

/// The scrubber: one run, the playhead over it, and everything around.
pub struct Scrubber {
    pub(super) focus: FocusHandle,
    pub(super) fonts: Fonts,
    pub(super) engine: Arc<dyn Engine>,
    pub(super) session: Option<Session>,
    /// A run being read in the background.
    pub(super) loading: Option<PathBuf>,
    pub(super) step: u64,
    pub(super) log_filter: LogFilter,
    pub(super) log_scroll: UniformListScrollHandle,
    pub(super) files_scroll: UniformListScrollHandle,
    /// The playhead and line count the log last followed, so that a
    /// render that does not move the playhead leaves the user's scroll.
    pub(super) log_followed: Option<(u64, LogFilter)>,
    pub(super) files_followed: Option<usize>,
    /// Whether the pointer is dragging the playhead. Shared with the
    /// timeline's mouse listeners, which outlive a render.
    pub(super) dragging: Rc<Cell<bool>>,
    pub(super) forks: Vec<ForkMark>,
    pub(super) notices: Vec<Notice>,
    next_notice: u64,
    /// The window title last set, to set it only when it changes.
    title: Option<String>,
}

impl Scrubber {
    pub fn new(launch: Launch, fonts: Fonts, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut this = Scrubber {
            focus,
            fonts,
            engine: launch.engine,
            session: None,
            loading: None,
            step: 0,
            log_filter: LogFilter::Output,
            log_scroll: UniformListScrollHandle::new(),
            files_scroll: UniformListScrollHandle::new(),
            log_followed: None,
            files_followed: None,
            dragging: Rc::new(Cell::new(false)),
            forks: Vec::new(),
            notices: Vec::new(),
            next_notice: 0,
            title: None,
        };
        if let Some(session) = launch.session {
            this.show(session, launch.step, cx);
        }
        this
    }

    /// Puts a session on screen with the playhead at `step`, or at the
    /// failure, or at the end.
    fn show(&mut self, session: Session, step: Option<u64>, cx: &mut Context<Self>) {
        let timeline = &session.run.timeline;
        let start = step
            .or(timeline.failure.map(|f| f.step))
            .unwrap_or(timeline.total)
            .min(timeline.total);
        if let Some(warning) = &session.run.manifest_warning {
            self.notify_user(NoticeTone::Error, "Manifest ignored", warning.clone(), cx);
        }
        self.step = start;
        self.forks.clear();
        self.log_followed = None;
        self.files_followed = None;
        self.session = Some(session);
        cx.notify();
    }

    /// Sets the window title to the run's name when it changes.
    pub(super) fn sync_title(&mut self, window: &mut Window) {
        let title = match &self.session {
            Some(s) => format!("Rewind \u{b7} {}", s.run.name()),
            None => "Rewind".to_string(),
        };
        if self.title.as_deref() == Some(title.as_str()) {
            return;
        }
        window.set_window_title(&title);
        self.title = Some(title);
    }

    /// Moves the playhead to `step`, clamped to the run.
    pub(super) fn go_to(&mut self, step: u64, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let step = step.min(session.run.timeline.total);
        if step == self.step {
            return;
        }
        self.step = step;
        cx.notify();
    }

    /// Moves the playhead by a motion: an event, a step, a phase, or to
    /// one of the markers.
    pub(super) fn go(&mut self, motion: Motion, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let target = session
            .run
            .timeline
            .seek(motion, self.step, session.divergence_step());
        self.go_to(target, cx);
    }

    /// Moves the playhead to a point along the track, from 0 to 1.
    pub(super) fn scrub_to(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let step = session.run.timeline.step_at_fraction(fraction);
        self.go_to(step, cx);
    }

    pub(super) fn toggle_console(&mut self, cx: &mut Context<Self>) {
        self.log_filter = match self.log_filter {
            LogFilter::Output => LogFilter::WithConsole,
            LogFilter::WithConsole => LogFilter::Output,
        };
        cx.notify();
    }

    /// Shows a notice, and takes it down after `NOTICE_DURATION`.
    pub(super) fn notify_user(
        &mut self,
        tone: NoticeTone,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_notice;
        self.next_notice += 1;
        self.notices.push(Notice {
            id,
            tone,
            title: title.into(),
            body: body.into(),
        });
        cx.notify();

        let timer = cx.background_executor().timer(NOTICE_DURATION);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.dismiss(id, cx));
        })
        .detach();
    }

    pub(super) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.retain(|n| n.id != id);
        cx.notify();
    }

    /// Runs an engine call on a background thread and hands its result to
    /// `done` on the UI thread.
    fn with_engine<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        call: impl FnOnce(&dyn Engine) -> EngineResult<T> + Send + 'static,
        done: impl FnOnce(&mut Self, EngineResult<T>, &mut Context<Self>) + 'static,
    ) {
        let engine = self.engine.clone();
        let task = cx
            .background_executor()
            .spawn(async move { call(engine.as_ref()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Forks the run at the playhead with the next seed, and marks the
    /// step on the timeline while the engine works.
    pub(super) fn fork_here(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let step = self.step;
        let seed = session.run.seed() + self.forks.len() as u64 + 1;
        let run = session.run.path.clone();
        let index = self.forks.len();
        self.forks.push(ForkMark {
            step,
            seed,
            state: ForkState::Pending,
        });
        cx.notify();

        self.with_engine(
            cx,
            move |engine| engine.fork(&run, step, seed),
            move |this, result, cx| {
                let Some(mark) = this.forks.get_mut(index) else {
                    return;
                };
                match result {
                    Ok(path) => {
                        mark.state = ForkState::Created(path.clone());
                        this.notify_user(
                            NoticeTone::Info,
                            format!("Forked at step {}", thousands(step)),
                            format!("New run in {}", path.display()),
                            cx,
                        );
                    }
                    Err(e) => {
                        mark.state = ForkState::Failed(e.to_string());
                        this.notify_user(NoticeTone::Error, "Could not fork", e.to_string(), cx);
                    }
                }
            },
        );
    }

    /// Asks the engine for a gdb server at the playhead, and puts the
    /// command to attach on the clipboard.
    pub(super) fn attach_gdb(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let (run, step) = (session.run.path.clone(), self.step);
        self.with_engine(
            cx,
            move |engine| engine.gdb(&run, step),
            move |this, result, cx| match result {
                Ok(address) => {
                    let command = format!("target remote {address}");
                    cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                    this.notify_user(
                        NoticeTone::Info,
                        format!("gdb server at step {}", thousands(step)),
                        format!("{command} (copied to the clipboard)"),
                        cx,
                    );
                }
                Err(e) => {
                    this.notify_user(NoticeTone::Error, "Could not attach gdb", e.to_string(), cx)
                }
            },
        );
    }

    /// Asks the engine for a shell in the guest at the playhead.
    pub(super) fn open_shell(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let (run, step) = (session.run.path.clone(), self.step);
        self.with_engine(
            cx,
            move |engine| engine.shell(&run, step),
            move |this, result, cx| {
                if let Err(e) = result {
                    this.notify_user(
                        NoticeTone::Error,
                        "Could not open a shell",
                        e.to_string(),
                        cx,
                    );
                }
            },
        );
    }

    /// Asks the engine to export the run to a single file.
    pub(super) fn export(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let run = session.run.path.clone();
        self.with_engine(
            cx,
            move |engine| engine.export(&run),
            move |this, result, cx| match result {
                Ok(path) => {
                    this.notify_user(NoticeTone::Info, "Exported", path.display().to_string(), cx)
                }
                Err(e) => {
                    this.notify_user(NoticeTone::Error, "Could not export", e.to_string(), cx)
                }
            },
        );
    }

    /// Compares with the other run: jumps to where the two first differ
    /// and says what each run did there.
    pub(super) fn diff_runs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(other) = &session.other else {
            return;
        };
        let other_name = other.name();
        let Some(d) = session
            .comparison
            .as_ref()
            .and_then(|c| c.divergence.clone())
        else {
            self.notify_user(
                NoticeTone::Info,
                format!("Same as {other_name}"),
                "The two traces are identical, event for event.",
                cx,
            );
            return;
        };
        let here = session.run.timeline.event(d.index).map_or_else(
            || "the end of the run".to_string(),
            |e| describe::describe(e).text,
        );
        let there = other.timeline.event(d.index).map_or_else(
            || "the end of the run".to_string(),
            |e| describe::describe(e).text,
        );
        let step = d.left_step;
        self.go_to(step, cx);
        self.notify_user(
            NoticeTone::Info,
            format!("First difference at step {}", thousands(step)),
            format!("Here: {here}\n{other_name}: {there}"),
            cx,
        );
    }

    /// Asks for a run to open, and opens it.
    pub(super) fn prompt_open(&mut self, kind: PickKind, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: kind == PickKind::TraceFile,
            directories: kind == PickKind::RunDirectory,
            multiple: false,
            prompt: Some(SharedString::from("Open")),
        };
        let answer = cx.prompt_for_paths(options);
        cx.spawn(async move |this, cx| {
            let picked = match answer.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(e)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.notify_user(
                            NoticeTone::Error,
                            "No file chooser",
                            format!("{e:#}. Pass the run on the command line instead."),
                            cx,
                        );
                    });
                    None
                }
                _ => None,
            };
            let Some(path) = picked else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.open(path, cx));
        })
        .detach();
    }

    /// Reads a run on a background thread and shows it.
    pub(super) fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.loading = Some(path.clone());
        cx.notify();
        let read = cx
            .background_executor()
            .spawn(async move { Session::open(&path, None) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = None;
                match result {
                    Ok(session) => this.show(session, None, cx),
                    Err(e) => this.notify_user(
                        NoticeTone::Error,
                        "Could not open the run",
                        format!("{e:#}"),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
