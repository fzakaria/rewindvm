//! The Open link dialog: a field to paste or type the URL of a .rwd file
//! into, such as one on a case study page, and a button to open it.
//!
//! GPUI has no text field of its own, so the field is a line of text that
//! takes typed characters, Backspace and Ctrl+V. That is all a URL needs.

use std::path::{Path, PathBuf};

use gpui::{
    Context, CursorStyle, Div, FocusHandle, FontWeight, KeyDownEvent, SharedString, Window, div,
    prelude::*, px, rgb, rgba,
};

use crate::archive;
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::{Availability, ButtonStyle, button};
use crate::ui::{CloseDialog, ConfirmLink, LINK_CONTEXT, PasteLink};

/// The dialog's backdrop and width, as the license dialog's.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 600.0;

/// The width of the field's caret.
const CARET_WIDTH: f32 = 1.5;

/// The dialog's words.
const DIALOG_TITLE: &str = "Open link";
const DIALOG_HELP: &str = "The link to a .rwd file, such as one on a case study page. A replayable export can be shelled into, debugged and forked once it is in.";
const PLACEHOLDER: &str = "https://\u{2026}/run.rwd";
const NOT_A_LINK: &str = "That is not an http or https link.";

/// A one-line text being typed: the URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineInput {
    pub text: String,
}

impl LineInput {
    /// Adds what a key typed. Control characters, Enter and Tab among
    /// them, type nothing.
    pub fn type_str(&mut self, typed: &str) {
        self.text.extend(typed.chars().filter(|c| !c.is_control()));
    }

    /// Removes the last character.
    pub fn backspace(&mut self) {
        self.text.pop();
    }

    /// Adds pasted text: its first line, without the spaces around it,
    /// since a link copied from a page often carries a newline.
    pub fn paste(&mut self, pasted: &str) {
        let line = pasted.trim().lines().next().unwrap_or_default();
        self.type_str(line);
    }
}

/// The dialog's state: what was typed, and why it was not opened.
pub struct LinkDialog {
    pub focus: FocusHandle,
    pub input: LineInput,
    pub error: Option<&'static str>,
}

impl Scrubber {
    /// Opens the dialog, with the clipboard's link in the field when there
    /// is one.
    pub(super) fn open_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut input = LineInput::default();
        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        if archive::is_url(Path::new(clipboard.trim())) {
            input.paste(&clipboard);
        }
        self.link_dialog = Some(LinkDialog {
            focus,
            input,
            error: None,
        });
        cx.notify();
    }

    pub(super) fn close_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.link_dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Takes a key typed in the field.
    fn link_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.link_dialog else {
            return;
        };
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        if keystroke.key == "backspace" {
            dialog.input.backspace();
        } else if let Some(typed) = &keystroke.key_char {
            dialog.input.type_str(typed);
        } else {
            return;
        }
        dialog.error = None;
        cx.notify();
    }

    fn paste_link(&mut self, cx: &mut Context<Self>) {
        let pasted = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let Some(dialog) = &mut self.link_dialog else {
            return;
        };
        dialog.input.paste(&pasted);
        dialog.error = None;
        cx.notify();
    }

    /// Opens the link in the field, or says why it cannot.
    fn confirm_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.link_dialog else {
            return;
        };
        let link = dialog.input.text.trim().to_string();
        if !archive::is_url(Path::new(&link)) {
            dialog.error = Some(NOT_A_LINK);
            cx.notify();
            return;
        }
        self.close_link_dialog(window, cx);
        self.open(PathBuf::from(link), None, cx);
    }

    /// The dialog over a dimmed window, when it is open.
    pub(super) fn render_link_dialog(&self, cx: &mut Context<Self>) -> Option<Div> {
        let dialog = self.link_dialog.as_ref()?;

        // The field: what was typed and the caret, or a placeholder.
        let typed = &dialog.input.text;
        let (shown, color): (SharedString, u32) = if typed.is_empty() {
            (PLACEHOLDER.into(), theme::MUTED)
        } else {
            (typed.clone().into(), theme::TEXT)
        };
        let focused_border = rgba(theme::FOCUS_RING_A);
        let field = div()
            .id("link-field")
            .track_focus(&dialog.focus)
            .key_context(LINK_CONTEXT)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| this.link_key(e, cx)))
            .on_action(cx.listener(|this, _: &PasteLink, _, cx| this.paste_link(cx)))
            .on_action(
                cx.listener(|this, _: &ConfirmLink, window, cx| this.confirm_link(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseDialog, window, cx| this.close_link_dialog(window, cx)),
            )
            .flex()
            .flex_wrap()
            .items_center()
            .cursor(CursorStyle::IBeam)
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .focus(move |s| s.border_color(focused_border))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(color))
            .when(!typed.is_empty(), |d| d.child(shown.clone()))
            .child(
                div()
                    .w(px(CARET_WIDTH))
                    .h(px(size::TEXT_MONO * 1.4))
                    .bg(rgb(theme::AMBER)),
            )
            .when(typed.is_empty(), |d| d.child(shown));

        let can_open = if typed.trim().is_empty() {
            Availability::Disabled
        } else {
            Availability::Enabled
        };
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(size::CONTROL_GAP))
            .child(
                button("link-paste", ButtonStyle::Neutral, Availability::Enabled)
                    .child("Paste")
                    .on_click(cx.listener(|this, _, _, cx| this.paste_link(cx))),
            )
            .child(
                button("link-cancel", ButtonStyle::Neutral, Availability::Enabled)
                    .child("Cancel")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_link_dialog(window, cx)),
                    ),
            )
            .child(
                button("link-open", ButtonStyle::Primary, can_open)
                    .child("Open")
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_link(window, cx))),
            );

        let mut card = div()
            .w(px(DIALOG_WIDTH))
            .flex()
            .flex_col()
            .gap(px(size::SECTION_GAP))
            .p(px(size::CARD_PAD * 1.5))
            .rounded(px(size::RADIUS_CARD))
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .shadow_lg()
            .child(
                div()
                    .text_size(px(size::TEXT_BRAND))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(DIALOG_TITLE),
            )
            .child(div().text_color(rgb(theme::SOFT)).child(DIALOG_HELP))
            .child(field);
        if let Some(error) = dialog.error {
            card = card.child(div().text_color(rgb(theme::RED_SOFT)).child(error));
        }
        card = card.child(buttons);

        Some(
            div().absolute().top_0().left_0().size_full().child(
                div()
                    .id("link-backdrop")
                    .occlude()
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

#[cfg(test)]
mod tests {
    // The field's editing rules on plain strings: typing, deleting and
    // pasting a link copied with the whitespace around it.
    use super::*;

    #[test]
    fn typing_skips_control_characters_and_backspace_deletes() {
        let mut input = LineInput::default();
        input.type_str("https://a");
        input.type_str("\r");
        input.type_str("\t");
        input.backspace();
        assert_eq!(input.text, "https://");
        input.backspace();
        input.type_str("/x");
        assert_eq!(input.text, "https://x");
    }

    #[test]
    fn a_paste_takes_the_first_line_trimmed() {
        let mut input = LineInput::default();
        input.paste("  https://example.com/run.rwd\nsecond line\n");
        assert_eq!(input.text, "https://example.com/run.rwd");
    }
}
