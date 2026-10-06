//! The Compare tab: this run's events beside the compared run's, from
//! shortly before they part, with the text that differs marked, and the
//! compared run a click or `x` away at the matching step.

use std::ops::Range;

use gpui::{
    Context, Div, FontWeight, HighlightStyle, SharedString, Stateful, div, prelude::*, px,
    relative, rgb,
};

use crate::compare::{Cell, Row};
use crate::describe::thousands;
use crate::selection::{Mapped, Pos, Surface, part_of_line};
use crate::theme::{self, layout, size};
use crate::ui::bookmarks::link;
use crate::ui::scrubber::Scrubber;
use crate::ui::selectable::{Registry, selectable, selects};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::panel_title;

/// What a cell shows where its run's events had run out.
const NO_EVENT: &str = "no more events";

/// Which run a cell is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The run on screen: a click moves the playhead.
    Here,
    /// The compared run: a click shows it, at the cell's step.
    There,
}

impl Scrubber {
    /// Shows the compared run in this one's place, compared with this one,
    /// with the playhead at `step` of it, or at the step that matches the
    /// playhead's. The Compare tab stays open when it was.
    pub(super) fn show_other_run(&mut self, step: Option<u64>, cx: &mut Context<Self>) {
        let Some(session) = self.session.take() else {
            return;
        };
        if session.other.is_none() {
            self.session = Some(session);
            return;
        }
        let at = step.or_else(|| session.matching_step(self.step));
        let tab = self.right_tab;
        self.show(session.swapped(), at, cx);
        if tab == RightTab::Compare {
            self.select_tab(RightTab::Compare, cx);
        }
        cx.notify();
    }

    /// The Compare tab: the events both runs had just before they part,
    /// once each, then each run's next events side by side.
    pub(super) fn render_compare(&self, cx: &mut Context<Self>) -> Div {
        let session = self.session();
        let registry = self.selecting.registry.clone();
        let selected = self.selected_range(Surface::Compare);
        let verdict = |run: &crate::run::Run| run.verdict_label();
        let this_title = format!("This run \u{b7} {}", verdict(&session.run));
        let other_title = session
            .other
            .as_ref()
            .map(|o| format!("{} \u{b7} {}", o.label(), verdict(o)))
            .unwrap_or_default();

        let mut list = div().flex().flex_col().py(px(size::LIST_PAD_Y));
        if session.rows.is_empty() {
            list = list.child(
                div()
                    .px(px(size::PANEL_PAD_X))
                    .text_color(rgb(theme::MUTED))
                    .child("Neither run has events of the compared program."),
            );
        }

        // Each cell's text is one line of the tab's selectable text, in
        // the order `compare_lines` lists them.
        let mut line = 0;
        let mut said_shared = false;
        let mut said_apart = false;
        for (i, row) in session.rows.iter().enumerate() {
            match row {
                Row::Shared(here, there) => {
                    if !said_shared {
                        said_shared = true;
                        list = list.child(heading("Both runs, the same"));
                    }
                    let step = here.step;
                    let steps = format!(
                        "{} here \u{b7} {} there \u{b7} {}/{}",
                        thousands(here.step),
                        thousands(there.step),
                        here.pid,
                        here.tid
                    );
                    let text = self.cell_text(here, line, &selected, &registry);
                    line += 1;
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("compare-shared-{i}")))
                            .px(px(size::PANEL_PAD_X))
                            .py(px(size::CARD_GAP / 2.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
                            .when(here.step == self.step, |d| d.bg(rgb(theme::ROW_NOW)))
                            .on_click(cx.listener(move |this, e, _, cx| {
                                if this.plain_click(e) {
                                    this.jump_to(step, cx);
                                }
                            }))
                            .child(self.cell_steps(steps))
                            .child(text.text_color(rgb(theme::MUTED))),
                    );
                }
                Row::Apart(here, there) => {
                    if !said_apart {
                        said_apart = true;
                        list = list.child(heading("Then")).child(
                            div()
                                .flex()
                                .px(px(size::PANEL_PAD_X / 2.0))
                                .text_size(px(size::TEXT_SMALL))
                                .text_color(rgb(theme::MUTED))
                                .child(column_title(this_title.clone()))
                                .child(column_title(other_title.clone())),
                        );
                    }
                    let left = self.cell(
                        here.as_ref(),
                        Side::Here,
                        (i, line),
                        &selected,
                        &registry,
                        cx,
                    );
                    let right = self.cell(
                        there.as_ref(),
                        Side::There,
                        (i, line + 1),
                        &selected,
                        &registry,
                        cx,
                    );
                    line += 2;
                    list = list.child(
                        div()
                            .flex()
                            .px(px(size::PANEL_PAD_X / 2.0))
                            .child(left)
                            .child(right),
                    );
                }
            }
        }

