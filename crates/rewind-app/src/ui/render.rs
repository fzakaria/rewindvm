//! Drawing the scrubber: the header, the timeline and its controls, the
//! three panels, the notices, and the empty state before a run is open.
//!
//! Every frame reads the precomputed tables in `model::Timeline`; nothing
//! here walks the trace.

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, CursorStyle, DispatchPhase, Div, FontWeight,
    HighlightStyle, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels,
    Point, Role, ScrollStrategy, SharedString, Window, canvas, div, fill, point, prelude::*, px,
    relative, rgb, rgba, uniform_list,
};

use crate::describe::{self, EventTone, short_store_paths, thousands};
use crate::family::{Family, Row, RunEntry};
use crate::model::{FileOp, FileTone, LogFilter, Motion, RowKind, Tone, ticks};
use crate::run::{Agreement, Session, Verdict, short_id};
use crate::selection::{Mapped, Surface, part_of_line};
use crate::theme::{self, layout, size};
use crate::tour::Anchor;
use crate::ui::chrome::client_tiling;
use crate::ui::icons::Icon;
use crate::ui::scrubber::{ForkState, NoticeAction, NoticeTone, Scrubber};
use crate::ui::selectable::{PID_CHARS, mapped, process_row, selectable, selects};
use crate::ui::tour::explore_button;
use crate::ui::widgets::{
    Availability, ButtonStyle, PillTone, button, icon, panel_title, pill, readout,
};
use crate::ui::{
    CopySelection, EnterLicense, ForkHere, GoToEnd, GoToStart, JumpToDivergence, JumpToFailure,
    KEY_CONTEXT, NextEvent, NextPhase, OpenRun, PreviousEvent, PreviousPhase, SelectAll, StartTour,
    StepBack, StepForward,
};

/// Header labels are cut to this many characters.
const MAX_NAME_CHARS: usize = 36;
const MAX_SUBJECT_CHARS: usize = 72;

/// The processor count shown in the header: the engine runs one vCPU.
const VCPUS: u32 = 1;

impl Render for Scrubber {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_title(window);

        // The selectable lines register where they land as this frame is
        // laid out.
        self.selecting.registry.clear();

        // The root takes the keyboard: every binding in the scrubber's
        // context lands on one of these handlers.
        let root = div()
            .id("scrubber")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &PreviousEvent, _, cx| this.go(Motion::PreviousEvent, cx)),
            )
            .on_action(cx.listener(|this, _: &NextEvent, _, cx| this.go(Motion::NextEvent, cx)))
            .on_action(cx.listener(|this, _: &StepBack, _, cx| this.go(Motion::StepBack, cx)))
            .on_action(cx.listener(|this, _: &StepForward, _, cx| this.go(Motion::StepForward, cx)))
            .on_action(cx.listener(|this, _: &GoToStart, _, cx| this.go(Motion::Start, cx)))
            .on_action(cx.listener(|this, _: &GoToEnd, _, cx| this.go(Motion::End, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousPhase, _, cx| this.go(Motion::PreviousPhase, cx)),
            )
            .on_action(cx.listener(|this, _: &NextPhase, _, cx| this.go(Motion::NextPhase, cx)))
            .on_action(cx.listener(|this, _: &JumpToFailure, _, cx| this.go(Motion::Failure, cx)))
            .on_action(
                cx.listener(|this, _: &JumpToDivergence, _, cx| this.go(Motion::Divergence, cx)),
            )
            .on_action(cx.listener(|this, _: &ForkHere, _, cx| this.fork_here(cx)))
            .on_action(cx.listener(|this, _: &OpenRun, _, cx| this.prompt_open(cx)))
            .on_action(cx.listener(|this, _: &EnterLicense, window, cx| {
                this.open_license_dialog(window, cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_UI));

        let root = root
            .on_action(cx.listener(|this, _: &StartTour, window, cx| this.start_tour(window, cx)))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)));

        // A run on screen, or the empty state asking for one.
        let body: AnyElement = if self.session.is_some() {
            self.render_session(window, cx).into_any_element()
        } else {
            self.render_empty(window, cx).into_any_element()
        };

        // The notices and the license dialog sit beside the scrubber's
        // key context, so keys typed in the dialog do not scrub.
        let mut window_root = div()
            .size_full()
            .relative()
            .text_color(rgb(theme::TEXT))
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_UI))
            .child(root.child(body))
            .child(self.render_notices(cx));
        if let Some(dialog) = self.render_license_dialog(cx) {
            window_root = window_root.child(dialog);
        }
        if let Some(dialog) = self.render_link_dialog(cx) {
            window_root = window_root.child(dialog);
        }
        if let Some(menu) = self.render_context_menu(cx) {
            window_root = window_root.child(menu);
        }
        window_root = window_root.child(self.selection_listener(cx));

        // Drawing its own chrome, the window needs a visible edge and
        // handles to resize it by.
        if let Some(tiling) = client_tiling(window) {
            if !tiling.is_tiled() {
                window_root = window_root.border_1().border_color(rgb(theme::LINE_2));
            }
            if let Some(edges) = self.resize_edges(window) {
                window_root = window_root.child(edges);
            }
        }
        window_root
    }
}

impl Scrubber {
    fn session(&self) -> &Session {
        self.session
            .as_ref()
            .expect("rendering a session without one")
    }

    fn render_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let header = self.render_header(window, cx);
        let timeline = self.render_timeline(window, cx);
        let log = self.render_log(cx);
        let middle = self.render_middle(cx);
        // The right column: the file viewer when a file is open, which
        // takes the log's share of the width, else "At this step".
        let (at_step, right_flex) = match self.render_viewer(cx) {
            Some(viewer) => (viewer, layout::LOG_FLEX),
            None => (self.render_at_step(cx), layout::SIDE_FLEX),
        };

        // The panel row: three columns with one pixel rules between them,
        // drawn by the row's background showing through the gaps.
        let log_column = log.flex_grow(layout::LOG_FLEX);
        let middle_column = middle.flex_grow(layout::SIDE_FLEX);
        let at_step_column = at_step.flex_grow(right_flex);
        let runs_column = self
            .family
            .as_ref()
            .filter(|_| self.runs_open)
            .map(|family| self.render_runs(family, cx));
        let panels = div()
            .flex()
            .flex_grow(layout::FILL)
            .min_h_0()
            .gap(px(1.0))
            .bg(rgb(theme::LINE))
            .child(log_column)
            .child(middle_column)
            .child(at_step_column)
            .children(runs_column);

        // The terminal pane, while a shell or gdb runs, under the panels.
        let terminal = self.render_terminal(window, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(timeline)
            .child(panels)
            .children(terminal)
    }

