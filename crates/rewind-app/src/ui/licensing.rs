//! Registration in the window: the license dialog, the header's license
//! pill, and the reminder an unregistered copy shows now and then.
//!
//! Nothing here blocks work or turns anything off. The reminder is a
//! notice like any other, and the dialog closes with Escape.

use std::time::{Duration, Instant};

use gpui::{
    Context, Div, FocusHandle, FontWeight, MouseButton, SharedString, Window, div, prelude::*, px,
    rgb, rgba,
};

use crate::license::{self, Coverage, License, LicenseError, Registration, Reminder};
use crate::theme::{self, size};
use crate::ui::scrubber::{NoticeAction, NoticeTone, Scrubber};
use crate::ui::widgets::{Availability, ButtonStyle, PillTone, button, pill};
use crate::ui::{CloseDialog, LICENSE_CONTEXT, PasteLicense};

/// How often the reminder's clock is checked.
const REMINDER_CHECK: Duration = Duration::from_secs(60);

/// The reminder's words.
const REMINDER_TITLE: &str = "Rewind VM is unregistered";
const REMINDER_BODY: &str =
    "It works fully while you evaluate it; a license is $49 personal or $99 per seat.";

/// The dialog's backdrop: the window behind it, dimmed.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 600.0;
const PASTE_FIELD_HEIGHT: f32 = 220.0;

/// The license dialog's state: what was pasted and what checking it said.
pub struct LicenseDialog {
    pub focus: FocusHandle,
    pub text: String,
    pub result: Option<Result<License, LicenseError>>,
}

/// The app's registration and when to remind.
pub struct Licensing {
    pub registration: Registration,
    reminder: Reminder,
    started: Instant,
    pub dialog: Option<LicenseDialog>,
}

impl Licensing {
    pub fn load() -> Licensing {
        Licensing {
            registration: license::load(),
            reminder: Reminder::default(),
            started: Instant::now(),
            dialog: None,
        }
    }

    fn is_registered(&self) -> bool {
        matches!(self.registration, Registration::Registered(_))
    }
}

