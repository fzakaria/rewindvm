//! Check from here: what the amber button at the end of the controls does
//! once its menu turns it from Fork from here, trying schedules from the
//! playhead, each a fork of the run there, under the run's own seed or
//! several, and the card in "At this step" that counts how they ended, a
//! cell per run and a row per seed.
//!
//! The engine runs the schedules as `rewind check --run` does, several at
//! once, and says a line as each ends, which fills the card as it goes.
//! The forks stay among the run's forks, where the Runs tab lists them.
//! A cell picked in the card opens its run, or compares this run with it.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use gpui::{
    Context, Div, MouseButton, Pixels, Point, SharedString, anchored, div, prelude::*, px, rgb,
};

use crate::describe::thousands;
use crate::engine::Cancel;
use crate::request::Request;
use crate::run::{Origin, short_id};
use crate::sweep::{Cell, Checked, HereAction, Outcome, SEEDED, SIZES, cells, schedule_said};
use crate::theme::{self, size};
use crate::ui::render::{card, card_title};
use crate::ui::scrubber::{EXAMPLE_FORK, NoticeTone, Replay, Scrubber, replay_unavailable};
use crate::ui::widgets::{Availability, ButtonStyle, button, tooltip};

/// The height of a schedule's cell.
const CELL_HEIGHT: f32 = 26.0;

/// The gap between cells, and how many cells make a row.
const CELL_GAP: f32 = 3.0;
const CELLS_PER_ROW: usize = 16;

/// The legend's swatches.
const SWATCH: f32 = 10.0;

/// The width of the menu of what the amber button does.
const HERE_MENU_WIDTH: f32 = 250.0;

/// A check from here, running or done. The check is of one run and is
/// kept while other runs are shown: showing another run neither stops a
/// running check nor drops a finished one's results.
pub struct Sweep {
    /// The run it forks, the step it forks at, and how many schedules and
    /// seeds it tries.
    pub run: PathBuf,
    pub step: u64,
    pub schedules: u64,
    pub seeds: u64,
    /// The run's own seed, once check has said it, which the other seeds
    /// count from.
    pub first_seed: Option<u64>,
    pub state: SweepState,
    /// The run picked in the card, by its cell.
    pub picked: Option<Cell>,
    request: Request,
    cancel: Cancel,
}

