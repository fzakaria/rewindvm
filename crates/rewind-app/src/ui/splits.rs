//! The edges between the scrubber's panels, which drag to resize them:
//! the build log, the processes and files, "At this step" and the Runs
//! panel side by side, the process tree over the files, and the terminal
//! pane under all of them. A double click on an edge puts it back.
//!
//! Each edge is a thin grip on the left or top of the panel after it, drawn
//! after the panel before it so it is on top of both. The panels keep their
//! layout; the edges only change their shares.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use gpui::{
    Bounds, Context, CursorStyle, Div, MouseButton, MouseDownEvent, Pixels, Point, Stateful,
    canvas, div, prelude::*, px,
};
use serde::{Deserialize, Serialize};

use crate::theme::{layout, size};
use crate::ui::scrubber::Scrubber;

/// How the panels share the window: the three columns' flex weights, the
/// process tree's and the files' weights in their column, and the
/// terminal pane's share of the height.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Splits {
    pub log: f32,
    pub middle: f32,
    pub right: f32,
    pub procs: f32,
    pub files: f32,
    pub terminal: f32,
}

impl Default for Splits {
    fn default() -> Splits {
        Splits {
            log: layout::LOG_FLEX,
            middle: layout::SIDE_FLEX,
            right: layout::SIDE_FLEX,
            procs: layout::FILL,
            files: layout::FILL,
            terminal: layout::TERMINAL_SHARE,
        }
    }
}

/// The file the panels' shares are kept in, in the app's settings.
const LAYOUT_FILE: &str = "layout.json";

/// Where the panels' shares are kept between launches.
pub fn layout_path() -> Option<std::path::PathBuf> {
    Some(crate::tour::config_dir()?.join(LAYOUT_FILE))
}

impl Splits {
    /// The shares saved at `path`, each brought inside its bounds; the
    /// default ones when there is no file or it does not read.
    pub fn load(path: &Path) -> Splits {
        let saved: Option<Splits> = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(saved) = saved else {
            return Splits::default();
        };
        let start = Splits::default();
        let weight = |w: f32, start: f32| {
            if w.is_finite() && w > 0.0 { w } else { start }
        };
        Splits {
            log: weight(saved.log, start.log),
            middle: weight(saved.middle, start.middle),
            right: weight(saved.right, start.right),
            procs: weight(saved.procs, start.procs),
            files: weight(saved.files, start.files),
            terminal: if saved.terminal.is_finite() {
                saved.terminal.clamp(TERMINAL_MIN, TERMINAL_MAX)
            } else {
                start.terminal
            },
        }
    }

    /// Saves the shares to `path`, making its directory.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }
}

/// The edges that drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// Between the build log and the processes and files.
    LogMiddle,
    /// Between the processes and files and "At this step".
    MiddleRight,
    /// The Runs panel's left edge.
    Runs,
    /// Between the process tree and the files.
    ProcsFiles,
    /// The terminal pane's top edge.
    Terminal,
}

impl Edge {
    fn cursor(self) -> CursorStyle {
        match self {
            Edge::ProcsFiles | Edge::Terminal => CursorStyle::ResizeUpDown,
            _ => CursorStyle::ResizeLeftRight,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Edge::LogMiddle => "edge-log-middle",
            Edge::MiddleRight => "edge-middle-right",
            Edge::Runs => "edge-runs",
            Edge::ProcsFiles => "edge-procs-files",
            Edge::Terminal => "edge-terminal",
        }
    }
}

/// An edge being dragged: which, where the pointer was pressed, and the
/// sizes then.
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub edge: Edge,
    pub from: Point<Pixels>,
    pub splits: Splits,
    pub runs_width: f32,
}

/// Where the areas the edges divide were last painted, which turns a
/// pointer's move in pixels into a change of share.
#[derive(Default)]
pub struct Measured {
    /// The row of panels.
    pub panels: Cell<Option<Bounds<Pixels>>>,
    /// The processes and files column.
    pub middle: Cell<Option<Bounds<Pixels>>>,
    /// The whole run view, header to terminal.
    pub session: Cell<Option<Bounds<Pixels>>>,
}

