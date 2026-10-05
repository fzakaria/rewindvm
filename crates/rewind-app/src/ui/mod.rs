//! The window: GPUI start-up, the keyboard map, and the scrubber view.
//!
//! `scrubber` holds the view's state and what it does; `render` draws it.

mod chrome;
mod icons;
mod licensing;
mod link;
mod render;
mod scrubber;
mod selectable;
mod sideways;
mod source;
mod splits;
mod terminal;
mod tour;
mod viewer;
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
        ToggleSource,
        OpenRun,
        EnterLicense,
        PasteLicense,
        ConfirmLink,
        CloseDialog,
        StartTour,
        TourNext,
        TourBack,
        TourSkip,
        CopySelection,
        SelectAll,
        TerminalCopy,
        TerminalPaste,
        Quit,
    ]
);

/// The key context of the scrubber's root.
const KEY_CONTEXT: &str = "Scrubber";

/// Where the scrubber's keys apply: anywhere in it but the terminal pane,
/// which sends its keys to the command running in it.
const SCRUBBER_KEYS: &str = "Scrubber && !Terminal";

/// The key context of the terminal pane.
pub const TERMINAL_CONTEXT: &str = "Terminal";

/// Where the app-wide keys apply: everywhere but the terminal pane.
const APP_KEYS: &str = "!Terminal";

/// The key context of the license dialog's paste field.
const LICENSE_CONTEXT: &str = "LicenseDialog";

/// The key context of the Open link dialog's field.
const LINK_CONTEXT: &str = "LinkDialog";

/// The key context of the tour's callout.
const TOUR_CONTEXT: &str = "Tour";

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
    /// What the right column shows first.
    pub right: RightColumn,
    pub engine: Arc<dyn Engine>,
}

/// What the right column shows: "At this step", or the source panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightColumn {
    AtStep,
    Source,
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

            load_bundled_fonts(cx);
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
                window_decorations: Some(chrome::requested_decorations()),
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

/// A directory of .ttf and .otf files the app loads at start, for installs
/// that ship their own fonts, like the portable tarball.
pub const FONTS_ENV: &str = "REWIND_APP_FONTS";

/// Loads the fonts in the directory REWIND_APP_FONTS names, if any. A font
/// that does not load only costs itself: the system's fonts stand in.
fn load_bundled_fonts(cx: &App) {
    const FONT_EXTENSIONS: [&str; 2] = ["ttf", "otf"];
    let Some(dir) = std::env::var_os(FONTS_ENV) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        log::warn!(
            "{FONTS_ENV}: cannot read {}",
            std::path::Path::new(&dir).display()
        );
        return;
    };
    let fonts: Vec<std::borrow::Cow<'static, [u8]>> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| FONT_EXTENSIONS.contains(&e))
        })
        .filter_map(|path| std::fs::read(path).ok())
        .map(std::borrow::Cow::Owned)
        .collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        log::warn!("{FONTS_ENV}: {e:#}");
    }
}

/// The keyboard map. Arrows move between events, Shift+arrows by one
/// step, Page Up and Page Down between phases, Home and End to the ends,
/// f to the failure and d to the divergence; s opens or closes the source
/// panel. Ctrl+C copies the selected
/// text and Ctrl+A selects all of the panel last clicked in; in the
/// terminal pane, where Ctrl+C belongs to the command, Ctrl+Shift+C and
/// Ctrl+Shift+V copy and paste.
fn bind_keys(cx: &mut App) {
    // The Open link dialog's text field edits with its own keys.
    cx.bind_keys(rewind_text_input::bindings());
    let context = Some(SCRUBBER_KEYS);
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
        KeyBinding::new("s", ToggleSource, context),
        KeyBinding::new("ctrl-c", CopySelection, context),
        KeyBinding::new("ctrl-a", SelectAll, context),
        KeyBinding::new("ctrl-c", CopySelection, Some(LICENSE_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(LICENSE_CONTEXT)),
        KeyBinding::new("ctrl-shift-c", TerminalCopy, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("ctrl-v", PasteLicense, Some(LICENSE_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(LICENSE_CONTEXT)),
        KeyBinding::new("enter", ConfirmLink, Some(LINK_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(LINK_CONTEXT)),
        KeyBinding::new("enter", TourNext, Some(TOUR_CONTEXT)),
        KeyBinding::new("right", TourNext, Some(TOUR_CONTEXT)),
        KeyBinding::new("left", TourBack, Some(TOUR_CONTEXT)),
        KeyBinding::new("escape", TourSkip, Some(TOUR_CONTEXT)),
        KeyBinding::new("f1", StartTour, Some(APP_KEYS)),
        KeyBinding::new("ctrl-o", OpenRun, Some(APP_KEYS)),
        KeyBinding::new("ctrl-q", Quit, Some(APP_KEYS)),
    ]);
}
