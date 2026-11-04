//! Blame for memory on screen: the underlines on values the source panel
//! can blame, the popover a click opens beside the value, which shows the
//! search's counts and then its answer, the chip in the app bar, and the
//! search's band and the write's mark on the timeline.
//!
//! Nothing here waits. A click starts the engine's search on a thread of
//! its own and the app stays live: the playhead scrubs, the source reads,
//! and a second value can be clicked while the first is searched. Escape
//! stops a search and closes its popover.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, HighlightStyle, MouseButton, Pixels, Point,
    Role, SharedString, Stateful, UnderlineStyle, anchored, deferred, div, prelude::*, px,
    relative, rgb, rgba,
};

use crate::blame::{
    AccessKind, Ask, Blame, Blamed, BlamedAccess, Expression, Searching, Target, access_place,
    blameable_on, counted, file_line, lanes, overlay, thread_place,
};
use crate::clock::Instant;
use crate::describe::thousands;
use crate::engine::Cancel;
use crate::source::Frame;
use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::Scrubber;
use crate::ui::widgets::{Availability, ButtonStyle, button, icon, spinner, tooltip};
use crate::view::View;

/// What keys a search: the run, the step, the value's address and size,
/// and what is asked.
pub(crate) type BlameKey = (PathBuf, u64, u64, u64, Ask);

/// A search, its answer once it comes, and what stops it.
pub(crate) struct BlameEntry {
    pub(crate) blame: Blame,
    started: Instant,
    /// The time it took, once it is done.
    took_ms: Option<u64>,
    cancel: Option<Cancel>,
}

impl BlameEntry {
    /// How long the search has taken so far, or took.
    fn elapsed_ms(&self) -> u64 {
        self.took_ms
            .unwrap_or_else(|| self.started.elapsed().as_millis() as u64)
    }
}

/// Every search the app has made in this window, and the one on screen.
#[derive(Default)]
pub(crate) struct Blames {
    entries: HashMap<BlameKey, BlameEntry>,
    /// The search the chip and the popover show.
    shown: Option<BlameKey>,
    /// Where the popover opens, while it is open.
    popover: Option<Point<Pixels>>,
    /// The popover being moved by its head, while it is.
    drag: Option<PopoverDrag>,
    /// The note of the value under the pointer in the source panel, and
    /// where the pointer is.
    hover: Option<(Point<Pixels>, String)>,
}

/// A drag of the popover by its head: where the pointer was pressed, and
/// where the popover was then.
#[derive(Clone, Copy, Debug)]
struct PopoverDrag {
    from: Point<Pixels>,
    start: Point<Pixels>,
}

impl PopoverDrag {
    /// Where the popover goes with the pointer at `pointer`: moved as far
    /// as the pointer has moved since the press.
    fn to(self, pointer: Point<Pixels>) -> Point<Pixels> {
        self.start + (pointer - self.from)
    }
}

impl Blames {
    /// Whether the popover is open.
    pub(crate) fn popover_open(&self) -> bool {
        self.popover.is_some() && self.shown.is_some()
    }

    /// The search on screen.
    fn shown(&self) -> Option<&BlameEntry> {
        self.entries.get(self.shown.as_ref()?)
    }

    /// The search for `target`'s last write, when one was made.
    fn last_write_search(&self, run: &Path, target: &Target) -> Option<&BlameEntry> {
        let key = (
            run.to_path_buf(),
            target.step,
            target.address,
            target.size,
            Ask::Writes(1),
        );
        self.entries.get(&key)
    }
}

/// How far below the clicked value the popover opens.
const POPOVER_DROP: f32 = 14.0;

/// The popover's width, and the draw order that puts it over the panels.
const POPOVER_WIDTH: f32 = 440.0;
const POPOVER_PRIORITY: usize = 2;

/// The underline under a value that can be blamed, one being searched,
/// and one whose last write was found.
const UNDERLINE_THICKNESS: f32 = 1.0;
const UNDERLINE_IDLE_A: u32 = 0xf2a541a0;
const UNDERLINE_DONE_THICKNESS: f32 = 2.0;

