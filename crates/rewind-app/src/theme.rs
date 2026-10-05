//! Colors, fonts and sizes of the scrubber, taken from the product design
//! (the Scrubber mockup and the site's palette in site/style.css).
//!
//! Colors are 0xRRGGBB words for `gpui::rgb`, or 0xRRGGBBAA words for
//! `gpui::rgba` where the name ends in `_A`.

/// The window behind everything.
pub const BG: u32 = 0x0e1013;
/// Panels and the header bar.
pub const PANEL: u32 = 0x121418;
/// Cards and buttons raised over a panel.
pub const RAISED: u32 = 0x171a1f;
/// Borders between the header, the timeline and the panels.
pub const LINE: u32 = 0x262a31;
/// Button borders.
pub const LINE_2: u32 = 0x2f343c;
/// The rule under a panel title.
pub const LINE_SOFT: u32 = 0x1f2329;

/// Body text.
pub const TEXT: u32 = 0xebe7df;
/// Secondary text: log lines, card bodies.
pub const SOFT: u32 = 0xc9c5bd;
/// Labels and readout captions.
pub const MUTED: u32 = 0xa19d95;
/// Step numbers in the log and file lists.
pub const FAINT: u32 = 0x5f6571;
/// A run picked in the Runs panel, for copying or deleting.
pub const ROW_PICKED: u32 = 0x1b2633;
/// The lines of the family graph in the Runs panel.
pub const GRAPH_LINE: u32 = 0x4d5463;
/// Timeline tick marks.
pub const TICK: u32 = 0x5b6170;

/// The playhead and anything the user can act on.
pub const AMBER: u32 = 0xf2a541;
/// The primary button's text.
pub const AMBER_INK: u32 = 0x1a1206;
/// The primary button under the pointer.
pub const AMBER_HI: u32 = 0xffc978;
/// The glow around the playhead: amber at 18 percent.
pub const AMBER_GLOW_A: u32 = 0xf2a5412e;
/// The phase the playhead is in, the fork card, and a running run's pill.
pub const AMBER_DEEP: u32 = 0x5a4520;
/// Text on the active phase and the fork card's title.
pub const AMBER_PALE: u32 = 0xf2d3a0;
/// The fork card's and a running run's pill's background.
pub const AMBER_CARD: u32 = 0x221a0e;
/// The focus ring around the timeline.
pub const FOCUS_RING_A: u32 = 0xf2a54140;

/// The first step where two runs differ.
pub const BLUE: u32 = 0x7fb2ff;
pub const BLUE_SOFT: u32 = 0xa9cbff;
pub const BLUE_CARD: u32 = 0x131b26;
pub const BLUE_PILL: u32 = 0x16202e;
pub const BLUE_BORDER: u32 = 0x25364d;

/// The failure.
pub const RED: u32 = 0xff7a6b;
pub const RED_SOFT: u32 = 0xff9a8e;
pub const RED_CARD: u32 = 0x1f1513;
pub const RED_PILL: u32 = 0x2a1a18;
pub const RED_BORDER: u32 = 0x4a2723;

/// A run that passed.
pub const GREEN_SOFT: u32 = 0x9fd8a4;
pub const GREEN_PILL: u32 = 0x15221a;
pub const GREEN_BORDER: u32 = 0x24402c;

/// The terminal's 16 colors as the log draws them on the panel: the
/// theme's own red, green, amber, blue and text for theirs, and for black
/// a grey that shows on the dark panel. Then the same eight brighter.
pub const ANSI: [u32; 16] = [
    FAINT, RED, GREEN_SOFT, AMBER, BLUE, 0xd59bf6, 0x7fd6d8, SOFT, MUTED, RED_SOFT, 0xb8e8bb,
    0xffc46b, 0xa3c8ff, 0xe5b8fa, 0xa3e6e8, TEXT,
];

