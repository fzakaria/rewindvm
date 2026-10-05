//! Selectable text on screen: the element that draws a line of a surface
//! with its selected part highlighted, the registry of where each drawn
//! line landed, and the scrubber's handling of presses, drags, the
//! context menu, Copy and Select All.
//!
//! The selection itself, and how it reads and copies text, is
//! `crate::selection`. This module turns pointer positions into positions
//! in a surface's lines with the text layouts GPUI made for the frame.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClipboardItem, Context, DispatchPhase, Element, ElementId, GlobalElementId,
    HighlightStyle, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollStrategy, SharedString, StyledText,
    TextLayout, Window, anchored, canvas, div, fill, point, prelude::*, px, rgb, rgba,
};

use crate::describe::short_store_paths_mapped;
use crate::family::{Folds, RowKind};
use crate::selection::{DrawnText, Lines, Mapped, Pos, Selection, Surface, Unit, nearest_line};
use crate::theme::{self, size};
use crate::ui::scrubber::Scrubber;
use crate::viewer::{TabbedLines, expand_tabs};

/// What GPUI draws in place of the part of a line cut to fit.
const ELLIPSIS: &str = "\u{2026}";

/// Characters the process id takes in a process tree row, with its gap.
pub const PID_CHARS: usize = 7;

/// One drawn line: where it landed this frame and the layout GPUI made.
#[derive(Clone)]
struct Drawn {
    surface: Surface,
    line: usize,
    /// The line's text, which selection offsets index.
    text: SharedString,
    layout: TextLayout,
    bounds: Bounds<Pixels>,
}

/// Every selectable line drawn in the last frame. The scrubber clears it
/// before each render and the lines add themselves as they are placed.
#[derive(Clone, Default)]
pub struct Registry(Rc<RefCell<Vec<Drawn>>>);

impl Registry {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    fn push(&self, drawn: Drawn) {
        self.0.borrow_mut().push(drawn);
    }

    /// The position in `surface` under `at`: the nearest drawn line, and
    /// the character boundary nearest the pointer along it.
    pub fn hit(&self, surface: Surface, at: Point<Pixels>) -> Option<Pos> {
        let drawn = self.0.borrow();
        let spans: Vec<(usize, f32, f32)> = drawn
            .iter()
            .filter(|d| d.surface == surface)
            .map(|d| {
                (
                    d.line,
                    f32::from(d.bounds.top()),
                    f32::from(d.bounds.bottom()),
                )
            })
            .collect();
        let line = nearest_line(&spans, f32::from(at.y))?;
        let entry = drawn
            .iter()
            .find(|d| d.surface == surface && d.line == line)?;
        let drawn_index = index_at(&entry.layout, entry.bounds, at);
        let map = DrawnText::new(&entry.text, &entry.layout.text(), ELLIPSIS);
        let offset = crate::selection::floor_boundary(&entry.text, map.text_offset(drawn_index));
        Some(Pos::new(line, offset))
    }

    /// The first and last lines of `surface` drawn on screen.
    pub fn drawn_lines(&self, surface: Surface) -> Option<(usize, usize)> {
        let drawn = self.0.borrow();
        let lines = drawn
            .iter()
            .filter(|d| d.surface == surface)
            .map(|d| d.line);
        let first = lines.clone().min()?;
        Some((first, lines.max()?))
    }

    /// The top and bottom of what `surface` drew.
    fn extent(&self, surface: Surface) -> Option<(Pixels, Pixels)> {
        let drawn = self.0.borrow();
        let mut spans = drawn.iter().filter(|d| d.surface == surface);
        let first = spans.next()?;
        let (mut top, mut bottom) = (first.bounds.top(), first.bounds.bottom());
        for d in spans {
            top = top.min(d.bounds.top());
            bottom = bottom.max(d.bounds.bottom());
        }
        Some((top, bottom))
    }
}

