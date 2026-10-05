//! The file viewer: a file's contents as they were at the playhead.
//!
//! Clicking a row of the files panel opens the file here, in place of
//! "At this step". Reading a file means the engine brings the run back to
//! the step and reads it inside the VM, which takes seconds, so while the
//! viewer is pinned to the playhead a move waits for the playhead to rest
//! before asking, and the last contents stay on screen, dimmed, until the
//! new ones arrive. Runs the engine cannot replay (the built-in examples,
//! runs opened from a .rwd file or a bare trace) say so instead.

use std::time::Duration;

use gpui::{
    Context, CursorStyle, Div, MouseButton, Role, ScrollStrategy, UniformListScrollHandle, div,
    prelude::*, px, relative, rgb, uniform_list,
};

use crate::answers::FileKey;
use crate::describe::{short_store_paths, thousands};
use crate::engine::{
    Cancel, EngineError, EngineResult, FileAtStep, REPLAYS_ANOTHER_WAY, goes_another_way,
};
use crate::request::Request;
use crate::run::{Origin, Replays, Session};
use crate::selection::{Mapped, Surface, part_of_line};
use crate::sideways::{Sideways, line_width, text_column};
use crate::syntax::{Highlighter, Language};
use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::Scrubber;
use crate::ui::selectable::{colored, selectable, selects, viewer_line};
use crate::ui::sideways::{shifted, sideways_layer};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{icon, panel_title};
use crate::viewer::{self, Kind, View, hex_spans};

/// How long the playhead must rest before a pinned viewer asks again.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// The opacity of the last contents while new ones load.
const STALE_OPACITY: f32 = 0.45;

/// Why the viewer cannot read files of some runs.
const EXAMPLE_REASON: &str = "This example was recorded on another machine and ships as its trace only. Reading a file means replaying the run up to the step, which needs the run's inputs and Rewind with KVM on this machine. Record a run of your own to read its files at any step.";
const EXPORT_TRACE_REASON: &str = "This run was opened from a .rwd file that holds its trace only. Open its replayable export, the -replayable.rwd file, to read its files at any step.";
const EXPORT_IMPORTING_REASON: &str = "This run is being imported into Rewind from its .rwd file. Its files can be read at any step once it is in.";
const TRACE_REASON: &str = "This run was opened from a bare trace file. Open its run directory instead to read its files at any step.";

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
}

/// The open viewer.
pub struct FileViewer {
    pub path: String,
    /// The process that wrote the file, whose view of paths the engine
    /// reads it with.
    pub pid: u32,
    pub pin: Pin,
    /// The step the viewer shows or is loading.
    pub step: u64,
    /// Why this run's files cannot be read, when they cannot.
    pub unavailable: Option<&'static str>,
    pub shown: Option<Fetched>,
    pub loading: bool,
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
fn unavailable(session: &Session) -> Option<&'static str> {
    if session.replays == Replays::AnotherWay {
        return Some(REPLAYS_ANOTHER_WAY);
    }
    match session.run.origin {
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
        let unavailable = unavailable(session);
        self.right_tab = RightTab::File;
        self.viewer = Some(FileViewer {
            path,
            pid,
            pin: Pin::FollowPlayhead,
            step: self.step,
            unavailable,
            shown: None,
            loading: unavailable.is_none(),
            request: self.requests.issue(),
            in_flight: None,
            scroll: UniformListScrollHandle::new(),
            highlighter: Highlighter::new(None),
            sideways: Sideways::default(),
        });
        self.clear_selection_in(&[Surface::Viewer, Surface::Source]);
        if unavailable.is_none() {
            self.count_engine_action(cx);
            self.fetch_file(cx);
        }
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

    /// The message the viewer shows in place of contents, as one line.
    pub(super) fn viewer_message_lines(&self) -> Vec<Mapped> {
        self.viewer
            .as_ref()
            .and_then(viewer_message)
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
        if viewer.pin != Pin::FollowPlayhead || viewer.unavailable.is_some() || viewer.step == step
        {
            return;
        }
        viewer.step = step;
        viewer.loading = true;
        viewer.supersede();
        viewer.request = self.requests.issue();
        let request = viewer.request;

        // An answer the engine gave for the same version of the file shows
        // at once; anything else waits for the playhead to rest.
        if let Some(answer) = self.known_file() {
            self.show_file(step, Ok(answer), cx);
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

    /// The answer kept for the file at the viewer's step, when the engine
    /// gave one for the version the file has there. None while the VM
    /// boots, which the viewer says instead.
    fn known_file(&self) -> Option<FileAtStep> {
        let (viewer, session) = (self.viewer.as_ref()?, self.session.as_ref()?);
        let timeline = &session.run.timeline;
        if viewer.step < timeline.job_start.unwrap_or(0) {
            return None;
        }
        let key = FileKey::at(
            &session.run.path,
            &timeline.trace,
            &viewer.path,
            viewer.pid,
            viewer.step,
        );
        self.file_answers.get(&key)
    }

    /// Asks the engine for the file at the viewer's step, on a background
    /// thread, unless an answer for the file's version there is kept.
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
        if let Some(answer) = self.known_file() {
            self.show_file(step, Ok(answer), cx);
            return;
        }

        // The engine's answer, kept by the file's version.
        let (Some(viewer), Some(session)) = (&mut self.viewer, &self.session) else {
            return;
        };
        viewer.loading = true;
        let request = viewer.request;
        let (run, pid, path) = (session.run.path.clone(), viewer.pid, viewer.path.clone());
        let key = FileKey::at(&run, &session.run.timeline.trace, &path, pid, step);
        let engine = self.engine.clone();
        let cancel = Cancel::default();
        viewer.in_flight = Some(cancel.clone());
        let task =
            crate::jobs::on_own_thread(move || engine.cat(&run, step, Some(pid), &path, &cancel));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.viewer.as_ref().is_none_or(|v| v.request != request) {
                    return;
                }
                if let Ok(answer) = &result {
                    this.file_answers.insert(key, answer.clone());
                }
                this.show_file(step, result, cx);
            });
        })
        .detach();
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
            Err(e) if goes_another_way(&e) => Fetched::Unreadable {
                step,
                message: REPLAYS_ANOTHER_WAY,
            },
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
            | Some(Fetched::Booting { step, .. }) => *step,
            None => viewer.step,
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
            &format!("File \u{b7} as of step {}", thousands(shown_step)),
            Some(controls.into_any_element()),
        );

        // The path and what the viewer knows about the file.
        let status = match (&viewer.shown, viewer.unavailable) {
            (_, Some(_)) => "not available for this run".to_string(),
            (None, None) => format!("reading step {}\u{2026}", thousands(viewer.step)),
            (Some(Fetched::Contents { view, .. }), None) => describe_view(view),
            (Some(Fetched::Missing { step }), None) => {
                format!("did not exist at step {}", thousands(*step))
            }
            (Some(Fetched::Failed { .. }), None) => "could not be read".to_string(),
            (Some(Fetched::Unreadable { .. }), None) => "not available for this run".to_string(),
            (Some(Fetched::Booting { .. }), None) => "the VM is still booting".to_string(),
        };
        let status = if viewer.loading && viewer.shown.is_some() {
            format!(
                "{status} \u{b7} reading step {}\u{2026}",
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

        // The body: the contents, or a message in their place, both
        // selectable.
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
        None => (
            "Rewind is bringing the run back to this step to read the file. This takes a few seconds.".to_string(),
            theme::MUTED,
        ),
    })
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
fn explain(error: &EngineError) -> String {
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
