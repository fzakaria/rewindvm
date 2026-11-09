//! The file viewer: a file's contents as they were at the playhead.
//!
//! Clicking a row of the files panel opens the file here, in place of
//! "At this step". Reading a file means the engine brings the run back to
//! the step and reads it inside the VM, which takes seconds, so while the
//! viewer is pinned to the playhead a move waits for the playhead to rest
//! before asking, and the last contents stay on screen, dimmed, until the
//! new ones arrive. Runs the engine cannot replay (the built-in examples,
//! runs opened from a .rwd file or a bare trace) say so instead.
//!
//! A path that names a directory, as a job's output often does, shows a
//! listing in place of contents. The files the run wrote under it show at
//! once, from the trace, in every run: each with the step of its last
//! write, which a click goes to, and a click on its path opens the file.
//! Where the engine can read the run's files, the VM's own listing of the
//! directory at the step then takes their place, with links, empty
//! directories, the input image's files and sizes, and the trace's steps
//! beside the files the run wrote (see [`crate::listing`]).

use std::time::Duration;

use futures::StreamExt;
use gpui::{
    ClickEvent, Context, CursorStyle, Div, MouseButton, Role, ScrollStrategy, SharedString,
    UniformListScrollHandle, div, prelude::*, px, relative, rgb, uniform_list,
};

use crate::answers::{DirKey, FileKey};
use crate::describe::{short_store_paths, thousands};
use crate::engine::{
    Cancel, EngineError, EngineResult, Expect, FileAtStep, Looked, PathAt, REPLAYS_ANOTHER_WAY,
    goes_another_way, look,
};
use crate::listing::{self, Entry, Source};
use crate::request::Request;
use crate::run::{Origin, Replays, Run, Session};
use crate::selection::{Mapped, Surface, part_of_line};
use crate::sideways::{Sideways, line_width, text_column};
use crate::syntax::{Highlighter, Language};
use crate::theme::{self, layout, size};
use crate::ui::bookmarks::link;
use crate::ui::icons::Icon;
use crate::ui::render::seek_on_click;
use crate::ui::scrubber::{READING_A_FILE, Scrubber};
use crate::ui::selectable::{colored, selectable, selects, viewer_line};
use crate::ui::sideways::{shifted, sideways_layer};
use crate::ui::tabs::RightTab;
use crate::ui::waiting::waiting_block;
use crate::ui::widgets::{icon, panel_title, tooltip};
use crate::viewer::{self, Kind, View, hex_spans};
use crate::waiting::Waiting;
use rewind_trace::{ListedAll, ListedKind, Listing};

/// How long the playhead must rest before a pinned viewer asks again.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// The opacity of the last contents while new ones load.
const STALE_OPACITY: f32 = 0.45;

/// Why the viewer cannot read files of some runs.
const EXAMPLE_REASON: &str = "This example was recorded on another machine and ships as its trace only. Reading a file means replaying the run up to the step, which needs the run's inputs and Rewind with KVM on this machine. Record a run of your own to read its files at any step.";
const EXPORT_TRACE_REASON: &str = "This run was opened from a .rwd file that holds its trace only. Open its replayable export, the -replayable.rwd file, to read its files at any step.";
const EXPORT_IMPORTING_REASON: &str = "This run is being imported into Rewind from its .rwd file. Its files can be read at any step once it is in.";
const TRACE_REASON: &str = "This run was opened from a bare trace file. Open its run directory instead to read its files at any step.";

/// What the viewer says while the engine brings the run back to the step.
pub(super) const READING: &str = "Rewind is bringing the run back to this step to read the file.";

/// What the viewer says once Cancel stopped a read.
const STOPPED: &str =
    "Stopped before the file was read. Move the playhead, or click the file again, to read it.";

/// What a listed file's step says on hover.
const LISTED_STEP_NOTE: &str = "The step this file was last written or renamed at. Click to go there; click the path to see the file at the playhead.";

/// What the viewer says under a listing the VM did not give, before the
/// engine's reason.
const UNLISTED: &str = "Only the files the run wrote are listed";

/// Whether the viewer follows the playhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    /// Reads the file again whenever the playhead comes to rest.
    FollowPlayhead,
    /// Keeps the contents of the step it was opened or last read at.
    Stay,
}

/// What the viewer last learned about the file.
#[derive(Clone, Debug)]
pub enum Fetched {
    Contents {
        step: u64,
        view: View,
    },
    Missing {
        step: u64,
    },
    Failed {
        step: u64,
        message: String,
    },
    /// The engine cannot read this run's files at all; not an error in
    /// the run or the app.
    Unreadable {
        step: u64,
        message: &'static str,
    },
    /// The step is before the job started, while the VM boots.
    Booting {
        step: u64,
        job_start: u64,
    },
    /// Cancel stopped the read before the engine answered.
    Stopped {
        step: u64,
    },
    /// The path names a directory: its entries at the step, as far as
    /// `source` knows them.
    Directory {
        step: u64,
        entries: Vec<Entry>,
        source: Source,
        /// How many of the trace's file events happened by the step. The
        /// trace's listing is the same at every step with as many.
        events: usize,
        /// Why the VM's listing is not shown, when the engine was asked
        /// for one and could not give it.
        unlisted: Option<String>,
    },
}

/// A directory's listing a file was opened from, to go back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedIn {
    pub path: String,
    /// The process the listing was opened with.
    pub pid: u32,
}

