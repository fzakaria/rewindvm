//! The Threads tab: a lane per thread over a window of steps, a bar where
//! the thread held the CPU, this run above the compared run in each lane.
//!
//! Show threads opens it in place of "At this step", as Show source opens
//! the source. Each run's lanes are read from the context switches in its
//! trace, which the app holds, as `rewind threads` reads them: nothing is
//! replayed and no engine is asked, so the lanes are there at every pan
//! and zoom, and for a run that is only a trace. The window opens
//! centered on where the two runs part, or on the playhead without a
//! compared run, and follows the playhead from then on: it holds still
//! while the playhead moves inside it, and centers on the playhead again,
//! at the width it has, when a move takes the playhead out of it. Ctrl and
//! the wheel zoom it around the pointer, Shift and the wheel pan it, and
//! the zoom out and in buttons zoom it around its center. A window placed
//! around a step never reaches past the run's last step while the run is
//! longer than it. Each run's switches are read only for the steps not
//! read yet, half a window more on each side.
//!
//! The compared run's window is lined up with this run's by a fixed
//! offset. Two runs whose schedules part at a step are the same machine
//! until it, so the offset is 0; other runs are paired by their events, as
//! the Compare tab pairs them.

use gpui::{
    Context, Div, ScrollWheelEvent, SharedString, Stateful, div, prelude::*, px, relative, rgb,
    rgba,
};

use crate::describe::{self, thousands};
use crate::lanes::{Lane, Parted, Row, Side, Slice, Window, first_switch, held, rows};
use crate::run::{Run, Session};
use crate::selection::{Mapped, Surface, part_of_line};
use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::Scrubber;
use crate::ui::selectable::{mapped, selectable, selects};
use crate::ui::splits::measure;
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{Availability, ButtonStyle, button, icon, panel_title, tooltip};
use crate::ui::zoom::{PAN_PER_NOTCH, wheel_notches};
use crate::view::ZOOM_IN;
use rewind_trace::slices::{Exec, execs};

/// How far either side of its center a new tab's window reaches.
const DEFAULT_HALF: u64 = 64;

/// How much one press of − or + zooms the window.
const BUTTON_ZOOM: f64 = 0.5;

/// What the tab says for a run whose guest kernel wrote no switch records,
/// as `rewind threads` says it.
const NO_SWITCHES: &str = "This run was recorded by a guest kernel that did not record context switches. Record it again with this Rewind to see its threads.";
const OTHER_NO_SWITCHES: &str = "The compared run was recorded by a guest kernel that did not record context switches. Record it again with this Rewind to see the threads of both.";

/// The width of the column of lane labels.
const LABEL_WIDTH: f32 = 170.0;

/// The height of one run's lane in a row, and the bar inside it.
const LANE_HEIGHT: f32 = 16.0;
const BAR_HEIGHT: f32 = 10.0;

/// The narrowest a bar is drawn, so a slice of one step at a wide window
/// still shows.
const MIN_BAR_WIDTH: f32 = 2.0;

/// The width of an event's notch, the markers' lines, and the run marker
/// left of each lane. A notch sits at the top of the lane, so a thread
/// that writes at every step still shows its bar.
const TICK_WIDTH: f32 = 1.0;
const TICK_HEIGHT: f32 = 4.0;
const MARKER_WIDTH: f32 = 2.0;
const RUN_MARK_WIDTH: f32 = 3.0;

/// The width of the −/+ buttons.
const ZOOM_BUTTON_WIDTH: f32 = 28.0;

/// The most events of a slice its card lists.
const MAX_SLICE_EVENTS: usize = 8;

/// The first step another thread held the CPU is drawn fainter than the
/// markers from the timeline.
const SWITCH_OPACITY: f32 = 0.6;

/// The hatching over steps past a run's end.
const ENDED_A: u32 = 0xffffff0f;

const ZOOM_NOTE: &str = "The lanes show this many steps either side of the window's center. Ctrl and the wheel over the lanes zoom around the pointer, Shift and the wheel pan, and the wheel alone scrolls the threads.";
const ZOOM_IN_NOTE: &str =
    "Zoom in: fewer steps, each wider. Ctrl and the wheel zoom around the pointer.";
const ZOOM_OUT_NOTE: &str =
    "Zoom out: more steps, each narrower. Ctrl and the wheel zoom around the pointer.";
