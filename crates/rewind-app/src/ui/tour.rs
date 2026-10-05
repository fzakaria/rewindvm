//! The guided tour in the window, and the example run it starts on.
//!
//! The tour is a small callout next to the part of the window it talks
//! about, with Back, Next and Skip; Enter and the arrow keys move through
//! it and Escape ends it. Each stop moves the playhead to its subject.
//! F1, or the header's "?" button, starts it again.

use gpui::{
    AnyElement, Context, Div, FontWeight, Role, Window, anchored, deferred, div, prelude::*, px,
    relative, rgb,
};

use crate::describe::thousands;
use crate::examples;
use crate::theme::{self, size};
use crate::tour::{self, Advance, Anchor, Tour};
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::widgets::{Availability, ButtonStyle, button};
use crate::ui::{TOUR_CONTEXT, TourBack, TourNext, TourSkip};

/// The callout's width, and its gap from what it points at.
const CALLOUT_WIDTH: f32 = 360.0;
const CALLOUT_GAP: f32 = 10.0;
/// The callout keeps this far from the window's edges.
const WINDOW_MARGIN: f32 = 16.0;
/// Draw order of the callout over other deferred elements.
const CALLOUT_PRIORITY: usize = 1;

/// Whether opening the example starts the tour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TourStart {
    /// Only for someone who has not finished or skipped it before.
    UnlessDismissed,
    Always,
}

impl Scrubber {
    /// Opens the example runs, and the tour on them.
    pub(super) fn explore_example(
        &mut self,
        start: TourStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.requests.issue();
        self.opening = Some(request);
        self.loading = Some("the example run".into());
        cx.notify();
        let read = cx.background_executor().spawn(async { examples::open() });
        cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                // A run asked for after the example is the one to show.
                if this.opening != Some(request) {
                    return;
                }
                this.opening = None;
                this.loading = None;
                match result {
                    Ok(session) => {
                        this.show(session, None, cx);
                        let wanted = start == TourStart::Always || !tour::was_dismissed();
                        if wanted {
                            this.start_tour(window, cx);
                        }
                    }
                    Err(e) => this.notify_user(
                        NoticeTone::Error,
                        "Could not open the example",
                        format!("{e:#}"),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Starts the tour on the run on screen, or on the example when no run
    /// is open.
    pub(super) fn start_tour(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            self.explore_example(TourStart::Always, window, cx);
            return;
        };
        let stops = tour::stops_for(
            session.run.timeline.failure.is_some(),
            session.divergence_step().is_some(),
            self.family.as_ref().is_some_and(|f| f.runs.len() > 1),
        );
        self.tour = Some(Tour::new(stops));
        self.show_tour_stop(cx);
        window.focus(&self.tour_focus, cx);
    }

    /// Moves the playhead to what the current stop talks about.
    fn show_tour_stop(&mut self, cx: &mut Context<Self>) {
        let (Some(tour), Some(session)) = (&self.tour, &self.session) else {
            return;
        };
        let Some(stop) = tour.current() else {
            return;
        };
        let step = tour::playhead_step(
            stop.playhead,
            &session.run.timeline,
            session.divergence_step(),
        );
        // The stop about the Runs panel opens it.
        if stop.anchor == tour::Anchor::RunsPill {
            self.right_tab = crate::ui::tabs::RightTab::Runs;
            self.runs_tab = true;
        }
        self.go_to(step, cx);
        cx.notify();
    }

    pub(super) fn tour_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tour) = &mut self.tour else {
            return;
        };
        match tour.advance() {
            Advance::Moved => self.show_tour_stop(cx),
            Advance::Finished => self.end_tour(window, cx),
        }
    }

    pub(super) fn tour_back(&mut self, cx: &mut Context<Self>) {
        let Some(tour) = &mut self.tour else {
            return;
        };
        tour.back();
        self.show_tour_stop(cx);
    }