/// The search's band on the timeline: at least this wide, so a window
/// narrower than a pixel still shows, and its colors.
const BAND_MIN_WIDTH: f32 = 6.0;
const BAND_A: u32 = 0x7fb2ff40;
const FRONTIER_WIDTH: f32 = 2.0;
const WRITE_MARK_SIZE: f32 = 9.0;

/// The height of a lane in the reads view, and the width of its labels.
const LANE_HEIGHT: f32 = 20.0;
const LANE_LABEL_WIDTH: f32 = 110.0;
const LANE_MARK_SIZE: f32 = 8.0;

/// What a click on a value's popover says, after the search.
const KEEP_WORKING: &str = "Esc to stop \u{b7} keep working, the answer lands here";

impl Scrubber {
    /// Searches for who wrote `target`, as `ask` asks, unless that search
    /// was made already, and shows it in the popover at `popover` when one
    /// is given. A search with no popover runs unseen, as the ones the
    /// panel makes ahead of a click at a failure.
    pub(super) fn blame(
        &mut self,
        target: Target,
        ask: Ask,
        popover: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let run = session.run.path.clone();
        let key: BlameKey = (run.clone(), target.step, target.address, target.size, ask);
        if let Some(at) = popover {
            self.blames.shown = Some(key.clone());
            self.blames.popover = Some(at);
        }
        let failed = self
            .blames
            .entries
            .get(&key)
            .is_some_and(|e| matches!(e.blame.answer, Some(Err(_))));
        if self.blames.entries.contains_key(&key) && !failed {
            cx.notify();
            return;
        }

        // The engine's lines come through a channel as it says them, and
        // the channel closes when it is done.
        let cancel = Cancel::default();
        self.blames.entries.insert(
            key.clone(),
            BlameEntry {
                blame: Blame::new(target.clone(), ask),
                started: Instant::now(),
                took_ms: None,
                cancel: Some(cancel.clone()),
            },
        );
        let engine = self.engine.clone();
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let task = crate::jobs::on_own_thread(move || {
            engine.blame(&run, &target, ask, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            while let Some(line) = said.next().await {
                let Some(progress) = Searching::parse(&line) else {
                    continue;
                };
                let alive = this.update(cx, |this, cx| {
                    if let Some(entry) = this.blames.entries.get_mut(&key) {
                        entry.blame.progress = Some(progress);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(entry) = this.blames.entries.get_mut(&key) {
                    entry.took_ms = Some(entry.elapsed_ms());
                    entry.blame.answer = Some(result);
                    entry.cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Closes the popover; Escape on a search still going stops it too.
    pub(super) fn close_blame(&mut self, stop: bool, cx: &mut Context<Self>) {
        if stop
            && let Some(key) = self.blames.shown.clone()
            && let Some(entry) = self.blames.entries.get(&key)
            && entry.blame.searching()
        {
            if let Some(cancel) = &entry.cancel {
                cancel.cancel();
            }
            self.blames.entries.remove(&key);
            self.blames.shown = None;
        }
        self.blames.popover = None;
        self.blames.drag = None;
        cx.notify();
    }

    /// Starts moving the popover with the pointer, pressed at `at`.
    fn start_popover_drag(&mut self, at: Point<Pixels>) {
        if let Some(start) = self.blames.popover {
            self.blames.drag = Some(PopoverDrag { from: at, start });
        }
    }

    /// Moves the popover being dragged to follow the pointer at `at`.
    /// False when no popover is being dragged.
    pub(super) fn drag_popover(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.blames.drag else {
            return false;
        };
        self.blames.popover = Some(drag.to(at));
        cx.notify();
        true
    }

    /// Ends a drag of the popover, if one is under way.
    pub(super) fn end_popover_drag(&mut self) {
        self.blames.drag = None;
    }

    /// Opens the popover of the search on screen again, at `at`.
    fn reopen_blame(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.blames.popover = Some(at);
        cx.notify();
    }

    /// Searches ahead for each value on the shown frame's line, when the
    /// panel shows the step the run failed at: a click there is usually
    /// answered by the time it comes.
    pub(super) fn blame_ahead(
        &mut self,
        frame: &Frame,
        (pid, tid): (u32, u32),
        step: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(line) = frame.line else {
            return;
        };
        let targets: Vec<Target> = frame
            .expressions
            .iter()
            .filter(|e| e.line == line)
            .filter_map(|e| crate::blame::target(e, pid, tid, step))
            .collect();
        for target in targets {
            self.blame(target, Ask::Writes(1), None, cx);
        }
    }

    /// The values on the shown file's line under `at`, as that line
    /// underlines them and notes them.
    fn values_under(&self, at: Point<Pixels>) -> Option<(usize, Underlined)> {
        let hit = self
            .selecting
            .registry
            .hit(crate::selection::Surface::Source, at)?;
        let (file, frame) = self.shown_file()?;
        let (located, _) = self.source.as_ref()?.shown.as_ref()?.located()?;
        let step = self.source.as_ref()?.shown.as_ref()?.step();
        let line = file.lines.get(hit.line)?;
        let mapped = crate::ui::selectable::viewer_line(line);
        let run = &self.session.as_ref()?.run.path;
        let underlined = underlines(
            &self.blames,
            run,
            frame,
            (file.number_of(hit.line), line),
            &mapped,
            Vec::new(),
            (located.pid, located.tid, step),
        );
        Some((hit.offset, underlined))
    }

    /// A plain click in the source: on a value that can be blamed, its
    /// search starts, with its popover at the click.
    pub(super) fn source_click(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        if self.selected_text().is_some() {
            return;
        }
        let Some((offset, underlined)) = self.values_under(at) else {
            return;
        };
        let Some((_, target)) = underlined
            .clicks
            .into_iter()
            .find(|(range, _)| range.start <= offset && offset <= range.end)
        else {
            return;
        };
        self.blames.hover = None;
        self.blame(target, Ask::Writes(1), Some(at), cx);
    }

    /// The pointer over the source: the note of the value under it, if
    /// any, beside it.
    pub(super) fn source_hover(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let note = self.values_under(at).and_then(|(offset, underlined)| {
            underlined
                .notes
                .into_iter()
                .find(|(range, _)| range.start <= offset && offset < range.end)
                .map(|(_, note)| note)
        });
        let was = self.blames.hover.as_ref().map(|(_, n)| n.clone());
        if note == was {
            return;
        }
        self.blames.hover = note.map(|n| (at, n));
        cx.notify();
    }

    /// The pointer left the source: its note goes.
    pub(super) fn leave_source(&mut self, cx: &mut Context<Self>) {
        if self.blames.hover.take().is_some() {
            cx.notify();
        }
    }

    /// The note of the value under the pointer, beside it.
    pub(super) fn render_value_note(&self) -> Option<AnyElement> {
        let (at, note) = self.blames.hover.as_ref()?;
        let position = gpui::point(at.x + px(HOVER_OFFSET), at.y + px(HOVER_OFFSET));
        Some(
            deferred(
                anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(size::MENU_EDGE_MARGIN))
                    .child(
                        div()
                            .max_w(px(POPOVER_WIDTH))
                            .px(px(size::CARD_GAP))
                            .py(px(4.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(rgb(theme::LINE))
                            .bg(rgb(theme::RAISED))
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::SOFT))
                            .child(note.clone()),
                    ),
            )
            .with_priority(POPOVER_PRIORITY)
            .into_any_element(),
        )
    }

    /// The popover, while it is open: over everything, below the value
    /// clicked, the search's counts and then its answer.
    pub(super) fn render_blame_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let at = self.blames.popover?;
        let entry = self.blames.shown()?;
        let card = self.blame_card(entry, cx);
        let position = gpui::point(at.x, at.y + px(POPOVER_DROP));
        Some(
            deferred(
                anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(size::MENU_EDGE_MARGIN))
                    .child(card),
            )
            .with_priority(POPOVER_PRIORITY)
            .into_any_element(),
        )
    }

    /// The popover's card for `entry`.
    fn blame_card(&self, entry: &BlameEntry, cx: &mut Context<Self>) -> Stateful<Div> {
        let blame = &entry.blame;
        let target = &blame.target;
        let mono = self.fonts.mono.clone();
        let close = div()
            .id("blame-close")
            .role(Role::Button)
            .aria_label("Close")
            .cursor_pointer()
            .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
            // A press on the close button closes the popover rather than
            // starting to move it.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| this.close_blame(false, cx)));
        let title = match blame.ask {
            Ask::Reads => "ACCESSES",
            Ask::Writes(_) => match blame.searching() {
                true => "BLAME",
                false => "LAST WRITE",
            },
        };
        // The head moves the popover: pressed, the popover follows the
        // pointer until the button comes up.
        let head = div()
            .flex()
            .items_center()
            .gap(px(size::CARD_GAP))
            .cursor(CursorStyle::OpenHand)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, _| {
                    this.start_popover_drag(e.position)
                }),
            )
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(title),
            )
            .child(
                div()
                    .flex_grow(layout::FILL)
                    .min_w_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_color(rgb(theme::AMBER))
                    .child(format!(
                        "{} \u{b7} {} bytes at {:#x}",
                        target.text, target.size, target.address
                    )),
            )
            .child(close);

        let mut card = div()
            .id("blame-card")
            .occlude()
            .w(px(POPOVER_WIDTH))
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP))
            .p(px(size::CARD_PAD))
            .rounded(px(size::RADIUS_CARD))
            .border_1()
            .border_color(rgb(theme::AMBER_DEEP))
            .bg(rgb(theme::RAISED))
            .text_size(px(size::TEXT_UI))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(head);

        let elapsed = entry.elapsed_ms();
        card = match &blame.answer {
            None => {
                let mut lines = div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .font_family(mono.clone());
                for (i, line) in blame.progress_lines(elapsed).into_iter().enumerate() {
                    let row = div().flex().items_center().gap(px(size::CARD_GAP));
                    let row = match i {
                        0 => row
                            .child(spinner("blame-spinner", size::ICON_CLOSE, theme::BLUE))
                            .text_color(rgb(theme::BLUE_SOFT)),
                        _ => row.text_color(rgb(theme::SOFT)),
                    };
                    lines = lines.child(row.child(line));
                }
                card.child(lines).child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child(KEEP_WORKING),
                )
            }
            Some(Err(error)) => card.child(
                div()
                    .text_color(rgb(theme::RED_SOFT))
                    .child(error.to_string()),
            ),
            Some(Ok(blamed)) => match blame.ask {
                Ask::Reads => self.reads_view(card, blamed, cx),
                Ask::Writes(n) => self.writes_view(card, blamed, target, n, cx),
            },
        };
        card
    }

