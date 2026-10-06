//! The scrubber view's state and behavior: the playhead, where it can go,
//! the forks made from it, the notices shown over it, and opening runs.
//! Drawing lives in `render`.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext, Context, Entity, FocusHandle, PathPromptOptions, SharedString, Subscription,
    UniformListScrollHandle, Window, rgb, rgba,
};
use rewind_text_input::{TextInput, TextInputStyle};

use super::notices::Notices;
use super::runs::{Pick, RunsPanel};
use crate::answers::{Answers, FileKey, PlaceKey};
use crate::bookmarks::Bookmarks;
use crate::describe::thousands;
use crate::engine::{
    Engine, EngineError, EngineResult, EngineVersion, FileAtStep, Forked, GdbAt, PROGRAM_ENV,
    REPLAYS_ANOTHER_WAY, goes_another_way,
};
use crate::family::{
    Executing, Family, Progress, Row, RowKind, RunEntry, families, family_of, scan,
};
use crate::history::History;
use crate::model::{LogFilter, Motion};
use crate::request::{Request, Requests};
use crate::run::{Origin, Replays, Session, short_id};
use crate::search::Index;
use crate::selection::Surface;
use crate::source::Located;
use crate::stride::{Direction, Stride};
use crate::theme;
use crate::tour::Tour;
use crate::ui::bookmarks::{BookmarkEditor, bookmarks_dir};
use crate::ui::licensing::Licensing;
use crate::ui::link::LinkDialog;
use crate::ui::search::SearchBox;
use crate::ui::selectable::SelectionState;
use crate::ui::source::SourcePanel;
use crate::ui::splits::{Drag, Measured, Splits, layout_path};
use crate::ui::step_entry::StepEntry;
use crate::ui::tabs::RightTab;
use crate::ui::terminal::{PaneKind, TerminalPane};
use crate::ui::viewer::FileViewer;
use crate::ui::widgets::Fonts;
use crate::ui::{Launch, RightColumn};
use crate::view::View;

/// The app's version, which the engine's should match.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a notice that informs stays up.
const NOTICE_DURATION: Duration = Duration::from_secs(8);

/// How often a running export's notice reports the size written so far.
const EXPORT_PROGRESS_EVERY: Duration = Duration::from_millis(500);

/// Bytes in the megabytes the export's progress is counted in.
const MEGABYTE: f64 = 1_000_000.0;

/// The name an export is offered under when the run has no id.
const EXPORT_FALLBACK_NAME: &str = "run";

/// The extension of Rewind's export files.
const EXPORT_EXTENSION: &str = "rwd";

/// Why an example run cannot be forked.
const EXAMPLE_FORK: &str = "Forking runs the build again from the playhead, which needs the engine and KVM on this machine. The example's inputs, its kernel, initramfs and Nix store paths, belong to the machine that recorded it. Record a run of your own with rewind nix to fork it.";

/// Why a run the engine is still executing does not open.
const STILL_RUNNING: &str = "Its trace is written when it finishes, which for a fork takes about as long as the run it was forked from did.";

/// How often the Runs panel is read again while a run in it is running.
const RUNNING_POLL: Duration = Duration::from_secs(1);

/// Where "Buy" goes: the site's pricing section, whose buttons open
/// the Stripe checkouts, tagged so the site's analytics count the visit
/// as coming from the app.
pub const BUY_URL: &str = "https://rewindvm.dev/?utm_source=rewind-app&utm_medium=app#pricing";

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
    /// The fork's request, which its answer finds the mark by.
    pub request: Request,
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

impl NoticeTone {
    /// How long a notice of this tone stays up by itself: an error stays
    /// until it is closed, since it may arrive while the user looks
    /// elsewhere.
    fn lifetime(self) -> Option<Duration> {
        match self {
            NoticeTone::Info => Some(NOTICE_DURATION),
            NoticeTone::Error => None,
        }
    }
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
    /// Export the run on screen to this path, when there is no file
    /// chooser to ask with.
    ExportTo(PathBuf),
    /// Show a file in the desktop's file manager.
    Reveal(PathBuf),
    /// Put text on the clipboard.
    CopyText(String),
    /// Remove runs and their forks from the engine's runs.
    RemoveRuns(Vec<PathBuf>),
    /// Close the notice.
    Dismiss,
}

impl NoticeAction {
    pub fn label(&self) -> &'static str {
        match self {
            NoticeAction::OpenRun { .. } => "Open fork",
            NoticeAction::Buy => "Buy",
            NoticeAction::EnterLicense => "Enter license",
            NoticeAction::ExportTo(_) => "Export there",
            NoticeAction::Reveal(_) => "Show in folder",
            NoticeAction::CopyText(_) => "Copy path",
            NoticeAction::RemoveRuns(_) => "Delete",
            NoticeAction::Dismiss => "Not now",
        }
    }
}

/// A message over the bottom right corner. One without actions goes away
/// on its own; one with actions stays until an action or its close button.
#[derive(Clone, Debug)]

/// An export the engine is writing.
pub struct ExportJob {
    pub out: PathBuf,
    /// The notice that shows its progress.
    pub notice: u64,
    pub started: Instant,
}

/// What needs the engine to bring a run back to a step, for the message
/// shown when a run cannot be brought back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Shell,
    Gdb,
    Fork,
    Where,
}

impl Replay {
    fn doing(self) -> &'static str {
        match self {
            Replay::Shell => "Opening a shell",
            Replay::Gdb => "Attaching gdb",
            Replay::Fork => "Forking",
            Replay::Where => "Showing the source",
        }
    }

    fn to_do(self) -> &'static str {
        match self {
            Replay::Shell => "open a shell",
            Replay::Gdb => "attach gdb",
            Replay::Fork => "fork it",
            Replay::Where => "see its source",
        }
    }
}

