//! The Runs panel's step axis on screen: the controls and the axis over
//! the runs, each row's track, and the window they share. The window is
//! the Threads tab's: it opens centered where the run on screen and the
//! compared run part, else on the playhead, follows the playhead from
//! then on, and Ctrl and the wheel zoom it while Shift and the wheel pan
//! it. It zooms out further than the Threads tab's, to the family's
//! longest run.
//!
//! Where each run parts is worked out from the traces in the background
//! (`crate::family_axis`): from the run files on this machine, or from
//! the runs the session holds when the family is only those, as in the
//! browser and the tour.

use std::collections::{HashMap, HashSet};

use gpui::{
    Context, Div, Pixels, ScrollWheelEvent, canvas, div, fill, point, prelude::*, px, rgb, rgba,
};
use rewind_trace::Trace;
use rewind_trace::manifest::TRACE;

use crate::describe::thousands;
use crate::family::RunEntry;
use crate::family_axis::{Cluster, Parted, ToPlayhead, Track, partings, shows_every_step};
use crate::lanes::{MIN_HALF, Window};
use crate::theme::{self, size};
use crate::ui::bookmarks::link;
use crate::ui::icons::Icon;
use crate::ui::runs::{Layout, Work};
use crate::ui::scrubber::Scrubber;
use crate::ui::splits::measure;
use crate::ui::widgets::{Availability, ButtonStyle, button, icon, tooltip};
use crate::ui::zoom::{PAN_PER_NOTCH, wheel_notches};
use crate::view::ZOOM_IN;

/// How far either side of its center the axis reaches when it opens: wide
/// enough to show where a failing program behaves differently a few
/// hundred steps after the runs part.
const OPEN_HALF: u64 = 400;

/// How much one press of − or + zooms the axis.
const BUTTON_ZOOM: f64 = 0.5;

/// The width of the −/+ buttons.
const ZOOM_BUTTON_WIDTH: f32 = 28.0;

/// The width of the pill and id beside each row's graph, so every track
/// starts at the same place under the axis.
pub(super) const LABEL_WIDTH: f32 = 150.0;

/// The heights of a run's own steps, of the steps it shares with the run
/// it is measured against, of the band where its schedule perturbed it,
/// and of the mark where the failing program behaves differently.
const OWN_HEIGHT: f32 = 3.0;
const SAME_HEIGHT: f32 = 1.0;
const PERTURBED_HEIGHT: f32 = 12.0;
const BEHAVES_HEIGHT: f32 = 16.0;

/// The widths of the marks: where the machine first differs, the end of
/// the run, the failing program's mark, and the playhead.
const MACHINE_MARK: f32 = 7.0;
const END_MARK: f32 = 7.0;
const BEHAVES_WIDTH: f32 = 2.0;
const PLAYHEAD_WIDTH: f32 = 1.0;

/// The marks a folding row draws for the runs it hides: smaller than a
/// run's end, and see-through, so where many fall together reads darker.
const CLUSTER_MARK: f32 = 5.0;
const CLUSTER_OPACITY: f32 = 0.55;

/// The narrowest the band of perturbed steps is drawn: wider than the
/// mark where the machine first differs, which sits inside a narrow
/// window rewind check made.
const PERTURBED_MIN_WIDTH: f32 = 13.0;

/// The band where a schedule perturbed a run: the compared run's blue,
/// faint.
const PERTURBED_A: u32 = 0x7fb2ff2e;

const ZOOM_NOTE: &str = "The runs show this many steps either side of the axis's center. Ctrl and the wheel over the runs zoom around the pointer, Shift and the wheel pan, and the wheel alone scrolls the runs.";
const ZOOM_IN_NOTE: &str =
    "Zoom in: fewer steps, each wider. Ctrl and the wheel zoom around the pointer.";
const ZOOM_OUT_NOTE: &str =
    "Zoom out: more steps, each narrower. Ctrl and the wheel zoom around the pointer.";
const SAME_NOTE: &str = "A thin line: steps where the run is still exactly its parent, the run it hangs under in the tree. A fork's parent is the run it was forked from; a run from boot under another schedule hangs under the run recorded without one. Step N of the two runs is the same moment only here.";
const OWN_NOTE: &str = "A thick line: the run after it first differs from its parent, to its end, green for a run that passed and red for one that failed. From here on its step numbers are its own; step N of two runs is no longer the same moment.";
const MACHINE_NOTE: &str = "The first step where the run's trace differs from its parent's, as rewind diff finds it. A perturbed schedule makes the machine differ within a few steps of where it starts perturbing, so for most runs this sits at the left edge of the blue band.";
const BEHAVES_NOTE: &str = "Where the program that failed in one of the two runs first behaves differently, as rewind check reports it. Drawn only when one of the run and its parent failed and the other passed.";
const PERTURBED_NOTE: &str = "The steps the run's thread schedule perturbed: from where the job started for a run from boot, from the fork step for a fork, and the window rewind check confined a schedule to as it narrowed it down.";
const WORKING_NOTE: &str = "Comparing each run's trace with the run it hangs under.";

