//! Zooming the timeline and the step under the pointer: the wheel over the
//! track zooms around the pointer, Shift and the wheel, or a sideways
//! wheel, pans, and + and - zoom around the playhead, 0 back to the whole
//! run. While the pointer is over the track, a line and a label show the
//! step under it. `crate::view` does the arithmetic.

use gpui::{Context, Div, ScrollDelta, div, prelude::*, px, relative, rgb};

use crate::describe::thousands;
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::view::{View, ZOOM_IN, ZOOM_OUT};

/// The wheel's travel that zooms by one notch's factor: in pixels from a
/// touchpad, in lines from a wheel, which reports three lines a click.
const PIXELS_PER_NOTCH: f32 = 20.0;
const LINES_PER_NOTCH: f32 = 3.0;

/// How much of its width one notch of a sideways wheel pans the window.
pub(super) const PAN_PER_NOTCH: f32 = 0.1;

/// How far above the track the labels sit.
const LABEL_RISE: f32 = 20.0;

/// Past this share of the track the pointer's label reads leftward.
const LABEL_FLIP: f32 = 0.75;

impl Scrubber {
    /// The steps the timeline shows: the zoomed window, or the whole run.
    pub(super) fn timeline_view(&self) -> View {
        let total = self.session.as_ref().map_or(0, |s| s.run.timeline.total);
        self.view
            .filter(|v| v.hi <= total && v.lo < v.hi)
            .unwrap_or(View::whole(total))
    }

    /// Zooms by `factor`, below 1 to zoom in, keeping the step `fraction`
    /// of the way along the track where it is.
    pub(super) fn zoom_at(&mut self, fraction: f32, factor: f64, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let total = session.run.timeline.total;
        let zoomed = self.timeline_view().zoomed(fraction, factor, total);
        self.view = (!zoomed.is_whole(total)).then_some(zoomed);
        cx.notify();
    }

    /// Zooms in around the playhead, as + does.
    pub(super) fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.zoom_around_playhead(ZOOM_IN, cx);
    }

    /// Zooms out around the playhead, as - does.
    pub(super) fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.zoom_around_playhead(ZOOM_OUT, cx);
    }

    /// Zooms by `factor` keeping the playhead where it is, after bringing
    /// it on screen when the wheel left it outside the window.
    fn zoom_around_playhead(&mut self, factor: f64, cx: &mut Context<Self>) {
        self.keep_playhead_in_view();
        let at = self.timeline_view().fraction_of(self.step).clamp(0.0, 1.0);
        self.zoom_at(at, factor, cx);
    }

    /// Shows the whole run again, as 0 does.
    pub(super) fn zoom_reset(&mut self, cx: &mut Context<Self>) {
        self.view = None;
        cx.notify();
    }

    /// Moves the zoomed window by `fraction` of its width.
    pub(super) fn pan(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let (Some(session), Some(view)) = (&self.session, self.view) else {
            return;
        };
        self.view = Some(view.panned(fraction, session.run.timeline.total));
        cx.notify();
    }

    /// The wheel over the track: a sideways turn, or Shift and the wheel,
    /// pans; any other zooms around the pointer, in for a turn up.
    pub(super) fn track_wheel(
        &mut self,
        fraction: f32,
        delta: ScrollDelta,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let (dx, dy) = wheel_notches(delta);
        if shift || dx.abs() > dy.abs() {
            let along = if dx.abs() > dy.abs() { dx } else { dy };
            self.pan(-along * PAN_PER_NOTCH, cx);
            return;
        }
        self.zoom_at(fraction, ZOOM_IN.powf(dy as f64), cx);
    }

    /// Keeps the playhead on screen: when it moves out of the zoomed
    /// window, the window moves to have it in its middle.
    pub(super) fn keep_playhead_in_view(&mut self) {
        let (Some(session), Some(view)) = (&self.session, self.view) else {
            return;
        };
        if !view.contains(self.step) {
            self.view = Some(view.centred_on(self.step, session.run.timeline.total));
        }
    }

    /// Records where the pointer is along the track, or that it left it.
    pub(super) fn set_hover(&mut self, fraction: Option<f32>, cx: &mut Context<Self>) {
        if self.hover != fraction {
            self.hover = fraction;
            cx.notify();
        }
    }

    /// The line and label for the step under the pointer, and while
    /// zoomed, which steps the track shows.
    pub(super) fn track_labels(&self) -> Vec<Div> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let t = &session.run.timeline;
        let view = self.timeline_view();
        let mut labels = Vec::new();

        // Which steps a zoom shows, while neither the pointer's label nor
        // the playhead's is where it would go.
        let playhead_right = self.step > view.hi;
        if !view.is_whole(t.total) && self.hover.is_none() && !playhead_right {
            labels.push(
                div()
                    .absolute()
                    .right_0()
                    .top(px(-LABEL_RISE))
                    .text_color(rgb(theme::MUTED))
                    .child(format!(
                        "steps {} \u{2013} {} of {} \u{b7} 0 shows the whole run",
                        thousands(view.lo),
                        thousands(view.hi),
                        thousands(t.total)
                    )),
            );
        }
        // Where the playhead is, at the edge it is past, when the wheel
        // zoomed in somewhere else; the pointer's label takes its place
        // while the pointer is over the track.
        if !view.contains(self.step) && self.hover.is_none() {
            let label = div()
                .absolute()
                .top(px(-LABEL_RISE))
                .text_color(rgb(theme::AMBER))
                .whitespace_nowrap();
            let at = thousands(self.step);
            labels.push(if self.step < view.lo {
                label.left_0().child(format!("\u{25c2} playhead at {at}"))
            } else {
                label.right_0().child(format!("playhead at {at} \u{25b8}"))
            });
        }

        let Some(fraction) = self.hover else {
            return labels;
        };
        let step = view.step_at(fraction);
        let phase = t
            .phase_index_at(step)
            .map_or_else(String::new, |i| format!(" \u{b7} {}", t.phases[i].name));
        labels.push(
            div()
                .absolute()
                .left(relative(fraction))
                .top_0()
                .bottom_0()
                .w(px(1.0))
                .bg(rgb(theme::MUTED)),
        );
        // The label reads from the pointer rightward, or near the right
        // end leftward, so it stays over the track.
        let label = div()
            .absolute()
            .top(px(-LABEL_RISE))
            .whitespace_nowrap()
            .text_color(rgb(theme::SOFT))
            .child(format!("step {}{phase}", thousands(step)));
        labels.push(if fraction > LABEL_FLIP {
            label
                .right(relative(1.0 - fraction))
                .mr(px(size::SEGMENT_LABEL_PAD))
        } else {
            label
                .left(relative(fraction))
                .ml(px(size::SEGMENT_LABEL_PAD))
        });
        labels
    }
}

/// A turn of the wheel in notches, sideways and up, from a touchpad's
/// pixels or a wheel's lines.
pub(super) fn wheel_notches(delta: ScrollDelta) -> (f32, f32) {
    match delta {
        ScrollDelta::Lines(lines) => (lines.x / LINES_PER_NOTCH, lines.y / LINES_PER_NOTCH),
        ScrollDelta::Pixels(pixels) => (
            f32::from(pixels.x) / PIXELS_PER_NOTCH,
            f32::from(pixels.y) / PIXELS_PER_NOTCH,
        ),
    }
}