/// Why `session`'s run cannot be brought back to a step for `replay`, in
/// words for a notice, or None when it can. `importing` says a replayable
/// export is on its way into the engine's runs.
pub fn replay_unavailable(session: &Session, replay: Replay, importing: bool) -> Option<String> {
    if session.replays == Replays::AnotherWay {
        return Some(REPLAYS_ANOTHER_WAY.to_string());
    }
    let origin = &session.run.origin;
    let needs = format!(
        "{} forks the run at the playhead, which needs the run's inputs and Rewind with KVM on this machine.",
        replay.doing()
    );
    let why = match origin {
        Origin::Local => return None,
        Origin::Example => format!(
            "This example was recorded on another machine and ships as its trace only. Record a run of your own with rewind nix to {} at any step.",
            replay.to_do()
        ),
        Origin::Export(export) if !export.replayable => format!(
            "{} holds the run's trace only. Open its replayable export, the -replayable.rwd file, to {}.",
            export.source.display(),
            replay.to_do()
        ),
        Origin::Export(_) if importing => {
            "The run is being imported into Rewind; try again in a moment.".to_string()
        }
        Origin::Export(export) => format!(
            "Importing the run into Rewind did not work. Run rewind import {}, then open the imported run.",
            export.file.display()
        ),
        Origin::TraceFile => {
            "This run was opened from a bare trace file. Open its run directory instead."
                .to_string()
        }
    };
    Some(format!("{needs} {why}"))
}

/// How the pointer moved the playhead on the track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrub {
    /// A press, which jumps.
    Press,
    /// A drag on from a press.
    Drag,
}

/// The scrubber: one run, the playhead over it, and everything around.
pub struct Scrubber {
    /// Kept for as long as the scrubber, which keeps its quit hook.
    _on_quit: Subscription,
    pub(super) focus: FocusHandle,
    pub(super) fonts: Fonts,
    pub(super) engine: Arc<dyn Engine>,
    pub(super) session: Option<Session>,
    /// A run being read in the background.
    pub(super) loading: Option<PathBuf>,
    /// The request that reads it, the latest run asked for; a run read
    /// for an earlier request is not shown.
    pub(super) opening: Option<Request>,
    /// The request importing a replayable export into the engine's runs.
    pub(super) importing: Option<Request>,
    /// Where requests to the engine and other background work get their
    /// numbers (see `crate::request`).
    pub(super) requests: Requests,
    /// What files held at steps, as the engine said, by the file's version.
    pub(super) file_answers: Answers<FileKey, FileAtStep>,
    /// Where threads were at steps, as the engine said.
    pub(super) place_answers: Answers<PlaceKey, Located>,
    /// The families of the engine's runs, the one changed last first, for
    /// the empty state.
    pub(super) recent: Vec<Family>,
    /// The empty state's field that filters the families it lists.
    pub(super) recent_filter: Entity<TextInput>,
    /// Draws the list again as the filter is typed into.
    _recent_typed: Subscription,
    /// The Runs panel: the family of the run on screen, the runs picked
    /// in it, its folds and its chosen comparison.
    pub(super) runs: RunsPanel,
    /// What the right column shows: "At this step", the Runs panel, the
    /// file viewer or the source panel.
    pub(super) right_tab: RightTab,
    /// Whether the Runs panel has a tab, which the runs pill opens and the
    /// tab's x takes away.
    pub(super) runs_tab: bool,
    /// Whether the bookmarks have a tab, which "All bookmarks" opens.
    pub(super) bookmarks_tab: bool,
    /// How the panels share the window, which their edges drag.
    pub(super) splits: Splits,
    /// The edge being dragged, if one is.
    pub(super) split_drag: Option<Drag>,
    /// Where the areas the edges divide were last painted.
    pub(super) measured: Rc<Measured>,
    /// The Open link dialog, when it is open.
    pub(super) link_dialog: Option<LinkDialog>,
    pub(super) step: u64,
    /// The steps the playhead jumped from, for Back and Forward.
    pub(super) history: History,
    /// The step readout's field, while a step is being typed.
    pub(super) step_entry: Option<StepEntry>,
    /// What Previous and Next stop at.
    pub(super) stride: Stride,
    /// The search box, while it is open.
    pub(super) search: Option<SearchBox>,
    /// The run's bookmarks.
    pub(super) bookmarks: Bookmarks,
    /// The bookmark note dialog, while it is open.
    pub(super) bookmark_editor: Option<BookmarkEditor>,
    /// The steps the timeline is zoomed in on; None shows the whole run.
    pub(super) view: Option<View>,
    /// How far along the track the pointer is, while it is over it.
    pub(super) hover: Option<f32>,
    /// The searchable text of the run it was built for, by its path.
    pub(super) search_index: Option<(PathBuf, Rc<Index>)>,
    /// Where the "Stop at" menu opened, while it is open.
    pub(super) stride_menu: Option<gpui::Point<gpui::Pixels>>,
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
    pub(super) notices: Notices,
    /// The license, the dialog to enter one, and the reminder.
    pub(super) licensing: Licensing,
    /// The guided tour, while it runs, and the focus its callout takes.
    pub(super) tour: Option<Tour>,
    pub(super) tour_focus: FocusHandle,
    /// Whether the shortcut sheet is open, and the focus it takes.
    pub(super) shortcuts_open: bool,
    pub(super) shortcuts_focus: FocusHandle,
    /// The file viewer, while a file is open in it.
    pub(super) viewer: Option<FileViewer>,
    /// The source panel, while it is open; it shares the file viewer's
    /// place.
    pub(super) source: Option<SourcePanel>,
    /// A press on the title bar that the next motion turns into a window
    /// move.
    pub(super) titlebar_armed: bool,
    /// The text selection, the context menu, and where lines were drawn.
    pub(super) selecting: SelectionState,
    /// The terminal pane, while a shell or gdb is open in it.
    pub(super) terminal: Option<TerminalPane>,
    /// The export the engine is writing, if any.
    pub(super) exporting: Option<ExportJob>,
    /// The window title last set, to set it only when it changes.
    title: Option<String>,
}

