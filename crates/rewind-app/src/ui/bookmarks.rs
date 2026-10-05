//! Bookmarks on screen: b marks the playhead's step with a note, a card in
//! "At this step" lists the run's bookmarks and goes to one on a click,
//! and the timeline marks each one. `crate::bookmarks` keeps them in the
//! run's directory.

use std::path::PathBuf;

use gpui::{
    Context, Div, Entity, Focusable, FontWeight, SharedString, Window, div, prelude::*, px,
    relative, rgb, rgba,
};
use rewind_text_input::{TextInput, TextInputStyle};

use crate::describe::{clip, thousands};
use crate::run::{Origin, Run};
use crate::theme::{self, size};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::widgets::{Availability, ButtonStyle, button, tooltip};
use crate::ui::{BOOKMARK_CONTEXT, CloseDialog, SaveBookmark};

/// The dialog's backdrop and width, as the Open link dialog's.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 520.0;

/// How many characters of a note the card shows.
const MAX_NOTE_CHARS: usize = 60;

/// The bookmark marks on the timeline: a small square above the track.
const MARK_SIZE: f32 = 7.0;

/// What the card's add row says it does.
const ADD_NOTE: &str =
    "Marks this step with a note, kept with the run and carried in its exports. Key: b.";

/// The note dialog while it is open: the step it marks and its field.
pub struct BookmarkEditor {
    pub step: u64,
    pub input: Entity<TextInput>,
    /// Whether the step had a bookmark when the dialog opened.
    pub existed: bool,
}

/// Where a run's bookmarks are kept: the directory of a run on this
/// machine or of an export unpacked into the cache. The example and a
/// bare trace have none.
pub fn bookmarks_dir(run: &Run) -> Option<PathBuf> {
    match run.origin {
        Origin::Local | Origin::Export(_) => Some(run.path.clone()),
        Origin::Example | Origin::TraceFile => None,
    }
}

