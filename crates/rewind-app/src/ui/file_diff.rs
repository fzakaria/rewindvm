//! The two-run file view: one file as this run and the compared run had
//! it, side by side or one under the other, with what differs marked in
//! each run's color, amber for this run and blue for the other. And the
//! list it starts from: the files the two runs' jobs left behind whose
//! bytes differ, which is what a build that does not reproduce needs.
//!
//! While two runs are open, a click on a file in the Files panel compares
//! it at the playhead: this run at the playhead's step, the other at the
//! step that matches it, read again whenever the playhead rests. The
//! Compare tab's "Files that differ" lists the files that differ at each
//! run's end, and a click on one compares it there. Reading a file means
//! the engine brings each run back to the step, which takes seconds; the
//! browser viewer has no machine to do that on.
//!
//! In runs with read and write records, a byte that differs, or a changed
//! line, names the step whose write set it, and a click goes there: this
//! run's playhead moves to the step, and the other run is shown in this
//! one's place at its step. The summary offers the same for the first
//! difference.
//!
//! The rows' text selects and copies as the file viewer's does: a drag
//! selects lines, Copy and Copy line copy them without their numbers, and
//! side by side a selection stays in the run's side it started in.

use std::ops::Range;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, HighlightStyle, MouseButton, Role,
    ScrollStrategy, SharedString, UniformListScrollHandle, WeakEntity, div, prelude::*, px,
    relative, rgb, uniform_list,
};

use crate::answers::FileKey;
use crate::describe::{clip, short_store_paths, thousands};
use crate::diff_view::{
    self, Cell, Column, Compared, Copy, Ends, Laid, Layout, Parted, Place, Row, Side, Whose,
    Writes, first_offset, other_step, unrecorded_note, write_note,
};
use crate::engine::{Cancel, EngineError, EngineResult, FileAtStep, Outputs};
use crate::request::Request;
use crate::run::Run;
use crate::selection::{Mapped, Pos, Surface, part_of_line};
use crate::sideways::{Sideways, line_width, text_column};
use crate::theme::{self, layout, size};
use crate::ui::bookmarks::link;
use crate::ui::icons::Icon;
use crate::ui::scrubber::{READING_A_FILE, Scrubber};
use crate::ui::selectable::{Registry, selectable, selects};
use crate::ui::sideways::{shifted, sideways_layer};
use crate::ui::tabs::RightTab;
use crate::ui::viewer::{explain, unavailable, unavailable_run};
use crate::ui::waiting::waiting_block;
use crate::ui::widgets::{icon, panel_title, tooltip};
use crate::viewer::{FetchedAll, expand_tabs, hex_spans};
use crate::waiting::Waiting;
use rewind_trace::file_diff::Kind;
use rewind_trace::writes::FileWrites;

/// How long the playhead must rest before the view reads both copies
/// again.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// The opacity of the last comparison while new copies load.
const STALE_OPACITY: f32 = 0.45;

/// The rows above the first difference that stay in view when the view
/// scrolls to it.
const CONTEXT_ROWS: usize = 3;

/// The process whose view of paths the engine reads a file with, as the
/// answers keep it: none, so each run reads the path as its job saw it.
const JOB_VIEW: u32 = 0;

/// How many characters of a file's name the tab shows.
pub const MAX_TAB_CHARS: usize = 24;

/// What the view says while it waits for both copies, and for the list.
const READING_COPIES: &str = "Rewind is bringing each run back to its step to read the file.";
const READING_OUTPUTS: &str =
    "Rewind is bringing each run to the step its job exited, to read the files it left behind.";

/// What the view says once Cancel stopped reading the copies, or the list.
const COPIES_STOPPED: &str = "Stopped before both copies were read. Move the playhead, or click the file again, to read them.";
const OUTPUTS_STOPPED: &str =
    "Stopped before the files were read. Click Files that differ to read them.";

/// What the view says of a copy whose path named a directory.
const IS_A_DIRECTORY: &str = "The path names a directory at this step, which has no bytes to compare. Click the directory in the Files panel to list its files.";

/// The names a replay's line goes by in a view of two runs.
const THIS_RUN: &str = "This run";
const OTHER_RUN: &str = "The other run";

/// What the view says under a comparison at the playhead that neither
/// run has the file at.
const MOVE_HINT: &str =
    "Move the playhead to a step after the file was written, or compare each run's end.";

/// The legend over a comparison one under the other.
const INTERLEAVED_LEGEND: &str = "Amber lines are this run's. Blue lines are the other run's.";

/// The summary's action that goes to the write that set the first byte
/// that differs.
const GO_TO_WRITE: &str = "Go to the write";

/// The columns of the rows, each a text that selects on its own.
const COLUMNS: [Column; 3] = [Column::This, Column::Other, Column::One];

/// The selection surfaces of the rows, one a column.
const ROW_SURFACES: [Surface; 3] = [
    Surface::FileDiff(Column::This),
    Surface::FileDiff(Column::Other),
    Surface::FileDiff(Column::One),
];

/// Which steps of the two runs the view compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffAt {
    /// This run at the playhead, the other at the step that matches it.
    Playhead,
    /// Each run at the step its job exited, or its last step.
    End,
}

/// One run's copy of the file, as far as the view has it.
#[derive(Clone, Debug)]
enum Read {
    Waiting,
    Answered(FileAtStep),
    Failed(String),
    /// Cancel stopped the read before the engine answered.
    Stopped,
}

/// The list of files that differ at the runs' ends.
#[derive(Clone, Debug)]
pub enum OutputList {
    Reading,
    Listed(Outputs),
    Failed(String),
    /// Cancel stopped the reading before the engine answered.
    Stopped,
}