    /// The header: the mark, the run and what it built, verdict pills, and
    /// the run's size on the right.
    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let session = self.session();
        let run = &session.run;
        let fonts = &self.fonts;

        let brand = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(size::BRAND_GAP))
            .child(icon(Icon::Mark, size::MARK_ICON, theme::AMBER))
            .child(
                div()
                    .text_size(px(size::TEXT_BRAND))
                    .font_weight(FontWeight::BOLD)
                    .child("Rewind"),
            );

        let subject = div()
            .min_w_0()
            .truncate()
            .font_family(fonts.mono.clone())
            .text_size(px(size::TEXT_SUBJECT))
            .text_color(rgb(theme::MUTED))
            .child(describe::clip(
                &short_store_paths(&run.subject()),
                MAX_SUBJECT_CHARS,
            ));

        // The verdict of the run on screen, then the run it is compared
        // with, if any.
        let verdict = run.verdict();
        let tone = match verdict {
            Verdict::Failed => PillTone::Failed,
            Verdict::Passed => PillTone::Passed,
        };
        let name = describe::clip(&run.label(), MAX_NAME_CHARS);
        let mut left = div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(size::HEADER_GAP))
            .child(brand)
            .child(subject)
            .child(pill(
                format!("{name} \u{b7} {}", run.verdict_label()),
                tone,
                fonts,
            ));
        if let Some(other) = &session.other {
            let other_name = describe::clip(&other.label(), MAX_NAME_CHARS);
            left = left.child(pill(
                format!("{other_name} \u{b7} {}", other.verdict_label()),
                PillTone::Compared,
                fonts,
            ));
        }

        // Where a forked run branched off its parent.
        if let Some(parent) = &run.manifest.parent {
            let schedule = run
                .manifest
                .schedule
                .map_or_else(String::new, |s| format!(" \u{b7} schedule {s}"));
            left = left.child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(fonts.mono.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(format!(
                        "fork of {} at {}{schedule}",
                        short_id(&parent.id),
                        thousands(parent.step)
                    )),
            );
        }

        // The Runs panel's toggle, with how many runs the build has.
        if let Some(family) = &self.family {
            let open = self.runs_open;
            left = left.child(
                div()
                    .id("runs-toggle")
                    .flex_none()
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(pill(
                        format!("runs \u{b7} {}", family.runs.len()),
                        if open {
                            PillTone::Compared
                        } else {
                            PillTone::Quiet
                        },
                        fonts,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_runs(cx))),
            );
        }

        let timeline = &run.timeline;
        let stats = format!(
            "{} steps \u{b7} {} events \u{b7} {VCPUS} vCPU",
            thousands(timeline.total),
            thousands(timeline.trace.events.len() as u64)
        );
        self.header_frame(left, Some(stats), window, cx)
    }

    /// The header bar around `left`: on the right the run's size, the
    /// license pill, the tour's "?" button and, when the app draws its own
    /// chrome, the window controls. The whole bar is the title bar then.
    fn header_frame(
        &self,
        left: Div,
        stats: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let mut right = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(size::HEADER_GAP))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::MUTED));
        if let Some(stats) = stats {
            right = right.child(stats);
        }
        right = right
            .child(self.render_license_pill(cx))
            .child(self.help_button(cx));
        let controls = self.window_controls(window);
        let pad_right = if controls.is_some() {
            size::CONTROLS_PAD_RIGHT
        } else {
            size::PAGE_PAD_X
        };
        if let Some(controls) = controls {
            right = right.child(controls);
        }

        let header = div()
            .h(px(size::HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(size::HEADER_GAP))
            .pl(px(size::PAGE_PAD_X))
            .pr(px(pad_right))
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::LINE))
            .child(left)
            .child(right);
        self.titlebar(header, window, cx)
    }

    /// The "?" button that starts the tour.
    fn help_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("help")
            .role(Role::Button)
            .aria_label("Start the tour (F1)")
            .size(px(size::ICON_BUTTON_WIDTH))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_UI))
            .text_color(rgb(theme::MUTED))
            .hover(|s| s.text_color(rgb(theme::TEXT)).bg(rgb(theme::RAISED_HOVER)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| this.start_tour(window, cx)))
            .child("?")
    }

    /// The timeline: phase segments, ticks, markers and the playhead over
    /// a track that scrubs on click and drag, then the controls row.
    fn render_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let session = self.session();
        let t = &session.run.timeline;
        let step = self.step;
        let active_phase = t.phase_index_at(step);
        let mono = self.fonts.mono.clone();

        let mut track = div()
            .id("track")
            .relative()
            .h(px(size::TRACK_HEIGHT))
            .w_full()
            .cursor_pointer()
            .font_family(mono)
            .text_size(px(size::TEXT_SMALL));

        // The phases, each a segment as wide as its share of the run.
        for (i, phase) in t.phases.iter().enumerate() {
            let start = t.fraction_of(phase.start);
            let share = t.fraction_of(phase.end) - start;
            let active = Some(i) == active_phase;
            let (bg, fg) = if active {
                (theme::AMBER_DEEP, theme::AMBER_PALE)
            } else {
                (
                    theme::PHASE_GREYS[i % theme::PHASE_GREYS.len()],
                    theme::SOFT,
                )
            };
            let mut segment = div()
                .size_full()
                .flex()
                .items_center()
                .pl(px(size::SEGMENT_LABEL_PAD))
                .overflow_hidden()
                .whitespace_nowrap()
                .rounded(px(size::RADIUS_SEGMENT))
                .bg(rgb(bg))
                .text_color(rgb(fg));
            if share > layout::LABEL_MIN_SHARE {
                segment = segment.child(phase.name.clone());
            }
            track = track.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(start))
                    .w(relative(share))
                    .pr(px(size::SEGMENT_GAP))
                    .child(segment),
            );
        }

        // Tick marks under the track at round step counts.
        for tick in ticks(t.total, layout::TICK_TARGET) {
            track = track.child(
                div()
                    .absolute()
                    .left(relative(t.fraction_of(tick)))
                    .bottom(px(-size::TICK_DROP))
                    .w(px(size::TICK_WIDTH))
                    .h(px(size::TICK_HEIGHT))
                    .bg(rgb(theme::TICK)),
            );
        }

        // Markers: forks made from here, the first divergence, the failure.
        let marker = |at: u64, width: f32, color: u32| {
            div()
                .absolute()
                .left(relative(t.fraction_of(at)))
                .top(px(-size::MARKER_OVERHANG))
                .bottom(px(-size::MARKER_OVERHANG))
                .w(px(width))
                .bg(rgb(color))
        };
        // The step this run was forked from its parent at.
        if let Some(parent) = &session.run.manifest.parent {
            track = track.child(
                marker(parent.step, size::FORK_MARK_WIDTH, theme::AMBER_PALE)
                    .bg(rgba(0))
                    .border_l_2()
                    .border_dashed()
                    .border_color(rgb(theme::AMBER_PALE)),
            );
        }
        // Forks of this run on disk, from this session or before it.
        let shown_id = session.run.manifest.id.clone().unwrap_or_default();
        let disk_forks = self
            .family
            .iter()
            .flat_map(|f| f.runs.iter())
            .filter_map(|r| {
                r.parent
                    .as_ref()
                    .filter(|p| p.id == shown_id)
                    .map(|p| p.step)
            });
        for step in disk_forks {
            track = track.child(
                marker(step, size::FORK_MARK_WIDTH, theme::AMBER_PALE)
                    .bg(rgba(0))
                    .border_l_1()
                    .border_dashed()
                    .border_color(rgb(theme::AMBER_PALE)),
            );
        }
        for fork in &self.forks {
            let color = match fork.state {
                ForkState::Failed(_) => theme::MUTED,
                ForkState::Pending | ForkState::Created(_) => theme::AMBER_PALE,
            };
            track = track.child(
                marker(fork.step, size::FORK_MARK_WIDTH, color)
                    .bg(rgba(0))
                    .border_l_2()
                    .border_dashed()
                    .border_color(rgb(color)),
            );
        }
        if let Some(divergence) = session.divergence_step() {
            track = track.child(marker(divergence, size::DIVERGENCE_WIDTH, theme::BLUE));
        }
        if let Some(failure) = t.failure {
            track = track.child(marker(failure.step, size::FAILURE_WIDTH, theme::RED));
        }

        // The playhead: an amber bar in a soft glow.
        let at = relative(t.fraction_of(step));
        track = track
            .child(
                div()
                    .absolute()
                    .left(at)
                    .ml(px(-size::PLAYHEAD_GLOW_WIDTH / 2.0))
                    .top(px(-size::PLAYHEAD_OVERHANG - size::PLAYHEAD_WIDTH))
                    .bottom(px(-size::PLAYHEAD_OVERHANG - size::PLAYHEAD_WIDTH))
                    .w(px(size::PLAYHEAD_GLOW_WIDTH))
                    .rounded(px(size::PLAYHEAD_GLOW_WIDTH / 2.0))
                    .bg(rgba(theme::AMBER_GLOW_A)),
            )
            .child(
                div()
                    .absolute()
                    .left(at)
                    .ml(px(-size::PLAYHEAD_WIDTH / 2.0))
                    .top(px(-size::PLAYHEAD_OVERHANG))
                    .bottom(px(-size::PLAYHEAD_OVERHANG))
                    .w(px(size::PLAYHEAD_WIDTH))
                    .rounded(px(size::RADIUS_PLAYHEAD))
                    .bg(rgb(theme::AMBER)),
            );

        // The focus ring, while the scrubber has the keyboard.
        if self.focus.is_focused(window) {
            track = track.child(
                div()
                    .absolute()
                    .top(px(-size::FOCUS_RING_OUTSET_Y))
                    .bottom(px(-size::FOCUS_RING_OUTSET_Y))
                    .left(px(-size::FOCUS_RING_OUTSET_X))
                    .right(px(-size::FOCUS_RING_OUTSET_X))
                    .rounded(px(size::RADIUS_FOCUS))
                    .border_1()
                    .border_color(rgba(theme::FOCUS_RING_A)),
            );
        }

        track = track.child(self.track_input(cx));
        let track = self.with_callout(track, Anchor::Timeline, cx);

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(size::TRACK_TO_CONTROLS))
            .pt(px(size::TIMELINE_PAD_TOP))
            .pb(px(size::TIMELINE_PAD_BOTTOM))
            .px(px(size::PAGE_PAD_X))
            .border_b_1()
            .border_color(rgb(theme::LINE))
            .child(track)
            .child(self.render_controls(cx))
    }

    /// An invisible layer over the track that turns clicks and drags into
    /// playhead moves. The listeners are window-wide so a drag keeps going
    /// when the pointer leaves the track.
    fn track_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let dragging = self.dragging.clone();
        let focus = self.focus.clone();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let along = move |position: Point<Pixels>| {
                    let offset = position.x - bounds.origin.x;
                    offset / bounds.size.width
                };

                // A press on the track starts a drag and moves the playhead.
                let (down_view, down_drag) = (view.clone(), dragging.clone());
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    let pressed = e.button == MouseButton::Left && bounds.contains(&e.position);
                    if phase != DispatchPhase::Bubble || !pressed {
                        return;
                    }
                    down_drag.set(true);
                    window.focus(&focus, cx);
                    down_view.update(cx, |this, cx| this.scrub_to(along(e.position), cx));
                });

                // Moving with the button held drags the playhead.
                let (move_view, move_drag) = (view.clone(), dragging.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || !move_drag.get() {
                        return;
                    }
                    if e.pressed_button != Some(MouseButton::Left) {
                        move_drag.set(false);
                        return;
                    }
                    move_view.update(cx, |this, cx| this.scrub_to(along(e.position), cx));
                });

                // Letting go anywhere ends the drag.
                let up_drag = dragging.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, _, _, _| {
                    if e.button == MouseButton::Left {
                        up_drag.set(false);
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// Transport buttons on the left; readouts and the fork button on the
    /// right.
    fn render_controls(&self, cx: &mut Context<Self>) -> Div {
        let session = self.session();
        let t = &session.run.timeline;
        let has_divergence = if session.divergence_step().is_some() {
            Availability::Enabled
        } else {
            Availability::Disabled
        };
        let has_failure = if t.failure.is_some() {
            Availability::Enabled
        } else {
            Availability::Disabled
        };

        let start = button("to-start", ButtonStyle::Neutral, Availability::Enabled)
            .aria_label("Go to start (Home)")
            .w(px(size::ICON_BUTTON_WIDTH))
            .px_0()
            .child(icon(Icon::GoToStart, size::ICON_START, theme::TEXT))
            .on_click(cx.listener(|this, _, _, cx| this.go(Motion::Start, cx)));
        let previous = button(
            "previous-event",
            ButtonStyle::Neutral,
            Availability::Enabled,
        )
        .child(icon(Icon::ChevronLeft, size::ICON_CHEVRON, theme::TEXT))
        .child("Previous event")
        .on_click(cx.listener(|this, _, _, cx| this.go(Motion::PreviousEvent, cx)));
        let next = button("next-event", ButtonStyle::Neutral, Availability::Enabled)
            .child("Next event")
            .child(icon(Icon::ChevronRight, size::ICON_CHEVRON, theme::TEXT))
            .on_click(cx.listener(|this, _, _, cx| this.go(Motion::NextEvent, cx)));
        let divergence = button("to-divergence", ButtonStyle::Divergence, has_divergence)
            .child("Jump to divergence")
            .on_click(cx.listener(|this, _, _, cx| this.go(Motion::Divergence, cx)));
        let divergence = self.with_callout(divergence, Anchor::DivergenceButton, cx);
        let failure = button("to-failure", ButtonStyle::Failure, has_failure)
            .child("Jump to failure")
            .on_click(cx.listener(|this, _, _, cx| this.go(Motion::Failure, cx)));
        let failure = self.with_callout(failure, Anchor::FailureButton, cx);

        let phase = t
            .phase_index_at(self.step)
            .map_or_else(String::new, |i| t.phases[i].name.clone());
        let readouts = div()
            .flex()
            .items_center()
            .gap(px(size::READOUT_GAP))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_READOUT))
            .child(readout(
                "step",
                format!("{} / {}", thousands(self.step), thousands(t.total)),
                theme::AMBER,
                FontWeight::SEMIBOLD,
            ))
            .child(readout("phase", phase, theme::TEXT, FontWeight::NORMAL));
        let fork = button("fork", ButtonStyle::Primary, Availability::Enabled)
            .child(icon(Icon::Fork, size::ICON_FORK, theme::AMBER_INK))
            .child("Fork from here")
            .on_click(cx.listener(|this, _, _, cx| this.fork_here(cx)));
        let fork = self.with_callout(fork, Anchor::ForkButton, cx);

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(size::READOUT_GAP))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(size::CONTROL_GAP))
                    .child(start)
                    .child(previous)
                    .child(next)
                    .child(divergence)
                    .child(failure),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(size::READOUT_GAP))
                    .child(readouts)
                    .child(fork),
            )
    }

    /// The width of the step column: the run's longest step number.
    fn step_column_width(&self) -> Pixels {
        let digits = thousands(self.session().run.timeline.total).len();
        px(digits as f32 * size::MONO_CHAR_WIDTH)
    }

    /// The build log up to the playhead, newest line at the bottom and
    /// highlighted. Only the rows on screen are built.
    fn render_log(&mut self, cx: &mut Context<Self>) -> Div {
        let filter = self.log_filter;
        let count = self.session().run.timeline.line_count_at(filter, self.step);

        // Follow the playhead: when it moves, scroll the newest line to
        // the bottom; otherwise leave the user's scroll alone.
        let key = (self.step, filter);
        if self.log_followed != Some(key) && count > 0 {
            self.log_scroll
                .scroll_to_item_strict(count - 1, ScrollStrategy::Bottom);
            self.log_followed = Some(key);
        }

        let console_on = filter == LogFilter::WithConsole;
        let toggle = div()
            .id("console-toggle")
            .cursor_pointer()
            .text_color(rgb(if console_on {
                theme::AMBER
            } else {
                theme::MUTED
            }))
            .hover(|s| s.text_color(rgb(theme::TEXT)))
            .child(if console_on {
                "kernel console: on"
            } else {
                "kernel console: off"
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_console(cx)));

        let step_width = self.step_column_width();
        let selected = self.selected_range(Surface::Log);
        let list = uniform_list(
            "log",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, _cx| {
                let lines = this.session().run.timeline.lines(this.log_filter);
                let registry = this.selecting.registry.clone();
                range
                    .map(|i| {
                        let line = &lines[i];
                        let now = i + 1 == count;
                        let color = match line.tone {
                            Tone::Error => theme::RED,
                            Tone::Phase => theme::AMBER,
                            Tone::Stderr => theme::STDERR,
                            Tone::Console => theme::MUTED,
                            Tone::Normal => theme::SOFT,
                        };
                        let text = mapped(&line.text).shown;
                        let part = selected
                            .as_ref()
                            .and_then(|r| part_of_line(r, i, text.len()));
                        div()
                            .id(i)
                            .w_full()
                            .h(px(size::LOG_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(size::LOG_COLUMN_GAP))
                            .px(px(size::PANEL_PAD_X))
                            .when(now, |d| d.bg(rgb(theme::ROW_NOW)))
                            .child(
                                div()
                                    .flex_none()
                                    .w(step_width)
                                    .flex()
                                    .justify_end()
                                    .text_color(rgb(theme::FAINT))
                                    .child(thousands(line.step)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(color))
                                    .child(selectable(Surface::Log, i, text, part, &registry)),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.log_scroll)
        .flex_grow(layout::FILL)
        .min_h_0()
        .py(px(size::LIST_PAD_Y));

        let body = div()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .cursor(CursorStyle::IBeam)
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .when(count == 0, |d| {
                d.child(placeholder("No output up to this step."))
            })
            .child(list);
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_basis(relative(0.0))
            .bg(rgb(theme::PANEL))
            .child(panel_title(
                "Build log \u{b7} up to this step",
                Some(toggle.into_any_element()),
            ))
            .child(selects(body, Surface::Log, cx))
    }

    /// The middle column: processes alive at the playhead above, files
    /// written up to it below.
    fn render_middle(&mut self, cx: &mut Context<Self>) -> Div {
        let step = self.step;
        let t = &self.session().run.timeline;
        let mono = self.fonts.mono.clone();

        // The process tree, indented by depth, threads in grey. Each row
        // is one line of text, the id padded to its column, so a selection
        // copies ids and labels together.
        let rows: Vec<_> = t.alive_rows(step).collect();
        let alive = rows.len();
        let selected = self.selected_range(Surface::Processes);
        let registry = self.selecting.registry.clone();
        let procs = div()
            .id("procs")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .flex_basis(relative(0.0))
            .min_h_0()
            .overflow_y_scroll()
            .py(px(size::LIST_PAD_Y))
            .cursor(CursorStyle::IBeam)
            .font_family(mono.clone())
            .text_size(px(size::TEXT_MONO))
            .children(rows.into_iter().enumerate().map(|(i, row)| {
                let (id_color, label_color) = match row.kind {
                    RowKind::Process => (theme::BLUE, theme::TEXT),
                    RowKind::Thread => (theme::FAINT, theme::MUTED),
                };
                let text = process_row(row.tid, &row.label).shown;
                let id_end = PID_CHARS.min(text.len());
                let part = selected
                    .as_ref()
                    .and_then(|r| part_of_line(r, i, text.len()));
                let colors = [
                    (0..id_end, color_only(id_color)),
                    (id_end..text.len(), color_only(label_color)),
                ];
                div()
                    .flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .h(px(size::LIST_ROW_HEIGHT))
                    .pl(px(size::PANEL_PAD_X + row.depth as f32 * size::TREE_INDENT))
                    .pr(px(size::PANEL_PAD_X))
                    .child(
                        div().min_w_0().truncate().child(
                            selectable(Surface::Processes, i, text, part, &registry)
                                .with_highlights(colors),
                        ),
                    )
            }));
        let procs = selects(procs, Surface::Processes, cx);

        // Files, newest first: scroll back to the top when a new one lands.
        let files = t.file_count_at(step);
        if self.files_followed != Some(files) {
            self.files_scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.files_followed = Some(files);
        }
        let step_width = self.step_column_width();
        let selected = self.selected_range(Surface::Files);
        let file_list = uniform_list(
            "files",
            files,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let t = &this.session().run.timeline;
                let registry = this.selecting.registry.clone();
                range
                    .map(|i| {
                        let file = &t.files[files - 1 - i];
                        let color = match t.file_tone(file, step) {
                            FileTone::Error => theme::RED,
                            FileTone::Recent => theme::AMBER,
                            FileTone::Gone => theme::FAINT,
                            FileTone::Output => theme::GREEN_SOFT,
                            FileTone::Normal => theme::SOFT,
                        };
                        let op = match file.op {
                            FileOp::Write => "",
                            FileOp::Unlink => "rm",
                            FileOp::Rename => "mv",
                            FileOp::Output => "out",
                        };

                        // A click opens the file in the viewer; a drag or a
                        // double click selects its path instead.
                        let (path, pid) = (file.path.clone(), file.pid);
                        let open = cx.listener(move |this, e: &ClickEvent, _, cx| {
                            let selecting = this.selected_text().is_some();
                            if e.click_count() == 1 && !selecting {
                                this.open_file(path.clone(), pid, cx)
                            }
                        });
                        let text = mapped(&file.path).shown;
                        let part = selected
                            .as_ref()
                            .and_then(|r| part_of_line(r, i, text.len()));
                        div()
                            .id(i)
                            .role(Role::Button)
                            .aria_label(format!("Show {} at this step", file.path))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                            .on_click(open)
                            .w_full()
                            .h(px(size::LIST_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(size::LIST_COLUMN_GAP))
                            .px(px(size::PANEL_PAD_X))
                            .child(
                                div()
                                    .flex_none()
                                    .w(step_width)
                                    .flex()
                                    .justify_end()
                                    .text_color(rgb(theme::FAINT))
                                    .child(thousands(file.step)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(size::OP_COLUMN_WIDTH))
                                    .text_color(rgb(theme::FAINT))
                                    .child(op),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_start()
                                    .text_color(rgb(color))
                                    .child(selectable(Surface::Files, i, text, part, &registry)),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.files_scroll)
        .flex_grow(layout::FILL)
        .flex_basis(relative(0.0))
        .min_h_0()
        .py(px(size::LIST_PAD_Y))
        .font_family(mono.clone())
        .text_size(px(size::TEXT_MONO));
        let file_list = selects(file_list, Surface::Files, cx);

        let count = |n: usize| div().font_family(mono.clone()).child(thousands(n as u64));
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_basis(relative(0.0))
            .bg(rgb(theme::PANEL))
            .child(panel_title(
                "Process tree \u{b7} alive",
                Some(count(alive).into_any_element()),
            ))
            .child(procs)
            .child(
                div()
                    .border_t_1()
                    .border_color(rgb(theme::LINE_SOFT))
                    .child(panel_title(
                        "Files \u{b7} newest first",
                        Some(count(files).into_any_element()),
                    )),
            )
            .when(files == 0, |d| {
                d.child(placeholder("No files written up to this step."))
            })
            .child(file_list)
    }

    /// The right column: the last event at the playhead, the divergence
    /// and fork cards when they apply, and the inspect buttons.
    fn render_at_step(&mut self, cx: &mut Context<Self>) -> Div {
        let session = self.session();
        let t = &session.run.timeline;
        let step = self.step;
        let mono = self.fonts.mono.clone();

        // The last event at or before the playhead, as a system call: a
        // line about where it happened, then the call itself.
        let registry = self.selecting.registry.clone();
        let event_range = self.selected_range(Surface::EventCard);
        let event_lines = self.event_card_lines();
        let event_line = |line: usize| {
            let text = event_lines
                .get(line)
                .map(|m| m.shown.clone())
                .unwrap_or_default();
            let part = event_range
                .as_ref()
                .and_then(|r| part_of_line(r, line, text.len()));
            selectable(Surface::EventCard, line, text, part, &registry)
        };
        let event_card = match t
            .event_index_at(step)
            .and_then(|i| t.event(i).map(|e| (i, e)))
        {
            None => card(theme::RAISED, theme::LINE)
                .child(div().text_color(rgb(theme::MUTED)).child(event_line(0))),
            Some((index, event)) => {
                let described = describe::describe(event);
                let diverged_here = session
                    .comparison
                    .as_ref()
                    .and_then(|c| c.point.as_ref())
                    .and_then(|p| p.here)
                    .is_some_and(|here| here.index == index);
                let color = if diverged_here {
                    theme::BLUE_SOFT
                } else if described.tone == EventTone::Error {
                    theme::RED
                } else {
                    theme::TEXT
                };
                card(theme::RAISED, theme::LINE)
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(event_line(0)),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(size::TEXT_EVENT))
                            .text_color(rgb(color))
                            .child(event_line(1)),
                    )
                    .when_some(file_named_by(event), |card, path| {
                        // An event on a file links to the file at this step.
                        let pid = event.pid;
                        let label = format!(
                            "Show {} at this step",
                            describe::clip(&path, MAX_NAME_CHARS)
                        );
                        card.child(
                            div()
                                .id("show-event-file")
                                .role(Role::Button)
                                .cursor_pointer()
                                .text_color(rgb(theme::AMBER))
                                .hover(|s| s.text_color(rgb(theme::AMBER_HI)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_file(path.clone(), pid, cx)
                                })),
                        )
                    })
            }
        };
        let event_card = selects(
            event_card.cursor(CursorStyle::IBeam),
            Surface::EventCard,
            cx,
        );

        let mut column = div()
            .id("at-step")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(size::SECTION_GAP))
            .p(px(size::PANEL_PAD_X))
            .child(self.with_callout(event_card, Anchor::EventCard, cx));

        // Against the compared run: where the two first part, once the
        // playhead is past it, or that they never do.
        if self.shown_agreement().is_some() {
            let range = self.selected_range(Surface::Divergence);
            let lines = self.divergence_lines();
            let mut divergence =
                card(theme::BLUE_CARD, theme::BLUE_BORDER).cursor(CursorStyle::IBeam);
            for (i, line) in lines.into_iter().enumerate() {
                let part = range
                    .as_ref()
                    .and_then(|r| part_of_line(r, i, line.shown.len()));
                let text = selectable(Surface::Divergence, i, line.shown, part, &registry);
                divergence = divergence.child(if i == 0 {
                    card_title(theme::BLUE_SOFT).child(text)
                } else {
                    div().text_color(rgb(theme::SOFT)).child(text)
                });
            }
            column = column.child(selects(divergence, Surface::Divergence, cx));
        }

        // The latest fork made from the playhead.
        let fork_lines = self.fork_card_lines();
        if !fork_lines.is_empty() {
            let range = self.selected_range(Surface::ForkCard);
            let mut fork = card(theme::AMBER_CARD, theme::AMBER_DEEP).cursor(CursorStyle::IBeam);
            for (i, line) in fork_lines.into_iter().enumerate() {
                let part = range
                    .as_ref()
                    .and_then(|r| part_of_line(r, i, line.shown.len()));
                let text = selectable(Surface::ForkCard, i, line.shown, part, &registry);
                fork = fork.child(if i == 0 {
                    card_title(theme::AMBER_PALE).child(text)
                } else {
                    div().text_color(rgb(theme::SOFT)).child(text)
                });
            }
            column = column.child(selects(fork, Surface::ForkCard, cx));
        }

        // Inspect: engine actions at the playhead, two by two.
        let diff_label = match &session.other {
            Some(other) => format!(
                "Diff vs {}",
                describe::clip(&other.label(), MAX_NAME_CHARS / 2)
            ),
            None => "Diff vs other run".to_string(),
        };
        let can_diff = if session.other.is_some() {
            Availability::Enabled
        } else {
            Availability::Disabled
        };
        let inspect = |id: &'static str, label: String, availability: Availability| {
            button(id, ButtonStyle::Neutral, availability)
                .flex_1()
                .min_w_0()
                .h(px(size::INSPECT_BUTTON_HEIGHT))
                .child(div().truncate().child(label))
        };
        let grid = div()
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP))
            .child(
                div()
                    .flex()
                    .gap(px(size::CARD_GAP))
                    .child(
                        inspect("gdb", "Attach gdb".into(), Availability::Enabled).on_click(
                            cx.listener(|this, _, window, cx| this.attach_gdb(window, cx)),
                        ),
                    )
                    .child(
                        inspect("shell", "Open shell".into(), Availability::Enabled).on_click(
                            cx.listener(|this, _, window, cx| this.open_shell(window, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(size::CARD_GAP))
                    .child(
                        inspect("diff", diff_label, can_diff)
                            .on_click(cx.listener(|this, _, _, cx| this.diff_runs(cx))),
                    )
                    .child(
                        inspect("export", "Export run".into(), Availability::Enabled)
                            .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
                    ),
            );
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap(px(size::CARD_GAP))
                .child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child("INSPECT"),
                )
                .child(grid),
        );

        div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_basis(relative(0.0))
            .bg(rgb(theme::PANEL))
            .child(panel_title("At this step", None))
            .child(column)
    }

    /// Notices stacked over the bottom right corner, newest last.
    fn render_notices(&self, cx: &mut Context<Self>) -> Div {
        let mut stack = div()
            .absolute()
            .right(px(size::NOTICE_INSET))
            .bottom(px(size::NOTICE_INSET))
            .w(px(size::NOTICE_WIDTH))
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP));
        let registry = self.selecting.registry.clone();
        for notice in &self.notices {
            let surface = Surface::Notice(notice.id);
            let range = self.selected_range(surface);
            let line = |i: usize, text: String| {
                let part = range.as_ref().and_then(|r| part_of_line(r, i, text.len()));
                selectable(surface, i, text, part, &registry)
            };
            let (border, title_color) = match notice.tone {
                NoticeTone::Info => (theme::AMBER_DEEP, theme::AMBER_PALE),
                NoticeTone::Error => (theme::RED_BORDER, theme::RED_SOFT),
            };
            let id = notice.id;
            let close = div()
                .id(SharedString::from(format!("close-notice-{id}")))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .flex_none()
                .cursor_pointer()
                .text_color(rgb(theme::MUTED))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
                .on_click(cx.listener(move |this, _, _, cx| this.dismiss(id, cx)));
            let body = div()
                .flex()
                .flex_col()
                .gap(px(size::CARD_GAP / 2.0))
                .cursor(CursorStyle::IBeam)
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap(px(size::CARD_GAP))
                        .child(card_title(title_color).child(line(0, notice.title.to_string())))
                        .child(close),
                )
                .children(notice.body.lines().enumerate().map(|(i, text)| {
                    div()
                        .text_size(px(size::TEXT_CARD_TITLE))
                        .text_color(rgb(theme::SOFT))
                        .child(line(i + 1, mapped(text).shown))
                }));
            stack = stack.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(size::CARD_GAP / 2.0))
                    .p(px(size::NOTICE_PAD))
                    .rounded(px(size::RADIUS_CARD))
                    .bg(rgb(theme::RAISED))
                    .border_1()
                    .border_color(rgb(border))
                    .shadow_lg()
                    .child(selects(body, surface, cx))
                    .when(!notice.actions.is_empty(), |d| {
                        d.child(self.render_notice_actions(id, &notice.actions, cx))
                    }),
            );
        }
        stack
    }

    /// A notice's actions as a row of small buttons, the first one
    /// filled.
    fn render_notice_actions(
        &self,
        id: u64,
        actions: &[NoticeAction],
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div()
            .flex()
            .flex_wrap()
            .gap(px(size::CONTROL_GAP))
            .pt(px(size::CARD_GAP / 2.0));
        for (i, action) in actions.iter().enumerate() {
            let style = if i == 0 {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Neutral
            };
            let action = action.clone();
            row = row.child(
                button(
                    SharedString::from(format!("notice-{id}-{i}")),
                    style,
                    Availability::Enabled,
                )
                .h(px(size::NOTICE_BUTTON_HEIGHT))
                .child(action.label())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run_action(id, action.clone(), window, cx)
                })),
            );
        }
        row
    }

    /// Before a run is open: the mark, a line of explanation, buttons to
    /// open a file or a link, and the runs recorded here most recently.
    fn render_empty(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let status = match &self.loading {
            Some(path) => format!("Opening {}\u{2026}", path.display()),
            None => "Open a recorded run to scrub through it, or look around an example first."
                .to_string(),
        };
        let open_file = button("open-file", ButtonStyle::Neutral, Availability::Enabled)
            .child("Open\u{2026}")
            .on_click(cx.listener(|this, _, _, cx| this.prompt_open(cx)));
        let open_link = button("open-link", ButtonStyle::Neutral, Availability::Enabled)
            .child("Open link")
            .on_click(cx.listener(|this, _, window, cx| this.open_link_dialog(window, cx)));

        // The header, with only the mark on the left, is still the title
        // bar.
        let brand = div()
            .flex()
            .items_center()
            .gap(px(size::BRAND_GAP))
            .child(icon(Icon::Mark, size::MARK_ICON, theme::AMBER))
            .child(
                div()
                    .text_size(px(size::TEXT_BRAND))
                    .font_weight(FontWeight::BOLD)
                    .child("Rewind"),
            );
        let header = self.header_frame(brand, None, window, cx);

        let middle = div()
            .flex_grow(layout::FILL)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(size::EMPTY_GAP))
            .child(icon(Icon::Mark, size::EMPTY_MARK_ICON, theme::AMBER))
            .child(
                div()
                    .text_size(px(size::TEXT_EMPTY_TITLE))
                    .font_weight(FontWeight::BOLD)
                    .child("Rewind"),
            )
            .child(div().text_color(rgb(theme::SOFT)).child(status))
            .child(
                div()
                    .flex()
                    .gap(px(size::CONTROL_GAP))
                    .child(explore_button(cx))
                    .child(open_file)
                    .child(open_link),
            )
            .when(!self.recent.is_empty(), |d| d.child(self.render_recent(cx)))
            .child(
                div()
                    .font_family(self.fonts.mono.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child("rewind-app <run-dir | run.rwd | URL | trace> [--compare <run>]    F1 starts the tour"),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(middle)
    }
}

