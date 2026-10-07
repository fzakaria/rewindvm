//! The Threads tab: a lane per thread over a window of steps, a bar where
//! the thread held the CPU, this run above the compared run in each lane.
//!
//! Show threads opens it in place of "At this step", as Show source opens
//! the source. The engine replays the window one step at a time in each
//! run (`rewind threads`), which takes about a second for a few thousand
//! steps, so the window stays where it was put while the playhead moves:
//! centered on where the two runs part, or on the playhead without a
//! compared run, until Center on playhead or a wider window asks again.
//!
//! The compared run's window is lined up with this run's. Two runs whose
//! schedules part at a step are the same machine until it, so their
//! windows cover the same steps; other runs are paired by their events,
//! as the Compare tab pairs them.

use gpui::{Context, Div, SharedString, Stateful, div, prelude::*, px, relative, rgb, rgba};

use crate::describe::{self, thousands};
use crate::engine::Cancel;
use crate::lanes::{Lane, Row, Side, Slice, Window, first_switch, rows};
use crate::request::Request;
use crate::run::Session;
use crate::theme::{self, layout, size};
use crate::ui::scrubber::{Replay, Scrubber, replay_unavailable};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{panel_title, spinner};

/// How far either side of its center a window reaches, as the presets
/// offer it, and the one a new tab starts with.
const HALVES: [u64; 4] = [64, 256, 1024, 4096];
const DEFAULT_HALF: u64 = 256;

/// The width of the column of lane labels.
const LABEL_WIDTH: f32 = 170.0;

/// The height of one run's lane in a row, and the bar inside it.
const LANE_HEIGHT: f32 = 16.0;
const BAR_HEIGHT: f32 = 10.0;

/// The narrowest a bar is drawn, so a slice of one step at a wide window
/// still shows.
const MIN_BAR_WIDTH: f32 = 2.0;

/// The width of an event's tick, the markers' lines, and the run marker
/// left of each lane. A tick is a notch at the top of the lane, so a
/// thread that writes at every step still shows its bar.
const TICK_WIDTH: f32 = 1.0;
const TICK_HEIGHT: f32 = 4.0;
const MARKER_WIDTH: f32 = 2.0;
const RUN_MARK_WIDTH: f32 = 3.0;

/// The most events of a slice its card lists.
const MAX_SLICE_EVENTS: usize = 8;

/// The first step another thread held the CPU is drawn fainter than the
/// markers from the timeline.
const SWITCH_OPACITY: f32 = 0.6;

/// The hatching over steps past a run's end.
const ENDED_A: u32 = 0xffffff0f;

/// One run's slices, as far as the engine has answered.
pub enum Fetch {
    Loading,
    Ready(Vec<Slice>),
    Failed(String),
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
    /// The compared run's, lined up with it, while a run is compared.
    pub other_window: Option<Window>,
    /// Why this run cannot be brought to a step, when it cannot.
    pub unavailable: Option<String>,
    pub here: Fetch,
    pub there: Fetch,
    pub picked: Option<Picked>,
    /// The request whose answers this waits on; answers to any other are
    /// dropped.
    request: Request,
    /// Stops the engine commands of the latest request while they run.
    in_flight: Vec<Cancel>,
}

impl LanesPanel {
    /// Stops the engine commands the latest request started.
    fn supersede(&mut self) {
        for cancel in self.in_flight.drain(..) {
            cancel.cancel();
        }
    }

    /// Both runs' rows, once both have answered; the compared run's
    /// slices are empty without one.
    fn rows(&self) -> Option<Vec<Row>> {
        let Fetch::Ready(here) = &self.here else {
            return None;
        };
        let there: &[Slice] = match (&self.there, self.other_window) {
            (_, None) => &[],
            (Fetch::Ready(there), Some(_)) => there,
            _ => return None,
        };
        Some(rows(here, there))
    }
}

impl Drop for LanesPanel {
    fn drop(&mut self) {
        self.supersede();
    }
}

/// The window the compared run's lanes show beside `window` of this run.
fn other_window(session: &Session, window: Window) -> Option<Window> {
    session.other.as_ref()?;
    let center = match &session.split {
        Some(_) => window.center,
        None => session.matching_step(window.center)?,
    };
    Some(Window {
        center,
        half: window.half,
    })
}

