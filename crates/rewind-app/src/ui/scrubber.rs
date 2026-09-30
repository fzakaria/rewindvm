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

use crate::describe::thousands;
use crate::engine::{Engine, EngineError, EngineResult, Forked};
use crate::model::{LogFilter, Motion};
use crate::run::{Agreement, Origin, Session, short_id};
use crate::tour::Tour;
use crate::ui::Launch;
use crate::ui::licensing::Licensing;
use crate::ui::viewer::FileViewer;
use crate::ui::widgets::Fonts;

/// How long a notice stays up.
const NOTICE_DURATION: Duration = Duration::from_secs(8);

/// Why an example run cannot be forked.
const EXAMPLE_FORK: &str = "Forking runs the build again from the playhead, which needs the engine and KVM on this machine. The example's inputs, its kernel, initramfs and Nix store paths, belong to the machine that recorded it. Record a run of your own with rewind nix to fork it.";

/// Where "Buy" goes.
pub const BUY_URL: &str = "https://rewindvm.dev/#buy";

/// Where a fork made from the playhead stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkState {
    /// The engine is working on it.
    Pending,
    /// The engine made it: its id, its directory, and how it ended.
    Created(Forked),
    /// The engine could not make it.
    Failed(String),
}

/// A fork made from the playhead, marked on the timeline.
#[derive(Clone, Debug)]
pub struct ForkMark {
    pub step: u64,
    /// The schedule seed the fork perturbs the run with.
    pub schedule: u64,
    pub state: ForkState,
}

/// How a notice is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeTone {
    Info,
    Error,
}

/// Something a notice offers to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    /// Open a run, compared with another.
    OpenRun { dir: PathBuf, compare: PathBuf },
    /// Open the store page in the browser.
    Buy,
    /// Open the license dialog.
    EnterLicense,
    /// Close the notice.
    Dismiss,
}

impl NoticeAction {
    pub fn label(&self) -> &'static str {
        match self {
            NoticeAction::OpenRun { .. } => "Open fork",
            NoticeAction::Buy => "Buy",
            NoticeAction::EnterLicense => "Enter license",
            NoticeAction::Dismiss => "Not now",
        }
    }
}

/// A message over the bottom right corner. One without actions goes away
/// on its own; one with actions stays until an action or its close button.
#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u64,
    pub tone: NoticeTone,
    pub title: SharedString,
    pub body: SharedString,
    pub actions: Vec<NoticeAction>,
}

