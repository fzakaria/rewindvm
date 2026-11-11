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
//! the engine cannot fork say so instead.
//!
//! The panel shows the whole source file of the frame it shows, scrolled
//! so the frame's line is in the middle, with that frame's arguments and
//! locals and then the frame list pinned below it. Clicking a frame in the
//! list shows that frame's file and variables, from the same answer, until
//! the next answer shows its chosen frame again.

use std::collections::HashMap;
use std::ops::Range;

use futures::StreamExt;
use gpui::{
    AnyElement, Context, CursorStyle, Div, MouseButton, Role, ScrollStrategy, SharedString,
    UniformListScrollHandle, div, prelude::*, px, relative, rgb, uniform_list,
};

use crate::answers::PlaceKey;
use crate::describe::thousands;
use crate::engine::{Cancel, EngineResult, REPLAYS_ANOTHER_WAY, goes_another_way};
use crate::model::Timeline;
use crate::request::Request;
use crate::run::Replays;
use crate::selection::{Mapped, Pos, Surface, part_of_line};
use crate::sideways::{Sideways, line_width, text_column};
use crate::source::{
    AfterAccess, Frame, Kind, Located, Progress, Shown, SourceFile, Thread, shown, target,
    variable_lines, without_source,
};
use crate::syntax::{Highlighter, Language};
use crate::theme::{self, layout, size};
use crate::ui::RightColumn;
use crate::ui::icons::Icon;
use crate::ui::scrubber::{Replay, Scrubber, replay_unavailable};
use crate::ui::selectable::{colored, selectable, selects, viewer_line};
use crate::ui::sideways::{shifted, sideways_layer};
use crate::ui::splits::{Edge, SOURCE_FILE_MIN, grip, measure};
use crate::ui::tabs::RightTab;
use crate::ui::waiting::waiting_block;
use crate::ui::widgets::{icon, panel_title};

/// Whose stack the panel shows at `step`: the user thread from the latest
/// event or switch, or the CPU's task when no user thread is identified.
fn thread_at(timeline: &Timeline, step: u64) -> Thread {
    timeline
        .active_thread(step)
        .map_or(Thread::OnTheCpu, |(pid, tid)| target(pid, tid))
}

/// Whose stack `panel` shows at `step`, and the access in the step it
/// shows it just after: as pinned there, else the thread [`thread_at`]
/// picks at the step's start.
fn panel_look(
    panel: &SourcePanel,
    timeline: &Timeline,
    step: u64,
) -> (Thread, Option<AfterAccess>) {
    match panel.pinned {
        Some((at, thread, after)) if at == step => (thread, after),
        _ => (thread_at(timeline, step), None),
    }
}

/// How long the playhead must rest before the panel asks again.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// The opacity of the last answer while a new one loads.
const STALE_OPACITY: f32 = 0.45;

/// What a variable's line has between its name and its value, what marks
/// an argument in the variables list, and what the list says of a frame
/// without any.
const VALUE_AFTER: &str = " = ";
const ARGUMENT_MARKER: &str = "arg";
const NO_VARIABLES: &str = "no arguments or locals";

/// The least the variables and frame lists shrink to in a panel too short
/// for their dragged heights and the file's least: one row each, so both
/// stay in sight under the file.
const LIST_SHRUNK: f32 = size::LOG_ROW_HEIGHT;

/// What marks the frame whose source is shown in the frame list.
const SHOWN_MARKER: &str = "\u{25b8}";

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
    /// Stops the engine command of the latest request while it runs.
    in_flight: Option<Cancel>,
    /// The shown file's scroll position.
    scroll: UniformListScrollHandle,
    /// Each file's colors, as far as they have been drawn, by the path
    /// the answer keys the file by.
    highlighters: HashMap<String, Highlighter>,
    /// How far the shown file's lines are scrolled sideways.
    sideways: Sideways,
    /// A thread the panel shows at a step in place of the step's event's,
    /// as going to a write shows the writer's, and the access in the step
    /// it shows the thread just after.
    pinned: Option<(u64, Thread, Option<AfterAccess>)>,
}

