//! Sideways scrolling on screen, for the source panel and the file viewer:
//! the layer that takes sideways wheel and trackpad motion over a text
//! area, and the cell that draws a line's text moved left by the offset
//! while its line number stays put. The offsets themselves, and how far
//! they go, are `crate::sideways`.

use gpui::{
    Context, DispatchPhase, Div, HitboxBehavior, ScrollWheelEvent, canvas, div, prelude::*, px,
};

use crate::theme::layout;
use crate::ui::scrubber::Scrubber;

/// What a text area does with sideways motion: scroll by its pixels
/// toward the lines' ends, in an area its pixels wide.
pub type ScrollSideways = fn(&mut Scrubber, f32, f32, &mut Context<Scrubber>);

/// A layer over a text area that hands sideways wheel and trackpad
/// motion, and Shift with the wheel, to `scroll`, and keeps that motion
/// from the list under it, which scrolls only up and down. Motion more up
/// and down than sideways passes through to the list.
pub fn sideways_layer(scroll: ScrollSideways, cx: &mut Context<Scrubber>) -> impl IntoElement {
    let view = cx.entity();
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, _| {
            let line_height = window.line_height();
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                    return;
                }

                // Sideways motion, or Shift with up and down motion, as
                // a platform that does not turn it sideways sends it.
                let delta = event.delta.pixel_delta(line_height);
                let (across, down) = (f32::from(delta.x), f32::from(delta.y));
                let sideways = if across.abs() > down.abs() {
                    across
                } else if event.modifiers.shift {
                    down
                } else {
                    return;
                };

                // GPUI's deltas move the text: a positive one back toward
                // the lines' starts.
                cx.stop_propagation();
                let width = f32::from(bounds.size.width);
                view.update(cx, |this, cx| scroll(this, -sideways, width, cx));
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The cell of a row that holds its text, moved `offset` pixels left and
/// cut at the cell's edges, so it never covers the line number.
pub fn shifted(offset: f32, text: impl IntoElement) -> Div {
    div()
        .flex_grow(layout::FILL)
        .min_w_0()
        .overflow_hidden()
        .child(div().relative().left(px(-offset)).child(text))
}
