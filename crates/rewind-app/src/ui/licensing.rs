//! Registration in the window: the license dialog, the header's license
//! pill, and the reminder an evaluating copy shows now and then.
//!
//! Nothing here blocks work or turns anything off. For its first week an
//! evaluation shows only the pill. After that, the first moment of a day
//! where the app was worth something, a fork that differs from its parent,
//! a shell or gdb pane closed, a jump to the divergence or a finished
//! export, shows the reminder, which closes itself.

use std::time::Duration;

use gpui::{
    Context, CursorStyle, Div, FocusHandle, FontWeight, MouseButton, SharedString, Window, div,
    prelude::*, px, rgb, rgba,
};

use crate::license::{
    self, Coverage, Date, Evaluation, License, LicenseError, Registration, Reminder,
};
use crate::run::Origin;
use crate::selection::{Mapped, Surface, part_of_line};
use crate::theme::{self, size};
use crate::ui::scrubber::{BUY_URL, NoticeAction, NoticeTone, Scrubber};
use crate::ui::selectable::{selectable, selects};
use crate::ui::widgets::{Availability, ButtonStyle, PillTone, button, pill};
use crate::ui::{CloseDialog, CopySelection, LICENSE_CONTEXT, PasteLicense, SelectAll};

/// How long the reminder stays up by itself.
const REMINDER_LIFETIME: Duration = Duration::from_secs(20);

/// The reminder's words, and the line it adds from COMMERCIAL_DAYS on.
const REMINDER_TITLE: &str = "You are evaluating Rewind VM";
const REMINDER_BODY: &str = "It works fully while you evaluate it. A license is $49 for personal use, or $99 per seat at a company, with 3 years of updates.";
const AT_WORK_LINE: &str =
    " Use at a company needs the commercial license, one seat for each person who uses the app.";

/// The pill of a copy without a license.
const EVALUATING_PILL: &str = "Evaluating \u{b7} Buy";
const UPDATES_ENDED_TITLE: &str = "Your license's updates have ended";
const UPDATES_ENDED_PILL: &str = "Updates ended \u{b7} Renew";
const REFUSED_PILL: &str = "License not accepted";

/// The dialog's backdrop: the window behind it, dimmed.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 600.0;
const PASTE_FIELD_HEIGHT: f32 = 220.0;

/// The dialog's titles and what the paste view asks for.
const DIALOG_TITLE: &str = "Enter license";
const DETAILS_TITLE: &str = "License";
const DIALOG_HELP: &str = "Paste the whole block from your email, from the BEGIN line to the END line; a block that checks out registers at once. It is checked on this machine, and nothing is sent anywhere.";

/// What the license dialog shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogView {
    /// The license the app is registered with.
    Details,
    /// The paste field, for a license to register with.
    Paste,
}

/// The license dialog's state: what it shows, what was pasted and what
/// checking it said.
pub struct LicenseDialog {
    pub focus: FocusHandle,
    pub view: DialogView,
    pub text: String,
    pub result: Option<Result<License, LicenseError>>,
}

/// The app's registration and its evaluation's record.
pub struct Licensing {
    pub registration: Registration,
    evaluation: Evaluation,
    pub dialog: Option<LicenseDialog>,
}

impl Licensing {
    pub fn load() -> Licensing {
        Licensing {
            registration: license::load(),
            evaluation: license::load_evaluation(Date::today()),
            dialog: None,
        }
    }

    /// Whether this version runs registered, without reminders.
    fn is_registered(&self) -> bool {
        self.registration.covers_this_version()
    }
}

impl Scrubber {
    /// A moment where the app was worth something: a fork that differs
    /// from its parent, a shell or gdb pane closed, a jump to the
    /// divergence, a finished export. The first of a day reminds an
    /// evaluating copy past its quiet first week, unless a terminal pane is
    /// open, the tour runs or the example is on screen.
    pub(super) fn value_moment(&mut self, cx: &mut Context<Self>) {
        if self.licensing.is_registered() {
            return;
        }
        let example = self
            .session
            .as_ref()
            .is_some_and(|s| s.run.origin == Origin::Example);
        if self.terminal.is_some() || self.tour.is_some() || example {
            return;
        }
        let today = Date::today();
        let Some(reminder) = self.licensing.evaluation.due(today) else {
            return;
        };
        self.licensing.evaluation.reminded = Some(today);
        let _ = license::save_evaluation(&self.licensing.evaluation);
        self.remind(reminder, cx);
    }