pub enum SweepState {
    /// The runs that have ended so far, by their cells.
    Running {
        ended: Vec<Cell>,
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

/// What stands in the way of starting a check.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Busy {
    /// Nothing: no check runs.
    No,
    /// A check of the run on screen runs, and its card shows it.
    WithThisRun,
    /// A check of another run runs, from `step` of the run in `run`. Its
    /// card shows when that run is on screen again.
    WithAnotherRun { run: PathBuf, step: u64 },
}

/// What stands in the way of a check of the run in `run`, given the latest
/// check.
fn busy_with(sweep: Option<&Sweep>, run: &Path) -> Busy {
    let Some(sweep) = sweep.filter(|s| matches!(s.state, SweepState::Running { .. })) else {
        return Busy::No;
    };
    if sweep.run == run {
        return Busy::WithThisRun;
    }
    Busy::WithAnotherRun {
        run: sweep.run.clone(),
        step: sweep.step,
    }
}

impl Scrubber {
    /// Tries schedules from the playhead, unless a check runs already or
    /// the run cannot be forked here.
    pub(super) fn check_here(&mut self, schedules: u64, seeds: u64, cx: &mut Context<Self>) {
        // The browser viewer has no machine to run the recording on.
        if cfg!(target_family = "wasm") && self.session.is_some() {
            self.replay_in_desktop_app(Replay::Check.doing(), cx);
            return;
        }

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
        let run = session.run.path.clone();

        // One check runs at a time. A check of another run goes on while
        // this run is on screen, with its card out of sight, so the app
        // says where the check is.
        match busy_with(self.sweep.as_ref(), &run) {
            Busy::No => {}
            Busy::WithThisRun => return,
            Busy::WithAnotherRun { run, step } => {
                let id = run.file_name().unwrap_or_default().to_string_lossy();
                self.notify_user(
                    NoticeTone::Info,
                    "Another run is being checked",
                    format!(
                        "Run {} is being checked from step {}. Open that run to see the check or to cancel it.",
                        short_id(&id),
                        thousands(step)
                    ),
                    cx,
                );
                return;
            }
        }
        let step = self.step;
        let request = self.requests.issue();
        let cancel = Cancel::default();
        self.sweep = Some(Sweep {
            run: run.clone(),
            step,
            schedules,
            seeds,
            first_seed: None,
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
            engine.check_from(&run, step, schedules, seeds, &cancel, &mut |line| {
                let _ = lines.unbounded_send(line.to_string());
            })
        });
        cx.spawn(async move |this, cx| {
            while let Some(line) = said.next().await {
                let Some((seed, schedule)) = schedule_said(&line) else {
                    continue;
                };
                let alive = this.update(cx, |this, cx| {
                    let Some(sweep) = this.sweep.as_mut().filter(|s| s.request == request) else {
                        return;
                    };
                    // The first line is the run itself, under its own seed.
                    let seed = seed.unwrap_or(0);
                    let first = *sweep.first_seed.get_or_insert(seed);
                    let cell = (seed.wrapping_sub(first), schedule);
                    if let SweepState::Running { ended } = &mut sweep.state
                        && cell != (0, 0)
                    {
                        ended.push(cell);
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
            HereAction::Check { schedules, seeds } => self.check_here(schedules, seeds, cx),
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
            .chain(SIZES.map(|schedules| HereAction::Check {
                schedules,
                seeds: 1,
            }))
            .chain(SEEDED.map(|(seeds, schedules)| HereAction::Check { schedules, seeds }));
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
                HereAction::Check {
                    schedules,
                    seeds: 1,
                } => format!("Check from here, {schedules} schedules"),
                HereAction::Check { schedules, seeds } => {
                    format!("Check from here, {seeds} seeds \u{d7} {schedules} schedules")
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

    /// The check card, while the run the check forks is the one on
    /// screen. The check stays with its run while another run is shown,
    /// and its card shows again when its run does.
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
                        "{} of {} {} ended",
                        ended.len(),
                        cells(sweep.seeds, sweep.schedules).len(),
                        runs_noun(sweep.seeds)
                    )))
                    .child(self.render_cells(sweep, cx))
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(if sweep.seeds > 1 {
                                "Each is a fork of this run at the step, run under its own seed and schedule. Cancel keeps the forks that ended."
                            } else {
                                "Each is a fork of this run at the step, run under its own schedule. Cancel keeps the forks that ended."
                            }),
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
                        "{} of {} {} ended differently",
                        checked.differing,
                        checked.tried,
                        runs_noun(sweep.seeds)
                    )))
                    .child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::SOFT))
                            .child(checked.summary()),
                    );
                // What the program said some run should make true and no
                // run did: the check never reached that case.
                if !checked.sometimes_never.is_empty() {
                    let quoted: Vec<String> = checked
                        .sometimes_never
                        .iter()
                        .map(|id| format!("\"{id}\""))
                        .collect();
                    card = card.child(
                        div()
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::AMBER_PALE))
                            .child(format!(
                                "Never true in any run: sometimes {}",
                                quoted.join(", ")
                            )),
                    );
                }
                card = card.child(self.render_cells(sweep, cx)).child(legend());
                if let Some(picked) = self.render_picked_schedule(sweep, checked, cx) {
                    card = card.child(picked);
                }
            }
        }
        Some(card)
    }

    /// A cell per run, CELLS_PER_ROW to a row and a row per seed, each
    /// seed's labelled when there are several: grey ended as this run
    /// did, blue differently, outlined timed out; while the check runs,
    /// filled once ended. A seed whose unperturbed run already differed
    /// leaves its schedules untried.
    fn render_cells(&self, sweep: &Sweep, cx: &mut Context<Self>) -> Div {
        let first = match &sweep.state {
            SweepState::Done(checked) => checked.schedules.first().map(|t| t.seed),
            _ => sweep.first_seed,
        }
        .unwrap_or(0);
        let mut rows = div().flex().flex_col().gap(px(CELL_GAP));
        for seed in 0..sweep.seeds {
            if sweep.seeds > 1 {
                rows = rows.child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child(if seed == 0 {
                            format!("seed {}, the run's own", first)
                        } else {
                            format!("seed {}, from step {}", first + seed, thousands(sweep.step))
                        }),
                );
            }
            let mut strip = div().flex().gap(px(CELL_GAP));
            let row: Vec<Cell> = cells(sweep.seeds, sweep.schedules)
                .into_iter()
                .filter(|c| c.0 == seed)
                .collect();
            for (i, cell_at) in row.into_iter().enumerate() {
                if i > 0 && i % CELLS_PER_ROW == 0 {
                    rows = rows.child(std::mem::replace(
                        &mut strip,
                        div().flex().gap(px(CELL_GAP)),
                    ));
                }
                let (cell_seed, schedule) = cell_at;
                let cell = div()
                    .id(SharedString::from(format!(
                        "check-cell-{cell_seed}-{schedule}"
                    )))
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
                    SweepState::Running { ended } if ended.contains(&cell_at) => cell
                        .bg(rgb(theme::PHASE_GREYS[1]))
                        .border_color(rgb(theme::PHASE_GREYS[1]))
                        .text_color(rgb(theme::SOFT)),
                    SweepState::Running { .. } | SweepState::Failed(_) => cell
                        .border_color(rgb(theme::LINE_2))
                        .text_color(rgb(theme::FAINT)),
                    SweepState::Done(checked) => {
                        let Some(tried) = checked.at(cell_at) else {
                            strip = strip.child(
                                cell.border_dashed()
                                    .border_color(rgb(theme::LINE_2))
                                    .text_color(rgb(theme::FAINT))
                                    .tooltip(tooltip(UNTRIED.to_string())),
                            );
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
                        let cell = if sweep.picked == Some(cell_at) {
                            cell.border_2().border_color(rgb(theme::AMBER))
                        } else {
                            cell
                        };
                        cell.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(sweep) = &mut this.sweep {
                                    sweep.picked = Some(cell_at);
                                }
                                cx.notify();
                            }))
                    }
                };
                strip = strip.child(cell);
            }
            rows = rows.child(strip);
        }
        rows
    }

    /// The picked schedule's run and how it ended, with Compare, which
    /// compares this run with it, and Open, which shows it.
    fn render_picked_schedule(
        &self,
        sweep: &Sweep,
        checked: &Checked,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let tried = checked.at(sweep.picked?)?;
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
                            "{}schedule {} \u{b7} run {} \u{b7} {}",
                            if sweep.seeds > 1 {
                                format!("seed {} \u{b7} ", tried.seed)
                            } else {
                                String::new()
                            },
                            tried.schedule,
                            short_id(&tried.id),
                            tried.ending.as_deref().unwrap_or("still running")
                        )),
                )
                // What the run violated, and where.
                .children(tried.violations.iter().map(|v| {
                    let at = v
                        .step
                        .map_or(String::new(), |s| format!("step {}: ", thousands(s)));
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::RED_SOFT))
                        .child(format!("{}: {at}{}", v.property, v.why))
                }))
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

