//! The terminal pane: a bottom panel that runs a command in a host pty and
//! draws its screen, for the shell and gdb at a step.
//!
//! `crate::terminal` holds the emulator and the key encoding; this module
//! starts the pane, feeds it keys, pastes and scroll, sizes the pty to the
//! panel, and closes it when the command exits.

use std::cell::Cell;
use std::rc::Rc;

use alacritty_terminal::event::Event;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::Rgb;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{
    ClipboardItem, Context, Div, FocusHandle, FontStyle, FontWeight, HighlightStyle, KeyDownEvent,
    Keystroke, MouseButton, Role, ScrollDelta, ScrollWheelEvent, UnderlineStyle, Window, canvas,
    div, font, prelude::*, px, relative, rgb,
};

use crate::describe::thousands;
use crate::engine::CommandLine;
use crate::selection::{Surface, part_of_line};
use crate::terminal::{CursorKeys, GridSize, Key, Mods, Screen, Session, encode_key};
use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;
use crate::ui::scrubber::{NoticeTone, Scrubber};
use crate::ui::selectable::{selectable, selects};
use crate::ui::widgets::{icon, panel_title};
use crate::ui::{TERMINAL_CONTEXT, TerminalCopy, TerminalPaste};

/// What the pane runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    Shell,
    Gdb,
}

impl PaneKind {
    fn name(self) -> &'static str {
        match self {
            PaneKind::Shell => "Shell",
            PaneKind::Gdb => "gdb",
        }
    }
}

/// How the command in the pane ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Code(i32),
    Signal(i32),
}

/// The open pane.
pub struct TerminalPane {
    pub kind: PaneKind,
    /// The step the command forked the run at.
    pub step: u64,
    /// The process the shell started in, when one was asked for.
    pub pid: Option<u32>,
    /// The title the command set with an escape sequence, if any.
    pub title: Option<String>,
    pub session: Session,
    /// How the command ended, once it has; the pane then stays to show
    /// what it printed.
    pub ended: Option<Ended>,
    pub focus: FocusHandle,
    /// The grid size the pane's last layout fits, shared with the canvas
    /// that measures it.
    measured: Rc<Cell<Option<GridSize>>>,
}

/// Pixels of padding around the grid.
const GRID_PAD: f32 = 8.0;

/// Lines a wheel notch scrolls.
const WHEEL_LINES: f32 = 3.0;

/// How a key in the pane maps to what the terminal sends.
fn key_of(keystroke: &Keystroke) -> Option<Key> {
    const FUNCTION_KEYS: u8 = 12;
    let named = match keystroke.key.as_str() {
        "enter" => Some(Key::Enter),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "escape" => Some(Key::Escape),
        "up" => Some(Key::Up),
        "down" => Some(Key::Down),
        "left" => Some(Key::Left),
        "right" => Some(Key::Right),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "insert" => Some(Key::Insert),
        "delete" => Some(Key::Delete),
        "space" => Some(Key::Char(' ')),
        other => other
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| (1..=FUNCTION_KEYS).contains(n))
            .map(Key::F),
    };
    if named.is_some() {
        return named;
    }

    // A character: what the key typed, or the key itself under Control or
    // Alt, which type nothing.
    let typed = keystroke
        .key_char
        .as_deref()
        .filter(|_| !keystroke.modifiers.control && !keystroke.modifiers.alt)
        .unwrap_or(keystroke.key.as_str());
    let mut chars = typed.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(Key::Char(c))
}

fn rgb_of(c: Rgb) -> gpui::Hsla {
    rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32).into()
}

impl Scrubber {
    pub(super) fn terminal_session(&self) -> Option<&Session> {
        self.terminal.as_ref().map(|t| &t.session)
    }

