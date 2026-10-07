//! The shortcut sheet: ? or the header's ? button lists every key the app
//! answers to, with a button that starts the tour. Escape, a click outside
//! it, or ? again closes it.

use gpui::{Context, Div, FontWeight, Window, div, prelude::*, px, rgb, rgba};

use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::{Availability, ButtonStyle, button};
use crate::ui::{CloseDialog, SHORTCUTS_CONTEXT, ShowShortcuts};

/// The sheet's backdrop and width.
const BACKDROP_A: u32 = 0x0000_00a0;
const SHEET_WIDTH: f32 = 900.0;

/// The sheet's padding.
const SHEET_PAD: f32 = size::CARD_PAD * 1.5;

/// How many of the groups go in the left column; the rest go right.
const LEFT_GROUPS: usize = 2;

/// The width of the keys column in a group.
const KEYS_WIDTH: f32 = 170.0;

/// Every key, by what it is for. The window's keyboard map is in
/// `crate::ui::bind_keys`; a key bound there is listed here.
pub const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Moving the playhead",
        &[
            (
                "Left  Right",
                "the previous or next stop, as Stop at chooses",
            ),
            ("Shift+Left  Shift+Right", "one step back or forward"),
            ("Page Up  Page Down", "the previous or next phase"),
            ("Home  End", "the start or the end of the run"),
            ("f  d", "the failure, the divergence"),
            ("x", "the compared run, at the matching step"),
            ("g", "type a step to go to"),
            ("Alt+Left  Alt+Right", "back and forward through jumps"),
        ],
    ),
    (
        "The timeline",
        &[
            ("+  -", "zoom in or out around the playhead"),
            ("0", "show the whole run"),
            ("Wheel", "zoom around the pointer; Shift+wheel pans"),
        ],
    ),
    (
        "Looking around",
        &[
            ("Ctrl+f  /", "search the log, files and events"),
            ("b", "bookmark the playhead's step"),
            ("s", "show or hide the source panel"),
            ("t", "show or hide the threads"),
            ("Escape", "close the tab, menu or terminal pane"),
            ("Ctrl+c  Ctrl+a", "copy, select all"),
            ("Ctrl+Shift+c  v", "copy and paste in the terminal pane"),
        ],
    ),
    (
        "The app",
        &[
            ("Ctrl+o", "open a run"),
            ("F1", "start the tour"),
            ("?", "this sheet"),
            ("Ctrl+q", "quit"),
        ],
    ),
];

impl Scrubber {
    /// Shows the sheet, or closes it when it shows.
    pub(super) fn toggle_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts_open = !self.shortcuts_open;
        if self.shortcuts_open {
            window.focus(&self.shortcuts_focus, cx);
        } else {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    fn close_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The sheet over a dimmed window, while it is open.
    pub(super) fn render_shortcuts(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.shortcuts_open {
            return None;
        }
        let column = || {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(size::SECTION_GAP))
        };
        let (mut left, mut right) = (column(), column());
        for (i, (title, keys)) in SHORTCUTS.iter().enumerate() {
            let mut group = div().flex().flex_col().gap(px(size::CARD_GAP / 2.0)).child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(title.to_uppercase()),
            );
            for (key, what) in *keys {
                group = group.child(
                    div()
                        .flex()
                        .gap(px(size::CARD_GAP))
                        .child(
                            div()
                                .flex_none()
                                .w(px(KEYS_WIDTH))
                                .font_family(self.fonts.mono.clone())
                                .text_size(px(size::TEXT_MONO))
                                .text_color(rgb(theme::AMBER))
                                .child(*key),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(rgb(theme::SOFT))
                                .child(*what),
                        ),
                );
            }
            if i < LEFT_GROUPS {
                left = left.child(group);
            } else {
                right = right.child(group);
            }
        }
        let groups = div()
            .flex()
            .gap(px(size::SECTION_GAP))
            .child(left)
            .child(right);
        let card = div()
            .w(px(SHEET_WIDTH))
            .flex()
            .flex_col()
            .gap(px(size::SECTION_GAP))
            .p(px(SHEET_PAD))
            .rounded(px(size::RADIUS_CARD))
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .shadow_lg()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(px(size::TEXT_BRAND))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Keys"),
            )
            .child(groups)
            .child(
                div().flex().justify_end().child(
                    button(
                        "shortcuts-tour",
                        ButtonStyle::Neutral,
                        Availability::Enabled,
                    )
                    .child("Start the tour")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_shortcuts(window, cx);
                        this.start_tour(window, cx);
                    })),
                ),
            );
        Some(
            div().absolute().top_0().left_0().size_full().child(
                div()
                    .id("shortcuts-backdrop")
                    .occlude()
                    .track_focus(&self.shortcuts_focus)
                    .key_context(SHORTCUTS_CONTEXT)
                    .on_action(cx.listener(|this, _: &CloseDialog, window, cx| {
                        this.close_shortcuts(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ShowShortcuts, window, cx| {
                        this.close_shortcuts(window, cx)
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_shortcuts(window, cx)),
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(BACKDROP_A))
                    .child(card),
            ),
        )
    }
}