/// What the "Open" prompt asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKind {
    RunDirectory,
    /// A trace file or a .rwd export.
    File,
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
    /// The license, the dialog to enter one, and the reminder.
    pub(super) licensing: Licensing,
    /// The guided tour, while it runs, and the focus its callout takes.
    pub(super) tour: Option<Tour>,
    pub(super) tour_focus: FocusHandle,
    /// The file viewer, while a file is open in it.
    pub(super) viewer: Option<FileViewer>,
    /// A press on the title bar that the next motion turns into a window
    /// move.
    pub(super) titlebar_armed: bool,
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
            licensing: Licensing::load(),
            tour: None,
            tour_focus: cx.focus_handle(),
            titlebar_armed: false,
            viewer: None,
            next_notice: 0,
            title: None,
        };
        if let Some(session) = launch.session {
            this.show(session, launch.step, cx);
        }
        this.start_licensing(cx);
        this
    }

    /// Puts a session on screen with the playhead at `step`, or at the
    /// failure, or at the end.
    pub(super) fn show(&mut self, session: Session, step: Option<u64>, cx: &mut Context<Self>) {
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
        self.tour = None;
        self.viewer = None;
        self.log_followed = None;
        self.files_followed = None;
        self.session = Some(session);
        cx.notify();
    }

    /// Sets the window title to the run's name when it changes.
    pub(super) fn sync_title(&mut self, window: &mut Window) {
        let title = match &self.session {
            Some(s) => format!("Rewind \u{b7} {}", s.run.label()),
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
        self.playhead_moved(cx);
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
        let id = self.post(tone, title.into(), body.into(), Vec::new(), cx);
        let timer = cx.background_executor().timer(NOTICE_DURATION);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.dismiss(id, cx));
        })
        .detach();
    }

    /// Shows a notice that offers `actions` and stays until one is taken
    /// or the notice is closed.
    pub(super) fn offer(
        &mut self,
        tone: NoticeTone,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
        actions: Vec<NoticeAction>,
        cx: &mut Context<Self>,
    ) {
        self.post(tone, title.into(), body.into(), actions, cx);
    }

    fn post(
        &mut self,
        tone: NoticeTone,
        title: SharedString,
        body: SharedString,
        actions: Vec<NoticeAction>,
        cx: &mut Context<Self>,
    ) -> u64 {
        // The same message again replaces the one on screen rather than
        // stacking a copy.
        self.notices.retain(|n| n.title != title || n.body != body);
        let id = self.next_notice;
        self.next_notice += 1;
        self.notices.push(Notice {
            id,
            tone,
            title,
            body,
            actions,
        });
        cx.notify();
        id
    }

    pub(super) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.retain(|n| n.id != id);
        cx.notify();
    }

    /// Takes a notice's action and closes the notice.
    pub(super) fn run_action(
        &mut self,
        id: u64,
        action: NoticeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss(id, cx);
        match action {
            NoticeAction::OpenRun { dir, compare } => self.open(dir, Some(compare), cx),
            NoticeAction::Buy => cx.open_url(BUY_URL),
            NoticeAction::EnterLicense => self.open_license_dialog(window, cx),
            NoticeAction::Dismiss => {}
        }
    }

    /// Shows an engine error: the engine's missing features as
    /// information, anything else as an error titled `failed`.
    fn report(&mut self, failed: &str, error: EngineError, cx: &mut Context<Self>) {
        if error.is_not_yet() {
            self.notify_user(
                NoticeTone::Info,
                "Not in the engine yet",
                error.to_string(),
                cx,
            );
            return;
        }
        self.notify_user(NoticeTone::Error, failed.to_string(), error.to_string(), cx);
    }

    /// Runs an engine call on a background thread and hands its result to
    /// `done` on the UI thread. Every call counts toward the evaluation
    /// reminder.
    fn with_engine<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        call: impl FnOnce(&dyn Engine) -> EngineResult<T> + Send + 'static,
        done: impl FnOnce(&mut Self, EngineResult<T>, &mut Context<Self>) + 'static,
    ) {
        self.count_engine_action(cx);
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

    /// Forks the run at the playhead, and marks the step on the timeline
    /// while the engine works. The schedule seed is one more than the
    /// forks already made from this run, on disk or in this session.
    pub(super) fn fork_here(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };

        // The example's inputs are another machine's, and forking runs
        // them again.
        if session.run.origin == Origin::Example {
            self.notify_user(
                NoticeTone::Info,
                "The example can be scrubbed, not forked",
                EXAMPLE_FORK,
                cx,
            );
            return;
        }
        let step = self.step;
        let schedule = 1 + session.forks_on_disk as u64 + self.forks.len() as u64;
        let run = session.run.path.clone();
        let parent = run.clone();
        let index = self.forks.len();
        self.forks.push(ForkMark {
            step,
            schedule,
            state: ForkState::Pending,
        });
        cx.notify();

        self.with_engine(
            cx,
            move |engine| engine.fork(&run, step, schedule),
            move |this, result, cx| {
                let Some(mark) = this.forks.get_mut(index) else {
                    return;
                };
                match result {
                    Ok(forked) => {
                        mark.state = ForkState::Created(forked.clone());
                        this.offer(
                            NoticeTone::Info,
                            format!(
                                "Forked at step {} as run {}",
                                thousands(step),
                                short_id(&forked.id)
                            ),
                            forked.summary.clone(),
                            vec![
                                NoticeAction::OpenRun {
                                    dir: forked.dir,
                                    compare: parent,
                                },
                                NoticeAction::Dismiss,
                            ],
                            cx,
                        );
                    }
                    Err(e) => {
                        mark.state = ForkState::Failed(e.to_string());
                        this.report("Could not fork", e, cx);
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
                Err(e) => this.report("Could not attach gdb", e, cx),
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
                    this.report("Could not open a shell", e, cx);
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
                Err(e) => this.report("Could not export", e, cx),
            },
        );
    }

    /// Compares with the other run: jumps to where the two first differ
    /// and says what each run did there.
    pub(super) fn diff_runs(&mut self, cx: &mut Context<Self>) {
        let Some(agreement) = self.session.as_ref().and_then(Session::agreement) else {
            return;
        };
        match agreement {
            Agreement::Same { title, lines } => {
                self.notify_user(NoticeTone::Info, title, lines.join("\n"), cx);
            }
            Agreement::Parted { step, title, lines } => {
                self.go_to(step, cx);
                self.notify_user(NoticeTone::Info, title, lines.join("\n"), cx);
            }
        }
    }

    /// Asks for a run to open, and opens it.
    pub(super) fn prompt_open(&mut self, kind: PickKind, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: kind == PickKind::File,
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
            let _ = this.update(cx, |this, cx| this.open(path, None, cx));
        })
        .detach();
    }

    /// Reads a run, and the run to compare it with, on a background
    /// thread and shows them.
    pub(super) fn open(&mut self, path: PathBuf, compare: Option<PathBuf>, cx: &mut Context<Self>) {
        self.loading = Some(path.clone());
        cx.notify();
        let read = cx
            .background_executor()
            .spawn(async move { Session::open(&path, compare.as_deref()) });
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
