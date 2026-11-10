//! The process or thread picked in the process tree, and its card in the
//! "At this step" tab, in place of the last event's.
//!
//! `crate::proc_card` says what the card holds; this draws it and keeps
//! the pick.

use gpui::{ClickEvent, Context, Div, MouseButton, Role, SharedString, div, prelude::*, px, rgb};
use rewind_trace::ending::ExitStatus;
use rewind_trace::signal_name;

use crate::describe::thousands;
use crate::model::{RowKind, Stream};
use crate::proc_card::{End, Pick, ProcCard, command_line, proc_card};
use crate::theme::{self, size};
use crate::ui::bookmarks::link;
use crate::ui::render::{card, card_title, seek_on_click};
use crate::ui::scrubber::{Aim, Replay, Scrubber};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{Availability, ButtonStyle, button, tooltip};

/// How many of the latest output lines the card draws; the build log has
/// the rest.
const MAX_OUTPUT_LINES: usize = 200;

const CLOSE_NOTE: &str = "Close this card and show the last event again. Key: Escape.";
const GDB_HERE_NOTE: &str =
    "Runs gdb against a fork of the run at the playhead, in this process or thread.";
const SHELL_HERE_NOTE: &str = "Opens a shell inside a fork of the run at the playhead, in this process's root, working directory and environment.";
const NOT_RUNNING_NOTE: &str =
    "It is not running at the playhead. Move the playhead into its lifetime first.";

/// A click on a row of the tree: a plain one, or one that is part of
/// selecting text, such as the end of a drag or a double click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClick {
    Plain,
    Selecting,
}

/// The tree's state a row click can change: the pick and the playhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeState {
    pub picked: Option<Pick>,
    pub step: u64,
}

/// The state after a click on `row`: a plain click picks it and leaves the
/// playhead where it is; a click that selects text changes nothing.
pub fn after_row_click(state: TreeState, row: Pick, click: RowClick) -> TreeState {
    match click {
        RowClick::Plain => TreeState {
            picked: Some(row),
            ..state
        },
        RowClick::Selecting => state,
    }
}

impl Scrubber {
    /// A click on a row of the process tree.
    pub(super) fn click_row(&mut self, row: Pick, e: &ClickEvent, cx: &mut Context<Self>) {
        let click = if self.plain_click(e) {
            RowClick::Plain
        } else {
            RowClick::Selecting
        };
        let before = TreeState {
            picked: self.shown.picked_row,
            step: self.step,
        };
        let after = after_row_click(before, row, click);
        if after == before {
            return;
        }
        self.pick_row(after.picked, cx);
    }

    /// Picks a row, or none, and shows "At this step", where its card is.
    pub(super) fn pick_row(&mut self, pick: Option<Pick>, cx: &mut Context<Self>) {
        self.shown.picked_row = pick;
        if pick.is_some() && self.right_tab != RightTab::AtStep {
            self.select_tab(RightTab::AtStep, cx);
        }
        cx.notify();
    }

    /// The card for the picked row at the playhead, while one is picked.
    pub(super) fn render_picked_row(&self, cx: &mut Context<Self>) -> Option<Div> {
        let pick = self.shown.picked_row?;
        let t = &self.session.as_ref()?.run.timeline;
        let card = proc_card(t, pick, self.step)?;
        Some(self.render_proc_card(&card, cx))
    }