/// The open view.
pub struct FileDiffPanel {
    /// The file compared; None while the view lists the files that
    /// differ.
    pub path: Option<String>,
    pub at: DiffAt,
    pub layout: Layout,
    /// The steps compared: this run's, then the other run's.
    steps: (u64, u64),
    /// Each run's copy: this run's, then the other run's.
    copies: (Read, Read),
    /// Why one of the runs' files cannot be read, when one cannot.
    unavailable: Option<&'static str>,
    /// The comparison on screen, and the steps it is of.
    shown: Option<(Compared, (u64, u64))>,
    /// The writes that made each run's copy on screen, this run's then
    /// the other's; None for a run without read and write records.
    writes: (Option<FileWrites>, Option<FileWrites>),
    loading: bool,
    /// The request whose answers this waits on; an answer to any other is
    /// dropped.
    request: Request,
    /// Stops the engine commands of the latest request while they run.
    in_flight: Vec<Cancel>,
    /// How far the engine's replays to the copies' steps have got.
    waiting: Waiting,
    /// The list of files that differ, once asked for.
    pub outputs: Option<OutputList>,
    outputs_request: Option<Request>,
    outputs_cancel: Option<Cancel>,
    /// How far the engine's replays to the runs' ends have got.
    outputs_waiting: Waiting,
    scroll: UniformListScrollHandle,
    sideways: Sideways,
}

impl FileDiffPanel {
    /// Stops the engine commands the latest request started, if they still
    /// run.
    fn supersede(&mut self) {
        for cancel in self.in_flight.drain(..) {
            cancel.cancel();
        }
    }

    /// Run `side`'s copy.
    fn copy_of(&mut self, side: Side) -> &mut Read {
        match side {
            Side::This => &mut self.copies.0,
            Side::Other => &mut self.copies.1,
        }
    }

    /// The writes that made run `side`'s copy on screen, when that run
    /// has read and write records.
    fn writes_of(&self, side: Side) -> Option<&FileWrites> {
        match side {
            Side::This => self.writes.0.as_ref(),
            Side::Other => self.writes.1.as_ref(),
        }
    }

    /// The tab's label: the file compared, or the list.
    pub fn label(&self) -> String {
        match &self.path {
            Some(path) => {
                let name = path.rsplit('/').next().unwrap_or(path);
                format!("Both runs \u{b7} {}", clip(name, MAX_TAB_CHARS))
            }
            None => "Files that differ".to_string(),
        }
    }
}

impl Drop for FileDiffPanel {
    fn drop(&mut self) {
        self.supersede();
        if let Some(cancel) = self.outputs_cancel.take() {
            cancel.cancel();
        }
    }
}

/// The step a run's job exited at, or its last step.
fn end_of(run: &Run) -> u64 {
    let timeline = &run.timeline;
    timeline.job_exit.map_or(timeline.total, |exit| exit.step)
}

/// Whether `run` recorded its writes to files.
fn writes_recorded(run: &Run) -> Writes {
    if run.io.records.is_empty() {
        return Writes::Unrecorded;
    }
    Writes::Recorded
}

/// The writes that made `path` in `run` as it was at `step`; None for a
/// run without read and write records.
fn file_writes(run: &Run, path: &str, step: u64) -> Option<FileWrites> {
    if writes_recorded(run) == Writes::Unrecorded {
        return None;
    }
    Some(FileWrites::of(&run.timeline.trace, &run.io, path, step))
}

impl Scrubber {
    /// Compares `path` in both runs at the playhead, as the Files panel
    /// does while two runs are open.
    pub(super) fn open_file_diff(&mut self, path: String, cx: &mut Context<Self>) {
        self.show_file_diff(Some(path), DiffAt::Playhead, cx);
    }

    /// Lists the files that differ at the runs' ends, as the Compare tab
    /// does.
    pub(super) fn open_outputs(&mut self, cx: &mut Context<Self>) {
        self.show_file_diff(None, DiffAt::End, cx);
    }

    /// Opens the view on `path`, or on the list when None, at `at`. The
    /// list, once read, stays for as long as the view is open.
    fn show_file_diff(&mut self, path: Option<String>, at: DiffAt, cx: &mut Context<Self>) {
        // The browser viewer has no machine to run the recordings on.
        if cfg!(target_family = "wasm") && self.session.is_some() {
            self.replay_in_desktop_app(READING_A_FILE, cx);
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let Some(other) = &session.other else {
            return;
        };
        let unavailable = unavailable(session).or_else(|| unavailable_run(other));

        // The layout and the list, and the wait for it, go on from a view
        // open before.
        let mut kept = self.shown.file_diff.take();
        let (layout, outputs, outputs_request, outputs_cancel, outputs_waiting) =
            match kept.as_mut() {
                Some(panel) => (
                    panel.layout,
                    panel.outputs.clone(),
                    panel.outputs_request.take(),
                    panel.outputs_cancel.take(),
                    std::mem::take(&mut panel.outputs_waiting),
                ),
                None => (Layout::SideBySide, None, None, None, Waiting::default()),
            };
        drop(kept);
        self.clear_selection_in(&ROW_SURFACES);

        // The list is read when it never was, or Cancel stopped the last
        // reading.
        let unread = matches!(outputs, None | Some(OutputList::Stopped));
        let reading_list = path.is_none() && unread && unavailable.is_none();
        self.right_tab = RightTab::FileDiff;
        self.shown.file_diff = Some(FileDiffPanel {
            path,
            at,
            layout,
            steps: (0, 0),
            copies: (Read::Waiting, Read::Waiting),
            unavailable,
            shown: None,
            writes: (None, None),
            loading: false,
            request: self.requests.issue(),
            in_flight: Vec::new(),
            waiting: Waiting::default(),
            outputs,
            outputs_request,
            outputs_cancel,
            outputs_waiting,
            scroll: UniformListScrollHandle::new(),
            sideways: Sideways::default(),
        });
        if reading_list {
            self.fetch_outputs(cx);
        }
        self.fetch_copies(cx);
        cx.notify();
    }

    /// The view, once the export on screen is in the engine's runs: a
    /// view that said the run was still being imported reads the list and
    /// both copies through the engine now.
    pub(super) fn file_diff_run_imported(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(session)) = (&mut self.shown.file_diff, &self.session) else {
            return;
        };
        let Some(other) = &session.other else {
            return;
        };
        panel.unavailable = unavailable(session).or_else(|| unavailable_run(other));

        // The list is read when it never was, as when the view opens.
        let unread = matches!(panel.outputs, None | Some(OutputList::Stopped));
        let reading_list = panel.path.is_none() && unread && panel.unavailable.is_none();
        if reading_list {
            self.fetch_outputs(cx);
        }
        self.fetch_copies(cx);
    }

    pub(super) fn close_file_diff(&mut self, cx: &mut Context<Self>) {
        self.shown.file_diff = None;
        self.clear_selection_in(&ROW_SURFACES);
        if self.right_tab == RightTab::FileDiff {
            self.right_tab = RightTab::AtStep;
        }
        cx.notify();
    }

