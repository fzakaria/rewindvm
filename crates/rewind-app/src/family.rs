//! Runs grouped by the build they ran: a family is every run of one
//! derivation, or of one command on one root image. It holds the run as
//! first recorded, the schedules `rewind check` tried, and every fork of
//! any of them, so a thousand forks are one family, not a thousand runs.
//!
//! The engine's runs are read from their manifests alone, without their
//! traces, so a whole home of runs is quick to list.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rewind_trace::signal_name;

use crate::describe::{ago, thousands};
use crate::model::ExitStatus;
use crate::run::{MANIFEST_FILE, Manifest, Parent, read_manifest, short_id};

/// What a run without a recorded exit status is listed as.
const UNKNOWN_ENDING: &str = "unknown";

/// One run, as its manifest describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEntry {
    pub dir: PathBuf,
    pub id: String,
    /// What it built or ran: the manifest's name, else its derivation or
    /// command.
    pub title: String,
    /// The family it belongs to: its derivation, else its command and
    /// image.
    pub family: String,
    pub parent: Option<Parent>,
    pub schedule: u64,
    /// How it ended: exited:2, killed:SIGSEGV, or unknown.
    pub ending: String,
    pub failed: bool,
    pub first_difference: Option<u64>,
    pub trace_hash: Option<String>,
    /// The vCPUs the VM had.
    pub cores: u64,
    /// The steps its schedule was confined to, for the runs rewind check
    /// makes as it narrows a schedule down.
    pub window: Option<(u64, u64)>,
    /// Whether it came from an export made on another machine.
    pub imported: bool,
    /// Its inputs less its schedule, which tie a run from boot under a
    /// perturbed schedule to the run recorded without one.
    pub inputs: Option<String>,
    /// When the run was made, for ordering runs made in the same second
    /// apart from when their directories last changed.
    pub created: u64,
    pub modified: SystemTime,
}

impl RunEntry {
    /// The run in `dir`, from its manifest, or None without one.
    pub fn read(dir: &Path) -> Option<RunEntry> {
        let modified = std::fs::metadata(dir).ok()?.modified().ok()?;
        let manifest = read_manifest(&dir.join(MANIFEST_FILE)).ok()?;
        Some(RunEntry::from_manifest(dir, &manifest, modified))
    }

    pub fn from_manifest(dir: &Path, manifest: &Manifest, modified: SystemTime) -> RunEntry {
        let command = manifest.command.as_ref().map(|c| c.join(" "));
        let title = [manifest.name.clone(), manifest.drv.clone(), command.clone()]
            .into_iter()
            .flatten()
            .find(|t| !t.is_empty())
            .unwrap_or_else(|| dir.display().to_string());
        let family = match (&manifest.drv, &command) {
            (Some(drv), _) => drv.clone(),
            (None, Some(command)) => format!(
                "{command}\u{0}{}",
                manifest.image_hash.as_deref().unwrap_or_default()
            ),
            (None, None) => dir.display().to_string(),
        };
        let status = manifest
            .outcome
            .as_ref()
            .and_then(|o| o.status)
            .and_then(|s| u32::try_from(s).ok());
        let stop = manifest.outcome.as_ref().and_then(|o| o.stop.as_deref());
        let ending = match status.map(ExitStatus::from_raw) {
            Some(ExitStatus::Code(code)) => format!("exited:{code}"),
            Some(ExitStatus::Signal { signo, .. }) => format!("killed:{}", signal_name(signo)),
            None if stop.is_some_and(rewind_trace::stop::timed_out) => {
                rewind_trace::stop::TIMED_OUT_ENDING.to_string()
            }
            None => UNKNOWN_ENDING.to_string(),
        };

        // Failed by a wait status other than 0, or with none by a machine
        // that stopped any way but its guest powering off.
        let failed = match status {
            Some(s) => s != 0,
            None => stop.is_some_and(|stop| !rewind_trace::stop::clean(stop)),
        };
        RunEntry {
            dir: dir.to_path_buf(),
            id: manifest.id.clone().unwrap_or_default(),
            title,
            family,
            parent: manifest.parent.clone(),
            schedule: manifest.schedule.unwrap_or(0),
            ending,
            failed,
            first_difference: manifest.first_difference,
            trace_hash: manifest.trace_hash.clone(),
            cores: manifest.cores.unwrap_or(DEFAULT_CORES),
            window: manifest.window,
            imported: manifest.imported,
            inputs: manifest.inputs.clone(),
            created: manifest.created.unwrap_or(0),
            modified,
        }
    }
}

/// The vCPUs of a run whose manifest does not say, as the engine's
/// default.
const DEFAULT_CORES: u64 = 1;

/// Every run under `runs` with a readable manifest.
pub fn scan(runs: &Path) -> Vec<RunEntry> {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| RunEntry::read(&e.ok()?.path()))
        .collect()
}

/// The runs of one build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    pub runs: Vec<RunEntry>,
}

/// One line of a family's tree: a run, how deep it sits, and the run
/// with the same trace that came before it, if any, with what the graph
/// beside it draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The row's run, or for a row that folds runs, the schedule 0 run
    /// they are under.
    pub run: RunEntry,
    pub depth: usize,
    pub identical_to: Option<String>,
    /// For a schedule 0 run in a family with several, the words that tell
    /// it from the others: its cores, and whether it was imported or how
    /// long ago it was made.
    pub apart: Option<String>,
    pub graph: Graph,
    pub kind: RowKind,
}

