//! The window: GPUI start-up, the keyboard map, and the scrubber view.
//!
//! `scrubber` holds the view's state and what it does; `render` draws it.

mod icons;
mod render;
mod scrubber;
mod widgets;

use std::sync::Arc;

use gpui::{
    App, Bounds, KeyBinding, SharedString, TitlebarOptions, WindowBounds, WindowOptions, actions,
    prelude::*, px, size,
};

use crate::engine::Engine;
use crate::run::Session;
use scrubber::Scrubber;
use widgets::Fonts;

actions!(
    rewind,
    [
        PreviousEvent,
        NextEvent,
        StepBack,
        StepForward,
        GoToStart,
        GoToEnd,
        PreviousPhase,
        NextPhase,
        JumpToFailure,
        JumpToDivergence,
        ForkHere,
        OpenRun,
        Quit,
    ]
);

/// The key context the scrubber's bindings apply in.
const KEY_CONTEXT: &str = "Scrubber";

/// The window's size on first open: the design's frame.
const WINDOW_WIDTH: f32 = 1440.0;
const WINDOW_HEIGHT: f32 = 900.0;

/// The smallest window the layout still reads in.
const MIN_WIDTH: f32 = 960.0;
const MIN_HEIGHT: f32 = 600.0;

/// The app id Wayland compositors and desktop files match the window by.
const APP_ID: &str = "surf.lunchtime.Rewind";

/// What the app opens with.
pub struct Launch {
    /// The run to show, already read; None shows the empty state.
    pub session: Option<Session>,
    /// Where to put the playhead first; None picks the failure, or the end.
    pub step: Option<u64>,
    pub engine: Arc<dyn Engine>,
}

/// Runs the app until its window closes.
pub fn run(launch: Launch) {
    gpui_platform::application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
            bind_keys(cx);
            cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let fonts = Fonts::resolve(&cx.text_system().all_font_names());
            let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(MIN_WIDTH), px(MIN_HEIGHT))),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Rewind")),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.to_string()),
                ..Default::default()
            };
            let opened = cx.open_window(options, |window, cx| {
                cx.new(|cx| Scrubber::new(launch, fonts, window, cx))
            });
            if let Err(e) = opened {
                eprintln!("rewind-app: could not open a window: {e:#}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

/// The keyboard map. Arrows move between events, Shift+arrows by one
/// step, Page Up and Page Down between phases, Home and End to the ends,
/// f to the failure and d to the divergence.
fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", PreviousEvent, context),
        KeyBinding::new("right", NextEvent, context),
        KeyBinding::new("shift-left", StepBack, context),
        KeyBinding::new("shift-right", StepForward, context),
        KeyBinding::new("home", GoToStart, context),
        KeyBinding::new("end", GoToEnd, context),
        KeyBinding::new("pageup", PreviousPhase, context),
        KeyBinding::new("pagedown", NextPhase, context),
        KeyBinding::new("f", JumpToFailure, context),
        KeyBinding::new("d", JumpToDivergence, context),
        KeyBinding::new("ctrl-o", OpenRun, None),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}