    /// The answer of a search for the last writes: each write, newest
    /// first, its buttons, and where the other threads were.
    fn writes_view(
        &self,
        card: Stateful<Div>,
        blamed: &Blamed,
        target: &Target,
        n: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mono = self.fonts.mono.clone();
        let writes: Vec<&BlamedAccess> = blamed.accesses.iter().rev().collect();
        let Some(last) = writes.first() else {
            let initial = blamed.values.first().and_then(|v| v.initial.clone());
            return card.child(div().text_color(rgb(theme::SOFT)).child(format!(
                    "Not written since step {}, where the search began{}.",
                    thousands(blamed.searched_from),
                    initial.map(|i| format!("; it held {i} there")).unwrap_or_default()
                )));
        };

        // The last write, then the ones before it that were asked for.
        let mut card = card.child(write_block(last, blamed.step, &mono, true));
        for earlier in writes.iter().skip(1) {
            card = card.child(write_block(earlier, blamed.step, &mono, false));
        }

        // What to do next.
        let step = last.step;
        let writer = crate::source::Thread::Of {
            pid: blamed.pid,
            tid: last.tid,
        };
        let reads_target = target.clone();
        let earlier_target = target.clone();
        let at = self.blames.popover;
        let buttons = div()
            .flex()
            .flex_wrap()
            .gap(px(size::CARD_GAP))
            .child(
                small_button("blame-go", ButtonStyle::Primary, "Go to the write")
                    .tooltip(tooltip(format!(
                        "Move the playhead to step {}, with the source of thread {}, the writer: \
                         a fork there is just before the write, which comes later in the step.",
                        thousands(step),
                        last.tid
                    )))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.show_thread_at(step, writer, cx)),
                    ),
            )
            .child(
                small_button("blame-reads", ButtonStyle::Neutral, "Show reads too").on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.blame(reads_target.clone(), Ask::Reads, at, cx)
                    }),
                ),
            )
            .when(writes.len() == n, |b| {
                b.child(
                    small_button(
                        "blame-earlier",
                        ButtonStyle::Neutral,
                        "Blame the earlier value",
                    )
                    .tooltip(tooltip("Go one more write back at the same address."))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.blame(earlier_target.clone(), Ask::Writes(n + 1), at, cx)
                    })),
                )
            });
        let mut card = card.child(buttons);

        // Where everyone else was at the last write.
        if !blamed.threads.is_empty() {
            let mut threads = div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .pt(px(size::CARD_GAP))
                .border_t_1()
                .border_color(rgb(theme::LINE_SOFT))
                .child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child(format!("OTHER THREADS AT STEP {}", thousands(last.step))),
                );
            for t in &blamed.threads {
                threads = threads.child(
                    div()
                        .flex()
                        .gap(px(size::CARD_GAP))
                        .font_family(mono.clone())
                        .text_size(px(size::TEXT_MONO))
                        .child(
                            div()
                                .flex_none()
                                .text_color(rgb(theme::SOFT))
                                .child(format!("thread {}", t.tid)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(theme::AMBER_PALE))
                                .child(thread_place(t)),
                        ),
                );
            }
            card = card.child(threads);
        }
        card.child(footer(blamed, &mono))
    }

    /// The answer of a search with reads: each thread that touched the
    /// value in a lane, its reads and writes along the steps.
    fn reads_view(
        &self,
        card: Stateful<Div>,
        blamed: &Blamed,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mono = self.fonts.mono.clone();
        let (Some(first), Some(last)) = (blamed.accesses.first(), blamed.accesses.last()) else {
            return card.child(
                div()
                    .text_color(rgb(theme::SOFT))
                    .child("No accesses found."),
            );
        };
        let mut rows = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .font_family(mono.clone());
        for (lane, (tid, accesses)) in lanes(blamed).into_iter().enumerate() {
            let mut track = div()
                .relative()
                .flex_grow(layout::FILL)
                .h(px(LANE_HEIGHT))
                .border_b_1()
                .border_color(rgb(theme::LINE));
            for (i, (at, access)) in accesses.into_iter().enumerate() {
                let color = match access.kind {
                    AccessKind::Read => theme::BLUE,
                    AccessKind::Write => theme::AMBER,
                };
                let step = access.step;
                let note = format!(
                    "Step {}: thread {} {} {} = {} \u{b7} {}",
                    thousands(step),
                    access.tid,
                    match access.kind {
                        AccessKind::Read => "read",
                        AccessKind::Write => "wrote",
                    },
                    blamed
                        .values
                        .get(access.value)
                        .map_or("", |v| v.text.as_str()),
                    access.after.as_deref().unwrap_or("?"),
                    access_place(access)
                );
                track = track.child(
                    div()
                        .id(SharedString::from(format!("blame-lane-{lane}-{i}")))
                        .absolute()
                        .top(px((LANE_HEIGHT - LANE_MARK_SIZE) / 2.0))
                        .left(relative(at))
                        .ml(px(-LANE_MARK_SIZE / 2.0))
                        .size(px(LANE_MARK_SIZE))
                        .rounded(px(LANE_MARK_SIZE / 2.0))
                        .bg(rgb(color))
                        .cursor_pointer()
                        .tooltip(tooltip(note))
                        .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx))),
                );
            }
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(size::CARD_GAP))
                    .child(
                        div()
                            .flex_none()
                            .w(px(LANE_LABEL_WIDTH))
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(format!("thread {tid}")),
                    )
                    .child(track),
            );
        }
        let span = div()
            .flex()
            .justify_between()
            .pl(px(LANE_LABEL_WIDTH + size::CARD_GAP))
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::FAINT))
            .child(thousands(first.step))
            .child(thousands(last.step));
        let legend = div()
            .flex()
            .gap(px(size::CARD_GAP))
            .text_size(px(size::TEXT_SMALL))
            .child(div().text_color(rgb(theme::AMBER)).child("\u{25cf} write"))
            .child(div().text_color(rgb(theme::BLUE)).child("\u{25cf} read"))
            .child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child("hover a mark for its line; click to go there"),
            );
        card.child(legend)
            .child(rows)
            .child(span)
            .child(footer(blamed, &mono))
    }

    /// The chip in the app bar for the search on screen: a spinner and
    /// the time while it searches, then the write's step. A click opens
    /// its popover again.
    pub(super) fn blame_chip(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let entry = self.blames.shown()?;
        let searching = entry.blame.searching();
        let (border, color) = match (&entry.blame.answer, searching) {
            (_, true) => (theme::BLUE_BORDER, theme::BLUE_SOFT),
            (Some(Err(_)), _) => (theme::RED_BORDER, theme::RED_SOFT),
            _ => (theme::AMBER_DEEP, theme::AMBER),
        };
        Some(
            div()
                .id("blame-chip")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(size::CARD_GAP / 2.0))
                .px(px(size::CARD_GAP))
                .py(px(2.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(border))
                .font_family(self.fonts.mono.clone())
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(color))
                .cursor_pointer()
                .when(searching, |chip| {
                    chip.child(spinner("blame-chip-spinner", size::TEXT_SMALL, theme::BLUE))
                })
                .child(entry.blame.chip(entry.elapsed_ms()))
                .tooltip(tooltip(
                    "The value blamed last. Click to show its search again.",
                ))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, e: &gpui::ClickEvent, _, cx| {
                    this.reopen_blame(e.position(), cx)
                })),
        )
    }

    /// The search on screen on the timeline: the steps searched so far as
    /// a band back from the value's step, its edge while it goes, and the
    /// write it found as a mark that goes to it.
    pub(super) fn blame_marks(&self, view: View, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(entry) = self.blames.shown() else {
            return Vec::new();
        };
        let blame = &entry.blame;
        let mut marks = Vec::new();
        if let Some((from, to)) = blame.searched() {
            let lo = from.max(view.lo);
            let hi = to.min(view.hi);
            if lo <= hi {
                let left = view.fraction_of(lo);
                let width = view.fraction_of(hi) - left;
                marks.push(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(left))
                        .w(relative(width.max(0.0)))
                        .min_w(px(BAND_MIN_WIDTH))
                        .bg(rgba(BAND_A))
                        .into_any_element(),
                );
            }
            if blame.searching() && view.contains(from) {
                marks.push(
                    div()
                        .absolute()
                        .top(px(-size::MARKER_OVERHANG))
                        .bottom(px(-size::MARKER_OVERHANG))
                        .left(relative(view.fraction_of(from)))
                        .w(px(FRONTIER_WIDTH))
                        .bg(rgb(theme::BLUE))
                        .into_any_element(),
                );
            }
        }
        if let Some(step) = blame.write_step()
            && view.contains(step)
        {
            let note = format!(
                "Step {}: {} last written here. Click to go there.",
                thousands(step),
                blame.target.text
            );
            marks.push(
                div()
                    .id("blame-write-mark")
                    .absolute()
                    .left(relative(view.fraction_of(step)))
                    .ml(px(-WRITE_MARK_SIZE / 2.0))
                    .top(px(-size::MARKER_OVERHANG - WRITE_MARK_SIZE))
                    .size(px(WRITE_MARK_SIZE))
                    .rounded(px(2.0))
                    .bg(rgb(theme::AMBER))
                    .border_1()
                    .border_color(rgb(theme::AMBER_INK))
                    .cursor_pointer()
                    .tooltip(tooltip(note))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(step, cx)))
                    .into_any_element(),
            );
        }
        marks
    }
}

