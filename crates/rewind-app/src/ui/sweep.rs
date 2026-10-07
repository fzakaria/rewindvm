//! Check from here: what the amber button at the end of the controls does
//! once its menu turns it from Fork from here, trying schedules from the
//! playhead, each a fork of the run there, and the card in "At this step"
//! that counts how they ended, a cell per schedule.
//!
//! The engine runs the schedules as `rewind check --run` does, several at
//! once, and says a line as each ends, which fills the card as it goes.
//! The forks stay among the run's forks, where the Runs tab lists them.
//! A cell picked in the card opens its run, or compares this run with it.

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    Context, Div, MouseButton, Pixels, Point, SharedString, anchored, div, prelude::*, px, rgb,
};

use crate::describe::thousands;
use crate::engine::Cancel;
use crate::request::Request;
use crate::run::{Origin, short_id};
use crate::sweep::{Checked, HereAction, Outcome, SIZES, schedule_said};
use crate::theme::{self, size};
use crate::ui::render::{card, card_title};
use crate::ui::scrubber::{EXAMPLE_FORK, NoticeTone, Replay, Scrubber, replay_unavailable};
use crate::ui::widgets::{Availability, ButtonStyle, button};

/// The height of a schedule's cell.
const CELL_HEIGHT: f32 = 26.0;

/// The gap between cells, and how many cells make a row.
const CELL_GAP: f32 = 3.0;
const CELLS_PER_ROW: usize = 16;

/// The legend's swatches.
const SWATCH: f32 = 10.0;

/// The width of the menu of what the amber button does.
const HERE_MENU_WIDTH: f32 = 250.0;

/// A check from here, running or done.
pub struct Sweep {
    /// The run it forks, the step it forks at, and how many schedules it
    /// tries.
    pub run: PathBuf,
    pub step: u64,
    pub schedules: u64,
    pub state: SweepState,
    /// The schedule picked in the card, by its place among the forks.
    pub picked: Option<usize>,
    request: Request,
    cancel: Cancel,
}

pub enum SweepState {
    /// The schedules that have ended so far.
    Running {
        ended: Vec<u64>,
    },
    Done(Checked),
    Failed(String),
}

impl Drop for Sweep {
    fn drop(&mut self) {
        if matches!(self.state, SweepState::Running { .. }) {
            self.cancel.cancel();
        }
    }
}