/// The narrowest a panel drags to, and the terminal's least and greatest
/// share of the height.
const MIN_PANEL: f32 = 140.0;
const MIN_LIST: f32 = 60.0;
const TERMINAL_MIN: f32 = 0.15;
const TERMINAL_MAX: f32 = 0.8;

/// Two neighbours' flex weights after the edge between them moves
/// `moved` pixels toward the second, in an area `width` pixels across
/// whose weights add up to `total`. Neither gets narrower than `min`
/// pixels, and the two weights keep their sum.
pub fn shift(a: f32, b: f32, total: f32, width: f32, moved: f32, min: f32) -> (f32, f32) {
    if total <= 0.0 || width <= 0.0 {
        return (a, b);
    }
    let a_px = a / total * width;
    let b_px = b / total * width;

    // Too narrow to move either way: leave them. This comes first, since
    // the clamp below needs its lower bound under its upper one.
    if a_px + b_px < 2.0 * min {
        return (a, b);
    }
    let moved = moved.clamp(min - a_px, b_px - min);
    let to_weight = |px: f32| px / width * total;
    (to_weight(a_px + moved), to_weight(b_px - moved))
}

/// A grip along an edge of the panel it is put in: its left edge, or its
/// top edge for the edges that drag up and down. The panel must be
/// `relative`.
pub fn grip(edge: Edge, cx: &mut Context<Scrubber>) -> Stateful<Div> {
    let half = px(size::RUNS_GRIP / 2.0);
    let along = div().id(edge.id()).absolute().cursor(edge.cursor());
    let along = match edge {
        Edge::ProcsFiles | Edge::Terminal => {
            along.left_0().right_0().top(-half).h(px(size::RUNS_GRIP))
        }
        _ => along.top_0().bottom_0().left(-half).w(px(size::RUNS_GRIP)),
    };
    along.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if e.click_count >= 2 {
                this.reset_edge(edge, cx);
                return;
            }
            this.split_drag = Some(Drag {
                edge,
                from: e.position,
                splits: this.splits,
                runs_width: this.runs_width,
            });
        }),
    )
}