impl Scrubber {
    /// Lays the Runs panel out as `layout`. The step axis opens, the
    /// first time, centered where the two runs on screen part, else on
    /// the playhead.
    pub(super) fn set_runs_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.runs.layout = layout;
        if layout == Layout::Steps {
            if self.runs.axis.is_none() {
                let center = self
                    .session
                    .as_ref()
                    .and_then(|s| s.divergence_step())
                    .unwrap_or(self.step);
                let half = OPEN_HALF.min(self.family_axis_max_half());
                self.runs.axis = Some(Window::around(center, half, self.family_axis_last()));
            }
            self.work_out_partings(cx);
        }
        cx.notify();
    }

    /// The last step the axis may reach: the end of the family's longest
    /// run, or of the run on screen before the family is read.
    fn family_axis_last(&self) -> u64 {
        let shown = self.session.as_ref().map_or(0, |s| s.run.timeline.total);
        self.runs.longest().max(shown)
    }

    /// The widest the axis zooms out: the longest run, whole.
    fn family_axis_max_half(&self) -> u64 {
        self.family_axis_last().div_ceil(2).max(MIN_HALF)
    }

    /// Brings the axis to the playhead when the playhead has left it,
    /// while the step axis is on screen, as the Threads tab follows it.
    pub(super) fn family_axis_playhead_moved(&mut self, cx: &mut Context<Self>) {
        if self.runs.layout != Layout::Steps {
            return;
        }
        let Some(axis) = self.runs.axis else {
            return;
        };
        if axis.at(self.step).is_some() {
            return;
        }
        self.center_family_axis_on_playhead(cx);
    }

    /// Centers the axis on the playhead, at the width it has.
    fn center_family_axis_on_playhead(&mut self, cx: &mut Context<Self>) {
        let Some(axis) = self.runs.axis else {
            return;
        };
        let window = Window::around(self.step, axis.half, self.family_axis_last());
        self.move_family_axis(window, cx);
    }

    /// Shows every run whole.
    fn show_whole_runs(&mut self, cx: &mut Context<Self>) {
        let last = self.family_axis_last();
        let half = self.family_axis_max_half();
        self.move_family_axis(Window::around(0, half, last), cx);
    }

    /// Moves the axis to `window`, never past where the longest run ends
    /// nor to start before step 0.
    fn move_family_axis(&mut self, mut window: Window, cx: &mut Context<Self>) {
        window.center = window.center.min(self.family_axis_last() as i64);
        let window = window.from_step_0();
        if self.runs.axis == Some(window) {
            return;
        }
        self.runs.axis = Some(window);
        cx.notify();
    }

    /// Zooms the axis by `factor` around the step `fraction` of the way
    /// across it.
    fn zoom_family_axis(&mut self, fraction: f32, factor: f64, cx: &mut Context<Self>) {
        let max_half = self.family_axis_max_half();
        if let Some(window) = self
            .runs
            .axis
            .map(|w| w.zoomed_within(fraction, factor, max_half))
        {
            self.move_family_axis(window, cx);
        }
    }

    /// The wheel over the runs: Ctrl zooms around the pointer, Shift or a
    /// sideways turn pans, and the wheel alone is left to scroll the rows.
    pub(super) fn family_axis_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let (dx, dy) = wheel_notches(event.delta);
        let sideways = dx.abs() > dy.abs();
        if event.modifiers.shift || sideways {
            let along = if sideways { dx } else { dy };
            if let Some(window) = self.runs.axis.map(|w| w.panned(-along * PAN_PER_NOTCH)) {
                self.move_family_axis(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if !event.modifiers.control {
            return;
        }
        let fraction = self.measured.runs_axis.get().map_or(0.5, |b| {
            ((event.position.x - b.origin.x) / b.size.width).clamp(0.0, 1.0)
        });
        self.zoom_family_axis(fraction, ZOOM_IN.powf(dy as f64), cx);
        cx.stop_propagation();
    }

    /// Works out where the family's runs part, for the runs that have
    /// ended and have no parting yet. A family that is only the session's
    /// runs is compared from the traces the session holds; any other is
    /// read from the run files, in the background.
    pub(super) fn work_out_partings(&mut self, cx: &mut Context<Self>) {
        if self.runs.layout != Layout::Steps || !self.runs.needs_partings() {
            return;
        }
        match self.runs.work {
            Work::Idle => {}
            Work::Working | Work::WorkingStale => {
                self.runs.work = Work::WorkingStale;
                return;
            }
        }
        let (Some(family), Some(session)) = (self.runs.family.clone(), &self.session) else {
            return;
        };
        let known: HashSet<String> = self.runs.partings.keys().cloned().collect();

        // The traces the session holds, by run id.
        let held: HashMap<String, &Trace> = std::iter::once(&session.run)
            .chain(session.other.as_ref())
            .filter_map(|r| Some((r.id()?.to_string(), &r.timeline.trace)))
            .collect();
        if family.runs.iter().all(|r| held.contains_key(&r.id)) {
            let found = partings(&family, &known, |r| held.get(&r.id).copied());
            self.runs.partings.extend(found);
            cx.notify();
            return;
        }

        self.runs.work = Work::Working;
        let task = cx.background_executor().spawn(async move {
            partings(&family, &known, |r| Trace::read(&r.dir.join(TRACE)).ok())
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                let again = this.runs.work == Work::WorkingStale;
                this.runs.work = Work::Idle;
                this.runs.partings.extend(found);
                if again {
                    this.work_out_partings(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The controls and the axis over the rows: zoom out and in around the
    /// axis's width, the links that show whole runs and center on the
    /// playhead, what each line and mark is, and the step numbers along
    /// the top, over the place the wheel reads.
    pub(super) fn render_family_axis_header(&self, columns: usize, cx: &mut Context<Self>) -> Div {
        let Some(axis) = self.runs.axis else {
            return div();
        };
        let zoom_button = |id: &'static str, glyph: Icon, note: &'static str, factor: f64| {
            button(id, ButtonStyle::Neutral, Availability::Enabled)
                .h(px(size::NOTICE_BUTTON_HEIGHT))
                .w(px(ZOOM_BUTTON_WIDTH))
                .px_0()
                .aria_label(note)
                .tooltip(tooltip(note))
                .child(icon(glyph, size::ICON_CHEVRON, theme::TEXT))
                .on_click(cx.listener(move |this, _, _, cx| this.zoom_family_axis(0.5, factor, cx)))
        };
        let readout = div()
            .id("runs-axis-width")
            .min_w(px(size::MENU_WIDTH / 2.0))
            .text_center()
            .font_family(self.fonts.mono.clone())
            .text_color(rgb(theme::SOFT))
            .tooltip(tooltip(ZOOM_NOTE))
            .child(format!("\u{b1}{} steps", thousands(axis.half)));
        // "Whole runs" only while some steps are out of view; the playhead
        // link zooms in where centering could not move the window.
        let last = self.family_axis_last();
        let whole = (!shows_every_step(axis, last)).then(|| {
            link("runs-axis-whole", "Whole runs")
                .on_click(cx.listener(|this, _, _, cx| this.show_whole_runs(cx)))
        });
        let to_playhead = ToPlayhead::of(axis, self.step, last, OPEN_HALF);
        let recenter = link("runs-axis-center", to_playhead.words()).on_click(
            cx.listener(move |this, _, _, cx| this.move_family_axis(to_playhead.window(), cx)),
        );
        let working = matches!(self.runs.work, Work::Working | Work::WorkingStale);
        let controls = div()
            .flex()
            .flex_none()
            .flex_wrap()
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
                        "runs-axis-zoom-out",
                        Icon::ZoomOut,
                        ZOOM_OUT_NOTE,
                        1.0 / BUTTON_ZOOM,
                    ))
                    .child(readout)
                    .child(zoom_button(
                        "runs-axis-zoom-in",
                        Icon::ZoomIn,
                        ZOOM_IN_NOTE,
                        BUTTON_ZOOM,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(size::CARD_GAP))
                    .when(working, |d| {
                        d.child(
                            div()
                                .id("runs-axis-working")
                                .text_color(rgb(theme::FAINT))
                                .tooltip(tooltip(WORKING_NOTE))
                                .child("comparing\u{2026}"),
                        )
                    })
                    .children(whole)
                    .child(recenter),
            );

        // What each line and mark is, each with a note on hover.
        let item = |id: &'static str, swatch: Div, text: &'static str, note: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(size::LEGEND_GAP * 1.5))
                .tooltip(tooltip(note))
                .child(swatch)
                .child(text)
        };
        let line = |height: f32, color: u32| {
            div()
                .flex_none()
                .w(px(MACHINE_MARK * 3.0))
                .h(px(height))
                .bg(rgb(color))
        };
        let legend = div()
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
                "runs-legend-same",
                line(SAME_HEIGHT, theme::GRAPH_LINE),
                "same as parent",
                SAME_NOTE,
            ))
            .child(item(
                "runs-legend-own",
                line(OWN_HEIGHT, theme::GREEN_SOFT),
                "diverged",
                OWN_NOTE,
            ))
            .child(item(
                "runs-legend-machine",
                div()
                    .flex_none()
                    .size(px(MACHINE_MARK))
                    .border_1()
                    .border_color(rgb(theme::SOFT)),
                "first difference",
                MACHINE_NOTE,
            ))
            .child(item(
                "runs-legend-behaves",
                div()
                    .flex_none()
                    .w(px(BEHAVES_WIDTH))
                    .h(px(BEHAVES_HEIGHT * 0.75))
                    .bg(rgb(theme::RED_SOFT)),
                "failing program differs",
                BEHAVES_NOTE,
            ))
            .child(item(
                "runs-legend-perturbed",
                div()
                    .flex_none()
                    .w(px(MACHINE_MARK * 3.0))
                    .h(px(PERTURBED_HEIGHT * 0.75))
                    .bg(rgba(PERTURBED_A)),
                "schedule perturbed",
                PERTURBED_NOTE,
            ));

        // The step numbers, over the tracks: as far in as each row's graph
        // and label.
        let axis_row = div()
            .flex()
            .flex_none()
            .gap(px(size::CONTROL_GAP))
            .px(px(size::PANEL_PAD_X))
            .child(div().flex_none().w(px(size::GRAPH_LANE * columns as f32)))
            .child(div().flex_none().w(px(LABEL_WIDTH)))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_grow(theme::layout::FILL)
                    .justify_between()
                    .pb(px(size::LEGEND_GAP))
                    .border_b_1()
                    .border_color(rgb(theme::LINE))
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::FAINT))
                    .child(measure(|m| &m.runs_axis, &self.measured))
                    .child(thousands(axis.from()))
                    .child(thousands(axis.center_step()))
                    .child(thousands(axis.to())),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .on_scroll_wheel(
                cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.family_axis_wheel(e, cx)),
            )
            .child(controls)
            .child(legend)
            .child(axis_row)
    }

    /// What the step axis knows of a row's run, as words for its hover
    /// note: where it parts and where it ends.
    pub(super) fn family_axis_words(&self, run: &RunEntry) -> String {
        let mut words = Vec::new();
        match self.runs.parted(run) {
            Parted::Root => words.push("the run the others hang under".to_string()),
            Parted::Unknown => words.push("not compared yet".to_string()),
            Parted::Known(p) => {
                match p.machine {
                    Some(step) => words.push(format!(
                        "first differs from its parent at {}",
                        thousands(step)
                    )),
                    None => words.push("the same as its parent to the end".to_string()),
                }
                if let Some(step) = p.behaves {
                    words.push(format!("failing program differs at {}", thousands(step)));
                }
            }
        }
        if let Some(last) = run.last_step {
            words.push(format!("{} at {}", run.ending, thousands(last)));
        }
        words.join(" \u{b7} ")
    }
}

