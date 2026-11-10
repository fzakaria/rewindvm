//! The search box: Ctrl+f, or /, opens a field over the panels that finds
//! text in the build log, the kernel's console, file paths and the other
//! events, with every match listed by its step.
//!
//! Up and Down move through the matches and take the playhead to each.
//! A click takes the playhead to a match and closes the box; a file
//! match opens in the viewer at the playhead instead. Enter and Escape close the box where the
//! playhead is, and Back returns to where the search started.
//! `crate::search` builds the index and runs the queries, both in the
//! background: the index holds every line and event of the run, and a
//! query reads all of it.

use gpui::{
    Context, Div, Entity, Focusable, HighlightStyle, ScrollStrategy, SharedString, StyledText,
    Subscription, UniformListScrollHandle, Window, div, prelude::*, px, relative, rgb, rgba,
    uniform_list,
};
use rewind_text_input::{TextInput, TextInputStyle};

use crate::describe::{clip, thousands};
use crate::request::Request;
use crate::search::{Hits, Index, hits_for_enter};
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::ui::{CloseDialog, ConfirmSearch, SEARCH_CONTEXT, SearchNext, SearchPrevious};

/// The most matches the box lists; the rest are counted.
const MAX_HITS: usize = 500;

/// The most characters of a match's text a row shows.
const MAX_TEXT_CHARS: usize = 160;

/// The box's width, and how many rows it shows before it scrolls.
const BOX_WIDTH: f32 = 640.0;
const ROWS_SHOWN: f32 = 12.0;

/// Where the box sits: under the timeline, across the window's middle.
const BOX_TOP: f32 = size::HEADER_HEIGHT + 132.0;

/// What the box says under the field while the run's index is built.
const INDEXING: &str = "Reading the run's log, files and events\u{2026}";

/// The search box while it is open.
pub struct SearchBox {
    pub input: Entity<TextInput>,
    /// The query the matches are for.
    query: String,
    hits: Hits,
    /// The query being searched for in the background, and its request:
    /// matches found for any other request are dropped.
    asked: Option<(Request, String)>,
    /// The match the playhead was taken to, if any.
    selected: Option<usize>,
    /// The step the playhead was at when the box opened, for Back.
    origin: u64,
    scroll: UniformListScrollHandle,
    /// Runs the query again as the field changes.
    _typed: Subscription,
}

impl Scrubber {
    /// Opens the box, with the field focused; the run's index is built the
    /// first time.
    pub(super) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if let Some(search) = &self.shown.search {
            let handle = search.input.focus_handle(cx);
            window.focus(&handle, cx);
            return;
        }
        let timeline = session.run.timeline.clone();
        self.build_search_index(timeline, cx);