/// The open viewer.
pub struct FileViewer {
    pub path: String,
    /// The process that wrote the file, whose view of paths the engine
    /// reads it with.
    pub pid: u32,
    pub pin: Pin,
    /// The directory whose listing the file was opened from, if it was.
    pub listed_in: Option<ListedIn>,
    /// The step the viewer shows or is loading.
    pub step: u64,
    /// Why this run's files cannot be read, when they cannot.
    pub unavailable: Option<&'static str>,
    pub shown: Option<Fetched>,
    pub loading: bool,
    /// How far the engine's replay to the step has got, while loading.
    waiting: Waiting,
    /// The request whose answer this waits on; an answer to any other,
    /// from before the playhead moved or this was opened, is dropped.
    request: Request,
    /// Stops the engine command of the latest request while it runs.
    in_flight: Option<Cancel>,
    scroll: UniformListScrollHandle,
    /// The shown text's colors, as far as they have been drawn.
    highlighter: Highlighter,
    /// How far the lines are scrolled sideways; kept as the playhead
    /// moves, so a long line stays in view from step to step.
    sideways: Sideways,
}

impl FileViewer {
    /// Stops the engine command the latest request started, if it still
    /// runs: a newer request, or the viewer closing, leaves its answer
    /// with no one to read it.
    fn supersede(&mut self) {
        if let Some(cancel) = self.in_flight.take() {
            cancel.cancel();
        }
    }
}

impl Drop for FileViewer {
    fn drop(&mut self) {
        self.supersede();
    }
}

/// Why a session's files cannot be read, if they cannot.
pub(super) fn unavailable(session: &Session) -> Option<&'static str> {
    if session.replays == Replays::AnotherWay {
        return Some(REPLAYS_ANOTHER_WAY);
    }
    unavailable_run(&session.run)
}

/// Why a run's files cannot be read, by where it was opened from, if they
/// cannot.
pub(super) fn unavailable_run(run: &Run) -> Option<&'static str> {
    match run.origin {
        Origin::Example => Some(EXAMPLE_REASON),
        Origin::Export(ref export) if export.replayable => Some(EXPORT_IMPORTING_REASON),
        Origin::Export(_) => Some(EXPORT_TRACE_REASON),
        Origin::TraceFile => Some(TRACE_REASON),
        Origin::Local => None,
    }
}

impl Scrubber {
    /// Opens `path` in the viewer at the playhead.
    pub(super) fn open_file(&mut self, path: String, pid: u32, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };

        // The browser viewer has no machine to run the recording on. A
        // directory's files come from the trace, which needs none.
        let listed = session.run.timeline.has_files_under(&path);
        if cfg!(target_family = "wasm") && !listed {
            self.replay_in_desktop_app(READING_A_FILE, cx);
            return;
        }
        let unavailable = unavailable(session);
        self.right_tab = RightTab::File;
        self.viewer = Some(FileViewer {
            path,
            pid,
            pin: Pin::FollowPlayhead,
            listed_in: None,
            step: self.step,
            unavailable,
            shown: None,
            loading: false,
            waiting: Waiting::default(),
            request: self.requests.issue(),
            in_flight: None,
            scroll: UniformListScrollHandle::new(),
            highlighter: Highlighter::new(None),
            sideways: Sideways::default(),
        });
        self.clear_selection_in(&[Surface::Viewer, Surface::Source]);
        self.read_viewer(cx);
        cx.notify();
    }

    /// Opens `path`, a file of the listing the viewer shows, and keeps the
    /// way back to the listing.
    fn open_listed_file(&mut self, path: String, pid: u32, cx: &mut Context<Self>) {
        let listed_in = self.viewer.as_ref().map(|viewer| ListedIn {
            path: viewer.path.clone(),
            pid: viewer.pid,
        });
        self.open_file(path, pid, cx);
        if let Some(viewer) = &mut self.viewer {
            viewer.listed_in = listed_in;
        }
    }

    /// Shows what the viewer's path holds at the viewer's step: a
    /// directory's written files at once, from the trace, and then what
    /// the engine reads there, when the engine can read this run's files.
    fn read_viewer(&mut self, cx: &mut Context<Self>) {
        self.list_from_trace(cx);
        if !self.engine_reads() {
            return;
        }
        if let Some(viewer) = &mut self.viewer {
            viewer.loading = true;
        }
        self.fetch_file(cx);
    }

    /// Whether the engine can read the files of the run the viewer is
    /// open on. The browser viewer has no engine.
    fn engine_reads(&self) -> bool {
        let readable = self
            .viewer
            .as_ref()
            .is_some_and(|viewer| viewer.unavailable.is_none());
        readable && !cfg!(target_family = "wasm")
    }

    /// Shows the files the trace has under the viewer's path at the
    /// viewer's step, when the trace has a file under the path at some
    /// step, which makes the path a directory. Any other path is left as
    /// it is.
    fn list_from_trace(&mut self, cx: &mut Context<Self>) {
        let (Some(viewer), Some(session)) = (&mut self.viewer, &self.session) else {
            return;
        };
        let timeline = &session.run.timeline;
        if !timeline.has_files_under(&viewer.path) {
            return;
        }

        // A listing on screen stands while the playhead crosses no file
        // event: working the files out again walks every one of them. The
        // trace's own is the listing of the new step too. The VM's stays
        // as the listing of its step until the engine reads the new one.
        let events = timeline.file_count_at(viewer.step);
        if let Some(Fetched::Directory {
            step,
            source,
            events: listed,
            ..
        }) = &mut viewer.shown
            && *listed == events
        {
            if *source == Source::Trace {
                *step = viewer.step;
            }
            cx.notify();
            return;
        }
        viewer.highlighter = Highlighter::new(None);
        viewer.shown = Some(Fetched::Directory {
            step: viewer.step,
            entries: listing::from_trace(timeline.files_under(&viewer.path, viewer.step)),
            source: Source::Trace,
            events,
            unlisted: None,
        });
        self.selecting.selection = self
            .selecting
            .selection
            .filter(|s| s.surface != Surface::Viewer);
        cx.notify();
    }

    pub(super) fn close_viewer(&mut self, cx: &mut Context<Self>) {
        self.viewer = None;
        if self.right_tab == RightTab::File {
            self.right_tab = RightTab::AtStep;
        }
        self.clear_selection_in(&[Surface::Viewer]);
        cx.notify();
    }

    /// Stops the read the viewer waits on; the engine's answer, a
    /// cancellation, then says so.
    pub(super) fn cancel_viewer_read(&mut self, cx: &mut Context<Self>) {
        if let Some(viewer) = &mut self.viewer
            && let Some(cancel) = &viewer.in_flight
        {
            cancel.cancel();
        }
        cx.notify();
    }

    /// The contents on screen, when the viewer shows a file's contents.
    pub(super) fn viewer_view(&self) -> Option<&View> {
        match self.viewer.as_ref()?.shown.as_ref()? {
            Fetched::Contents { view, .. } => Some(view),
            _ => None,
        }
    }

    /// Scrolls the viewer so line `line` is at its top.
    pub(super) fn scroll_viewer_to(&self, line: usize) {
        if let Some(viewer) = &self.viewer {
            viewer.scroll.scroll_to_item(line, ScrollStrategy::Top);
        }
    }

    /// Scrolls the viewer's lines `pixels` sideways, in an area `width`
    /// pixels wide.
    fn scroll_viewer_sideways(&mut self, pixels: f32, width: f32, cx: &mut Context<Self>) {
        let Some(view) = self.viewer_view() else {
            return;
        };
        let widest = line_width(view.widest);
        let visible = text_column(width, gutter_digits(view));
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        viewer.sideways.scroll_by(pixels, widest, visible);
        cx.notify();
    }

    /// What the viewer shows in place of contents, as lines: a
    /// directory's entries, each by its path under the directory, or a
    /// message as one line.
    pub(super) fn viewer_message_lines(&self) -> Vec<Mapped> {
        let Some(viewer) = &self.viewer else {
            return Vec::new();
        };
        if let Some(entries) = listed_entries(viewer) {
            return entries
                .iter()
                .map(|entry| Mapped::plain(entry.shown(&viewer.path)))
                .collect();
        }
        viewer_message(viewer)
            .map(|(text, _)| vec![Mapped::plain(text)])
            .unwrap_or_default()
    }

    pub(super) fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        viewer.pin = match viewer.pin {
            Pin::FollowPlayhead => Pin::Stay,
            Pin::Stay => Pin::FollowPlayhead,
        };
        let follow = viewer.pin == Pin::FollowPlayhead && viewer.step != self.step;
        if follow {
            self.playhead_moved(cx);
        }
        cx.notify();
    }

    /// Called whenever the playhead moves: a pinned viewer marks its
    /// contents stale and reads the file again once the playhead rests.
    pub(super) fn playhead_moved(&mut self, cx: &mut Context<Self>) {
        let step = self.step;
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        if viewer.pin != Pin::FollowPlayhead || viewer.step == step {
            return;
        }
        viewer.step = step;

        // A directory's written files come from the trace, at once, in
        // every run.
        self.list_from_trace(cx);
        if !self.engine_reads() {
            return;
        }
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        viewer.loading = true;
        viewer.supersede();
        viewer.request = self.requests.issue();
        let request = viewer.request;

        // An answer the engine gave for the same version of the file, or
        // for the directory at this step, shows at once; anything else
        // waits for the playhead to rest.
        if let Some(answer) = self.known_answer() {
            self.show_looked(step, Ok(answer), cx);
            return;
        }
        let timer = cx.background_executor().timer(DEBOUNCE);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let still_latest = this.viewer.as_ref().is_some_and(|v| v.request == request);
                if still_latest {
                    this.fetch_file(cx);
                }
            });
        })
        .detach();
    }

    /// The answer kept for the viewer's path at the viewer's step: a
    /// directory's listing the engine gave for the step, or a file's
    /// answer for the version the file has there. None while the VM
    /// boots, which the viewer says instead.
    fn known_answer(&self) -> Option<Looked> {
        let (viewer, session) = (self.viewer.as_ref()?, self.session.as_ref()?);
        let timeline = &session.run.timeline;
        if viewer.step < timeline.job_start.unwrap_or(0) {
            return None;
        }
        let listed: DirKey = (
            session.run.path.clone(),
            viewer.path.clone(),
            viewer.pid,
            viewer.step,
        );
        if let Some(listing) = self.dir_answers.get(&listed) {
            return Some(Looked::Listed(listing));
        }
        let key = FileKey::at(
            &session.run.path,
            &timeline.trace,
            &viewer.path,
            viewer.pid,
            viewer.step,
        );
        self.file_answers.get(&key).map(Looked::File)
    }

    /// Asks the engine what the viewer's path holds at the viewer's step,
    /// on a background thread, unless an answer for it is kept.
    fn fetch_file(&mut self, cx: &mut Context<Self>) {
        let (Some(viewer), Some(session)) = (&mut self.viewer, &self.session) else {
            return;
        };
        viewer.supersede();
        viewer.request = self.requests.issue();

        // While the VM boots there are no files of the job to read yet.
        let job_start = session.run.timeline.job_start.unwrap_or(0);
        if viewer.step < job_start {
            viewer.loading = false;
            viewer.shown = Some(Fetched::Booting {
                step: viewer.step,
                job_start,
            });
            cx.notify();
            return;
        }
        let step = viewer.step;
        if let Some(answer) = self.known_answer() {
            self.show_looked(step, Ok(answer), cx);
            return;
        }

        // The engine's answer, kept by the file's version or the
        // directory's step.
        let (Some(viewer), Some(session)) = (&mut self.viewer, &self.session) else {
            return;
        };
        viewer.loading = true;
        viewer.waiting = Waiting::default();
        let request = viewer.request;
        let timeline = &session.run.timeline;
        let at = PathAt {
            run: session.run.path.clone(),
            step,
            pid: Some(viewer.pid),
            path: viewer.path.clone(),
        };
        let key = FileKey::at(&at.run, &timeline.trace, &at.path, viewer.pid, step);
        let listed: DirKey = (at.run.clone(), at.path.clone(), viewer.pid, step);

        // A path no event writes is most often a directory: a job's
        // output, or one of the input image's. Every file the run made
        // has an event that wrote it.
        let expect = if timeline.never_written(&at.path) {
            Expect::Directory
        } else {
            Expect::File
        };
        let engine = self.engine.clone();
        let cancel = Cancel::default();
        viewer.in_flight = Some(cancel.clone());

        // The engine's lines come through a channel as it says them, and
        // the channel closes when the engine is done.
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let task = crate::jobs::on_own_thread(move || {
            look(engine.as_ref(), &at, expect, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            // Each line of the replay's progress updates what the viewer
            // says while it still waits for this request.
            while let Some(line) = said.next().await {
                let alive = this.update(cx, |this, cx| {
                    let Some(viewer) = &mut this.viewer else {
                        return;
                    };
                    if viewer.request == request && viewer.waiting.said(&line) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }

            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.viewer.as_ref().is_none_or(|v| v.request != request) {
                    return;
                }
                match &result {
                    Ok(Looked::Listed(listing)) => {
                        this.dir_answers.insert(listed, listing.clone());
                    }

                    // A path no event writes that is missing, or a
                    // directory, at one step may be anything at another:
                    // a directory appears with no event. A file's bytes
                    // there change only with an event.
                    Ok(Looked::File(answer))
                        if expect == Expect::File
                            || matches!(answer, FileAtStep::Exists { .. }) =>
                    {
                        this.file_answers.insert(key, answer.clone());
                    }
                    _ => {}
                }
                this.show_looked(step, result, cx);
            });
        })
        .detach();
    }

    /// Shows what the engine found at the viewer's path at `step`.
    fn show_looked(&mut self, step: u64, result: EngineResult<Looked>, cx: &mut Context<Self>) {
        match result {
            Ok(Looked::File(FileAtStep::Directory)) => self.show_directory(step, None, None, cx),
            Ok(Looked::File(answer)) => self.show_file(step, Ok(answer), cx),
            Ok(Looked::Listed(listing)) => self.show_directory(step, Some(listing), None, cx),
            Ok(Looked::Unlisted(why)) => self.show_directory(step, None, Some(why), cx),
            Err(e) => self.show_file(step, Err(e), cx),
        }
    }

    /// Shows the directory at the viewer's path at `step`: the VM's
    /// `listing` with the trace's steps, or without one the files the
    /// trace has, and why the VM gave none when the engine said.
    fn show_directory(
        &mut self,
        step: u64,
        listing: Option<Listing>,
        unlisted: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.selecting.selection = self
            .selecting
            .selection
            .filter(|s| s.surface != Surface::Viewer);
        let (Some(viewer), Some(session)) = (&mut self.viewer, &self.session) else {
            return;
        };
        viewer.loading = false;
        viewer.in_flight = None;
        viewer.highlighter = Highlighter::new(None);

        let timeline = &session.run.timeline;
        let written = timeline.files_under(&viewer.path, step);
        let (entries, source) = match &listing {
            Some(listing) => (
                listing::merged(&viewer.path, listing, &written, viewer.pid),
                Source::Vm(listing.complete),
            ),
            None => (listing::from_trace(written), Source::Trace),
        };
        viewer.shown = Some(Fetched::Directory {
            step,
            entries,
            source,
            events: timeline.file_count_at(step),
            unlisted,
        });
        cx.notify();
    }

    /// Shows the engine's answer for the file at `step`.
    fn show_file(&mut self, step: u64, result: EngineResult<FileAtStep>, cx: &mut Context<Self>) {
        self.selecting.selection = self
            .selecting
            .selection
            .filter(|s| s.surface != Surface::Viewer);
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        viewer.loading = false;
        viewer.in_flight = None;

        // A run this build replays another way is so at every step, so the
        // viewer stops asking, and so does everything else that brings the
        // run to a step.
        if result.as_ref().is_err_and(goes_another_way) {
            viewer.unavailable = Some(REPLAYS_ANOTHER_WAY);
            if let Some(session) = &mut self.session {
                session.replays = Replays::AnotherWay;
            }
        }

        // The trace's listing of a directory stands when the engine could
        // not look: with the engine's reason under it, unless the look
        // was stopped or the run does not replay, which the viewer says
        // elsewhere.
        if let Err(e) = &result
            && let Some(Fetched::Directory {
                step: listed,
                source: Source::Trace,
                unlisted,
                ..
            }) = &mut viewer.shown
            && *listed == step
        {
            let says_elsewhere = matches!(e, EngineError::Cancelled) || goes_another_way(e);
            if !says_elsewhere {
                *unlisted = Some(explain(e));
            }
            cx.notify();
            return;
        }

        // Colors start over with the new contents; only text in a known
        // language has any.
        viewer.highlighter = Highlighter::new(None);
        viewer.shown = Some(match result {
            Ok(FileAtStep::Exists { bytes, complete }) => {
                let view = View::new(&bytes, complete);
                viewer.highlighter = Highlighter::new(language_of(&viewer.path, &view));
                Fetched::Contents { step, view }
            }
            Ok(FileAtStep::Missing) => Fetched::Missing { step },

            // A directory nothing listed: the trace has no file under it.
            Ok(FileAtStep::Directory) => Fetched::Directory {
                step,
                entries: Vec::new(),
                source: Source::Trace,
                events: 0,
                unlisted: None,
            },
            Err(e) if goes_another_way(&e) => Fetched::Unreadable {
                step,
                message: REPLAYS_ANOTHER_WAY,
            },
            Err(EngineError::Cancelled) => Fetched::Stopped { step },
            Err(e) => Fetched::Failed {
                step,
                message: explain(&e),
            },
        });
        cx.notify();
    }

    /// The viewer in place of the right column.
    pub(super) fn render_viewer(&self, cx: &mut Context<Self>) -> Option<Div> {
        let viewer = self.viewer.as_ref()?;
        let mono = self.fonts.mono.clone();

        // The title bar: the step shown, the pin and the close button.
        let shown_step = match &viewer.shown {
            Some(Fetched::Contents { step, .. })
            | Some(Fetched::Missing { step })
            | Some(Fetched::Failed { step, .. })
            | Some(Fetched::Unreadable { step, .. })
            | Some(Fetched::Booting { step, .. })
            | Some(Fetched::Stopped { step })
            | Some(Fetched::Directory { step, .. }) => *step,
            None => viewer.step,
        };
        let what = match &viewer.shown {
            Some(Fetched::Directory { .. }) => "Directory",
            _ => "File",
        };
        let pin_label = match viewer.pin {
            Pin::FollowPlayhead => "following the playhead",
            Pin::Stay => "pinned to this step",
        };
        let controls = div()
            .flex()
            .items_center()
            .gap(px(size::LIST_COLUMN_GAP))
            .child(
                div()
                    .id("viewer-pin")
                    .role(Role::Button)
                    .aria_label("Follow the playhead or stay at this step")
                    .cursor_pointer()
                    .text_color(rgb(match viewer.pin {
                        Pin::FollowPlayhead => theme::AMBER,
                        Pin::Stay => theme::MUTED,
                    }))
                    .hover(|s| s.text_color(rgb(theme::TEXT)))
                    .child(pin_label)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_pin(cx))),
            )
            .child(
                div()
                    .id("viewer-close")
                    .role(Role::Button)
                    .aria_label("Close the file")
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
                    .on_click(cx.listener(|this, _, _, cx| this.close_viewer(cx))),
            );
        let title = panel_title(
            &format!("{what} \u{b7} as of step {}", thousands(shown_step)),
            Some(controls.into_any_element()),
        );

        // The path and what the viewer knows about the file.
        let status = match (&viewer.shown, viewer.unavailable) {
            (
                Some(Fetched::Directory {
                    step,
                    entries,
                    source,
                    ..
                }),
                _,
            ) => describe_listing(entries.len(), *source, *step),
            (_, Some(_)) => "not available for this run".to_string(),
            (None, None) => format!("reading step {}\u{2026}", thousands(viewer.step)),
            (Some(Fetched::Contents { view, .. }), None) => describe_view(view),
            (Some(Fetched::Missing { step }), None) => {
                format!("did not exist at step {}", thousands(*step))
            }
            (Some(Fetched::Failed { .. }), None) => "could not be read".to_string(),
            (Some(Fetched::Unreadable { .. }), None) => "not available for this run".to_string(),
            (Some(Fetched::Booting { .. }), None) => "the VM is still booting".to_string(),
            (Some(Fetched::Stopped { .. }), None) => "not read".to_string(),
        };
        let status = if viewer.loading && viewer.shown.is_some() {
            let left = viewer.waiting.left().map_or(String::new(), |left| {
                format!(", {} left", rewind_trace::replaying::about(left))
            });
            format!(
                "{status} \u{b7} reading step {}\u{2026}{left}",
                thousands(viewer.step)
            )
        } else {
            status
        };
        let heading = div()
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP / 2.0))
            .px(px(size::PANEL_PAD_X))
            .py(px(size::LIST_PAD_Y))
            .border_b_1()
            .border_color(rgb(theme::LINE_SOFT))
            .font_family(mono.clone())
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .text_size(px(size::TEXT_MONO))
                    .text_color(rgb(theme::TEXT))
                    .child(short_store_paths(&viewer.path)),
            )
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(status),
            );

        // Why a directory's listing is the trace's alone, when the engine
        // was asked for the VM's and could not give it.
        let heading = match &viewer.shown {
            Some(Fetched::Directory {
                unlisted: Some(why),
                ..
            }) => heading.child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(format!("{UNLISTED}: {why}")),
            ),
            _ => heading,
        };

        // A file opened from a directory's listing has the way back.
        let heading = match viewer.listed_in.clone() {
            Some(listing) => {
                let name = short_store_paths(&listing.path);
                heading.child(div().flex().text_size(px(size::TEXT_SMALL)).child(
                    link("viewer-listing", format!("Back to {name}")).on_click(cx.listener(
                        move |this, _, _, cx| this.open_file(listing.path.clone(), listing.pid, cx),
                    )),
                ))
            }
            None => heading,
        };

        // While older contents stay on screen, a long replay's line, bar
        // and Cancel button go under the status.
        let rows = viewer.waiting.rows(|_| None);
        let heading = if viewer.loading && viewer.shown.is_some() && !rows.is_empty() {
            heading.child(waiting_block(
                "viewer-cancel",
                None,
                rows,
                cx.listener(|this, _, _, cx| this.cancel_viewer_read(cx)),
            ))
        } else {
            heading
        };

        // The body: the contents, or a message in their place, both
        // selectable; while the first read waits, what the replay says.
        let registry = self.selecting.registry.clone();
        let selected = self.selected_range(Surface::Viewer);
        let body = match (&viewer.shown, viewer.unavailable, viewer_message(viewer)) {
            (Some(Fetched::Contents { view, .. }), None, _) => {
                let lines = view.lines.len();
                let numbered = view.kind == Kind::Text;
                let digits = gutter_digits(view);

                // The sideways offset, kept within what the list's width,
                // as of the last frame, allows.
                let width = f32::from(viewer.scroll.0.borrow().base_handle.bounds().size.width);
                let offset = viewer
                    .sideways
                    .offset(line_width(view.widest), text_column(width, digits));
                uniform_list(
                    "viewer-lines",
                    lines,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let registry = this.selecting.registry.clone();
                        let Some(FileViewer {
                            shown: Some(Fetched::Contents { view, .. }),
                            highlighter,
                            request,
                            ..
                        }) = this.viewer.as_mut()
                        else {
                            return Vec::new();
                        };
                        let rows = range
                            .filter_map(|i| {
                                let line = view.lines.get(i)?;
                                let number = if numbered {
                                    format!("{:>digits$}", i + 1)
                                } else {
                                    String::new()
                                };

                                // Text in its language's colors, a hex dump
                                // in its bytes'.
                                let mapped = viewer_line(line);
                                let parts: Vec<(std::ops::Range<usize>, u32)> = match view.kind {
                                    Kind::Text => highlighter
                                        .spans(&view.lines, i)
                                        .iter()
                                        .map(|s| (s.range.clone(), s.token.color()))
                                        .collect(),
                                    Kind::Binary => hex_spans(view.chunk(i)),
                                };
                                let colors = colored(&mapped, parts);
                                let text = mapped.shown;
                                let part = selected
                                    .as_ref()
                                    .and_then(|r| part_of_line(r, i, text.len()));
                                Some(
                                    div()
                                        .id(i)
                                        .w_full()
                                        .h(px(size::LOG_ROW_HEIGHT))
                                        .flex()
                                        .items_center()
                                        .gap(px(size::LOG_COLUMN_GAP))
                                        .px(px(size::PANEL_PAD_X))
                                        .whitespace_nowrap()
                                        .when(numbered, |row| {
                                            row.child(
                                                div()
                                                    .flex_none()
                                                    .text_color(rgb(theme::FAINT))
                                                    .child(number),
                                            )
                                        })
                                        .child(
                                            shifted(
                                                offset,
                                                selectable(
                                                    Surface::Viewer,
                                                    i,
                                                    text,
                                                    part,
                                                    &registry,
                                                )
                                                .with_highlights(colors),
                                            )
                                            .text_color(rgb(theme::SOFT)),
                                        ),
                                )
                            })
                            .collect::<Vec<_>>();

                        // Lines too far down to color in this frame wait for
                        // the rest of the file, colored on another thread.
                        if let Some(rest) = highlighter.to_finish() {
                            finish_colors(rest, view.lines.clone(), *request, cx);
                        }
                        rows
                    }),
                )
                .track_scroll(&viewer.scroll)
                .flex_grow(layout::FILL)
                .min_h_0()
                .py(px(size::LIST_PAD_Y))
                .into_any_element()
            }
            (Some(Fetched::Directory { entries, .. }), _, None) => {
                let step_width = self.step_column_width();
                let comparing = self.session().other.is_some();

                // The size column is as wide as the longest size, and
                // absent from a listing with none, as the trace's is.
                let size_chars = entries
                    .iter()
                    .filter_map(|entry| entry.size)
                    .map(|size| thousands(size).len())
                    .max();
                let size_width = size_chars.map(|chars| px(chars as f32 * size::MONO_CHAR_WIDTH));
                uniform_list(
                    "viewer-entries",
                    entries.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let registry = this.selecting.registry.clone();
                        let Some(viewer) = this.viewer.as_ref() else {
                            return Vec::new();
                        };
                        let Some(entries) = listed_entries(viewer) else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|i| {
                                let entry = entries.get(i)?;

                                // A click on the step goes there; a click
                                // anywhere else opens the entry, or, while
                                // two runs are open, compares a file in
                                // both. A directory has no bytes to
                                // compare, so its listing opens either
                                // way. A drag or a double click selects
                                // the path instead.
                                let compares = comparing && entry.kind != ListedKind::Directory;
                                let (path, pid) = (entry.path.clone(), entry.pid);
                                let open = cx.listener(move |this, e: &ClickEvent, _, cx| {
                                    if !this.plain_click(e) {
                                        return;
                                    }
                                    if compares {
                                        this.open_file_diff(path.clone(), cx);
                                    } else {
                                        this.open_listed_file(path.clone(), pid, cx);
                                    }
                                });
                                let label = if compares {
                                    format!("Compare {} in both runs", entry.path)
                                } else {
                                    format!("Show {} at this step", entry.path)
                                };
                                let text = entry.shown(&viewer.path);
                                let part = selected
                                    .as_ref()
                                    .and_then(|r| part_of_line(r, i, text.len()));

                                // The step the run last wrote the entry
                                // at; blank for one the run did not write.
                                let step = div()
                                    .id(SharedString::from(format!("listed-step-{i}")))
                                    .flex_none()
                                    .w(step_width)
                                    .flex()
                                    .justify_end()
                                    .text_color(rgb(theme::FAINT));
                                let step = match entry.written {
                                    Some(written) => step
                                        .hover(|s| s.text_color(rgb(theme::AMBER)))
                                        .on_click(seek_on_click(written, cx))
                                        .tooltip(tooltip(LISTED_STEP_NOTE))
                                        .child(thousands(written)),
                                    None => step,
                                };
                                let size = size_width.map(|width| {
                                    div()
                                        .flex_none()
                                        .w(width)
                                        .flex()
                                        .justify_end()
                                        .text_color(rgb(theme::FAINT))
                                        .children(entry.size.map(thousands))
                                });
                                Some(
                                    div()
                                        .id(i)
                                        .role(Role::Button)
                                        .aria_label(label)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                                        .on_click(open)
                                        .w_full()
                                        .h(px(size::LIST_ROW_HEIGHT))
                                        .flex()
                                        .items_center()
                                        .gap(px(size::LIST_COLUMN_GAP))
                                        .px(px(size::PANEL_PAD_X))
                                        .child(step)
                                        .children(size)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis_start()
                                                .text_color(rgb(theme::SOFT))
                                                .child(selectable(
                                                    Surface::Viewer,
                                                    i,
                                                    text,
                                                    part,
                                                    &registry,
                                                )),
                                        ),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&viewer.scroll)
                .flex_grow(layout::FILL)
                .min_h_0()
                .py(px(size::LIST_PAD_Y))
                .into_any_element()
            }
            (None, None, Some((text, _))) if viewer.loading => div()
                .p(px(size::PANEL_PAD_X))
                .font_family(self.fonts.ui.clone())
                .child(waiting_block(
                    "viewer-cancel",
                    Some(text),
                    viewer.waiting.rows(|_| None),
                    cx.listener(|this, _, _, cx| this.cancel_viewer_read(cx)),
                ))
                .into_any_element(),
            (_, _, Some((text, color))) => {
                let part = selected
                    .as_ref()
                    .and_then(|r| part_of_line(r, 0, text.len()));
                div()
                    .p(px(size::PANEL_PAD_X))
                    .font_family(self.fonts.ui.clone())
                    .text_size(px(size::TEXT_UI))
                    .text_color(rgb(color))
                    .child(selectable(Surface::Viewer, 0, text, part, &registry))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        };
        let dim = if viewer.loading && viewer.shown.is_some() {
            STALE_OPACITY
        } else {
            1.0
        };

        Some(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_basis(relative(0.0))
                .bg(rgb(theme::PANEL))
                .child(title)
                .child(heading)
                .child(selects(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_grow(layout::FILL)
                        .min_h_0()
                        .opacity(dim)
                        .cursor(CursorStyle::IBeam)
                        .font_family(mono)
                        .text_size(px(size::TEXT_MONO))
                        .child(body)
                        .when(self.viewer_view().is_some(), |area| {
                            area.child(sideways_layer(Self::scroll_viewer_sideways, cx))
                        }),
                    Surface::Viewer,
                    cx,
                )),
        )
    }
}