/// Standard error lines in the log: the soft text pulled toward red.
pub const STDERR: u32 = 0xe0b4a8;
/// The log row under the playhead.
pub const ROW_NOW: u32 = 0x1c2026;
/// A row under the pointer.
pub const ROW_HOVER: u32 = 0x1a1d22;
/// Buttons under the pointer.
pub const RAISED_HOVER: u32 = 0x1d2127;

/// Selected text's background: the divergence blue at 40 percent.
pub const SELECTION_A: u32 = 0x4a77b866;

/// Source text by what it is, in the source panel and the file viewer,
/// from the palette above where it has a color to spare. Text of no kind
/// keeps its row's color, SOFT or, on a marked line, TEXT.
pub mod syntax {
    /// Between FAINT and MUTED: comments recede but stay readable on
    /// the marked line's AMBER_CARD.
    pub const COMMENT: u32 = 0x80868f;
    pub const KEYWORD: u32 = super::BLUE;
    pub const STRING: u32 = super::GREEN_SOFT;
    pub const NUMBER: u32 = super::AMBER_PALE;
    /// Lavender, the one color here the palette has no use for, so types
    /// stand apart from keywords' blue and strings' green.
    pub const TYPE: u32 = 0xb69cf0;
    pub const FUNCTION: u32 = super::TEXT;
}

/// A hex dump's bytes by what they are, as hexyl colors them, and the
/// offsets in the gutter's color.
pub mod bytes {
    pub const OFFSET: u32 = super::FAINT;
    pub const NULL: u32 = super::FAINT;
    pub const PRINTABLE: u32 = super::BLUE_SOFT;
    pub const WHITESPACE: u32 = super::GREEN_SOFT;
    pub const CONTROL: u32 = super::STDERR;
    pub const ALL_ONES: u32 = super::AMBER;
    pub const NON_ASCII: u32 = super::AMBER_PALE;
}

/// Phase segments cycle through these greys, darkest first.
pub const PHASE_GREYS: [u32; 5] = [0x2c323d, 0x323946, 0x39414f, 0x414a5a, 0x4a5466];

/// Font families in order of preference. The app uses the first one the
/// system has, so a machine without the bundled fonts still gets a
/// sans-serif and a monospace.
pub const UI_FONTS: &[&str] = &[
    "IBM Plex Sans",
    "Inter",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Cantarell",
];
pub const MONO_FONTS: &[&str] = &[
    "JetBrains Mono",
    "IBM Plex Mono",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];

/// Sizes in pixels, from the design's 1440 by 900 layout.
pub mod size {
    // The header bar.
    pub const HEADER_HEIGHT: f32 = 56.0;
    pub const PAGE_PAD_X: f32 = 24.0;
    pub const HEADER_GAP: f32 = 16.0;
    /// Room right of the window controls, which carry their own padding.
    pub const CONTROLS_PAD_RIGHT: f32 = 6.0;
    pub const BRAND_GAP: f32 = 8.0;
    pub const MARK_ICON: f32 = 20.0;

    // The timeline section and its track.
    pub const TIMELINE_PAD_TOP: f32 = 24.0;
    pub const TIMELINE_PAD_BOTTOM: f32 = 16.0;
    /// Room between the track and the controls, which the playhead's
    /// overhang reaches into.
    pub const TRACK_TO_CONTROLS: f32 = 28.0;
    pub const TRACK_HEIGHT: f32 = 40.0;
    pub const SEGMENT_GAP: f32 = 2.0;
    pub const SEGMENT_LABEL_PAD: f32 = 8.0;
    pub const TICK_WIDTH: f32 = 1.0;
    pub const TICK_HEIGHT: f32 = 5.0;
    /// Ticks hang this far below the track's bottom edge.
    pub const TICK_DROP: f32 = 7.0;
    /// Markers reach this far above and below the track.
    pub const MARKER_OVERHANG: f32 = 6.0;
    pub const PLAYHEAD_OVERHANG: f32 = 10.0;
    pub const PLAYHEAD_WIDTH: f32 = 3.0;
    pub const PLAYHEAD_GLOW_WIDTH: f32 = 9.0;
    pub const DIVERGENCE_WIDTH: f32 = 2.0;
    pub const FAILURE_WIDTH: f32 = 3.0;
    pub const FORK_MARK_WIDTH: f32 = 2.0;
    /// The focus ring sits this far outside the track.
    pub const FOCUS_RING_OUTSET_X: f32 = 8.0;
    pub const FOCUS_RING_OUTSET_Y: f32 = 14.0;

