//! The tabs over the right column: "At this step", the Runs panel, the
//! file viewer and the source panel. Each shows in the column's whole
//! height when chosen, so none takes another's place or squeezes a fourth
//! column into the window. The file, the source and the runs close back to
//! "At this step", by their x or Escape, and the runs pill opens the runs
//! again.
//!
//! Only the chosen tab follows the playhead: a file or source tab behind
//! another asks the engine again once it is chosen, rather than forking
//! the run at every step the playhead rests on while nobody looks.

use gpui::{Context, Div, MouseButton, Role, SharedString, div, prelude::*, px, rgb};

use crate::describe::clip;
use crate::theme::{self, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::icon;

/// How many characters of a file's name its tab shows.
const MAX_FILE_CHARS: usize = 24;

/// The tab bar's height.
const TAB_HEIGHT: f32 = 32.0;

/// The room around a tab's close icon that takes a click.
const CLOSE_PAD: f32 = 4.0;

/// What the right column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightTab {
    AtStep,
    Runs,
    File,
    Source,
}

impl Scrubber {
    /// Shows `tab`, and brings a file or source tab up to the playhead.
    pub(super) fn select_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.right_tab = tab;
        if tab == RightTab::Runs {
            self.runs_tab = true;
        }
        match tab {
            RightTab::File => self.playhead_moved(cx),
            RightTab::Source => self.source_playhead_moved(cx),
            RightTab::AtStep | RightTab::Runs => {}
        }
        cx.notify();
    }

    /// Closes `tab`: the file viewer, the source panel or the Runs panel,
    /// which the runs pill opens again. "At this step" stays.
    pub(super) fn close_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        match tab {
            RightTab::File => self.close_viewer(cx),
            RightTab::Source => self.close_source(cx),
            RightTab::Runs => {
                self.runs_tab = false;
                if self.right_tab == RightTab::Runs {
                    self.right_tab = RightTab::AtStep;
                }
                cx.notify();
            }
            RightTab::AtStep => {}
        }
    }

    /// The tabs there are: "At this step" always, and the runs, the file
    /// and the source while they are open.
    fn tabs(&self) -> Vec<RightTab> {
        let mut tabs = vec![RightTab::AtStep];
        if self.runs_tab && self.family.is_some() {
            tabs.push(RightTab::Runs);
        }
        if self.viewer.is_some() {
            tabs.push(RightTab::File);
        }
        if self.source.is_some() {
            tabs.push(RightTab::Source);
        }
        tabs
    }

    fn tab_label(&self, tab: RightTab) -> String {
        match tab {
            RightTab::AtStep => "At this step".to_string(),
            RightTab::Runs => match &self.family {
                Some(family) => format!("Runs \u{b7} {}", family.runs.len()),
                None => "Runs".to_string(),
            },
            RightTab::File => {
                let name = self
                    .viewer
                    .as_ref()
                    .and_then(|v| v.path.rsplit('/').next().map(str::to_string))
                    .unwrap_or_default();
                format!("File \u{b7} {}", clip(&name, MAX_FILE_CHARS))
            }
            RightTab::Source => "Source".to_string(),
        }
    }

    /// The tab bar.
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> Div {
        let mut bar = div()
            .flex()
            .flex_none()
            .items_end()
            .h(px(TAB_HEIGHT))
            .px(px(size::PANEL_PAD_X / 2.0))
            .gap(px(size::CARD_GAP / 2.0))
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::LINE_SOFT))
            .text_size(px(size::TEXT_SMALL));
        for tab in self.tabs() {
            let chosen = tab == self.right_tab;
            let closable = tab != RightTab::AtStep;
            let name = format!("{tab:?}");
            let mut item = div()
                .id(SharedString::from(format!("tab-{name}")))
                .role(Role::Tab)
                .flex()
                .items_center()
                .gap(px(size::CARD_GAP / 2.0))
                .h(px(TAB_HEIGHT - 4.0))
                .px(px(size::CARD_GAP))
                .cursor_pointer()
                .border_b_2()
                .border_color(rgb(if chosen { theme::AMBER } else { theme::PANEL }))
                .text_color(rgb(if chosen { theme::TEXT } else { theme::MUTED }))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
                .child(self.tab_label(tab));
            if closable {
                item = item.child(
                    div()
                        .id(SharedString::from(format!("tab-close-{name}")))
                        .role(Role::Button)
                        .aria_label("Close")
                        .p(px(CLOSE_PAD))
                        .rounded(px(size::RADIUS_MENU_ITEM))
                        .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_tab(tab, cx)
                        }))
                        .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED)),
                );
            }
            bar = bar.child(item);
        }
        bar
    }
}