    fn render_proc_card(&self, c: &ProcCard, cx: &mut Context<Self>) -> Div {
        let mono = self.fonts.mono.clone();
        let fact = |label: &'static str| {
            div()
                .flex()
                .flex_wrap()
                .items_baseline()
                .gap_x(px(size::CARD_GAP))
                .child(div().flex_none().text_color(rgb(theme::MUTED)).child(label))
        };
        let step_link = |id: SharedString, step: u64, cx: &mut Context<Self>| {
            link(id, format!("step {}", thousands(step)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(seek_on_click(step, cx))
        };
        // The title: what it is, and the way back to the last event.
        let title = match c.kind {
            RowKind::Process => format!("Process {} \u{b7} {}", c.pick.pid, c.command),
            RowKind::Thread => format!(
                "Thread {} of {} \u{b7} {}",
                c.pick.tid, c.pick.pid, c.command
            ),
        };
        let close = div()
            .id("proc-card-close")
            .role(Role::Button)
            .aria_label("Close the process card (Escape)")
            .flex_none()
            .px(px(size::CARD_GAP))
            .cursor_pointer()
            .text_color(rgb(theme::MUTED))
            .hover(|s| s.text_color(rgb(theme::TEXT)))
            .tooltip(tooltip(CLOSE_NOTE))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| this.pick_row(None, cx)))
            .child("\u{d7}");
        let mut body = card(theme::RAISED, theme::LINE).child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap(px(size::CARD_GAP))
                .child(card_title(theme::TEXT).min_w_0().child(title))
                .child(close),
        );

        // The command line, whole, wrapped.
        if !c.argv.is_empty() {
            body = body.child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::SOFT))
                    .child(command_line(&c.argv)),
            );
        }

        // Its parent, or a thread's process.
        if let Some(parent) = &c.parent {
            let label = match c.kind {
                RowKind::Process => "parent",
                RowKind::Thread => "process",
            };
            body = body.child(fact(label).child(pick_link(
                "proc-card-parent".into(),
                format!("{} {}", parent.pick.pid, parent.command),
                parent.pick,
                cx,
            )));
        }

        // When it started, exec'd and ended.
        let mut started =
            fact("started at").child(step_link("proc-card-start".into(), c.start, cx));
        for (i, exec) in c.execs.iter().enumerate() {
            started = started
                .child(div().text_color(rgb(theme::MUTED)).child("exec'd at"))
                .child(step_link(format!("proc-card-exec-{i}").into(), *exec, cx));
        }
        body = body.child(started);
        body = body.child(match c.end {
            Some(end) => {
                let label = if end.reached { "exited at" } else { "exits at" };
                fact(label)
                    .child(step_link("proc-card-end".into(), end.step, cx))
                    .children(
                        how_it_ended(end).map(|how| div().text_color(rgb(theme::MUTED)).child(how)),
                    )
            }
            None => fact("still running when the recording ends"),
        });

        // A process's other threads.
        if !c.threads.is_empty() {
            let mut threads = fact("threads");
            for (i, thread) in c.threads.iter().enumerate() {
                let pick = Pick {
                    pid: c.pick.pid,
                    tid: thread.tid,
                    start: thread.start,
                };
                threads = threads.child(pick_link(
                    SharedString::from(format!("proc-card-thread-{i}")),
                    thread.tid.to_string(),
                    pick,
                    cx,
                ));
            }
            body = body.child(threads);
        }

        // gdb and a shell in this process, while it runs at the playhead.
        let running = c.start <= self.step && c.end.is_none_or(|end| self.step < end.step);
        let availability = if running {
            Availability::Enabled
        } else {
            Availability::Disabled
        };
        let aim = Aim::Process {
            pid: c.pick.pid,
            tid: c.pick.tid,
        };
        let action = |id: &'static str, label: &'static str, note: &'static str| {
            button(id, ButtonStyle::Neutral, availability)
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(label))
                .tooltip(tooltip(if running { note } else { NOT_RUNNING_NOTE }))
        };
        body = body.child(
            div()
                .flex()
                .gap(px(size::CARD_GAP))
                .child(
                    action("proc-card-gdb", "Attach gdb", GDB_HERE_NOTE).when(running, |b| {
                        b.on_click(cx.listener(move |this, _, window, cx| {
                            this.open_replay(Replay::Gdb, aim, window, cx)
                        }))
                    }),
                )
                .child(
                    action("proc-card-shell", "Open shell", SHELL_HERE_NOTE).when(running, |b| {
                        b.on_click(cx.listener(move |this, _, window, cx| {
                            this.open_replay(Replay::Shell, aim, window, cx)
                        }))
                    }),
                ),
        );

        // What its process wrote, up to the playhead, each line with the
        // step it was written at.
        let shown_from = c.output.len().saturating_sub(MAX_OUTPUT_LINES);
        let mut output = div()
            .flex()
            .flex_col()
            .font_family(mono.clone())
            .text_size(px(size::TEXT_SMALL))
            .child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child(format!("OUTPUT \u{b7} {}", lines_count(c.output.len()))),
            );
        if shown_from > 0 {
            output = output.child(div().text_color(rgb(theme::FAINT)).child(format!(
                "{} earlier in the build log",
                lines_count(shown_from)
            )));
        }
        for (i, line) in c.output.iter().enumerate().skip(shown_from) {
            let color = match line.stream {
                Stream::Stderr => theme::STDERR,
                _ => theme::SOFT,
            };
            output = output.child(
                div()
                    .flex()
                    .gap(px(size::CARD_GAP))
                    .child(
                        div()
                            .id(SharedString::from(format!("proc-out-step-{i}")))
                            .flex_none()
                            .cursor_pointer()
                            .text_color(rgb(theme::FAINT))
                            .hover(|s| s.text_color(rgb(theme::AMBER)))
                            .on_click(seek_on_click(line.step, cx))
                            .child(thousands(line.step)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_color(rgb(color))
                            .child(line.text.clone()),
                    ),
            );
        }
        if c.later_lines > 0 {
            output = output.child(div().text_color(rgb(theme::FAINT)).child(format!(
                "{} more after this step",
                thousands(c.later_lines as u64)
            )));
        }
        body.child(output)
    }
}

/// A link that picks a row.
fn pick_link(
    id: SharedString,
    text: String,
    pick: Pick,
    cx: &mut Context<Scrubber>,
) -> gpui::Stateful<Div> {
    link(id, text)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| this.pick_row(Some(pick), cx)))
}

/// How a process ended, after the step: its status, or the signal that
/// killed it. A thread's end says nothing more.
fn how_it_ended(end: End) -> Option<String> {
    Some(match end.status? {
        ExitStatus::Code(code) => format!("with status {code}"),
        ExitStatus::Signal { signo, core } => {
            let dumped = if core { " (core dumped)" } else { "" };
            format!("killed by {}{dumped}", signal_name(signo))
        }
    })
}

/// "1 line" or "12 lines".
fn lines_count(n: usize) -> String {
    match n {
        1 => "1 line".to_string(),
        n => format!("{} lines", thousands(n as u64)),
    }
}

#[cfg(test)]
mod tests {
    // What a click on a row of the tree does to the pick and the playhead.
    use super::*;

    #[test]
    fn a_plain_click_picks_the_row_and_leaves_the_playhead() {
        // A plain click on the compiler's row picks it at the same step;
        // one that ends a text selection picks nothing.
        let row = Pick {
            pid: 3,
            tid: 3,
            start: 5,
        };
        let before = TreeState {
            picked: None,
            step: 12,
        };
        assert_eq!(
            after_row_click(before, row, RowClick::Plain),
            TreeState {
                picked: Some(row),
                step: 12,
            }
        );
        assert_eq!(after_row_click(before, row, RowClick::Selecting), before);
    }
}