impl Scrubber {
    /// Opens the note dialog for the playhead's step, with its note when
    /// it has one, selected so typing replaces it.
    pub(super) fn open_bookmark_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let style = TextInputStyle {
            placeholder: "A note, or nothing".into(),
            placeholder_color: rgb(theme::MUTED).into(),
            caret_color: rgb(theme::AMBER).into(),
            selection_color: rgba(theme::FOCUS_RING_A).into(),
        };
        let step = self.step;
        let existing = self.bookmarks.at(step).map(|b| b.note.clone());
        let input = cx.new(|cx| TextInput::new(style, cx));
        if let Some(note) = &existing {
            input.update(cx, |input, cx| {
                input.insert(note, window, cx);
                input.select_everything(cx);
            });
        }
        window.focus(&input.focus_handle(cx), cx);
        self.bookmark_editor = Some(BookmarkEditor {
            step,
            input,
            existed: existing.is_some(),
        });
        cx.notify();
    }

    pub(super) fn close_bookmark_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bookmark_editor = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Saves the dialog's note on its step and closes it.
    fn save_bookmark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.bookmark_editor else {
            return;
        };
        let note = editor.input.read(cx).text().trim().to_string();
        self.bookmarks.set(editor.step, note);
        self.close_bookmark_editor(window, cx);
        self.keep_bookmarks(cx);
    }

    /// Removes the dialog's bookmark and closes it.
    fn remove_bookmark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.bookmark_editor else {
            return;
        };
        self.bookmarks.remove(editor.step);
        self.close_bookmark_editor(window, cx);
        self.keep_bookmarks(cx);
    }

    /// Writes the bookmarks to the run's directory, or says why they are
    /// kept only until the run is closed.
    fn keep_bookmarks(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(dir) = bookmarks_dir(&session.run) else {
            self.notify_user(
                NoticeTone::Info,
                "Bookmarks are not saved for this run",
                "The example and a bare trace have no run directory to keep them in; they last until another run is opened.",
                cx,
            );
            return;
        };
        if let Err(e) = self.bookmarks.save(&dir) {
            self.notify_user(
                NoticeTone::Error,
                "Could not save the bookmarks",
                format!("{}: {e}", dir.display()),
                cx,
            );
        }
    }

    /// The card listing the run's bookmarks, each a link to its step, and
    /// a row to mark the playhead's step.
    pub(super) fn render_bookmarks_card(&self, cx: &mut Context<Self>) -> Div {
        let mut card = div().flex().flex_col().gap(px(size::CARD_GAP / 2.0)).child(
            div()
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(theme::MUTED))
                .child(match self.bookmarks.len() {
                    0 => "BOOKMARKS".to_string(),
                    n => format!("BOOKMARKS \u{b7} {n}"),
                }),
        );
        for (i, mark) in self.bookmarks.iter().enumerate() {
            let step = mark.step;
            let here = step == self.step;
            let note = if mark.note.is_empty() {
                "no note".to_string()
            } else {
                clip(&mark.note, MAX_NOTE_CHARS)
            };
            card = card.child(
                div()
                    .id(SharedString::from(format!("bookmark-{i}")))
                    .flex()
                    .gap(px(size::LIST_COLUMN_GAP))
                    .px(px(size::CARD_GAP / 2.0))
                    .rounded(px(size::RADIUS_MENU_ITEM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                    .when(here, |d| d.bg(rgb(theme::ROW_NOW)))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx)))
                    .child(
                        div()
                            .flex_none()
                            .font_family(self.fonts.mono.clone())
                            .text_color(rgb(if here { theme::AMBER } else { theme::FAINT }))
                            .child(thousands(step)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(rgb(if mark.note.is_empty() {
                                theme::MUTED
                            } else {
                                theme::SOFT
                            }))
                            .child(note),
                    ),
            );
        }
        let add = if self.bookmarks.at(self.step).is_some() {
            "Edit this step's note (b)"
        } else {
            "Bookmark this step (b)"
        };
        card.child(
            div()
                .id("bookmark-add")
                .px(px(size::CARD_GAP / 2.0))
                .cursor_pointer()
                .text_color(rgb(theme::AMBER))
                .hover(|s| s.text_color(rgb(theme::AMBER_HI)))
                .tooltip(tooltip(ADD_NOTE))
                .on_click(cx.listener(|this, _, window, cx| this.open_bookmark_editor(window, cx)))
                .child(add),
        )
    }

    /// The bookmarks' marks on the track, as children positioned by step.
    pub(super) fn bookmark_marks(&self, fraction_of: impl Fn(u64) -> f32) -> Vec<Div> {
        self.bookmarks
            .iter()
            .map(|mark| {
                div()
                    .absolute()
                    .left(relative(fraction_of(mark.step)))
                    .ml(px(-MARK_SIZE / 2.0))
                    .top(px(-size::MARKER_OVERHANG - MARK_SIZE))
                    .size(px(MARK_SIZE))
                    .rounded(px(1.0))
                    .bg(rgb(theme::GREEN_SOFT))
            })
            .collect()
    }

    /// The note dialog over a dimmed window, when it is open.
    pub(super) fn render_bookmark_editor(&self, cx: &mut Context<Self>) -> Option<Div> {
        let editor = self.bookmark_editor.as_ref()?;
        let field = div()
            .key_context(BOOKMARK_CONTEXT)
            .on_action(
                cx.listener(|this, _: &SaveBookmark, window, cx| this.save_bookmark(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseDialog, window, cx| {
                this.close_bookmark_editor(window, cx)
            }))
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .text_color(rgb(theme::TEXT))
            .child(editor.input.clone());
        let remove = if editor.existed {
            Availability::Enabled
        } else {
            Availability::Disabled
        };
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(size::CONTROL_GAP))
            .child(
                button("bookmark-remove", ButtonStyle::Neutral, remove)
                    .child("Remove")
                    .on_click(cx.listener(|this, _, window, cx| this.remove_bookmark(window, cx))),
            )
            .child(
                button(
                    "bookmark-cancel",
                    ButtonStyle::Neutral,
                    Availability::Enabled,
                )
                .child("Cancel")
                .on_click(
                    cx.listener(|this, _, window, cx| this.close_bookmark_editor(window, cx)),
                ),
            )
            .child(
                button("bookmark-save", ButtonStyle::Primary, Availability::Enabled)
                    .child("Save")
                    .on_click(cx.listener(|this, _, window, cx| this.save_bookmark(window, cx))),
            );
        let card = div()
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
                    .child(format!("Bookmark at step {}", thousands(editor.step))),
            )
            .child(
                div()
                    .text_color(rgb(theme::SOFT))
                    .child("Enter saves the note; Escape leaves the bookmarks as they were."),
            )
            .child(field)
            .child(buttons);
        Some(
            div().absolute().top_0().left_0().size_full().child(
                div()
                    .id("bookmark-backdrop")
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