/// What a Runs panel row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A run.
    Run,
    /// Stands for `count` runs under the row's run; a click shows them.
    Folded { count: usize, folds: Folds },
    /// Comes before those runs once shown; a click folds them again.
    Unfolded { count: usize, folds: Folds },
}

/// Which runs a folding row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Folds {
    /// Runs from boot under a schedule 0 run that ended the way it did.
    EndedLike,
    /// Runs under a schedule that perturb it only in a window of steps,
    /// as rewind check makes them narrowing the schedule down.
    Windows,
}

/// The fewest runs from boot a row folds: one run is shown as itself.
const MIN_FOLD: usize = 2;

/// What `Family::rows_folded` folds: the schedule 0 runs whose runs from
/// boot are shown, and the runs that stay in view whatever their ending.
struct Fold<'a> {
    unfolded: &'a HashSet<String>,
    keep: &'a [&'a str],
}

/// What makes a placeholder's id, which stands in the tree for a row that
/// folds runs, from the id of the run they are under. No run id has it.
const FOLD_ID_SUFFIX: &str = "/fold";

/// The lanes of the family graph on one row, as ISL and Jujutsu draw a
/// history: each run is a dot in the column of its depth, and its line
/// runs down that column past its forks, each of which curves off it to
/// its own dot one column to the right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    /// For each column left of the parent's, whether a line passes
    /// straight through the row: an ancestor there has forks still to
    /// come below.
    pub through: Vec<bool>,
    /// Whether the run is the last fork of its parent, where the parent's
    /// line ends.
    pub last: bool,
    /// Whether the run has forks below it, so its line goes on down.
    pub has_forks: bool,
}

impl Row {
    /// How the row's run came to be, in words: "recorded" for the first
    /// run, "schedule 3" for one `rewind check` tried, and for a fork the
    /// step it forked at, its schedule, and where it first differs from
    /// its parent or which older fork it repeats.
    pub fn detail(&self) -> String {
        let (id, schedule) = (short_id(&self.run.id), self.run.schedule);
        match self.kind {
            RowKind::Run => {}
            RowKind::Folded {
                count,
                folds: Folds::EndedLike,
            } => return format!("{count} more at boot ended like {id} \u{b7} show"),
            RowKind::Unfolded {
                count,
                folds: Folds::EndedLike,
            } => return format!("hide the {count} at boot that ended like {id}"),
            RowKind::Folded {
                count,
                folds: Folds::Windows,
            } => return format!("{count} more windows of schedule {schedule} \u{b7} show"),
            RowKind::Unfolded {
                count,
                folds: Folds::Windows,
            } => return format!("hide the {count} windows of schedule {schedule}"),
        }
        let run = &self.run;
        let Some(parent) = &run.parent else {
            return match (run.schedule, self.depth) {
                (0, _) => match &self.apart {
                    Some(apart) => format!("schedule 0 \u{b7} {apart}"),
                    None => "schedule 0".to_string(),
                },
                (n, 0) => format!("schedule {n}{}", window_words(run)),
                (n, _) => format!("at boot \u{b7} schedule {n}{}", window_words(run)),
            };
        };
        let mut words = format!(
            "at {} \u{b7} schedule {}",
            thousands(parent.step),
            run.schedule
        );
        if let Some(original) = &self.identical_to {
            words.push_str(&format!(" \u{b7} same as {}", short_id(original)));
        } else if let Some(step) = run.first_difference {
            words.push_str(&format!(" \u{b7} differs at {}", thousands(step)));
        }
        words
    }
}

/// The steps a run's schedule was confined to, as words to append, or
/// nothing for a schedule that ran to the end.
fn window_words(run: &RunEntry) -> String {
    run.window.map_or(String::new(), |(from, until)| {
        format!(
            " \u{b7} steps {}\u{2013}{}",
            thousands(from),
            thousands(until)
        )
    })
}

impl Family {
    /// The run the family is known by: the first recorded one, with no
    /// parent and the unperturbed schedule, else the oldest run without a
    /// parent here, else the oldest run.
    pub fn base(&self) -> &RunEntry {
        let by_id = self.by_id();
        let roots = || self.runs.iter().filter(|r| !Self::has_parent_in(r, &by_id));
        roots()
            .filter(|r| r.schedule == 0)
            .min_by_key(|r| r.created)
            .or_else(|| roots().min_by_key(|r| r.created))
            .unwrap_or(&self.runs[0])
    }