const NOTCH_NOTE: &str = "A notch at the top of a lane is one of the thread's events: a write, an open, a fork, an exit or a signal. A bar is the steps the thread held the CPU between them.";
const SWITCH_NOTE: &str = "The first place in the two windows where a different thread held the CPU, which can come before the first event that differs.";
const DIVERGENCE_NOTE: &str = "The first event where this run differs from the compared run, the solid blue mark on the timeline.";

/// One run's slices: the ones read from its switches so far, over the
/// steps `range` covers, and the run's execs, which rename its tasks.
#[derive(Default)]
pub struct Held {
    pub slices: Vec<Slice>,
    pub range: Option<(u64, u64)>,
    execs: Vec<Exec>,
}

impl Held {
    /// Nothing read yet of `run`.
    fn of(run: &Run) -> Held {
        Held {
            execs: execs(&run.timeline.trace.events),
            ..Held::default()
        }
    }

    /// Reads `run`'s slices around `window` when the ones read do not
    /// cover it.
    fn read(&mut self, run: &Run, window: Window) {
        let last = run.timeline.total;
        if window.covered_by(self.range, last) {
            return;
        }
        let (from, to) = window.fetch_range();
        let to = to.min(last);
        self.slices = held(&run.timeline.trace.switches, &self.execs, from, to);
        self.range = Some((from, to));
    }
}

/// A bar picked by a click: its run and the slice's first step there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picked {
    pub side: Side,
    pub lane: Lane,
    pub from: u64,
}

/// The open tab.
pub struct LanesPanel {
    /// This run's window.
    pub window: Window,
    /// The compared run's window's center less this one's, while a run is
    /// compared.
    pub offset: Option<i64>,
    /// Why the tab has no lanes, when it has none: a run recorded without
    /// switch records.
    pub unavailable: Option<&'static str>,
    pub here: Held,
    pub there: Held,
    pub picked: Option<Picked>,
}

impl LanesPanel {
    /// The compared run's window, while a run is compared.
    fn other_window(&self) -> Option<Window> {
        self.offset.map(|by| self.window.shifted(by))
    }

    /// Both runs' rows, once each run has some slices: a row per thread
    /// that held the CPU inside the windows, not in the steps read around
    /// them.
    fn rows(&self) -> Option<Vec<Row>> {
        self.here.range?;
        let inside = |slices: &[Slice], window: Window| -> Vec<Slice> {
            slices
                .iter()
                .filter(|s| window.overlaps(s.from, s.to))
                .cloned()
                .collect()
        };
        let there = match self.other_window() {
            None => Vec::new(),
            Some(window) => {
                self.there.range?;
                inside(&self.there.slices, window)
            }
        };
        Some(rows(&inside(&self.here.slices, self.window), &there))
    }

    /// What the tab says in place of the lanes: why it has none.
    fn message(&self) -> Option<String> {
        self.unavailable.map(str::to_string)
    }

    /// Reads both runs' slices around their windows, where the ones read
    /// do not cover them.
    fn read(&mut self, session: &Session) {
        if self.unavailable.is_some() {
            return;
        }
        self.here.read(&session.run, self.window);
        if let (Some(other), Some(window)) = (&session.other, self.other_window()) {
            self.there.read(other, window);
        }
    }
}

/// Why `session`'s runs have no lanes, when they have none: a run whose
/// guest kernel wrote no switch records has nothing to read them from.
fn no_lanes(session: &Session) -> Option<&'static str> {
    let without = |run: &Run| run.timeline.trace.switches.is_empty();
    if without(&session.run) {
        return Some(NO_SWITCHES);
    }
    session
        .other
        .as_ref()
        .is_some_and(without)
        .then_some(OTHER_NO_SWITCHES)
}

/// The offset of the compared run's window from this run's, centered on
/// `center`.
fn offset_of(session: &Session, center: u64) -> Option<i64> {
    session.other.as_ref()?;

    // Runs whose schedules part are the same machine until then, and are
    // lined up step for step.
    if session.split.is_some() {
        return Some(0);
    }
    let parted = session
        .comparison
        .as_ref()
        .and_then(|c| c.point.as_ref())
        .map(|p| Parted {
            this: p.step,
            other: p.other_step,
        });
    crate::lanes::offset(center, parted, session.matching_step(center))
}

impl Scrubber {
    /// Opens the Threads tab, centered where the two runs part, else on
    /// the playhead, with both runs' threads read from their switches.
    pub(super) fn open_lanes(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let center = session.divergence_step().unwrap_or(self.step);
        let window = Window::around(center, DEFAULT_HALF, self.lanes_last());
        self.lanes = Some(self.new_lanes(window));
        self.right_tab = RightTab::Threads;
        cx.notify();
    }