        let other_run = link("compare-other-run", "Show in the other run (x)")
            .on_click(cx.listener(|this, _, _, cx| this.show_other_run(None, cx)));
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .bg(rgb(theme::PANEL))
            .child(panel_title("Compare", Some(other_run.into_any_element())))
            .child(selects(
                div()
                    .id("compare")
                    .flex()
                    .flex_col()
                    .flex_grow(layout::FILL)
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
                Surface::Compare,
                cx,
            ))
    }

    /// One run's cell of a row after the runs part: its step and thread,
    /// then its event with the part that differs marked. `row` is the
    /// row's index and `line` the cell's line of selectable text.
    fn cell(
        &self,
        cell: Option<&Cell>,
        side: Side,
        (row, line): (usize, usize),
        selected: &Option<Range<Pos>>,
        registry: &Registry,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = SharedString::from(format!("compare-{row}-{side:?}"));
        let column = div()
            .id(id)
            .flex()
            .flex_col()
            .flex_basis(relative(0.0))
            .flex_grow(1.0)
            .min_w_0()
            .px(px(size::PANEL_PAD_X / 2.0))
            .py(px(size::CARD_GAP / 2.0));
        let Some(cell) = cell else {
            return column
                .child(self.cell_steps(String::new()))
                .child(div().text_color(rgb(theme::FAINT)).child(NO_EVENT));
        };
        let step = cell.step;
        let now = side == Side::Here && step == self.step;
        let steps = format!("{} \u{b7} {}/{}", thousands(step), cell.pid, cell.tid);
        column
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
            .when(now, |d| d.bg(rgb(theme::ROW_NOW)))
            .on_click(cx.listener(move |this, e, _, cx| {
                if !this.plain_click(e) {
                    return;
                }
                match side {
                    Side::Here => this.jump_to(step, cx),
                    Side::There => this.show_other_run(Some(step), cx),
                }
            }))
            .child(self.cell_steps(steps))
            .child(
                self.cell_text(cell, line, selected, registry)
                    .text_color(rgb(theme::SOFT)),
            )
    }

    /// A cell's step and thread, small and faint.
    fn cell_steps(&self, text: String) -> Div {
        div()
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::FAINT))
            .child(text)
    }

    /// A cell's event as line `line` of the tab's selectable text, the
    /// part that differs from the other run's in amber.
    fn cell_text(
        &self,
        cell: &Cell,
        line: usize,
        selected: &Option<Range<Pos>>,
        registry: &Registry,
    ) -> Div {
        let part = selected
            .as_ref()
            .and_then(|r| part_of_line(r, line, cell.text.len()));
        let marked = cell.differs.clone().map(|range| {
            (
                range,
                HighlightStyle {
                    color: Some(rgb(theme::AMBER).into()),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            )
        });
        div().font_family(self.fonts.mono.clone()).child(
            selectable(Surface::Compare, line, cell.text.clone(), part, registry)
                .with_highlights(marked),
        )
    }

    /// The Compare tab's lines as the selection sees them: each cell's
    /// event, in the order the tab draws them.
    pub(super) fn compare_lines(&self) -> Vec<Mapped> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let text = |cell: &Option<Cell>| {
            Mapped::plain(
                cell.as_ref()
                    .map_or(NO_EVENT.to_string(), |c| c.text.clone()),
            )
        };
        session
            .rows
            .iter()
            .flat_map(|row| match row {
                Row::Shared(here, _) => vec![Mapped::plain(here.text.clone())],
                Row::Apart(here, there) => vec![text(here), text(there)],
            })
            .collect()
    }
}

/// A heading over a part of the tab's rows.
fn heading(text: &'static str) -> Div {
    div()
        .px(px(size::PANEL_PAD_X))
        .pt(px(size::CARD_GAP))
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::MUTED))
        .child(text.to_uppercase())
}

/// The title over one run's column.
fn column_title(text: String) -> Div {
    div()
        .flex_basis(relative(0.0))
        .flex_grow(1.0)
        .min_w_0()
        .px(px(size::PANEL_PAD_X / 2.0))
        .child(text)
}