    /// When any run of the family last changed.
    pub fn modified(&self) -> SystemTime {
        self.runs
            .iter()
            .map(|r| r.modified)
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// How many of the runs are forks.
    pub fn forks(&self) -> usize {
        self.runs.iter().filter(|r| r.parent.is_some()).count()
    }

    /// The family in words for its line in the list of recent runs: how
    /// many runs, and how they ended, most common first.
    pub fn summary(&self) -> String {
        let runs = self.runs.len();
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        let mut endings: BTreeMap<&str, usize> = BTreeMap::new();
        for run in &self.runs {
            *endings.entry(run.ending.as_str()).or_default() += 1;
        }
        let mut endings: Vec<(&str, usize)> = endings.into_iter().collect();
        endings.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let endings: Vec<String> = endings
            .iter()
            .map(|(ending, n)| format!("{n} {ending}"))
            .collect();
        format!(
            "{} ({}): {}",
            plural(runs, "run"),
            plural(self.forks(), "fork"),
            endings.join(", ")
        )
    }

    /// The family's runs by id, built once for a pass over every run so
    /// each lookup in it does not walk the runs again: a family of
    /// thousands of runs is drawn every frame the Runs panel is open.
    fn by_id(&self) -> HashMap<&str, &RunEntry> {
        self.runs.iter().map(|r| (r.id.as_str(), r)).collect()
    }

    /// Whether `run`'s parent is among the runs `by_id` holds.
    fn has_parent_in(run: &RunEntry, by_id: &HashMap<&str, &RunEntry>) -> bool {
        run.parent
            .as_ref()
            .is_some_and(|p| by_id.contains_key(p.id.as_str()))
    }

    /// The family as a tree, one row per run in display order: runs
    /// without a parent here first, the base before the schedules after
    /// it, and under each run its forks by the step they forked at. Among
    /// the forks under one run without a parent, those with the same trace
    /// name the oldest of them, which is the one `rewind prune
    /// --identical` keeps.
    pub fn rows(&self) -> Vec<Row> {
        self.rows_at(SystemTime::now())
    }

    /// The rows as the Runs panel draws them: under each schedule 0 run
    /// whose id is not in `unfolded`, its runs from boot that ended as it
    /// did, have no forks and are not in `keep` are one row.
    pub fn rows_folded(&self, unfolded: &HashSet<String>, keep: &[&str]) -> Vec<Row> {
        self.rows_with(SystemTime::now(), Some(Fold { unfolded, keep }))
    }

    /// The rows as `rows` gives them, with how long ago each recorded run
    /// was made counted back from `now`.
    pub fn rows_at(&self, now: SystemTime) -> Vec<Row> {
        self.rows_with(now, None)
    }

    fn rows_with(&self, now: SystemTime, fold: Option<Fold>) -> Vec<Row> {
        let by_id = self.by_id();
        let recorded = self.recorded_by_inputs();

        let hangs = self.boot_hangs(&recorded);

        // The runs each folding row stands for, and a placeholder for the
        // row to take a run's place in the tree. Under a schedule 0 run,
        // the runs from boot that ended as it did fold; under a schedule
        // with windows, the windows fold but for the narrowest that still
        // ends as the schedule did, the one rewind check reports. Runs
        // with forks, or with windows under them, and runs to keep in view
        // stay.
        let parents: HashSet<&str> = self
            .runs
            .iter()
            .filter_map(|r| Some(r.parent.as_ref()?.id.as_str()))
            .chain(
                hangs
                    .values()
                    .filter(|u| u.schedule != 0)
                    .map(|u| u.id.as_str()),
            )
            .collect();
        let narrowest: HashMap<&str, &str> = {
            let mut best: HashMap<&str, &RunEntry> = HashMap::new();
            for run in &self.runs {
                let Some(under) = hangs.get(run.id.as_str()) else {
                    continue;
                };
                let Some((from, until)) = run.window else {
                    continue;
                };
                if under.schedule == 0 || run.ending != under.ending {
                    continue;
                }
                let width = until.saturating_sub(from);
                let slot = best.entry(under.id.as_str()).or_insert(run);
                let held = slot.window.map_or(u64::MAX, |(f, u)| u.saturating_sub(f));
                if (width, std::cmp::Reverse(run.created)) < (held, std::cmp::Reverse(slot.created))
                {
                    *slot = run;
                }
            }
            best.into_iter().map(|(u, r)| (u, r.id.as_str())).collect()
        };
        let mut groups: HashMap<&str, (Folds, Vec<&str>)> = HashMap::new();
        if let Some(fold) = &fold {
            for run in &self.runs {
                let Some(under) = hangs.get(run.id.as_str()) else {
                    continue;
                };
                if parents.contains(run.id.as_str()) || fold.keep.contains(&run.id.as_str()) {
                    continue;
                }
                let folds = if under.schedule == 0 {
                    if run.ending != under.ending {
                        continue;
                    }
                    Folds::EndedLike
                } else {
                    if narrowest.get(under.id.as_str()) == Some(&run.id.as_str()) {
                        continue;
                    }
                    Folds::Windows
                };
                groups
                    .entry(under.id.as_str())
                    .or_insert((folds, Vec::new()))
                    .1
                    .push(&run.id);
            }
        }
        let mut folded: HashSet<&str> = HashSet::new();
        let mut placeholders: Vec<(RunEntry, RowKind)> = Vec::new();
        for (under, (folds, runs)) in groups {
            if runs.len() < MIN_FOLD {
                continue;
            }
            let count = runs.len();
            let kind = if fold.as_ref().is_some_and(|f| f.unfolded.contains(under)) {
                RowKind::Unfolded { count, folds }
            } else {
                folded.extend(runs);
                RowKind::Folded { count, folds }
            };
            let Some(under) = by_id.get(under).copied() else {
                continue;
            };
            let placeholder = RunEntry {
                id: format!("{}{FOLD_ID_SUFFIX}", under.id),
                trace_hash: None,
                ..under.clone()
            };
            placeholders.push((placeholder, kind));
        }

        let mut children: HashMap<&str, Vec<&RunEntry>> = HashMap::new();
        let mut roots: Vec<&RunEntry> = Vec::new();
        for run in self.runs.iter().filter(|r| !folded.contains(r.id.as_str())) {
            match &run.parent {
                Some(p) if Self::has_parent_in(run, &by_id) => {
                    children.entry(p.id.as_str()).or_default().push(run)
                }
                _ => roots.push(run),
            }
        }
        // The runs from boot under other schedules, as rewind check makes
        // them, hang off the schedule 0 run of their inputs like forks at
        // step 0, after its real forks, and the windows of a schedule hang
        // off that schedule's run.
        let (from_boot, rest): (Vec<&RunEntry>, Vec<&RunEntry>) = roots
            .into_iter()
            .partition(|r| hangs.contains_key(r.id.as_str()));
        roots = rest;
        for run in from_boot {
            let Some(under) = hangs.get(run.id.as_str()) else {
                continue;
            };
            children.entry(under.id.as_str()).or_default().push(run);
        }
        let base_id = self.base().id.clone();
        roots.sort_by_key(|r| (r.id != base_id, r.schedule, r.created));
        for list in children.values_mut() {
            list.sort_by_key(|r| {
                (
                    r.parent.is_none(),
                    r.parent.as_ref().map_or(0, |p| p.step),
                    r.schedule,
                    r.created,
                )
            });
        }

        // Each folding row comes first among the runs from boot under the
        // run its runs are under, after the real forks, so it stays where
        // it is as it opens and closes.
        let mut stands_for: HashMap<&str, (&RunEntry, RowKind)> = HashMap::new();
        for (placeholder, kind) in &placeholders {
            let Some(under) = placeholder
                .id
                .strip_suffix(FOLD_ID_SUFFIX)
                .and_then(|id| by_id.get(id).copied())
            else {
                continue;
            };
            let list = children.entry(under.id.as_str()).or_default();
            let at = list
                .iter()
                .position(|r| r.parent.is_none())
                .unwrap_or(list.len());
            list.insert(at, placeholder);
            stands_for.insert(placeholder.id.as_str(), (under, *kind));
        }

        // Depth first, each row tagged with the root it is under and its
        // place in the graph. A fork's columns left of its parent's are
        // its parent's, plus the parent's own column, which goes on down
        // while the parent has later siblings.
        let mut order: Vec<(&RunEntry, usize, usize, Graph)> = Vec::with_capacity(self.runs.len());
        for (root_index, root) in roots.into_iter().enumerate() {
            let mut stack = vec![(root, 0, Graph::default())];
            while let Some((run, depth, mut graph)) = stack.pop() {
                let forks = children.get(run.id.as_str());
                graph.has_forks = forks.is_some_and(|f| !f.is_empty());
                if let Some(list) = forks {
                    let mut through = graph.through.clone();
                    if depth > 0 {
                        through.push(!graph.last);
                    }
                    let last = list.len() - 1;
                    stack.extend(list.iter().enumerate().rev().map(|(i, c)| {
                        let fork_graph = Graph {
                            through: through.clone(),
                            last: i == last,
                            has_forks: false,
                        };
                        (*c, depth + 1, fork_graph)
                    }));
                }
                order.push((run, depth, root_index, graph));
            }
        }

        // Forks repeat a trace within what rewind prune takes in: a run
        // without a parent here and every fork below it through parents,
        // which leaves out the runs from boot drawn under the recorded run.
        // There the original of a trace is that run when it has the trace,
        // as prune keeps the run it is given, and the oldest fork otherwise.
        let mut originals: HashMap<(&str, &str), (&RunEntry, bool)> = HashMap::new();
        for (run, _, _, _) in &order {
            let Some(hash) = &run.trace_hash else {
                continue;
            };
            let top = Self::chain_root_in(run, &by_id);
            let is_top = top.id == run.id;
            let original = originals
                .entry((top.id.as_str(), hash.as_str()))
                .or_insert((run, is_top));
            let older = (run.created, &run.id) < (original.0.created, &original.0.id);
            if is_top || (!original.1 && older) {
                *original = (run, is_top);
            }
        }

        // With more than one recorded run, each says what tells it apart.
        let is_recorded = |r: &RunEntry| r.parent.is_none() && r.schedule == 0;
        let several_recorded = self.runs.iter().filter(|r| is_recorded(r)).count() > 1;
        let apart = |r: &RunEntry| {
            if !several_recorded || !is_recorded(r) {
                return None;
            }
            let cores = match r.cores {
                1 => "1 vCPU".to_string(),
                n => format!("{n} vCPUs"),
            };
            if r.imported {
                return Some(format!("{cores} \u{b7} imported"));
            }
            let made = SystemTime::UNIX_EPOCH + Duration::from_secs(r.created);
            let elapsed = now.duration_since(made).unwrap_or_default();
            Some(format!("{cores} \u{b7} {}", ago(elapsed)))
        };

        order
            .into_iter()
            .map(|(run, depth, _, graph)| {
                if let Some((under, kind)) = stands_for.get(run.id.as_str()) {
                    return Row {
                        run: (*under).clone(),
                        depth,
                        identical_to: None,
                        apart: None,
                        graph,
                        kind: *kind,
                    };
                }
                let top = Self::chain_root_in(run, &by_id).id.as_str();
                let identical_to = match (run.parent.is_some(), &run.trace_hash) {
                    (true, Some(hash)) => originals
                        .get(&(top, hash.as_str()))
                        .filter(|(original, _)| original.id != run.id)
                        .map(|(original, _)| original.id.clone()),
                    _ => None,
                };
                Row {
                    run: run.clone(),
                    depth,
                    identical_to,
                    apart: apart(run),
                    graph,
                    kind: RowKind::Run,
                }
            })
            .collect()
    }

    /// The run `run` hangs under in the tree, which opening it compares it
    /// with: a fork's parent, or for a run from boot under another
    /// schedule the schedule 0 run of its inputs.
    pub fn tree_parent(&self, run: &RunEntry) -> Option<&RunEntry> {
        if let Some(parent) = &run.parent {
            return self.runs.iter().find(|r| r.id == parent.id);
        }
        Self::from_boot_under(run, &self.recorded_by_inputs())
    }

    /// The run whose folding row `run` goes with: the run itself when it
    /// is a schedule 0 run, or the run a run from boot hangs under.
    pub fn fold_under<'a>(&'a self, run: &'a RunEntry) -> Option<&'a RunEntry> {
        if run.parent.is_none() && run.schedule == 0 {
            return Some(run);
        }
        self.boot_hangs(&self.recorded_by_inputs())
            .get(run.id.as_str())
            .copied()
    }

    /// For each run from boot under another schedule, the run it hangs
    /// under in the tree: for one perturbed only in a window, the run of
    /// its schedule without one when that is here, else the schedule 0
    /// run of its inputs.
    fn boot_hangs<'a>(
        &'a self,
        recorded: &HashMap<&str, &'a RunEntry>,
    ) -> HashMap<&'a str, &'a RunEntry> {
        let whole: HashMap<(&str, u64), &RunEntry> = self
            .runs
            .iter()
            .filter(|r| r.window.is_none() && Self::from_boot_under(r, recorded).is_some())
            .filter_map(|r| Some(((r.inputs.as_deref()?, r.schedule), r)))
            .collect();
        self.runs
            .iter()
            .filter_map(|run| {
                let base = Self::from_boot_under(run, recorded)?;
                let schedule = run
                    .window
                    .and(run.inputs.as_deref())
                    .and_then(|i| whole.get(&(i, run.schedule)).copied());
                Some((run.id.as_str(), schedule.unwrap_or(base)))
            })
            .collect()
    }