    /// The last step a window may reach: the later end of the two runs.
    fn lanes_last(&self) -> u64 {
        let Some(session) = &self.session else {
            return 0;
        };
        session
            .other
            .as_ref()
            .map_or(0, |o| o.timeline.total)
            .max(session.run.timeline.total)
    }

    /// Brings the window to the playhead when the playhead has left it,
    /// while the Threads tab is on screen: the playhead moved, or the tab
    /// was chosen after it moved. A playhead still inside the window
    /// leaves the window where it is, so the lanes hold still under a
    /// walk from step to step and under a click on a bar.
    pub(super) fn lanes_playhead_moved(&mut self, cx: &mut Context<Self>) {
        let inside = self
            .lanes
            .as_ref()
            .is_some_and(|l| l.window.at(self.step).is_some());
        if inside {
            return;
        }
        self.center_lanes_on_playhead(cx);
    }

    /// Centers the window on the playhead, at the width it has.
    fn center_lanes_on_playhead(&mut self, cx: &mut Context<Self>) {
        let Some(half) = self.lanes.as_ref().map(|l| l.window.half) else {
            return;
        };
        let window = Window::around(self.step, half, self.lanes_last());
        self.move_lanes(window, cx);
    }

    /// A tab over `window`, with both runs' slices read around it.
    fn new_lanes(&mut self, window: Window) -> LanesPanel {
        let session = self.session();
        let mut lanes = LanesPanel {
            window,
            offset: offset_of(session, window.center_step()),
            unavailable: no_lanes(session),
            here: Held::of(&session.run),
            there: session.other.as_ref().map(Held::of).unwrap_or_default(),
            picked: None,
        };
        lanes.read(session);
        lanes
    }

    /// Opens the tab; shows it when it is open behind another; or closes
    /// it when it shows.
    pub(super) fn toggle_lanes(&mut self, cx: &mut Context<Self>) {
        match (&self.lanes, self.right_tab) {
            (Some(_), RightTab::Threads) => self.close_lanes(cx),
            (Some(_), _) => self.select_tab(RightTab::Threads, cx),
            (None, _) => self.open_lanes(cx),
        }
    }

    pub(super) fn close_lanes(&mut self, cx: &mut Context<Self>) {
        self.lanes = None;
        if self.right_tab == RightTab::Threads {
            self.right_tab = RightTab::AtStep;
        }
        self.clear_selection_in(&[Surface::Lanes]);
        cx.notify();
    }

    /// The tab, as the compared run is about to take this run's place on
    /// screen: the window moves onto the compared run's steps, so the
    /// lanes show the same place in both runs after the swap.
    pub(super) fn lanes_runs_swapping(&mut self) {
        let Some(lanes) = &mut self.lanes else {
            return;
        };
        let Some(offset) = lanes.offset else {
            return;
        };
        lanes.window = lanes.window.swapped(offset);
    }

    /// The tab for a session just shown: the same window over the new
    /// session's runs, read again, when the tab was open.
    pub(super) fn lanes_session_changed(&mut self) {
        let Some(lanes) = &self.lanes else {
            return;
        };
        let last = self.session().run.timeline.total as i64;
        let mut window = lanes.window;
        window.center = window.center.min(last);
        self.lanes = Some(self.new_lanes(window.from_step_0()));
    }

    /// Moves the window to `window`, and reads what it newly shows.
    fn move_lanes(&mut self, mut window: Window, cx: &mut Context<Self>) {
        // A window never moves past where both runs have ended, nor to
        // start before step 0.
        window.center = window.center.min(self.lanes_last() as i64);
        window = window.from_step_0();
        let (Some(lanes), Some(session)) = (&mut self.lanes, &self.session) else {
            return;
        };
        if lanes.window == window {
            return;
        }
        lanes.window = window;
        lanes.read(session);
        cx.notify();
    }

    /// Zooms the window by `factor` around the step `fraction` of the way
    /// across it.
    fn zoom_lanes(&mut self, fraction: f32, factor: f64, cx: &mut Context<Self>) {
        if let Some(window) = self
            .lanes
            .as_ref()
            .map(|l| l.window.zoomed(fraction, factor))
        {
            self.move_lanes(window, cx);
        }
    }

