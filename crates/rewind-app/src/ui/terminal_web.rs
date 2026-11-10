//! The terminal pane in the browser build: there is no pty to run a shell
//! or gdb in, so the pane never opens. The scrubber's calls into the pane
//! stay, and do nothing; opening one says the desktop app does it.

use gpui::{Context, Div, FocusHandle, Window};

use crate::engine::CommandLine;
use crate::selection::Lines;
use crate::ui::scrubber::{NoticeTone, Scrubber};

/// What the pane runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    Shell,
    Gdb,
}

impl PaneKind {
    fn name(self) -> &'static str {
        match self {
            PaneKind::Shell => "A shell",
            PaneKind::Gdb => "gdb",
        }
    }
}

/// A terminal session, which the browser build never has.
pub enum Session {}

impl Lines for Session {
    fn line_count(&self) -> usize {
        match *self {}
    }

    fn shown(&self, _line: usize) -> Option<String> {
        match *self {}
    }
}

/// The open pane, which the browser build never has.
pub struct TerminalPane {
    pub focus: FocusHandle,
    never: Session,
}

impl Scrubber {
    pub(super) fn terminal_session(&self) -> Option<&Session> {
        self.terminal.as_ref().map(|t| &t.never)
    }

    /// Says the pane needs the desktop app, in place of opening one.
    pub(super) fn open_terminal(
        &mut self,
        kind: PaneKind,
        _step: u64,
        _pid: Option<u32>,
        _command: CommandLine,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notify_user(
            NoticeTone::Error,
            format!("{} needs the desktop app", kind.name()),
            "The browser viewer reads a recording; running the machine at a step needs the Rewind app and KVM.".to_string(),
            cx,
        );
    }

    pub(super) fn close_terminal(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    pub(super) fn scroll_terminal_toward(&self, _up: bool) {}

    pub(super) fn render_terminal(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<Div>> {
        None
    }
}