    /// The folding row among `rows` for the runs under `under`, if it has
    /// one.
    pub fn fold_row<'a>(rows: &'a [Row], under: &RunEntry) -> Option<&'a Row> {
        rows.iter()
            .find(|r| r.kind != RowKind::Run && r.run.id == under.id)
    }

    /// The runs from boot under schedule 0 by their inputs. A family can
    /// hold several, one for each machine or build of the inputs, and
    /// each is where the runs of its inputs under other schedules start.
    fn recorded_by_inputs(&self) -> HashMap<&str, &RunEntry> {
        self.runs
            .iter()
            .filter(|r| r.parent.is_none() && r.schedule == 0)
            .filter_map(|r| Some((r.inputs.as_deref()?, r)))
            .collect()
    }

    /// For a run from boot under another schedule, the schedule 0 run of
    /// its inputs in `recorded`.
    fn from_boot_under<'a>(
        run: &RunEntry,
        recorded: &HashMap<&str, &'a RunEntry>,
    ) -> Option<&'a RunEntry> {
        if run.parent.is_some() || run.schedule == 0 {
            return None;
        }
        recorded.get(run.inputs.as_deref()?).copied()
    }

    /// The runs without a parent here that have identical forks under
    /// them, for `rewind prune --identical`.
    pub fn roots_with_identical(&self) -> Vec<RunEntry> {
        let by_id = self.by_id();
        let mut roots: Vec<RunEntry> = Vec::new();
        for row in self.rows() {
            if row.identical_to.is_none() {
                continue;
            }
            let top = Self::chain_root_in(&row.run, &by_id);
            if !roots.iter().any(|r| r.id == top.id) {
                roots.push(top.clone());
            }
        }
        roots
    }

    /// The run `run` descends from through parents among the runs `by_id`
    /// holds, with no parent there itself: what rewind prune is given to
    /// reach `run`.
    fn chain_root_in<'a>(run: &'a RunEntry, by_id: &HashMap<&str, &'a RunEntry>) -> &'a RunEntry {
        let mut at = run;
        while let Some(parent) = at.parent.as_ref().and_then(|p| by_id.get(p.id.as_str())) {
            at = parent;
        }
        at
    }

    /// Every run descended from the run `id` through its forks, nearest
    /// first: what removing it removes along with it.
    pub fn descendants(&self, id: &str) -> Vec<&RunEntry> {
        let mut found: Vec<&RunEntry> = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(parent) = frontier.pop() {
            for run in &self.runs {
                if run.parent.as_ref().is_some_and(|p| p.id == parent) {
                    found.push(run);
                    frontier.push(run.id.clone());
                }
            }
        }
        found
    }

    /// How many forks repeat the trace of a run listed before them, and
    /// so add nothing to the family.
    pub fn identical(&self) -> usize {
        self.rows()
            .iter()
            .filter(|r| r.identical_to.is_some())
            .count()
    }
}

