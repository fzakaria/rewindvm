//! The window's own chrome, for compositors that leave it to the app.
//!
//! The app asks for server-side decorations. Compositors that support the
//! request (KDE, most wlroots ones, any X11 window manager) draw the title
//! bar and borders themselves, and the app draws nothing extra. GNOME on
//! Wayland does not, so there the header bar becomes the title bar: it
//! drags the window, double-click maximizes, right-click opens the window
//! menu, the controls at its right end minimize, maximize and close, and
//! the window's edges and corners resize it.

use gpui::{
    AnyElement, Context, CursorStyle, Decorations, Div, MouseButton, ResizeEdge, Role, Stateful,
    Tiling, Window, WindowDecorations, div, prelude::*, px, rgb,
};

use crate::theme::{self, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::icon;

/// Set to `client` to draw the app's own title bar controls even where
/// the compositor would draw them, or `server` to never draw them.
pub const DECORATIONS_ENV: &str = "REWIND_APP_DECORATIONS";
const CLIENT: &str = "client";

/// Resize handles: the edges are this thick, the corners this square.
const EDGE: f32 = 6.0;
const CORNER: f32 = 14.0;

/// The window controls: 44 pixel targets, the close one red under the
/// pointer.
const CONTROL_SIZE: f32 = 44.0;
const CONTROL_ICON: f32 = 16.0;
const CLOSE_HOVER: u32 = 0xc4412f;

/// The decorations the app asks the compositor for.
pub fn requested_decorations() -> WindowDecorations {
    match std::env::var(DECORATIONS_ENV).as_deref() {
        Ok(CLIENT) => WindowDecorations::Client,
        _ => WindowDecorations::Server,
    }
}

/// Whether the app draws its own chrome, and which edges are free to
/// resize (a tiled or maximized edge is not).
pub(super) fn client_tiling(window: &Window) -> Option<Tiling> {
    match window.window_decorations() {
        Decorations::Server => None,
        Decorations::Client { tiling } => Some(tiling),
    }
}

/// A window control's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Control {
    fn label(self) -> &'static str {
        match self {
            Control::Minimize => "Minimize",
            Control::Maximize => "Maximize",
            Control::Restore => "Restore",
            Control::Close => "Close",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Control::Minimize => Icon::Minimize,
            Control::Maximize => Icon::Maximize,
            Control::Restore => Icon::Restore,
            Control::Close => Icon::Close,
        }
    }

    fn run(self, window: &mut Window) {
        match self {
            Control::Minimize => window.minimize_window(),
            Control::Maximize | Control::Restore => window.zoom_window(),
            Control::Close => window.remove_window(),
        }
    }
}