    /// The wheel over the lanes: Ctrl zooms around the pointer, Shift or a
    /// sideways turn pans, and the wheel alone is left to scroll the rows.
    fn lanes_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let (dx, dy) = wheel_notches(event.delta);
        let sideways = dx.abs() > dy.abs();
        if event.modifiers.shift || sideways {
            let along = if sideways { dx } else { dy };
            if let Some(window) = self
                .lanes
                .as_ref()
                .map(|l| l.window.panned(-along * PAN_PER_NOTCH))
            {
                self.move_lanes(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if !event.modifiers.control {
            return;
        }
        let fraction = self.measured.lanes_plot.get().map_or(0.5, |b| {
            ((event.position.x - b.origin.x) / b.size.width).clamp(0.0, 1.0)
        });
        self.zoom_lanes(fraction, ZOOM_IN.powf(dy as f64), cx);
        cx.stop_propagation();
    }

    /// A click on a bar: picks it, and moves the playhead to its first
    /// step, or for the compared run's bar, to the same place in this
    /// run's window.
    fn pick_bar(&mut self, picked: Picked, cx: &mut Context<Self>) {
        let Some(lanes) = &mut self.lanes else {
            return;
        };
        lanes.picked = Some(picked);
        let step = match picked.side {
            Side::Here => picked.from,
            Side::There => picked
                .from
                .saturating_add_signed(-lanes.offset.unwrap_or(0)),
        };
        self.clear_selection_in(&[Surface::Lanes]);
        self.jump_to(step, cx);
        cx.notify();
    }

    /// The Threads tab.
    pub(super) fn render_lanes(&self, cx: &mut Context<Self>) -> Option<Div> {
        let lanes = self.lanes.as_ref()?;
        let session = self.session();
        let window = lanes.window;
        let title = format!(
            "Threads \u{b7} steps {}\u{2013}{}",
            thousands(window.from()),
            thousands(window.to())
        );
        let mut panel = div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .flex_basis(relative(0.0))
            .flex_grow(layout::FILL)
            .bg(rgb(theme::PANEL))
            .child(panel_title(&title, None))
            .child(self.render_lanes_controls(lanes, cx));

        // A run recorded without switch records says so in place of the
        // lanes.
        let lines = self.lanes_lines();
        let registry = self.selecting.registry.clone();
        let range = self.selected_range(Surface::Lanes);
        let line = |i: usize, text: String| {
            let part = range.as_ref().and_then(|r| part_of_line(r, i, text.len()));
            selectable(Surface::Lanes, i, text, part, &registry)
        };
        if lanes.message().is_some() {
            let text = lines.first().map(|l| l.shown.clone()).unwrap_or_default();
            let note = div()
                .p(px(size::PANEL_PAD_X))
                .text_color(rgb(theme::MUTED))
                .child(line(0, text));
            return Some(panel.child(selects(note, Surface::Lanes, cx)));
        }
        let rows = lanes.rows()?;

        // The markers each lane draws: where the runs part, where their
        // threads first differ, and the playhead.
        let switch = lanes
            .other_window()
            .and_then(|w| first_switch((&lanes.here.slices, window), (&lanes.there.slices, w)));
        let markers = Markers {
            divergence: session.divergence_step(),
            switch,
            playhead: self.step,
        };

        panel = panel.child(self.render_lanes_legend(lanes, &markers));
        // The axis and the rows, where the wheel zooms and pans whether or
        // not it is over a row; a row takes the wheel first, so the rows
        // scroll under it only with neither Ctrl nor Shift held.
        let mut list = div()
            .id("lanes-rows")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_y_scroll()
            .px(px(size::PANEL_PAD_X))
            .pb(px(size::LIST_PAD_Y));
        for (i, row) in rows.iter().enumerate() {
            list = list.child(self.render_lane_row(i, row, lanes, &markers, cx));
        }
        let plot = div()
            .id("lanes-plot")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .on_scroll_wheel(
                cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.lanes_wheel(e, cx)),
            )
            .child(self.lanes_axis(window))
            .child(list);
        panel = panel.child(plot);
        if let Some(card) = self.render_picked(lanes, &rows, &lines, cx) {
            panel = panel.child(card);
        }
        Some(panel)
    }

    /// The zoom out and in buttons around the window's width, and the link
    /// that centers the window on the playhead.
    fn render_lanes_controls(&self, lanes: &LanesPanel, cx: &mut Context<Self>) -> Div {
        let zoom_button = |id: &'static str, glyph: Icon, note: &'static str, factor: f64| {
            button(id, ButtonStyle::Neutral, Availability::Enabled)
                .h(px(size::NOTICE_BUTTON_HEIGHT))
                .w(px(ZOOM_BUTTON_WIDTH))
                .px_0()
                .aria_label(note)
                .tooltip(tooltip(note))
                .child(icon(glyph, size::ICON_CHEVRON, theme::TEXT))
                .on_click(cx.listener(move |this, _, _, cx| this.zoom_lanes(0.5, factor, cx)))
        };
        let readout = div()
            .id("lanes-width")
            .min_w(px(size::MENU_WIDTH / 2.0))
            .text_center()
            .font_family(self.fonts.mono.clone())
            .text_color(rgb(theme::SOFT))
            .tooltip(tooltip(ZOOM_NOTE))
            .child(format!("\u{b1}{} steps", thousands(lanes.window.half)));
        let recenter = crate::ui::bookmarks::link("lanes-center", "Center on playhead")
            .on_click(cx.listener(move |this, _, _, cx| this.center_lanes_on_playhead(cx)));
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(size::CARD_GAP))
            .px(px(size::PANEL_PAD_X))
            .pt(px(size::LIST_PAD_Y))
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::MUTED))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(size::CARD_GAP))
                    .child(zoom_button(
                        "lanes-zoom-out",
                        Icon::ZoomOut,
                        ZOOM_OUT_NOTE,
                        1.0 / BUTTON_ZOOM,
                    ))
                    .child(readout)
                    .child(zoom_button(
                        "lanes-zoom-in",
                        Icon::ZoomIn,
                        ZOOM_IN_NOTE,
                        BUTTON_ZOOM,
                    )),
            )
            .child(recenter)
    }

    /// Which run each lane is, what a notch is, and what each marker line
    /// is, each with a note on hover.
    fn render_lanes_legend(&self, lanes: &LanesPanel, markers: &Markers) -> Div {
        let session = self.session();
        let swatch = |color: u32| {
            div()
                .flex_none()
                .w(px(RUN_MARK_WIDTH * 3.0))
                .h(px(BAR_HEIGHT))
                .rounded(px(1.0))
                .bg(rgb(color))
        };
        let item = |id: &'static str, swatch: Div, text: String, note: Option<&'static str>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(size::LEGEND_GAP * 1.5))
                .child(swatch)
                .child(text)
                .when_some(note, |d, note| d.tooltip(tooltip(note)))
        };
        let line = |color: u32| {
            div()
                .flex_none()
                .w(px(MARKER_WIDTH))
                .h(px(BAR_HEIGHT + 2.0))
                .bg(rgb(color))
        };
        let notch = div()
            .flex_none()
            .w(px(RUN_MARK_WIDTH * 3.0))
            .h(px(BAR_HEIGHT))
            .relative()
            .rounded(px(1.0))
            .bg(rgb(theme::PHASE_GREYS[3]))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(RUN_MARK_WIDTH))
                    .w(px(TICK_WIDTH))
                    .h(px(TICK_HEIGHT))
                    .bg(rgb(theme::AMBER_PALE)),
            );
        let mut legend = div()
            .flex()
            .flex_none()
            .flex_wrap()
            .gap_x(px(size::SECTION_GAP))
            .gap_y(px(size::LEGEND_GAP))
            .px(px(size::PANEL_PAD_X))
            .py(px(size::CARD_GAP))
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::MUTED))
            .child(item(
                "legend-here",
                swatch(theme::AMBER),
                format!("this run \u{b7} {}", session.run.verdict_label()),
                None,
            ));
        if let (Some(other), Some(_)) = (&session.other, lanes.offset) {
            legend = legend.child(item(
                "legend-there",
                swatch(theme::BLUE),
                format!("{} \u{b7} {}", other.label(), other.verdict_label()),
                None,
            ));
        }
        legend = legend.child(item(
            "legend-notch",
            notch,
            "an event".to_string(),
            Some(NOTCH_NOTE),
        ));
        if let Some(step) = markers.divergence {
            legend = legend.child(item(
                "legend-divergence",
                line(theme::BLUE),
                format!("runs part at {}", thousands(step)),
                Some(DIVERGENCE_NOTE),
            ));
        }
        if let Some(step) = markers.switch {
            legend = legend.child(item(
                "legend-switch",
                line(theme::BLUE_SOFT).opacity(SWITCH_OPACITY),
                format!("first other thread on the CPU at {}", thousands(step)),
                Some(SWITCH_NOTE),
            ));
        }
        legend.child(item(
            "legend-playhead",
            line(theme::AMBER),
            format!("playhead {}", thousands(markers.playhead)),
            None,
        ))
    }

    /// The step numbers along the top of the lanes, the window's first,
    /// its center and its last, over the plot whose place the wheel reads.
    fn lanes_axis(&self, window: Window) -> Div {
        div()
            .flex()
            .flex_none()
            .px(px(size::PANEL_PAD_X))
            .child(div().flex_none().w(px(LABEL_WIDTH)))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_grow(layout::FILL)
                    .justify_between()
                    .pb(px(size::LEGEND_GAP))
                    .border_b_1()
                    .border_color(rgb(theme::LINE))
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::FAINT))
                    .child(measure(|m| &m.lanes_plot, &self.measured))
                    .child(thousands(window.from()))
                    .child(thousands(window.center_step()))
                    .child(thousands(window.to())),
            )
    }

    /// One thread's row: its label, then a lane for this run and, while a
    /// run is compared, one for the compared run under it.
    fn render_lane_row(
        &self,
        index: usize,
        row: &Row,
        lanes: &LanesPanel,
        markers: &Markers,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let session = self.session();
        let label = div()
            .flex_none()
            .w(px(LABEL_WIDTH))
            .pr(px(size::LIST_COLUMN_GAP))
            .truncate()
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(match row.lane {
                Lane::Thread { .. } => theme::SOFT,
                Lane::Kernel | Lane::Idle => theme::MUTED,
            }))
            .child(row.label.clone());
        let mut stack = div()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_w_0()
            .gap(px(1.0));
        stack = stack.child(self.render_lane(
            (index, Side::Here),
            row,
            lanes.window,
            &session.run.timeline,
            markers,
            lanes,
            cx,
        ));
        if let (Some(other), Some(window)) = (&session.other, lanes.other_window()) {
            stack = stack.child(self.render_lane(
                (index, Side::There),
                row,
                window,
                &other.timeline,
                markers,
                lanes,
                cx,
            ));
        }
        div()
            .id(SharedString::from(format!("lane-row-{index}")))
            .flex()
            .items_center()
            .py(px(size::LEGEND_GAP))
            .border_b_1()
            .border_color(rgb(theme::LINE_SOFT))
            .on_scroll_wheel(
                cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.lanes_wheel(e, cx)),
            )
            .child(label)
            .child(stack)
    }

    /// One run's lane in a row: its bars, each with a note on hover, a
    /// notch at each of the thread's events, the steps past the run's end
    /// hatched, and the markers.
    #[allow(clippy::too_many_arguments)]
    fn render_lane(
        &self,
        (index, side): (usize, Side),
        row: &Row,
        window: Window,
        timeline: &crate::model::Timeline,
        markers: &Markers,
        lanes: &LanesPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let session = self.session();
        let (slices, mark, whose) = match side {
            Side::Here => (&row.here, theme::AMBER, "this run".to_string()),
            Side::There => (
                &row.there,
                theme::BLUE,
                session
                    .other
                    .as_ref()
                    .map(|o| o.label())
                    .unwrap_or_default(),
            ),
        };
        let mut lane = div()
            .relative()
            .flex_grow(layout::FILL)
            .h(px(LANE_HEIGHT))
            .child(
                div()
                    .absolute()
                    .left(px(-RUN_MARK_WIDTH * 3.0))
                    .top(px((LANE_HEIGHT - BAR_HEIGHT) / 2.0))
                    .w(px(RUN_MARK_WIDTH))
                    .h(px(BAR_HEIGHT))
                    .rounded(px(1.0))
                    .bg(rgb(mark)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(LANE_HEIGHT / 2.0))
                    .h(px(1.0))
                    .bg(rgb(theme::LINE_SOFT)),
            );

        // Past the run's end there are no steps to show.
        if let Some(end) = window.at(timeline.total + 1) {
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(end))
                    .right_0()
                    .bg(rgba(ENDED_A)),
            );
        }

        // The row's thread's events inside the window, for the notches and
        // the bars' notes.
        let events: Vec<u64> = match row.lane {
            Lane::Thread { pid, tid } => {
                let trace = &timeline.trace;
                let start = trace.index_after(window.from().saturating_sub(1));
                trace.events[start..]
                    .iter()
                    .take_while(|e| e.step <= window.to())
                    .filter(|e| e.pid == pid && e.tid == tid)
                    .map(|e| e.step)
                    .collect()
            }
            Lane::Kernel | Lane::Idle => Vec::new(),
        };

        // A bar per slice, clipped to the window.
        for (k, slice) in slices.iter().enumerate() {
            let from = slice.from.max(window.from());
            let to = slice.to.min(window.to());
            let (Some(left), Some(_)) = (window.at(from), window.at(to)) else {
                continue;
            };
            let width = (to - from + 1) as f32 * window.step_width();
            let picked = Picked {
                side,
                lane: row.lane,
                from: slice.from,
            };
            let chosen = lanes.picked == Some(picked);
            let (bg, hover) = match (row.lane, chosen) {
                (_, true) => (theme::AMBER_DEEP, theme::AMBER_DEEP),
                (Lane::Thread { .. }, false) => (theme::PHASE_GREYS[3], theme::PHASE_GREYS[4]),
                (Lane::Kernel | Lane::Idle, false) => {
                    (theme::PHASE_GREYS[0], theme::PHASE_GREYS[2])
                }
            };
            let note = bar_note(row, slice, &whose, &events, lanes.offset, side);
            let bar: Stateful<Div> = div()
                .id(SharedString::from(format!("lane-{index}-{side:?}-{k}")))
                .absolute()
                .top(px((LANE_HEIGHT - BAR_HEIGHT) / 2.0))
                .h(px(BAR_HEIGHT))
                .left(relative(left))
                .w(relative(width))
                .min_w(px(MIN_BAR_WIDTH))
                .rounded(px(1.0))
                .bg(rgb(bg))
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(hover)))
                .when(chosen, |b| b.border_1().border_color(rgb(theme::AMBER)))
                .tooltip(tooltip(note))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_bar(picked, cx)));
            lane = lane.child(bar);
        }

        // A notch at each event.
        for step in &events {
            let Some(at) = window.at(*step) else {
                continue;
            };
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .h(px(TICK_HEIGHT))
                    .left(relative(at + window.step_width() / 2.0))
                    .w(px(TICK_WIDTH))
                    .bg(rgb(theme::AMBER_PALE)),
            );
        }

        // The markers. They are steps of this run, at the same place in
        // the compared run's lane.
        let lines = [
            (markers.switch, theme::BLUE_SOFT, SWITCH_OPACITY),
            (markers.divergence, theme::BLUE, 1.0),
            (Some(markers.playhead), theme::AMBER, 1.0),
        ];
        for (step, color, opacity) in lines {
            let Some(x) = step.and_then(|s| lanes.window.at(s)) else {
                continue;
            };
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(x + window.step_width() / 2.0))
                    .w(px(MARKER_WIDTH))
                    .opacity(opacity)
                    .bg(rgb(color)),
            );
        }
        lane
    }

    /// The card under the lanes for the bar picked last: its thread, its
    /// steps, and the thread's events in them, as `lines` lists them, each
    /// a click from the playhead in this run.
    fn render_picked(
        &self,
        lanes: &LanesPanel,
        rows: &[Row],
        lines: &[Mapped],
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let picked = lanes.picked?;
        let (_, _, events) = picked_slice(self.session(), lanes, rows)?;
        let registry = self.selecting.registry.clone();
        let range = self.selected_range(Surface::Lanes);
        let line = |i: usize| {
            let text = lines.get(i).map(|l| l.shown.clone()).unwrap_or_default();
            let part = range.as_ref().and_then(|r| part_of_line(r, i, text.len()));
            selectable(Surface::Lanes, i, text, part, &registry)
        };
        let mut card = div()
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(size::LEGEND_GAP))
            .m(px(size::PANEL_PAD_X))
            .mt(px(size::CARD_GAP))
            .p(px(size::CARD_GAP + size::LEGEND_GAP))
            .rounded(px(size::RADIUS_CARD))
            .bg(rgb(theme::RAISED))
            .border_1()
            .border_color(rgb(theme::LINE))
            .text_size(px(size::TEXT_SMALL))
            .font_family(self.fonts.mono.clone())
            .child(div().text_color(rgb(theme::TEXT)).child(line(0)));
        for (i, step) in events.iter().take(MAX_SLICE_EVENTS).enumerate() {
            let step = *step;
            let row = div()
                .id(SharedString::from(format!("lane-event-{step}")))
                .text_color(rgb(theme::SOFT))
                .whitespace_nowrap()
                .truncate()
                .child(line(i + 1));
            let row = match picked.side {
                Side::Here => row
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(theme::TEXT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx))),
                Side::There => row,
            };
            card = card.child(row);
        }
        for i in events.len().min(MAX_SLICE_EVENTS) + 1..lines.len() {
            card = card.child(div().text_color(rgb(theme::MUTED)).child(line(i)));
        }
        let card = selects(card, Surface::Lanes, cx);
        if picked.side == Side::Here {
            return Some(card);
        }
        let from = picked.from;
        Some(
            card.child(
                crate::ui::bookmarks::link(
                    "lanes-show-other",
                    "Show the compared run at this step",
                )
                .font_family(self.fonts.ui.clone())
                .on_click(cx.listener(move |this, _, _, cx| this.show_other_run(Some(from), cx))),
            ),
        )
    }

    /// The tab's selectable text, in the order the render draws it: what
    /// it says in place of the lanes, or else the picked bar's card, its
    /// title, its events and how many more there are.
    pub(super) fn lanes_lines(&self) -> Vec<Mapped> {
        let Some(lanes) = &self.lanes else {
            return Vec::new();
        };
        if let Some(message) = lanes.message() {
            return vec![mapped(&message)];
        }
        let (Some(session), Some(rows)) = (&self.session, lanes.rows()) else {
            return Vec::new();
        };
        let Some((title, run, events)) = picked_slice(session, lanes, &rows) else {
            return Vec::new();
        };
        let mut lines = vec![Mapped::plain(title)];
        if events.is_empty() {
            lines.push(Mapped::plain(
                "No events: the thread ran without writing, opening or forking.",
            ));
        }
        let trace = &run.timeline.trace;
        for step in events.iter().take(MAX_SLICE_EVENTS) {
            let text = trace
                .events
                .iter()
                .find(|e| e.step == *step)
                .map(|e| describe::describe(e).text)
                .unwrap_or_default();
            lines.push(mapped(&format!("{:>7}  {text}", thousands(*step))));
        }
        if events.len() > MAX_SLICE_EVENTS {
            lines.push(Mapped::plain(format!(
                "and {} more",
                events.len() - MAX_SLICE_EVENTS
            )));
        }
        lines
    }
}

