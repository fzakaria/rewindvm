//! The step readout as a field: a click on it, or g, turns the step number
//! into a text field to type a step into, and Enter jumps there.
//!
//! `crate::step_entry` reads what was typed; this draws the field and acts
//! on it.

use gpui::{
    Context, Entity, Focusable, FontWeight, Subscription, Window, div, prelude::*, px, rgb, rgba,
};
use rewind_text_input::{TextInput, TextInputStyle};

use crate::describe::thousands;
use crate::step_entry::{StepEntryError, parse_step};
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::tooltip;
use crate::ui::{CloseDialog, ConfirmStep, STEP_CONTEXT};

/// How wide the field is, in characters of the monospace font: a step past
/// the billions with its commas.
const FIELD_CHARS: f32 = 14.0;

/// What the readout says it does when pointed at.
const READOUT_NOTE: &str = "Click, or press g, to type a step to go to: a number such as 3,495, or +100 and -100 to move from the playhead. Enter goes there and Escape cancels.";

/// The field while it is open: what is typed, and why the last Enter did
/// not go anywhere.
pub struct StepEntry {
    pub input: Entity<TextInput>,
    pub error: Option<StepEntryError>,
    /// Closes the field when it loses the keyboard.
    _blur: Subscription,
}

impl Scrubber {
    /// Opens the field over the step readout with the playhead's step in
    /// it, selected, so typing replaces it.
    pub(super) fn open_step_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let style = TextInputStyle {
            placeholder: "step".into(),
            placeholder_color: rgb(theme::MUTED).into(),
            caret_color: rgb(theme::AMBER).into(),
            selection_color: rgba(theme::FOCUS_RING_A).into(),
        };
        let input = cx.new(|cx| TextInput::new(style, cx));
        let step = self.step.to_string();
        input.update(cx, |input, cx| {
            input.insert(&step, window, cx);
            input.select_everything(cx);
        });
        let handle = input.focus_handle(cx);
        window.focus(&handle, cx);
        let blur = cx.on_blur(&handle, window, |this, _, cx| {
            this.step_entry = None;
            cx.notify();
        });
        self.step_entry = Some(StepEntry {
            input,
            error: None,
            _blur: blur,
        });
        cx.notify();
    }

    /// Closes the field and gives the keyboard back to the scrubber.
    pub(super) fn close_step_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step_entry = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Jumps to the step typed, or says why not and keeps the field open.
    fn confirm_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(entry), Some(session)) = (&mut self.step_entry, &self.session) else {
            return;
        };
        let typed = entry.input.read(cx).text().to_string();
        match parse_step(&typed, self.step, session.run.timeline.total) {
            Ok(step) => {
                self.close_step_entry(window, cx);
                self.jump_to(step, cx);
            }
            Err(error) => {
                entry.error = Some(error);
                cx.notify();
            }
        }
    }

    /// The step readout: the step and the run's length, or the field while
    /// it is open.
    pub(super) fn render_step_readout(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.session.as_ref().map_or(0, |s| s.run.timeline.total);
        let caption = div().text_color(rgb(theme::MUTED)).child("step");
        let of_total = div()
            .text_color(rgb(theme::AMBER))
            .font_weight(FontWeight::SEMIBOLD)
            .child(format!("/ {}", thousands(total)));

        let Some(entry) = &self.step_entry else {
            return div()
                .id("step-readout")
                .flex()
                .flex_none()
                .gap(px(size::READOUT_INNER_GAP))
                .whitespace_nowrap()
                .cursor_text()
                .tooltip(tooltip(READOUT_NOTE))
                .on_click(cx.listener(|this, _, window, cx| this.open_step_entry(window, cx)))
                .child(caption)
                .child(
                    div()
                        .text_color(rgb(theme::AMBER))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(thousands(self.step)),
                )
                .child(of_total);
        };

        // The field: Enter jumps and Escape cancels, from the key context
        // around it; the field's own keys edit.
        let border = if entry.error.is_some() {
            theme::RED_BORDER
        } else {
            theme::AMBER_DEEP
        };
        let field = div()
            .key_context(STEP_CONTEXT)
            .on_action(
                cx.listener(|this, _: &ConfirmStep, window, cx| this.confirm_step(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseDialog, window, cx| this.close_step_entry(window, cx)),
            )
            .w(px(FIELD_CHARS * size::MONO_CHAR_WIDTH))
            .px(px(size::READOUT_INNER_GAP))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(border))
            .text_color(rgb(theme::AMBER))
            .child(entry.input.clone());
        let mut readout = div()
            .id("step-readout")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(size::READOUT_INNER_GAP))
            .whitespace_nowrap()
            .child(caption)
            .child(field)
            .child(of_total);
        if let Some(error) = entry.error {
            readout = readout.child(
                div()
                    .text_color(rgb(theme::RED_SOFT))
                    .child(error.to_string()),
            );
        }
        readout
    }
}
