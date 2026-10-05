//! Bookmarks on screen: b marks the playhead's step with a note. The
//! timeline marks each bookmark, and a mark goes to its step on a click
//! and shows its note when pointed at; "At this step" shows the note of
//! the playhead's step; the Bookmarks tab lists them all. `crate::bookmarks`
//! keeps them in the run's directory.

use std::path::PathBuf;

use gpui::{
    Context, Div, Entity, Focusable, FontWeight, MouseButton, SharedString, Stateful, Window, div,
    prelude::*, px, relative, rgb, rgba,
};
use rewind_text_input::{TextInput, TextInputStyle};

use crate::describe::thousands;
use crate::run::{Origin, Run};
use crate::theme::{self, layout, size};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{Availability, ButtonStyle, button, panel_title, tooltip};
use crate::ui::{BOOKMARK_CONTEXT, CloseDialog, SaveBookmark};
use crate::view::View;

/// The dialog's backdrop and width, as the Open link dialog's.
const BACKDROP_A: u32 = 0x0000_00a0;
const DIALOG_WIDTH: f32 = 520.0;

/// The width of the Bookmarks tab's step column, in characters.
const STEP_CHARS: f32 = 9.0;

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
    /// Opens the note dialog for the playhead's step.
    pub(super) fn open_bookmark_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_bookmark_at(self.step, window, cx);
    }

    /// Opens the note dialog for `step`, with its note when it has one,
    /// selected so typing replaces it.
    fn edit_bookmark_at(&mut self, step: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let style = TextInputStyle {
            placeholder: "A note, or nothing".into(),
            placeholder_color: rgb(theme::MUTED).into(),
            caret_color: rgb(theme::AMBER).into(),
            selection_color: rgba(theme::FOCUS_RING_A).into(),
        };
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
        let step = editor.step;
        self.close_bookmark_editor(window, cx);
        self.remove_bookmark_at(step, cx);
    }

    /// Removes the bookmark at `step`.
    fn remove_bookmark_at(&mut self, step: u64, cx: &mut Context<Self>) {
        self.bookmarks.remove(step);
        self.keep_bookmarks(cx);
        cx.notify();
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

    /// What "At this step" says of bookmarks: the note of the playhead's
    /// step when it has one, with a link to edit it, then links to
    /// bookmark the step and to the Bookmarks tab.
    pub(super) fn render_bookmark_here(&self, cx: &mut Context<Self>) -> Div {
        let mut column = div().flex().flex_col().gap(px(size::CARD_GAP));
        if let Some(mark) = self.bookmarks.at(self.step) {
            let note = if mark.note.is_empty() {
                div().text_color(rgb(theme::MUTED)).child("No note.")
            } else {
                div().text_color(rgb(theme::SOFT)).child(mark.note.clone())
            };
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(size::CARD_GAP / 2.0))
                    .p(px(size::CARD_PAD))
                    .rounded(px(size::RADIUS_CARD))
                    .bg(rgb(theme::GREEN_PILL))
                    .border_1()
                    .border_color(rgb(theme::GREEN_BORDER))
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::GREEN_SOFT))
                            .child(format!("BOOKMARK \u{b7} STEP {}", thousands(mark.step))),
                    )
                    .child(note)
                    .child(link("bookmark-edit", "Edit the note (b)").on_click(
                        cx.listener(|this, _, window, cx| this.open_bookmark_editor(window, cx)),
                    )),
            );
        }

        let mut links = div().flex().gap(px(size::SECTION_GAP));
        if self.bookmarks.at(self.step).is_none() {
            links = links.child(
                link("bookmark-add", "Bookmark this step (b)")
                    .tooltip(tooltip(ADD_NOTE))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_bookmark_editor(window, cx)),
                    ),
            );
        }
        if !self.bookmarks.is_empty() {
            links = links.child(
                link(
                    "bookmark-all",
                    format!("All bookmarks \u{b7} {}", self.bookmarks.len()),
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_tab(RightTab::Bookmarks, cx))),
            );
        }
        column.child(links)
    }

    /// The Bookmarks tab: every bookmark of the run in step order, each a
    /// link to its step with its whole note, and links to edit or remove
    /// it.
    pub(super) fn render_bookmarks_panel(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div()
            .id("bookmarks")
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_y_scroll()
            .py(px(size::LIST_PAD_Y));
        if self.bookmarks.is_empty() {
            list = list.child(
                div()
                    .px(px(size::PANEL_PAD_X))
                    .text_color(rgb(theme::MUTED))
                    .child("No bookmarks. Press b to bookmark the playhead's step."),
            );
        }
        for (i, mark) in self.bookmarks.iter().enumerate() {
            let step = mark.step;
            let here = step == self.step;
            let note = if mark.note.is_empty() {
                div().text_color(rgb(theme::MUTED)).child("No note.")
            } else {
                div().text_color(rgb(theme::SOFT)).child(mark.note.clone())
            };
            let actions = div()
                .flex()
                .gap(px(size::SECTION_GAP))
                .child(
                    link(SharedString::from(format!("bookmark-edit-{i}")), "Edit").on_click(
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.edit_bookmark_at(step, window, cx)
                        }),
                    ),
                )
                .child(
                    link(SharedString::from(format!("bookmark-remove-{i}")), "Remove").on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_bookmark_at(step, cx)
                        }),
                    ),
                );
            list = list.child(
                div()
                    .id(SharedString::from(format!("bookmark-{i}")))
                    .flex()
                    .gap(px(size::LIST_COLUMN_GAP))
                    .px(px(size::PANEL_PAD_X))
                    .py(px(size::CARD_GAP / 2.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                    .when(here, |d| d.bg(rgb(theme::ROW_NOW)))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx)))
                    .child(
                        div()
                            .flex_none()
                            .w(px(size::MONO_CHAR_WIDTH * STEP_CHARS))
                            .flex()
                            .justify_end()
                            .font_family(self.fonts.mono.clone())
                            .text_color(rgb(if here { theme::AMBER } else { theme::FAINT }))
                            .child(thousands(step)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(size::CARD_GAP / 2.0))
                            .child(note)
                            .child(actions),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .bg(rgb(theme::PANEL))
            .child(panel_title(
                &format!("Bookmarks \u{b7} {}", self.bookmarks.len()),
                None,
            ))
            .child(list)
    }

    /// The marks of the bookmarks the track shows, by step: each goes to
    /// its step on a click and shows its note when pointed at.
    pub(super) fn bookmark_marks(&self, view: View, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        self.bookmarks
            .iter()
            .enumerate()
            .filter(|(_, mark)| view.contains(mark.step))
            .map(|(i, mark)| {
                let step = mark.step;
                let note = if mark.note.is_empty() {
                    format!("Bookmark at step {}", thousands(step))
                } else {
                    format!("Bookmark at step {}: {}", thousands(step), mark.note)
                };
                div()
                    .id(SharedString::from(format!("bookmark-mark-{i}")))
                    .absolute()
                    .left(relative(view.fraction_of(step)))
                    .ml(px(-MARK_SIZE / 2.0))
                    .top(px(-size::MARKER_OVERHANG - MARK_SIZE))
                    .size(px(MARK_SIZE))
                    .rounded(px(1.0))
                    .bg(rgb(theme::GREEN_SOFT))
                    .cursor_pointer()
                    .tooltip(tooltip(note))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx)))
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

/// A text link in the accent color.
fn link(id: impl Into<gpui::ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .text_color(rgb(theme::AMBER))
        .hover(|s| s.text_color(rgb(theme::AMBER_HI)))
        .child(text.into())
}