impl Scrubber {
    pub fn new(launch: Launch, fonts: Fonts, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        // Lookups still running when the app quits are stopped, as
        // dropping the viewer and the source panel stops them.
        let on_quit = cx.on_app_quit(|this: &mut Scrubber, _| {
            this.viewer = None;
            this.source = None;
            async {}
        });
        let recent_filter = cx.new(|cx| {
            let style = TextInputStyle {
                placeholder: "Filter by name, derivation or run id".into(),
                placeholder_color: rgb(theme::MUTED).into(),
                caret_color: rgb(theme::AMBER).into(),
                selection_color: rgba(theme::FOCUS_RING_A).into(),
            };
            TextInput::new(style, cx)
        });
        let recent_typed = cx.observe(&recent_filter, |_, _, cx| cx.notify());
        let mut this = Scrubber {
            _on_quit: on_quit,
            recent_filter,
            _recent_typed: recent_typed,
            focus,
            fonts,
            engine: launch.engine,
            session: None,
            loading: None,
            opening: None,
            importing: None,
            requests: Requests::default(),
            file_answers: Answers::default(),
            place_answers: Answers::default(),
            link_dialog: None,
            recent: Vec::new(),
            runs: RunsPanel::default(),
            right_tab: RightTab::AtStep,
            runs_tab: false,
            bookmarks_tab: false,
            splits: layout_path().map(|p| Splits::load(&p)).unwrap_or_default(),
            split_drag: None,
            measured: Rc::new(Measured::default()),
            step: 0,
            history: History::default(),
            step_entry: None,
            stride: Stride::Every,
            stride_menu: None,
            search: None,
            search_index: None,
            bookmarks: Bookmarks::default(),
            bookmark_editor: None,
            view: None,
            hover: None,
            log_filter: LogFilter::Output,
            log_scroll: UniformListScrollHandle::new(),
            files_scroll: UniformListScrollHandle::new(),
            log_followed: None,
            files_followed: None,
            dragging: Rc::new(Cell::new(false)),
            forks: Vec::new(),
            notices: Notices::default(),
            licensing: Licensing::load(),
            tour: None,
            tour_focus: cx.focus_handle(),
            shortcuts_open: false,
            shortcuts_focus: cx.focus_handle(),
            titlebar_armed: false,
            selecting: SelectionState::default(),
            terminal: None,
            exporting: None,
            viewer: None,
            source: None,
            title: None,
        };
        if let Some(session) = launch.session {
            this.show(session, launch.step, cx);
            if launch.right == RightColumn::Source {
                this.open_source(cx);
            }
        } else {
            this.reload_runs(cx);
        }
        this.check_engine(cx);
        this
    }

    /// Asks the engine its version once, at start, in the background, and
    /// says so when it is missing or of another version than the app: an
    /// older engine lacks commands the app runs, and its refusals would
    /// show where the answers go.
    fn check_engine(&mut self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let asked = crate::jobs::on_own_thread(move || engine.version());
        cx.spawn(async move |this, cx| {
            let result = asked.await;
            let _ = this.update(cx, |this, cx| this.engine_checked(result, cx));
        })
        .detach();
    }

    fn engine_checked(&mut self, result: EngineResult<EngineVersion>, cx: &mut Context<Self>) {
        match result {
            Ok(engine) => {
                if let Some(warning) = engine.mismatch(APP_VERSION) {
                    self.notify_user(
                        NoticeTone::Error,
                        "The engine is another version",
                        warning,
                        cx,
                    );
                }
            }
            Err(EngineError::Missing { program }) => self.notify_user(
                NoticeTone::Info,
                "The engine is not installed",
                format!(
                    "Runs open and scrub without it. Files, source, shells, gdb and forks need the engine command {program} on PATH, or {PROGRAM_ENV} naming it."
                ),
                cx,
            ),
            Err(e) => self.report("The engine did not say its version", e, cx),
        }
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
        if let Some(why) = &session.unopened_compare {
            self.notify_user(
                NoticeTone::Info,
                "Opened without a comparison",
                format!("The run to compare with did not open: {why}"),
                cx,
            );
        }
        // Steps jumped from in another run mean nothing in this one; the
        // same run shown again, as an import does, keeps them.
        let same_run = self
            .session
            .as_ref()
            .is_some_and(|shown| shown.run.id().is_some() && shown.run.id() == session.run.id());
        if !same_run {
            self.history = History::default();
            self.stride = Stride::Every;
            self.view = None;
        }
        self.step = start;
        self.search = None;
        self.bookmark_editor = None;
        self.bookmarks = bookmarks_dir(&session.run)
            .map(|dir| Bookmarks::load(&dir))
            .unwrap_or_default();
        self.forks.clear();
        self.tour = None;
        self.viewer = None;
        self.source = None;
        let keeps = match self.right_tab {
            RightTab::Runs | RightTab::Bookmarks => true,
            RightTab::Compare => session.other.is_some(),
            RightTab::AtStep | RightTab::File | RightTab::Source => false,
        };
        if !keeps {
            self.right_tab = RightTab::AtStep;
        }
        self.log_followed = None;
        self.files_followed = None;
        self.selecting.selection = None;

        // A replayable export goes into the engine's runs, where it can
        // be forked, while it is on screen.
        let replayable = match &session.run.origin {
            Origin::Export(export) if export.replayable => Some(export.file.clone()),
            _ => None,
        };
        // The example's two runs are its family, for the tour; they are
        // not among the engine's runs.
        if session.run.origin == Origin::Example {
            let now = std::time::SystemTime::now();
            let runs = std::iter::once(&session.run)
                .chain(session.other.as_ref())
                .filter_map(|run| {
                    Some(RunEntry::from_manifest(
                        &run.path,
                        run.manifest.as_ref()?,
                        now,
                        Executing::No,
                    ))
                })
                .collect();
            self.runs.set_family(Some(Family { runs }));
        }

        // The engine's runs are read after the run is on screen, for its
        // family and the forks it has.
        self.session = Some(session);
        if let Some(file) = replayable {
            self.bring_in(file, cx);
        }
        self.reload_runs(cx);
        cx.notify();
    }

    /// Reads the engine's runs in the background, for the families the
    /// empty state lists and the family of the run on screen.
    pub(super) fn reload_runs(&mut self, cx: &mut Context<Self>) {
        let read = cx.background_executor().spawn(async move {
            crate::engine::runs_dir()
                .map(|dir| scan(&dir))
                .unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            let runs = read.await;
            let _ = this.update(cx, |this, cx| this.apply_runs(runs, cx));
        })
        .detach();
    }

    /// Shows `runs`, the engine's runs: the families the empty state lists
    /// and the family of the run on screen.
    fn apply_runs(&mut self, runs: Vec<RunEntry>, cx: &mut Context<Self>) {
        let running = runs.iter().any(|r| r.progress == Progress::Running);
        let origin = self.session.as_ref().map(|s| &s.run.origin);
        if origin != Some(&Origin::Example) {
            let shown = self
                .session
                .as_ref()
                .filter(|s| s.run.origin == Origin::Local)
                .and_then(|s| s.run.id())
                .map(ToString::to_string);
            self.runs
                .set_family(shown.and_then(|id| family_of(runs.clone(), &id)));
        }
        self.recent = families(runs);
        if running {
            self.watch_running(cx);
        }
        cx.notify();
    }

