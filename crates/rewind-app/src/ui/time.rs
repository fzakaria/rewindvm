//! The Time tab: where the run's time went and what would make it finish
//! sooner, as `rewind time` says it, with every part a way into the
//! recording.
//!
//! The run's time on the VM's one CPU and its critical path head the tab,
//! with what the path was doing: computing, or asleep and in which call.
//! Under them, the path in order as one bar to scale, with the Nix phases
//! on the same scale; a phase picked scopes the rest of the tab to it.
//! Then the path as a flame graph: each process as wide as its time on the
//! path with everything it started, and under it its children and its own
//! time, computing warm and asleep blue. A click picks a frame; a second
//! click on a process zooms into it, with a breadcrumb back out.
//!
//! The card under the graph is what is picked: a process, a part of one's
//! own time, or every process that ran one program. A process shows its
//! own time by state, what it started, what would make the run sooner and
//! what held it up. A part of its own time shows the source lines of its
//! biggest piece, which the engine finds by replaying the run there, and
//! opens the source panel or gdb there. A program shows its processes,
//! longest first, lit up in the graph. With nothing picked, the card is
//! what would make the whole run sooner.
//!
//! A pick is the window's pick: the playhead moves to the piece picked and
//! the process tree picks its process, so the log, the files and the tree
//! say what was going on then; a process picked in the tree is picked in
//! the graph. The tab's text is selectable and copies as it reads.
//!
//! The path takes up to a few seconds to work out on a big run, so it is
//! worked out in the background, once per run, and kept for the next time
//! the run is on screen; so is each process picked and each source line
//! looked up.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Bounds, ClickEvent, Context, DispatchPhase, Div, HitboxBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, SharedString, Stateful, Window, canvas, div, fill, point, prelude::*, px,
    relative, rgb, rgba,
};
use rewind_trace::Life;
use rewind_trace::time_report::{OwnPart, PathNode, PathTree, Row, RowKind, State, Stretch, share};

use crate::describe::thousands;
use crate::engine::Cancel;
use crate::proc_card;
use crate::selection::{Mapped, Pos, Surface, part_of_line};
use crate::source::{Located, Thread};
use crate::theme::{self, layout, size};
use crate::time::{
    self, AtStep, CHILDREN_SHOWN, Computed, Doing, Drilled, IceFrame, LEVERS_SHOWN, MIN_SEGMENT_PX,
    Marked, NOT_FINISHED, Named, Paint, Pick, PlayheadOnPath, Runner, Spot, Trail, WAITS_SHOWN,
    WORKING, at_playhead, crumb, duration, find, icicle, levers, line_name, members_in, own_ns,
    own_pick, paint_order, phase_blocks, phase_steps, pick_marks, playhead_frame, playhead_on_path,
    segments, skipped_frames, sooner, spot_of, stretch_at, waits, where_lines,
};
use crate::tour::Anchor;
use crate::ui::render::color_only;
use crate::ui::scrubber::{Aim, Replay, Scrubber};
use crate::ui::selectable::{Registry, SelectableText, selectable, selects};
use crate::ui::tabs::RightTab;
use crate::ui::widgets::{Availability, ButtonStyle, button, panel_title, pulsing, tooltip};

/// The size of the two times at the head of the tab.
const FIGURE_TEXT: f32 = size::TEXT_EMPTY_TITLE;

/// The path bar's height, and the phase strip's under it.
const CHAIN_HEIGHT: f32 = 14.0;
const PHASE_HEIGHT: f32 = 18.0;

/// The track under the path bar that marks where what is picked is on
/// the path: how tall, and how far under the bar. The track keeps its
/// room with nothing picked, so picking does not move what is below.
const PICK_TRACK_HEIGHT: f32 = 3.0;
const PICK_TRACK_GAP: f32 = 2.0;

/// How far the line over the stretch under the pointer reaches past the
/// bar, above and below.
const HOVER_OVERHANG: f32 = 2.0;

/// The playhead's mark on the path bar: an amber stick this wide, edged
/// in the panel's color so it shows on amber and brown stretches, and
/// standing out of the bar by its overhang as the timeline's playhead
/// stands out of the track.
const PLAYHEAD_MARK: f32 = 3.0;
const PLAYHEAD_OVERHANG: f32 = 4.0;

/// The frame the playhead is in is lit: the text color over it at this
/// alpha, out of 0xff, enough to show on the smallest frame drawn.
const PLAYHEAD_LIGHT_ALPHA: u32 = 0x99;

/// What the line under the bar starts with: whether the playhead is on
/// the path, on something off it while the path computes elsewhere, or
/// on something off it while the path sleeps. Each is dotted underneath
/// and says on hover what the path is and what being on it means.
const ON_PATH: &str = "On the path";
const OFF_PATH: &str = "Off the path";
const PATH_ASLEEP: &str = "Path asleep";
const PATH_MEANS: &str = "The critical path is the longest chain in the run of work that \
     waited on the work before it: the least the run can take on any number of CPUs. Making \
     what is on it faster makes the run finish sooner; making what is off it faster does not.";
const ON_PATH_TIP: &str = "The process at the playhead is computing on the critical path.";
const OFF_PATH_TIP: &str = "The process at the playhead runs beside the critical path, which \
     is computing in another process.";
/// What comes between the label and the rest of the line: a space before
/// "at" on the path, a colon off it. The spaces do not break, so the
/// line's pieces keep them at their edges.
const JOIN_ON: &str = "\u{a0}";
const JOIN_OFF: &str = ":\u{a0}";
const PATH_ASLEEP_TIP: &str = "The critical path is asleep here, waiting on a timer, so \
     whatever runs at the playhead is off it.";

/// How many shades of grey the phases take turns in.
const PHASE_SHADES: usize = 3;

/// A row of the flame graph: its height, and the gap under it.
const FRAME_HEIGHT: f32 = 18.0;
const FRAME_GAP: f32 = 1.0;

/// A frame narrower than this share of the graph has no text.
const FRAME_TEXT_MIN_SHARE: f32 = 0.04;

/// The ends a process's frame color runs between: warm when it computed
/// the whole time, blue when it slept.
const PROCESS_WARM: u32 = 0xb9894a;
const PROCESS_COLD: u32 = 0x5d7fae;

/// How far a frame's shade strays from its color, either way, by a hash
/// of its name, as flamegraph.pl varies its frames so neighbors part.
const SHADE_SPREAD: f32 = 0.12;

/// The frames of the program picked, lit as flamegraph.pl's search lights
/// what it matches.
const MATCH: u32 = 0xd88ad8;

/// The share of the tab the figures, bar and graph may take before they
/// scroll, so the card under them keeps the rest.
const TOP_SHARE: f32 = 0.6;

/// A window at least this many pixels tall keeps the graph in place while
/// the card scrolls; a shorter one, such as the browser viewer's frame,
/// scrolls the tab as one page, since the graph would leave the card no
/// room.
const SPLIT_MIN_HEIGHT: f32 = 820.0;

/// How the tab's parts scroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeLayout {
    /// The figures, bar and graph scroll apart from the card under them.
    Split,
    /// The whole tab scrolls as one page.
    OnePage,
}

impl TimeLayout {
    /// The layout for a window `height` pixels tall.
    pub fn for_height(height: f32) -> TimeLayout {
        if height >= SPLIT_MIN_HEIGHT {
            TimeLayout::Split
        } else {
            TimeLayout::OnePage
        }
    }
}

/// The characters the time takes at the start of a card's row.
const TIME_CHARS: usize = 7;

/// The side of the square before a card's row in the color its frame has.
const SWATCH: f32 = 8.0;

/// What the head says the path is.
const PATH_CAPTION: &str = "critical path: the least it can take on any number of CPUs";

/// What the flame graph says before anything is picked.
const GRAPH_HINT: &str = "Width is time on the critical path, not time order. Click a frame to pick it and go there; click a process again to zoom into it.";

/// How to use the bar under the head: on the line under it before the
/// playhead has a place on the path, and in the hover note of its label.
const BAR_HINT: &str =
    "The path in order. Point at it to name each stretch; click to move the playhead there.";

/// The card's sections.
const LEVERS_TITLE: &str = "What would shorten it";
const LEVERS_CAPTION: &str = "sooner, if this took no time";
const OWN_TITLE: &str = "Its own time on the path";
const STARTED_TITLE: &str = "What it started";
const WAITS_TITLE: &str = "What held it up";
const MEMBERS_TITLE: &str = "Its processes, by their own time on the path";
const SLEPT_TITLE: &str = "Where its longest sleep was";
const BEGAN_TITLE: &str = "Where its longest stretch of computing began";
const NOTHING_SOONER: &str = "Nothing on its own would make it noticeably sooner.";

/// What the card says while the engine replays to a part's biggest piece.
const LOOKING: &str = "Replaying to the longest piece to find its source line\u{2026}";

/// What the breadcrumb puts between its crumbs.
const CRUMB_SEPARATOR: &str = "\u{203a}";

/// Whether the background work for a run is done.
pub enum Job {
    Working,
    Ready(Arc<Computed>),
    /// Why the run has no report, as a sentence.
    Unavailable(String),
}

/// The source lines of a part's biggest piece: being looked up, found, or
/// not found, and why.
enum Looked {
    Looking,
    Found(Arc<Located>),
    Failed(String),
}