    /// Shows the reminder, which closes itself after REMINDER_LIFETIME. A
    /// license whose updates ended before this version gets its own words.
    fn remind(&mut self, reminder: Reminder, cx: &mut Context<Self>) {
        let (title, mut body) = match &self.licensing.registration {
            Registration::Registered(license) => (
                UPDATES_ENDED_TITLE,
                updates_ended_body(license.updates_until),
            ),
            Registration::Unregistered | Registration::Invalid(_) => {
                (REMINDER_TITLE, REMINDER_BODY.to_string())
            }
        };
        if reminder == Reminder::AtWork {
            body.push_str(AT_WORK_LINE);
        }
        let id = self.offer(
            NoticeTone::Info,
            title,
            body,
            vec![
                NoticeAction::Buy,
                NoticeAction::EnterLicense,
                NoticeAction::Dismiss,
            ],
            cx,
        );
        let timer = cx.background_executor().timer(REMINDER_LIFETIME);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.dismiss(id, cx));
        })
        .detach();
    }

    /// Opens the license dialog: the license's details when the app has
    /// one, else the paste field.
    pub(super) fn open_license_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let view = match self.licensing.registration {
            Registration::Registered(_) => DialogView::Details,
            Registration::Unregistered | Registration::Invalid(_) => DialogView::Paste,
        };
        self.licensing.dialog = Some(LicenseDialog {
            focus,
            view,
            text: String::new(),
            result: None,
        });
        cx.notify();
    }

    /// Turns the details into the paste field, for another license.
    fn enter_another_license(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.licensing.dialog {
            dialog.view = DialogView::Paste;
        }
        cx.notify();
    }

    pub(super) fn close_license_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.licensing.dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Puts the clipboard's text in the paste field and checks it,
    /// registering with it when it checks out.
    pub(super) fn paste_license(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let Some(dialog) = &mut self.licensing.dialog else {
            return;
        };
        let result = license::verify(&text);
        let valid = result.is_ok();
        dialog.result = Some(result);
        dialog.text = text;
        if valid {
            self.register(window, cx);
        }
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
                let title = match license.coverage() {
                    Coverage::Current => format!("Registered to {}", license.name),
                    Coverage::EndedBefore(_) => UPDATES_ENDED_TITLE.to_string(),
                };
                let body = match license.coverage() {
                    Coverage::Current => {
                        format!("Thank you. The license is kept in {}.", path.display())
                    }
                    Coverage::EndedBefore(until) => updates_ended_body(until),
                };
                self.notify_user(NoticeTone::Info, title, body, cx);
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
        if self.licensing.is_registered() {
            self.notices.remove_titled(REMINDER_TITLE);
        }
        cx.notify();
    }

    /// The header's license pill, which opens the dialog: that the copy is
    /// evaluating, the licensee, that the license's updates ended before this version,
    /// or that the stored license was refused.
    pub(super) fn render_license_pill(&self, cx: &mut Context<Self>) -> Div {
        let fonts = &self.fonts;
        let (label, tone) = match &self.licensing.registration {
            Registration::Registered(license) => match license.coverage() {
                Coverage::Current => (license.name.clone(), PillTone::Quiet),
                Coverage::EndedBefore(_) => (UPDATES_ENDED_PILL.to_string(), PillTone::Quiet),
            },
            Registration::Unregistered => (EVALUATING_PILL.to_string(), PillTone::Quiet),
            Registration::Invalid(_) => (REFUSED_PILL.to_string(), PillTone::Failed),
        };
        div().child(
            div()
                .id("license-pill")
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(pill(label, tone, fonts))
                .on_click(cx.listener(|this, _, window, cx| this.open_license_dialog(window, cx))),
        )
    }

    /// The license dialog over a dimmed window, when it is open.
    pub(super) fn render_license_dialog(&self, cx: &mut Context<Self>) -> Option<Div> {
        let dialog = self.licensing.dialog.as_ref()?;
        let mono = self.fonts.mono.clone();
        let registry = self.selecting.registry.clone();
        let range = self.selected_range(Surface::LicenseDialog);
        let lines = dialog_lines(dialog.view, &self.licensing.registration, &dialog.result);
        let line = |i: usize, (color, text): (u32, String)| {
            let part = range.as_ref().and_then(|r| part_of_line(r, i, text.len()));
            div()
                .text_color(rgb(color))
                .cursor(CursorStyle::IBeam)
                .child(selectable(Surface::LicenseDialog, i, text, part, &registry))
        };

        // The paste field: what was pasted, or how to paste, ringed as the
        // place a paste goes while the dialog has the keyboard.
        let field_text: SharedString = if dialog.text.is_empty() {
            "Press Ctrl+v to paste the license block from your email.".into()
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
            .h(px(PASTE_FIELD_HEIGHT))
            .overflow_y_scroll()
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgba(theme::FOCUS_RING_A))
            .font_family(mono)
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(field_color))
            .child(field_text);

        // Buy on the left, for someone without a license or whose updates
        // ended; on the right, pasting, which registers, or entering
        // another license, and closing.
        let covered = self.licensing.is_registered();
        let buy = (!covered).then(|| {
            button("license-buy", ButtonStyle::Neutral, Availability::Enabled)
                .child("Buy a license")
                .on_click(|_, _, cx| cx.open_url(BUY_URL))
        });
        let close = button(
            "license-cancel",
            ButtonStyle::Neutral,
            Availability::Enabled,
        )
        .child(match dialog.view {
            DialogView::Details => "Close",
            DialogView::Paste => "Cancel",
        })
        .on_click(cx.listener(|this, _, window, cx| this.close_license_dialog(window, cx)));
        let act = match dialog.view {
            DialogView::Paste => {
                button("license-paste", ButtonStyle::Primary, Availability::Enabled)
                    .child("Paste")
                    .on_click(cx.listener(|this, _, window, cx| this.paste_license(window, cx)))
            }
            DialogView::Details => button(
                "license-another",
                ButtonStyle::Neutral,
                Availability::Enabled,
            )
            .child("Enter another license")
            .on_click(cx.listener(|this, _, _, cx| this.enter_another_license(cx))),
        };
        let buttons = div()
            .flex()
            .justify_between()
            .child(div().children(buy))
            .child(
                div()
                    .flex()
                    .gap(px(size::CONTROL_GAP))
                    .child(close)
                    .child(act),
            );

        // The title, then the text, with the paste field after the help.
        let mut lines = lines.into_iter().enumerate();
        let mut card = div()
            .id("license-card")
            .track_focus(&dialog.focus)
            .key_context(LICENSE_CONTEXT)
            .on_action(
                cx.listener(|this, _: &PasteLicense, window, cx| this.paste_license(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &CloseDialog, window, cx| {
                this.close_license_dialog(window, cx)
            }))
            .w(px(DIALOG_WIDTH))
            .flex()
            .flex_col()
            .gap(px(size::SECTION_GAP))
            .p(px(size::CARD_PAD * 1.5))
            .rounded(px(size::RADIUS_CARD))
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .shadow_lg();
        if let Some((i, title)) = lines.next() {
            card = card.child(
                line(i, title)
                    .text_size(px(size::TEXT_BRAND))
                    .font_weight(FontWeight::SEMIBOLD),
            );
        }
        let body_before_field = match dialog.view {
            DialogView::Paste => 1,
            DialogView::Details => usize::MAX,
        };
        for (i, text) in lines.by_ref().take(body_before_field) {
            card = card.child(line(i, text));
        }
        if dialog.view == DialogView::Paste {
            card = card.child(field);
        }
        for (i, text) in lines {
            card = card.child(line(i, text));
        }
        card = card.child(buttons);
        let card = selects(card, Surface::LicenseDialog, cx);

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

impl Scrubber {
    /// The dialog's text lines, as the selection sees them.
    pub(super) fn license_dialog_lines(&self) -> Vec<Mapped> {
        let Some(dialog) = &self.licensing.dialog else {
            return Vec::new();
        };
        dialog_lines(dialog.view, &self.licensing.registration, &dialog.result)
            .into_iter()
            .map(|(_, text)| Mapped::plain(text))
            .collect()
    }
}

/// The dialog's text, each line in its color: the title, then for the
/// details the license's fields and whether it covers this version, or
/// for pasting the help, why a stored license was refused, and the
/// verdict on what was pasted.
fn dialog_lines(
    view: DialogView,
    registration: &Registration,
    result: &Option<Result<License, LicenseError>>,
) -> Vec<(u32, String)> {
    if let (DialogView::Details, Registration::Registered(license)) = (view, registration) {
        let seats = if license.seats == 1 { "seat" } else { "seats" };
        let covers = match license.coverage() {
            Coverage::Current => format!(
                "It covers this version, released {}.",
                license::RELEASE_DATE
            ),
            Coverage::EndedBefore(until) => updates_ended_body(until),
        };
        return vec![
            (theme::TEXT, DETAILS_TITLE.to_string()),
            (
                theme::SOFT,
                format!("Registered to {} <{}>", license.name, license.email),
            ),
            (
                theme::SOFT,
                format!(
                    "{}, {} {seats} \u{b7} id {}",
                    license.edition.as_str(),
                    license.seats,
                    license.id
                ),
            ),
            (
                theme::SOFT,
                format!(
                    "Issued {} \u{b7} updates until {}",
                    license.issued, license.updates_until
                ),
            ),
            (theme::SOFT, covers),
        ];
    }

    let mut lines = vec![
        (theme::TEXT, DIALOG_TITLE.to_string()),
        (theme::SOFT, DIALOG_HELP.to_string()),
    ];
    if let Registration::Invalid(reason) = registration {
        lines.push((
            theme::RED_SOFT,
            format!("The stored license was not accepted: {reason}"),
        ));
    }
    lines.extend(verdict(result));
    lines
}

/// What a license whose updates ended before this version means.
fn updates_ended_body(until: license::Date) -> String {
    format!(
        "It registers the versions released until {until}. This one, released {}, runs fully as an evaluation; a new license covers it and 3 more years of updates.",
        license::RELEASE_DATE
    )
}

/// What checking the pasted text said, in its color.
fn verdict(result: &Option<Result<License, LicenseError>>) -> Option<(u32, String)> {
    match result {
        None => None,
        Some(Ok(license)) => {
            let coverage = match license.coverage() {
                Coverage::Current => String::new(),
                Coverage::EndedBefore(until) => format!(" {}", updates_ended_body(until)),
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
    }
}

#[cfg(test)]
mod tests {
    // The license dialog's text, from the registration alone: what a
    // registered user is shown about their license, and what a user whose
    // stored license was refused is told before pasting another.
    use super::*;

    fn licensed(until: &str) -> License {
        License {
            name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            edition: license::Edition::Commercial,
            seats: 3,
            id: "0123456789abcdef".into(),
            issued: license::Date::parse("2026-09-30").unwrap(),
            updates_until: license::Date::parse(until).unwrap(),
        }
    }

    fn texts(lines: Vec<(u32, String)>) -> Vec<String> {
        lines.into_iter().map(|(_, text)| text).collect()
    }

    #[test]
    fn a_registered_user_sees_their_license() {
        // The license's fields, and that it covers this version.
        let registered = Registration::Registered(licensed("2099-01-01"));
        let lines = texts(dialog_lines(DialogView::Details, &registered, &None));
        assert_eq!(
            lines,
            [
                "License".to_string(),
                "Registered to Ada Lovelace <ada@example.com>".to_string(),
                "Commercial, 3 seats \u{b7} id 0123456789abcdef".to_string(),
                "Issued 2026-09-30 \u{b7} updates until 2099-01-01".to_string(),
                format!(
                    "It covers this version, released {}.",
                    license::RELEASE_DATE
                ),
            ]
        );
    }

    #[test]
    fn a_license_whose_updates_ended_says_so_in_the_details() {
        // The last line says this version runs as an evaluation.
        let registered = Registration::Registered(licensed("2020-01-01"));
        let lines = texts(dialog_lines(DialogView::Details, &registered, &None));
        let until = license::Date::parse("2020-01-01").unwrap();
        assert_eq!(lines.last(), Some(&updates_ended_body(until)));
    }

    #[test]
    fn a_refused_stored_license_is_explained_before_pasting() {
        // The paste view leads with why the stored license was refused,
        // then the verdict on what was pasted, if anything was.
        let refused = Registration::Invalid("License 0123 has been revoked.".into());
        let lines = texts(dialog_lines(DialogView::Paste, &refused, &None));
        assert_eq!(
            lines,
            [
                DIALOG_TITLE.to_string(),
                DIALOG_HELP.to_string(),
                "The stored license was not accepted: License 0123 has been revoked.".to_string(),
            ]
        );
        let pasted = Some(Err(LicenseError::NoBlock));
        let lines = texts(dialog_lines(DialogView::Paste, &refused, &pasted));
        assert_eq!(lines.last(), Some(&LicenseError::NoBlock.to_string()));
    }
}