/// A source line's highlights with its underlines, the values a click
/// on it blames, by the shown range of each one's name, and what every
/// value on it says on hover.
pub(super) struct Underlined {
    pub(super) highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub(super) clicks: Vec<(Range<usize>, Target)>,
    pub(super) notes: Vec<(Range<usize>, String)>,
}

/// How far from the pointer the hover note is drawn.
const HOVER_OFFSET: f32 = 16.0;

/// The highlights of a source line, `colors` being its syntax colors
/// over its shown text, with an underline under each value of
/// `frame`'s on it that can be blamed; and those values, each with the
/// shown range of its name and why it can or cannot be.
pub(super) fn underlines(
    blames: &Blames,
    run: &Path,
    frame: &Frame,
    (number, line): (u32, &str),
    mapped: &crate::selection::Mapped,
    colors: Vec<(Range<usize>, HighlightStyle)>,
    (pid, tid, step): (u32, u32, u64),
) -> Underlined {
    let shown = |r: &Range<usize>| mapped.shown_offset(r.start)..mapped.shown_offset(r.end);

    let mut clicks = Vec::new();
    let mut underlines = Vec::new();
    for (range, expression) in blameable_on(frame, number, line) {
        let Some(target) = crate::blame::target(expression, pid, tid, step) else {
            continue;
        };
        let entry = blames.last_write_search(run, &target);
        let (color, thickness) = match entry {
            Some(e) if e.blame.searching() => (rgb(theme::BLUE).into(), UNDERLINE_DONE_THICKNESS),
            Some(e) if e.blame.blamed().is_some() => {
                (rgb(theme::AMBER).into(), UNDERLINE_DONE_THICKNESS)
            }
            _ => (rgba(UNDERLINE_IDLE_A).into(), UNDERLINE_THICKNESS),
        };
        let style = HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(thickness),
                color: Some(color),
                wavy: false,
            }),
            ..HighlightStyle::default()
        };
        let range = shown(&range);
        underlines.push((range.clone(), style));
        clicks.push((range, target));
    }

    // Every value on the line names itself on hover: where it is, and
    // for one that cannot be blamed, why not.
    let notes = notes_on(frame, number, line, mapped);
    let merged = overlay(&colors, &underlines, |base, over| HighlightStyle {
        underline: over.underline,
        ..*base
    });
    Underlined {
        highlights: merged,
        clicks,
        notes,
    }
}