    // Buttons and the controls row.
    pub const BUTTON_HEIGHT: f32 = 40.0;
    pub const BUTTON_PAD_X: f32 = 14.0;
    pub const BUTTON_GAP: f32 = 8.0;
    pub const INSPECT_BUTTON_HEIGHT: f32 = 44.0;
    pub const ICON_BUTTON_WIDTH: f32 = 44.0;
    pub const CONTROL_GAP: f32 = 8.0;
    pub const READOUT_GAP: f32 = 24.0;
    pub const READOUT_INNER_GAP: f32 = 6.0;
    pub const ICON_START: f32 = 16.0;
    pub const ICON_CHEVRON: f32 = 14.0;
    pub const ICON_FORK: f32 = 15.0;
    pub const ICON_CLOSE: f32 = 12.0;
    pub const PILL_PAD_X: f32 = 9.0;
    pub const PILL_PAD_Y: f32 = 3.0;

    // Panels.
    pub const PANEL_PAD_X: f32 = 18.0;
    pub const PANEL_TITLE_PAD_Y: f32 = 12.0;
    pub const LIST_PAD_Y: f32 = 10.0;
    pub const LOG_ROW_HEIGHT: f32 = 22.0;
    pub const LIST_ROW_HEIGHT: f32 = 24.0;
    pub const LOG_COLUMN_GAP: f32 = 14.0;
    pub const LIST_COLUMN_GAP: f32 = 10.0;
    pub const PID_COLUMN_WIDTH: f32 = 44.0;
    pub const OP_COLUMN_WIDTH: f32 = 28.0;
    pub const TREE_INDENT: f32 = 16.0;
    /// The width of one character of the monospace font at TEXT_MONO,
    /// for sizing the step column to the run's longest step number.
    pub const MONO_CHAR_WIDTH: f32 = 7.6;
    pub const CARD_PAD: f32 = 16.0;
    pub const CARD_GAP: f32 = 8.0;
    pub const SECTION_GAP: f32 = 16.0;

    // Text selection and its menu.
    /// The sliver a selection shows past a line's end for the line break.
    pub const SELECTION_LINE_END: f32 = 4.0;
    pub const MENU_WIDTH: f32 = 160.0;
    /// The least room between the menu and a window edge.
    pub const MENU_EDGE_MARGIN: f32 = 8.0;
    pub const MENU_PAD: f32 = 4.0;
    pub const MENU_ITEM_PAD_X: f32 = 10.0;
    pub const MENU_ITEM_PAD_Y: f32 = 6.0;
    pub const RADIUS_MENU_ITEM: f32 = 6.0;

    // The terminal pane.
    pub const TERMINAL_ROW_HEIGHT: f32 = 17.0;

    // Notices.
    pub const NOTICE_WIDTH: f32 = 400.0;
    pub const NOTICE_INSET: f32 = 16.0;
    pub const NOTICE_PAD: f32 = 12.0;
    pub const NOTICE_BUTTON_HEIGHT: f32 = 32.0;