    /// Opens the pane running `command`, in place of any pane open.
    pub(super) fn open_terminal(
        &mut self,
        kind: PaneKind,
        step: u64,
        pid: Option<u32>,
        command: CommandLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The first size is a guess; the pane's first layout sets the real
        // one before the command draws much.
        const FIRST: GridSize = GridSize {
            columns: 120,
            lines: 16,
            cell_width: 8,
            cell_height: 18,
        };
        self.close_terminal(cx);
        let (session, events) = match Session::spawn(&command, FIRST) {
            Ok(started) => started,
            Err(e) => {
                self.notify_user(
                    NoticeTone::Error,
                    format!("Could not start {}", kind.name()),
                    format!("{}: {e}", command.display()),
                    cx,
                );
                return;
            }
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.terminal = Some(TerminalPane {
            kind,
            step,
            pid,
            title: None,
            session,
            ended: None,
            focus,
            measured: Rc::new(Cell::new(None)),
        });
        self.clear_selection_in(&[Surface::Terminal]);
        self.listen_to_terminal(events, cx);
        cx.notify();
    }

    /// Reads the terminal's events on the UI thread for as long as the
    /// pane's session lives.
    fn listen_to_terminal(&mut self, mut events: UnboundedReceiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this.update(cx, |this, cx| this.terminal_event(event, cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn terminal_event(&mut self, event: Event, cx: &mut Context<Self>) {
        let Some(pane) = &mut self.terminal else {
            return;
        };
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {}
            Event::Title(title) => pane.title = Some(title),
            Event::ResetTitle => pane.title = None,
            Event::PtyWrite(text) => pane.session.write(text.into_bytes()),
            Event::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            Event::ClipboardLoad(_, format) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                pane.session.write(format(&text).into_bytes());
            }
            Event::ColorRequest(index, format) => {
                let color = pane.session.color(index);
                pane.session.write(format(color).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = pane.session.size();
                let window = alacritty_terminal::event::WindowSize {
                    num_lines: size.lines as u16,
                    num_cols: size.columns as u16,
                    cell_width: size.cell_width,
                    cell_height: size.cell_height,
                };
                pane.session.write(format(window).into_bytes());
            }
            Event::Bell => {}
            Event::ChildExit(status) => {
                use std::os::unix::process::ExitStatusExt;
                pane.ended = Some(match (status.code(), status.signal()) {
                    (Some(code), _) => Ended::Code(code),
                    (None, Some(signal)) => Ended::Signal(signal),
                    (None, None) => Ended::Code(0),
                });
                if pane.ended == Some(Ended::Code(0)) {
                    let (kind, step) = (pane.kind, pane.step);
                    self.close_terminal(cx);
                    self.notify_user(
                        NoticeTone::Info,
                        format!("{} at step {} ended", kind.name(), thousands(step)),
                        "It exited normally. The fork it ran in is gone; the run is unchanged.",
                        cx,
                    );
                    return;
                }
            }
            Event::Exit => {}
        }
        cx.notify();
    }

    /// Closes the pane and hangs up on its command if it still runs.
    pub(super) fn close_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminal.take().is_some() {
            self.clear_selection_in(&[Surface::Terminal]);
            cx.notify();
        }
    }

    /// A key typed in the pane, sent to the command as xterm encodes it.
    /// Shift with Page Up and Page Down scrolls the scrollback instead.
    pub(super) fn terminal_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(pane) = &self.terminal else {
            return;
        };
        if pane.ended.is_some() {
            return;
        }
        let m = &event.keystroke.modifiers;
        let mods = Mods {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.control,
        };
        let Some(key) = key_of(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        let page = pane.session.size().lines as i32;
        match (&key, mods.shift && !mods.ctrl && !mods.alt) {
            (Key::PageUp, true) => {
                pane.session.scroll(page);
                cx.notify();
                return;
            }
            (Key::PageDown, true) => {
                pane.session.scroll(-page);
                cx.notify();
                return;
            }
            _ => {}
        }
        let cursor_keys = if pane.session.mode().contains(TermMode::APP_CURSOR) {
            CursorKeys::Application
        } else {
            CursorKeys::Normal
        };
        // Character keys carry their shift already.
        let mods = match key {
            Key::Char(_) => Mods {
                shift: false,
                ..mods
            },
            _ => mods,
        };
        if let Some(bytes) = encode_key(&key, mods, cursor_keys) {
            pane.session.scroll_to_bottom();
            pane.session.write(bytes);
        }
        self.clear_selection_in(&[Surface::Terminal]);
    }

    /// Ctrl+Shift+C in the pane, and the menu's Copy.
    pub(super) fn terminal_copy(&mut self, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    /// Ctrl+Shift+V in the pane: the clipboard's text, pasted.
    pub(super) fn terminal_paste(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = &self.terminal else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if pane.ended.is_none() {
            pane.session.scroll_to_bottom();
            pane.session.paste(&text);
        }
    }

    /// The wheel: the scrollback, or the arrow keys for a full-screen
    /// program that asked for them.
    fn terminal_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = &self.terminal else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y * WHEEL_LINES,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / size::TERMINAL_ROW_HEIGHT,
        };
        let lines = lines.round() as i32;
        if lines == 0 {
            return;
        }
        let mode = pane.session.mode();
        let _ = window;
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 { Key::Up } else { Key::Down };
            let cursor_keys = if mode.contains(TermMode::APP_CURSOR) {
                CursorKeys::Application
            } else {
                CursorKeys::Normal
            };
            if let Some(bytes) = encode_key(&key, Mods::default(), cursor_keys) {
                for _ in 0..lines.unsigned_abs() {
                    pane.session.write(bytes.clone());
                }
            }
        } else {
            pane.session.scroll(lines);
        }
        cx.notify();
    }