/// One run's report, the processes picked in it, and the pieces whose
/// source lines were looked up, by step and thread: None while a process
/// is being worked out.
struct RunTimes {
    job: Job,
    drilled: HashMap<Life, Option<Arc<Drilled>>>,
    looked: HashMap<(u64, u32), Looked>,
}

/// The reports of the runs the tab was opened on, by the run's path. They
/// stay for as long as the app runs, so a run shown again has its report
/// at once.
#[derive(Default)]
pub struct Times {
    runs: HashMap<PathBuf, RunTimes>,
}

/// The open tab: the run it is of, the processes zoomed into, the phase
/// it is scoped to, the pick, and what is under the pointer. Its text
/// lines, as last drawn, are what a selection in the tab reads; the part
/// selected is worked out once per frame, before they are drawn again.
pub struct TimePanel {
    run: PathBuf,
    pub trail: Trail,
    hover: Option<f32>,
    phase: Option<usize>,
    picked: Option<Pick>,
    frame_hover: Option<usize>,
    lines: RefCell<Vec<Mapped>>,
    selected: RefCell<Option<Range<Pos>>>,
}

impl TimePanel {
    fn of(run: PathBuf) -> TimePanel {
        TimePanel {
            run,
            trail: Trail::default(),
            hover: None,
            phase: None,
            picked: None,
            frame_hover: None,
            lines: RefCell::default(),
            selected: RefCell::default(),
        }
    }
}

/// The tab's pick and the process tree's, at a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Picks {
    time: Option<Pick>,
    tree: Option<proc_card::Pick>,
}

/// A place in the playhead's history: its step, and while the Time tab
/// is open the picks there, so Back and Forward bring a pick back and two
/// picks on one step are two places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Place {
    pub(super) step: u64,
    picks: Option<Picks>,
}

impl Place {
    /// A place that is only a step, with no picks to bring back.
    #[cfg(test)]
    pub(super) fn at(step: u64) -> Place {
        Place { step, picks: None }
    }
}

/// Whether a frame of the flame graph is the one the playhead is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtPlayhead {
    Yes,
    No,
}

/// Where a pick came from: the tab, which moves the playhead to it, or
/// the process tree, which is already at a step the process was alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickedIn {
    Tab,
    Tree,
}

impl Scrubber {
    /// Opens the Time tab on the run, and starts working out its path
    /// when it is not known yet.
    pub(super) fn open_time(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        self.time = Some(TimePanel::of(session.run.path.clone()));
        self.right_tab = RightTab::Time;
        self.ensure_time(cx);
        cx.notify();
    }

    /// Opens the tab; shows it when it is open behind another; or closes
    /// it when it shows.
    pub(super) fn toggle_time(&mut self, cx: &mut Context<Self>) {
        match (&self.time, self.right_tab) {
            (Some(_), RightTab::Time) => self.close_time(cx),
            (Some(_), _) => self.select_tab(RightTab::Time, cx),
            (None, _) => self.open_time(cx),
        }
    }

    /// Shows the tab, opening it when it is not open.
    pub(super) fn show_time(&mut self, cx: &mut Context<Self>) {
        match self.time {
            Some(_) => self.select_tab(RightTab::Time, cx),
            None => self.open_time(cx),
        }
    }

    pub(super) fn close_time(&mut self, cx: &mut Context<Self>) {
        self.time = None;
        self.clear_selection_in(&[Surface::Time]);
        if self.right_tab == RightTab::Time {
            self.right_tab = RightTab::AtStep;
        }
        cx.notify();
    }