/// `runs` grouped into families, the one changed most recently first.
pub fn families(runs: Vec<RunEntry>) -> Vec<Family> {
    let mut by_key: HashMap<String, Vec<RunEntry>> = HashMap::new();
    for run in runs {
        by_key.entry(run.family.clone()).or_default().push(run);
    }
    let mut families: Vec<Family> = by_key.into_values().map(|runs| Family { runs }).collect();
    families.sort_by_key(|f| std::cmp::Reverse(f.modified()));
    families
}

/// The family `id` belongs to, among `runs`.
pub fn family_of(runs: Vec<RunEntry>, id: &str) -> Option<Family> {
    let key = runs.iter().find(|r| r.id == id)?.family.clone();
    let runs = runs.into_iter().filter(|r| r.family == key).collect();
    Some(Family { runs })
}

#[cfg(test)]
mod tests {
    // Families built from runs described in memory: a base, a schedule
    // check tried, forks, a fork of a fork, an identical fork, and a run
    // of another build.
    use super::*;
    use std::time::Duration;

    const DRV: &str = "/nix/store/x-mylib.drv";
    const INPUTS: &str = "one core";

    fn run(id: &str, parent: Option<(&str, u64)>, schedule: u64, ending: &str) -> RunEntry {
        RunEntry {
            dir: PathBuf::from(format!("/runs/{id}")),
            id: id.to_string(),
            title: "mylib".to_string(),
            family: DRV.to_string(),
            parent: parent.map(|(id, step)| Parent {
                id: id.to_string(),
                step,
            }),
            schedule,
            ending: ending.to_string(),
            failed: ending != "exited:0",
            first_difference: None,
            trace_hash: Some(format!("hash-{id}")),
            cores: DEFAULT_CORES,
            window: None,
            imported: false,
            inputs: Some(INPUTS.to_string()),
            created: 0,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn family() -> Family {
        // dup repeats f1's trace and was made after it.
        let mut same = run("dup", Some(("base", 50)), 2, "exited:2");
        same.trace_hash = Some("hash-f1".to_string());
        same.created = 2;
        let mut first = run("f1", Some(("base", 50)), 1, "exited:2");
        first.created = 1;
        Family {
            runs: vec![
                run("f2", Some(("base", 90)), 1, "exited:0"),
                run("check3", None, 3, "exited:2"),
                run("base", None, 0, "exited:2"),
                first,
                run("f1a", Some(("f1", 70)), 1, "exited:0"),
                same,
            ],
        }
    }

    #[test]
    fn the_tree_puts_the_base_first_and_forks_under_their_parents() {
        let rows: Vec<(String, usize, Option<String>)> = family()
            .rows()
            .into_iter()
            .map(|r| (r.run.id, r.depth, r.identical_to))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("base".to_string(), 0, None),
                ("f1".to_string(), 1, None),
                ("f1a".to_string(), 2, None),
                ("dup".to_string(), 1, Some("f1".to_string())),
                ("f2".to_string(), 1, None),
                ("check3".to_string(), 1, None),
            ]
        );
    }

