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
use crate::stride::{Direction, Stride, count, step_by};
use crate::theme::{self, size};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::toolbar::{Clipped, ToolbarFit, clip_label};
use crate::ui::widgets::{Availability, ButtonStyle, button, tooltip};
use rewind_trace::Life;

/// How many characters of a file's path the chooser shows.
const MAX_PATH_CHARS: usize = 40;

/// The menu's width, wide enough for a thread and its program's name.
const MENU_WIDTH: f32 = 300.0;

/// What the chooser says it does when pointed at.
const CHOOSER_NOTE: &str = "What Previous, Next and the Left and Right keys stop at: every event, context switches, the build log's lines, processes starting and exiting, the bookmarks, the thread, process, kind of event or file of the event at the playhead, or the switches that give the thread or process on the CPU at the playhead the CPU and take it away. The menu shows how many stops each has over the run.";

/// What a stride is called in the chooser and its menu.
pub fn stride_label(stride: &Stride, timeline: &Timeline) -> String {
    let name = |life: Life| {
        timeline
            .name_of(life)
            .map_or_else(String::new, |n| format!(" ({n})"))
    };
    match stride {
        Stride::Every => "every event".to_string(),
        Stride::Switches => "context switches".to_string(),
        Stride::LogLine => "log lines".to_string(),
        Stride::Lifecycle => "process starts and exits".to_string(),
        Stride::Bookmarks => "bookmarks".to_string(),
        Stride::Thread { life, tid } => format!("thread {tid} of {}{}", life.pid, name(*life)),
        Stride::Process { life } => format!("process {}{}", life.pid, name(*life)),
        Stride::ThreadSwitches { life, tid } => {
            format!("switches of thread {tid} of {}{}", life.pid, name(*life))
        }
        Stride::ProcessSwitches { life } => {
            format!("switches of process {}{}", life.pid, name(*life))
        }
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
        let bookmarks: Vec<u64> = self.shown.bookmarks.iter().map(|b| b.step).collect();
        let found = step_by(
            timeline,
            &self.shown.stride,
            self.log_filter,
            &bookmarks,
            direction,
            self.step,
        );
        let target = match (found, &self.shown.stride, direction) {
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
        self.shown.stride = stride;
        self.stride_menu = None;
        cx.notify();
    }

    /// The strides the menu offers: the ones that stand alone, then the
    /// ones that follow the event at the playhead, then the ones that
    /// follow the task the switch at the playhead gave the CPU, then the
    /// file in the viewer.
    fn offered_strides(&self) -> Vec<Stride> {
        let timeline = self.session.as_ref().map(|s| &s.run.timeline);
        let mut offered = vec![Stride::Every];
        if timeline.is_some_and(|t| !t.trace.switches.is_empty()) {
            offered.push(Stride::Switches);
        }
        offered.extend([Stride::LogLine, Stride::Lifecycle]);
        if !self.shown.bookmarks.is_empty() {
            offered.push(Stride::Bookmarks);
        }
        let Some(t) = timeline else {
            return offered;
        };
        if let Some(event) = t.event_index_at(self.step).and_then(|i| t.event(i)) {
            offered.extend(Stride::following(t, event));
        }
        if let Some(switch) = t.switch_at(self.step) {
            offered.extend(Stride::following_switch(t, switch));
        }
        if let Some(viewer) = &self.shown.viewer {
            offered.push(Stride::File(viewer.path.clone()));
        }
        let mut unique: Vec<Stride> = Vec::new();
        for stride in offered {
            if !unique.contains(&stride) {
                unique.push(stride);
            }
        }
        if !unique.contains(&self.shown.stride) {
            unique.push(self.shown.stride.clone());
        }
        unique
    }

    /// The chosen stride as the chooser shows it.
    pub(super) fn chosen_stride(&self) -> Clipped {
        clip_label(
            &self
                .session
                .as_ref()
                .map(|s| stride_label(&self.shown.stride, &s.run.timeline))
                .unwrap_or_default(),
        )
    }

    /// The chooser: "Stop at" and the stride chosen, which opens the menu.
    /// The compact toolbar shows the stride alone.
    pub(super) fn render_stride_chooser(
        &self,
        fit: ToolbarFit,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The stride is cut to the toolbar's cap; the note and the menu
        // show it whole.
        let label = self.chosen_stride();
        let note = match &label.whole {
            Some(whole) => format!("Stopping at {whole}. {CHOOSER_NOTE}"),
            None => CHOOSER_NOTE.to_string(),
        };
        let label = label.shown;
        // The note would cover the menu while the menu is open.
        button("stride", ButtonStyle::Neutral, Availability::Enabled)
            .when(self.stride_menu.is_none(), |b| b.tooltip(tooltip(note)))
            .when(fit == ToolbarFit::Full, |b| {
                b.child(div().text_color(rgb(theme::MUTED)).child("Stop at"))
            })
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
        // Each row names its stride and, at the right, how many stops the
        // stride has over the whole run: the size of the walk Next makes,
        // known before the stride is chosen.
        let bookmarks: Vec<u64> = self.shown.bookmarks.iter().map(|b| b.step).collect();
        for (i, stride) in self.offered_strides().into_iter().enumerate() {
            let chosen = stride == self.shown.stride;
            let label = stride_label(&stride, timeline);
            let stops = count(timeline, &stride, self.log_filter, &bookmarks);
            items = items.child(
                div()
                    .id(SharedString::from(format!("stride-{i}")))
                    .px(px(size::MENU_ITEM_PAD_X))
                    .py(px(size::MENU_ITEM_PAD_Y))
                    .rounded(px(size::RADIUS_MENU_ITEM))
                    .cursor_pointer()
                    .flex()
                    .justify_between()
                    .gap(px(size::MENU_ITEM_PAD_X))
                    .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                    .when(chosen, |d| d.text_color(rgb(theme::AMBER)))
                    .child(div().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(thousands(stops as u64)),
                    )
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
