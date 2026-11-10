//! The Open link dialog: a field to paste or type the URL of a .rwd file
//! into, such as one on a case study page, and a button to open it.
//!
//! The field is rewind-text-input (vendor/text-input), a one-line text
//! field adapted from GPUI's own example: typing, selecting with the mouse
//! or the keyboard, and copy, cut and paste.

use std::path::Path;

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
const PLACEHOLDER: &str = "https://\u{2026}/run.rwd";
const NOT_A_LINK: &str = "That is not an http or https link.";

/// What the browser viewer says of where a link can point. A browser
/// fetches a file from another site only when that site allows it, with
/// CORS headers; of GitHub's hosts, raw file links and Pages sites do and
/// release downloads do not.
#[cfg(any(target_family = "wasm", test))]
pub const CORS_NOTE: &str = "The link's site has to let other sites fetch from it: a raw.githubusercontent.com link or a GitHub Pages site does, a GitHub release download does not.";

/// What the dialog says under its title.
#[cfg(not(target_family = "wasm"))]
fn dialog_help() -> String {
    "The link to a .rwd file, such as one on a case study page. A replayable export can be shelled into, debugged and forked once it is in.".to_string()
}

/// What the dialog says under its title in the browser viewer, which
/// fetches the link from the page.
#[cfg(target_family = "wasm")]
fn dialog_help() -> String {
    use crate::ui::page::NOT_UPLOADED;
    format!("The link to a .rwd file, such as one a CI job keeps. {CORS_NOTE} {NOT_UPLOADED}")
}

/// What the browser viewer says of a link it could not open: the server's
/// `answer`, such as "404 Not Found", as it is; or when the browser got
/// none, as it gets none from a site that does not let this one fetch
/// from it, where a link can point.
#[cfg(any(target_family = "wasm", test))]
pub fn unfetched_words(url: &str, answer: Option<&str>) -> String {
    match answer {
        Some(status) => format!("{url} answered {status}."),
        None => format!("The browser could not fetch {url}. {CORS_NOTE}"),
    }
}

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

        // The browser fetches the link from the page; the desktop app
        // downloads it.
        #[cfg(target_family = "wasm")]
        self.open_link(link, cx);
        #[cfg(not(target_family = "wasm"))]
        self.open(std::path::PathBuf::from(link), None, cx);
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
        // The browser lets the page read the clipboard only as it pastes,
        // so there Ctrl+V in the field pastes and the button is left out.
        let paste = (!cfg!(target_family = "wasm")).then(|| {
            button("link-paste", ButtonStyle::Neutral, Availability::Enabled)
                .child("Paste")
                .on_click(cx.listener(|this, _, window, cx| this.paste_link(window, cx)))
        });
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(size::CONTROL_GAP))
            .children(paste)
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
            .child(div().text_color(rgb(theme::SOFT)).child(dialog_help()))
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

#[cfg(test)]
mod tests {
    // What the browser viewer says of a link it could not open.
    use super::*;

    #[test]
    fn a_refused_fetch_names_the_hosts_that_work_and_the_one_that_does_not() {
        // A server's answer is said as it is. No answer means the browser
        // refused the fetch, as it does for a site that does not let this
        // one fetch from it: the words name the hosts that do, and GitHub
        // release downloads as one that does not.
        let refused = unfetched_words("https://github.com/o/r/releases/download/v1/run.rwd", None);
        assert!(
            refused.contains("raw.githubusercontent.com")
                && refused.contains("GitHub Pages")
                && refused.contains("release download"),
            "{refused}"
        );
        let answered = unfetched_words("https://ci.example/run.rwd", Some("404 Not Found"));
        assert_eq!(
            answered,
            "https://ci.example/run.rwd answered 404 Not Found."
        );
    }
}