impl Scrubber {
    /// Opens the Threads tab, centered where the two runs part, else on
    /// the playhead, and asks the engine for both runs' threads.
    pub(super) fn open_lanes(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let center = session.divergence_step().unwrap_or(self.step);
        self.lanes = Some(self.new_lanes(center, DEFAULT_HALF));
        self.right_tab = RightTab::Threads;
        self.fetch_lanes(cx);
        cx.notify();
    }

    /// A tab over `half` steps either side of `center`, not yet asked for.
    fn new_lanes(&mut self, center: u64, half: u64) -> LanesPanel {
        let session = self.session();
        let window = Window { center, half };
        LanesPanel {
            window,
            other_window: other_window(session, window),
            unavailable: replay_unavailable(session, Replay::Threads, self.importing.is_some()),
            here: Fetch::Loading,
            there: Fetch::Loading,
            picked: None,
            request: self.requests.issue(),
            in_flight: Vec::new(),
        }
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
        cx.notify();
    }

    /// The tab for a session just shown: the same window over the new
    /// session's runs, asked for again, when the tab was open.
    pub(super) fn lanes_session_changed(&mut self, cx: &mut Context<Self>) {
        let Some(lanes) = &self.lanes else {
            return;
        };
        let (center, half) = (lanes.window.center, lanes.window.half);
        let center = center.min(self.session().run.timeline.total);
        self.lanes = Some(self.new_lanes(center, half));
        self.fetch_lanes(cx);
    }

    /// Moves the window to `center`, `half` steps either side, and asks
    /// again.
    fn move_lanes(&mut self, center: u64, half: u64, cx: &mut Context<Self>) {
        if let Some(lanes) = &mut self.lanes {
            lanes.supersede();
        }
        self.lanes = Some(self.new_lanes(center, half));
        self.fetch_lanes(cx);
        cx.notify();
    }