/// A row's track: `track` painted across the row's width, green for a run
/// that passed and red for one that failed, with the playhead at
/// `playhead` of the way across when it is in the window.
pub(super) fn track_cell(track: Track, failed: bool, playhead: Option<f32>) -> impl IntoElement {
    let own_color = if failed {
        theme::RED
    } else {
        theme::GREEN_SOFT
    };
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let width = bounds.size.width;
            let x = |fraction: f32| bounds.left() + width * fraction;
            let mid = bounds.top() + bounds.size.height / 2.0;
            let band = |from: f32, to: f32, height: f32| {
                let h = px(height);
                gpui::Bounds::new(
                    point(x(from), mid - h / 2.0),
                    gpui::size((x(to) - x(from)).max(px(1.0)), h),
                )
            };
            let mark = |at: f32, w: f32, h: f32| {
                let (w, h): (Pixels, Pixels) = (px(w), px(h));
                gpui::Bounds::new(point(x(at) - w / 2.0, mid - h / 2.0), gpui::size(w, h))
            };

            // The band of perturbed steps under the lines, widened about
            // its middle when it is narrower than the marks over it.
            if let Some(s) = track.perturbed {
                let mut b = band(s.from, s.to, PERTURBED_HEIGHT);
                let min = px(PERTURBED_MIN_WIDTH);
                if b.size.width < min {
                    b.origin.x -= (min - b.size.width) / 2.0;
                    b.size.width = min;
                }
                window.paint_quad(fill(b, rgba(PERTURBED_A)));
            }

            // The steps shared with the run it hangs under, thin, then its
            // own steps, thick.
            if let Some(s) = track.same {
                window.paint_quad(fill(
                    band(s.from, s.to, SAME_HEIGHT),
                    rgb(theme::GRAPH_LINE),
                ));
            }
            if let Some(s) = track.own {
                window.paint_quad(fill(band(s.from, s.to, OWN_HEIGHT), rgb(own_color)));
            }

            // Where the machine first differs: a hollow square.
            if let Some(at) = track.machine {
                window.paint_quad(
                    fill(mark(at, MACHINE_MARK, MACHINE_MARK), rgb(theme::PANEL))
                        .border_widths(px(1.0))
                        .border_color(rgb(theme::SOFT)),
                );
            }

            // Where the failing program first behaves differently.
            if let Some(at) = track.behaves {
                window.paint_quad(fill(
                    mark(at, BEHAVES_WIDTH, BEHAVES_HEIGHT),
                    rgb(theme::RED_SOFT),
                ));
            }

            // The end: a dot for a run that passed, a square for one that
            // failed.
            if let Some(at) = track.end {
                let dot = mark(at, END_MARK, END_MARK);
                let radius = if failed { px(1.0) } else { px(END_MARK / 2.0) };
                window.paint_quad(fill(dot, rgb(own_color)).corner_radii(radius));
            }

            // The playhead, top to bottom.
            if let Some(at) = playhead {
                let line = gpui::Bounds::new(
                    point(x(at), bounds.top()),
                    gpui::size(px(PLAYHEAD_WIDTH), bounds.size.height),
                );
                window.paint_quad(fill(line, rgb(theme::AMBER)));
            }
        },
    )
    .flex_grow(theme::layout::FILL)
    .min_w_0()
    .h_full()
}