impl SourcePanel {
    /// Stops the engine command the latest request started, if it still
    /// runs: a newer request, or the panel closing, leaves its answer with
    /// no one to read it.
    fn supersede(&mut self) {
        if let Some(cancel) = self.in_flight.take() {
            cancel.cancel();
        }
    }
}

impl Drop for SourcePanel {
    fn drop(&mut self) {
        self.supersede();
    }
}

impl Scrubber {
    /// Opens the source panel at the playhead, in a tab of its own.
    pub(super) fn open_source(&mut self, cx: &mut Context<Self>) {
        // The browser viewer has no machine to run the recording on; at a
        // step an export with answers answers, the source comes from them.
        if cfg!(target_family = "wasm") && self.session.is_some() && !self.answered_here() {
            self.replay_in_desktop_app(Replay::Where.doing(), RightColumn::Source, cx);
            return;
        }

        let Some(session) = &self.session else {
            return;
        };
        let unavailable = replay_unavailable(session, Replay::Where, self.importing.is_some());
        let readable = unavailable.is_none();
        self.right_tab = RightTab::Source;
        self.shown.source = Some(SourcePanel {
            step: self.step,
            unavailable,
            shown: None,
            loading: readable,
            progress: Progress::default(),
            request: self.requests.issue(),
            in_flight: None,
            scroll: UniformListScrollHandle::new(),
            highlighters: HashMap::new(),
            sideways: Sideways::default(),
            pinned: None,
        });
        self.clear_selection_in(&[Surface::Viewer, Surface::Source, Surface::Variables]);
        if readable {
            self.fetch_source(cx);
        }
        cx.notify();
    }

    /// The panel, once the export on screen is in the engine's runs: a
    /// panel that said the run was still being imported asks the engine
    /// where the playhead's thread was now.
    pub(super) fn source_run_imported(&mut self, cx: &mut Context<Self>) {
        let importing = self.importing.is_some();
        let (Some(panel), Some(session)) = (&mut self.shown.source, &self.session) else {
            return;
        };
        panel.unavailable = replay_unavailable(session, Replay::Where, importing);
        if panel.unavailable.is_some() {
            return;
        }
        panel.loading = true;
        self.fetch_source(cx);
    }

    /// Moves the playhead to `step` with the source panel open on
    /// `thread` there, in place of the step's event's thread.
    pub(super) fn show_thread_at(
        &mut self,
        step: u64,
        (thread, after): (Thread, Option<AfterAccess>),
        cx: &mut Context<Self>,
    ) {
        if self.shown.source.is_none() {
            self.open_source(cx);
        }
        self.right_tab = RightTab::Source;
        if let Some(panel) = &mut self.shown.source {
            panel.pinned = Some((step, thread, after));
        }

        // At the step already, the playhead does not move, so the panel
        // asks for the new thread or moment itself.
        let stays = self
            .shown
            .source
            .as_ref()
            .is_some_and(|panel| panel.step == step);
        self.jump_to(step, cx);
        if stays {
            self.fetch_source(cx);
        }
    }

    pub(super) fn close_source(&mut self, cx: &mut Context<Self>) {
        self.shown.source = None;
        if self.right_tab == RightTab::Source {
            self.right_tab = RightTab::AtStep;
        }
        self.clear_selection_in(&[Surface::Source, Surface::Variables]);
        self.search_ahead(cx);
        cx.notify();
    }

    /// Opens the panel; shows its tab when it is open behind another; or
    /// closes it when it shows.
    pub(super) fn toggle_source(&mut self, cx: &mut Context<Self>) {
        match (&self.shown.source, self.right_tab) {
            (Some(_), RightTab::Source) => self.close_source(cx),
            (Some(_), _) => self.select_tab(RightTab::Source, cx),
            (None, _) => self.open_source(cx),
        }
    }