/// The drawn text's byte index nearest `at`, over wrapped lines too.
fn index_at(layout: &TextLayout, bounds: Bounds<Pixels>, at: Point<Pixels>) -> usize {
    let line_height = layout.line_height();
    let lines = layout.line_layouts();
    let mut origin = bounds.origin;
    let mut start = 0;
    for (i, line) in lines.iter().enumerate() {
        let height = line.size(line_height).height;
        let last = i + 1 == lines.len();
        if at.y < origin.y + height || last {
            // Inside the line's own box, so a point beside or below the
            // text takes the nearest place on it.
            let x = (at.x - origin.x).max(px(0.0));
            let y = (at.y - origin.y).clamp(px(0.0), (height - px(1.0)).max(px(0.0)));
            let index = line
                .closest_index_for_position(point(x, y), line_height)
                .unwrap_or_else(|i| i);
            return start + index.min(line.len());
        }
        origin.y += height;
        start += line.len() + 1;
    }
    0
}

/// Rectangles covering bytes `range` of the drawn text, one per visual
/// row: the rest of the first row, whole middle rows, the start of the
/// last.
fn highlight_rects(
    layout: &TextLayout,
    bounds: Bounds<Pixels>,
    range: Range<usize>,
) -> Vec<Bounds<Pixels>> {
    let line_height = layout.line_height();
    let lines = layout.line_layouts();

    // One unwrapped line: two positions bound the whole highlight.
    if let [line] = lines.as_slice()
        && line.wrap_boundaries.is_empty()
    {
        let x0 = line.unwrapped_layout.x_for_index(range.start);
        let x1 = line.unwrapped_layout.x_for_index(range.end);
        let width = (x1 - x0).max(px(0.0));
        return vec![Bounds::new(
            point(bounds.origin.x + x0, bounds.origin.y),
            gpui::size(width, line_height),
        )];
    }

    // Wrapped text: walk the characters and start a new rectangle where
    // the row changes.
    let text = layout.text();
    let mut rects: Vec<Bounds<Pixels>> = Vec::new();
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .filter(|&i| i >= range.start && i <= range.end)
        .collect();
    for pair in boundaries.windows(2) {
        let (Some(from), Some(to)) = (
            layout.position_for_index(pair[0]),
            layout.position_for_index(pair[1]),
        ) else {
            continue;
        };
        // GPUI places the index at a wrap at the end of the row above, so
        // a character whose end lands on a lower row is the first of that
        // row and starts at its left edge.
        let from = if to.y == from.y {
            from
        } else {
            point(bounds.left(), to.y)
        };
        match rects.last_mut() {
            Some(last) if last.origin.y == from.y => {
                last.size.width = (to.x - last.origin.x).max(last.size.width);
            }
            _ => rects.push(Bounds::new(
                from,
                gpui::size((to.x - from.x).max(px(0.0)), line_height),
            )),
        }
    }
    rects
}

/// A line of a surface's text, drawn by GPUI's text layout with the
/// selected part highlighted behind it.
pub struct SelectableText {
    surface: Surface,
    line: usize,
    text: SharedString,
    selected: Option<Range<usize>>,
    registry: Registry,
    styled: StyledText,
}

/// The line `line` of `surface`, showing `text`, with `selected` of it
/// highlighted.
pub fn selectable(
    surface: Surface,
    line: usize,
    text: impl Into<SharedString>,
    selected: Option<Range<usize>>,
    registry: &Registry,
) -> SelectableText {
    let text = text.into();
    SelectableText {
        surface,
        line,
        styled: StyledText::new(text.clone()),
        text,
        selected,
        registry: registry.clone(),
    }
}