    #[test]
    fn the_graph_carries_each_line_down_to_its_last_fork() {
        // base
        // ├─ f1
        // │  ╰─ f1a      base's line passes f1a; f1's ends there
        // ├─ dup
        // ├─ f2
        // ╰─ check3      from boot, after the forks; base's line ends here
        let graphs: Vec<(String, Graph)> = family()
            .rows()
            .into_iter()
            .map(|r| (r.run.id, r.graph))
            .collect();
        let graph = |through: &[bool], last: bool, has_forks: bool| Graph {
            through: through.to_vec(),
            last,
            has_forks,
        };
        assert_eq!(
            graphs,
            vec![
                ("base".to_string(), graph(&[], false, true)),
                ("f1".to_string(), graph(&[], false, true)),
                ("f1a".to_string(), graph(&[true], true, false)),
                ("dup".to_string(), graph(&[], false, false)),
                ("f2".to_string(), graph(&[], false, false)),
                ("check3".to_string(), graph(&[], true, false)),
            ]
        );
    }

    #[test]
    fn a_run_stopped_at_its_time_limit_is_listed_as_failed() {
        // A manifest with no wait status whose machine was stopped at its
        // time limit: the start screen and the Runs panel call it
        // timed-out and failed, as the header does, not unknown.
        let manifest = Manifest::from_json(&serde_json::json!({
            "id": "abc",
            "outcome": { "stop": "timed out while still making exits" }
        }));
        let entry =
            RunEntry::from_manifest(Path::new("/runs/abc"), &manifest, SystemTime::UNIX_EPOCH);
        assert_eq!(entry.ending, "timed-out");
        assert!(entry.failed);
    }

    #[test]
    fn a_family_is_summed_up_by_its_endings() {
        let f = family();
        assert_eq!(f.base().id, "base");
        assert_eq!(f.forks(), 4);
        assert_eq!(f.identical(), 1);
        let roots: Vec<String> = f.roots_with_identical().into_iter().map(|r| r.id).collect();
        assert_eq!(roots, vec!["base"]);
        assert_eq!(f.summary(), "6 runs (4 forks): 4 exited:2, 2 exited:0");
    }