    /// The tab for a session just shown: the new run's, when the tab was
    /// open on another.
    pub(super) fn time_session_changed(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(session)) = (&self.time, &self.session) else {
            return;
        };
        if panel.run == session.run.path {
            return;
        }
        self.time = Some(TimePanel::of(session.run.path.clone()));
        self.clear_selection_in(&[Surface::Time]);
        self.ensure_time(cx);
    }

    /// Starts the background work the tab waits on: the run's path, and
    /// the process picked, or the process a program picked is under,
    /// unless either is known or under way.
    pub(super) fn ensure_time(&mut self, cx: &mut Context<Self>) {
        let (Some(panel), Some(session)) = (&self.time, &self.session) else {
            return;
        };
        let run = &session.run;
        let path = run.path.clone();
        let drill = match panel.picked {
            Some(Pick::Program { under, .. }) => under,
            Some(pick) => pick.life(),
            None => None,
        };

        // The run's path, worked out once.
        if !self.times.runs.contains_key(&path) {
            let job = match &run.manifest {
                None => Job::Unavailable(NOT_FINISHED.to_string()),
                Some(_) => Job::Working,
            };
            let working = matches!(job, Job::Working);
            self.times.runs.insert(
                path.clone(),
                RunTimes {
                    job,
                    drilled: HashMap::new(),
                    looked: HashMap::new(),
                },
            );
            if working {
                self.compute_time(path.clone(), cx);
            }
        }

        // The process picked, once the path is known.
        let Some(life) = drill else {
            return;
        };
        let Some(times) = self.times.runs.get_mut(&path) else {
            return;
        };
        let Job::Ready(computed) = &times.job else {
            return;
        };
        if times.drilled.contains_key(&life) {
            return;
        }
        times.drilled.insert(life, None);
        let computed = computed.clone();
        let worked = cx
            .background_executor()
            .spawn(async move { Arc::new(time::drill(&computed.timing, life)) });
        cx.spawn(async move |this, cx| {
            let drilled = worked.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(times) = this.times.runs.get_mut(&path) {
                    times.drilled.insert(life, Some(drilled));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Works out the path of the run on screen, at `path`, in the
    /// background, and keeps it.
    fn compute_time(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let run = &self.session().run;
        let timeline = run.timeline.clone();
        let io = run.io.clone();
        let manifest = run.manifest.as_ref();
        let end_ns = manifest
            .and_then(|m| m.outcome.as_ref())
            .map(|o| o.virtual_ns);
        let clock = manifest.map(|m| m.spec.clock).unwrap_or_default();
        let worked = cx
            .background_executor()
            .spawn(async move { time::compute(&timeline.trace, &io, end_ns, clock).map(Arc::new) });
        cx.spawn(async move |this, cx| {
            let computed = worked.await;
            let _ = this.update(cx, |this, cx| {
                let job = match computed {
                    Ok(computed) => Job::Ready(computed),
                    Err(why) => Job::Unavailable(why),
                };
                if let Some(times) = this.times.runs.get_mut(&path) {
                    times.job = job;
                }

                // A process picked while the path was worked out is worked
                // out now.
                this.ensure_time(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The report of the run the tab is open on, once worked out.
    fn time_computed(&self) -> Option<Arc<Computed>> {
        let panel = self.time.as_ref()?;
        match &self.times.runs.get(&panel.run)?.job {
            Job::Ready(computed) => Some(computed.clone()),
            Job::Working | Job::Unavailable(_) => None,
        }
    }

    /// Picks a frame or a row: the card then shows it, a part of a
    /// process's own time has its source lines looked up, and a pick in
    /// the tab moves the playhead to its biggest piece and picks its
    /// process in the process tree.
    fn time_pick(&mut self, pick: Pick, picked_in: PickedIn, cx: &mut Context<Self>) {
        let before = self.place_at(self.step);
        let Some(panel) = &mut self.time else {
            return;
        };
        panel.picked = Some(pick);
        self.clear_selection_in(&[Surface::Time]);
        self.ensure_time(cx);
        if let Pick::Own { life, part } = pick
            && let Some(spot) = self.time_part(life, part).map(|p| Spot::of(life, &p))
        {
            self.time_look(spot, cx);
        }
        if picked_in == PickedIn::Tab {
            self.time_go(pick, before, cx);
        }
        cx.notify();
    }

    /// Moves the playhead to where `pick` is, and picks its process in
    /// the process tree, without leaving the tab. The jump goes into the
    /// history from the place `before` it, picks and all, so Back brings
    /// them back even when the pick did not move the playhead.
    fn time_go(&mut self, pick: Pick, before: Place, cx: &mut Context<Self>) {
        let Some(computed) = self.time_computed() else {
            return;
        };
        let Some(panel) = &self.time else {
            return;
        };
        let members = match pick {
            Pick::Program { program, under } => Some(computed.members(program, under)),
            Pick::Process(_) | Pick::Own { .. } => None,
        };
        let tree = computed.tree_of(panel.phase);
        let Some(spot) = spot_of(tree, &computed.stretches, pick, members.as_ref()) else {
            return;
        };
        self.go_to(spot.step, cx);
        let timeline = &self.session().run.timeline;
        self.shown.picked_row = proc_card::pick_thread(timeline, spot.pid, spot.step);
        let after = self.place_at(self.step);
        self.shown.history.jumped(before, after);
    }

    /// The place at `step`: the step, and the tab's and the process
    /// tree's picks while the tab is open.
    pub(super) fn place_at(&self, step: u64) -> Place {
        let picks = self.time.as_ref().map(|panel| Picks {
            time: panel.picked,
            tree: self.shown.picked_row,
        });
        Place { step, picks }
    }

    /// After Back or Forward to `place`: the picks the tab had there,
    /// when it was open.
    pub(super) fn time_restore(&mut self, place: Place, cx: &mut Context<Self>) {
        let Some(picks) = place.picks else {
            return;
        };
        let Some(panel) = &mut self.time else {
            return;
        };
        panel.picked = picks.time;
        panel.frame_hover = None;
        self.shown.picked_row = picks.tree;
        self.clear_selection_in(&[Surface::Time]);
        self.ensure_time(cx);
        cx.notify();
    }

    /// A process picked in the process tree while the tab shows: picked
    /// in the graph too, when it is on the path. Whether the tab took it.
    pub(super) fn time_takes(&mut self, row: proc_card::Pick, cx: &mut Context<Self>) -> bool {
        if self.right_tab != RightTab::Time {
            return false;
        }
        let Some(computed) = self.time_computed() else {
            return false;
        };
        let Some(panel) = &self.time else {
            return false;
        };
        let trace = &self.session().run.timeline.trace;
        let Some(life) = trace.life_at(row.pid, row.start) else {
            return false;
        };
        let Some(node) = find(computed.tree_of(panel.phase), life) else {
            return false;
        };
        let pick = Pick::Process(node.life);
        self.shown.picked_row = Some(row);
        self.time_pick(pick, PickedIn::Tree, cx);
        true
    }

    /// A click on a frame of the flame graph: a plain click picks it, and
    /// a second click on the process picked, or a double click, zooms
    /// into it. A click that ended a drag selecting text does neither.
    fn time_frame_click(&mut self, pick: Pick, e: &ClickEvent, cx: &mut Context<Self>) {
        let Some(panel) = &self.time else {
            return;
        };
        let again = panel.picked == Some(pick);
        let double = e.click_count() > 1;
        let plain = self.plain_click(e);
        if let Pick::Process(life) = pick
            && again
            && (double || plain)
        {
            self.time_zoom(life, cx);
            return;
        }
        if plain {
            self.time_pick(pick, PickedIn::Tab, cx);
        }
    }

    /// A click on a row of the card: a plain click picks what it is.
    fn time_row_click(&mut self, pick: Pick, e: &ClickEvent, cx: &mut Context<Self>) {
        if self.plain_click(e) {
            self.time_pick(pick, PickedIn::Tab, cx);
        }
    }

    /// Zooms into `life`'s line of the graph.
    fn time_zoom(&mut self, life: Life, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.time else {
            return;
        };
        panel.trail.enter(life);
        panel.frame_hover = None;
        self.clear_selection_in(&[Surface::Time]);
        cx.notify();
    }

    /// A part of `life`'s own time in the tree on screen.
    fn time_part(&self, life: Life, part: usize) -> Option<OwnPart> {
        let computed = self.time_computed()?;
        let panel = self.time.as_ref()?;
        let node = find(computed.tree_of(panel.phase), life)?;
        node.own.get(part).cloned()
    }

    /// Scopes the tab to a phase, or back to the whole path when it is the
    /// phase it is scoped to. The zoom starts over in the new tree, and a
    /// process picked that is not in it is let go.
    fn time_phase(&mut self, index: usize, cx: &mut Context<Self>) {
        let computed = self.time_computed();
        let Some(panel) = &mut self.time else {
            return;
        };
        panel.phase = if panel.phase == Some(index) {
            None
        } else {
            Some(index)
        };
        panel.trail.back_to(0);
        panel.frame_hover = None;
        let picked_life = panel.picked.and_then(|p| p.life());
        if let (Some(life), Some(computed)) = (picked_life, computed)
            && find(computed.tree_of(panel.phase), life).is_none()
        {
            panel.picked = None;
        }
        self.clear_selection_in(&[Surface::Time]);
        cx.notify();
    }

    /// Goes back to the crumb at `depth`, 0 being the whole path.
    fn time_back_to(&mut self, depth: usize, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.time else {
            return;
        };
        panel.trail.back_to(depth);
        panel.frame_hover = None;
        self.clear_selection_in(&[Surface::Time]);
        cx.notify();
    }

    /// Escape in the tab: lets go of the pick, then zooms one process
    /// back out.
    pub(super) fn time_out(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.time else {
            return;
        };
        if panel.picked.take().is_none() {
            panel.trail.out();
        }
        panel.frame_hover = None;
        self.clear_selection_in(&[Surface::Time]);
        cx.notify();
    }

    /// Whether Escape has something to undo in the tab before it closes
    /// it: a pick, or a zoom.
    pub(super) fn time_inside(&self) -> bool {
        self.time
            .as_ref()
            .is_some_and(|panel| panel.picked.is_some() || panel.trail.shown().is_some())
    }

    /// The tab's text as last drawn, for a selection in it.
    pub(super) fn time_lines(&self) -> Vec<Mapped> {
        self.time
            .as_ref()
            .map(|panel| panel.lines.borrow().clone())
            .unwrap_or_default()
    }

    /// Records where the pointer is along the path bar, or that it left.
    fn set_time_hover(&mut self, fraction: Option<f32>, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.time else {
            return;
        };
        if panel.hover != fraction {
            panel.hover = fraction;
            cx.notify();
        }
    }

    /// Records the frame of the flame graph under the pointer.
    fn set_frame_hover(&mut self, frame: Option<usize>, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.time else {
            return;
        };
        if panel.frame_hover != frame {
            panel.frame_hover = frame;
            cx.notify();
        }
    }

    /// Looks up the source lines at `spot` in the background: the engine
    /// replays the run to the step and walks the thread's stack in gdb,
    /// as `rewind where` does. The answer is kept for the tab and for the
    /// source panel, which then shows the spot at once.
    fn time_look(&mut self, spot: Spot, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let run = session.run.path.clone();
        let Some(times) = self.times.runs.get_mut(&run) else {
            return;
        };
        let key = (spot.step, spot.tid);
        if matches!(
            times.looked.get(&key),
            Some(Looked::Looking | Looked::Found(_))
        ) {
            return;
        }
        times.looked.insert(key, Looked::Looking);
        let thread = Thread::Of {
            pid: spot.pid,
            tid: spot.tid,
        };
        let engine = self.engine_for_run();
        let asked = run.clone();
        let task = crate::jobs::on_own_thread(move || {
            engine.locate(
                &asked,
                spot.step,
                thread,
                None,
                &Cancel::default(),
                &mut |_| {},
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let looked = match result {
                    Ok(located) => {
                        this.place_answers
                            .insert((run.clone(), thread, spot.step, None), located.clone());
                        Looked::Found(Arc::new(located))
                    }
                    Err(why) => Looked::Failed(why.to_string()),
                };
                if let Some(times) = this.times.runs.get_mut(&run) {
                    times.looked.insert(key, looked);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The source panel at `spot`, on its thread.
    fn time_source(&mut self, spot: Spot, cx: &mut Context<Self>) {
        let thread = Thread::Of {
            pid: spot.pid,
            tid: spot.tid,
        };
        self.show_thread_at(spot.step, (thread, None), cx);
    }

    /// gdb at `spot`, in its thread, in the terminal pane.
    fn time_gdb(&mut self, spot: Spot, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_to(spot.step, cx);
        let aim = Aim::Process {
            pid: spot.pid,
            tid: spot.tid,
        };
        self.open_replay(Replay::Gdb, aim, window, cx);
    }

    /// The Time tab: the figures, the bar and the graph over the card, each
    /// part scrolling on its own so the card's rows and the frames they
    /// light stay on screen together.
    pub(super) fn render_time(&self, layout: TimeLayout, cx: &mut Context<Self>) -> Option<Div> {
        let panel = self.time.as_ref()?;
        let session = self.session();
        let times = self.times.runs.get(&session.run.path);

        // The selection in the tab, worked out from the lines as they were
        // drawn last, before they are drawn again.
        *panel.selected.borrow_mut() = self.selected_range(Surface::Time);
        panel.lines.borrow_mut().clear();

        let top = div().id("time-top").flex().flex_col().flex_none();
        let mut top = match layout {
            TimeLayout::Split => top.max_h(relative(TOP_SHARE)).overflow_y_scroll(),
            TimeLayout::OnePage => top,
        }
        .gap(px(size::SECTION_GAP))
        .px(px(size::PANEL_PAD_X))
        .py(px(size::LIST_PAD_Y));

        // Until the path is worked out, or for a run that has none, the
        // tab says so.
        let computed = match times.map(|t| &t.job) {
            None | Some(Job::Working) => {
                top = top.child(quiet_working("time-working", WORKING.to_string()));
                return Some(time_frame(layout, top, None, None));
            }
            Some(Job::Unavailable(why)) => {
                top = top.child(div().text_color(rgb(theme::MUTED)).child(why.clone()));
                return Some(time_frame(layout, top, None, None));
            }
            Some(Job::Ready(computed)) => computed.clone(),
        };
        let times = times?;
        let tree = computed.tree_of(panel.phase);

        // The line zoomed into, or the path's biggest root.
        let zoomed = panel.trail.shown().and_then(|life| find(tree, life));
        let shown = zoomed.or_else(|| tree.roots.first());

        // The program picked lights its processes in the bar and graph.
        let members = match panel.picked {
            Some(Pick::Program { program, under }) => Some(computed.members(program, under)),
            _ => None,
        };

        // Where the playhead is on the path. The graph lights the stretch
        // under the pointer while it is on the bar, as the line under the
        // bar names it, and else the playhead's stretch; either only when
        // it is in the phase the tab is scoped to.
        let playhead = playhead_on_path(&computed.stretches, self.step);
        let pointed = panel
            .hover
            .and_then(|f| stretch_at(&computed.stretches, computed.report.chain_ns, f));
        let in_scope = pointed
            .or(playhead.map(|p| p.stretch))
            .map(|i| &computed.stretches[i])
            .filter(|s| {
                panel
                    .phase
                    .is_none_or(|i| phase_steps(&computed.report.phases, i).contains(&s.step))
            });
        let lineage = in_scope
            .and_then(|s| s.life)
            .map(|l| computed.timing.lineage(l))
            .unwrap_or_default();
        let marked = in_scope.map(|s| (s, lineage.as_slice()));

        top = top
            .child(self.render_time_head(panel, &computed, tree, cx))
            .child(self.render_path_bar(&computed, panel, members.as_ref(), playhead, cx))
            .child(self.render_crumbs(&computed, panel, cx));

        // The graph's readout stays under it on one page. When the top
        // and the card scroll apart, the readout is pinned between them,
        // so the frame under the pointer is named however far either has
        // scrolled, and a top cut short by its share of the tab does not
        // hide it.
        let mut pinned = None;
        if let Some(shown) = shown {
            let flame = self.render_flame(shown, panel, members.as_ref(), marked, cx);
            top = match layout {
                TimeLayout::OnePage => top.child(flame.joined()),
                TimeLayout::Split => {
                    pinned = Some(
                        flame
                            .readout
                            .flex_none()
                            .px(px(size::PANEL_PAD_X))
                            .py(px(size::LEGEND_GAP)),
                    );
                    top.child(flame.graph)
                }
            };
        }

        // The card's border stays put and its rows scroll inside it, so
        // the box keeps its top edge however far the rows have scrolled.
        let rows = div()
            .id("time-card")
            .flex()
            .flex_col()
            .min_h_0()
            .p(px(size::CARD_PAD))
            .child(self.render_card(&computed, tree, panel, times, members.as_ref(), cx));
        let rows = match layout {
            TimeLayout::Split => rows.overflow_y_scroll(),
            TimeLayout::OnePage => rows,
        };
        let boxed = div()
            .flex()
            .flex_col()
            .min_h_0()
            .rounded(px(size::RADIUS_SEGMENT))
            .border_1()
            .border_color(rgb(theme::LINE))
            .child(rows);
        let card = div().id("time-card-pane").flex().flex_col();
        let card = match layout {
            TimeLayout::Split => card.flex_grow(layout::FILL).min_h_0(),
            TimeLayout::OnePage => card.flex_none(),
        }
        .px(px(size::PANEL_PAD_X))
        .pb(px(size::LIST_PAD_Y))
        .child(boxed);
        Some(time_frame(layout, top, pinned, Some(card)))
    }

    /// The two times, and what the path was doing.
    fn render_time_head(
        &self,
        panel: &TimePanel,
        computed: &Computed,
        tree: &PathTree,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let report = &computed.report;
        let figure = |value: String, caption: &str, color: u32| {
            div()
                .flex()
                .flex_none()
                .flex_col()
                .child(
                    div()
                        .text_size(px(FIGURE_TEXT))
                        .text_color(rgb(color))
                        .whitespace_nowrap()
                        .child(text_line(panel, registry, value)),
                )
                .child(
                    div()
                        .text_size(px(size::TEXT_SMALL))
                        .text_color(rgb(theme::MUTED))
                        .child(text_line(panel, registry, caption.to_string())),
                )
        };
        let head = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(size::CARD_GAP))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x(px(size::SECTION_GAP * 2.0))
                    .gap_y(px(size::CARD_GAP))
                    .child(figure(
                        duration(report.took_ns),
                        "elapsed on the VM's one CPU",
                        theme::TEXT,
                    ))
                    .child(figure(
                        duration(report.chain_ns),
                        PATH_CAPTION,
                        theme::AMBER_PALE,
                    )),
            )
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::SOFT))
                    .child(text_line(panel, registry, states_line(tree))),
            );
        let head = self.with_callout(head, Anchor::TimeHead, cx);
        selects(head, Surface::Time, cx)
    }

    /// The path in order as a bar with the playhead's place on it, the
    /// line naming the stretch under the pointer or else the playhead's,
    /// and the phases on the bar's scale.
    fn render_path_bar(
        &self,
        computed: &Arc<Computed>,
        panel: &TimePanel,
        members: Option<&HashSet<Life>>,
        playhead: Option<PlayheadOnPath>,
        cx: &mut Context<Self>,
    ) -> Div {
        // What is picked, and what it started, are marked on a track under
        // the bar, in the color the graph picks them in: the pick's border
        // for a process, magenta for every process of a program.
        let tree = computed.tree_of(panel.phase);
        let marked = marked_of(computed, tree, panel.picked, members);
        let steps = panel.phase.map(|i| phase_steps(&computed.report.phases, i));
        let mark_color = match panel.picked {
            Some(Pick::Program { .. }) => MATCH,
            _ => theme::TEXT,
        };
        let track = div()
            .relative()
            .w_full()
            .h(px(PICK_TRACK_HEIGHT))
            .child(pick_paint(computed.clone(), marked, steps, mark_color));
        let mut bar = div()
            .id("time-chain")
            .relative()
            .w_full()
            .h(px(CHAIN_HEIGHT))
            .rounded(px(size::RADIUS_SEGMENT))
            .bg(rgb(theme::RAISED))
            .cursor_pointer()
            .child(chain_paint(computed.clone()))
            .child(self.chain_input(computed.clone(), cx));

        // The playhead's place on the path, in the playhead's color with
        // the panel's on each side.
        let chain_ns = computed.report.chain_ns;
        if let Some(p) = playhead
            && chain_ns > 0
        {
            let width = PLAYHEAD_MARK + 2.0;
            bar = bar.child(
                div()
                    .absolute()
                    .left(relative(p.ns as f32 / chain_ns as f32))
                    .ml(px(-width / 2.0))
                    .top(px(-PLAYHEAD_OVERHANG))
                    .bottom(px(-PLAYHEAD_OVERHANG))
                    .w(px(width))
                    .bg(rgb(theme::AMBER))
                    .border_l_1()
                    .border_r_1()
                    .border_color(rgb(theme::PANEL)),
            );
        }

        // The stretch under the pointer: a line over it, and its name,
        // length and step on the line under the bar. Off the bar, the
        // line names the stretch the playhead is in.
        let hovered = panel
            .hover
            .and_then(|f| stretch_at(&computed.stretches, chain_ns, f).map(|i| (f, i)));
        // Off the bar, the playhead's stretch, and whether the playhead is
        // on the path or on something running beside it.
        let mut where_on = None;
        let readout = match hovered {
            None => match playhead {
                Some(p) => {
                    let s = &computed.stretches[p.stretch];
                    let timing = &computed.timing;
                    let at = at_playhead(&timing.graph.runs, &timing.lives, s, self.step);
                    let along = duration(p.ns);
                    match at {
                        AtStep::OnPath => {
                            where_on = Some((ON_PATH, ON_PATH_TIP, theme::SOFT));
                            format!("at {along}: {}", s.label)
                        }
                        AtStep::Beside(runner) => {
                            where_on = Some(match s.state {
                                State::Asleep(_) => (PATH_ASLEEP, PATH_ASLEEP_TIP, theme::BLUE),
                                State::Computing => (OFF_PATH, OFF_PATH_TIP, theme::AMBER),
                            });
                            let runs = match runner {
                                Runner::Process(l) => timing.name(l),
                                Runner::Kernel => "a kernel thread".to_string(),
                                Runner::Idle => "the CPU idle".to_string(),
                            };
                            format!("{runs} \u{b7} path at {along}: {}", s.label)
                        }
                    }
                }
                None => BAR_HINT.to_string(),
            },
            Some((fraction, i)) => {
                bar = bar.child(
                    div()
                        .absolute()
                        .left(relative(fraction))
                        .top(px(-HOVER_OVERHANG))
                        .bottom(px(-HOVER_OVERHANG))
                        .w(px(1.0))
                        .bg(rgb(theme::TEXT)),
                );
                let s = &computed.stretches[i];
                format!(
                    "{} \u{b7} step {} \u{b7} {}",
                    duration(s.ns),
                    thousands(s.step),
                    s.label
                )
            }
        };
        let readout_color = if hovered.is_some() || playhead.is_some() {
            theme::SOFT
        } else {
            theme::FAINT
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(size::LEGEND_GAP))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(PICK_TRACK_GAP))
                    .child(bar)
                    .child(track),
            )
            .child(self.render_phase_strip(computed, panel, cx))
            .child(match where_on {
                None => div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(readout_color))
                    .truncate()
                    .child(readout),

                // Off the bar, the line says first whether the playhead
                // is on the path, in the color of what the path is doing,
                // dotted underneath and explained on hover; then what
                // runs there and where the path is.
                Some((label, tip, color)) => {
                    let joiner = if label == ON_PATH { JOIN_ON } else { JOIN_OFF };
                    div()
                        .flex()
                        .min_w_0()
                        .text_size(px(size::TEXT_SMALL))
                        .child(
                            div()
                                .id("time-on-path")
                                .flex_none()
                                .text_color(rgb(color))
                                .border_b_1()
                                .border_dashed()
                                .border_color(rgb(color))
                                .tooltip(tooltip(format!("{tip} {PATH_MEANS} {BAR_HINT}")))
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(rgb(readout_color))
                                .child(joiner),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(readout_color))
                                .child(readout),
                        )
                }
            })
    }

    /// The layer over the path bar that takes the pointer: moving over it
    /// names the stretch under it, and a press moves the playhead to the
    /// stretch's first step. Each listener asks the layer's hitbox
    /// whether the pointer is on the bar, as the switch band's do.
    fn chain_input(&self, computed: Arc<Computed>, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                let along = move |x: gpui::Pixels| (x - bounds.origin.x) / bounds.size.width;

                let (move_entity, move_hitbox) = (entity.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let over = move_hitbox.is_hovered(window).then(|| along(e.position.x));
                    move_entity.update(cx, |this, cx| this.set_time_hover(over, cx));
                });

                let down_entity = entity.clone();
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    let pressed = e.button == MouseButton::Left && hitbox.is_hovered(window);
                    if phase != DispatchPhase::Bubble || !pressed {
                        return;
                    }
                    let chain_ns = computed.report.chain_ns;
                    let Some(i) = stretch_at(&computed.stretches, chain_ns, along(e.position.x))
                    else {
                        return;
                    };
                    let step = computed.stretches[i].step;
                    down_entity.update(cx, |this, cx| this.jump_to(step, cx));
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The Nix phases on the path bar's scale, each named where it is
    /// wide enough; a click scopes the tab to the phase, and again back to
    /// the whole path.
    fn render_phase_strip(
        &self,
        computed: &Computed,
        panel: &TimePanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let report = &computed.report;
        let mut strip = div().relative().w_full().h(px(PHASE_HEIGHT));
        for (k, block) in phase_blocks(&report.phases, report.chain_ns)
            .into_iter()
            .enumerate()
        {
            let index = block.index;
            let phase = &report.phases[index];
            let chosen = panel.phase == Some(index);
            let note = format!(
                "{}: {} on the path. Click to see only this phase; again for the whole path.",
                phase.name,
                duration(phase.on_chain_ns),
            );
            let (bg, ink) = if chosen {
                (theme::AMBER, theme::AMBER_INK)
            } else {
                (theme::PHASE_GREYS[k % PHASE_SHADES], theme::MUTED)
            };
            let mut cell = div()
                .id(SharedString::from(format!("time-phase-{k}")))
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(block.from))
                .w(relative(block.share))
                .flex()
                .items_center()
                .overflow_hidden()
                .px(px(size::LEGEND_GAP))
                .bg(rgb(bg))
                .border_r_1()
                .border_color(rgb(theme::PANEL))
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(ink))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .tooltip(tooltip(note))
                .on_click(cx.listener(move |this, _, _, cx| this.time_phase(index, cx)));
            if block.named == Named::Shown {
                cell = cell.child(div().truncate().child(block.name));
            }
            strip = strip.child(cell);
        }
        strip
    }

    /// The breadcrumb: the whole path, or the phase picked, then each
    /// process zoomed into, each a click back to it but the last.
    fn render_crumbs(&self, computed: &Computed, panel: &TimePanel, cx: &mut Context<Self>) -> Div {
        let first = match panel.phase.and_then(|i| computed.report.phases.get(i)) {
            Some(phase) => format!("{}, {}", self.session().run.name(), phase.name),
            None => self.session().run.name(),
        };
        let mut names = vec![first];
        for life in panel.trail.lives() {
            names.push(crumb(&computed.timing.name(*life), life.pid));
        }
        let last = names.len() - 1;
        let mut crumbs = div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(px(size::CARD_GAP))
            .text_size(px(size::TEXT_SMALL));
        for (depth, name) in names.into_iter().enumerate() {
            if depth > 0 {
                crumbs = crumbs.child(div().text_color(rgb(theme::FAINT)).child(CRUMB_SEPARATOR));
            }
            if depth == last {
                crumbs = crumbs.child(div().text_color(rgb(theme::TEXT)).child(name));
                continue;
            }
            crumbs = crumbs.child(
                crate::ui::bookmarks::link(SharedString::from(format!("time-crumb-{depth}")), name)
                    .on_click(cx.listener(move |this, _, _, cx| this.time_back_to(depth, cx))),
            );
        }
        crumbs
    }

    /// The flame graph of `shown`, and the line naming the frame under
    /// the pointer. The frames' text is selectable row by row, left to
    /// right; the processes of a program picked are lit.
    fn render_flame(
        &self,
        shown: &PathNode,
        panel: &TimePanel,
        members: Option<&HashSet<Life>>,
        playhead: Option<(&Stretch, &[Life])>,
        cx: &mut Context<Self>,
    ) -> Flame {
        let frames = icicle(shown);

        // The playhead's frame is the picked process's when the playhead
        // runs under it; the pointer on the bar lights the deepest frame.
        let holder = match panel.hover {
            Some(_) => None,
            None => panel.picked,
        };
        let marked =
            playhead.and_then(|(s, lineage)| playhead_frame(&frames, shown, s, lineage, holder));
        let rows = frames.iter().map(|f| f.depth + 1).max().unwrap_or(0);
        let mut reading: Vec<usize> = (0..frames.len()).collect();
        reading.sort_by(|&a, &b| {
            frames[a]
                .depth
                .cmp(&frames[b].depth)
                .then(frames[a].from.total_cmp(&frames[b].from))
        });
        let mut graph = div()
            .relative()
            .w_full()
            .h(px(rows as f32 * (FRAME_HEIGHT + FRAME_GAP)))
            .overflow_hidden()
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_SMALL));
        for i in reading {
            let at = if marked == Some(i) {
                AtPlayhead::Yes
            } else {
                AtPlayhead::No
            };
            graph = graph.child(self.render_frame(i, &frames[i], panel, members, at, cx));
        }
        let graph = selects(graph, Surface::Time, cx);
        let readout = match panel.frame_hover.and_then(|i| frames.get(i)) {
            Some(f) => frame_readout(f, shown.total_ns),
            None => GRAPH_HINT.to_string(),
        };
        let readout_color = if panel.frame_hover.is_some() {
            theme::SOFT
        } else {
            theme::FAINT
        };
        Flame {
            graph,
            readout: div()
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(readout_color))
                .truncate()
                .child(readout),
        }
    }

    /// One frame of the flame graph: a click picks it, and a second click
    /// on a process zooms into it. The frame the playhead is in is lit,
    /// or while the pointer is on the path bar, the frame it points at.
    fn render_frame(
        &self,
        i: usize,
        frame: &IceFrame,
        panel: &TimePanel,
        members: Option<&HashSet<Life>>,
        at: AtPlayhead,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let lit = members.is_some_and(|m| frame.pick.life().is_some_and(|l| m.contains(&l)));
        let color = if lit { MATCH } else { frame_color(frame) };
        let picked = panel.picked == Some(frame.pick);
        let pick = frame.pick;
        let mut cell = div()
            .id(SharedString::from(format!("time-frame-{i}")))
            .absolute()
            .left(relative(frame.from))
            .w(relative(frame.to - frame.from))
            .top(px(frame.depth as f32 * (FRAME_HEIGHT + FRAME_GAP)))
            .h(px(FRAME_HEIGHT))
            .px(px(size::LEGEND_GAP))
            .flex()
            .items_center()
            .overflow_hidden()
            .bg(rgb(color))
            .border_r_1()
            .border_color(rgb(theme::PANEL))
            .text_color(rgb(theme::AMBER_INK))
            .cursor_pointer()
            .hover(|s| s.opacity(0.85))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_frame_hover(hovered.then_some(i), cx);
            }))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                this.time_frame_click(pick, e, cx);
            }));
        if picked {
            cell = cell.border_2().border_color(rgb(theme::TEXT));
        }
        if at == AtPlayhead::Yes {
            cell = cell.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(rgba((theme::TEXT << 8) | PLAYHEAD_LIGHT_ALPHA)),
            );
        }
        if frame.to - frame.from >= FRAME_TEXT_MIN_SHARE {
            let text = format!("{} {}", frame.label, duration(frame.ns));
            cell = cell.child(div().min_w_0().truncate().child(text_line(
                panel,
                &self.selecting.registry,
                text,
            )));
        }
        cell
    }

    /// The card under the flame graph: what is picked, or with nothing
    /// picked, what would make the whole run sooner.
    fn render_card(
        &self,
        computed: &Computed,
        tree: &PathTree,
        panel: &TimePanel,
        times: &RunTimes,
        members: Option<&HashSet<Life>>,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let chain_ns = computed.report.chain_ns;
        let card = div().flex().flex_col().flex_none().gap(px(size::CARD_GAP));

        let card = match panel.picked {
            None => {
                let rows = levers(computed.report.rows.iter(), chain_ns, LEVERS_SHOWN);
                card.child(section_title(panel, registry, LEVERS_TITLE))
                    .child(self.render_levers(panel, "time-job", computed, tree, &rows, None, cx))
            }
            Some(Pick::Program { program, under }) => {
                let members = members.cloned().unwrap_or_default();
                self.render_group(
                    card, computed, tree, panel, times, program, under, &members, cx,
                )
            }
            Some(pick @ (Pick::Process(_) | Pick::Own { .. })) => {
                let node = pick.life().and_then(|life| find(tree, life));
                match node {
                    None => card,
                    Some(node) => {
                        self.render_time_pick(card, computed, tree, panel, times, pick, node, cx)
                    }
                }
            }
        };
        selects(card, Surface::Time, cx)
    }

    /// A process or a part of its own time: the process's name, its time
    /// on the path and how much of that asleep, then the part or the
    /// process.
    #[allow(clippy::too_many_arguments)]
    fn render_time_pick(
        &self,
        card: Div,
        computed: &Computed,
        tree: &PathTree,
        panel: &TimePanel,
        times: &RunTimes,
        pick: Pick,
        node: &PathNode,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let name = format!("{} (pid {})", line_name(node), node.life.pid);
        let card = card
            .child(self.render_name(panel, "time-name", name))
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(text_line(
                        panel,
                        registry,
                        format!(
                            "{} on the path with what it started, {} of {}; {} asleep",
                            duration(node.total_ns),
                            share(node.total_ns, tree.ns),
                            duration(tree.ns),
                            share(node.asleep_ns, node.total_ns)
                        ),
                    )),
            );
        match pick {
            Pick::Own { part, .. } => match node.own.get(part) {
                Some(own) => self.render_part(card, panel, node, own, times, cx),
                None => card,
            },
            Pick::Process(_) | Pick::Program { .. } => {
                self.render_process(card, computed, tree, panel, node, times, cx)
            }
        }
    }

    /// The name at the head of the card, on one line, cut to fit; it
    /// copies whole and shows whole on hover.
    fn render_name(&self, panel: &TimePanel, id: &'static str, name: String) -> Stateful<Div> {
        div()
            .id(id)
            .min_w_0()
            .truncate()
            .font_family(self.fonts.mono.clone())
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::TEXT))
            .tooltip(tooltip(name.clone()))
            .child(text_line(panel, &self.selecting.registry, name))
    }

    /// A process's card: its own time by state, each with its way to the
    /// source; the biggest of what it started; what would make the run
    /// sooner; and what held it up.
    #[allow(clippy::too_many_arguments)]
    fn render_process(
        &self,
        mut card: Div,
        computed: &Computed,
        tree: &PathTree,
        panel: &TimePanel,
        node: &PathNode,
        times: &RunTimes,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let chain_ns = computed.report.chain_ns;

        // Its own time, each part a click to its source lines.
        if !node.own.is_empty() {
            let mut list = div().flex().flex_col();
            for (i, own) in node.own.iter().enumerate() {
                let pick = Pick::Own {
                    life: node.life,
                    part: i,
                };
                list = list.child(self.render_list_row(
                    panel,
                    format!("time-own-{i}"),
                    Some(state_color(&own.state)),
                    duration(own.ns),
                    part_name(own),
                    Some(pick),
                    cx,
                ));
            }
            card = card
                .child(section_title(panel, registry, OWN_TITLE))
                .child(list);
        }

        // The biggest of what it started.
        if !node.children.is_empty() {
            let mut list = div().flex().flex_col();
            for (i, child) in node.children.iter().take(CHILDREN_SHOWN).enumerate() {
                list = list.child(self.render_list_row(
                    panel,
                    format!("time-child-{i}"),
                    Some(tree_color(child)),
                    duration(child.total_ns),
                    format!("{} (pid {})", line_name(child), child.life.pid),
                    Some(Pick::Process(child.life)),
                    cx,
                ));
            }
            let more = node.children.len().saturating_sub(CHILDREN_SHOWN) + node.hidden;
            if more > 0 {
                list = list.child(muted(panel, registry, format!("and {more} more")));
            }
            card = card
                .child(section_title(panel, registry, STARTED_TITLE))
                .child(list);
        }

        // What would make the run sooner, and what held it up, once the
        // process is worked out.
        let drilled = times.drilled.get(&node.life).cloned().flatten();
        let Some(drilled) = drilled else {
            return card.child(quiet_working(
                "time-drilling",
                format!("Working out pid {}\u{2026}", node.life.pid),
            ));
        };
        let rows = levers(
            drilled.inside.rows.iter().chain(drilled.started.iter()),
            chain_ns,
            LEVERS_SHOWN,
        );
        card = card
            .child(section_title(panel, registry, LEVERS_TITLE))
            .child(self.render_levers(panel, "time-levers", computed, tree, &rows, Some(node), cx));
        let held = waits(&drilled.inside);
        if held.is_empty() {
            return card;
        }
        let mut list = div().flex().flex_col();
        for (i, wait) in held.iter().take(WAITS_SHOWN).enumerate() {
            list = list.child(self.render_list_row(
                panel,
                format!("time-wait-{i}"),
                Some(row_color(computed, tree, wait, Some(node.life))),
                duration(wait.took_ns),
                wait.label.clone(),
                wait.life.map(Pick::Process),
                cx,
            ));
        }
        card.child(section_title(panel, registry, WAITS_TITLE))
            .child(list)
    }

    /// Every process that ran a program: how many, their time on the
    /// path, how much sooner the run would be without them, and the
    /// processes, longest own time first.
    #[allow(clippy::too_many_arguments)]
    fn render_group(
        &self,
        card: Div,
        computed: &Computed,
        tree: &PathTree,
        panel: &TimePanel,
        times: &RunTimes,
        program: u32,
        under: Option<Life>,
        members: &HashSet<Life>,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let chain_ns = computed.report.chain_ns;
        let name = computed
            .programs
            .get(program as usize)
            .cloned()
            .unwrap_or_default();
        let lines = members_in(tree, members);
        let on_path: u64 = lines.iter().map(|n| own_ns(n)).sum();
        let title = match under {
            Some(life) => format!(
                "every {name} under {}, {} processes",
                crumb(&computed.timing.name(life), life.pid),
                members.len()
            ),
            None => format!("every {name}, {} processes", members.len()),
        };
        let mut facts = format!(
            "{} of their own on the path, in {} of them",
            duration(on_path),
            lines.len()
        );

        // The row this group is in its list, for how much sooner.
        let rows: Vec<&Row> = match under {
            None => computed.report.rows.iter().collect(),
            Some(life) => times
                .drilled
                .get(&life)
                .and_then(|d| d.as_ref())
                .map(|d| d.inside.rows.iter().chain(d.started.iter()).collect())
                .unwrap_or_default(),
        };
        let row = rows
            .into_iter()
            .find(|r| matches!(&r.kind, RowKind::Program { program: p, .. } if *p == name));
        if let Some(row) = row {
            facts.push_str(&format!(
                "; {} sooner if every one took no time",
                sooner(row, chain_ns)
            ));
        }

        let mut list = div().flex().flex_col();
        for (i, node) in lines.iter().take(LEVERS_SHOWN).enumerate() {
            list = list.child(self.render_list_row(
                panel,
                format!("time-member-{i}"),
                Some(own_color(node)),
                duration(own_ns(node)),
                format!("{} (pid {})", line_name(node), node.life.pid),
                Some(Pick::Process(node.life)),
                cx,
            ));
        }
        let more = lines.len().saturating_sub(LEVERS_SHOWN);
        if more > 0 {
            list = list.child(muted(panel, registry, format!("and {more} more")));
        }
        card.child(self.render_name(panel, "time-group", title))
            .child(
                div()
                    .text_size(px(size::TEXT_SMALL))
                    .text_color(rgb(theme::MUTED))
                    .child(text_line(panel, registry, facts)),
            )
            .child(section_title(panel, registry, MEMBERS_TITLE))
            .child(list)
    }

    /// A part of a process's own time: how much, how many pieces, and the
    /// source lines its biggest piece was at, with the source panel and
    /// gdb there.
    fn render_part(
        &self,
        card: Div,
        panel: &TimePanel,
        node: &PathNode,
        own: &OwnPart,
        times: &RunTimes,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        let spot = Spot::of(node.life, own);
        let (pieces, title) = match own.state {
            State::Computing => ("times on the CPU", BEGAN_TITLE),
            State::Asleep(_) => ("sleeps", SLEPT_TITLE),
        };
        let mut card = card.child(
            div()
                .text_size(px(size::TEXT_SMALL))
                .text_color(rgb(theme::SOFT))
                .child(text_line(
                    panel,
                    registry,
                    format!(
                        "{} {}, {} {pieces}; the longest {} from step {} on thread {}",
                        own.state.name(),
                        duration(own.ns),
                        own.count,
                        duration(own.biggest_ns),
                        thousands(own.step),
                        own.tid
                    ),
                )),
        );

        // The stack there, as the engine found it: each frame on a line,
        // and under it the source line when there is one.
        card = card.child(section_title(panel, registry, title));
        card = match times.looked.get(&(spot.step, spot.tid)) {
            None | Some(Looked::Looking) => {
                card.child(quiet_working("time-looking", LOOKING.to_string()))
            }
            Some(Looked::Failed(why)) => card.child(muted(panel, registry, why.clone())),
            Some(Looked::Found(located)) => {
                let mut lines = div()
                    .flex()
                    .flex_col()
                    .font_family(self.fonts.mono.clone())
                    .text_size(px(size::TEXT_SMALL));
                if let Some(skipped) = skipped_frames(located) {
                    lines = lines.child(
                        div()
                            .truncate()
                            .text_color(rgb(theme::FAINT))
                            .child(text_line(panel, registry, skipped)),
                    );
                }
                for line in where_lines(located) {
                    let head = format!("#{} {}", line.level, line.function);
                    let text = format!("{head}  {}", line.place);
                    let colors = [
                        (0..head.len(), color_only(theme::TEXT)),
                        (head.len()..text.len(), color_only(theme::MUTED)),
                    ];
                    lines = lines.child(
                        div()
                            .truncate()
                            .child(text_line(panel, registry, text).with_highlights(colors)),
                    );
                    if line.text.is_empty() {
                        continue;
                    }
                    lines = lines.child(
                        div()
                            .pl(px(size::TREE_INDENT))
                            .truncate()
                            .text_color(rgb(theme::AMBER_PALE))
                            .child(text_line(panel, registry, line.text)),
                    );
                }
                card.child(lines)
            }
        };

        // The ways into the recording at the piece; the playhead is there
        // already.
        let action = |id: &'static str, label: &'static str, style: ButtonStyle| {
            button(id, style, Availability::Enabled).child(label)
        };
        card.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(size::BUTTON_GAP))
                .child(
                    action("time-source", "Show source", ButtonStyle::Primary)
                        .tooltip(tooltip("The source panel at this step, on this thread."))
                        .on_click(cx.listener(move |this, _, _, cx| this.time_source(spot, cx))),
                )
                .child(
                    action("time-gdb", "Open gdb here", ButtonStyle::Neutral)
                        .tooltip(tooltip(
                            "gdb in a fork of the run at this step, in this thread.",
                        ))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.time_gdb(spot, window, cx)),
                        ),
                ),
        )
    }

    /// The rows that would make the run sooner, one number each, a click
    /// picking the process or the program a row is, or for a thread's
    /// computing or sleeps the part of `node`'s own time it is; `node` is
    /// the process the rows are of, for a process's card.
    #[allow(clippy::too_many_arguments)]
    fn render_levers(
        &self,
        panel: &TimePanel,
        id: &str,
        computed: &Computed,
        tree: &PathTree,
        rows: &[&Row],
        node: Option<&PathNode>,
        cx: &mut Context<Self>,
    ) -> Div {
        let registry = &self.selecting.registry;
        if rows.is_empty() {
            return muted(panel, registry, NOTHING_SOONER.to_string());
        }
        let chain_ns = computed.report.chain_ns;
        let under = node.map(|n| n.life);
        let mut list = div().flex().flex_col();
        for (i, row) in rows.iter().enumerate() {
            list = list.child(
                self.render_list_row(
                    panel,
                    format!("{id}-{i}"),
                    Some(row_color(computed, tree, row, under)),
                    sooner(row, chain_ns),
                    row.label.clone(),
                    computed
                        .row_pick(row, under)
                        .or_else(|| node.and_then(|n| own_pick(row, n))),
                    cx,
                ),
            );
        }
        list.child(muted(panel, registry, LEVERS_CAPTION.to_string()))
    }

    /// A row of one of the card's lists: a square in the color its frame
    /// has, a time and what it is, a click picking `pick` when there is
    /// one. The time and the name are one line of text, so a selection
    /// copies them together.
    #[allow(clippy::too_many_arguments)]
    fn render_list_row(
        &self,
        panel: &TimePanel,
        id: String,
        swatch: Option<u32>,
        time: String,
        label: String,
        pick: Option<Pick>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let padded = format!("{time:>TIME_CHARS$}");
        let text = format!("{padded}  {label}");
        let colors = [
            (0..padded.len(), color_only(theme::TEXT)),
            (padded.len()..text.len(), color_only(theme::SOFT)),
        ];
        let square = div()
            .flex_none()
            .size(px(SWATCH))
            .rounded(px(size::RADIUS_SEGMENT));
        let square = match swatch {
            Some(color) => square.bg(rgb(color)),
            None => square,
        };
        let row =
            div()
                .id(SharedString::from(id))
                .flex()
                .items_center()
                .gap(px(size::LEGEND_GAP))
                .px(px(size::LEGEND_GAP))
                .h(px(size::LIST_ROW_HEIGHT))
                .rounded(px(size::RADIUS_SEGMENT))
                .font_family(self.fonts.mono.clone())
                .text_size(px(size::TEXT_SMALL))
                .child(square)
                .child(div().flex_1().min_w_0().truncate().child(
                    text_line(panel, &self.selecting.registry, text).with_highlights(colors),
                ))
                .tooltip(tooltip(label));
        let Some(pick) = pick else {
            return row;
        };
        row.cursor_pointer()
            .hover(|s| s.bg(rgb(theme::ROW_HOVER)))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                this.time_row_click(pick, e, cx);
            }))
    }
}