    /// Asks the engine for each run's slices over its window, each on a
    /// thread of its own.
    fn fetch_lanes(&mut self, cx: &mut Context<Self>) {
        let (Some(lanes), Some(session)) = (&mut self.lanes, &self.session) else {
            return;
        };
        if lanes.unavailable.is_some() {
            return;
        }
        let request = lanes.request;
        let mut asks = vec![(Side::Here, session.run.path.clone(), lanes.window)];
        if let (Some(other), Some(window)) = (&session.other, lanes.other_window) {
            asks.push((Side::There, other.path.clone(), window));
        }
        for (side, run, window) in asks {
            let cancel = Cancel::default();
            lanes.in_flight.push(cancel.clone());
            let engine = self.engine.clone();
            let task = crate::jobs::on_own_thread(move || {
                engine.threads(&run, window.from(), window.to(), &cancel)
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(lanes) = &mut this.lanes else {
                        return;
                    };
                    if lanes.request != request {
                        return;
                    }
                    let fetch = match result {
                        Ok(slices) => Fetch::Ready(slices),
                        Err(e) => Fetch::Failed(e.to_string()),
                    };
                    match side {
                        Side::Here => lanes.here = fetch,
                        Side::There => lanes.there = fetch,
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// A click on a bar: picks it, and moves the playhead to its first
    /// step in this run, or shows the compared run there.
    fn pick_bar(&mut self, picked: Picked, cx: &mut Context<Self>) {
        if let Some(lanes) = &mut self.lanes {
            lanes.picked = Some(picked);
        }
        match picked.side {
            Side::Here => self.jump_to(picked.from, cx),
            Side::There => cx.notify(),
        }
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

        // A run that cannot be brought to a step says why, and nothing
        // else is drawn.
        if let Some(why) = &lanes.unavailable {
            return Some(panel.child(note(why.clone())));
        }
        let failed = [&lanes.here, &lanes.there]
            .into_iter()
            .find_map(|f| match f {
                Fetch::Failed(why) => Some(why.clone()),
                _ => None,
            });
        if let Some(why) = failed {
            return Some(panel.child(note(why)));
        }
        let Some(rows) = lanes.rows() else {
            let other = session
                .other
                .as_ref()
                .map(|o| format!(" and of {}", o.label()))
                .unwrap_or_default();
            return Some(
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(size::CARD_GAP))
                        .p(px(size::PANEL_PAD_X))
                        .text_color(rgb(theme::SOFT))
                        .child(spinner("lanes-spinner", size::ICON_FORK, theme::AMBER))
                        .child(format!(
                            "Replaying {} steps of this run{other} one step at a time",
                            thousands(window.to() - window.from() + 1)
                        )),
                ),
            );
        };

        // The markers each lane draws: where the runs part, where their
        // threads first differ, and the playhead.
        let (here, there) = match (&lanes.here, &lanes.there) {
            (Fetch::Ready(here), Fetch::Ready(there)) => (here.as_slice(), there.as_slice()),
            (Fetch::Ready(here), _) => (here.as_slice(), &[][..]),
            _ => (&[][..], &[][..]),
        };
        let switch = lanes
            .other_window
            .and_then(|w| first_switch((here, window), (there, w)));
        let markers = Markers {
            divergence: session.divergence_step(),
            switch,
            playhead: self.step,
        };

        panel = panel
            .child(self.render_lanes_legend(lanes, &markers))
            .child(axis(window));
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
        panel = panel.child(list);
        if let Some(card) = self.render_picked(lanes, &rows, cx) {
            panel = panel.child(card);
        }
        Some(panel)
    }

    /// The window presets and the button that centers the window on the
    /// playhead.
    fn render_lanes_controls(&self, lanes: &LanesPanel, cx: &mut Context<Self>) -> Div {
        let window = lanes.window;
        let mut presets = div()
            .flex()
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .rounded(px(size::RADIUS_MENU_ITEM));
        for half in HALVES {
            let chosen = half == window.half;
            presets = presets.child(
                div()
                    .id(SharedString::from(format!("lanes-half-{half}")))
                    .px(px(size::MENU_ITEM_PAD_X))
                    .py(px(size::PILL_PAD_Y))
                    .cursor_pointer()
                    .font_family(self.fonts.mono.clone())
                    .bg(rgb(if chosen {
                        theme::AMBER_CARD
                    } else {
                        theme::RAISED
                    }))
                    .text_color(rgb(if chosen {
                        theme::AMBER_PALE
                    } else {
                        theme::SOFT
                    }))
                    .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                    .child(format!("\u{b1}{}", thousands(half)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let center = this.lanes.as_ref().map_or(this.step, |l| l.window.center);
                        this.move_lanes(center, half, cx);
                    })),
            );
        }
        let recenter = crate::ui::bookmarks::link("lanes-center", "Center on playhead").on_click(
            cx.listener(move |this, _, _, cx| {
                let half = this.lanes.as_ref().map_or(DEFAULT_HALF, |l| l.window.half);
                this.move_lanes(this.step, half, cx);
            }),
        );
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
                    .child("Window")
                    .child(presets),
            )
            .child(recenter)
    }

    /// Which run each lane is, and what each marker line is.
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
        let item = |swatch: Div, text: String| {
            div()
                .flex()
                .items_center()
                .gap(px(size::LEGEND_GAP * 1.5))
                .child(swatch)
                .child(text)
        };
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
                swatch(theme::AMBER),
                format!("this run \u{b7} {}", session.run.verdict_label()),
            ));
        if let (Some(other), Some(_)) = (&session.other, lanes.other_window) {
            legend = legend.child(item(
                swatch(theme::BLUE),
                format!("{} \u{b7} {}", other.label(), other.verdict_label()),
            ));
        }
        let line = |color: u32| {
            div()
                .flex_none()
                .w(px(MARKER_WIDTH))
                .h(px(BAR_HEIGHT + 2.0))
                .bg(rgb(color))
        };
        if let Some(step) = markers.divergence {
            legend = legend.child(item(
                line(theme::BLUE),
                format!("runs part at {}", thousands(step)),
            ));
        }
        if let Some(step) = markers.switch {
            legend = legend.child(item(
                line(theme::BLUE_SOFT).opacity(SWITCH_OPACITY),
                format!("first other thread on the CPU at {}", thousands(step)),
            ));
        }
        legend.child(item(
            line(theme::AMBER),
            format!("playhead {}", thousands(markers.playhead)),
        ))
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
    ) -> Div {
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
            &row.here,
            lanes.window,
            &session.run.timeline,
            markers,
            lanes,
            cx,
        ));
        if let (Some(other), Some(window)) = (&session.other, lanes.other_window) {
            stack = stack.child(self.render_lane(
                (index, Side::There),
                row,
                &row.there,
                window,
                &other.timeline,
                markers,
                lanes,
                cx,
            ));
        }
        div()
            .flex()
            .items_center()
            .py(px(size::LEGEND_GAP))
            .border_b_1()
            .border_color(rgb(theme::LINE_SOFT))
            .child(label)
            .child(stack)
    }

    /// One run's lane in a row: its bars, a tick at each of the thread's
    /// events, the steps past the run's end hatched, and the markers.
    #[allow(clippy::too_many_arguments)]
    fn render_lane(
        &self,
        (index, side): (usize, Side),
        row: &Row,
        slices: &[Slice],
        window: Window,
        timeline: &crate::model::Timeline,
        markers: &Markers,
        lanes: &LanesPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let mark = match side {
            Side::Here => theme::AMBER,
            Side::There => theme::BLUE,
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
                .on_click(cx.listener(move |this, _, _, cx| this.pick_bar(picked, cx)));
            lane = lane.child(bar);
        }

        // A tick at each event of the row's thread inside the window.
        if let Lane::Thread { pid, tid } = row.lane {
            let trace = &timeline.trace;
            let start = trace.index_after(window.from().saturating_sub(1));
            for event in trace.events[start..]
                .iter()
                .take_while(|e| e.step <= window.to())
            {
                if event.pid != pid || event.tid != tid {
                    continue;
                }
                let Some(at) = window.at(event.step) else {
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
        }

        // The markers. They are steps of this run, at the same offset in
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
    /// steps, and the thread's events in them, each a click from the
    /// playhead in this run.
    fn render_picked(
        &self,
        lanes: &LanesPanel,
        rows: &[Row],
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let picked = lanes.picked?;
        let session = self.session();
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

        // The thread's events in the slice.
        let trace = &run.timeline.trace;
        let start = trace.index_after(slice.from.saturating_sub(1));
        let events: Vec<_> = trace.events[start..]
            .iter()
            .take_while(|e| e.step <= slice.to)
            .filter(|e| match picked.lane {
                Lane::Thread { pid, tid } => e.pid == pid && e.tid == tid,
                Lane::Kernel | Lane::Idle => false,
            })
            .collect();
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
            .child(
                div()
                    .text_color(rgb(theme::TEXT))
                    .font_family(self.fonts.mono.clone())
                    .child(title),
            );
        if events.is_empty() {
            card = card.child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child("No events: the thread ran without writing, opening or forking."),
            );
        }
        for event in events.iter().take(MAX_SLICE_EVENTS) {
            let step = event.step;
            let text = describe::short_store_paths(&describe::describe(event).text);
            let line = div()
                .id(SharedString::from(format!("lane-event-{step}")))
                .flex()
                .gap(px(size::LIST_COLUMN_GAP))
                .font_family(self.fonts.mono.clone())
                .text_color(rgb(theme::SOFT))
                .whitespace_nowrap()
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::FAINT))
                        .child(thousands(step)),
                )
                .child(div().truncate().child(text));
            let line = match picked.side {
                Side::Here => line
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(theme::TEXT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx))),
                Side::There => line,
            };
            card = card.child(line);
        }
        if events.len() > MAX_SLICE_EVENTS {
            card = card.child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child(format!("and {} more", events.len() - MAX_SLICE_EVENTS)),
            );
        }
        if picked.side == Side::There {
            let from = slice.from;
            card = card.child(
                crate::ui::bookmarks::link(
                    "lanes-show-other",
                    "Show the compared run at this step",
                )
                .on_click(cx.listener(move |this, _, _, cx| this.show_other_run(Some(from), cx))),
            );
        }
        Some(card)
    }
}

/// The steps each lane marks with a line.
struct Markers {
    /// Where the two runs part, by their events.
    divergence: Option<u64>,
    /// The first step their threads on the CPU differ.
    switch: Option<u64>,
    playhead: u64,
}

/// The step numbers along the top of the lanes: the window's first, its
/// center and its last.
fn axis(window: Window) -> Div {
    div()
        .flex()
        .flex_none()
        .px(px(size::PANEL_PAD_X))
        .child(div().flex_none().w(px(LABEL_WIDTH)))
        .child(
            div()
                .flex()
                .flex_grow(layout::FILL)
                .justify_between()
                .pb(px(size::LEGEND_GAP))
                .border_b_1()
                .border_color(rgb(theme::LINE))
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(theme::FAINT))
                .child(thousands(window.from()))
                .child(thousands(window.center))
                .child(thousands(window.to())),
        )
}

/// A line of muted text in place of the lanes.
fn note(text: String) -> Div {
    div()
        .p(px(size::PANEL_PAD_X))
        .text_color(rgb(theme::MUTED))
        .child(text)
}