    /// Called whenever the playhead moves: the panel marks its answer
    /// stale and asks again once the playhead rests.
    pub(super) fn source_playhead_moved(&mut self, cx: &mut Context<Self>) {
        let step = self.step;
        let Some(panel) = &mut self.shown.source else {
            return;
        };
        if panel.unavailable.is_some() || panel.step == step {
            return;
        }
        panel.step = step;
        panel.loading = true;
        panel.supersede();
        panel.request = self.requests.issue();
        let request = panel.request;

        // An answer the engine gave for this thread at this step shows at
        // once; anything else waits for the playhead to rest.
        if let Some(answer) = self.known_place() {
            self.show_place(step, Ok(answer), cx);
            return;
        }
        let timer = cx.background_executor().timer(DEBOUNCE);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let still_latest = this
                    .shown
                    .source
                    .as_ref()
                    .is_some_and(|p| p.request == request);
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
        let Some(panel) = &mut self.shown.source else {
            return;
        };
        panel.supersede();
        panel.request = self.requests.issue();
        let step = panel.step;

        // The searches made ahead for the step the panel leaves stop.
        self.search_ahead(cx);
        let (Some(panel), Some(session)) = (&mut self.shown.source, &self.session) else {
            return;
        };

        // While the VM boots no thread of the job runs.
        let timeline = &session.run.timeline;
        let job_start = timeline.job_start.unwrap_or(0);
        if step < job_start {
            panel.loading = false;
            panel.shown = Some(Shown::Booting { step, job_start });
            cx.notify();
            return;
        }
        let (thread, after) = panel_look(panel, timeline, step);
        let run = session.run.path.clone();
        if let Some(answer) = self.known_place() {
            self.show_place(step, Ok(answer), cx);
            return;
        }

        // The engine's answer, kept by the thread and the step.
        let Some(panel) = &mut self.shown.source else {
            return;
        };
        panel.loading = true;
        panel.progress = Progress::default();
        let request = panel.request;
        let key: PlaceKey = (run.clone(), thread, step, after);
        let engine = self.engine_for_run();
        let Some(panel) = &mut self.shown.source else {
            return;
        };

        // The engine's lines come through a channel as it says them, and
        // the channel closes when the engine is done.
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let cancel = Cancel::default();
        panel.in_flight = Some(cancel.clone());
        let task = crate::jobs::on_own_thread(move || {
            engine.locate(&run, step, thread, after, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            // Each line becomes the panel's loading text while the panel
            // still waits for this request.
            while let Some(line) = said.next().await {
                let alive = this.update(cx, |this, cx| {
                    let Some(panel) = &mut this.shown.source else {
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
                if this
                    .shown
                    .source
                    .as_ref()
                    .is_none_or(|p| p.request != request)
                {
                    return;
                }
                if let Ok(answer) = &result {
                    this.place_answers.insert(key, answer.clone());
                }
                this.show_place(step, result, cx);
            });
        })
        .detach();
    }

    /// The answer kept for the thread of the panel's step, when the engine
    /// gave one there. None while the VM boots, which the panel says
    /// instead.
    fn known_place(&self) -> Option<Located> {
        let (panel, session) = (self.shown.source.as_ref()?, self.session.as_ref()?);
        let timeline = &session.run.timeline;
        if panel.step < timeline.job_start.unwrap_or(0) {
            return None;
        }
        let (thread, after) = panel_look(panel, timeline, panel.step);
        let key: PlaceKey = (session.run.path.clone(), thread, panel.step, after);
        self.place_answers.get(&key)
    }

    /// The answer the source panel shows for the playhead's step, when it
    /// shows one: what gdb starts in, at the frame the panel shows.
    pub(super) fn place_at_playhead(&self) -> Option<(&Located, Option<usize>)> {
        let panel = self.shown.source.as_ref()?;
        if panel.step != self.step || panel.loading {
            return None;
        }
        panel.shown.as_ref()?.located()
    }

    /// Stops the lookup the panel waits on; the engine's answer, a
    /// cancellation, then says so.
    fn cancel_source_lookup(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self
            .shown
            .source
            .as_ref()
            .and_then(|p| p.in_flight.as_ref())
        {
            cancel.cancel();
        }
        cx.notify();
    }

    /// Shows the engine's answer for the thread at `step`.
    fn show_place(&mut self, step: u64, result: EngineResult<Located>, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.shown.source else {
            return;
        };
        panel.loading = false;
        panel.in_flight = None;

        // A run this build replays another way is so at every step, so the
        // panel stops asking, and so does everything else that brings the
        // run to a step.
        if result.as_ref().is_err_and(goes_another_way) {
            panel.unavailable = Some(REPLAYS_ANOTHER_WAY.to_string());
            if let Some(session) = &mut self.session {
                session.replays = Replays::AnotherWay;
            }
        }
        panel.shown = Some(shown(step, result));
        panel.highlighters.clear();
        panel.sideways = Sideways::default();
        self.clear_selection_in(&[Surface::Source, Surface::Variables]);
        self.centre_shown_line();

        // At the step the run failed at, the values on the failing line
        // are searched for ahead of a click on one.
        self.search_ahead(cx);
        cx.notify();
    }

    /// The answer on screen, when the panel shows one, and the frame whose
    /// source it shows.
    fn located(&self) -> Option<(&Located, Option<usize>)> {
        self.shown.source.as_ref()?.shown.as_ref()?.located()
    }

    /// Shows frame `frame`'s source, when a row of the frame list is
    /// clicked, from the start of its lines.
    fn select_frame(&mut self, frame: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.shown.source.as_mut() else {
            return;
        };
        let Some(shown) = panel.shown.as_mut() else {
            return;
        };
        if !shown.select_frame(frame) {
            return;
        }
        panel.sideways = Sideways::default();
        self.clear_selection_in(&[Surface::Source, Surface::Variables]);
        self.centre_shown_line();
        self.search_ahead(cx);
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
        let Some(panel) = &self.shown.source else {
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
        let Some(panel) = &mut self.shown.source else {
            return;
        };
        panel.sideways.scroll_by(pixels, widest, visible);
        cx.notify();
    }

    /// Scrolls the shown file so row `row` is at its top.
    pub(super) fn scroll_source_to(&self, row: usize) {
        if let Some(panel) = &self.shown.source {
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
            return without_source(frame)
                .into_iter()
                .map(Mapped::plain)
                .collect();
        }
        self.shown
            .source
            .as_ref()
            .and_then(source_message)
            .map(|(text, _)| vec![Mapped::plain(text)])
            .unwrap_or_default()
    }

    /// The panel in place of the right column.
    pub(super) fn render_source(&self, cx: &mut Context<Self>) -> Option<Div> {
        let panel = self.shown.source.as_ref()?;
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
        let when = match panel.pinned {
            Some((at, _, Some(after))) if at == shown_step => match after.kind {
                crate::blame::AccessKind::Write => "just after the write in step",
                crate::blame::AccessKind::Read => "just after the read in step",
            },
            _ => "as of step",
        };
        let title = panel_title(
            &format!("Source \u{b7} {when} {}", thousands(shown_step)),
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
            (Some(Shown::Booting { .. }), None) => "the VM is still booting".to_string(),
            (Some(Shown::Unreadable { .. }), None) => "not available for this run".to_string(),
            (Some(Shown::Failed { .. }), None) => "could not walk the stack".to_string(),
            (Some(Shown::Stopped { .. }), None) => "not read".to_string(),
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
                for (i, text) in without_source(frame).into_iter().enumerate() {
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
            (None, None, Some((text, _))) if panel.loading && panel.shown.is_none() => div()
                .p(px(size::PANEL_PAD_X))
                .font_family(self.fonts.ui.clone())
                .child(waiting_block(
                    "source-cancel",
                    Some(text),
                    panel.progress.waiting.rows(|_| None),
                    cx.listener(|this, _, _, cx| this.cancel_source_lookup(cx)),
                ))
                .into_any_element(),
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

        // The shown frame's variables, then the frame list, pinned under
        // the file area so they stay in reach however small the window,
        // each as high as its top edge was dragged. The file area has no
        // height of its own to give up past its least, so in a short
        // panel the two lists shrink, down to their least, rather than
        // cover the file.
        let variables = located.map(|_| self.render_variables(frame_shown, cx).opacity(dim));
        let frames = located.map(|located| self.render_frames(located, at, cx).opacity(dim));

        Some(
            div()
                .relative()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .flex_basis(relative(0.0))
                .bg(rgb(theme::PANEL))
                .child(measure(|m| &m.source, &self.measured))
                .child(title)
                .child(heading)
                .child(selects(
                    div()
                        .id("source-body")
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_grow(layout::FILL)
                        .flex_basis(relative(0.0))
                        .min_h(px(SOURCE_FILE_MIN))
                        .overflow_hidden()
                        .opacity(dim)
                        .cursor(CursorStyle::IBeam)
                        .font_family(mono)
                        .text_size(px(size::TEXT_MONO))
                        .child(file_area)
                        .when(shown_file.is_some(), |body| {
                            body.child(sideways_layer(Self::scroll_source_sideways, cx))
                        })
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                                this.source_click(e.position, cx)
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                            this.source_hover(e.position, cx)
                        }))
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            if !hovered {
                                this.leave_source(cx);
                            }
                        })),
                    Surface::Source,
                    cx,
                ))
                .children(variables)
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
                }) = this.shown.source.as_mut()
                else {
                    return Vec::new();
                };
                let Some((file, key, highlighter)) = file_and_colors(shown, highlighters) else {
                    return Vec::new();
                };