/// A selectable line of the tab: the next line of its text, drawn with the
/// part selected highlighted.
fn text_line(panel: &TimePanel, registry: &Registry, text: String) -> SelectableText {
    let mut lines = panel.lines.borrow_mut();
    let index = lines.len();
    let part = panel
        .selected
        .borrow()
        .as_ref()
        .and_then(|range| part_of_line(range, index, text.len()));
    lines.push(Mapped::plain(text.clone()));
    selectable(Surface::Time, index, text, part, registry)
}

/// What the pick marks under the path bar, in `tree`, the tree the tab
/// shows: a process and what it started, every process of a program
/// (its `members`), or one part of a process's own time.
fn marked_of(
    computed: &Computed,
    tree: &PathTree,
    picked: Option<Pick>,
    members: Option<&HashSet<Life>>,
) -> Option<Marked> {
    let roots: Vec<Life> = match picked? {
        Pick::Own { life, part } => {
            let state = find(tree, life)?.own.get(part)?.state.clone();
            return Some(Marked::Part { life, state });
        }
        Pick::Process(life) => vec![life],
        Pick::Program { .. } => members?.iter().copied().collect(),
    };
    let mut lives = HashSet::new();
    for life in roots {
        lives.insert(life);
        lives.extend(computed.timing.descendants(life));
    }
    Some(Marked::Lives(lives))
}