/// Colors the rest of the viewer's file with `rest` on a background
/// thread, and shows the colors if the viewer still shows the same
/// contents, the answer to `request`, when they are done.
fn finish_colors(
    rest: Highlighter,
    lines: Vec<String>,
    request: Request,
    cx: &mut Context<Scrubber>,
) {
    let task = cx
        .background_executor()
        .spawn(async move { rest.finish(&lines) });
    cx.spawn(async move |this, cx| {
        let finished = task.await;
        let _ = this.update(cx, |this, cx| {
            let Some(viewer) = &mut this.viewer else {
                return;
            };
            if viewer.request != request {
                return;
            }
            viewer.highlighter = finished;
            cx.notify();
        });
    })
    .detach();
}

/// The digits of the line numbers in the gutter, or none for a hex dump,
/// which has no gutter.
fn gutter_digits(view: &View) -> usize {
    match view.kind {
        Kind::Text => view.lines.len().max(1).to_string().len(),
        Kind::Binary => 0,
    }
}

/// The language a file's text is in, by its path and first line; none
/// for a hex dump.
fn language_of(path: &str, view: &View) -> Option<Language> {
    match view.kind {
        Kind::Text => Language::of(path, view.lines.first().map_or("", String::as_str)),
        Kind::Binary => None,
    }
}