                // The shown frame and whose it is, whose values can be
                // blamed.
                let step = shown.step();
                let whose = shown
                    .located()
                    .and_then(|(l, at)| Some((l.pid, l.tid, l.frames.get(at?)?)));
                let run = this.session.as_ref().map(|s| s.run.path.clone());
                let blames = &this.blames;
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
                        let underlined = match (&whose, &run) {
                            (Some((pid, tid, frame)), Some(run)) => crate::ui::blame::underlines(
                                blames,
                                run,
                                frame,
                                (number, line),
                                &mapped,
                                colors,
                                (*pid, *tid, step),
                            ),
                            _ => crate::ui::blame::Underlined {
                                highlights: colors,
                                clicks: Vec::new(),
                                notes: Vec::new(),
                            },
                        };
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
                                            .with_highlights(underlined.highlights),
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

    /// The selectable lines of the variables list: the shown frame's
    /// arguments, then its locals, each as `name = value`.
    pub(super) fn variables_lines(&self) -> Vec<Mapped> {
        let Some((located, Some(at))) = self.located() else {
            return Vec::new();
        };
        let Some(frame) = located.frames.get(at) else {
            return Vec::new();
        };
        variable_lines(frame)
            .into_iter()
            .map(|(_, text)| Mapped::plain(text))
            .collect()
    }