/// The path by state, as `rewind time` says it: `67% asleep (epoll_pwait
/// 23.1 s, ...), 33% computing`.
fn states_line(tree: &PathTree) -> String {
    let whole = tree.ns;
    let computing: u64 = tree
        .states
        .iter()
        .filter(|(s, _)| *s == State::Computing)
        .map(|(_, ns)| ns)
        .sum();
    let asleep = whole.saturating_sub(computing);
    let calls: Vec<String> = tree
        .states
        .iter()
        .filter_map(|(s, ns)| match s {
            State::Asleep(call) => Some(format!("{call} {}", duration(*ns))),
            State::Computing => None,
        })
        .take(STATES_SHOWN)
        .collect();
    let asleep_part = match calls.is_empty() {
        true => format!("{} asleep", share(asleep, whole)),
        false => format!("{} asleep ({})", share(asleep, whole), calls.join(", ")),
    };
    let computing_part = format!("{} computing", share(computing, whole));
    if asleep == 0 {
        return "All computing.".to_string();
    }
    if computing >= asleep {
        return format!("{computing_part}, {asleep_part}.");
    }
    format!("{asleep_part}, {computing_part}.")
}

/// How many calls the state line names.
const STATES_SHOWN: usize = 3;

/// A part of a process's own time, named with how many pieces it is.
fn part_name(own: &OwnPart) -> String {
    match own.state {
        State::Computing => format!("computing, {} times on the CPU", own.count),
        State::Asleep(ref call) => format!("asleep in {call}, {} sleeps", own.count),
    }
}