    /// Goes to `step` of run `side`, where a write set a byte the view
    /// shows: this run's playhead moves there, and the other run is shown
    /// in this one's place, at the step, with the same file compared.
    fn jump_to_write(&mut self, side: Side, step: u64, cx: &mut Context<Self>) {
        let Some(panel) = &self.shown.file_diff else {
            return;
        };
        if side == Side::This {
            self.jump_to(step, cx);
            return;
        }

        // Showing the other run in this one's place closes the view, so
        // the view opens again on the same file, compared the same way.
        let (path, at) = (panel.path.clone(), panel.at);
        self.show_other_run(Some(step), cx);
        self.show_file_diff(path, at, cx);
    }

    /// Back from a file to the list of files that differ.
    fn back_to_outputs(&mut self, cx: &mut Context<Self>) {
        self.show_file_diff(None, DiffAt::End, cx);
    }

    /// Compares at the playhead, or at each run's end.
    fn set_diff_at(&mut self, at: DiffAt, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        if panel.at == at {
            return;
        }
        panel.at = at;
        self.fetch_copies(cx);
        cx.notify();
    }

    /// Lays the copies out side by side, or one under the other.
    fn set_diff_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        if panel.layout == layout {
            return;
        }
        panel.layout = layout;
        self.compare_copies(cx);
        cx.notify();
    }

    /// The steps the view compares now: this run's and the other run's.
    fn diff_steps(&self) -> Option<(u64, u64)> {
        let (panel, session) = (self.shown.file_diff.as_ref()?, self.session.as_ref()?);
        let other = session.other.as_ref()?;
        Some(match panel.at {
            DiffAt::Playhead => {
                let ends = Ends {
                    this: end_of(&session.run),
                    other: end_of(other),
                };
                let parted = session.comparison.as_ref().and_then(|c| {
                    c.point.as_ref().map(|p| Parted {
                        this: p.step,
                        other: p.other_step,
                    })
                });
                let matched = session.matching_step(self.step);
                (self.step, other_step(self.step, ends, parted, matched))
            }
            DiffAt::End => (end_of(&session.run), end_of(other)),
        })
    }

    /// Called whenever the playhead moves: a view at the playhead marks
    /// its comparison stale and reads both copies again once the playhead
    /// rests.
    pub(super) fn file_diff_playhead_moved(&mut self, cx: &mut Context<Self>) {
        let steps = self.diff_steps();
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        let follows = panel.at == DiffAt::Playhead && panel.path.is_some();
        if !follows || panel.unavailable.is_some() || Some(panel.steps) == steps {
            return;
        }
        panel.loading = true;
        panel.supersede();
        panel.request = self.requests.issue();
        let request = panel.request;
        let timer = cx.background_executor().timer(DEBOUNCE);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let still_latest = this
                    .shown
                    .file_diff
                    .as_ref()
                    .is_some_and(|p| p.request == request);
                if still_latest {
                    this.fetch_copies(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Asks the engine for both runs' copies at the view's steps, each on
    /// a thread of its own, unless an answer for the file's version there
    /// is kept.
    fn fetch_copies(&mut self, cx: &mut Context<Self>) {
        let steps = self.diff_steps();
        let (Some(panel), Some(session), Some(steps)) =
            (&mut self.shown.file_diff, &self.session, steps)
        else {
            return;
        };
        let (Some(path), None, Some(other)) =
            (panel.path.clone(), panel.unavailable, &session.other)
        else {
            return;
        };
        panel.supersede();
        panel.request = self.requests.issue();
        panel.steps = steps;
        panel.loading = true;
        panel.waiting = Waiting::default();
        let request = panel.request;

        // Each run's copy: a kept answer, nothing while its VM still boots,
        // or the engine's.
        let runs = [
            (Side::This, &session.run, steps.0),
            (Side::Other, other, steps.1),
        ];
        let mut asks = Vec::new();
        for (side, run, step) in runs {
            let timeline = &run.timeline;
            let key = FileKey::at(&run.path, &timeline.trace, &path, JOB_VIEW, step);
            let read = if step < timeline.job_start.unwrap_or(0) {
                Read::Answered(FileAtStep::Missing)
            } else if let Some(answer) = self.file_answers.get(&key) {
                Read::Answered(answer)
            } else {
                asks.push((side, run.path.clone(), step, key));
                Read::Waiting
            };
            *panel.copy_of(side) = read;
        }
        for (side, run, step, key) in asks {
            let cancel = Cancel::default();
            panel.in_flight.push(cancel.clone());
            let engine = self.engine.clone();
            let path = path.clone();

            // The engine's lines come through a channel as it says them,
            // and the channel closes when the engine is done.
            let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
            let task = crate::jobs::on_own_thread(move || {
                engine.cat(&run, step, None, &path, &cancel, &mut |line| {
                    let _ = lines.unbounded_send(line.to_string());
                })
            });
            cx.spawn(async move |this, cx| {
                // Each line of a replay's progress updates what the view
                // says while it still waits for this request.
                while let Some(line) = said.next().await {
                    let alive = this.update(cx, |this, cx| {
                        let Some(panel) = &mut this.shown.file_diff else {
                            return;
                        };
                        if panel.request == request && panel.waiting.said(&line) {
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }

                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.got_copy(request, side, key, result, cx);
                });
            })
            .detach();
        }
        self.compare_copies(cx);
    }

    /// Keeps the engine's answer for run `side`'s copy, and compares the
    /// copies once both are in.
    fn got_copy(
        &mut self,
        request: Request,
        side: Side,
        key: FileKey,
        result: EngineResult<FileAtStep>,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        if panel.request != request {
            return;
        }
        let read = match result {
            // A directory has no bytes to compare, and its answer is not
            // kept: a directory appears with no event in the trace.
            Ok(FileAtStep::Directory) => Read::Failed(IS_A_DIRECTORY.to_string()),
            Ok(answer) => {
                self.file_answers.insert(key, answer.clone());
                Read::Answered(answer)
            }
            Err(EngineError::Cancelled) => Read::Stopped,
            Err(e) => Read::Failed(explain(&e)),
        };
        *panel.copy_of(side) = read;
        self.compare_copies(cx);
    }

    /// Compares the two copies on a background thread, once both are in,
    /// and shows the comparison if the view still waits on the same ones.
    fn compare_copies(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        let Some(path) = panel.path.clone() else {
            return;
        };
        let copy = |read: &Read| -> Option<Option<(Vec<u8>, FetchedAll)>> {
            match read {
                Read::Answered(FileAtStep::Exists { bytes, complete }) => {
                    Some(Some((bytes.clone(), *complete)))
                }
                Read::Answered(FileAtStep::Missing) => Some(None),
                Read::Answered(FileAtStep::Directory)
                | Read::Waiting
                | Read::Failed(_)
                | Read::Stopped => None,
            }
        };
        let (Some(here), Some(there)) = (copy(&panel.copies.0), copy(&panel.copies.1)) else {
            // A copy that could not be read, or whose reading was stopped,
            // ends the wait: its reason shows.
            let ended = |read: &Read| {
                matches!(
                    read,
                    Read::Failed(_) | Read::Stopped | Read::Answered(FileAtStep::Directory)
                )
            };
            if ended(&panel.copies.0) || ended(&panel.copies.1) {
                panel.loading = false;
            }
            cx.notify();
            return;
        };
        let (request, steps, layout) = (panel.request, panel.steps, panel.layout);
        let task = cx.background_executor().spawn(async move {
            let name = path.rsplit('/').next().unwrap_or(&path);
            diff_view::compare_copies(name, as_copy(&here), as_copy(&there), layout)
        });
        cx.spawn(async move |this, cx| {
            let compared = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(panel) = &mut this.shown.file_diff else {
                    return;
                };
                if panel.request != request || panel.layout != layout {
                    return;
                }

                // Scrolled to the first difference, a few rows of what
                // both share above it.
                let first = compared.laid.as_ref().and_then(|l| l.first).unwrap_or(0);
                panel
                    .scroll
                    .scroll_to_item(first.saturating_sub(CONTEXT_ROWS), ScrollStrategy::Top);

                // The writes that made each copy, for naming the step that
                // wrote a byte.
                if let (Some(session), Some(path)) = (&this.session, &panel.path) {
                    let other = session.other.as_ref();
                    panel.writes = (
                        file_writes(&session.run, path, steps.0),
                        other.and_then(|o| file_writes(o, path, steps.1)),
                    );
                }
                panel.shown = Some((compared, steps));
                panel.loading = false;

                // A selection in the rows this comparison replaces is gone
                // with them.
                this.clear_selection_in(&ROW_SURFACES);
                cx.notify();
            });
        })
        .detach();
    }

    /// The rows of the comparison on screen, for the selection.
    pub(super) fn file_diff_rows(&self) -> Option<&[Row]> {
        match &self.shown.file_diff.as_ref()?.shown {
            Some((
                Compared {
                    laid: Some(laid), ..
                },
                _,
            )) => Some(&laid.rows),
            _ => None,
        }
    }

    /// Scrolls the view so row `row` is at its top.
    pub(super) fn scroll_file_diff_to(&self, row: usize) {
        if let Some(panel) = &self.shown.file_diff {
            panel.scroll.scroll_to_item(row, ScrollStrategy::Top);
        }
    }

    /// Stops reading both copies; the engine's answers, cancellations,
    /// then say so.
    fn cancel_copies(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = &self.shown.file_diff {
            for cancel in &panel.in_flight {
                cancel.cancel();
            }
        }
        cx.notify();
    }

    /// Stops reading the list of files that differ; the engine's answer, a
    /// cancellation, then says so.
    fn cancel_outputs(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self
            .shown
            .file_diff
            .as_ref()
            .and_then(|p| p.outputs_cancel.as_ref())
        {
            cancel.cancel();
        }
        cx.notify();
    }

    /// The name a replay of the run with id `run` goes by in this view:
    /// this run or the other one.
    fn run_label(&self, run: &str) -> Option<String> {
        let session = self.session.as_ref()?;
        let is = |r: Option<&Run>| r.and_then(Run::id).is_some_and(|id| id.as_str() == run);
        if is(Some(&session.run)) {
            return Some(THIS_RUN.to_string());
        }
        if is(session.other.as_ref()) {
            return Some(OTHER_RUN.to_string());
        }
        None
    }

    /// Asks the engine for the files that differ at the runs' ends.
    fn fetch_outputs(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(session)) = (&mut self.shown.file_diff, &self.session) else {
            return;
        };
        let Some(other) = &session.other else {
            return;
        };
        let request = self.requests.issue();
        let cancel = Cancel::default();
        panel.outputs = Some(OutputList::Reading);
        panel.outputs_request = Some(request);
        panel.outputs_cancel = Some(cancel.clone());
        panel.outputs_waiting = Waiting::default();
        let (this_run, other_run) = (session.run.path.clone(), other.path.clone());
        let engine = self.engine.clone();

        // The engine's lines come through a channel as it says them, and
        // the channel closes when the engine is done.
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let task = crate::jobs::on_own_thread(move || {
            engine.differing_outputs(&this_run, &other_run, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            // Each line of a replay's progress updates what the list says
            // while it still waits for this request.
            while let Some(line) = said.next().await {
                let alive = this.update(cx, |this, cx| {
                    let Some(panel) = &mut this.shown.file_diff else {
                        return;
                    };
                    if panel.outputs_request == Some(request) && panel.outputs_waiting.said(&line) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }

            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(panel) = &mut this.shown.file_diff else {
                    return;
                };
                if panel.outputs_request != Some(request) {
                    return;
                }
                panel.outputs_cancel = None;
                panel.outputs = Some(match result {
                    Ok(outputs) => OutputList::Listed(outputs),
                    Err(EngineError::Cancelled) => OutputList::Stopped,
                    Err(e) => OutputList::Failed(explain(&e)),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Scrolls the view's rows `pixels` sideways, in an area `width`
    /// pixels wide.
    fn scroll_diff_sideways(&mut self, pixels: f32, width: f32, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.shown.file_diff else {
            return;
        };
        let Some((
            Compared {
                laid: Some(laid), ..
            },
            _,
        )) = &panel.shown
        else {
            return;
        };
        let visible = cell_width(width, panel.layout, laid);
        panel
            .sideways
            .scroll_by(pixels, line_width(laid.widest), visible);
        cx.notify();
    }

    /// The view in place of the right column.
    pub(super) fn render_file_diff(&self, cx: &mut Context<Self>) -> Option<Div> {
        let panel = self.shown.file_diff.as_ref()?;
        let session = self.session.as_ref()?;
        let other = session.other.as_ref()?;
        let mono = self.fonts.mono.clone();

        // The title bar: what the view shows, and the close button.
        let close = div()
            .id("file-diff-close")
            .role(Role::Button)
            .aria_label("Close the comparison")
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
            .on_click(cx.listener(|this, _, _, cx| this.close_file_diff(cx)));
        let title = match &panel.path {
            Some(_) => "File in both runs",
            None => "Files that differ",
        };
        let title = panel_title(title, Some(close.into_any_element()));

        let body = match &panel.path {
            None => self.render_outputs(panel, cx),
            Some(path) => self.render_copies(panel, path, other, cx),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_basis(relative(0.0))
                .bg(rgb(theme::PANEL))
                .font_family(self.fonts.ui.clone())
                .child(title)
                .child(body.font_family(mono)),
        )
    }

    /// The list of files that differ at the runs' ends.
    fn render_outputs(&self, panel: &FileDiffPanel, cx: &mut Context<Self>) -> Div {
        let column = div()
            .id("file-diff-outputs")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_y_scroll()
            .px(px(size::PANEL_PAD_X))
            .py(px(size::LIST_PAD_Y))
            .gap(px(size::CARD_GAP))
            .text_size(px(size::TEXT_MONO));
        let message = |text: String, color: u32| {
            div()
                .font_family(self.fonts.ui.clone())
                .text_size(px(size::TEXT_UI))
                .text_color(rgb(color))
                .child(text)
        };
        if let Some(reason) = panel.unavailable {
            return div()
                .flex()
                .flex_col()
                .child(column.child(message(reason.to_string(), theme::SOFT)));
        }
        let listed = match &panel.outputs {
            None | Some(OutputList::Reading) => {
                let rows = panel.outputs_waiting.rows(|run| self.run_label(run));
                let waiting = waiting_block(
                    "file-diff-outputs-cancel",
                    Some(READING_OUTPUTS.to_string()),
                    rows,
                    cx.listener(|this, _, _, cx| this.cancel_outputs(cx)),
                );
                return div()
                    .flex()
                    .flex_col()
                    .child(column.child(waiting.font_family(self.fonts.ui.clone())));
            }
            Some(OutputList::Stopped) => {
                return div()
                    .flex()
                    .flex_col()
                    .child(column.child(message(OUTPUTS_STOPPED.to_string(), theme::SOFT)));
            }
            Some(OutputList::Failed(message_text)) => {
                return div()
                    .flex()
                    .flex_col()
                    .child(column.child(message(message_text.clone(), theme::RED_SOFT)));
            }
            Some(OutputList::Listed(outputs)) => outputs,
        };

        // How many differ, then a row for each: the path, and how it
        // differs under it.
        let total = listed.differ.len() + listed.same;
        let count = match listed.differ.len() {
            0 => format!("None of the {total} files the runs left behind differ."),
            n => format!("{n} of the {total} files the runs left behind differ."),
        };
        let mut column = column.child(message(count, theme::TEXT)).child(message(
            "Each run's files as they were when its job exited.".to_string(),
            theme::MUTED,
        ));
        for (i, output) in listed.differ.iter().enumerate() {
            let path = output.path.clone();
            column = column.child(
                div()
                    .id(SharedString::from(format!("file-diff-output-{i}")))
                    .role(Role::Button)
                    .aria_label(format!("Compare {} in both runs", output.path))
                    .flex()
                    .flex_col()
                    .py(px(size::CARD_GAP / 2.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_file_diff(Some(path.clone()), DiffAt::End, cx)
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .text_color(rgb(theme::AMBER))
                            .child(short_store_paths(&output.path)),
                    )
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(output_note(output)),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .child(column)
    }

    /// One file's two copies: the path, the summary, the choices, then
    /// the rows.
    fn render_copies(
        &self,
        panel: &FileDiffPanel,
        path: &str,
        other: &Run,
        cx: &mut Context<Self>,
    ) -> Div {
        // The choices: which steps, and how the copies are laid out.
        let at = panel.at;
        let chosen_layout = panel.layout;
        let choice = |id: &'static str, text: &'static str, chosen: bool| {
            div()
                .id(id)
                .role(Role::Button)
                .cursor_pointer()
                .text_color(rgb(if chosen { theme::TEXT } else { theme::MUTED }))
                .when(chosen, |d| d.border_b_1().border_color(rgb(theme::AMBER)))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .child(text)
        };
        let mut choices = div()
            .flex()
            .flex_wrap()
            .gap(px(size::LIST_COLUMN_GAP))
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_SMALL))
            .child(
                choice(
                    "file-diff-at-playhead",
                    "At the playhead",
                    at == DiffAt::Playhead,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_diff_at(DiffAt::Playhead, cx))),
            )
            .child(
                choice("file-diff-at-end", "At each run's end", at == DiffAt::End)
                    .on_click(cx.listener(|this, _, _, cx| this.set_diff_at(DiffAt::End, cx))),
            )
            .child(div().text_color(rgb(theme::FAINT)).child("\u{b7}"))
            .child(
                choice(
                    "file-diff-side-by-side",
                    "Side by side",
                    chosen_layout == Layout::SideBySide,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_diff_layout(Layout::SideBySide, cx)),
                ),
            )
            .child(
                choice(
                    "file-diff-interleaved",
                    "One under the other",
                    chosen_layout == Layout::Interleaved,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_diff_layout(Layout::Interleaved, cx)),
                ),
            )
            .child(div().text_color(rgb(theme::FAINT)).child("\u{b7}"))
            .child(
                link("file-diff-outputs-link", "Files that differ")
                    .on_click(cx.listener(|this, _, _, cx| this.back_to_outputs(cx))),
            );
        let alone_path = path.to_string();
        choices = choices.child(link("file-diff-alone", "This run's copy alone").on_click(
            cx.listener(move |this, _, _, cx| {
                let pid = this.session.as_ref().map_or(0, |s| {
                    s.run
                        .timeline
                        .files
                        .iter()
                        .rev()
                        .find(|f| f.path == alone_path)
                        .map_or(0, |f| f.pid)
                });
                this.open_file(alone_path.clone(), pid, cx)
            }),
        ));

        // The summary, one fact a line, or why there is none yet.
        let mut status: Vec<(String, u32)> = Vec::new();
        if let Some(reason) = panel.unavailable {
            status.push((reason.to_string(), theme::SOFT));
        }
        for read in [&panel.copies.0, &panel.copies.1] {
            if let Read::Failed(message) = read {
                status.push((message.clone(), theme::RED_SOFT));
            }
        }
        let stopped = [&panel.copies.0, &panel.copies.1]
            .iter()
            .any(|read| matches!(read, Read::Stopped));
        if stopped {
            status.push((COPIES_STOPPED.to_string(), theme::SOFT));
        }
        let shown = panel.shown.as_ref();
        let mut summary_line = None;
        match shown {
            Some((compared, _)) if status.is_empty() => {
                summary_line = Some(status.len());
                for (i, line) in compared.summary.iter().enumerate() {
                    let color = if i == 0 { theme::TEXT } else { theme::SOFT };
                    status.push((line.clone(), color));
                }
                if compared.laid.is_none() && at == DiffAt::Playhead {
                    status.push((MOVE_HINT.to_string(), theme::MUTED));
                }
            }
            None if status.is_empty() => status.push((READING_COPIES.to_string(), theme::MUTED)),
            _ => {}
        }

        // While the copies load, a long replay's lines, bars and Cancel
        // button.
        let rows = panel.waiting.rows(|run| self.run_label(run));
        let waiting = (panel.loading && !rows.is_empty()).then(|| {
            waiting_block(
                "file-diff-copies-cancel",
                None,
                rows,
                cx.listener(|this, _, _, cx| this.cancel_copies(cx)),
            )
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_SMALL))
        });

        // What the view can say of the writes that set the bytes: a line
        // when a run did not record them, and the way to the write that
        // set the first byte that differs.
        let mut first_write = None;
        if let Some((
            Compared {
                laid: Some(laid), ..
            },
            _,
        )) = shown
        {
            let recorded = (writes_recorded(&self.session().run), writes_recorded(other));
            if let Some(note) = unrecorded_note(recorded.0, recorded.1) {
                status.push((note.to_string(), theme::MUTED));
            }
            first_write = first_write_of(panel, laid);
        }
        let mut heading = div()
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP / 2.0))
            .px(px(size::PANEL_PAD_X))
            .py(px(size::LIST_PAD_Y))
            .border_b_1()
            .border_color(rgb(theme::LINE_SOFT))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .text_size(px(size::TEXT_MONO))
                    .text_color(rgb(theme::TEXT))
                    .child(short_store_paths(path)),
            );
        for (i, (text, color)) in status.into_iter().enumerate() {
            let line = div()
                .font_family(self.fonts.ui.clone())
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(color))
                .child(text);

            // The first difference's line, with the way to its write.
            let Some((side, step, note)) = first_write.clone().filter(|_| Some(i) == summary_line)
            else {
                heading = heading.child(line);
                continue;
            };
            let go = link("file-diff-go-to-write", GO_TO_WRITE)
                .font_family(self.fonts.ui.clone())
                .text_size(px(size::TEXT_SMALL))
                .role(Role::Button)
                .tooltip(tooltip(note))
                .on_click(cx.listener(move |this, _, _, cx| this.jump_to_write(side, step, cx)));
            heading = heading.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(size::LIST_COLUMN_GAP))
                    .child(line)
                    .child(go),
            );
        }
        if let Some(waiting) = waiting {
            heading = heading.child(waiting);
        }
        heading = heading.child(choices);

        // Which run each side or color is, and the steps compared.
        let steps = shown.map_or(panel.steps, |(_, steps)| *steps);
        let this_title = format!("This run \u{b7} step {}", thousands(steps.0));
        let other_title = format!("{} \u{b7} step {}", other.label(), thousands(steps.1));
        let columns = match panel.layout {
            Layout::SideBySide => div()
                .flex()
                .px(px(size::PANEL_PAD_X / 2.0))
                .py(px(size::CARD_GAP / 2.0))
                .font_family(self.fonts.ui.clone())
                .text_size(px(size::TEXT_SMALL))
                .child(column_title(this_title, theme::AMBER))
                .child(column_title(other_title, theme::BLUE)),
            Layout::Interleaved => div()
                .flex()
                .flex_col()
                .px(px(size::PANEL_PAD_X))
                .py(px(size::CARD_GAP / 2.0))
                .font_family(self.fonts.ui.clone())
                .text_size(px(size::TEXT_SMALL))
                .child(
                    div()
                        .flex()
                        .gap(px(size::LIST_COLUMN_GAP))
                        .child(div().text_color(rgb(theme::AMBER)).child(this_title))
                        .child(div().text_color(rgb(theme::BLUE)).child(other_title)),
                )
                .child(
                    div()
                        .text_color(rgb(theme::MUTED))
                        .child(INTERLEAVED_LEGEND),
                ),
        };

        // The rows, dimmed while newer copies load.
        let rows = match shown {
            Some((
                Compared {
                    laid: Some(laid), ..
                },
                _,
            )) if panel.unavailable.is_none() => Some(self.render_rows(panel, laid, cx)),
            _ => None,
        };
        let dim = if panel.loading && shown.is_some() {
            STALE_OPACITY
        } else {
            1.0
        };
        let has_rows = rows.is_some();
        div()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .child(heading)
            .when(has_rows, |d| d.child(columns))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_grow(layout::FILL)
                    .min_h_0()
                    .opacity(dim)
                    .cursor(CursorStyle::IBeam)
                    .text_size(px(size::TEXT_MONO))
                    .children(rows)
                    .when(has_rows, |area| {
                        area.child(sideways_layer(Self::scroll_diff_sideways, cx))
                    }),
            )
    }

    /// The rows of a comparison.
    fn render_rows(
        &self,
        panel: &FileDiffPanel,
        laid: &Laid,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let width = f32::from(panel.scroll.0.borrow().base_handle.bounds().size.width);
        let offset = panel.sideways.offset(
            line_width(laid.widest),
            cell_width(width, panel.layout, laid),
        );
        let digits = gutter_digits(laid);
        let scrubber = cx.entity().downgrade();

        // The selection, in whichever column it is.
        let registry = self.selecting.registry.clone();
        let selected = COLUMNS.into_iter().find_map(|column| {
            let range = self.selected_range(Surface::FileDiff(column))?;
            Some((column, range))
        });
        uniform_list(
            "file-diff-rows",
            laid.rows.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let Some(
                    panel @ FileDiffPanel {
                        shown:
                            Some((
                                Compared {
                                    laid: Some(laid), ..
                                },
                                _,
                            )),
                        ..
                    },
                ) = this.shown.file_diff.as_ref()
                else {
                    return Vec::new();
                };

                // Each cell, with where its differing parts lead and what of
                // it is selected.
                let half = |i: usize, column: Column, cx: &mut Context<Scrubber>| {
                    let cell = laid.rows.get(i).and_then(|row| row.cell(column));
                    let jumps = cell.and_then(|c| jumps_of(panel, c, &scrubber));
                    let selecting = Selecting {
                        column,
                        range: selected
                            .as_ref()
                            .filter(|(at, _)| *at == column)
                            .map(|(_, range)| range),
                        registry: &registry,
                    };
                    half(i, cell, digits, offset, jumps, selecting, cx)
                };
                range
                    .filter_map(|i| {
                        let row = laid.rows.get(i)?;
                        let line = div()
                            .id(i)
                            .w_full()
                            .h(px(size::LOG_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .whitespace_nowrap();
                        Some(match row {
                            Row::Pair(..) => line
                                .px(px(size::PANEL_PAD_X / 2.0))
                                .gap(px(size::CARD_GAP))
                                .child(half(i, Column::This, cx))
                                .child(div().w(px(1.0)).h_full().bg(rgb(theme::LINE_SOFT)))
                                .child(half(i, Column::Other, cx)),
                            Row::One(_) => {
                                line.px(px(size::PANEL_PAD_X / 2.0))
                                    .child(half(i, Column::One, cx))
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&panel.scroll)
        .flex_grow(layout::FILL)
        .min_h_0()
        .py(px(size::LIST_PAD_Y))
        .into_any_element()
    }
}

/// A copy the engine read, as the comparison takes it; None for a run
/// without the file.
fn as_copy(copy: &Option<(Vec<u8>, FetchedAll)>) -> Option<Copy<'_>> {
    copy.as_ref().map(|(bytes, complete)| Copy {
        bytes,
        complete: *complete,
    })
}

/// The title over one run's side.
fn column_title(text: String, color: u32) -> Div {
    div()
        .flex_basis(relative(0.0))
        .flex_grow(1.0)
        .min_w_0()
        .px(px(size::PANEL_PAD_X / 2.0))
        .truncate()
        .text_color(rgb(color))
        .child(text)
}

/// The way to the write that set the first byte that differs in either
/// run's copy, this run's first: the run, the write's step, and the note
/// on it. None when no copy that differs has a recorded write there.
fn first_write_of(panel: &FileDiffPanel, laid: &Laid) -> Option<(Side, u64, String)> {
    [Side::This, Side::Other].into_iter().find_map(|side| {
        let offset = first_offset(laid, side)?;
        let write = panel.writes_of(side)?.byte(offset as u64)?;
        let note = write_note(side, Place::Byte { offset }, Some(write));
        Some((side, write.step, note))
    })
}

/// Where a cell's differing parts lead: for each, its range in the shown
/// text, the note on the write that set the byte it stands for, and that
/// write's step when the records name one.
struct Jumps {
    side: Side,
    parts: Vec<(Range<usize>, SharedString, Option<u64>)>,
    scrubber: WeakEntity<Scrubber>,
}

/// Where `cell`'s differing parts lead, when its run has read and write
/// records; None for a cell both copies share, or a run without them.
fn jumps_of(panel: &FileDiffPanel, cell: &Cell, scrubber: &WeakEntity<Scrubber>) -> Option<Jumps> {
    let side = match cell.whose {
        Whose::Both => return None,
        Whose::This => Side::This,
        Whose::Other => Side::Other,
    };
    let writes = panel.writes_of(side)?;
    let mapped: Mapped = expand_tabs(&cell.text);
    let parts = cell
        .targets
        .iter()
        .map(|target| {
            let write = writes.byte(target.offset as u64);
            let note = write_note(side, Place::of(cell, target), write);
            let shown =
                mapped.shown_offset(target.range.start)..mapped.shown_offset(target.range.end);
            (shown, SharedString::from(note), write.map(|w| w.step))
        })
        .collect();
    Some(Jumps {
        side,
        parts,
        scrubber: scrubber.clone(),
    })
}

/// Which of a row's two cells an element is, for its id.
fn cell_id(row: usize, side: Side) -> SharedString {
    let side = match side {
        Side::This => "this",
        Side::Other => "other",
    };
    SharedString::from(format!("file-diff-cell-{row}-{side}"))
}

/// How a cell's text takes part in the selection: the column it is in,
/// the selected range when the selection is in that column, and the
/// registry its line is drawn into.
struct Selecting<'a> {
    column: Column,
    range: Option<&'a Range<Pos>>,
    registry: &'a Registry,
}

/// One cell of row `row`, or the empty place of a copy that has nothing
/// there: its number in the gutter, then its text moved `offset` pixels
/// left, tinted when it differs and with what differs marked in its run's
/// color. The text is line `row` of its column's selectable text, and a
/// press anywhere in the cell selects in that column. A cell with `jumps`
/// names the write behind each part that differs on hover, and goes to it
/// on a click that selects nothing: a hex dump byte by byte, a line as a
/// whole.
fn half(
    row: usize,
    cell: Option<&Cell>,
    digits: usize,
    offset: f32,
    jumps: Option<Jumps>,
    selecting: Selecting,
    cx: &mut Context<Scrubber>,
) -> AnyElement {
    let surface = Surface::FileDiff(selecting.column);
    let place = div()
        .flex()
        .items_center()
        .h_full()
        .flex_basis(relative(0.0))
        .flex_grow(1.0)
        .min_w_0()
        .px(px(size::PANEL_PAD_X / 2.0))
        .gap(px(size::LOG_COLUMN_GAP));
    let place = selects(place, surface, cx);
    let Some(cell) = cell else {
        return place.bg(rgb(theme::BG)).into_any_element();
    };
    let (tint, mark, mark_bg) = match cell.whose {
        Whose::Both => (None, theme::SOFT, None),
        Whose::This => (
            Some(theme::AMBER_CARD),
            theme::AMBER,
            Some(theme::AMBER_DEEP),
        ),
        Whose::Other => (
            Some(theme::BLUE_CARD),
            theme::BLUE,
            Some(theme::BLUE_BORDER),
        ),
    };
    let number = cell
        .number
        .map(|n| format!("{n:>digits$}"))
        .unwrap_or_default();
    let place = place
        .when_some(tint, |d, tint| d.bg(rgb(tint)))
        .when(digits > 0, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_color(rgb(theme::FAINT))
                    .child(number),
            )
        });
    let (shown, highlights) = styled(cell, mark, mark_bg);
    let part = selecting
        .range
        .and_then(|range| part_of_line(range, row, shown.len()));
    let text =
        selectable(surface, row, shown, part, selecting.registry).with_highlights(highlights);
    let Some(Jumps {
        side,
        parts,
        scrubber,
    }) = jumps
    else {
        return place
            .child(shifted(offset, text).text_color(rgb(theme::SOFT)))
            .into_any_element();
    };
    let id = cell_id(row, side);

    // A line: the whole cell names the write and goes to it.
    if cell.bytes.is_empty() {
        let Some((_, note, step)) = parts.into_iter().next() else {
            return place
                .child(shifted(offset, text).text_color(rgb(theme::SOFT)))
                .into_any_element();
        };
        return place
            .id(id)
            .tooltip(tooltip(note))
            .when_some(step, |d, step| {
                d.cursor_pointer().on_click(move |e, _, cx| {
                    let _ = scrubber.update(cx, |this, cx| {
                        if this.plain_click(e) {
                            this.jump_to_write(side, step, cx);
                        }
                    });
                })
            })
            .child(shifted(offset, text).text_color(rgb(theme::SOFT)))
            .into_any_element();
    }

    // A hex dump line: each differing byte's pair and character name its
    // write, and the ones with a recorded write go to it.
    let steps: Vec<(Range<usize>, u64)> = parts
        .iter()
        .filter_map(|(range, _, step)| step.map(|step| (range.clone(), step)))
        .collect();
    let notes: Vec<(Range<usize>, SharedString)> = parts
        .into_iter()
        .map(|(range, note, _)| (range, note))
        .collect();
    let clickable = steps.iter().map(|(range, _)| range.clone()).collect();
    let interactive = text.interactive(id, move |text| {
        text.on_click(clickable, move |i, _, cx| {
            let Some(&(_, step)) = steps.get(i) else {
                return;
            };

            // A press that ends a drag selecting text is not a click on
            // the byte.
            let _ = scrubber.update(cx, |this, cx| {
                if this.selected_text().is_none() {
                    this.jump_to_write(side, step, cx);
                }
            });
        })
        .tooltip(move |at, window, cx| {
            let (_, note) = notes.iter().find(|(range, _)| range.contains(&at))?;
            Some(tooltip(note.clone())(window, cx))
        })
    });
    place
        .child(shifted(offset, interactive).text_color(rgb(theme::SOFT)))
        .into_any_element()
}

/// A cell's text as shown, tabs drawn as spaces, and its highlights: a
/// hex dump's bytes in their classes' colors, and what differs in `mark`
/// over `mark_bg`.
fn styled(
    cell: &Cell,
    mark: u32,
    mark_bg: Option<u32>,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mapped: Mapped = expand_tabs(&cell.text);
    let marked_style = HighlightStyle {
        color: Some(rgb(mark).into()),
        background_color: mark_bg.map(|bg| rgb(bg).into()),
        font_weight: Some(FontWeight::BOLD),
        ..HighlightStyle::default()
    };
    let color = |c: u32| HighlightStyle {
        color: Some(rgb(c).into()),
        ..HighlightStyle::default()
    };

    // A hex dump's spans, each in its byte's color unless it differs;
    // a line of text has only the parts that differ.
    let parts: Vec<(Range<usize>, HighlightStyle)> = if cell.bytes.is_empty() {
        cell.marked
            .iter()
            .map(|r| (r.clone(), marked_style))
            .collect()
    } else {
        hex_spans(&cell.bytes)
            .into_iter()
            .map(|(range, c)| {
                let style = if cell.marked.contains(&range) {
                    marked_style
                } else {
                    color(c)
                };
                (range, style)
            })
            .collect()
    };

    // GPUI draws highlights in order along the line, and a hex dump's
    // spans go pair, character, pair, character.
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = parts
        .into_iter()
        .map(|(r, style)| {
            (
                mapped.shown_offset(r.start)..mapped.shown_offset(r.end),
                style,
            )
        })
        .filter(|(r, _)| !r.is_empty())
        .collect();
    highlights.sort_by_key(|(r, _)| r.start);
    (mapped.shown, highlights)
}

/// The digits of the line numbers in a gutter, or none for hex dumps.
fn gutter_digits(laid: &Laid) -> usize {
    if laid.kind == Kind::Binary {
        return 0;
    }
    let most = laid
        .rows
        .iter()
        .flat_map(|row| match row {
            Row::Pair(here, there) => [here.as_ref(), there.as_ref()],
            Row::One(cell) => [Some(cell), None],
        })
        .flatten()
        .filter_map(|cell| cell.number)
        .max()
        .unwrap_or(1);
    most.to_string().len()
}

/// How wide one cell's text is in a list `width` pixels wide.
fn cell_width(width: f32, layout: Layout, laid: &Laid) -> f32 {
    const HALVES: f32 = 2.0;
    let digits = gutter_digits(laid);
    match layout {
        Layout::SideBySide => text_column(width / HALVES, digits),
        Layout::Interleaved => text_column(width, digits),
    }
}

/// How a file in the list differs, under its path.
fn output_note(output: &crate::engine::DifferingOutput) -> String {
    match (output.left_size, output.right_size) {
        (Some(_), None) => format!("only in {}", diff_view::SIDES.a),
        (None, Some(_)) => format!("only in {}", diff_view::SIDES.b),
        (left, right) => format!(
            "{} of {} bytes differ, first at byte {}",
            thousands(output.differing_bytes as u64),
            thousands(left.max(right).unwrap_or(0) as u64),
            thousands(output.first_byte.unwrap_or(0) as u64)
        ),
    }
}
