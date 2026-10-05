//! The source panel: the line of the program's own code the playhead's
//! thread was on, with the frames around it.
//!
//! Show source, or the s key, opens it in place of "At this step", as a
//! file opens in the file viewer. The engine forks the run at the step and
//! walks the thread's stack in gdb, which takes seconds, so the panel asks
//! again only once the playhead rests, and the last answer stays on
//! screen, dimmed, until the new one arrives. With no answer on screen,
//! the panel shows the engine's latest line while it works, such as the
//! debug info gdb downloads the first time, which can take a minute. Runs
//! the engine cannot fork say so instead, as do runs recorded before the
//! guest kernel listed its tasks.
//!
//! The panel shows the whole source file of the frame it shows, scrolled
//! so the frame's line is in the middle, with the frame list pinned below
//! it. Clicking a frame in the list shows that frame's file, from the same
//! answer, until the next answer shows its chosen frame again.

use std::collections::HashMap;
use std::ops::Range;

use futures::StreamExt;
use gpui::{
    AnyElement, Context, CursorStyle, Div, MouseButton, Role, ScrollStrategy, SharedString,
    UniformListScrollHandle, div, prelude::*, px, relative, rgb, uniform_list,
};

use crate::describe::thousands;
use crate::request::Request;
use crate::selection::{Mapped, Pos, Surface, part_of_line};
use crate::sideways::{Sideways, line_width, text_column};
use crate::source::{Frame, Located, Progress, Shown, SourceFile, shown, target};
use crate::syntax::{Highlighter, Language};
use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::{Replay, Scrubber, replay_unavailable};
use crate::ui::selectable::{colored, selectable, selects, viewer_line};
use crate::ui::sideways::{shifted, sideways_layer};
use crate::ui::widgets::{icon, panel_title};

/// How long the playhead must rest before the panel asks again.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// The opacity of the last answer while a new one loads.
const STALE_OPACITY: f32 = 0.45;

/// What marks the frame whose source is shown in the frame list.
const SHOWN_MARKER: &str = "\u{25b8}";

/// How many rows of the frame list show before the list scrolls, so the
/// file above keeps the rest of the panel however deep the stack.
const FRAME_ROWS_SHOWN: f32 = 4.0;

/// The open panel.
pub struct SourcePanel {
    /// The step the panel shows or is loading.
    pub step: u64,
    /// Why this run cannot be forked, when it cannot.
    pub unavailable: Option<String>,
    pub shown: Option<Shown>,
    pub loading: bool,
    /// What the engine has said while it looks for the latest request.
    pub progress: Progress,
    /// The request whose answer this waits on; an answer to any other,
    /// from before the playhead moved or this was opened, is dropped.
    request: Request,
    /// The shown file's scroll position.
    scroll: UniformListScrollHandle,
    /// Each file's colors, as far as they have been drawn, by the path
    /// the answer keys the file by.
    highlighters: HashMap<String, Highlighter>,
    /// How far the shown file's lines are scrolled sideways.
    sideways: Sideways,
}

impl Scrubber {
    /// Opens the source panel at the playhead, closing the file viewer,
    /// which shares its place.
    pub(super) fn open_source(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let unavailable =
            replay_unavailable(&session.run.origin, Replay::Where, self.importing.is_some());
        let readable = unavailable.is_none();
        self.viewer = None;
        self.source = Some(SourcePanel {
            step: self.step,
            unavailable,
            shown: None,
            loading: readable,
            progress: Progress::default(),
            request: self.requests.issue(),
            scroll: UniformListScrollHandle::new(),
            highlighters: HashMap::new(),
            sideways: Sideways::default(),
        });
        self.clear_selection_in(&[Surface::Viewer, Surface::Source]);
        if readable {
            self.count_engine_action(cx);
            self.fetch_source(cx);
        }
        cx.notify();
    }

    pub(super) fn close_source(&mut self, cx: &mut Context<Self>) {
        self.source = None;
        self.clear_selection_in(&[Surface::Source]);
        cx.notify();
    }

    /// Opens the panel, or closes it when it is open.
    pub(super) fn toggle_source(&mut self, cx: &mut Context<Self>) {
        match self.source {
            Some(_) => self.close_source(cx),
            None => self.open_source(cx),
        }
    }