/// A folding row's track: a small mark where each run it hides ended,
/// green for a run that passed and red for one that failed, and a tick
/// where each one's failing program first behaves differently, faint
/// enough that a pile of them reads as where the runs gather.
pub(super) fn cluster_cell(cluster: Cluster, playhead: Option<f32>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let width = bounds.size.width;
            let x = |fraction: f32| bounds.left() + width * fraction;
            let mid = bounds.top() + bounds.size.height / 2.0;
            let mark = |at: f32, w: f32, h: f32| {
                let (w, h): (Pixels, Pixels) = (px(w), px(h));
                gpui::Bounds::new(point(x(at) - w / 2.0, mid - h / 2.0), gpui::size(w, h))
            };

            // Each program mark, then each end over them.
            for at in &cluster.behaves {
                window.paint_quad(fill(
                    mark(*at, BEHAVES_WIDTH, BEHAVES_HEIGHT),
                    rgb(theme::RED_SOFT).opacity(CLUSTER_OPACITY),
                ));
            }
            for end in &cluster.ends {
                let color = if end.failed {
                    theme::RED
                } else {
                    theme::GREEN_SOFT
                };
                window.paint_quad(
                    fill(
                        mark(end.at, CLUSTER_MARK, CLUSTER_MARK),
                        rgb(color).opacity(CLUSTER_OPACITY),
                    )
                    .corner_radii(px(CLUSTER_MARK / 2.0)),
                );
            }

            // The playhead, top to bottom.
            if let Some(at) = playhead {
                let line = gpui::Bounds::new(
                    point(x(at), bounds.top()),
                    gpui::size(px(PLAYHEAD_WIDTH), bounds.size.height),
                );
                window.paint_quad(fill(line, rgb(theme::AMBER)));
            }
        },
    )
    .flex_grow(theme::layout::FILL)
    .min_w_0()
    .h_full()
}

/// The layout the toggle in the panel's title switches to, and its words.
pub(super) fn other_layout(layout: Layout) -> (Layout, &'static str) {
    match layout {
        Layout::List => (Layout::Steps, "Show steps"),
        Layout::Steps => (Layout::List, "Show list"),
    }
}