        let style = TextInputStyle {
            placeholder: "Search the log, files and events".into(),
            placeholder_color: rgb(theme::MUTED).into(),
            caret_color: rgb(theme::AMBER).into(),
            selection_color: rgba(theme::FOCUS_RING_A).into(),
        };
        let input = cx.new(|cx| TextInput::new(style, cx));
        window.focus(&input.focus_handle(cx), cx);
        let typed = cx.observe(&input, |this, _, cx| this.search_typed(cx));
        self.shown.search = Some(SearchBox {
            input,
            query: String::new(),
            hits: Hits::default(),
            asked: None,
            selected: None,
            origin: self.step,
            scroll: UniformListScrollHandle::new(),
            _typed: typed,
        });
        cx.notify();
    }

    /// Builds the index of the run on screen, whose timeline is
    /// `timeline`, in the background, unless the index is built or being
    /// built. The index of a run shown before, arriving late, is dropped.
    fn build_search_index(
        &mut self,
        timeline: std::sync::Arc<crate::model::Timeline>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.search_index.begin(&mut self.requests) else {
            return;
        };
        let built = cx
            .background_executor()
            .spawn(async move { Index::build(&timeline) });
        cx.spawn(async move |this, cx| {
            let index = built.await;
            let _ = this.update(cx, |this, cx| {
                if !this.search_index.built(request, index) {
                    return;
                }

                // What was typed while the index was built is searched
                // for now.
                this.search_typed(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Closes the box where the playhead is; Back returns to where the
    /// search started.
    pub(super) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.shown.search.take() else {
            return;
        };
        self.shown.history.jumped(search.origin, self.step);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Enter: with no match chosen yet, takes the playhead to the first
    /// one at or after it; then closes the box. The matches are those of
    /// the text in the field: when its search has not answered yet, the
    /// index is searched here, once, so Enter never acts on an earlier
    /// query's matches. While the run's index is still being built, Enter
    /// waits: the box stays open.
    fn confirm_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = &mut self.shown.search else {
            return;
        };
        let typed = search.input.read(cx).text().to_string();
        let index = self.search_index.ready().map(|index| index.as_ref());
        let on_screen = (search.query.as_str(), &search.hits);
        let Some(hits) = hits_for_enter(&typed, on_screen, index, MAX_HITS) else {
            return;
        };
        if typed != search.query {
            search.hits = hits;
            search.query = typed;
            search.asked = None;
            search.selected = None;
        }
        let first = self.shown.search.as_ref().and_then(|search| {
            if search.selected.is_some() {
                return None;
            }
            let hits = &search.hits.hits;
            hits.iter()
                .position(|h| h.step >= search.origin)
                .or((!hits.is_empty()).then_some(0))
        });
        if let Some(index) = first {
            self.select_hit(index, cx);
        }
        self.close_search(window, cx);
    }

    /// Runs the query again, in the background, when the field's text
    /// changed.
    fn search_typed(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &mut self.shown.search else {
            return;
        };
        let query = search.input.read(cx).text().to_string();
        let latest = search
            .asked
            .as_ref()
            .map_or(&search.query, |(_, asked)| asked);
        if query == *latest {
            return;
        }

        // Until the index is built there is nothing to search; the text
        // in the field is searched for when the index arrives.
        let Some(index) = self.search_index.ready().cloned() else {
            return;
        };
        let request = self.requests.issue();
        search.asked = Some((request, query.clone()));
        let found = cx
            .background_executor()
            .spawn(async move { index.find(&query, MAX_HITS) });
        cx.spawn(async move |this, cx| {
            let hits = found.await;
            let _ = this.update(cx, |this, cx| {
                // Matches for a query typed over since are dropped.
                let Some(search) = &mut this.shown.search else {
                    return;
                };
                let Some((_, query)) = search.asked.take_if(|(asked, _)| *asked == request) else {
                    return;
                };
                search.hits = hits;
                search.query = query;
                search.selected = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// Takes the playhead to match `index` and marks it, scrolling the
    /// list to it as Up and Down move through the matches.
    fn select_hit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(search) = &mut self.shown.search else {
            return;
        };
        let Some(hit) = search.hits.hits.get(index) else {
            return;
        };
        let step = hit.step;
        search.selected = Some(index);
        search.scroll.scroll_to_item(index, ScrollStrategy::Center);
        self.go_to(step, cx);
        cx.notify();
    }

    /// A click on match `index` closes the box: a file opens in the viewer
    /// as it was at the playhead, since at the step it was last opened for
    /// writing it is often empty; any other match takes the playhead to
    /// its step.
    fn open_hit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self
            .shown
            .search
            .as_ref()
            .and_then(|s| s.hits.hits.get(index))
        else {
            return;
        };
        let (step, file) = (hit.step, hit.file.clone());
        if file.is_none() {
            self.go_to(step, cx);
        }
        self.close_search(window, cx);
        if let Some((path, pid)) = file {
            self.open_file(path, pid, cx);
        }
    }

    /// Down or Up: the next or previous match, starting from the playhead
    /// when none is chosen yet.
    fn move_hit(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = &self.shown.search else {
            return;
        };
        let hits = &search.hits.hits;
        if hits.is_empty() {
            return;
        }
        let next = match (search.selected, forward) {
            (Some(i), true) => (i + 1).min(hits.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
            (None, true) => hits
                .iter()
                .position(|h| h.step > self.step)
                .unwrap_or(hits.len() - 1),
            (None, false) => hits.iter().rposition(|h| h.step < self.step).unwrap_or(0),
        };
        self.select_hit(next, cx);
    }

    /// The box over the panels, when it is open.
    pub(super) fn render_search(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        let search = self.shown.search.as_ref()?;
        let field = div()
            .key_context(SEARCH_CONTEXT)
            .on_action(
                cx.listener(|this, _: &ConfirmSearch, window, cx| this.confirm_search(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseDialog, window, cx| this.close_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchNext, _, cx| this.move_hit(true, cx)))
            .on_action(cx.listener(|this, _: &SearchPrevious, _, cx| this.move_hit(false, cx)))
            .p(px(size::NOTICE_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(theme::AMBER_DEEP))
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(theme::TEXT))
            .child(search.input.clone());

        // How many matched, under the field, or that the run's text is
        // still being read into the index.
        let indexing = self.search_index.ready().is_none();
        let count = match (search.query.trim().is_empty(), search.hits.total) {
            _ if indexing => INDEXING.to_string(),
            (true, _) => "Up and Down go through the matches; Enter or Escape closes.".to_string(),
            (false, 0) => "No matches.".to_string(),
            (false, 1) => "1 match".to_string(),
            (false, n) if n > search.hits.hits.len() => format!(
                "The first {} of {} matches",
                thousands(search.hits.hits.len() as u64),
                thousands(n as u64)
            ),
            (false, n) => format!("{} matches", thousands(n as u64)),
        };

        // The matches, each with its step and where it was found.
        let rows = search.hits.hits.len();
        let query = search.query.trim().to_lowercase();
        let selected = search.selected;
        let mono = self.fonts.mono.clone();
        let list = uniform_list(
            "search-hits",
            rows,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let Some(search) = &this.shown.search else {
                    return Vec::new();
                };
                range
                    .filter_map(|i| {
                        let hit = search.hits.hits.get(i)?;
                        let text = clip(hit.text.trim_end(), MAX_TEXT_CHARS);
                        let highlights = match_range(&text, &query)
                            .map(|r| {
                                vec![(
                                    r,
                                    HighlightStyle {
                                        color: Some(rgb(theme::AMBER).into()),
                                        ..Default::default()
                                    },
                                )]
                            })
                            .unwrap_or_default();
                        Some(
                            div()
                                .id(i)
                                .h(px(size::LIST_ROW_HEIGHT))
                                .flex()
                                .items_center()
                                .gap(px(size::LIST_COLUMN_GAP))
                                .px(px(size::PANEL_PAD_X / 2.0))
                                .rounded(px(size::RADIUS_MENU_ITEM))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                                .when(selected == Some(i), |d| d.bg(rgb(theme::ROW_NOW)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_hit(i, window, cx)
                                }))
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(size::MONO_CHAR_WIDTH * 9.0))
                                        .flex()
                                        .justify_end()
                                        .text_color(rgb(theme::FAINT))
                                        .child(thousands(hit.step)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(size::MONO_CHAR_WIDTH * 5.0))
                                        .text_color(rgb(theme::MUTED))
                                        .child(hit.found.label()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(rgb(theme::SOFT))
                                        .child(
                                            StyledText::new(SharedString::from(text))
                                                .with_highlights(highlights),
                                        ),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&search.scroll)
        .h(px(size::LIST_ROW_HEIGHT * (rows as f32).min(ROWS_SHOWN)))
        .font_family(mono)
        .text_size(px(size::TEXT_MONO));

        Some(
            div()
                .id("search-box")
                .occlude()
                .absolute()
                .top(px(BOX_TOP))
                .left(relative(0.5))
                .ml(px(-BOX_WIDTH / 2.0))
                .w(px(BOX_WIDTH))
                .flex()
                .flex_col()
                .gap(px(size::CARD_GAP))
                .p(px(size::NOTICE_PAD))
                .rounded(px(size::RADIUS_CARD))
                .bg(rgb(theme::RAISED))
                .border_1()
                .border_color(rgb(theme::LINE_2))
                .shadow_lg()
                .child(field)
                .when(rows > 0, |d| d.child(list))
                .child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child(count),
                ),
        )
    }
}

/// Where `query`, in lower case, first appears in `text`, ignoring case,
/// as a byte range of `text`.
fn match_range(text: &str, query: &str) -> Option<std::ops::Range<usize>> {
    if query.is_empty() {
        return None;
    }
    // Lower-casing can change a character's length, so the match is
    // found character by character in the original text.
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    starts.iter().find_map(|&start| {
        let mut end = start;
        let mut wanted = query.chars();
        let mut chars = text[start..].chars();
        loop {
            let Some(want) = wanted.next() else {
                return Some(start..end);
            };
            let c = chars.next()?;
            if !c.to_lowercase().eq(std::iter::once(want)) {
                return None;
            }
            end += c.len_utf8();
        }
    })
}

#[cfg(test)]
mod tests {
    // Where a match is highlighted in a row's text.
    use super::*;

    #[test]
    fn the_first_match_is_found_ignoring_case() {
        // The query in lower case finds its first place in mixed-case
        // text, as a byte range; no match, or no query, finds nothing.
        assert_eq!(match_range("FAIL: test_Pool", "pool"), Some(11..15));
        assert_eq!(match_range("job job", "job"), Some(0..3));
        assert_eq!(match_range("nothing", "pool"), None);
        assert_eq!(match_range("anything", ""), None);
    }
}