/// What each value on line `number` of `frame`'s file says on hover, by
/// the shown range of its name: its value and where it is, and for one
/// that cannot be blamed, why not.
fn notes_on(
    frame: &Frame,
    number: u32,
    line: &str,
    mapped: &crate::selection::Mapped,
) -> Vec<(Range<usize>, String)> {
    frame
        .expressions
        .iter()
        .filter(|e| e.line == number)
        .filter_map(|e| {
            let range = char_bytes(line, e.name_start, e.end)?;
            let shown = mapped.shown_offset(range.start)..mapped.shown_offset(range.end);
            Some((shown, note_of(e)))
        })
        .collect()
}

/// A value's hover note.
fn note_of(e: &Expression) -> String {
    match e.location.why_not() {
        None => format!("{} = {}. Click to see who last wrote it.", e.text, e.value),
        Some(why) => format!("{} = {}: {why}.", e.text, e.value),
    }
}

/// The bytes of `text` from character `start` to character `end`.
fn char_bytes(text: &str, start: u32, end: u32) -> Option<Range<usize>> {
    let at = |n: u32| -> Option<usize> {
        if n as usize == text.chars().count() {
            return Some(text.len());
        }
        text.char_indices().nth(n as usize).map(|(i, _)| i)
    };
    let range = at(start)?..at(end)?;
    (!range.is_empty()).then_some(range)
}