impl SelectableText {
    /// Colors and weights for parts of the line.
    pub fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.styled = StyledText::new(self.text.clone()).with_highlights(highlights);
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .prepaint(id, inspector_id, bounds, state, window, cx);
        self.registry.push(Drawn {
            surface: self.surface,
            line: self.line,
            text: self.text.clone(),
            layout: self.styled.layout().clone(),
            bounds,
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // Run backgrounds, then the selection, then the glyphs over both.
        let layout = self.styled.layout().clone();
        let _ = layout.paint_background(window, cx);
        if let Some(selected) = &self.selected {
            let map = DrawnText::new(&self.text, &layout.text(), ELLIPSIS);
            let range = map.drawn_offset(selected.start)..map.drawn_offset(selected.end);
            let mut rects = highlight_rects(&layout, bounds, range.clone());

            // A line selected through its end shows a sliver for the line
            // break, so a selected empty line is visible.
            if selected.end >= self.text.len()
                && let Some(last) = rects.last_mut()
            {
                last.size.width += px(size::SELECTION_LINE_END);
            }
            for rect in rects {
                window.paint_quad(fill(rect, rgba(theme::SELECTION_A)));
            }
        }
        let _ = layout.paint_foreground(window, cx);
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The right-click menu: where it opened and over which surface.
#[derive(Clone, Copy, Debug)]
pub struct ContextMenu {
    pub surface: Surface,
    pub position: Point<Pixels>,
    /// The line under the pointer, for "Copy line" when nothing is
    /// selected.
    pub line: Option<usize>,
}

/// The scrubber's selection state.
#[derive(Default)]
pub struct SelectionState {
    pub selection: Option<Selection>,
    /// Whether the left button is down after a press in a surface.
    pub dragging: bool,
    /// The surface pressed in last, which Select All selects.
    pub active: Option<Surface>,
    pub menu: Option<ContextMenu>,
    pub registry: Registry,
}

/// A line's text for a log line, a file path, a card line: store paths
/// shortened, copied in full.
pub fn mapped(text: &str) -> Mapped {
    short_store_paths_mapped(text)
}

/// A file viewer line: tabs drawn as spaces, copied as tabs.
pub fn viewer_line(line: &str) -> Mapped {
    expand_tabs(line)
}

/// The text of a process tree row: the id, padded to its column, then
/// the label.
pub fn process_row(tid: u32, label: &str) -> Mapped {
    mapped(label).prefixed(&format!("{tid:<PID_CHARS$}"))
}

/// The lines of the build log up to the playhead, read on demand: the log
/// can be long, and a selection reads only the lines it covers.
struct LogLines<'a> {
    lines: &'a [crate::model::LogLine],
}

impl Lines for LogLines<'_> {
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn shown(&self, line: usize) -> Option<String> {
        self.lines.get(line).map(|l| mapped(&l.text).shown)
    }

    fn copied(&self, line: usize, range: Range<usize>) -> Option<String> {
        self.lines.get(line).map(|l| mapped(&l.text).copy(range))
    }
}

impl Scrubber {
    /// What takes the keyboard after a press in `surface`: the terminal
    /// pane and the license dialog keep their own, the rest is the
    /// scrubber's.
    fn focus_for(&self, surface: Surface) -> gpui::FocusHandle {
        match surface {
            Surface::Terminal => self.terminal.as_ref().map(|t| t.focus.clone()),
            Surface::LicenseDialog => self.licensing.dialog.as_ref().map(|d| d.focus.clone()),
            _ => None,
        }
        .unwrap_or_else(|| self.focus.clone())
    }

    /// The lines of `surface` as they are on screen now.
    pub(super) fn surface_lines(&self, surface: Surface) -> Box<dyn Lines + '_> {
        let empty: Box<dyn Lines> = Box::new(Vec::<String>::new());
        match surface {
            Surface::Log => {
                let Some(session) = &self.session else {
                    return empty;
                };
                let t = &session.run.timeline;
                let count = t.line_count_at(self.log_filter, self.step);
                Box::new(LogLines {
                    lines: &t.lines(self.log_filter)[..count],
                })
            }
            Surface::Viewer => match self.viewer_view() {
                Some(view) => Box::new(TabbedLines(&view.lines)),
                None => Box::new(self.viewer_message_lines()),
            },
            Surface::Source => match self.shown_file() {
                Some((file, _)) => Box::new(TabbedLines(&file.lines)),
                None => Box::new(self.source_lines()),
            },
            Surface::Terminal => match self.terminal_session() {
                Some(session) => Box::new(session),
                None => empty,
            },
            other => Box::new(self.card_lines(other)),
        }
    }

