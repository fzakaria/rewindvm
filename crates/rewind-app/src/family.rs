//! Runs grouped by the build they ran: a family is every run of one
//! derivation, or of one command on one root image. It holds the run as
//! first recorded, the schedules `rewind check` tried, and every fork of
//! any of them, so a thousand forks are one family, not a thousand runs.
//!
//! The engine's runs are read from their manifests alone, without their
//! traces, so a whole home of runs is quick to list.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rewind_trace::signal_name;

use crate::describe::thousands;
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
        let ending = match status.map(ExitStatus::from_raw) {
            Some(ExitStatus::Code(code)) => format!("exited:{code}"),
            Some(ExitStatus::Signal { signo, .. }) => format!("killed:{}", signal_name(signo)),
            None => UNKNOWN_ENDING.to_string(),
        };
        RunEntry {
            dir: dir.to_path_buf(),
            id: manifest.id.clone().unwrap_or_default(),
            title,
            family,
            parent: manifest.parent.clone(),
            schedule: manifest.schedule.unwrap_or(0),
            ending,
            failed: status.is_some_and(|s| s != 0),
            first_difference: manifest.first_difference,
            trace_hash: manifest.trace_hash.clone(),
            created: manifest.created.unwrap_or(0),
            modified,
        }
    }
}

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
    pub run: RunEntry,
    pub depth: usize,
    pub identical_to: Option<String>,
    pub graph: Graph,
}

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
        let run = &self.run;
        let Some(parent) = &run.parent else {
            return match run.schedule {
                0 => "recorded".to_string(),
                n => format!("schedule {n}"),
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

impl Row {
    /// The row's run in a sentence, for its hover note: what a fork or a
    /// schedule is, in this run's numbers.
    pub fn explanation(&self) -> String {
        let run = &self.run;
        let Some(parent) = &run.parent else {
            return match run.schedule {
                0 => "The run as it was first recorded, under the unperturbed thread schedule."
                    .to_string(),
                n => format!(
                    "The same inputs from boot, with the thread schedule perturbed by seed {n}, as rewind check tries them."
                ),
            };
        };
        format!(
            "A fork of {}: the same run up to step {}, then the thread schedule perturbed from there by seed {}, so the threads interleave another way. The same step and seed always make the same fork. Right-click for more.",
            short_id(&parent.id),
            thousands(parent.step),
            run.schedule
        )
    }
}

impl Family {
    /// The run the family is known by: the first recorded one, with no
    /// parent and the unperturbed schedule, else the oldest run without a
    /// parent here, else the oldest run.
    pub fn base(&self) -> &RunEntry {
        let roots = || self.runs.iter().filter(|r| !self.has_parent_here(r));
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

    fn has_parent_here(&self, run: &RunEntry) -> bool {
        run.parent
            .as_ref()
            .is_some_and(|p| self.runs.iter().any(|r| r.id == p.id))
    }

    /// The family as a tree, one row per run in display order: runs
    /// without a parent here first, the base before the schedules after
    /// it, and under each run its forks by the step they forked at. Among
    /// the forks under one run without a parent, those with the same trace
    /// name the oldest of them, which is the one `rewind prune
    /// --identical` keeps.
    pub fn rows(&self) -> Vec<Row> {
        let mut children: HashMap<&str, Vec<&RunEntry>> = HashMap::new();
        let mut roots: Vec<&RunEntry> = Vec::new();
        for run in &self.runs {
            match &run.parent {
                Some(p) if self.has_parent_here(run) => {
                    children.entry(p.id.as_str()).or_default().push(run)
                }
                _ => roots.push(run),
            }
        }
        let base_id = self.base().id.clone();
        roots.sort_by_key(|r| (r.id != base_id, r.schedule, r.created));
        for list in children.values_mut() {
            list.sort_by_key(|r| {
                (
                    r.parent.as_ref().map_or(0, |p| p.step),
                    r.schedule,
                    r.created,
                )
            });
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

        // Under each root, the original of a trace is the root when the
        // root has it, as rewind prune keeps the run it is given, and the
        // oldest fork with it otherwise.
        let mut originals: HashMap<(usize, &str), (&RunEntry, bool)> = HashMap::new();
        for (run, depth, root, _) in &order {
            let Some(hash) = &run.trace_hash else {
                continue;
            };
            let is_root = *depth == 0;
            let original = originals
                .entry((*root, hash.as_str()))
                .or_insert((run, is_root));
            let older = (run.created, &run.id) < (original.0.created, &original.0.id);
            if is_root || (!original.1 && older) {
                *original = (run, is_root);
            }
        }

        order
            .into_iter()
            .map(|(run, depth, root, graph)| {
                let identical_to = match (depth > 0, &run.trace_hash) {
                    (true, Some(hash)) => originals
                        .get(&(root, hash.as_str()))
                        .filter(|(original, _)| original.id != run.id)
                        .map(|(original, _)| original.id.clone()),
                    _ => None,
                };
                Row {
                    run: run.clone(),
                    depth,
                    identical_to,
                    graph,
                }
            })
            .collect()
    }

    /// The runs without a parent here that have identical forks under
    /// them, for `rewind prune --identical`.
    pub fn roots_with_identical(&self) -> Vec<RunEntry> {
        let mut roots: Vec<RunEntry> = Vec::new();
        let mut current: Option<&RunEntry> = None;
        let rows = self.rows();
        for row in &rows {
            if row.depth == 0 {
                current = Some(&row.run);
            }
            if let (Some(_), Some(root)) = (&row.identical_to, current)
                && !roots.iter().any(|r| r.id == root.id)
            {
                roots.push(root.clone());
            }
        }
        roots
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
                ("check3".to_string(), 0, None),
            ]
        );
    }

    #[test]
    fn the_graph_carries_each_line_down_to_its_last_fork() {
        // base
        // ├─ f1
        // │  ╰─ f1a      base's line passes f1a; f1's ends there
        // ├─ dup
        // ╰─ f2          base's line ends here
        // check3
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
                ("f2".to_string(), graph(&[], true, false)),
                ("check3".to_string(), graph(&[], false, false)),
            ]
        );
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
        assert_eq!(detail("base"), "recorded");
        assert_eq!(detail("check3"), "schedule 3");
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
    fn a_fork_whose_parent_is_gone_is_a_root() {
        let f = Family {
            runs: vec![run("orphan", Some(("deleted", 10)), 1, "exited:2")],
        };
        assert_eq!(f.base().id, "orphan");
        assert_eq!(f.rows()[0].depth, 0);
    }
}