/// The message the viewer shows in place of a file's contents, and its
/// color, or None while it shows contents.
fn viewer_message(viewer: &FileViewer) -> Option<(String, u32)> {
    // A directory's listing shows whether or not the engine can read this
    // run's files: the trace has one. With no entry, a line says so.
    if let Some(Fetched::Directory {
        step,
        entries,
        source,
        ..
    }) = &viewer.shown
    {
        if !entries.is_empty() {
            return None;
        }
        let step = thousands(*step);
        let empty = match source {
            Source::Trace => format!(
                "{} is a directory. The run had written no file under it by step {step}; the list shows the files the run wrote.",
                viewer.path
            ),
            Source::Vm(_) => format!("{} is an empty directory at step {step}.", viewer.path),
        };
        return Some((empty, theme::SOFT));
    }
    if let Some(reason) = viewer.unavailable {
        return Some((reason.to_string(), theme::SOFT));
    }
    Some(match viewer.shown.as_ref() {
        Some(Fetched::Contents { .. }) => return None,
        Some(Fetched::Missing { step }) => (
            format!(
                "{} did not exist at step {}. Move the playhead to a step after the file was written.",
                viewer.path,
                thousands(*step)
            ),
            theme::SOFT,
        ),
        Some(Fetched::Failed { message, .. }) => (message.clone(), theme::RED_SOFT),
        Some(Fetched::Unreadable { message, .. }) => (message.to_string(), theme::SOFT),
        Some(Fetched::Booting { step, job_start }) => (
            format!(
                "At step {} the VM is still booting. The job starts at step {}; move the playhead past it to read the file.",
                thousands(*step),
                thousands(*job_start)
            ),
            theme::SOFT,
        ),
        Some(Fetched::Stopped { .. }) => (STOPPED.to_string(), theme::SOFT),
        Some(Fetched::Directory { .. }) => return None,
        None => (READING.to_string(), theme::MUTED),
    })
}

