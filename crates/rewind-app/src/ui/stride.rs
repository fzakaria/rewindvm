//! The "Stop at" chooser beside the Previous and Next buttons: what the
//! two buttons and the Left and Right keys stop at.
//!
//! `crate::stride` finds the stops; this draws the chooser and its menu,
//! and moves the playhead.

use gpui::{
    Context, Div, MouseButton, MouseDownEvent, Pixels, Point, SharedString, anchored, div,
    prelude::*, px, rgb,
};

use crate::describe::{clip, thousands};
use crate::model::Timeline;
use crate::stride::{Direction, Stride, step_by};
use crate::theme::{self, size};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::widgets::{Availability, ButtonStyle, button, tooltip};

/// How many characters of a file's path the chooser shows.
const MAX_PATH_CHARS: usize = 40;

/// How many characters of the chosen stride the chooser shows; the menu
/// shows them whole.
const MAX_CHOSEN_CHARS: usize = 22;

/// The menu's width, wide enough for a thread and its program's name.
const MENU_WIDTH: f32 = 300.0;

/// What the chooser says it does when pointed at.
const CHOOSER_NOTE: &str = "What Previous, Next and the Left and Right keys stop at: every event, the build log's lines, processes starting and exiting, or the thread, process, kind of event or file of the event at the playhead.";

/// What a stride is called in the chooser and its menu.
pub fn stride_label(stride: &Stride, timeline: &Timeline) -> String {
    let name = |pid: u32| {
        timeline
            .name_of(pid)
            .map_or_else(String::new, |n| format!(" ({n})"))
    };
    match stride {
        Stride::Every => "every event".to_string(),
        Stride::LogLine => "log lines".to_string(),
        Stride::Lifecycle => "process starts and exits".to_string(),
        Stride::Thread { pid, tid } => format!("thread {tid} of {pid}{}", name(*pid)),
        Stride::Process { pid } => format!("process {pid}{}", name(*pid)),
        Stride::Kind(kind) => format!("{} events", kind.call()),
        Stride::File(path) => clip(path, MAX_PATH_CHARS),
    }
}

impl Scrubber {
    /// Moves the playhead to the next or previous stop of the chosen
    /// stride. With every event chosen and none left that way, it goes to
    /// the end of the run, as the ends are stops too; with anything else,
    /// it stays and says there is none.
    pub(super) fn step_by_stride(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let timeline = &session.run.timeline;
        let found = step_by(
            timeline,
            &self.stride,
            self.log_filter,
            direction,
            self.step,
        );
        let target = match (found, &self.stride, direction) {
            (Some(step), _, _) => step,
            (None, Stride::Every, Direction::Forward) => timeline.total,
            (None, Stride::Every, Direction::Back) => 0,
            (None, stride, _) => {
                let which = match direction {
                    Direction::Forward => "later",
                    Direction::Back => "earlier",
                };
                let label = stride_label(stride, timeline);
                self.notify_user(
                    NoticeTone::Info,
                    format!("No {which} stop"),
                    format!(
                        "Stopping at {label}, there is none {} step {}.",
                        match direction {
                            Direction::Forward => "after",
                            Direction::Back => "before",
                        },
                        thousands(self.step)
                    ),
                    cx,
                );
                return;
            }
        };
        self.go_to(target, cx);
    }

    /// Opens the chooser's menu under the pointer.
    fn open_stride_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.stride_menu = Some(position);
        cx.notify();
    }

    pub(super) fn close_stride_menu(&mut self, cx: &mut Context<Self>) {
        if self.stride_menu.take().is_some() {
            cx.notify();
        }
    }

    fn choose_stride(&mut self, stride: Stride, cx: &mut Context<Self>) {
        self.stride = stride;
        self.stride_menu = None;
        cx.notify();
    }

    /// The strides the menu offers: the ones that stand alone, then the
    /// ones that follow the event at the playhead, then the file in the
    /// viewer.
    fn offered_strides(&self) -> Vec<Stride> {
        let mut offered = vec![Stride::Every, Stride::LogLine, Stride::Lifecycle];
        let Some(session) = &self.session else {
            return offered;
        };
        let t = &session.run.timeline;
        if let Some(event) = t.event_index_at(self.step).and_then(|i| t.event(i)) {
            offered.extend(Stride::following(event));
        }
        if let Some(viewer) = &self.viewer {
            offered.push(Stride::File(viewer.path.clone()));
        }
        let mut unique: Vec<Stride> = Vec::new();
        for stride in offered {
            if !unique.contains(&stride) {
                unique.push(stride);
            }
        }
        if !unique.contains(&self.stride) {
            unique.push(self.stride.clone());
        }
        unique
    }

    /// The chooser: "Stop at" and the stride chosen, which opens the menu.
    pub(super) fn render_stride_chooser(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .session
            .as_ref()
            .map(|s| {
                clip(
                    &stride_label(&self.stride, &s.run.timeline),
                    MAX_CHOSEN_CHARS,
                )
            })
            .unwrap_or_default();
        // The note would cover the menu while the menu is open.
        button("stride", ButtonStyle::Neutral, Availability::Enabled)
            .when(self.stride_menu.is_none(), |b| {
                b.tooltip(tooltip(CHOOSER_NOTE))
            })
            .child(div().text_color(rgb(theme::MUTED)).child("Stop at"))
            .child(label)
            .child(div().text_color(rgb(theme::MUTED)).child("\u{25be}"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_stride_menu(e.position, cx);
                }),
            )
    }

    /// The chooser's menu over a backdrop that closes it, when it is open.
    pub(super) fn render_stride_menu(&self, cx: &mut Context<Self>) -> Option<Div> {
        let position = self.stride_menu?;
        let session = self.session.as_ref()?;
        let timeline = &session.run.timeline;
        let mut items = div()
            .w(px(MENU_WIDTH))
            .flex()
            .flex_col()
            .p(px(size::MENU_PAD))
            .rounded(px(size::RADIUS_BUTTON))
            .bg(rgb(theme::RAISED))
            .border_1()
            .border_color(rgb(theme::LINE_2))
            .shadow_lg()
            .text_size(px(size::TEXT_UI))
            .text_color(rgb(theme::TEXT))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for (i, stride) in self.offered_strides().into_iter().enumerate() {
            let chosen = stride == self.stride;
            let label = stride_label(&stride, timeline);
            items = items.child(
                div()
                    .id(SharedString::from(format!("stride-{i}")))
                    .px(px(size::MENU_ITEM_PAD_X))
                    .py(px(size::MENU_ITEM_PAD_Y))
                    .rounded(px(size::RADIUS_MENU_ITEM))
                    .cursor_pointer()
                    .truncate()
                    .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                    .when(chosen, |d| d.text_color(rgb(theme::AMBER)))
                    .child(label)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.choose_stride(stride.clone(), cx)),
                    ),
            );
        }
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    // The press that closes the menu goes no further, so
                    // it does not also click the row under it.
                    div()
                        .id("stride-backdrop")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.close_stride_menu(cx)),
                        ),
                )
                .child(
                    anchored()
                        .position(position)
                        .snap_to_window_with_margin(px(size::MENU_EDGE_MARGIN))
                        .child(items),
                ),
        )
    }
}