    /// The lines of the short surfaces, the lists, cards, notices and the
    /// license dialog, built the same way the render builds them.
    pub(super) fn card_lines(&self, surface: Surface) -> Vec<Mapped> {
        match surface {
            Surface::Processes => self.process_lines(),
            Surface::Files => self.file_lines(),
            Surface::EventCard => self.event_card_lines(),
            Surface::Divergence => self.divergence_lines(),
            Surface::ForkCard => self.fork_card_lines(),
            Surface::Notice(id) => self
                .notices
                .iter()
                .find(|n| n.id == id)
                .map(|n| {
                    std::iter::once(Mapped::plain(n.title.to_string()))
                        .chain(n.body.lines().map(mapped))
                        .collect()
                })
                .unwrap_or_default(),
            Surface::LicenseDialog => self.license_dialog_lines(),
            Surface::Runs => self.runs_lines(),
            Surface::Log | Surface::Viewer | Surface::Source | Surface::Terminal => Vec::new(),
        }
    }

    /// The selected range of `surface`, when the selection is in it.
    pub(super) fn selected_range(&self, surface: Surface) -> Option<Range<Pos>> {
        let selection = self.selecting.selection.filter(|s| s.surface == surface)?;
        let lines = self.surface_lines(surface);
        Some(selection.range(lines.as_ref()))
    }

    /// A press in `surface`: a new selection by characters, words or
    /// lines with the click count, or the current one extended with
    /// Shift.
    pub(super) fn selection_press(
        &mut self,
        surface: Surface,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        self.selecting.menu = None;
        self.selecting.active = Some(surface);
        let Some(at) = self.selecting.registry.hit(surface, event.position) else {
            self.selecting.selection = None;
            cx.notify();
            return;
        };
        let extending = event.modifiers.shift
            && self
                .selecting
                .selection
                .is_some_and(|s| s.surface == surface);
        let selection = if extending {
            let mut selection = self.selecting.selection.expect("checked above");
            selection.extend_to(at);
            selection
        } else {
            let lines = self.surface_lines(surface);
            Selection::press(
                surface,
                at,
                Unit::for_clicks(event.click_count),
                lines.as_ref(),
            )
        };
        self.selecting.selection = Some(selection);
        self.selecting.dragging = true;
        cx.notify();
    }