impl Scrubber {
    /// The families of runs that changed last, newest first, one row
    /// each however many forks they hold: the base run's ending, id and
    /// title, how many runs and how they ended, and when one last changed.
    /// A row opens the base run.
    fn render_recent(&self, cx: &mut Context<Self>) -> Div {
        let now = std::time::SystemTime::now();
        let mut list = card(theme::PANEL, theme::LINE)
            .w(px(size::RECENT_WIDTH))
            .child(card_title(theme::MUTED).child("Recent runs"));
        for (i, family) in self.recent.iter().enumerate() {
            let base = family.base();
            let when = now
                .duration_since(family.modified())
                .map(crate::describe::ago)
                .unwrap_or_default();
            let summary = (family.runs.len() > 1).then(|| family.summary());
            let family = family.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("recent-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(size::CONTROL_GAP))
                    .px(px(size::RECENT_ROW_PAD_X))
                    .py(px(size::RECENT_ROW_PAD_Y))
                    .rounded(px(size::RADIUS_CARD))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                    .child(
                        div()
                            .flex_none()
                            .w(px(size::RECENT_ENDING_WIDTH))
                            .child(pill(base.ending.clone(), ending_tone(base), &self.fonts)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(self.fonts.mono.clone())
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(short_id(&base.id)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_grow(layout::FILL)
                            .min_w_0()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(rgb(theme::SOFT))
                                    .child(base.title.clone()),
                            )
                            .when_some(summary, |d, summary| {
                                d.child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_size(px(size::TEXT_SMALL))
                                        .text_color(rgb(theme::MUTED))
                                        .child(summary),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(when),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.open_family(&family, cx))),
            );
        }
        list
    }

    /// The Runs panel: the family of the run on screen as a tree, the run
    /// on screen marked. A row opens its run, compared with the run it
    /// was forked from. The title offers to remove forks that repeat an
    /// older fork's trace.
    fn render_runs(&self, family: &Family, cx: &mut Context<Self>) -> Div {
        let rows = family.rows();
        let count = rows.len();
        let shown_id = self.session().run.manifest.id.clone().unwrap_or_default();
        let identical = family.identical();
        let prune = (identical > 0).then(|| {
            let label = if self.pruning {
                "removing\u{2026}".to_string()
            } else {
                format!("remove {identical} identical")
            };
            div()
                .id("prune-identical")
                .cursor_pointer()
                .text_color(rgb(theme::AMBER))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .child(label)
                .on_click(cx.listener(|this, _, _, cx| this.prune_identical(cx)))
                .into_any_element()
        });

        // Every row's graph is as wide as the deepest row's, so the dots
        // and the text after them line up.
        let columns = rows.iter().map(|r| r.depth + 1).max().unwrap_or(1);
        let fonts = self.fonts.clone();
        let list = uniform_list(
            "runs",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let row = &rows[i];
                        let shown = row.run.id == shown_id;
                        let run = row.run.clone();
                        div()
                            .id(i)
                            .w_full()
                            .h(px(size::RUNS_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(size::CONTROL_GAP))
                            .pl(px(size::PANEL_PAD_X))
                            .pr(px(size::PANEL_PAD_X))
                            .cursor_pointer()
                            .when(shown, |d| d.bg(rgb(theme::ROW_NOW)))
                            .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                            .child(graph_cell(row, columns, shown))
                            .child(pill(row.run.ending.clone(), ending_tone(&row.run), &fonts))
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(fonts.mono.clone())
                                    .text_color(rgb(if shown { theme::AMBER } else { theme::SOFT }))
                                    .child(short_id(&row.run.id)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(size::TEXT_SMALL))
                                    .text_color(rgb(theme::MUTED))
                                    .child(row.detail()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_family_run(run.clone(), cx)
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.runs_scroll)
        .flex_grow(layout::FILL)
        .min_h_0()
        .py(px(size::LIST_PAD_Y));

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(size::RUNS_PANEL_WIDTH))
            .min_h_0()
            .bg(rgb(theme::PANEL))
            .child(panel_title(
                &format!("Runs of this build \u{b7} {count}"),
                prune,
            ))
            .child(list)
    }
}

/// One row of the family graph, drawn as ISL and Jujutsu draw a history:
/// the lines passing through, the parent's line with a curve off it to
/// this run's dot, this run's own line down to its forks, and the dot,
/// colored by how the run ended and ringed when it is the run on screen.
fn graph_cell(row: &Row, columns: usize, shown: bool) -> impl IntoElement {
    let graph = row.graph.clone();
    let depth = row.depth;
    let dot_color = if row.run.failed {
        theme::RED
    } else if row.run.ending == "exited:0" {
        theme::GREEN_SOFT
    } else {
        theme::MUTED
    };
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let lane = px(size::GRAPH_LANE);
            let x = |column: usize| bounds.left() + lane * column as f32 + lane / 2.0;
            let (top, bottom) = (bounds.top(), bounds.bottom());
            let mid = top + (bottom - top) / 2.0;
            let curve = px(size::GRAPH_CURVE);
            let dot = px(size::GRAPH_DOT);
            let line_color = rgb(theme::GRAPH_LINE);
            let mut stroke =
                |points: &[Point<Pixels>], bend: Option<(Point<Pixels>, Point<Pixels>)>| {
                    let mut path = PathBuilder::stroke(px(size::GRAPH_STROKE));
                    path.move_to(points[0]);
                    if let Some((to, ctrl)) = bend {
                        path.curve_to(to, ctrl);
                    }
                    for p in &points[1..] {
                        path.line_to(*p);
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, line_color);
                    }
                };

            // Lines of ancestors with forks still to come pass through.
            for (column, through) in graph.through.iter().enumerate() {
                if *through {
                    stroke(&[point(x(column), top), point(x(column), bottom)], None);
                }
            }

            // The parent's line comes down and curves off to this dot, and
            // goes on unless this is its last fork.
            if depth > 0 {
                let parent = x(depth - 1);
                let end = if graph.last { mid - curve } else { bottom };
                stroke(&[point(parent, top), point(parent, end)], None);
                stroke(
                    &[point(parent, mid - curve), point(x(depth) - dot, mid)],
                    Some((point(parent + curve, mid), point(parent, mid))),
                );
            }

            // This run's line, down to its forks.
            if graph.has_forks {
                stroke(&[point(x(depth), mid), point(x(depth), bottom)], None);
            }

            // The dot, and a ring around the run on screen.
            let center = point(x(depth), mid);
            let circle = |radius: Pixels| {
                Bounds::new(
                    point(center.x - radius, center.y - radius),
                    gpui::size(radius * 2.0, radius * 2.0),
                )
            };
            window.paint_quad(fill(circle(dot), rgb(dot_color)).corner_radii(dot));
            if shown {
                let ring = px(size::GRAPH_RING);
                window.paint_quad(
                    fill(circle(ring), gpui::transparent_black())
                        .corner_radii(ring)
                        .border_widths(px(size::GRAPH_STROKE))
                        .border_color(rgb(theme::AMBER)),
                );
            }
        },
    )
    .flex_none()
    .w(px(size::GRAPH_LANE * columns as f32))
    .h_full()
}