    // The empty state.
    pub const EMPTY_MARK_ICON: f32 = 44.0;
    pub const EMPTY_GAP: f32 = 14.0;
    pub const TEXT_EMPTY_TITLE: f32 = 26.0;
    pub const RECENT_WIDTH: f32 = 640.0;
    /// The most the list of builds grows before it scrolls: eight rows.
    pub const RECENT_MAX_HEIGHT: f32 = 400.0;
    pub const RECENT_ROW_PAD_X: f32 = 12.0;
    pub const RECENT_ROW_PAD_Y: f32 = 7.0;
    /// Wide enough for the longest ending, killed:SIGSEGV, so the ids
    /// after it line up.
    pub const RECENT_ENDING_WIDTH: f32 = 128.0;

    // Hover notes.
    pub const TOOLTIP_WIDTH: f32 = 320.0;
    pub const TOOLTIP_PAD_X: f32 = 10.0;
    pub const TOOLTIP_PAD_Y: f32 = 7.0;

    /// The grip along an edge between panels, which drags it.
    pub const EDGE_GRIP: f32 = 6.0;

    // The Runs panel.
    pub const RUNS_ROW_HEIGHT: f32 = 30.0;
    /// The family graph: the width of a lane, the radius of a run's dot,
    /// the ring around the run on screen, the radius of the curve off a
    /// parent's line, and how thick the lines are.
    pub const GRAPH_LANE: f32 = 16.0;
    pub const GRAPH_DOT: f32 = 4.0;
    pub const GRAPH_RING: f32 = 7.0;
    pub const GRAPH_CURVE: f32 = 6.0;
    pub const GRAPH_STROKE: f32 = 1.5;
    /// Half the height of a folding row's chevron.
    pub const GRAPH_CHEVRON: f32 = 5.0;
    pub const LEGEND_GAP: f32 = 4.0;

    // Text.
    pub const TEXT_UI: f32 = 14.0;
    pub const TEXT_SMALL: f32 = 12.0;
    pub const TEXT_CARD_TITLE: f32 = 13.0;
    pub const TEXT_READOUT: f32 = 13.0;
    pub const TEXT_SUBJECT: f32 = 13.0;
    pub const TEXT_MONO: f32 = 12.5;
    pub const TEXT_EVENT: f32 = 14.0;
    pub const TEXT_BRAND: f32 = 18.0;

    // Corners.
    pub const RADIUS_BUTTON: f32 = 8.0;
    pub const RADIUS_CARD: f32 = 10.0;
    pub const RADIUS_SEGMENT: f32 = 3.0;
    pub const RADIUS_PLAYHEAD: f32 = 2.0;
    pub const RADIUS_FOCUS: f32 = 6.0;
}

/// Layout proportions.
pub mod layout {
    /// A flex item that grows to fill free space.
    pub const FILL: f32 = 1.0;
    /// The build log's share of the panel row against 1 for each of the
    /// other two panels.
    pub const LOG_FLEX: f32 = 1.5;
    pub const SIDE_FLEX: f32 = 1.0;
    /// A phase needs at least this share of the run to carry its label.
    pub const LABEL_MIN_SHARE: f32 = 0.05;
    /// About this many tick marks along the timeline.
    pub const TICK_TARGET: u64 = 12;
    /// The terminal pane's share of the window's height at first: room
    /// for a backtrace, leaving the panels most of the window.
    pub const TERMINAL_SHARE: f32 = 0.3;
    /// The source panel's frame list's height at first: four frames, so
    /// the file above keeps the rest of the panel however deep the stack.
    pub const FRAMES_HEIGHT: f32 = 4.0 * super::size::LOG_ROW_HEIGHT;
    /// A disabled button is drawn at this opacity.
    pub const DISABLED_OPACITY: f32 = 0.4;
}

/// What moves while the engine works on something the user started.
pub mod motion {
    use std::time::Duration;

    /// One turn of a spinner.
    pub const SPIN: Duration = Duration::from_millis(1000);
    /// One fade out and back of something that pulses.
    pub const PULSE: Duration = Duration::from_millis(1600);
    /// The faintest a pulse goes.
    pub const PULSE_MIN_OPACITY: f32 = 0.35;
}