    /// `frame`'s arguments, then its locals, each `name = value` with its
    /// name colored and each argument marked as one, in a list of its own
    /// that scrolls, as high as its top edge was dragged or a short panel
    /// leaves it. A frame gdb named no variables in, as a frame without
    /// DWARF, says so.
    fn render_variables(&self, frame: Option<&Frame>, cx: &mut Context<Self>) -> Div {
        let registry = self.selecting.registry.clone();
        let selected = self.selected_range(Surface::Variables);
        let lines = frame.map(variable_lines).unwrap_or_default();
        let empty = lines.is_empty();

        // Each variable, in the list's order, and whose they are: one in
        // memory can be blamed with a click on its row.
        let variables: Vec<&crate::source::Variable> = frame
            .map(|f| f.arguments.iter().chain(&f.locals).collect())
            .unwrap_or_default();
        let whose = self.located().map(|(l, _)| (l.pid, l.tid));
        let step = self
            .shown
            .source
            .as_ref()
            .map_or(self.step, |p| p.shown.as_ref().map_or(p.step, Shown::step));
        let rows = lines.into_iter().enumerate().map(|(i, (kind, text))| {
            let variable = variables.get(i).copied();
            let target = variable
                .zip(whose)
                .and_then(|(v, whose)| crate::blame::target_of(&v.name, &v.location, whose, step));
            let place = variable.and_then(|v| where_it_is(&v.location));
            let name_end = text.find(VALUE_AFTER).unwrap_or(text.len());
            let (marker, name_color) = match kind {
                Kind::Argument => (ARGUMENT_MARKER, theme::AMBER_PALE),
                Kind::Local => ("", theme::SOFT),
            };
            let colors = colored(
                &Mapped::plain(text.clone()),
                [
                    (0..name_end, name_color),
                    (name_end..text.len(), theme::TEXT),
                ],
            );
            let part = selected
                .as_ref()
                .and_then(|r| part_of_line(r, i, text.len()));
            let row = div()
                .w_full()
                .flex()
                .flex_none()
                .gap(px(size::CARD_GAP))
                .px(px(size::PANEL_PAD_X))
                .h(px(size::LOG_ROW_HEIGHT))
                .items_center()
                .whitespace_nowrap()
                .child(
                    div()
                        .flex_none()
                        .w(px(size::ARGUMENT_MARKER_WIDTH))
                        .text_color(rgb(theme::FAINT))
                        .child(marker),
                )
                .child(
                    div().min_w_0().overflow_hidden().text_ellipsis().child(
                        selectable(Surface::Variables, i, text, part, &registry)
                            .with_highlights(colors),
                    ),
                )
                .when_some(place, |row, place| {
                    row.child(div().flex_none().text_color(rgb(theme::FAINT)).child(place))
                });
            let Some(target) = target else {
                return row.into_any_element();
            };
            row.id(SharedString::from(format!("source-variable-{i}")))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                .tooltip(crate::ui::widgets::tooltip(format!(
                    "{} is in memory: click to see who last wrote it.",
                    target.text
                )))
                .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                    if this.plain_click(e) {
                        this.blame(
                            target.clone(),
                            crate::blame::Ask::Writes(1),
                            Some(e.position()),
                            cx,
                        );
                    }
                }))
                .into_any_element()
        });
        let list = div()
            .id("source-variables")
            .flex()
            .flex_col()
            .flex_basis(px(self.splits.variables))
            .flex_shrink(layout::SHRINK)
            .min_h(px(LIST_SHRUNK))
            .overflow_y_scroll()
            .cursor(CursorStyle::IBeam)
            .when(empty, |list| {
                list.child(
                    div()
                        .px(px(size::PANEL_PAD_X))
                        .h(px(size::LOG_ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .text_color(rgb(theme::MUTED))
                        .child(NO_VARIABLES),
                )
            })
            .children(rows);
        let list = selects(list, Surface::Variables, cx);
        self.list_section("VARIABLES", list, Edge::Variables, cx)
    }

    /// One of the lists under the file, under its heading, with the grip
    /// that drags its top edge. The heading and the list are clipped to
    /// what the panel leaves the section, so a panel too short for both
    /// lists cuts each short rather than draw one over the other. The grip
    /// reaches above the section's top edge, outside the clip.
    fn list_section(
        &self,
        heading: &'static str,
        list: impl IntoElement,
        edge: Edge,
        cx: &mut Context<Self>,
    ) -> Div {
        let clipped = div()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_hidden()
            .pt(px(size::CARD_GAP))
            .pb(px(size::LIST_PAD_Y))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .child(
                div()
                    .px(px(size::PANEL_PAD_X))
                    .pb(px(size::CARD_GAP))
                    .font_family(self.fonts.ui.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(heading),
            )
            .child(list);
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_shrink(layout::SHRINK)
            .min_h_0()
            .border_t_1()
            .border_color(rgb(theme::LINE_SOFT))
            .child(clipped)
            .child(grip(edge, cx))
    }

    /// The thread's frames, innermost first, the one whose source is
    /// shown marked: each frame's level, function and place, in a list of
    /// its own that scrolls, as high as its top edge was dragged or a
    /// short panel leaves it. Clicking a row shows that frame's source.
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
        let list = div()
            .id("source-frames")
            .flex()
            .flex_col()
            .flex_basis(px(self.splits.frames))
            .flex_shrink(layout::SHRINK)
            .min_h(px(LIST_SHRUNK))
            .overflow_y_scroll()
            .children(
                located
                    .frames
                    .iter()
                    .enumerate()
                    .map(|(i, frame)| row(i, frame)),
            );
        self.list_section("FRAMES", list, Edge::Frames, cx)
    }
}