impl Scrubber {
    /// Makes `header` the window's title bar when the app draws its own
    /// chrome: dragging it moves the window, double-click maximizes and
    /// right-click opens the compositor's window menu. The window controls
    /// are placed by the header, from `window_controls`.
    pub(super) fn titlebar(
        &self,
        header: Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let header = header.id("titlebar");
        if client_tiling(window).is_none() {
            return header;
        }

        // A press arms a move and the first motion starts it, so that a
        // plain click or a double-click never becomes a move.
        header
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_armed = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_armed = false),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.titlebar_armed {
                    this.titlebar_armed = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.is_right_click() {
                    window.show_window_menu(event.position());
                    return;
                }
                const DOUBLE_CLICK: usize = 2;
                if event.click_count() == DOUBLE_CLICK {
                    window.zoom_window();
                }
            })
    }

    /// Minimize, maximize or restore, and close, when the app draws its
    /// own chrome.
    pub(super) fn window_controls(&self, window: &Window) -> Option<Div> {
        client_tiling(window)?;
        let controls = window.window_controls();
        let zoom = if window.is_maximized() {
            Control::Restore
        } else {
            Control::Maximize
        };
        let mut shown = Vec::new();
        if controls.minimize {
            shown.push(Control::Minimize);
        }
        if controls.maximize {
            shown.push(zoom);
        }
        shown.push(Control::Close);

        let row = div()
            .flex()
            .flex_none()
            .items_center()
            .children(shown.into_iter().map(|control| {
                let hover = if control == Control::Close {
                    CLOSE_HOVER
                } else {
                    theme::RAISED_HOVER
                };
                div()
                    .id(control.label())
                    .role(Role::Button)
                    .aria_label(control.label())
                    .size(px(CONTROL_SIZE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(size::RADIUS_BUTTON))
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(hover)))
                    // A press here must not arm the title bar's move.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        control.run(window);
                    })
                    .child(icon(control.icon(), CONTROL_ICON, theme::SOFT))
            }));
        Some(row)
    }

    /// Invisible handles along the window's free edges and corners that
    /// resize it, when the app draws its own chrome.
    pub(super) fn resize_edges(&self, window: &Window) -> Option<AnyElement> {
        let tiling = client_tiling(window)?;
        if tiling.is_tiled() {
            return None;
        }
        let edge = px(EDGE);
        let corner = px(CORNER);
        let handle = |which: ResizeEdge, cursor: CursorStyle| {
            div()
                .id(edge_id(which))
                .absolute()
                .occlude()
                .cursor(cursor)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    window.start_window_resize(which);
                })
        };

        // Edges first, then corners over them.
        let mut layer = div().absolute().top_0().left_0().size_full();
        if !tiling.top {
            layer = layer.child(
                handle(ResizeEdge::Top, CursorStyle::ResizeUpDown)
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(edge),
            );
        }
        if !tiling.bottom {
            layer = layer.child(
                handle(ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .h(edge),
            );
        }
        if !tiling.left {
            layer = layer.child(
                handle(ResizeEdge::Left, CursorStyle::ResizeLeftRight)
                    .top_0()
                    .left_0()
                    .h_full()
                    .w(edge),
            );
        }
        if !tiling.right {
            layer = layer.child(
                handle(ResizeEdge::Right, CursorStyle::ResizeLeftRight)
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(edge),
            );
        }
        let corners = [
            (
                ResizeEdge::TopLeft,
                CursorStyle::ResizeUpLeftDownRight,
                !(tiling.top || tiling.left),
            ),
            (
                ResizeEdge::TopRight,
                CursorStyle::ResizeUpRightDownLeft,
                !(tiling.top || tiling.right),
            ),
            (
                ResizeEdge::BottomLeft,
                CursorStyle::ResizeUpRightDownLeft,
                !(tiling.bottom || tiling.left),
            ),
            (
                ResizeEdge::BottomRight,
                CursorStyle::ResizeUpLeftDownRight,
                !(tiling.bottom || tiling.right),
            ),
        ];
        for (which, cursor, free) in corners {
            if !free {
                continue;
            }
            let h = handle(which, cursor).size(corner);
            let h = match which {
                ResizeEdge::TopLeft => h.top_0().left_0(),
                ResizeEdge::TopRight => h.top_0().right_0(),
                ResizeEdge::BottomLeft => h.bottom_0().left_0(),
                _ => h.bottom_0().right_0(),
            };
            layer = layer.child(h);
        }
        Some(layer.into_any_element())
    }
}

/// The element id of the resize handle on `edge`.
fn edge_id(edge: ResizeEdge) -> &'static str {
    match edge {
        ResizeEdge::Top => "resize-top",
        ResizeEdge::TopRight => "resize-top-right",
        ResizeEdge::Right => "resize-right",
        ResizeEdge::BottomRight => "resize-bottom-right",
        ResizeEdge::Bottom => "resize-bottom",
        ResizeEdge::BottomLeft => "resize-bottom-left",
        ResizeEdge::Left => "resize-left",
        ResizeEdge::TopLeft => "resize-top-left",
    }
}