/// What the line under the flame graph says of the frame under the
/// pointer.
fn frame_readout(frame: &IceFrame, top_ns: u64) -> String {
    format!(
        "{} \u{b7} {} on the path, {} of this view",
        frame.label,
        duration(frame.ns),
        share(frame.ns, top_ns)
    )
}

/// The color of a state: computing warm, asleep blue.
fn state_color(state: &State) -> u32 {
    match state {
        State::Computing => theme::AMBER,
        State::Asleep(_) => theme::BLUE,
    }
}

/// The color of a share asleep: from warm, all computing, to blue.
fn asleep_color(asleep_ns: u64, ns: u64) -> u32 {
    let share = if ns == 0 {
        0.0
    } else {
        asleep_ns as f32 / ns as f32
    };
    mix(PROCESS_WARM, PROCESS_COLD, share)
}

/// The color of a process with what it started, as its frame has it.
fn tree_color(node: &PathNode) -> u32 {
    asleep_color(node.asleep_ns, node.total_ns)
}

/// What of a line's own time it slept.
fn own_asleep_ns(node: &PathNode) -> u64 {
    node.own
        .iter()
        .filter(|p| matches!(p.state, State::Asleep(_)))
        .map(|p| p.ns)
        .sum()
}

/// The color of a process's own time: what a row that takes away only
/// its own time stands for.
fn own_color(node: &PathNode) -> u32 {
    asleep_color(own_asleep_ns(node), own_ns(node))
}