/// The picked bar's card title, its run, and the steps of its thread's
/// events in it.
fn picked_slice<'a>(
    session: &'a Session,
    lanes: &LanesPanel,
    rows: &[Row],
) -> Option<(String, &'a crate::run::Run, Vec<u64>)> {
    let picked = lanes.picked?;
    let row = rows.iter().find(|r| r.lane == picked.lane)?;
    let slices = match picked.side {
        Side::Here => &row.here,
        Side::There => &row.there,
    };
    let slice = slices.iter().find(|s| s.from == picked.from)?;
    let (run, whose) = match picked.side {
        Side::Here => (&session.run, "this run".to_string()),
        Side::There => {
            let other = session.other.as_ref()?;
            (other, other.label())
        }
    };
    let steps = slice.to - slice.from + 1;
    let title = format!(
        "{} \u{b7} {whose} \u{b7} steps {}\u{2013}{} ({} {})",
        row.label,
        thousands(slice.from),
        thousands(slice.to),
        thousands(steps),
        if steps == 1 { "step" } else { "steps" }
    );
    let trace = &run.timeline.trace;
    let start = trace.index_after(slice.from.saturating_sub(1));
    let events = trace.events[start..]
        .iter()
        .take_while(|e| e.step <= slice.to)
        .filter(|e| match picked.lane {
            Lane::Thread { pid, tid } => e.pid == pid && e.tid == tid,
            Lane::Kernel | Lane::Idle => false,
        })
        .map(|e| e.step)
        .collect();
    Some((title, run, events))
}

