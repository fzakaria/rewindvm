//! The Open link dialog: a field to paste or type the URL of a .rwd file
//! into, such as one on a case study page, and a button to open it.
//!
//! The field is rewind-text-input (vendor/text-input), a one-line text
//! field adapted from GPUI's own example: typing, selecting with the mouse
//! or the keyboard, and copy, cut and paste.

use std::path::{Path, PathBuf};

use gpui::{
    Context, Div, Entity, Focusable, FontWeight, SharedString, Window, div, prelude::*, px, rgb,
    rgba,
};
use rewind_text_input::{TextInput, TextInputStyle};

use crate::archive;
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::{Availability, ButtonStyle, button};
use crate::ui::{CloseDialog, ConfirmLink, LINK_CONTEXT};

/// The dialog's backdrop and width, as the license dialog's.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 600.0;

/// The dialog's words.
const DIALOG_TITLE: &str = "Open link";
const DIALOG_HELP: &str = "The link to a .rwd file, such as one on a case study page. A replayable export can be shelled into, debugged and forked once it is in.";
const PLACEHOLDER: &str = "https://\u{2026}/run.rwd";
const NOT_A_LINK: &str = "That is not an http or https link.";

/// The dialog's state: the field, and why its link was not opened.
pub struct LinkDialog {
    pub input: Entity<TextInput>,
    pub error: Option<&'static str>,
}

impl Scrubber {
    /// Opens the dialog, with the clipboard's link in the field when there
    /// is one, and the field focused.
    pub(super) fn open_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let style = TextInputStyle {
            placeholder: PLACEHOLDER.into(),
            placeholder_color: rgb(theme::MUTED).into(),
            caret_color: rgb(theme::AMBER).into(),
            selection_color: rgba(theme::FOCUS_RING_A).into(),
        };
        let input = cx.new(|cx| TextInput::new(style, cx));
        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        if archive::is_url(Path::new(clipboard.trim())) {
            input.update(cx, |input, cx| input.insert(&clipboard, window, cx));
        }
        window.focus(&input.focus_handle(cx), cx);
        self.link_dialog = Some(LinkDialog { input, error: None });
        cx.notify();
    }

    pub(super) fn close_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.link_dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Puts the clipboard's text in the field, as Ctrl+V does there.
    fn paste_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pasted = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let Some(dialog) = &mut self.link_dialog else {
            return;
        };
        dialog.error = None;
        let input = dialog.input.clone();
        input.update(cx, |input, cx| input.insert(&pasted, window, cx));
        window.focus(&input.focus_handle(cx), cx);
    }

    /// Opens the link in the field, or says why it cannot.
    fn confirm_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.link_dialog else {
            return;
        };
        let link = dialog.input.read(cx).text().trim().to_string();
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
        let typed = !dialog.input.read(cx).text().trim().is_empty();

        // The field: Enter opens and Escape closes, from the key context
        // around it; the field's own keys edit.
        let field = div()
            .id("link-field")
            .key_context(LINK_CONTEXT)
            .on_action(
                cx.listener(|this, _: &ConfirmLink, window, cx| this.confirm_link(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseDialog, window, cx| this.close_link_dialog(window, cx)),
            )
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(theme::TEXT))
            .child(dialog.input.clone());

        let can_open = if typed {
            Availability::Enabled
        } else {
            Availability::Disabled
        };
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(size::CONTROL_GAP))
            .child(
                button("link-paste", ButtonStyle::Neutral, Availability::Enabled)
                    .child("Paste")
                    .on_click(cx.listener(|this, _, window, cx| this.paste_link(window, cx))),
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
            card = card.child(
                div()
                    .text_color(rgb(theme::RED_SOFT))
                    .child(SharedString::from(error)),
            );
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