/// The color of a row of the card, as what it takes away is painted: a
/// sleep blue, a thread's computing warm, a process by its own share
/// asleep, a program by its processes'. A row with nothing on the path to
/// go by is grey.
fn row_color(computed: &Computed, tree: &PathTree, row: &Row, under: Option<Life>) -> u32 {
    let line = |life: Life| find(tree, life).or_else(|| find(&computed.tree, life));
    match &row.kind {
        RowKind::Sleeps { .. } | RowKind::ThreadSleeps { .. } | RowKind::AllSleeps => theme::BLUE,
        RowKind::Thread { .. } => theme::AMBER,
        RowKind::Process | RowKind::Waits { .. } | RowKind::Tree { .. } => {
            row.life.and_then(line).map_or(theme::FAINT, own_color)
        }
        RowKind::Program { program, .. } => {
            let Some(id) = computed.program_id(program) else {
                return theme::FAINT;
            };
            let members = computed.members(id, under);
            let lines = members_in(tree, &members);
            let asleep: u64 = lines.iter().map(|n| own_asleep_ns(n)).sum();
            let own: u64 = lines.iter().map(|n| own_ns(n)).sum();
            if own == 0 {
                return theme::FAINT;
            }
            asleep_color(asleep, own)
        }
    }
}

/// A frame's color: its state's, or its process's by its share asleep,
/// a shade lighter or darker by a hash of its name, so frames side by
/// side part.
fn frame_color(frame: &IceFrame) -> u32 {
    let base = match frame.paint {
        Paint::Computing => theme::AMBER,
        Paint::Asleep => theme::BLUE,
        Paint::Process { asleep } => mix(PROCESS_WARM, PROCESS_COLD, asleep),
    };
    shade(base, &frame.label)
}