/// The pill tone for how a run ended.
fn ending_tone(run: &RunEntry) -> PillTone {
    if run.failed {
        PillTone::Failed
    } else if run.ending == "exited:0" {
        PillTone::Passed
    } else {
        PillTone::Quiet
    }
}

/// A rounded card with a border.
fn card(bg: u32, border: u32) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(size::CARD_GAP))
        .p(px(size::CARD_PAD))
        .rounded(px(size::RADIUS_CARD))
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
}

/// A card's title line; the caller adds its text.
fn card_title(color: u32) -> Div {
    div()
        .text_size(px(size::TEXT_CARD_TITLE))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color))
}

/// A highlight that only colors text.
fn color_only(color: u32) -> HighlightStyle {
    HighlightStyle {
        color: Some(rgb(color).into()),
        ..Default::default()
    }
}

/// Muted text standing in for an empty list.
fn placeholder(text: &'static str) -> Div {
    div()
        .px(px(size::PANEL_PAD_X))
        .py(px(size::LIST_PAD_Y))
        .text_color(rgb(theme::MUTED))
        .child(text)
}

/// The file an event wrote, deleted or renamed onto, if it names one.
fn file_named_by(event: &rewind_trace::Event) -> Option<String> {
    match &event.kind {
        rewind_trace::EventKind::Open { path, .. } | rewind_trace::EventKind::Unlink { path } => {
            Some(path.clone())
        }
        rewind_trace::EventKind::Rename { to, .. } => Some(to.clone()),
        _ => None,
    }
}