    /// Scrolls the pane one line while a selection drag is above or below
    /// it.
    pub(super) fn scroll_terminal_toward(&self, up: bool) {
        if let Some(pane) = &self.terminal {
            pane.session.scroll(if up { 1 } else { -1 });
        }
    }

    /// The pane under the panels.
    pub(super) fn render_terminal(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<Div>> {
        let pane = self.terminal.as_ref()?;
        let screen: Screen = pane.session.screen();
        let mono = self.fonts.mono.clone();

        // The title bar: what runs, its state, and the close button.
        let state = match pane.ended {
            None => "running".to_string(),
            Some(Ended::Code(code)) => format!("exited:{code}"),
            Some(Ended::Signal(signal)) => format!("killed by signal {signal}"),
        };
        let scrolled = if screen.scrolled_back > 0 {
            format!(
                " \u{b7} {} lines up",
                thousands(screen.scrolled_back as u64)
            )
        } else {
            String::new()
        };
        let process = pane
            .pid
            .map_or_else(String::new, |pid| format!(" \u{b7} pid {pid}"));
        let title = format!(
            "{} \u{b7} step {}{process} \u{b7} {state}{scrolled}",
            pane.title.as_deref().unwrap_or(pane.kind.name()),
            thousands(pane.step)
        );
        let close = div()
            .id("terminal-close")
            .role(Role::Button)
            .aria_label("Close the terminal")
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon(Icon::Close, size::ICON_CLOSE, theme::MUTED))
            .on_click(cx.listener(|this, _, _, cx| this.close_terminal(cx)));
        let bar = panel_title(&title, Some(close.into_any_element()));

        // The grid: one selectable line per row, styled per span, the
        // cursor drawn in reverse.
        let range = self.selected_range(Surface::Terminal);
        let registry = self.selecting.registry.clone();
        let rows = screen.rows.iter().enumerate().map(|(i, row)| {
            let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = row
                .spans
                .iter()
                .map(|span| {
                    let style = &span.style;
                    (
                        span.range.clone(),
                        HighlightStyle {
                            color: Some(rgb_of(style.fg)),
                            background_color: style.bg.map(rgb_of),
                            font_weight: style.bold.then_some(FontWeight::BOLD),
                            font_style: style.italic.then_some(FontStyle::Italic),
                            underline: style.underline.then(|| UnderlineStyle {
                                thickness: px(1.0),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            if let Some((cursor_row, cursor)) = &screen.cursor
                && *cursor_row == i
                && pane.ended.is_none()
                && !cursor.is_empty()
            {
                highlights = overlay(
                    highlights,
                    cursor.clone(),
                    HighlightStyle {
                        color: Some(rgb(theme::PANEL).into()),
                        background_color: Some(rgb(theme::TEXT).into()),
                        ..Default::default()
                    },
                );
            }
            let selected = range
                .as_ref()
                .and_then(|r| part_of_line(r, row.line, row.text.len()));
            div()
                .h(px(size::TERMINAL_ROW_HEIGHT))
                .flex()
                .items_center()
                .whitespace_nowrap()
                .overflow_hidden()
                .child(
                    selectable(
                        Surface::Terminal,
                        row.line,
                        row.text.clone(),
                        selected,
                        &registry,
                    )
                    .with_highlights(highlights),
                )
        });

        // A canvas over the grid measures it and sizes the pty to fit.
        let measured = pane.measured.clone();
        let view = cx.entity();
        let font_size = px(size::TEXT_MONO);
        let mono_font = font(mono.clone());
        let measure = canvas(
            move |bounds, window, cx| {
                let font_id = window.text_system().resolve_font(&mono_font);
                let Ok(advance) = window.text_system().advance(font_id, font_size, 'm') else {
                    return;
                };
                let cell_width = f32::from(advance.width).max(1.0);
                let width = f32::from(bounds.size.width) - 2.0 * GRID_PAD;
                let height = f32::from(bounds.size.height) - 2.0 * GRID_PAD;
                let fits = GridSize {
                    columns: ((width / cell_width).floor() as usize).max(GridSize::MIN_COLUMNS),
                    lines: ((height / size::TERMINAL_ROW_HEIGHT).floor() as usize)
                        .max(GridSize::MIN_LINES),
                    cell_width: cell_width.round() as u16,
                    cell_height: size::TERMINAL_ROW_HEIGHT as u16,
                };
                if measured.get() == Some(fits) {
                    return;
                }
                measured.set(Some(fits));
                view.update(cx, |this, _| {
                    if let Some(pane) = &mut this.terminal {
                        pane.session.resize(fits);
                    }
                });
                window.refresh();
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let grid = div()
            .id("terminal-grid")
            .relative()
            .flex()
            .flex_col()
            .flex_grow(layout::FILL)
            .min_h_0()
            .overflow_hidden()
            .p(px(GRID_PAD))
            .font_family(mono)
            .text_size(px(size::TEXT_MONO))
            .text_color(rgb(theme::TEXT))
            .cursor(gpui::CursorStyle::IBeam)
            .child(measure)
            .children(rows)
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, window, cx| {
                this.terminal_wheel(e, window, cx)
            }));
        let grid = selects(grid, Surface::Terminal, cx);

        let focused = pane.focus.is_focused(window);
        Some(
            div()
                .id("terminal")
                .key_context(TERMINAL_CONTEXT)
                .track_focus(&pane.focus)
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| this.terminal_key(e, cx)))
                .on_action(cx.listener(|this, _: &TerminalCopy, _, cx| this.terminal_copy(cx)))
                .on_action(cx.listener(|this, _: &TerminalPaste, _, cx| this.terminal_paste(cx)))
                .flex()
                .flex_col()
                .flex_none()
                .h(relative(layout::TERMINAL_SHARE))
                .min_h_0()
                .bg(rgb(theme::PANEL))
                .border_t_1()
                .border_color(rgb(if focused {
                    theme::AMBER_DEEP
                } else {
                    theme::LINE
                }))
                .child(bar)
                .child(grid),
        )
    }
}

/// `highlights` with `style` laid over `range`: spans that overlap it are
/// cut around it.
fn overlay(
    highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    range: std::ops::Range<usize>,
    style: HighlightStyle,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let mut out = Vec::with_capacity(highlights.len() + 2);
    for (span, span_style) in highlights {
        if span.end <= range.start || span.start >= range.end {
            out.push((span, span_style));
            continue;
        }
        if span.start < range.start {
            out.push((span.start..range.start, span_style));
        }
        if span.end > range.end {
            out.push((range.end..span.end, span_style));
        }
    }
    out.push((range, style));
    out.sort_by_key(|(r, _)| r.start);
    out
}

#[cfg(test)]
mod tests {
    // The cursor's overlay on a row's spans, checked on plain ranges.
    use super::*;

    #[test]
    fn the_cursor_cuts_the_span_under_it() {
        let plain = HighlightStyle::default();
        let cursor = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let out = overlay(vec![(0..10, plain)], 4..5, cursor);
        let ranges: Vec<_> = out.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..4, 4..5, 5..10]);
        assert_eq!(out[1].1, cursor);
    }
}