/// `color` a shade lighter or darker, up to [`SHADE_SPREAD`] either way,
/// by a hash of `name`: the same name always gets the same shade.
fn shade(color: u32, name: &str) -> u32 {
    const BUCKETS: u64 = 1_000;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let unit = (hasher.finish() % BUCKETS) as f32 / BUCKETS as f32;
    let factor = 1.0 + SHADE_SPREAD * (unit * 2.0 - 1.0);
    let channel = |shift: u32| {
        let c = ((color >> shift) & 0xff) as f32;
        ((c * factor).round().clamp(0.0, 255.0) as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// The color `share` of the way from `from` to `to`.
fn mix(from: u32, to: u32, share: f32) -> u32 {
    let share = share.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        ((a + (b - a) * share).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// The flame graph and the line under it naming the frame under the
/// pointer, apart so the line can be pinned between the tab's panes.
struct Flame {
    graph: Div,
    readout: Div,
}

impl Flame {
    /// The graph with its line under it, as one part of the page.
    fn joined(self) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(size::LEGEND_GAP))
            .child(self.graph)
            .child(self.readout)
    }
}

/// The tab's frame: its title over the figures, bar and graph, the line
/// `pinned` between them and the card when the two scroll apart, and the
/// card when there is one, scrolling as `layout` says.
fn time_frame(
    layout: TimeLayout,
    top: Stateful<Div>,
    pinned: Option<Div>,
    card: Option<Stateful<Div>>,
) -> Div {
    let frame = div()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .flex_basis(relative(0.0))
        .flex_grow(layout::FILL)
        .bg(rgb(theme::PANEL))
        .child(panel_title("Time", None));
    let parts = div()
        .flex()
        .flex_col()
        .child(top)
        .children(pinned)
        .children(card);
    match layout {
        TimeLayout::Split => frame.child(parts.flex_grow(layout::FILL).min_h_0()),
        TimeLayout::OnePage => frame.child(
            parts
                .id("time-page")
                .flex_grow(layout::FILL)
                .min_h_0()
                .overflow_y_scroll(),
        ),
    }
}

/// A quiet line, fading out and back, while work it waits on runs.
fn quiet_working(id: &'static str, text: String) -> impl IntoElement {
    pulsing(
        id,
        div()
            .text_size(px(size::TEXT_SMALL))
            .text_color(rgb(theme::MUTED))
            .child(text),
    )
}

/// A muted line of small text.
fn muted(panel: &TimePanel, registry: &Registry, text: String) -> Div {
    div()
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::MUTED))
        .child(text_line(panel, registry, text))
}

/// A section's title over its rows.
fn section_title(panel: &TimePanel, registry: &Registry, text: &'static str) -> Div {
    div()
        .pt(px(size::LEGEND_GAP))
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::TEXT))
        .child(text_line(panel, registry, text.to_string()))
}

/// The path bar's paint: a quad per segment, warm where the path computed
/// and blue where it slept, the segments thinner than a pixel painted last.
fn chain_paint(computed: Arc<Computed>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let width = f32::from(bounds.size.width);
            let chain_ns = computed.report.chain_ns;
            let all = segments(&computed.stretches, chain_ns, width);
            for segment in paint_order(&all) {
                let color = match segment.doing {
                    Doing::Sleeping => theme::BLUE,
                    Doing::Computing => theme::AMBER,
                };
                let wide = (segment.to - segment.from).max(MIN_SEGMENT_PX);
                let origin = point(bounds.origin.x + px(segment.from), bounds.origin.y);
                let quad = Bounds::new(origin, gpui::size(px(wide), bounds.size.height));
                window.paint_quad(fill(quad, rgb(color)));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The marks on the track under the path bar where what is `marked` is
/// on the path, within the phase's `steps` when the tab is scoped to one,
/// in `color`; nothing with nothing picked.
fn pick_paint(
    computed: Arc<Computed>,
    marked: Option<Marked>,
    steps: Option<std::ops::Range<u64>>,
    color: u32,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let Some(marked) = &marked else {
                return;
            };
            let width = f32::from(bounds.size.width);
            let chain_ns = computed.report.chain_ns;
            for (from, to) in
                pick_marks(&computed.stretches, chain_ns, marked, steps.clone(), width)
            {
                let origin = point(bounds.origin.x + px(from), bounds.origin.y);
                let quad = Bounds::new(origin, gpui::size(px(to - from), bounds.size.height));
                window.paint_quad(fill(quad, rgb(color)));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    // The card's wording and the graph's colors, from made-up parts.
    use super::*;

    #[test]
    fn a_process_frame_runs_from_warm_to_blue_by_its_sleep() {
        // All computing is the warm end, all asleep the blue end, and
        // half way is half way in each channel.
        assert_eq!(mix(PROCESS_WARM, PROCESS_COLD, 0.0), PROCESS_WARM);
        assert_eq!(mix(PROCESS_WARM, PROCESS_COLD, 1.0), PROCESS_COLD);
        assert_eq!(mix(0x000000, 0x0000ff, 0.5), 0x000080);
    }

    #[test]
    fn a_row_is_colored_by_what_it_takes_away() {
        // autoreconf sleeps 2 s itself and runs 6 s of computing tools: a
        // row that takes away its own time is blue, as its sleep is, while
        // its line with the tools it ran is mostly warm.
        let mut autoreconf = PathNode {
            life: Life { pid: 70, start: 0 },
            top: Life { pid: 70, start: 0 },
            through: Vec::new(),
            label: "autoreconf".to_string(),
            total_ns: 8,
            asleep_ns: 2,
            own: vec![OwnPart {
                state: State::Asleep("clock_nanosleep".to_string()),
                ns: 2,
                count: 2,
                tid: 70,
                step: 1,
                biggest_ns: 1,
            }],
            children: Vec::new(),
            hidden: 0,
            hidden_ns: 0,
        };
        assert_eq!(own_color(&autoreconf), PROCESS_COLD);
        assert_eq!(
            tree_color(&autoreconf),
            mix(PROCESS_WARM, PROCESS_COLD, 0.25)
        );
        autoreconf.own.clear();
        assert_eq!(own_color(&autoreconf), PROCESS_WARM);
    }

    #[test]
    fn a_short_window_scrolls_the_tab_as_one_page() {
        // The native window, 900 pixels tall, keeps the graph in place over
        // the card; the browser viewer's frame, about 600, scrolls it all.
        assert_eq!(TimeLayout::for_height(900.0), TimeLayout::Split);
        assert_eq!(TimeLayout::for_height(SPLIT_MIN_HEIGHT), TimeLayout::Split);
        assert_eq!(TimeLayout::for_height(600.0), TimeLayout::OnePage);
    }

    #[test]
    fn a_frame_keeps_its_shade_and_its_neighbor_differs() {
        // A name always gets the same shade, within the spread of its
        // color either way; two names side by side get different ones.
        let a = shade(theme::AMBER, "cc1 test-fs.c");
        assert_eq!(a, shade(theme::AMBER, "cc1 test-fs.c"));
        assert_ne!(a, shade(theme::AMBER, "cc1 test-ipc.c"));
        let red = |c: u32| ((c >> 16) & 0xff) as f32;
        let spread = red(theme::AMBER) * SHADE_SPREAD + 1.0;
        assert!((red(a) - red(theme::AMBER)).abs() <= spread);
    }

    #[test]
    fn a_part_is_named_by_its_state_and_pieces() {
        // A sleep counts sleeps, computing counts its times on the CPU.
        let own = |state: State, count: usize| OwnPart {
            state,
            ns: 1,
            count,
            tid: 1,
            step: 1,
            biggest_ns: 1,
        };
        assert_eq!(
            part_name(&own(State::Asleep("clock_nanosleep".to_string()), 75)),
            "asleep in clock_nanosleep, 75 sleeps"
        );
        assert_eq!(
            part_name(&own(State::Computing, 3)),
            "computing, 3 times on the CPU"
        );
    }
}