/// The entries of the listing the viewer shows, when it shows one with
/// entries in it.
fn listed_entries(viewer: &FileViewer) -> Option<&[Entry]> {
    match viewer.shown.as_ref()? {
        Fetched::Directory { entries, .. } if !entries.is_empty() => Some(entries),
        _ => None,
    }
}

/// The status line for a directory's listing of `entries` entries as of
/// `step`: what `source` lets the listing count.
fn describe_listing(entries: usize, source: Source, step: u64) -> String {
    let count = thousands(entries as u64);
    let step = thousands(step);
    let said = match (source, entries) {
        (Source::Trace, 0) => format!("no file written under it by step {step}"),
        (Source::Trace, 1) => "1 file the run wrote".to_string(),
        (Source::Trace, _) => format!("{count} files the run wrote"),
        (Source::Vm(ListedAll::No), _) => format!("{count} entries, the nearest of more"),
        (Source::Vm(ListedAll::Yes), 0) => format!("empty at step {step}"),
        (Source::Vm(ListedAll::Yes), 1) => "1 entry".to_string(),
        (Source::Vm(ListedAll::Yes), _) => format!("{count} entries"),
    };
    format!("directory \u{b7} {said}")
}

/// The status line for a file's contents: its kind, its size, and a note
/// when only part is shown.
fn describe_view(view: &View) -> String {
    let kind = match view.kind {
        Kind::Text => "text",
        Kind::Binary => "binary",
    };
    let size = format!("{kind}, {} bytes", thousands(view.size as u64));
    if !view.truncated {
        return size;
    }
    format!(
        "{size} \u{b7} showing the first {} KiB",
        thousands((viewer::MAX_SHOWN / 1024) as u64)
    )
}

