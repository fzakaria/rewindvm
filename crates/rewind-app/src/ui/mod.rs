//! The window: GPUI start-up, the keyboard map, and the scrubber view.
//!
//! `scrubber` holds the view's state and what it does; `render` draws it.

mod ansi;
mod bookmarks;
mod chrome;
mod compare;
mod icons;
mod lanes;
mod licensing;
mod link;
mod notices;
mod render;
mod runs;
mod scrubber;
mod search;
mod selectable;
mod shortcuts;
mod sideways;
mod source;
mod splits;
mod step_entry;
mod stride;
mod sweep;
mod tabs;
mod terminal;
mod tour;
mod viewer;
mod widgets;
mod zoom;

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
        ShowOtherRun,
        GoBack,
        GoForward,
        CloseNearest,
        GoToStep,
        ConfirmStep,
        OpenSearch,
        ConfirmSearch,
        AddBookmark,
        SaveBookmark,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ShowShortcuts,
        SearchNext,
        SearchPrevious,
        ForkHere,
        ToggleSource,
        ToggleThreads,
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
/// which sends its keys to the command running in it, and a text field,
/// where letters are typed.
const SCRUBBER_KEYS: &str = "Scrubber && !Terminal && !TextInput";

/// The key context of the terminal pane.
pub const TERMINAL_CONTEXT: &str = "Terminal";

/// Where the app-wide keys apply: everywhere but the terminal pane.
const APP_KEYS: &str = "!Terminal";

/// The key context of the license dialog's paste field.
const LICENSE_CONTEXT: &str = "LicenseDialog";

/// The key context of the Open link dialog's field.
const LINK_CONTEXT: &str = "LinkDialog";

/// The key context of the shortcut sheet.
const SHORTCUTS_CONTEXT: &str = "Shortcuts";

/// The key context of the bookmark dialog's field.
const BOOKMARK_CONTEXT: &str = "BookmarkField";

/// The key context of the search box's field.
const SEARCH_CONTEXT: &str = "SearchField";

/// The key context of the step readout's field.
const STEP_CONTEXT: &str = "StepField";

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

/// The keyboard map. The shortcut sheet lists every key by what it does
/// (`shortcuts::SHORTCUTS`), so a key added here is added there too.
///
/// The scrubber's keys apply outside the terminal pane and the text
/// fields, where they are typed; each dialog and field binds Enter and
/// Escape in its own key context. In the terminal pane, where Ctrl+C
/// belongs to the command, Ctrl+Shift+C and Ctrl+Shift+V copy and paste.
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
        KeyBinding::new("x", ShowOtherRun, context),
        KeyBinding::new("alt-left", GoBack, context),
        KeyBinding::new("alt-right", GoForward, context),
        KeyBinding::new("escape", CloseNearest, context),
        KeyBinding::new("g", GoToStep, context),
        KeyBinding::new("ctrl-f", OpenSearch, context),
        KeyBinding::new("/", OpenSearch, context),
        KeyBinding::new("b", AddBookmark, context),
        KeyBinding::new("=", ZoomIn, context),
        KeyBinding::new("+", ZoomIn, context),
        KeyBinding::new("shift-=", ZoomIn, context),
        KeyBinding::new("-", ZoomOut, context),
        KeyBinding::new("0", ZoomReset, context),
        KeyBinding::new("?", ShowShortcuts, context),
        KeyBinding::new("shift-/", ShowShortcuts, context),
        KeyBinding::new("escape", CloseDialog, Some(SHORTCUTS_CONTEXT)),
        KeyBinding::new("?", ShowShortcuts, Some(SHORTCUTS_CONTEXT)),
        KeyBinding::new("shift-/", ShowShortcuts, Some(SHORTCUTS_CONTEXT)),
        KeyBinding::new("s", ToggleSource, context),
        KeyBinding::new("t", ToggleThreads, context),
        KeyBinding::new("ctrl-c", CopySelection, context),
        KeyBinding::new("ctrl-a", SelectAll, context),
        KeyBinding::new("ctrl-c", CopySelection, Some(LICENSE_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(LICENSE_CONTEXT)),
        KeyBinding::new("ctrl-shift-c", TerminalCopy, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some(TERMINAL_CONTEXT)),
        KeyBinding::new("ctrl-v", PasteLicense, Some(LICENSE_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(LICENSE_CONTEXT)),
        KeyBinding::new("enter", SaveBookmark, Some(BOOKMARK_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(BOOKMARK_CONTEXT)),
        KeyBinding::new("enter", ConfirmSearch, Some(SEARCH_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(SEARCH_CONTEXT)),
        KeyBinding::new("down", SearchNext, Some(SEARCH_CONTEXT)),
        KeyBinding::new("up", SearchPrevious, Some(SEARCH_CONTEXT)),
        KeyBinding::new("enter", ConfirmStep, Some(STEP_CONTEXT)),
        KeyBinding::new("escape", CloseDialog, Some(STEP_CONTEXT)),
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