    /// Reads the engine's runs again every RUNNING_POLL while a fork made
    /// here or any run the engine executes is running, so the Runs panel
    /// lists a fork as running once its directory appears, and as how it
    /// ended once it finishes.
    pub(super) fn watch_running(&mut self, cx: &mut Context<Self>) {
        if self.runs.watching {
            return;
        }
        self.runs.watching = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(RUNNING_POLL).await;
                let more = this.update(cx, |this, cx| {
                    let forking = this.forks.iter().any(|f| f.state == ForkState::Pending);
                    if !forking && !this.runs.any_running() {
                        this.runs.watching = false;
                        return false;
                    }
                    this.reload_runs(cx);
                    true
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// The runs pill: shows the Runs panel, opening its tab, or closes
    /// the tab when the panel shows.
    pub(super) fn toggle_runs(&mut self, cx: &mut Context<Self>) {
        if self.right_tab == RightTab::Runs {
            self.close_tab(RightTab::Runs, cx);
            return;
        }
        self.select_tab(RightTab::Runs, cx);
    }

    /// Opens a run of the family, compared with the run
    /// [`Family::compare_with`] names unless another comparison is pinned.
    pub(super) fn open_family_run(&mut self, run: RunEntry, cx: &mut Context<Self>) {
        // A run the engine is still executing has no trace to open yet.
        if run.progress == Progress::Running {
            self.notify_user(
                NoticeTone::Info,
                format!("Run {} is still running", short_id(&run.id)),
                STILL_RUNNING,
                cx,
            );
            return;
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.run.origin == Origin::Example)
        {
            self.notify_user(
                NoticeTone::Info,
                "The example's runs are on screen already",
                "This is the failing run, compared with the passing one. Record runs of your own to browse and fork them here.",
                cx,
            );
            return;
        }
        // The chosen comparison holds, except against the run itself.
        // Without one, the run is compared with the run it hangs under in
        // the panel, which for a run from boot is not in its manifest, or
        // for a window rewind check narrowed, with the window one step
        // shorter, as check reports the two.
        let above = self
            .runs
            .family
            .as_ref()
            .and_then(|f| f.compare_with(&run))
            .map(|r| r.dir.clone());
        let compare = self
            .runs
            .pinned_compare
            .clone()
            .filter(|pinned| *pinned != run.dir)
            .or(above);
        self.open(run.dir, compare, cx);
    }

    /// The Runs panel's rows as it draws them, folded as the user left
    /// them. The runs on screen and compared always have rows of their own.
    pub(super) fn runs_rows(&self) -> Rc<Vec<Row>> {
        let ids = |run: Option<&crate::run::Run>| {
            run.and_then(|r| r.id())
                .map(ToString::to_string)
                .unwrap_or_default()
        };
        let shown = ids(self.session.as_ref().map(|s| &s.run));
        let compared = ids(self.session.as_ref().and_then(|s| s.other.as_ref()));
        self.runs.rows(&shown, &compared)
    }

    /// How many forks of the family repeat an older fork's trace.
    pub(super) fn identical_forks(&self) -> usize {
        self.runs.identical()
    }

    /// The folding row that Runs panel row `index` goes with, as the id
    /// of the schedule 0 run it is under and whether it is folded.
    pub(super) fn fold_for_row(&self, index: usize) -> Option<(String, RowKind)> {
        self.runs.fold_for_row(&self.runs_rows(), index)
    }

    /// Shows the runs a folding row stands for, or folds them again.
    pub(super) fn toggle_fold(&mut self, under: &str, cx: &mut Context<Self>) {
        self.runs.toggle_fold(under);
        cx.notify();
    }

    /// Opens `run` compared with the run on screen.
    /// Compares the run on screen with `run`, and keeps comparing with it
    /// as other runs are opened from the Runs panel.
    pub(super) fn compare_against(&mut self, run: RunEntry, cx: &mut Context<Self>) {
        let Some(shown) = self.session.as_ref().map(|s| s.run.path.clone()) else {
            return;
        };
        self.runs.pinned_compare = Some(run.dir.clone());
        self.open(shown, Some(run.dir), cx);
    }

    /// Drops the chosen comparison: the run on screen, and every run
    /// opened after, is compared with its parent again.
    pub(super) fn compare_with_parents(&mut self, cx: &mut Context<Self>) {
        self.runs.pinned_compare = None;
        let Some(shown) = self.session.as_ref().map(|s| s.run.path.clone()) else {
            return;
        };
        let above = self.runs.family.as_ref().and_then(|f| {
            let run = f.runs.iter().find(|r| r.dir == shown)?;
            f.tree_parent(run).map(|r| r.dir.clone())
        });
        self.open(shown, above, cx);
    }

    /// A press on Runs panel row `index` with a modifier: Ctrl picks or
    /// unpicks the run, Shift picks every run from the last one picked.
    /// Returns whether the press was a pick, which a click then ignores.
    pub(super) fn pick_run(
        &mut self,
        index: usize,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        let how = if modifiers.shift {
            Pick::Range
        } else if modifiers.control || modifiers.platform {
            Pick::Toggle
        } else {
            Pick::Open
        };
        let picked = self.runs.pick(&self.runs_rows(), index, how);
        if picked {
            cx.notify();
        }
        picked
    }

    /// Picks every run in the Runs panel.
    pub(super) fn pick_all_runs(&mut self, cx: &mut Context<Self>) {
        self.runs.pick_all();
        cx.notify();
    }

    /// A right click on row `index`: the row joins the pick unless it is
    /// in it already, when the click acts on every picked run.
    pub(super) fn pick_for_menu(&mut self, index: usize) {
        let rows = self.runs_rows();
        self.runs.pick_for_menu(&rows, index);
    }

    /// The runs the Runs panel's menu acts on: the picked ones, in the
    /// panel's order.
    pub(super) fn runs_menu_targets(&self) -> Vec<RunEntry> {
        self.runs.menu_targets()
    }

    /// Deletes `runs` and their forks: a single fork with no forks of its
    /// own at once, anything more once the user says so.
    pub(super) fn ask_delete_runs(&mut self, runs: Vec<RunEntry>, cx: &mut Context<Self>) {
        let Some(deletion) = self.runs.deletion(&runs) else {
            return;
        };
        let Some(question) = deletion.question else {
            self.remove_runs(deletion.dirs, cx);
            return;
        };
        self.offer(
            NoticeTone::Info,
            question,
            "Their traces and keyframes are deleted from Rewind's runs. Pages other runs share stay.",
            vec![NoticeAction::RemoveRuns(deletion.dirs), NoticeAction::Dismiss],
            cx,
        );
    }

    /// Has the engine delete the runs in `dirs` and their forks. When the
    /// run on screen was one of them, the nearest run left above it takes
    /// its place, or the empty state when none is.
    fn remove_runs(&mut self, dirs: Vec<PathBuf>, cx: &mut Context<Self>) {
        let call_dirs = dirs.clone();
        self.with_engine(
            cx,
            move |engine| {
                let mut removed = Vec::new();
                for dir in &call_dirs {
                    removed.extend(engine.remove(dir)?);
                }
                Ok(removed)
            },
            move |this, result, cx| {
                let removed = match result {
                    Ok(removed) => removed,
                    Err(e) => {
                        this.report("Could not delete", e, cx);
                        this.reload_runs(cx);
                        return;
                    }
                };
                this.runs.picked.retain(|id| !removed.contains(id));

                // A comparison chosen with a run that is gone is dropped,
                // or every run opened after would be compared with nothing.
                this.runs.pinned_compare = this
                    .runs
                    .pinned_compare
                    .take()
                    .filter(|dir| !removed.iter().any(|id| dir.ends_with(id)));
                let shown = this
                    .session
                    .as_ref()
                    .and_then(|s| s.run.id())
                    .map(ToString::to_string);

                // The nearest ancestor of the run on screen that is left.
                let mut replacement = None;
                if let (Some(shown), Some(family)) = (&shown, &this.runs.family)
                    && removed.contains(shown)
                {
                    let mut at = family.runs.iter().find(|r| &r.id == shown);
                    while let Some(run) = at {
                        if !removed.contains(&run.id) {
                            replacement = Some(run.dir.clone());
                            break;
                        }
                        at = run
                            .parent
                            .as_ref()
                            .and_then(|p| family.runs.iter().find(|r| r.id == p.id));
                    }
                    if replacement.is_none() {
                        this.session = None;
                        this.runs.set_family(None);
                    }
                }
                this.notify_user(
                    NoticeTone::Info,
                    match removed.len() {
                        1 => "Deleted 1 run".to_string(),
                        n => format!("Deleted {n} runs"),
                    },
                    removed
                        .iter()
                        .map(|id| short_id(id))
                        .collect::<Vec<_>>()
                        .join(", "),
                    cx,
                );
                if let Some(dir) = replacement {
                    this.open(dir, None, cx);
                }
                this.reload_runs(cx);
            },
        );
    }

    /// Opens a family from the empty state: a failing run compared with a
    /// passing one when it has both, else its base run, with "At this
    /// step" showing where the two part.
    pub(super) fn open_family(&mut self, family: &Family, cx: &mut Context<Self>) {
        self.right_tab = RightTab::AtStep;
        self.runs.pinned_compare = None;
        let (run, compare) = family.to_open();
        self.open(run.dir.clone(), compare.map(|c| c.dir.clone()), cx);
    }

    /// Has the engine remove the forks that repeat an older fork's trace,
    /// under each run of the family that has some, then reads the runs
    /// again.
    pub(super) fn prune_identical(&mut self, cx: &mut Context<Self>) {
        let Some(family) = &self.runs.family else {
            return;
        };
        let roots: Vec<PathBuf> = family
            .roots_with_identical()
            .into_iter()
            .map(|r| r.dir)
            .collect();
        if roots.is_empty() {
            return;
        }
        self.runs.pruning = true;
        cx.notify();
        self.with_engine(
            cx,
            move |engine| {
                let mut removed = Vec::new();
                for root in &roots {
                    removed.extend(engine.prune_identical(root)?);
                }
                Ok(removed)
            },
            |this, result, cx| {
                this.runs.pruning = false;
                match result {
                    Ok(removed) => this.notify_user(
                        NoticeTone::Info,
                        format!(
                            "Removed {} identical fork{}",
                            removed.len(),
                            if removed.len() == 1 { "" } else { "s" }
                        ),
                        "Each repeated the trace of an older fork, which is kept.",
                        cx,
                    ),
                    Err(e) => this.report("Could not remove identical forks", e, cx),
                }
                this.reload_runs(cx);
            },
        );
    }

    /// Imports the export `file` into the engine's runs in the background,
    /// then shows the imported run at the same step, so the shell, gdb,
    /// fork and file buttons work on it. The app already has the file, so
    /// nothing is downloaded again. A failure leaves the trace on screen.
    fn bring_in(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        let request = self.requests.issue();
        self.importing = Some(request);
        let compare = self
            .session
            .as_ref()
            .and_then(|s| s.other.as_ref())
            .map(|other| other.path.clone());
        let engine = self.engine.clone();
        let read = crate::jobs::on_own_thread({
            let file = file.clone();
            move || {
                let imported = engine.import(&file).map_err(|e| e.to_string())?;
                Session::open(&imported.dir, compare.as_deref()).map_err(|e| format!("{e:#}"))
            }
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                // An import started after this one is the one to wait on.
                if this.importing != Some(request) {
                    return;
                }
                this.importing = None;

                // Another run opened meanwhile keeps the screen.
                let still_shown = matches!(
                    this.session.as_ref().map(|s| &s.run.origin),
                    Some(Origin::Export(export)) if export.file == file
                );
                if !still_shown {
                    return;
                }
                match result {
                    Ok(session) => {
                        let step = this.step;
                        this.show(session, Some(step), cx);
                    }
                    Err(message) => this.notify_user(
                        NoticeTone::Info,
                        "Shown from its trace",
                        format!(
                            "Importing the run into Rewind did not work, so the shell, gdb and forks are off: {message}"
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
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
        self.keep_playhead_in_view();

        // The lists and cards say what is true at the playhead, so a
        // selection in them would now cover other text.
        self.clear_selection_in(&[
            Surface::Processes,
            Surface::Files,
            Surface::EventCard,
            Surface::Divergence,
        ]);
        // Only the tab on screen follows the playhead; one behind it
        // catches up when it is chosen.
        match self.right_tab {
            RightTab::File => self.playhead_moved(cx),
            RightTab::Source => self.source_playhead_moved(cx),
            RightTab::AtStep | RightTab::Compare | RightTab::Runs | RightTab::Bookmarks => {}
        }
        cx.notify();
    }

    /// Moves the playhead to `step` as a jump, which Back undoes.
    pub(super) fn jump_to(&mut self, step: u64, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let step = step.min(session.run.timeline.total);
        self.history.jumped(self.step, step);
        self.go_to(step, cx);
    }

    /// Moves the playhead by a motion: an event, a step, a phase, or to
    /// one of the markers. Motions that leave the neighbourhood are jumps.
    pub(super) fn go(&mut self, motion: Motion, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let target = session
            .run
            .timeline
            .seek(motion, self.step, session.divergence_step());
        if motion.is_jump() {
            self.jump_to(target, cx);
            if motion == Motion::Divergence {
                self.value_moment(cx);
            }
            return;
        }
        match motion {
            Motion::PreviousEvent => self.step_by_stride(Direction::Back, cx),
            Motion::NextEvent => self.step_by_stride(Direction::Forward, cx),
            _ => self.go_to(target, cx),
        }
    }

    /// Closes the nearest thing open, as Escape does outside the
    /// terminal pane: the menu, the file, source or runs tab on screen,
    /// or the terminal pane, and gives the keyboard back to the scrubber.
    pub(super) fn close_nearest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = Open {
            menu: self.selecting.menu.is_some() || self.stride_menu.is_some(),
            tab: self.right_tab,
            terminal: self.terminal.is_some(),
        };
        match escape_closes(open) {
            Some(Closable::Menu) => {
                self.close_context_menu(cx);
                self.close_stride_menu(cx);
            }
            Some(Closable::Tab(tab)) => self.close_tab(tab, cx),
            Some(Closable::Terminal) => self.close_terminal(cx),
            None => return,
        }
        window.focus(&self.focus, cx);
    }

    /// Moves the playhead back to the step it last jumped from.
    pub(super) fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(step) = self.history.back(self.step) {
            self.go_to(step, cx);
        }
        cx.notify();
    }

    /// Moves the playhead forward to the step Back last left.
    pub(super) fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(step) = self.history.forward(self.step) {
            self.go_to(step, cx);
        }
        cx.notify();
    }

    /// Moves the playhead to a point along the track, from 0 to 1. A
    /// press there is a jump; dragging on from it is not.
    pub(super) fn scrub_to(&mut self, fraction: f32, scrub: Scrub, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let step = self.timeline_view().step_at(fraction);
        match scrub {
            Scrub::Press => self.jump_to(step, cx),
            Scrub::Drag => self.go_to(step, cx),
        }
    }

    pub(super) fn toggle_console(&mut self, cx: &mut Context<Self>) {
        self.log_filter = match self.log_filter {
            LogFilter::Output => LogFilter::WithConsole,
            LogFilter::WithConsole => LogFilter::Output,
        };
        self.clear_selection_in(&[Surface::Log]);
        cx.notify();
    }

    /// Shows a notice, and takes it down after its tone's lifetime; an
    /// error stays until it is closed.
    pub(super) fn notify_user(
        &mut self,
        tone: NoticeTone,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self.post(tone, title.into(), body.into(), Vec::new(), cx);
        let Some(lifetime) = tone.lifetime() else {
            return;
        };
        let timer = cx.background_executor().timer(lifetime);
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
    ) -> u64 {
        self.post(tone, title.into(), body.into(), actions, cx)
    }

    /// Changes the body of a notice on screen.
    pub(super) fn update_notice(&mut self, id: u64, body: String, cx: &mut Context<Self>) {
        if self.notices.set_body(id, body.into()) {
            cx.notify();
        }
    }

    fn post(
        &mut self,
        tone: NoticeTone,
        title: SharedString,
        body: SharedString,
        actions: Vec<NoticeAction>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.notices.post(tone, title, body, actions);
        cx.notify();
        id
    }

    pub(super) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.remove(id);
        self.clear_selection_in(&[Surface::Notice(id)]);
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
            NoticeAction::ExportTo(path) => self.start_export(path, cx),
            NoticeAction::Reveal(path) => cx.reveal_path(&path),
            NoticeAction::CopyText(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text))
            }
            NoticeAction::RemoveRuns(dirs) => self.remove_runs(dirs, cx),
            NoticeAction::Dismiss => {}
        }
    }

    /// Shows an engine error as an error titled `failed`.
    fn report(&mut self, failed: &str, error: EngineError, cx: &mut Context<Self>) {
        self.notify_user(NoticeTone::Error, failed.to_string(), error.to_string(), cx);
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
        let task = crate::jobs::on_own_thread(move || call(engine.as_ref()));
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
        if let Some(reason) = replay_unavailable(session, Replay::Fork, self.importing.is_some()) {
            self.notify_user(
                NoticeTone::Info,
                "This run cannot be forked yet",
                reason,
                cx,
            );
            return;
        }
        let step = self.step;
        let id = session.run.id().map_or("", |id| id.as_str());
        let schedule = next_fork_schedule(self.runs.family.as_ref(), id, &self.forks);
        let run = session.run.path.clone();
        let parent = run.clone();
        let request = self.requests.issue();
        self.forks.push(ForkMark {
            request,
            step,
            schedule,
            state: ForkState::Pending,
        });
        self.watch_running(cx);
        cx.notify();

        self.with_engine(
            cx,
            move |engine| engine.fork(&run, step, schedule),
            move |this, result, cx| {
                // The fork's mark, unless another run was shown meanwhile;
                // the fork is made either way, so it is announced either way.
                let mark = this.forks.iter_mut().find(|m| m.request == request);
                match result {
                    Ok(forked) => {
                        if let Some(mark) = mark {
                            mark.state = ForkState::Created(forked.clone());
                        }
                        this.reload_runs(cx);
                        if forked.first_difference.is_some() {
                            this.value_moment(cx);
                        }
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
                        if let Some(mark) = mark {
                            mark.state = ForkState::Failed(e.to_string());
                        }

                        // A run this build replays another way cannot be
                        // brought to a step again, while it is on screen.
                        let shown = this.session.as_mut().filter(|s| s.run.path == parent);
                        if let Some(session) = shown.filter(|_| goes_another_way(&e)) {
                            session.replays = Replays::AnotherWay;
                        }
                        this.report("Could not fork", e, cx);
                    }
                }
            },
        );
    }

    /// Runs gdb against a fork of the run at the playhead, in the
    /// terminal pane.
    pub(super) fn attach_gdb(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_replay(Replay::Gdb, window, cx);
    }

    /// Opens a shell inside a fork of the run at the playhead, in the
    /// terminal pane.
    pub(super) fn open_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_replay(Replay::Shell, window, cx);
    }