/// What a cell the check did not try says.
const UNTRIED: &str =
    "Not tried: with this seed and its threads left alone, the fork already ended differently";

/// What a check counts: schedules under one seed, runs under several.
fn runs_noun(seeds: u64) -> &'static str {
    if seeds > 1 { "runs" } else { "schedules" }
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

#[cfg(test)]
mod tests {
    // What the latest check means for starting another, with no window.
    use super::*;
    use crate::request::Requests;

    fn sweep(run: &str, state: SweepState) -> Sweep {
        Sweep {
            run: run.into(),
            step: 40,
            schedules: 8,
            seeds: 1,
            first_seed: None,
            state,
            picked: None,
            request: Requests::default().issue(),
            cancel: Cancel::default(),
        }
    }

    #[test]
    fn a_check_stays_with_its_run_while_another_run_is_shown() {
        // A check of run a that still runs is in the way of another check
        // whichever run is on screen, and with run b on screen it says
        // which run and step it is of. A check that is done or failed, of
        // either run, is in the way of nothing.
        let running = sweep("/runs/a", SweepState::Running { ended: Vec::new() });
        let (a, b) = (Path::new("/runs/a"), Path::new("/runs/b"));
        assert_eq!(busy_with(None, a), Busy::No);
        assert_eq!(busy_with(Some(&running), a), Busy::WithThisRun);
        assert_eq!(
            busy_with(Some(&running), b),
            Busy::WithAnotherRun {
                run: a.to_path_buf(),
                step: 40
            }
        );
        let failed = sweep("/runs/a", SweepState::Failed("no KVM".into()));
        assert_eq!(busy_with(Some(&failed), a), Busy::No);
        assert_eq!(busy_with(Some(&failed), b), Busy::No);
    }
}