/// What hovering a bar says: whose and which steps, how many of the
/// thread's events, and where a click takes the playhead.
fn bar_note(
    row: &Row,
    slice: &Slice,
    whose: &str,
    events: &[u64],
    offset: Option<i64>,
    side: Side,
) -> String {
    let steps = slice.to - slice.from + 1;
    let count = events
        .iter()
        .filter(|s| (slice.from..=slice.to).contains(*s))
        .count();
    let click = match side {
        Side::Here => format!(
            "Click to move the playhead to step {}.",
            thousands(slice.from)
        ),
        Side::There => format!(
            "Click to move the playhead to the same place in this run, step {}.",
            thousands(slice.from.saturating_add_signed(-offset.unwrap_or(0)))
        ),
    };
    format!(
        "{} on the CPU, {whose}, steps {}\u{2013}{}: {} {}, {} {}. {click}",
        row.label,
        thousands(slice.from),
        thousands(slice.to),
        thousands(steps),
        if steps == 1 { "step" } else { "steps" },
        count,
        if count == 1 { "event" } else { "events" },
    )
}

/// The steps each lane marks with a line.
struct Markers {
    /// Where the two runs part, by their events.
    divergence: Option<u64>,
    /// The first step their threads on the CPU differ.
    switch: Option<u64>,
    playhead: u64,
}