    fn open_replay(&mut self, replay: Replay, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };

        // Runs the engine cannot bring back say so instead of opening a
        // pane that would only print an error.
        if let Some(reason) = replay_unavailable(session, replay, self.importing.is_some()) {
            self.notify_user(
                NoticeTone::Info,
                format!("{} needs a run recorded here", replay.doing()),
                reason,
                cx,
            );
            return;
        }
        let (run, step) = (session.run.path.clone(), self.step);

        // The shell starts in the process the playhead's event is from,
        // with its root, working directory and environment; the engine
        // takes the job's once that process has exited.
        let t = &session.run.timeline;
        let pid = t
            .event_index_at(step)
            .and_then(|i| t.event(i))
            .map(|e| e.pid);
        let (kind, pid, command) = match replay {
            Replay::Shell => (
                PaneKind::Shell,
                pid,
                self.engine.shell_command(&run, step, pid),
            ),
            // gdb starts in the thread and frame the source panel shows
            // for this step, when it shows one; else the engine picks the
            // thread `rewind where` would.
            Replay::Gdb => {
                let at = self.place_at_playhead().map(|(located, frame)| GdbAt {
                    pid: located.pid,
                    tid: located.tid,
                    frame: frame.map(|f| located.frames[f].level),
                });
                (PaneKind::Gdb, None, self.engine.gdb_command(&run, step, at))
            }
            // A fork makes a run and the source fills a panel rather than
            // a pane; fork_here and open_source do them.
            Replay::Fork | Replay::Where => return,
        };
        self.open_terminal(kind, step, pid, command, window, cx);
    }

    /// Asks where to save the run as a replayable .rwd file, then has the
    /// engine write it there. Runs that are not a run directory on this
    /// machine say what can be done with them instead.
    pub(super) fn export(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let refusal = match &session.run.origin {
            Origin::Local => None,
            Origin::Example => Some((
                "The example cannot be exported".to_string(),
                "Export writes a run with everything another machine needs to replay it: its keyframes, memory pages, inputs and kernel. The example ships as its trace only. Record a run of your own with rewind nix, then export it.".to_string(),
            )),
            Origin::Export(export) => Some((
                "This run already is a .rwd file".to_string(),
                format!(
                    "It was opened from {}; share that. Export writes a replayable .rwd from a run directory on this machine.",
                    export.source.display()
                ),
            )),
            Origin::TraceFile => Some((
                "A bare trace cannot be exported".to_string(),
                "Export needs the run's directory, with its manifest, keyframes and inputs. Open the run directory, then export it.".to_string(),
            )),
        };
        if let Some((title, body)) = refusal {
            self.notify_user(NoticeTone::Info, title, body, cx);
            return;
        }
        if let Some(job) = &self.exporting {
            let body = format!(
                "The export to {} is still being written.",
                job.out.display()
            );
            self.notify_user(NoticeTone::Info, "An export is running", body, cx);
            return;
        }

        let name = session
            .run
            .id()
            .map_or(EXPORT_FALLBACK_NAME, |id| id.as_str());
        let suggested = format!("{name}.{EXPORT_EXTENSION}");
        let directory = export_directory();
        let fallback = directory.join(&suggested);
        let answer = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let answer = answer.await;
            let _ = this.update(cx, |this, cx| match answer {
                Ok(Ok(Some(path))) => this.start_export(path, cx),
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(e)) => {
                    this.offer(
                        NoticeTone::Info,
                        "No file chooser",
                        format!(
                            "{e:#}. Rewind can write the export to {} instead.",
                            fallback.display()
                        ),
                        vec![NoticeAction::ExportTo(fallback), NoticeAction::Dismiss],
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Has the engine write the run on screen to `out`, with a notice that
    /// counts what is written until it is done.
    pub(super) fn start_export(&mut self, out: PathBuf, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if self.exporting.is_some() {
            return;
        }
        let run = session.run.path.clone();
        let existed = out.exists();
        let notice = self.offer(
            NoticeTone::Info,
            "Exporting the run",
            format!("Writing {}\u{2026}", out.display()),
            Vec::new(),
            cx,
        );
        self.exporting = Some(ExportJob {
            out: out.clone(),
            notice,
            started: Instant::now(),
        });
        self.watch_export(cx);

        let target = out.clone();
        self.with_engine(
            cx,
            move |engine| engine.export(&run, &target),
            move |this, result, cx| {
                let Some(job) = this.exporting.take() else {
                    return;
                };
                this.dismiss(job.notice, cx);
                match result {
                    Ok(()) => {
                        let written = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                        this.offer(
                            NoticeTone::Info,
                            "Exported the run",
                            format!(
                                "{} ({:.1} MB). Another machine with a compatible CPU can open and replay it.",
                                out.display(),
                                written as f64 / MEGABYTE
                            ),
                            vec![
                                NoticeAction::Reveal(out.clone()),
                                NoticeAction::CopyText(out.display().to_string()),
                                NoticeAction::Dismiss,
                            ],
                            cx,
                        );
                        this.value_moment(cx);
                    }
                    Err(e) => {
                        // A file the export started is half written.
                        if !existed {
                            let _ = std::fs::remove_file(&out);
                        }
                        this.report("Could not export the run", e, cx);
                    }
                }
            },
        );
    }

    /// Updates the export's notice with the size written so far, until
    /// the export is done.
    fn watch_export(&mut self, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(EXPORT_PROGRESS_EVERY);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let Some(job) = &this.exporting else {
                    return;
                };
                let written = std::fs::metadata(&job.out).map(|m| m.len()).unwrap_or(0);
                let body = format!(
                    "Writing {} \u{b7} {:.1} MB so far \u{b7} {} s",
                    job.out.display(),
                    written as f64 / MEGABYTE,
                    job.started.elapsed().as_secs()
                );
                let notice = job.notice;
                this.update_notice(notice, body, cx);
                this.watch_export(cx);
            });
        })
        .detach();
    }

    /// Asks for a run to open, and opens it: a .rwd file, a bare trace, or
    /// the manifest.json in a run's directory, which opens the run. A
    /// desktop file chooser picks files or directories but not both, so
    /// one prompt for files reaches every kind.
    pub(super) fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: true,
            directories: false,
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
        let request = self.requests.issue();
        self.opening = Some(request);
        self.loading = Some(path.clone());
        cx.notify();
        let read = cx
            .background_executor()
            .spawn(async move { Session::open(&path, compare.as_deref()) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                // A run asked for after this one is the one to show.
                if this.opening != Some(request) {
                    return;
                }
                this.opening = None;
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

/// Where the save dialog for an export starts: the home directory, or the
/// working directory without one.
fn export_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| Path::new("/").to_path_buf())
}