impl Scrubber {
    /// Tries schedules from the playhead, unless a check runs already or
    /// the run cannot be forked here.
    pub(super) fn check_here(&mut self, schedules: u64, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if session.run.origin == Origin::Example {
            self.notify_user(
                NoticeTone::Info,
                "The example can be scrubbed, not checked",
                EXAMPLE_FORK,
                cx,
            );
            return;
        }
        if let Some(reason) = replay_unavailable(session, Replay::Check, self.importing.is_some()) {
            self.notify_user(
                NoticeTone::Info,
                "This run cannot be checked yet",
                reason,
                cx,
            );
            return;
        }
        if self.checking() {
            return;
        }
        let run = session.run.path.clone();
        let step = self.step;
        let request = self.requests.issue();
        let cancel = Cancel::default();
        self.sweep = Some(Sweep {
            run: run.clone(),
            step,
            schedules,
            state: SweepState::Running { ended: Vec::new() },
            picked: None,
            request,
            cancel: cancel.clone(),
        });
        cx.notify();

        // Check's lines come through a channel as it says them, and the
        // channel closes when it is done.
        let engine = self.engine.clone();
        let (lines, mut said) = futures::channel::mpsc::unbounded::<String>();
        let task = crate::jobs::on_own_thread(move || {
            engine.check_from(&run, step, schedules, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            while let Some(line) = said.next().await {
                let Some(schedule) = schedule_said(&line) else {
                    continue;
                };
                let alive = this.update(cx, |this, cx| {
                    let Some(sweep) = this.sweep.as_mut().filter(|s| s.request == request) else {
                        return;
                    };
                    if let SweepState::Running { ended } = &mut sweep.state
                        && schedule > 0
                    {
                        ended.push(schedule);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }

            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(sweep) = this.sweep.as_mut().filter(|s| s.request == request) else {
                    return;
                };
                sweep.state = match result {
                    Ok(checked) => SweepState::Done(checked),
                    Err(e) => SweepState::Failed(e.to_string()),
                };
                this.reload_runs(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// What the amber button does now: forks, or checks with the count
    /// chosen last.
    pub(super) fn act_here(&mut self, cx: &mut Context<Self>) {
        match self.here_action {
            HereAction::Fork => self.fork_here(cx),
            HereAction::Check { schedules } => self.check_here(schedules, cx),
        }
    }

    /// Opens the menu of what the amber button does, under the pointer.
    pub(super) fn open_here_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.here_menu = Some(position);
        cx.notify();
    }

    pub(super) fn close_here_menu(&mut self, cx: &mut Context<Self>) {
        if self.here_menu.take().is_some() {
            cx.notify();
        }
    }

    fn choose_here_action(&mut self, action: HereAction, cx: &mut Context<Self>) {
        self.here_action = action;
        self.here_menu = None;
        cx.notify();
    }

    /// The menu of what the amber button does, over a backdrop that closes
    /// it, when it is open: fork once, or check with one of the counts.
    pub(super) fn render_here_menu(&self, cx: &mut Context<Self>) -> Option<Div> {
        let position = self.here_menu?;
        let actions = std::iter::once(HereAction::Fork)
            .chain(SIZES.map(|schedules| HereAction::Check { schedules }));
        let mut items = div()
            .w(px(HERE_MENU_WIDTH))
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
        for (i, action) in actions.enumerate() {
            let label = match action {
                HereAction::Fork => "Fork from here, once".to_string(),
                HereAction::Check { schedules } => {
                    format!("Check from here, {schedules} schedules")
                }
            };
            items = items.child(
                div()
                    .id(SharedString::from(format!("here-{i}")))
                    .px(px(size::MENU_ITEM_PAD_X))
                    .py(px(size::MENU_ITEM_PAD_Y))
                    .rounded(px(size::RADIUS_MENU_ITEM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                    .when(action == self.here_action, |d| {
                        d.text_color(rgb(theme::AMBER))
                    })
                    .child(label)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.choose_here_action(action, cx)),
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
                    // it does not also press what is under it.
                    div()
                        .id("here-backdrop")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.close_here_menu(cx)),
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

    /// Whether a check from here is running, for the amber button's icon.
    pub(super) fn checking(&self) -> bool {
        self.sweep
            .as_ref()
            .is_some_and(|s| matches!(s.state, SweepState::Running { .. }))
    }

    /// Stops the check that runs; the forks that ended stay.
    fn cancel_check(&mut self, cx: &mut Context<Self>) {
        self.sweep = None;
        self.reload_runs(cx);
        cx.notify();
    }

    /// The check card, for the run on screen.
    pub(super) fn render_sweep(&self, cx: &mut Context<Self>) -> Option<Div> {
        let sweep = self.sweep.as_ref()?;
        if self.session().run.path != sweep.run {
            return None;
        }
        let doing = match sweep.state {
            SweepState::Running { .. } => "Checking",
            SweepState::Done(_) | SweepState::Failed(_) => "Checked",
        };
        let mut card = card(theme::AMBER_CARD, theme::AMBER_DEEP).child(
            card_title(theme::AMBER_PALE)
                .child(format!("{doing} from step {}", thousands(sweep.step))),
        );
        match &sweep.state {
            SweepState::Failed(why) => {
                return Some(card.child(div().text_color(rgb(theme::RED_SOFT)).child(why.clone())));
            }
            SweepState::Running { ended } => {
                card = card
                    .child(div().text_color(rgb(theme::TEXT)).child(format!(
                        "{} of {} schedules ended",
                        ended.len(),
                        sweep.schedules
                    )))
                    .child(self.render_cells(sweep, cx))
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child("Each is a fork of this run at the step, run under its own schedule. Cancel keeps the forks that ended."),
                    )
                    .child(
                        div().flex().child(
                            button("check-cancel", ButtonStyle::Neutral, Availability::Enabled)
                                .h(px(size::NOTICE_BUTTON_HEIGHT))
                                .child("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_check(cx))),
                        ),
                    );
            }
            SweepState::Done(checked) => {
                card = card
                    .child(div().text_color(rgb(theme::TEXT)).child(format!(
                        "{} of {} schedules ended differently",
                        checked.differing, checked.tried
                    )))
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::SOFT))
                            .child(checked.summary()),
                    )
                    .child(self.render_cells(sweep, cx))
                    .child(legend());
                if let Some(picked) = self.render_picked_schedule(sweep, checked, cx) {
                    card = card.child(picked);
                }
            }
        }
        Some(card)
    }

    /// A cell per schedule, CELLS_PER_ROW to a row: grey ended as this
    /// run did, blue differently, outlined timed out; while the check
    /// runs, filled once ended.
    fn render_cells(&self, sweep: &Sweep, cx: &mut Context<Self>) -> Div {
        let mut rows = div().flex().flex_col().gap(px(CELL_GAP));
        let mut strip = div().flex().gap(px(CELL_GAP));
        for (i, schedule) in (1..=sweep.schedules).enumerate() {
            if i > 0 && i % CELLS_PER_ROW == 0 {
                rows = rows.child(std::mem::replace(
                    &mut strip,
                    div().flex().gap(px(CELL_GAP)),
                ));
            }
            let cell = div()
                .id(SharedString::from(format!("check-cell-{schedule}")))
                .flex()
                .flex_1()
                .items_end()
                .justify_center()
                .h(px(CELL_HEIGHT))
                .pb(px(2.0))
                .rounded(px(size::RADIUS_SEGMENT))
                .border_1()
                .font_family(self.fonts.mono.clone())
                .text_size(px(size::TEXT_SMALL - 2.0))
                .child(schedule.to_string());
            let cell = match &sweep.state {
                SweepState::Running { ended } if ended.contains(&schedule) => cell
                    .bg(rgb(theme::PHASE_GREYS[1]))
                    .border_color(rgb(theme::PHASE_GREYS[1]))
                    .text_color(rgb(theme::SOFT)),
                SweepState::Running { .. } | SweepState::Failed(_) => cell
                    .border_color(rgb(theme::LINE_2))
                    .text_color(rgb(theme::FAINT)),
                SweepState::Done(checked) => {
                    let Some(tried) = checked.forks().get(i) else {
                        strip = strip.child(cell.border_color(rgb(theme::LINE_2)));
                        continue;
                    };
                    let cell = match tried.outcome() {
                        Outcome::Same => cell
                            .bg(rgb(theme::PHASE_GREYS[3]))
                            .border_color(rgb(theme::PHASE_GREYS[3]))
                            .text_color(rgb(theme::SOFT)),
                        Outcome::Differs => cell
                            .bg(rgb(theme::BLUE))
                            .border_color(rgb(theme::BLUE))
                            .text_color(rgb(theme::BG)),
                        Outcome::TimedOut => cell
                            .border_dashed()
                            .border_color(rgb(theme::MUTED))
                            .text_color(rgb(theme::MUTED)),
                    };
                    let cell = if sweep.picked == Some(i) {
                        cell.border_2().border_color(rgb(theme::AMBER))
                    } else {
                        cell
                    };
                    cell.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(sweep) = &mut this.sweep {
                                sweep.picked = Some(i);
                            }
                            cx.notify();
                        }))
                }
            };
            strip = strip.child(cell);
        }
        rows.child(strip)
    }

    /// The picked schedule's run and how it ended, with Compare, which
    /// compares this run with it, and Open, which shows it.
    fn render_picked_schedule(
        &self,
        sweep: &Sweep,
        checked: &Checked,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let tried = checked.forks().get(sweep.picked?)?;
        let this_run = sweep.run.clone();
        let dir = tried.dir.clone();
        let (compare_this, compare_dir) = (this_run.clone(), dir.clone());
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(size::CARD_GAP))
                .pt(px(size::CARD_GAP))
                .border_t_1()
                .border_color(rgb(theme::AMBER_DEEP))
                .child(
                    div()
                        .font_family(self.fonts.mono.clone())
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::TEXT))
                        .child(format!(
                            "schedule {} \u{b7} run {} \u{b7} {}",
                            tried.schedule,
                            short_id(&tried.id),
                            tried.ending.as_deref().unwrap_or("still running")
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(size::CARD_GAP))
                        .child(
                            button(
                                "check-compare",
                                ButtonStyle::Divergence,
                                Availability::Enabled,
                            )
                            .h(px(size::NOTICE_BUTTON_HEIGHT))
                            .child("Compare")
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.open(compare_this.clone(), Some(compare_dir.clone()), cx)
                                },
                            )),
                        )
                        .child(
                            button("check-open", ButtonStyle::Neutral, Availability::Enabled)
                                .h(px(size::NOTICE_BUTTON_HEIGHT))
                                .child("Open")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open(dir.clone(), Some(this_run.clone()), cx)
                                })),
                        ),
                ),
        )
    }
}

/// What the cells' colors mean.
fn legend() -> Div {
    let swatch = |bg: Option<u32>, border: u32| {
        let s = div()
            .flex_none()
            .size(px(SWATCH))
            .rounded(px(2.0))
            .border_1()
            .border_color(rgb(border));
        match bg {
            Some(bg) => s.bg(rgb(bg)),
            None => s.border_dashed(),
        }
    };
    let item = |swatch: Div, text: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(px(size::LEGEND_GAP * 1.5))
            .child(swatch)
            .child(text)
    };
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(size::SECTION_GAP))
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::MUTED))
        .child(item(
            swatch(Some(theme::PHASE_GREYS[3]), theme::PHASE_GREYS[3]),
            "as this run",
        ))
        .child(item(swatch(Some(theme::BLUE), theme::BLUE), "differently"))
        .child(item(swatch(None, theme::MUTED), "timed out"))
}