/// A write in the card: what the value became and when, by whom and on
/// which line, and what it was. The last write is set larger.
fn write_block(write: &BlamedAccess, asked: u64, mono: &SharedString, last: bool) -> Div {
    let became = write.after.clone().unwrap_or_else(|| "?".into());
    let before = asked.saturating_sub(write.step);
    let when = match last {
        true => format!(
            "became {became} at step {}, {} before here",
            thousands(write.step),
            counted(before, "step")
        ),
        false => format!(
            "before that, became {became} at step {}",
            thousands(write.step)
        ),
    };
    let text = write.place.as_ref().and_then(|p| p.text.clone());
    let place = write
        .place
        .as_ref()
        .and_then(file_line)
        .or_else(|| write.kernel.clone())
        .unwrap_or_default();
    let function = write
        .place
        .as_ref()
        .and_then(|p| p.function.clone())
        .unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_color(rgb(if last { theme::TEXT } else { theme::SOFT }))
                .when(last, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(when),
        )
        .child(
            div()
                .font_family(mono.clone())
                .text_size(px(size::TEXT_MONO))
                .text_color(rgb(theme::SOFT))
                .child(format!(
                    "thread {} {} \u{b7} {function} \u{b7} {place}",
                    write.tid, write.thread
                )),
        )
        .when_some(text, |d, text| {
            d.child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(size::TEXT_MONO))
                    .px(px(size::CARD_GAP))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .bg(rgb(theme::BG))
                    .text_color(rgb(theme::AMBER_PALE))
                    .child(text),
            )
        })
        .when_some(write.before.clone(), |d, was| {
            d.child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(format!("was {was}")),
            )
        })
}