    /// Ends the tour, finished or skipped, and remembers that it was.
    pub(super) fn end_tour(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tour = None;
        tour::remember_dismissed();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The tour's callout, when the current stop points at `anchor`: a
    /// zero-size box under the anchor that the callout hangs from, drawn
    /// over everything and kept inside the window.
    pub(super) fn tour_callout(
        &self,
        anchor: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tour = self.tour.as_ref()?;
        let stop = tour.current()?;
        if stop.anchor != anchor {
            return None;
        }

        let counter = format!("{} of {}", tour.index + 1, tour.stops.len());
        let next_label = if tour.is_last() { "Done" } else { "Next" };
        let back = if tour.is_first() {
            Availability::Disabled
        } else {
            Availability::Enabled
        };
        let buttons = div()
            .flex()
            .items_center()
            .gap(px(size::CONTROL_GAP))
            .child(
                div()
                    .id("tour-skip")
                    .role(Role::Button)
                    .aria_label("Skip the tour")
                    .flex_1()
                    .cursor_pointer()
                    .text_color(rgb(theme::MUTED))
                    .hover(|s| s.text_color(rgb(theme::TEXT)))
                    .child("Skip")
                    .on_click(cx.listener(|this, _, window, cx| this.end_tour(window, cx))),
            )
            .child(
                button("tour-back", ButtonStyle::Neutral, back)
                    .h(px(size::NOTICE_BUTTON_HEIGHT))
                    .child("Back")
                    .on_click(cx.listener(|this, _, _, cx| this.tour_back(cx))),
            )
            .child(
                button("tour-next", ButtonStyle::Primary, Availability::Enabled)
                    .h(px(size::NOTICE_BUTTON_HEIGHT))
                    .child(next_label)
                    .on_click(cx.listener(|this, _, window, cx| this.tour_next(window, cx))),
            );

        let callout = div()
            .id("tour-callout")
            .track_focus(&self.tour_focus)
            .key_context(TOUR_CONTEXT)
            .on_action(cx.listener(|this, _: &TourNext, window, cx| this.tour_next(window, cx)))
            .on_action(cx.listener(|this, _: &TourBack, _, cx| this.tour_back(cx)))
            .on_action(cx.listener(|this, _: &TourSkip, window, cx| this.end_tour(window, cx)))
            .occlude()
            .w(px(CALLOUT_WIDTH))
            .mt(px(CALLOUT_GAP))
            .flex()
            .flex_col()
            .gap(px(size::CARD_GAP))
            .p(px(size::CARD_PAD))
            .rounded(px(size::RADIUS_CARD))
            .bg(rgb(theme::RAISED))
            .border_1()
            .border_color(rgb(theme::AMBER))
            .shadow_lg()
            // The callout hangs off buttons, whose text does not wrap and may
            // be semibold; it sets its own.
            .whitespace_normal()
            .font_weight(FontWeight::NORMAL)
            .font_family(self.fonts.ui.clone())
            .text_size(px(size::TEXT_CARD_TITLE))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::AMBER_PALE))
                            .child(stop.title),
                    )
                    .child(
                        div()
                            .font_family(self.fonts.mono.clone())
                            .text_size(px(size::TEXT_SMALL))
                            .text_color(rgb(theme::MUTED))
                            .child(counter),
                    ),
            )
            .child(div().text_color(rgb(theme::SOFT)).child(stop.body))
            .child(
                div()
                    .font_family(self.fonts.mono.clone())
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(format!("playhead at step {}", thousands(self.step))),
            )
            .child(buttons);

        Some(
            div()
                .absolute()
                .top(relative(1.0))
                .left_0()
                .child(
                    deferred(
                        anchored()
                            .snap_to_window_with_margin(px(WINDOW_MARGIN))
                            .child(callout),
                    )
                    .with_priority(CALLOUT_PRIORITY),
                )
                .into_any_element(),
        )
    }

    /// Hangs the tour's callout for `anchor` off `element`, which becomes
    /// the callout's reference box.
    pub(super) fn with_callout<E: ParentElement + Styled>(
        &self,
        element: E,
        anchor: Anchor,
        cx: &mut Context<Self>,
    ) -> E {
        match self.tour_callout(anchor, cx) {
            Some(callout) => element.relative().child(callout),
            None => element,
        }
    }
}

/// The empty state's first button.
pub(super) fn explore_button(cx: &mut Context<Scrubber>) -> gpui::Stateful<Div> {
    button("explore", ButtonStyle::Primary, Availability::Enabled)
        .child("Explore an example run")
        .on_click(cx.listener(|this, _, window, cx| {
            this.explore_example(TourStart::UnlessDismissed, window, cx)
        }))
}