/// The text of the cards, notices and lists, built once for both drawing
/// and copying so the two always agree.
impl Scrubber {
    /// The process tree rows alive at the playhead.
    pub(super) fn process_lines(&self) -> Vec<Mapped> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        session
            .run
            .timeline
            .alive_rows(self.step)
            .map(|row| process_row(row.tid, &row.label))
            .collect()
    }

    /// The files written up to the playhead, newest first.
    pub(super) fn file_lines(&self) -> Vec<Mapped> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let t = &session.run.timeline;
        let count = t.file_count_at(self.step);
        (0..count)
            .map(|i| mapped(&t.files[count - 1 - i].path))
            .collect()
    }

    /// The event card: where the last event happened, then the event.
    pub(super) fn event_card_lines(&self) -> Vec<Mapped> {
        const NOTHING_YET: &str = "Nothing has happened yet at this step.";
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let t = &session.run.timeline;
        let Some(event) = t.event_index_at(self.step).and_then(|i| t.event(i)) else {
            return vec![Mapped::plain(NOTHING_YET)];
        };
        let mut meta = format!(
            "last event \u{b7} step {} \u{b7} pid {}",
            thousands(event.step),
            event.pid
        );
        if event.tid != event.pid {
            meta.push_str(&format!(" \u{b7} tid {}", event.tid));
        }
        if let Some(name) = t.name_of(event.pid) {
            meta.push_str(&format!(" \u{b7} {}", describe::clip(name, MAX_NAME_CHARS)));
        }
        vec![Mapped::plain(meta), mapped(&describe::describe(event).text)]
    }

    /// Where the run parts from the one it is compared with, once the
    /// playhead is past it, or that the two never part.
    fn shown_agreement(&self) -> Option<(String, Vec<String>)> {
        match self.session.as_ref()?.agreement()? {
            Agreement::Parted {
                step: at,
                title,
                lines,
            } if self.step >= at => Some((title, lines)),
            Agreement::Same { title, lines } => Some((title, lines)),
            Agreement::Parted { .. } => None,
        }
    }

    /// The divergence card: its title, then its lines.
    pub(super) fn divergence_lines(&self) -> Vec<Mapped> {
        let Some((title, lines)) = self.shown_agreement() else {
            return Vec::new();
        };
        std::iter::once(Mapped::plain(title))
            .chain(lines.iter().map(|l| mapped(l)))
            .collect()
    }

    /// The fork card for the latest fork made from the playhead.
    pub(super) fn fork_card_lines(&self) -> Vec<Mapped> {
        let Some(fork) = self.forks.last() else {
            return Vec::new();
        };
        let step = thousands(fork.step);
        let schedule = fork.schedule;
        let (title, body) = match &fork.state {
            ForkState::Pending => (
                format!("Forking at step {step} with schedule {schedule}"),
                "The engine is running the branch; it takes about as long as the run did."
                    .to_string(),
            ),
            ForkState::Created(forked) => (
                format!(
                    "Forked at step {step} with schedule {schedule} \u{2192} run {}",
                    short_id(&forked.id)
                ),
                forked.summary.clone(),
            ),
            ForkState::Failed(error) => (
                format!("Fork at step {step} with schedule {schedule} failed"),
                error.clone(),
            ),
        };
        vec![Mapped::plain(title), mapped(&body)]
    }
}