    /// Called whenever the playhead moves: the panel marks its answer
    /// stale and asks again once the playhead rests.
    pub(super) fn source_playhead_moved(&mut self, cx: &mut Context<Self>) {
        let step = self.step;
        let Some(panel) = &mut self.source else {
            return;
        };
        if panel.unavailable.is_some() || panel.step == step {
            return;
        }
        panel.step = step;
        panel.loading = true;
        panel.request = self.requests.issue();
        let request = panel.request;
        let timer = cx.background_executor().timer(DEBOUNCE);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let still_latest = this.source.as_ref().is_some_and(|p| p.request == request);
                if still_latest {
                    this.fetch_source(cx);
                }
            });
        })
        .detach();
    }

    /// Asks the engine where the playhead's thread was, on a background
    /// thread. The thread is the one of the latest event at or before the
    /// panel's step.
    fn fetch_source(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(session)) = (&mut self.source, &self.session) else {
            return;
        };
        panel.request = self.requests.issue();
        let step = panel.step;

        // While the VM boots no thread of the job runs, and the kernel's
        // own events name none.
        let timeline = &session.run.timeline;
        let job_start = timeline.job_start.unwrap_or(0);
        let event = timeline
            .event_index_at(step)
            .and_then(|i| timeline.event(i));
        let thread = event.and_then(|e| target(e.pid, e.tid));
        let immediate = if step < job_start {
            Some(Shown::Booting { step, job_start })
        } else if thread.is_none() {
            Some(Shown::Kernel { step })
        } else {
            None
        };
        if let Some(answer) = immediate {
            panel.loading = false;
            panel.shown = Some(answer);
            cx.notify();
            return;
        }
        let Some((pid, tid)) = thread else {
            return;
        };

        panel.loading = true;
        panel.progress = Progress::default();
        let request = panel.request;
        let run = session.run.path.clone();
        let engine = self.engine.clone();

        // The engine's lines come through a channel as it says them, and
        // the channel closes when the engine is done.
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let task = cx.background_executor().spawn(async move {
            engine.locate(&run, step, pid, tid, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            // Each line becomes the panel's loading text while the panel
            // still waits for this request.
            while let Some(line) = said.next().await {
                let alive = this.update(cx, |this, cx| {
                    let Some(panel) = &mut this.source else {
                        return;
                    };
                    if panel.request != request {
                        return;
                    }
                    panel.progress.said(&line);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }

            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(panel) = &mut this.source else {
                    return;
                };
                if panel.request != request {
                    return;
                }
                panel.loading = false;
                panel.shown = Some(shown(step, result));
                panel.highlighters.clear();
                panel.sideways = Sideways::default();
                this.clear_selection_in(&[Surface::Source]);
                this.centre_shown_line();
                cx.notify();
            });
        })
        .detach();
    }

    /// The answer on screen, when the panel shows one, and the frame whose
    /// source it shows.
    fn located(&self) -> Option<(&Located, Option<usize>)> {
        self.source.as_ref()?.shown.as_ref()?.located()
    }

    /// Shows frame `frame`'s source, when a row of the frame list is
    /// clicked, from the start of its lines.
    fn select_frame(&mut self, frame: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.source.as_mut() else {
            return;
        };
        let Some(shown) = panel.shown.as_mut() else {
            return;
        };
        if !shown.select_frame(frame) {
            return;
        }
        panel.sideways = Sideways::default();
        self.clear_selection_in(&[Surface::Source]);
        self.centre_shown_line();
        cx.notify();
    }

    /// The source file the panel shows, and the frame it is shown for.
    pub(super) fn shown_file(&self) -> Option<(&SourceFile, &Frame)> {
        let (located, at) = self.located()?;
        let at = at?;
        Some((located.file_of(at)?, located.frames.get(at)?))
    }

    /// Scrolls the shown file so the shown frame's line is in the middle.
    fn centre_shown_line(&self) {
        let Some(panel) = &self.source else {
            return;
        };
        let Some((located, Some(at))) = self.located() else {
            return;
        };
        if let Some(row) = located.centred_row(at) {
            panel.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
    }

    /// Scrolls the shown file's lines `pixels` sideways, in an area
    /// `width` pixels wide.
    fn scroll_source_sideways(&mut self, pixels: f32, width: f32, cx: &mut Context<Self>) {
        let Some((file, _)) = self.shown_file() else {
            return;
        };
        let widest = line_width(file.widest);
        let visible = text_column(width, file.digits());
        let Some(panel) = &mut self.source else {
            return;
        };
        panel.sideways.scroll_by(pixels, widest, visible);
        cx.notify();
    }

    /// Scrolls the shown file so row `row` is at its top.
    pub(super) fn scroll_source_to(&self, row: usize) {
        if let Some(panel) = &self.source {
            panel.scroll.scroll_to_item(row, ScrollStrategy::Top);
        }
    }

    /// The selectable lines of the panel when it shows no file: what
    /// stands in for the source of a frame without any, or the message in
    /// place of an answer. A shown file's lines are read from the file.
    pub(super) fn source_lines(&self) -> Vec<Mapped> {
        if let Some((located, Some(at))) = self.located()
            && let Some(frame) = located.frames.get(at)
        {
            return frame
                .without_source()
                .into_iter()
                .map(Mapped::plain)
                .collect();
        }
        self.source
            .as_ref()
            .and_then(source_message)
            .map(|(text, _)| vec![Mapped::plain(text)])
            .unwrap_or_default()
    }

    /// The panel in place of the right column.
    pub(super) fn render_source(&self, cx: &mut Context<Self>) -> Option<Div> {
        let panel = self.source.as_ref()?;
        let mono = self.fonts.mono.clone();

        // The title bar: the step shown and the close button.
        let shown_step = panel.shown.as_ref().map_or(panel.step, Shown::step);
        let close = div()
            .id("source-close")
            .role(Role::Button)
            .aria_label("Close the source")
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
            .on_click(cx.listener(|this, _, _, cx| this.close_source(cx)));
        let title = panel_title(
            &format!("Source \u{b7} as of step {}", thousands(shown_step)),
            Some(close.into_any_element()),
        );

        // The frame shown, and whose stack it is.
        let answer = self.located();
        let located = answer.map(|(located, _)| located);
        let at = answer.and_then(|(_, at)| at);
        let frame_shown = located.zip(at).and_then(|(l, at)| l.frames.get(at));
        let headline = match (located, frame_shown) {
            (Some(_), Some(frame)) => {
                format!("{}  {}", frame.function_label(), frame.place_label())
            }
            (Some(_), None) => "no frame names a function".to_string(),
            (None, _) => String::new(),
        };
        let status = match (&panel.shown, &panel.unavailable) {
            (_, Some(_)) => "not available for this run".to_string(),
            (None, None) => format!(
                "walking the stack at step {}\u{2026}",
                thousands(panel.step)
            ),
            (Some(Shown::Located { located, .. }), None) => format!(
                "process {} ({}) \u{b7} thread {}",
                located.pid, located.process, located.tid
            ),
            (Some(Shown::Kernel { .. }), None) => "the kernel, in no process".to_string(),
            (Some(Shown::Booting { .. }), None) => "the VM is still booting".to_string(),
            (Some(Shown::Unreadable { .. }), None) => "not available for this run".to_string(),
            (Some(Shown::Failed { .. }), None) => "could not walk the stack".to_string(),
        };
        // A window of a large file says which of its lines it holds.
        let shown_file = self.shown_file();
        let status = match shown_file.and_then(|(file, _)| file.note()) {
            Some(note) => format!("{status} \u{b7} {note}"),
            None => status,
        };
        let status = if panel.loading && panel.shown.is_some() {
            format!(
                "{status} \u{b7} walking the stack at step {}\u{2026}",
                thousands(panel.step)
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
            .when(!headline.is_empty(), |h| {
                h.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .text_size(px(size::TEXT_MONO))
                        .text_color(rgb(theme::TEXT))
                        .child(headline),
                )
            })
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(status),
            );

        // The file area: the shown frame's whole file with its line marked,
        // its address and program when it has no file, or a message in
        // place of an answer.
        let registry = self.selecting.registry.clone();
        let selected = self.selected_range(Surface::Source);
        let file_area = match (shown_file, frame_shown, source_message(panel)) {
            (Some((file, frame)), _, _) => self.render_file(file, frame, panel, selected, cx),
            (None, Some(frame), _) => {
                let mut rows = div().flex().flex_col().py(px(size::LIST_PAD_Y));
                for (i, text) in frame.without_source().into_iter().enumerate() {
                    let part = selected
                        .as_ref()
                        .and_then(|r| part_of_line(r, i, text.len()));
                    rows = rows.child(
                        div()
                            .w_full()
                            .h(px(size::LOG_ROW_HEIGHT))
                            .flex()
                            .flex_none()
                            .items_center()
                            .px(px(size::PANEL_PAD_X))
                            .whitespace_nowrap()
                            .text_color(rgb(theme::MUTED))
                            .child(selectable(Surface::Source, i, text, part, &registry)),
                    );
                }
                rows.into_any_element()
            }
            (None, None, Some((text, color))) => {
                let part = selected
                    .as_ref()
                    .and_then(|r| part_of_line(r, 0, text.len()));
                div()
                    .p(px(size::PANEL_PAD_X))
                    .font_family(self.fonts.ui.clone())
                    .text_size(px(size::TEXT_UI))
                    .text_color(rgb(color))
                    .child(selectable(Surface::Source, 0, text, part, &registry))
                    .into_any_element()
            }
            (None, None, None) => div().into_any_element(),
        };
        let dim = if panel.loading && panel.shown.is_some() {
            STALE_OPACITY
        } else {
            1.0
        };

        // The frame list, pinned under the file area so it stays in reach
        // however small the window.
        let frames = located.map(|located| self.render_frames(located, at, cx).opacity(dim));

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
                        .id("source-body")
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_grow(layout::FILL)
                        .min_h_0()
                        .opacity(dim)
                        .cursor(CursorStyle::IBeam)
                        .font_family(mono)
                        .text_size(px(size::TEXT_MONO))
                        .child(file_area)
                        .when(shown_file.is_some(), |body| {
                            body.child(sideways_layer(Self::scroll_source_sideways, cx))
                        }),
                    Surface::Source,
                    cx,
                ))
                .children(frames),
        )
    }

    /// The lines of `file`, drawn only where they are on screen and moved
    /// left as far as `panel` is scrolled sideways, with `frame`'s line
    /// marked and the selected part of each highlighted.
    fn render_file(
        &self,
        file: &SourceFile,
        frame: &Frame,
        panel: &SourcePanel,
        selected: Option<Range<Pos>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = file.lines.len();
        let digits = file.digits();
        let marked_row = frame.line.and_then(|line| file.row_of(line));

        // The sideways offset, kept within what the list's width, as of
        // the last frame, allows.
        let width = f32::from(panel.scroll.0.borrow().base_handle.bounds().size.width);
        let offset = panel
            .sideways
            .offset(line_width(file.widest), text_column(width, digits));
        uniform_list(
            "source-lines",
            count,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let registry = this.selecting.registry.clone();
                let Some(SourcePanel {
                    shown: Some(shown),
                    highlighters,
                    request,
                    ..
                }) = this.source.as_mut()
                else {
                    return Vec::new();
                };
                let Some((file, key, highlighter)) = file_and_colors(shown, highlighters) else {
                    return Vec::new();
                };
                let rows = range
                    .filter_map(|row| {
                        let line = file.lines.get(row)?;
                        let marked = Some(row) == marked_row;
                        let number = file.number_of(row);
                        let mapped = viewer_line(line);
                        let spans = highlighter.spans(&file.lines, row);
                        let colors = colored(
                            &mapped,
                            spans.iter().map(|s| (s.range.clone(), s.token.color())),
                        );
                        let text = mapped.shown;
                        let part = selected
                            .as_ref()
                            .and_then(|r| part_of_line(r, row, text.len()));
                        Some(
                            div()
                                .id(row)
                                .w_full()
                                .h(px(size::LOG_ROW_HEIGHT))
                                .flex()
                                .items_center()
                                .gap(px(size::LOG_COLUMN_GAP))
                                .px(px(size::PANEL_PAD_X))
                                .whitespace_nowrap()
                                .when(marked, |row| row.bg(rgb(theme::AMBER_CARD)))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(if marked {
                                            theme::AMBER
                                        } else {
                                            theme::FAINT
                                        }))
                                        .child(format!("{number:>digits$}")),
                                )
                                .child(
                                    shifted(
                                        offset,
                                        selectable(Surface::Source, row, text, part, &registry)
                                            .with_highlights(colors),
                                    )
                                    .text_color(rgb(
                                        if marked { theme::TEXT } else { theme::SOFT },
                                    )),
                                ),
                        )
                    })
                    .collect::<Vec<_>>();

                // Lines too far down to color in this frame, as the line a
                // long file opens at can be, wait for the rest of the file,
                // colored on another thread.
                if let Some(rest) = highlighter.to_finish() {
                    let key = key.to_string();
                    finish_colors(rest, file.lines.clone(), key, *request, cx);
                }
                rows
            }),
        )
        .track_scroll(&panel.scroll)
        .flex_grow(layout::FILL)
        .min_h_0()
        .py(px(size::LIST_PAD_Y))
        .into_any_element()
    }

    /// The thread's frames, innermost first, the one whose source is
    /// shown marked: each frame's level, function and place, in a list of
    /// its own that scrolls past a few rows. Clicking a row shows that
    /// frame's source.
    fn render_frames(&self, located: &Located, at: Option<usize>, cx: &mut Context<Self>) -> Div {
        let row = |i: usize, frame: &Frame| {
            let shown = at == Some(i);
            let (name_color, place_color) = if shown {
                (theme::AMBER, theme::AMBER_PALE)
            } else {
                (theme::SOFT, theme::MUTED)
            };
            div()
                .id(SharedString::from(format!("source-frame-{i}")))
                .role(Role::Button)
                .aria_label(format!("Show the source of frame {}", frame.level))
                .w_full()
                .flex()
                .flex_none()
                .gap(px(size::CARD_GAP))
                .px(px(size::PANEL_PAD_X))
                .h(px(size::LOG_ROW_HEIGHT))
                .items_center()
                .whitespace_nowrap()
                .cursor_pointer()
                .when(shown, |r| r.bg(rgb(theme::AMBER_CARD)))
                .when(!shown, |r| r.hover(|s| s.bg(rgb(theme::RAISED_HOVER))))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| this.select_frame(i, cx)))
                .child(
                    div()
                        .flex_none()
                        .w(px(size::ICON_CLOSE))
                        .text_color(rgb(theme::AMBER))
                        .child(if shown { SHOWN_MARKER } else { "" }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::FAINT))
                        .child(format!("#{}", frame.level)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(name_color))
                        .child(frame.function_label()),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis_start()
                        .text_color(rgb(place_color))
                        .child(frame.place_label()),
                )
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .pt(px(size::CARD_GAP))
            .pb(px(size::LIST_PAD_Y))
            .border_t_1()
            .border_color(rgb(theme::LINE_SOFT))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .child(
                div()
                    .px(px(size::PANEL_PAD_X))
                    .pb(px(size::CARD_GAP))
                    .font_family(self.fonts.ui.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child("FRAMES"),
            )
            .child(
                div()
                    .id("source-frames")
                    .flex()
                    .flex_col()
                    .max_h(px(FRAME_ROWS_SHOWN * size::LOG_ROW_HEIGHT))
                    .overflow_y_scroll()
                    .children(
                        located
                            .frames
                            .iter()
                            .enumerate()
                            .map(|(i, frame)| row(i, frame)),
                    ),
            )
    }
}