/// What is open over or beside the panels, for Escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Open {
    menu: bool,
    /// The tab the right column shows.
    tab: RightTab,
    terminal: bool,
}

/// What Escape closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closable {
    Menu,
    Tab(RightTab),
    Terminal,
}

/// What one press of Escape closes: the nearest thing open, from the
/// menu over everything, through the file, source or runs tab on screen,
/// to the terminal pane under the panels.
fn escape_closes(open: Open) -> Option<Closable> {
    if open.menu {
        return Some(Closable::Menu);
    }
    if open.tab != RightTab::AtStep {
        return Some(Closable::Tab(open.tab));
    }
    if open.terminal {
        return Some(Closable::Terminal);
    }
    None
}

/// The schedule seed for the next fork of the run `id`: one past the
/// highest any fork of it has, on disk in `family` or still being made in
/// `pending`, so two forks of one run never share a seed.
fn next_fork_schedule(family: Option<&Family>, id: &str, pending: &[ForkMark]) -> u64 {
    let on_disk = family
        .into_iter()
        .flat_map(|f| &f.runs)
        .filter(|r| r.parent.as_ref().is_some_and(|p| p.id == id))
        .map(|r| r.schedule);
    let being_made = pending.iter().map(|m| m.schedule);
    1 + on_disk.chain(being_made).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    // What the scrubber decides about a run, with no window.
    use super::*;

    #[test]
    fn escape_closes_the_nearest_thing_first() {
        // With everything open, Escape takes the menu first, then the tab
        // on screen unless it is "At this step", then the terminal pane;
        // with nothing open it closes nothing.
        let all = Open {
            menu: true,
            tab: RightTab::File,
            terminal: true,
        };
        assert_eq!(escape_closes(all), Some(Closable::Menu));
        let tabs = Open { menu: false, ..all };
        assert_eq!(escape_closes(tabs), Some(Closable::Tab(RightTab::File)));
        let runs = Open {
            tab: RightTab::Runs,
            ..tabs
        };
        assert_eq!(escape_closes(runs), Some(Closable::Tab(RightTab::Runs)));
        let terminal = Open {
            tab: RightTab::AtStep,
            ..tabs
        };
        assert_eq!(escape_closes(terminal), Some(Closable::Terminal));
        let nothing = Open {
            terminal: false,
            ..terminal
        };
        assert_eq!(escape_closes(nothing), None);
    }

    #[test]
    fn each_fork_of_a_run_gets_a_schedule_of_its_own() {
        // Forks of base with seeds 1 and 4 on disk, a fork of another run
        // with seed 9, and one of base being made with seed 5: the next
        // fork of base takes 6, and of a run with no forks, 1.
        let manifest = crate::examples::manifest_of(crate::examples::FAILING);
        let entry = |id: &str, parent: Option<&str>, schedule: u64| RunEntry {
            id: id.into(),
            parent: parent.map(|id| crate::family::Parent {
                id: id.into(),
                step: 10,
            }),
            schedule,
            ..RunEntry::from_manifest(
                Path::new(id),
                &manifest,
                std::time::SystemTime::UNIX_EPOCH,
                Executing::No,
            )
        };
        let family = Family {
            runs: vec![
                entry("base", None, 0),
                entry("f1", Some("base"), 1),
                entry("f4", Some("base"), 4),
                entry("g9", Some("f1"), 9),
            ],
        };
        let pending = [ForkMark {
            request: Requests::default().issue(),
            step: 20,
            schedule: 5,
            state: ForkState::Pending,
        }];
        assert_eq!(next_fork_schedule(Some(&family), "base", &pending), 6);
        assert_eq!(next_fork_schedule(Some(&family), "f4", &[]), 1);
        assert_eq!(next_fork_schedule(None, "base", &[]), 1);
    }

    #[test]
    fn an_error_stays_until_it_is_closed() {
        // A notice that informs goes after a while; one that reports an
        // error stays until it is closed, so a message arriving while the
        // user looks elsewhere is still there to read.
        assert_eq!(NoticeTone::Info.lifetime(), Some(NOTICE_DURATION));
        assert_eq!(NoticeTone::Error.lifetime(), None);
    }

    #[test]
    fn a_run_replayed_another_way_cannot_be_brought_to_a_step() {
        // A run in a directory of its own can be forked and looked inside
        // until an answer finds this build replays it another way; then
        // every way of bringing it to a step says why not.
        let dir = std::env::temp_dir().join(format!("rewind-app-replays-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let trace = crate::examples::trace_of(crate::examples::FAILING);
        std::fs::write(dir.join(rewind_trace::manifest::TRACE), trace).unwrap();
        let mut session = Session::open(&dir, None).unwrap();
        assert_eq!(replay_unavailable(&session, Replay::Fork, false), None);

        session.replays = Replays::AnotherWay;
        for replay in [Replay::Fork, Replay::Shell, Replay::Gdb, Replay::Where] {
            let why = replay_unavailable(&session, replay, false);
            assert_eq!(why.as_deref(), Some(REPLAYS_ANOTHER_WAY));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_that_cannot_be_forked_here_says_what_would_let_it() {
        // The same run opened from each place a run comes from: a run
        // directory can be forked; the example, a view export, a bare
        // trace, and a replayable export still importing or whose import
        // failed each say what to do instead, naming what was asked for.
        let dir = std::env::temp_dir().join(format!("rewind-app-origins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let trace = crate::examples::trace_of(crate::examples::FAILING);
        std::fs::write(dir.join(rewind_trace::manifest::TRACE), trace).unwrap();
        let mut session = Session::open(&dir, None).unwrap();
        let export = |replayable| {
            Origin::Export(crate::run::Export {
                source: "/tmp/crash.rwd".into(),
                file: "/tmp/crash.rwd".into(),
                replayable,
            })
        };
        let mut why = |origin, importing| {
            session.run.origin = origin;
            replay_unavailable(&session, Replay::Shell, importing)
        };
        assert_eq!(why(Origin::Local, false), None);
        let example = why(Origin::Example, false).unwrap();
        assert!(example.contains("rewind nix to open a shell"), "{example}");
        let view = why(export(false), false).unwrap();
        assert!(view.contains("-replayable.rwd"), "{view}");
        let importing = why(export(true), true).unwrap();
        assert!(importing.contains("being imported"), "{importing}");
        let failed = why(export(true), false).unwrap();
        assert!(failed.contains("rewind import /tmp/crash.rwd"), "{failed}");
        let bare = why(Origin::TraceFile, false).unwrap();
        assert!(bare.contains("run directory"), "{bare}");
        assert!(bare.starts_with("Opening a shell forks the run"), "{bare}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