/// Where a variable is, as its row says it after the value: in which
/// register, or in memory and blameable; nothing for one optimized out,
/// whose value says so.
fn where_it_is(location: &crate::blame::Location) -> Option<String> {
    use crate::blame::Location;
    match location {
        Location::Memory { .. } => Some(BLAMEABLE.to_string()),
        Location::TooLarge { size, .. } => Some(format!("{size} bytes")),
        Location::Register {
            register: Some(register),
        } => Some(format!("in {register}")),
        Location::Register { register: None } => Some("in a register".to_string()),
        Location::OptimizedOut | Location::Unreadable => None,
    }
}

/// What a variable in memory says after its value.
const BLAMEABLE: &str = "blame";

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
            let Some(panel) = &mut this.shown.source else {
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
        Some(Shown::Stopped { .. }) => (crate::source::STOPPED.to_string(), theme::SOFT),
        None => (panel.progress.text(), theme::MUTED),
    })
}

#[cfg(test)]
mod tests {
    // Source targets from recorded events and switches, without starting gdb.
    use super::*;
    use rewind_trace::switches::Switch;
    use rewind_trace::{Event, EventKind, Trace};

    #[test]
    fn source_follows_a_switch_past_the_last_event() {
        // Thread 41 writes, then thread 42 runs without emitting an event.
        const WRITE: u64 = 10;
        const SWITCH: u64 = 20;
        const PID: u32 = 40;
        const WRITER: u32 = 41;
        const RUNNER: u32 = 42;
        let timeline = Timeline::new(
            Trace {
                events: vec![Event {
                    step: WRITE,
                    pid: PID,
                    tid: WRITER,
                    kind: EventKind::Output {
                        fd: 1,
                        bytes: b"ready".to_vec(),
                    },
                }],
                switches: vec![Switch {
                    step: SWITCH,
                    pid: PID,
                    tid: RUNNER,
                    memory: true,
                    name: "worker".into(),
                }],
            },
            Some(SWITCH + 1),
            None,
        );
        assert_eq!(
            thread_at(&timeline, WRITE),
            Thread::Of {
                pid: PID,
                tid: WRITER
            }
        );
        assert_eq!(
            thread_at(&timeline, SWITCH),
            Thread::Of {
                pid: PID,
                tid: RUNNER
            }
        );
        assert_eq!(
            thread_at(&timeline, SWITCH + 1),
            Thread::Of {
                pid: PID,
                tid: RUNNER
            }
        );
    }

    #[test]
    fn source_asks_the_cpu_after_idle_and_kernel_switches() {
        // A previous user write must not pin source to that user thread after a switch.
        const WRITE: u64 = 10;
        const SWITCH: u64 = 20;
        const USER_PID: u32 = 40;
        const IDLE_PID: u32 = 0;
        const KERNEL_PID: u32 = 2;
        for pid in [IDLE_PID, KERNEL_PID] {
            let timeline = Timeline::new(
                Trace {
                    events: vec![Event {
                        step: WRITE,
                        pid: USER_PID,
                        tid: USER_PID,
                        kind: EventKind::Output {
                            fd: 1,
                            bytes: b"ready".to_vec(),
                        },
                    }],
                    switches: vec![Switch {
                        step: SWITCH,
                        pid,
                        tid: pid,
                        memory: false,
                        name: "kernel".into(),
                    }],
                },
                None,
                None,
            );
            assert_eq!(thread_at(&timeline, SWITCH), Thread::OnTheCpu);
            assert_eq!(thread_at(&timeline, SWITCH + 1), Thread::OnTheCpu);
        }
    }
}