/// Colors the rest of the file the answer keys by `key` with `rest` on a
/// background thread, and keeps the colors if the panel still shows the
/// answer to `request` when they are done.
fn finish_colors(
    rest: Highlighter,
    lines: Vec<String>,
    key: String,
    request: Request,
    cx: &mut Context<Scrubber>,
) {
    let task = cx
        .background_executor()
        .spawn(async move { rest.finish(&lines) });
    cx.spawn(async move |this, cx| {
        let finished = task.await;
        let _ = this.update(cx, |this, cx| {
            let Some(panel) = &mut this.source else {
                return;
            };
            if panel.request != request {
                return;
            }
            panel.highlighters.insert(key, finished);
            cx.notify();
        });
    })
    .detach();
}

/// The file `shown` shows, the path the answer keys it by, and its
/// highlighter, made on first use with the language of the file's name,
/// as the shown frame's DWARF gives it, and of its first line when the
/// answer carries it.
fn file_and_colors<'a>(
    shown: &'a Shown,
    highlighters: &'a mut HashMap<String, Highlighter>,
) -> Option<(&'a SourceFile, &'a str, &'a mut Highlighter)> {
    let (located, at) = shown.located()?;
    let at = at?;
    let frame = located.frames.get(at)?;
    let key = frame.fullname.as_deref()?;
    let file = located.files.get(key)?;
    let highlighter = highlighters.entry(key.to_string()).or_insert_with(|| {
        let name = frame.file.as_deref().unwrap_or(key);
        let first_line = match file.first {
            FIRST_LINE => file.lines.first().map_or("", String::as_str),
            _ => "",
        };
        Highlighter::new(Language::of(name, first_line))
    });
    Some((file, key, highlighter))
}

/// The number of a file's first line.
const FIRST_LINE: u32 = 1;

/// The message the panel shows in place of an answer, and its color, or
/// None while it shows one.
fn source_message(panel: &SourcePanel) -> Option<(String, u32)> {
    if let Some(reason) = &panel.unavailable {
        return Some((reason.clone(), theme::SOFT));
    }
    Some(match panel.shown.as_ref() {
        Some(Shown::Located { .. }) => return None,
        Some(Shown::Kernel { step }) => (
            format!(
                "At step {} the kernel was running, in no process. Move the playhead to a step of a process to see its source.",
                thousands(*step)
            ),
            theme::SOFT,
        ),
        Some(Shown::Booting { step, job_start }) => (
            format!(
                "At step {} the VM is still booting. The job starts at step {}; move the playhead past it to see its source.",
                thousands(*step),
                thousands(*job_start)
            ),
            theme::SOFT,
        ),
        Some(Shown::Unreadable { message, .. }) => (message.to_string(), theme::SOFT),
        Some(Shown::Failed { message, .. }) => (message.clone(), theme::RED_SOFT),
        None => (panel.progress.text(), theme::MUTED),
    })
}