    #[test]
    fn runs_group_by_build_with_the_newest_family_first() {
        let mut other = run("other", None, 0, "exited:0");
        other.family = "/nix/store/y-hello.drv".to_string();
        other.modified = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let mut runs = family().runs;
        runs.push(other);
        let grouped = families(runs.clone());
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].runs[0].id, "other");
        assert_eq!(grouped[1].runs.len(), 6);
        assert_eq!(family_of(runs, "f1a").unwrap().runs.len(), 6);
    }

    #[test]
    fn each_row_says_how_its_run_came_to_be() {
        let rows = family().rows();
        let detail = |id: &str| {
            rows.iter()
                .find(|r| r.run.id == id)
                .map(Row::detail)
                .unwrap()
        };
        assert_eq!(detail("base"), "schedule 0");
        assert_eq!(detail("check3"), "at boot \u{b7} schedule 3");
        assert_eq!(detail("dup"), "at 50 \u{b7} schedule 2 \u{b7} same as f1");
        let mut differs = rows[1].clone();
        differs.run.first_difference = Some(1_204);
        assert_eq!(
            differs.detail(),
            "at 50 \u{b7} schedule 1 \u{b7} differs at 1,204"
        );
    }

    #[test]
    fn a_fork_that_repeats_its_root_is_the_same_as_the_root() {
        // As rewind prune keeps the run it is given: a fork with the
        // base's trace is a copy of the base, even made before other forks.
        let mut copy = run("copy", Some(("base", 10)), 5, "exited:2");
        copy.trace_hash = Some("hash-base".to_string());
        let mut f = family();
        f.runs.push(copy);
        let rows = f.rows();
        let copy_row = rows.iter().find(|r| r.run.id == "copy").unwrap();
        assert_eq!(copy_row.identical_to.as_deref(), Some("base"));
        assert!(
            rows.iter()
                .find(|r| r.run.id == "base")
                .unwrap()
                .identical_to
                .is_none()
        );
        assert_eq!(f.identical(), 2);
    }

    #[test]
    fn a_run_s_descendants_are_its_forks_and_theirs() {
        let f = family();
        let ids = |id: &str| {
            let mut ids: Vec<&str> = f.descendants(id).iter().map(|r| r.id.as_str()).collect();
            ids.sort();
            ids
        };
        assert_eq!(ids("f1"), vec!["f1a"]);
        assert_eq!(ids("f1a"), Vec::<&str>::new());
        assert_eq!(ids("base"), vec!["dup", "f1", "f1a", "f2"]);
    }

    #[test]
    fn forks_under_a_run_from_boot_are_compared_among_themselves() {
        // A fork of check3 with f1's trace is not the same as f1: pruning
        // base does not reach it, and pruning check3 keeps it.
        let mut cousin = run("cousin", Some(("check3", 40)), 1, "exited:2");
        cousin.trace_hash = Some("hash-f1".to_string());
        let mut f = family();
        f.runs.push(cousin);
        let rows = f.rows();
        let row = rows.iter().find(|r| r.run.id == "cousin").unwrap();
        assert_eq!(row.identical_to, None);
        assert_eq!(row.depth, 2);
    }

    #[test]
    fn runs_from_boot_hang_off_the_recorded_run_of_their_inputs() {
        // The build recorded on one core and on four, with rewind check
        // run on each: every schedule goes under the recorded run it
        // perturbs, and a schedule whose recorded run is gone stays a root.
        let on = |mut r: RunEntry, inputs: &str| {
            r.inputs = Some(inputs.to_string());
            r
        };
        let f = Family {
            runs: vec![
                run("one", None, 0, "exited:101"),
                on(run("four", None, 0, "exited:0"), "four cores"),
                run("one-s1", None, 1, "exited:101"),
                on(run("four-s1", None, 1, "exited:0"), "four cores"),
                on(run("lone-s1", None, 1, "exited:0"), "two cores"),
            ],
        };
        let rows: Vec<(String, usize)> =
            f.rows().into_iter().map(|r| (r.run.id, r.depth)).collect();
        assert_eq!(
            rows,
            vec![
                ("one".to_string(), 0),
                ("one-s1".to_string(), 1),
                ("four".to_string(), 0),
                ("four-s1".to_string(), 1),
                ("lone-s1".to_string(), 0),
            ]
        );
    }

    #[test]
    fn each_run_is_compared_with_the_run_it_hangs_under() {
        // A fork with its parent, a run from boot with the schedule 0
        // run of its inputs, and a schedule 0 run with nothing.
        let f = family();
        let above = |id: &str| {
            let run = f.runs.iter().find(|r| r.id == id).unwrap();
            f.tree_parent(run).map(|r| r.id.clone())
        };
        assert_eq!(above("f1a"), Some("f1".to_string()));
        assert_eq!(above("check3"), Some("base".to_string()));
        assert_eq!(above("base"), None);
    }

    #[test]
    fn several_recorded_runs_say_what_tells_them_apart() {
        // A run imported from another machine and two recorded here, on
        // one core and on four: each says its cores, and where it came
        // from or when it was made. A family with one schedule 0 run says
        // only that.
        const DAY: u64 = 86_400;
        let made = |mut r: RunEntry, cores: u64, created: u64| {
            r.cores = cores;
            r.created = created;
            r
        };
        let f = Family {
            runs: vec![
                RunEntry {
                    imported: true,
                    ..made(run("old", None, 0, "exited:101"), 1, 0)
                },
                made(run("one", None, 0, "exited:101"), 1, DAY),
                made(run("four", None, 0, "exited:0"), 4, DAY),
            ],
        };
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3 * DAY);
        let rows = f.rows_at(now);
        let detail = |id: &str| rows.iter().find(|r| r.run.id == id).unwrap().detail();
        assert_eq!(detail("old"), "schedule 0 \u{b7} 1 vCPU \u{b7} imported");
        assert_eq!(detail("one"), "schedule 0 \u{b7} 1 vCPU \u{b7} 2d ago");
        assert_eq!(detail("four"), "schedule 0 \u{b7} 4 vCPUs \u{b7} 2d ago");
        assert_eq!(family().rows_at(now)[0].detail(), "schedule 0");
    }

    /// A schedule 0 run that failed with a fork, and rewind check's runs
    /// of it: three that failed the same way, one of them with a fork,
    /// and one that passed.
    fn swept() -> Family {
        Family {
            runs: vec![
                run("r", None, 0, "exited:101"),
                run("f", Some(("r", 40)), 1, "exited:101"),
                run("s1", None, 1, "exited:101"),
                run("s2", None, 2, "exited:101"),
                run("s2a", Some(("s2", 30)), 1, "exited:101"),
                run("s3", None, 3, "exited:101"),
                run("s4", None, 4, "exited:0"),
            ],
        }
    }

    fn shown(rows: &[Row]) -> Vec<(String, usize, RowKind)> {
        rows.iter()
            .map(|r| (r.run.id.clone(), r.depth, r.kind))
            .collect()
    }

    #[test]
    fn a_run_perturbed_in_a_window_says_its_steps() {
        // rewind check narrows a schedule to the steps it perturbs: the
        // runs it makes share a seed and differ in their window.
        let mut narrowed = swept();
        narrowed.runs[2].window = Some((501, 1_432));
        let rows = narrowed.rows();
        let row = rows.iter().find(|r| r.run.id == "s1").unwrap();
        assert_eq!(
            row.detail(),
            "at boot \u{b7} schedule 1 \u{b7} steps 501\u{2013}1,432"
        );
    }

    #[test]
    fn runs_from_boot_that_end_like_their_run_fold_into_one_row() {
        // Folded, s1 and s3 become one row after r's fork and before the
        // runs from boot that stay: s2 with its fork, and s4, which ended
        // differently. The folded row stands for r, whose runs it holds.
        let rows = swept().rows_folded(&HashSet::new(), &[]);
        let row = |id: &str, depth| (id.to_string(), depth, RowKind::Run);
        assert_eq!(
            shown(&rows),
            vec![
                row("r", 0),
                row("f", 1),
                (
                    "r".to_string(),
                    1,
                    RowKind::Folded {
                        count: 2,
                        folds: Folds::EndedLike
                    }
                ),
                row("s2", 1),
                row("s2a", 2),
                row("s4", 1),
            ]
        );
        assert_eq!(rows[2].detail(), "2 more at boot ended like r \u{b7} show");
        assert!(rows[5].graph.last);

        // Unfolded, the row stays where it was, to fold them again, and
        // every run follows it.
        let open = HashSet::from(["r".to_string()]);
        let rows = swept().rows_folded(&open, &[]);
        assert_eq!(rows.len(), 8);
        assert_eq!(
            rows[2].kind,
            RowKind::Unfolded {
                count: 2,
                folds: Folds::EndedLike
            }
        );
        assert_eq!(rows[2].detail(), "hide the 2 at boot that ended like r");
        assert_eq!(rows[3].run.id, "s1");
    }

    #[test]
    fn a_run_knows_the_folding_row_it_belongs_with() {
        // The schedule 0 run and its runs from boot answer with r's row;
        // a fork answers with nothing.
        let f = swept();
        let rows = f.rows_folded(&HashSet::new(), &[]);
        let fold = |id: &str| {
            let run = f.runs.iter().find(|r| r.id == id).unwrap();
            Family::fold_row(&rows, f.fold_under(run)?).map(|row| row.kind)
        };
        let folded = Some(RowKind::Folded {
            count: 2,
            folds: Folds::EndedLike,
        });
        assert_eq!(fold("r"), folded);
        assert_eq!(fold("s4"), folded);
        assert_eq!(fold("f"), None);
    }

    /// A passing schedule 0 run, two runs from boot that passed like it,
    /// schedule 5 that failed, and four runs rewind check made narrowing
    /// schedule 5 to a window: w4 is the narrowest that still fails.
    fn narrowed() -> Family {
        let within = |id: &str, ending: &str, from: u64, until: u64, created: u64| RunEntry {
            window: Some((from, until)),
            created,
            ..run(id, None, 5, ending)
        };
        Family {
            runs: vec![
                run("r", None, 0, "exited:0"),
                run("s5", None, 5, "exited:1"),
                run("s6", None, 6, "exited:0"),
                run("s7", None, 7, "exited:0"),
                within("w1", "exited:1", 10, 100, 1),
                within("w2", "exited:0", 50, 100, 2),
                within("w3", "exited:1", 40, 100, 3),
                within("w4", "exited:1", 40, 60, 4),
            ],
        }
    }

    #[test]
    fn a_schedules_windows_hang_under_it_and_fold() {
        // The windows go under s5, the schedule they narrow, and fold but
        // for w4, the narrowest that still ends like s5. Opening a window
        // still compares it with r, where its schedule is perturbed from.
        let f = narrowed();
        let rows = f.rows_folded(&HashSet::new(), &[]);
        let ended_like = |count| RowKind::Folded {
            count,
            folds: Folds::EndedLike,
        };
        let windows = |count| RowKind::Folded {
            count,
            folds: Folds::Windows,
        };
        let row = |id: &str, depth| (id.to_string(), depth, RowKind::Run);
        assert_eq!(
            shown(&rows),
            vec![
                row("r", 0),
                ("r".to_string(), 1, ended_like(2)),
                row("s5", 1),
                ("s5".to_string(), 2, windows(3)),
                row("w4", 2),
            ]
        );
        assert_eq!(rows[3].detail(), "3 more windows of schedule 5 \u{b7} show");
        let w1 = f.runs.iter().find(|r| r.id == "w1").unwrap();
        assert_eq!(f.tree_parent(w1).map(|r| r.id.as_str()), Some("r"));

        // Unfolded, every window is there after the row that folds them.
        let open = HashSet::from(["s5".to_string()]);
        let rows = f.rows_folded(&open, &[]);
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[3].detail(), "hide the 3 windows of schedule 5");
    }

    #[test]
    fn a_run_on_screen_stays_and_one_run_is_not_folded() {
        // With s3 on screen only s1 would fold, and one run is shown as
        // itself rather than behind a row of its own.
        let rows = swept().rows_folded(&HashSet::new(), &["s3"]);
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|r| r.kind == RowKind::Run));
    }

    #[test]
    fn a_fork_whose_parent_is_gone_is_a_root() {
        let f = Family {
            runs: vec![run("orphan", Some(("deleted", 10)), 1, "exited:2")],
        };
        assert_eq!(f.base().id, "orphan");
        assert_eq!(f.rows()[0].depth, 0);
    }
}