    /// The pointer moved with the button down after a press: the head
    /// follows it, and a list scrolls when it leaves the list's top or
    /// bottom.
    pub(super) fn selection_drag(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut selection) = self.selecting.selection else {
            return;
        };
        let surface = selection.surface;
        let Some(pos) = self.selecting.registry.hit(surface, at) else {
            return;
        };
        self.scroll_while_selecting(surface, at);
        selection.extend_to(pos);
        if Some(selection) != self.selecting.selection {
            self.selecting.selection = Some(selection);
            cx.notify();
        }
    }

    /// Scrolls a list one line toward the pointer when a drag leaves it,
    /// so a selection can reach lines that were off screen.
    fn scroll_while_selecting(&self, surface: Surface, at: Point<Pixels>) {
        let (Some((top, bottom)), Some((first, last))) = (
            self.selecting.registry.extent(surface),
            self.selecting.registry.drawn_lines(surface),
        ) else {
            return;
        };
        let target = if at.y < top {
            first.checked_sub(1)
        } else if at.y > bottom {
            Some(last + 1)
        } else {
            None
        };
        let Some(target) = target else {
            return;
        };
        match surface {
            Surface::Log => self.log_scroll.scroll_to_item(target, ScrollStrategy::Top),
            Surface::Files => self
                .files_scroll
                .scroll_to_item(target, ScrollStrategy::Top),
            Surface::Viewer => self.scroll_viewer_to(target),
            Surface::Source => self.scroll_source_to(target),
            Surface::Terminal => self.scroll_terminal_toward(at.y < top),
            _ => {}
        }
    }

    pub(super) fn selection_release(&mut self) {
        self.selecting.dragging = false;
    }

    /// A right click in `surface`: the menu with Copy and Select All.
    pub(super) fn open_context_menu(
        &mut self,
        surface: Surface,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        self.selecting.active = Some(surface);
        let line = self
            .selecting
            .registry
            .hit(surface, event.position)
            .map(|p| p.line);
        if surface == Surface::Runs
            && let Some(index) = line
        {
            self.pick_for_menu(index);
        }
        self.selecting.menu = Some(ContextMenu {
            surface,
            position: event.position,
            line,
        });
        cx.notify();
    }

    pub(super) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.selecting.menu.take().is_some() {
            cx.notify();
        }
    }

    /// The selected text, if any is selected.
    pub(super) fn selected_text(&self) -> Option<String> {
        let selection = self.selecting.selection?;
        let lines = self.surface_lines(selection.surface);
        (!selection.is_empty(lines.as_ref())).then(|| selection.text(lines.as_ref()))
    }

    /// Copies the selection to the clipboard. Returns whether anything
    /// was selected.
    pub(super) fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// Copies one whole line of `surface`.
    pub(super) fn copy_line(&mut self, surface: Surface, line: usize, cx: &mut Context<Self>) {
        let lines = self.surface_lines(surface);
        let Some(shown) = lines.shown(line) else {
            return;
        };
        let Some(text) = lines.copied(line, 0..shown.len()) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Selects everything in the surface pressed in last, or the build
    /// log before any press.
    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        let surface = self.selecting.active.unwrap_or(Surface::Log);
        let selection = {
            let lines = self.surface_lines(surface);
            Selection::all(surface, lines.as_ref())
        };
        self.selecting.selection = Some(selection);
        cx.notify();
    }

    /// Drops the selection when the text it is in changes under it.
    pub(super) fn clear_selection_in(&mut self, surfaces: &[Surface]) {
        let inside = self
            .selecting
            .selection
            .is_some_and(|s| surfaces.contains(&s.surface));
        if inside {
            self.selecting.selection = None;
        }
    }

    /// An invisible layer over the window whose listeners carry a drag
    /// that started in a surface, wherever the pointer goes, and end it
    /// when the button comes up.
    pub(super) fn selection_listener(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let move_view = view.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    move_view.update(cx, |this, cx| {
                        // An edge between panels, while it is dragged.
                        if this.split_drag.is_some() {
                            if e.pressed_button != Some(MouseButton::Left) {
                                this.split_drag = None;
                                return;
                            }
                            this.drag_edge(e.position, cx);
                            return;
                        }
                        if !this.selecting.dragging {
                            return;
                        }
                        if e.pressed_button != Some(MouseButton::Left) {
                            this.selection_release();
                            return;
                        }
                        this.selection_drag(e.position, cx);
                    });
                });
                let up_view = view.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, _, _, cx| {
                    if e.button == MouseButton::Left {
                        up_view.update(cx, |this, _| {
                            this.split_drag = None;
                            this.selection_release();
                        });
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The right-click menu over a backdrop that closes it.
    pub(super) fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let menu = self.selecting.menu?;
        let has_selection = self
            .selecting
            .selection
            .is_some_and(|s| s.surface == menu.surface)
            && self.selected_text().is_some();

        let item = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .px(px(size::MENU_ITEM_PAD_X))
                .py(px(size::MENU_ITEM_PAD_Y))
                .rounded(px(size::RADIUS_MENU_ITEM))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::RAISED_HOVER)))
                .child(label)
        };
        let mut items = div()
            .min_w(px(size::MENU_WIDTH))
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
        // On the Runs panel, what can be done with the runs picked: the
        // text actions do not apply there.
        if menu.surface == Surface::Runs {
            let runs = self.runs_menu_targets();
            let shown = self
                .session
                .as_ref()
                .and_then(|s| s.run.manifest.id.clone());
            let compared = self
                .session
                .as_ref()
                .and_then(|s| s.other.as_ref())
                .and_then(|o| o.manifest.id.clone());
            if let [run] = runs.as_slice()
                && Some(&run.id) != shown.as_ref()
                && Some(&run.id) != compared.as_ref()
            {
                let run = run.clone();
                items = items.child(
                    item("menu-run-compare", "Compare against this run".into()).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.close_context_menu(cx);
                            this.compare_against(run.clone(), cx);
                        }),
                    ),
                );
            }
            if self.pinned_compare.is_some() {
                items = items.child(
                    item(
                        "menu-run-parents",
                        "Compare each run with its parent".into(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_context_menu(cx);
                        this.compare_with_parents(cx);
                    })),
                );
            }
            if !runs.is_empty() {
                let ids = runs
                    .iter()
                    .map(|r| r.id.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                let copy = match runs.len() {
                    1 => "Copy run id".to_string(),
                    n => format!("Copy {n} run ids"),
                };
                items = items.child(item("menu-run-id", copy.into()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(ids.clone()));
                        this.close_context_menu(cx);
                    },
                )));
                let delete = match runs.len() {
                    1 => "Delete".to_string(),
                    n => format!("Delete {n} runs"),
                };
                let targets = runs.clone();
                items = items.child(item("menu-run-delete", delete.into()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.close_context_menu(cx);
                        this.ask_delete_runs(targets.clone(), cx);
                    },
                )));
            }
            // On a schedule 0 run, its folding row or a run from boot under
            // it, the runs that ended as it did show or fold from here
            // as well as from their row.
            if let Some((under, kind)) = menu.line.and_then(|i| self.fold_for_row(i)) {
                let label = match kind {
                    RowKind::Folded {
                        count,
                        folds: Folds::EndedLike,
                    } => format!("Show {count} more at boot"),
                    RowKind::Unfolded {
                        count,
                        folds: Folds::EndedLike,
                    } => format!("Fold {count} at boot"),
                    RowKind::Folded {
                        count,
                        folds: Folds::Windows,
                    } => format!("Show {count} more windows"),
                    RowKind::Unfolded {
                        count,
                        folds: Folds::Windows,
                    } => format!("Fold {count} windows"),
                    RowKind::Run => String::new(),
                };
                items = items.child(item("menu-run-fold", label.into()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.close_context_menu(cx);
                        this.toggle_fold(&under, cx);
                    },
                )));
            }
            items = items.child(
                item("menu-run-all", "Select all".into()).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.close_context_menu(cx);
                        this.pick_all_runs(cx);
                    },
                )),
            );
            return Some(menu_frame(items, menu.position, cx));
        }
        if has_selection {
            items = items.child(item("menu-copy", "Copy".into()).on_click(cx.listener(
                |this, _, _, cx| {
                    this.copy_selection(cx);
                    this.close_context_menu(cx);
                },
            )));
        } else if let Some(line) = menu.line {
            let surface = menu.surface;
            items = items.child(
                item("menu-copy-line", "Copy line".into()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.copy_line(surface, line, cx);
                        this.close_context_menu(cx);
                    },
                )),
            );
        }
        items = items.child(
            item("menu-select-all", "Select all".into()).on_click(cx.listener(|this, _, _, cx| {
                this.select_all(cx);
                this.close_context_menu(cx);
            })),
        );

        Some(menu_frame(items, menu.position, cx))
    }
}

/// The right-click menu's `items` at `position` over a backdrop that
/// closes the menu on any press outside it. Near a window edge the menu
/// moves in to stay whole.
fn menu_frame(items: gpui::Div, position: Point<Pixels>, cx: &mut Context<Scrubber>) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            div()
                .id("menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_context_menu(cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| this.close_context_menu(cx)),
                ),
        )
        .child(
            anchored()
                .position(position)
                .snap_to_window_with_margin(px(size::MENU_EDGE_MARGIN))
                .child(items),
        )
}

/// Mouse handlers that make a container select text in `surface`: a
/// left press starts or extends a selection, a right press opens the menu.
pub fn selects<E: InteractiveElement>(
    element: E,
    surface: Surface,
    cx: &mut Context<Scrubber>,
) -> E {
    element
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                let focus = this.focus_for(surface);
                window.focus(&focus, cx);
                this.selection_press(surface, e, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                this.open_context_menu(surface, e, cx);
                cx.stop_propagation();
            }),
        )
}