impl Scrubber {
    /// Starts the reminder's clock, and says so when a stored license
    /// did not check out.
    pub(super) fn start_licensing(&mut self, cx: &mut Context<Self>) {
        if let Registration::Invalid(reason) = &self.licensing.registration {
            let reason = reason.clone();
            self.notify_user(NoticeTone::Error, "License not accepted", reason, cx);
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REMINDER_CHECK).await;
                let alive = this.update(cx, |this, cx| {
                    let now = this.licensing.started.elapsed();
                    if !this.licensing.is_registered() && this.licensing.reminder.tick(now) {
                        this.remind(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Counts an engine action toward the reminder.
    pub(super) fn count_engine_action(&mut self, cx: &mut Context<Self>) {
        if self.licensing.is_registered() {
            return;
        }
        let now = self.licensing.started.elapsed();
        if self.licensing.reminder.action(now) {
            self.remind(cx);
        }
    }

    /// Shows the reminder, unless one is already up.
    fn remind(&mut self, cx: &mut Context<Self>) {
        let showing = self
            .notices
            .iter()
            .any(|n| n.title.as_ref() == REMINDER_TITLE);
        if showing {
            return;
        }
        self.offer(
            NoticeTone::Info,
            REMINDER_TITLE,
            REMINDER_BODY,
            vec![
                NoticeAction::Buy,
                NoticeAction::EnterLicense,
                NoticeAction::Dismiss,
            ],
            cx,
        );
    }

    pub(super) fn open_license_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.licensing.dialog = Some(LicenseDialog {
            focus,
            text: String::new(),
            result: None,
        });
        cx.notify();
    }

    pub(super) fn close_license_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.licensing.dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Puts the clipboard's text in the paste field and checks it.
    pub(super) fn paste_license(&mut self, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let Some(dialog) = &mut self.licensing.dialog else {
            return;
        };
        dialog.result = Some(license::verify(&text));
        dialog.text = text;
        cx.notify();
    }

    /// Stores a license that checked out and closes the dialog.
    fn register(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.licensing.dialog else {
            return;
        };
        let Some(Ok(license)) = dialog.result.clone() else {
            return;
        };
        let text = dialog.text.clone();
        self.close_license_dialog(window, cx);
        match license::save(&text) {
            Ok(path) => {
                self.notify_user(
                    NoticeTone::Info,
                    format!("Registered to {}", license.name),
                    format!("Thank you. The license is kept in {}.", path.display()),
                    cx,
                );
            }
            Err(e) => {
                self.notify_user(
                    NoticeTone::Error,
                    "License checked out but was not saved",
                    format!("{e}. The app is registered until it quits."),
                    cx,
                );
            }
        }
        self.licensing.registration = Registration::Registered(license);
        self.notices.retain(|n| n.title.as_ref() != REMINDER_TITLE);
        cx.notify();
    }

    /// The header's license pill: "Unregistered", which opens the dialog,
    /// or the licensee, with the versions an older license covers.
    pub(super) fn render_license_pill(&self, cx: &mut Context<Self>) -> Div {
        let fonts = &self.fonts;
        let label = match &self.licensing.registration {
            Registration::Registered(license) => match license.coverage() {
                Coverage::Current => license.name.clone(),
                Coverage::EndedBefore(until) => {
                    format!(
                        "{} \u{b7} License covers versions until {until}",
                        license.name
                    )
                }
            },
            Registration::Unregistered | Registration::Invalid(_) => "Unregistered".to_string(),
        };
        div().child(
            div()
                .id("license-pill")
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(pill(label, PillTone::Quiet, fonts))
                .on_click(cx.listener(|this, _, window, cx| this.open_license_dialog(window, cx))),
        )
    }

    /// The license dialog over a dimmed window, when it is open.
    pub(super) fn render_license_dialog(&self, cx: &mut Context<Self>) -> Option<Div> {
        let dialog = self.licensing.dialog.as_ref()?;
        let mono = self.fonts.mono.clone();

        // The paste field: what was pasted, or how to paste.
        let focused_border = rgba(theme::FOCUS_RING_A);
        let field_text: SharedString = if dialog.text.is_empty() {
            "Press Ctrl+V to paste the license block from your email.".into()
        } else {
            dialog.text.clone().into()
        };
        let field_color = if dialog.text.is_empty() {
            theme::MUTED
        } else {
            theme::SOFT
        };
        let field = div()
            .id("license-field")
            .track_focus(&dialog.focus)
            .key_context(LICENSE_CONTEXT)
            .on_action(cx.listener(|this, _: &PasteLicense, _, cx| this.paste_license(cx)))
            .on_action(cx.listener(|this, _: &CloseDialog, window, cx| {
                this.close_license_dialog(window, cx)
            }))
            .h(px(PASTE_FIELD_HEIGHT))
            .overflow_y_scroll()
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .focus(move |s| s.border_color(focused_border))
            .font_family(mono)
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(field_color))
            .child(field_text);

        // What checking the pasted text said.
        let verdict = match &dialog.result {
            None => None,
            Some(Ok(license)) => {
                let coverage = match license.coverage() {
                    Coverage::Current => String::new(),
                    Coverage::EndedBefore(until) => {
                        format!(" Updates ended {until}; this version still registers.")
                    }
                };
                Some((
                    theme::GREEN_SOFT,
                    format!(
                        "Valid: {} ({}, {} seat{}), updates until {}.{coverage}",
                        license.name,
                        license.edition.as_str(),
                        license.seats,
                        if license.seats == 1 { "" } else { "s" },
                        license.updates_until
                    ),
                ))
            }
            Some(Err(e)) => Some((theme::RED_SOFT, e.to_string())),
        };
        let can_register = if matches!(dialog.result, Some(Ok(_))) {
            Availability::Enabled
        } else {
            Availability::Disabled
        };

        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(size::CONTROL_GAP))
            .child(
                button("license-paste", ButtonStyle::Neutral, Availability::Enabled)
                    .child("Paste")
                    .on_click(cx.listener(|this, _, _, cx| this.paste_license(cx))),
            )
            .child(
                button(
                    "license-cancel",
                    ButtonStyle::Neutral,
                    Availability::Enabled,
                )
                .child("Cancel")
                .on_click(cx.listener(|this, _, window, cx| this.close_license_dialog(window, cx))),
            )
            .child(
                button("license-register", ButtonStyle::Primary, can_register)
                    .child("Register")
                    .on_click(cx.listener(|this, _, window, cx| this.register(window, cx))),
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
                    .child("Enter license"),
            )
            .child(
                div()
                    .text_color(rgb(theme::SOFT))
                    .child("Paste the whole block, from the BEGIN line to the END line. It is checked on this machine; nothing is sent anywhere."),
            )
            .child(field);
        if let Some((color, text)) = verdict {
            card = card.child(div().text_color(rgb(color)).child(text));
        }
        card = card.child(buttons);

        Some(
            div()
                .id("license-backdrop")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(BACKDROP_A))
                .child(card)
                .into_any_element(),
        )
        .map(|backdrop| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(backdrop)
        })
    }
}