/// An invisible element that records where its parent was painted, for
/// `slot`. The parent must be `relative`.
pub fn measure(
    slot: fn(&Measured) -> &Cell<Option<Bounds<Pixels>>>,
    measured: &Rc<Measured>,
) -> impl IntoElement {
    let measured = measured.clone();
    canvas(
        move |bounds, _, _| slot(&measured).set(Some(bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

impl Scrubber {
    /// Moves the edge being dragged to the pointer at `at`.
    pub(super) fn drag_edge(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.split_drag else {
            return;
        };
        let start = drag.splits;
        let dx = f32::from(at.x - drag.from.x);
        let dy = f32::from(at.y - drag.from.y);

        // The three flex columns' width: the row less the Runs panel.
        let columns_width = self.measured.panels.get().map_or(0.0, |b| {
            let runs = if self.runs_open && self.family.is_some() {
                self.runs_width
            } else {
                0.0
            };
            f32::from(b.size.width) - runs
        });
        let columns_total = start.log + start.middle + start.right;

        match drag.edge {
            Edge::LogMiddle => {
                (self.splits.log, self.splits.middle) = shift(
                    start.log,
                    start.middle,
                    columns_total,
                    columns_width,
                    dx,
                    MIN_PANEL,
                );
            }
            Edge::MiddleRight => {
                (self.splits.middle, self.splits.right) = shift(
                    start.middle,
                    start.right,
                    columns_total,
                    columns_width,
                    dx,
                    MIN_PANEL,
                );
            }
            Edge::Runs => {
                self.runs_width =
                    (drag.runs_width - dx).clamp(size::RUNS_PANEL_MIN, size::RUNS_PANEL_MAX);
            }
            Edge::ProcsFiles => {
                let height = self
                    .measured
                    .middle
                    .get()
                    .map_or(0.0, |b| f32::from(b.size.height));
                (self.splits.procs, self.splits.files) = shift(
                    start.procs,
                    start.files,
                    start.procs + start.files,
                    height,
                    dy,
                    MIN_LIST,
                );
            }
            Edge::Terminal => {
                if let Some(session) = self.measured.session.get() {
                    let height = f32::from(session.size.height);
                    let share = f32::from(session.bottom() - at.y) / height;
                    self.splits.terminal = share.clamp(TERMINAL_MIN, TERMINAL_MAX);
                }
            }
        }
        cx.notify();
    }

    /// Ends dragging an edge, if one was dragged, and keeps the shares it
    /// left for the next launch.
    pub(super) fn end_edge_drag(&mut self) {
        if self.split_drag.take().is_some() {
            self.keep_splits();
        }
    }

    /// Saves the panels' shares. A failure to write only means the next
    /// launch starts from the default ones.
    fn keep_splits(&self) {
        if let Some(path) = layout_path() {
            let _ = self.splits.save(&path);
        }
    }

    /// Puts an edge back where it starts.
    fn reset_edge(&mut self, edge: Edge, cx: &mut Context<Self>) {
        let start = Splits::default();
        match edge {
            Edge::LogMiddle | Edge::MiddleRight => {
                (self.splits.log, self.splits.middle, self.splits.right) =
                    (start.log, start.middle, start.right);
            }
            Edge::Runs => self.runs_width = size::RUNS_PANEL_WIDTH,
            Edge::ProcsFiles => {
                (self.splits.procs, self.splits.files) = (start.procs, start.files);
            }
            Edge::Terminal => self.splits.terminal = start.terminal,
        }
        self.keep_splits();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // Edges moved on plain numbers: the weights follow the pointer, keep
    // their sum, and stop at the narrowest a panel may be.
    use super::*;

    #[test]
    fn an_edge_moves_by_the_pointer_and_keeps_the_sum() {
        // Weights 1.5, 1.0 and 1.0 over 700 pixels: 300, 200 and 200.
        let (a, b) = shift(1.5, 1.0, 3.5, 700.0, 50.0, 100.0);
        assert!((a / 3.5 * 700.0 - 350.0).abs() < 0.01);
        assert!((b / 3.5 * 700.0 - 150.0).abs() < 0.01);
        assert!((a + b - 2.5).abs() < 0.0001);
    }

    #[test]
    fn an_edge_stops_where_a_panel_is_narrowest() {
        let (a, b) = shift(1.5, 1.0, 3.5, 700.0, 500.0, 100.0);
        assert!((b / 3.5 * 700.0 - 100.0).abs() < 0.01);
        assert!((a / 3.5 * 700.0 - 400.0).abs() < 0.01);
        let (a, _) = shift(1.5, 1.0, 3.5, 700.0, -500.0, 100.0);
        assert!((a / 3.5 * 700.0 - 100.0).abs() < 0.01);
    }

    #[test]
    fn the_shares_are_kept_between_launches() {
        // Shares saved to a file read back the same; a file that is not
        // shares reads as the default ones, and shares out of bounds are
        // brought back inside them.
        let dir = std::env::temp_dir().join(format!("rewind-app-splits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("layout.json");
        let moved = Splits {
            log: 2.0,
            terminal: 0.5,
            ..Splits::default()
        };
        moved.save(&path).unwrap();
        assert_eq!(Splits::load(&path), moved);

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Splits::load(&path), Splits::default());

        let wild = Splits {
            log: -3.0,
            terminal: 7.0,
            ..Splits::default()
        };
        wild.save(&path).unwrap();
        let read = Splits::load(&path);
        assert_eq!(read.log, Splits::default().log);
        assert_eq!(read.terminal, TERMINAL_MAX);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_moves_without_room() {
        assert_eq!(shift(1.0, 1.0, 2.0, 0.0, 30.0, 100.0), (1.0, 1.0));
        assert_eq!(shift(1.0, 1.0, 2.0, 150.0, 30.0, 100.0), (1.0, 1.0));
    }
}