/// An engine error in the viewer's words.
pub(super) fn explain(error: &EngineError) -> String {
    match error {
        EngineError::Missing { .. } => format!(
            "{error} Reading files at a step needs the rewind command, which replays the run to that step."
        ),
        EngineError::Failed { message, .. } => format!(
            "Rewind could not read the file at this step: {}",
            message.trim_start_matches(ENGINE_PREFIX)
        ),
        _ => error.to_string(),
    }
}

/// How the rewind command starts its messages.
const ENGINE_PREFIX: &str = "rewind: ";

#[cfg(test)]
mod tests {
    // What a directory's listing calls its files and says of them, with
    // paths as a trace has them and no window.
    use super::*;

    #[test]
    fn a_listing_says_what_it_counts() {
        // The trace's listing counts the files the run wrote, and with
        // none names the step. The VM's counts entries, says when the
        // directory is empty, and says when the VM cut the listing.
        let whole = Source::Vm(ListedAll::Yes);
        assert_eq!(
            describe_listing(0, Source::Trace, 68_381),
            "directory \u{b7} no file written under it by step 68,381"
        );
        assert_eq!(
            describe_listing(1, Source::Trace, 5),
            "directory \u{b7} 1 file the run wrote"
        );
        assert_eq!(
            describe_listing(1_200, Source::Trace, 5),
            "directory \u{b7} 1,200 files the run wrote"
        );
        assert_eq!(
            describe_listing(0, whole, 68_381),
            "directory \u{b7} empty at step 68,381"
        );
        assert_eq!(describe_listing(1, whole, 5), "directory \u{b7} 1 entry");
        assert_eq!(describe_listing(4, whole, 5), "directory \u{b7} 4 entries");
        assert_eq!(
            describe_listing(9_100, Source::Vm(ListedAll::No), 5),
            "directory \u{b7} 9,100 entries, the nearest of more"
        );
    }
}
