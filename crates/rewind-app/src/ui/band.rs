//! The switch band under the track: a strip whose color at each point is
//! how often the guest switched tasks there, dark where threads handed the
//! CPU back and forth at nearly every step and light where one task ran
//! alone. It shows at a glance where in a run to open the Threads tab. The
//! band follows the track's zoom, the wheel over it zooms and pans as over
//! the track, a click moves the playhead, and pointing at it says how many
//! switches its bucket holds.

use std::rc::Rc;

use gpui::{
    Bounds, Context, DispatchPhase, Div, MouseButton, MouseDownEvent, MouseMoveEvent,
    ScrollWheelEvent, Stateful, canvas, div, fill, point, prelude::*, px, relative, rgb, rgba,
};

use crate::density::{Density, switches_between};
use crate::describe::thousands;
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::view::View;

/// How many buckets the band has, whatever its width: about 3 px each on
/// a laptop screen.
pub(super) const BUCKETS: usize = 400;

/// The band's height, and how far below the track it sits, under the
/// ticks.
const BAND_HEIGHT: f32 = 6.0;
const BAND_GAP: f32 = size::TICK_DROP + size::TICK_HEIGHT + 3.0;

/// How opaque a bucket with a single switch is at least, so that a few
/// switches still show, and how much more opaque a bucket where every
/// step is a switch is.
const ALPHA_FLOOR: f32 = 0.12;
const ALPHA_RANGE: f32 = 0.88;

/// Past this share of the band the pointer's label reads leftward.
const LABEL_FLIP: f32 = 0.75;

impl Scrubber {
    /// The density the band draws for `view`, computed once per view and
    /// kept until the view changes.
    fn band_density(&mut self, view: View) -> Rc<Density> {
        if let Some(density) = &self.band
            && density.view == view
        {
            return density.clone();
        }
        let switches = &self.session().run.timeline.trace.switches;
        let density = Rc::new(Density::of(switches, view, BUCKETS));
        self.band = Some(density.clone());
        density
    }

    /// Records where the pointer is along the band, or that it left it.
    pub(super) fn set_band_hover(&mut self, fraction: Option<f32>, cx: &mut Context<Self>) {
        if self.band_hover != fraction {
            self.band_hover = fraction;
            cx.notify();
        }
    }

    /// The band, when the run recorded its context switches.
    pub(super) fn render_switch_band(
        &mut self,
        view: View,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        if self.session().run.timeline.trace.switches.is_empty() {
            return None;
        }
        let density = self.band_density(view);
        let mut band = div()
            .id("switch-band")
            .relative()
            .w_full()
            .h(px(BAND_HEIGHT))
            .mt(px(BAND_GAP))
            .rounded(px(1.0))
            .cursor_pointer()
            .child(band_paint(density.clone()))
            .child(self.band_input(view, cx));

        // The bucket under the pointer: a line over it, and a label with
        // its steps and its switches, reading away from the nearer end.
        if let Some(fraction) = self.band_hover {
            let i = density.bucket_at(fraction);
            let (lo, hi) = density.bucket_steps(i);
            let switches = &self.session().run.timeline.trace.switches;
            let count = switches_between(switches, lo, hi);
            let note = format!(
                "steps {} \u{2013} {} \u{b7} {} context switch{}",
                thousands(lo),
                thousands(hi),
                thousands(count as u64),
                if count == 1 { "" } else { "es" }
            );
            band = band.child(
                div()
                    .absolute()
                    .left(relative(fraction))
                    .top(px(-2.0))
                    .bottom(px(-2.0))
                    .w(px(1.0))
                    .bg(rgb(theme::SOFT)),
            );
            let label = div()
                .absolute()
                .top(px(BAND_HEIGHT + 2.0))
                .px(px(size::SEGMENT_LABEL_PAD))
                .py(px(2.0))
                .rounded(px(size::RADIUS_SEGMENT))
                .bg(rgb(theme::RAISED))
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(theme::SOFT))
                .whitespace_nowrap()
                .child(note);
            band = band.child(if fraction > LABEL_FLIP {
                label.right(relative(1.0 - fraction))
            } else {
                label.left(relative(fraction))
            });
        }
        Some(band)
    }

    /// The layer over the band that takes the pointer: moving over it
    /// names the bucket, the wheel zooms and pans the track, and a press
    /// moves the playhead to the step under it.
    fn band_input(&self, view: View, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let along = move |x: gpui::Pixels| (x - bounds.origin.x) / bounds.size.width;

                let move_entity = entity.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let over = bounds.contains(&e.position).then(|| along(e.position.x));
                    move_entity.update(cx, |this, cx| this.set_band_hover(over, cx));
                });

                let wheel_entity = entity.clone();
                window.on_mouse_event(move |e: &ScrollWheelEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || !bounds.contains(&e.position) {
                        return;
                    }
                    wheel_entity.update(cx, |this, cx| {
                        this.track_wheel(along(e.position.x), e.delta, e.modifiers.shift, cx)
                    });
                });

                let down_entity = entity.clone();
                window.on_mouse_event(move |e: &MouseDownEvent, phase, _, cx| {
                    let pressed = e.button == MouseButton::Left && bounds.contains(&e.position);
                    if phase != DispatchPhase::Bubble || !pressed {
                        return;
                    }
                    let step = view.step_at(along(e.position.x));
                    down_entity.update(cx, |this, cx| this.jump_to(step, cx));
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

/// The band's paint: a quad per bucket with any switches, in amber as
/// opaque as the bucket is dense.
fn band_paint(density: Rc<Density>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let buckets = density.shares.len();
            if buckets == 0 {
                return;
            }
            let width = bounds.size.width / buckets as f32;
            for (i, share) in density.shares.iter().enumerate() {
                if *share <= 0.0 {
                    continue;
                }
                let alpha = ALPHA_FLOOR + ALPHA_RANGE * share;
                let color = (theme::AMBER << 8) | (alpha * 255.0).round() as u32;
                let origin = point(bounds.origin.x + width * i as f32, bounds.origin.y);
                let quad = Bounds::new(origin, gpui::size(width + px(0.5), bounds.size.height));
                window.paint_quad(fill(quad, rgba(color)));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