/// The card's last line: how far back the search went and how long it
/// took.
fn footer(blamed: &Blamed, mono: &SharedString) -> Div {
    let writes = blamed
        .accesses
        .iter()
        .filter(|a| a.kind == AccessKind::Write)
        .count() as u64;
    let reads = blamed.accesses.len() as u64 - writes;
    let mut seen = counted(writes, "write");
    if reads > 0 {
        seen.push_str(&format!(", {}", counted(reads, "read")));
    }
    let mut text = format!(
        "searched back to step {} \u{b7} {} \u{b7} {seen} \u{b7} {:.1} s",
        thousands(blamed.searched_from),
        counted(blamed.windows as u64, "keyframe window"),
        blamed.elapsed_ms as f64 / 1000.0
    );
    if blamed.dropped > 0 {
        text.push_str(&format!(" \u{b7} {} more left out", blamed.dropped));
    }
    div()
        .font_family(mono.clone())
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::FAINT))
        .child(text)
}

/// A button the card's size.
fn small_button(id: &'static str, style: ButtonStyle, text: &'static str) -> Stateful<Div> {
    button(id, style, Availability::Enabled)
        .h(px(size::BUTTON_HEIGHT * 0.7))
        .text_size(px(size::TEXT_SMALL))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(text)
}

#[cfg(test)]
mod tests {
    // The hover notes of a line's values, from expressions as the engine
    // prints them.
    use super::*;
    use crate::blame::Location;

    /// A value in a register says why it cannot be blamed; one in memory
    /// says a click blames it.
    #[test]
    fn a_value_says_why_it_cannot_be_blamed() {
        let e = |text: &str, location| Expression {
            text: text.into(),
            value: "16".into(),
            location,
            line: 73,
            start: 0,
            name_start: 0,
            end: 1,
        };
        assert!(
            note_of(&e(
                "job",
                Location::Register {
                    register: Some("r15".into())
                }
            ))
            .contains("in register r15")
        );
        assert!(note_of(&e("p->queue", Location::Unreadable)).contains("cannot read"));
    }

    /// A dragged popover moves by as far as the pointer has from where
    /// the drag began, either way.
    #[test]
    fn a_dragged_popover_follows_the_pointer() {
        let drag = PopoverDrag {
            from: gpui::point(px(100.0), px(200.0)),
            start: gpui::point(px(40.0), px(60.0)),
        };
        assert_eq!(
            drag.to(gpui::point(px(130.0), px(150.0))),
            gpui::point(px(70.0), px(10.0))
        );
        assert_eq!(drag.to(drag.from), drag.start);
    }
}
