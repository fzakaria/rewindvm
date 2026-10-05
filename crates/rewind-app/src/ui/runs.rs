//! The Runs panel's state: the family of the run on screen, the runs
//! picked in it, which folding rows are open, the comparison chosen from
//! it, and its rows laid out once per change. Nothing here draws or needs
//! a window, so the panel's decisions are tested without one.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::UniformListScrollHandle;

use crate::family::{Family, Row, RowKind, RunEntry};
use crate::memo::Memo;
use crate::run::short_id;

/// How a press on a row picks runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Shift: every run from the last one picked.
    Range,
    /// Ctrl: the run joins the pick, or leaves it.
    Toggle,
    /// No modifier: the pick is dropped and the press opens the run.
    Open,
}

/// What deleting picked runs takes, and whether to ask first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deletion {
    /// The directories of the picked runs no other picked run is above,
    /// which the engine removes with their forks.
    pub dirs: Vec<PathBuf>,
    /// Every run that goes, forks included.
    pub runs: usize,
    /// The question to ask, or None when the deletion goes at once: a
    /// single fork with no forks of its own, which the same step and seed
    /// make again.
    pub question: Option<String>,
}

#[derive(Default)]
pub struct RunsPanel {
    /// The family of the run on screen: every run of its build, its forks
    /// among them.
    pub family: Option<Family>,
    /// Runs picked with Ctrl and Shift clicks, by id, for copying their
    /// ids or deleting them together.
    pub picked: Vec<String>,
    /// The row a Shift click picks from.
    pub anchor: Option<usize>,
    /// The run chosen with Compare against this run, which every run
    /// opened from the Runs panel is compared with until the choice is
    /// dropped. Without one, a run is compared with its parent.
    pub pinned_compare: Option<PathBuf>,
    pub scroll: UniformListScrollHandle,
    /// Whether identical forks are being removed.
    pub pruning: bool,
    /// The schedule 0 runs whose runs from boot that ended as they did
    /// the Runs panel shows one by one instead of folded into one row.
    unfolded: HashSet<String>,
    /// Counts changes to `family` and `unfolded`, which the rows are laid
    /// out from: whatever changes either bumps it.
    changed: u64,
    /// The rows, laid out again only when `changed` or the runs on screen
    /// change, rather than every frame.
    rows_memo: Memo<(u64, String, String), Vec<Row>>,
    /// How many forks repeat an older fork's trace, likewise.
    identical_memo: Memo<u64, usize>,
}

impl RunsPanel {
    /// Takes the family of the run on screen, or none.
    pub fn set_family(&mut self, family: Option<Family>) {
        self.family = family;
        self.changed += 1;
    }

    /// The rows as the panel draws them, folded as the user left them.
    /// `shown` and `compared`, the ids of the runs on screen, always have
    /// rows of their own.
    pub fn rows(&self, shown: &str, compared: &str) -> Rc<Vec<Row>> {
        let key = (self.changed, shown.to_string(), compared.to_string());
        self.rows_memo.get(key, || match &self.family {
            Some(family) => family.rows_folded(&self.unfolded, &[shown, compared]),
            None => Vec::new(),
        })
    }

    /// How many forks of the family repeat an older fork's trace.
    pub fn identical(&self) -> usize {
        *self.identical_memo.get(self.changed, || {
            self.family.as_ref().map_or(0, Family::identical)
        })
    }

    /// The folding row that row `index` of `rows` goes with, as the id of
    /// the schedule 0 run it is under and whether it is folded: the row
    /// itself, or for a run the row of its schedule 0 run.
    pub fn fold_for_row(&self, rows: &[Row], index: usize) -> Option<(String, RowKind)> {
        let family = self.family.as_ref()?;
        let row = rows.get(index)?;
        if row.kind != RowKind::Run {
            return Some((row.run.id.clone(), row.kind));
        }
        // A run with windows folded under it answers for those first.
        let fold = Family::fold_row(rows, &row.run).or_else(|| {
            let under = family.fold_under(&row.run)?;
            Family::fold_row(rows, under)
        })?;
        Some((fold.run.id.clone(), fold.kind))
    }

    /// Shows the runs a folding row stands for, or folds them again.
    pub fn toggle_fold(&mut self, under: &str) {
        if !self.unfolded.remove(under) {
            self.unfolded.insert(under.to_string());
        }
        self.changed += 1;
    }

    /// A press on row `index` of `rows`, picking as `how` says. Returns
    /// whether the press was a pick, which a click then ignores.
    pub fn pick(&mut self, rows: &[Row], index: usize, how: Pick) -> bool {
        let Some(row) = rows.get(index) else {
            return false;
        };
        if row.kind != RowKind::Run {
            return false;
        }
        match how {
            Pick::Range => {
                let from = self.anchor.unwrap_or(index);
                let (lo, hi) = (from.min(index), from.max(index));
                for r in rows[lo..=hi.min(rows.len() - 1)]
                    .iter()
                    .filter(|r| r.kind == RowKind::Run)
                {
                    if !self.picked.contains(&r.run.id) {
                        self.picked.push(r.run.id.clone());
                    }
                }
                true
            }
            Pick::Toggle => {
                let id = &row.run.id;
                match self.picked.iter().position(|p| p == id) {
                    Some(at) => {
                        self.picked.remove(at);
                    }
                    None => self.picked.push(id.clone()),
                }
                self.anchor = Some(index);
                true
            }
            Pick::Open => {
                self.picked.clear();
                self.anchor = Some(index);
                false
            }
        }
    }

    /// Picks every run in the panel.
    pub fn pick_all(&mut self) {
        if let Some(family) = &self.family {
            self.picked = family.rows().into_iter().map(|r| r.run.id).collect();
        }
    }

    /// A right click on row `index` of `rows`: the row joins the pick
    /// unless it is in it already, when the click acts on every picked
    /// run.
    pub fn pick_for_menu(&mut self, rows: &[Row], index: usize) {
        let Some(row) = rows.get(index) else {
            return;
        };

        // A folding row is no run: the menu on it acts on no picked run,
        // so none from an earlier click is deleted by mistake.
        if row.kind != RowKind::Run {
            self.picked.clear();
            return;
        }
        if !self.picked.contains(&row.run.id) {
            self.picked = vec![row.run.id.clone()];
            self.anchor = Some(index);
        }
    }

    /// The runs the panel's menu acts on: the picked ones, in the panel's
    /// order.
    pub fn menu_targets(&self) -> Vec<RunEntry> {
        let Some(family) = &self.family else {
            return Vec::new();
        };
        family
            .rows()
            .into_iter()
            .filter(|r| self.picked.contains(&r.run.id))
            .map(|r| r.run)
            .collect()
    }

    /// What deleting `runs` and their forks takes, and the question to
    /// ask first when it is more than a single fork with no forks of its
    /// own.
    pub fn deletion(&self, runs: &[RunEntry]) -> Option<Deletion> {
        let family = self.family.as_ref()?;
        let picked: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();

        // Runs under another picked run go with it.
        let tops: Vec<&RunEntry> = runs
            .iter()
            .filter(|r| {
                !picked
                    .iter()
                    .any(|p| family.descendants(p).iter().any(|d| d.id == r.id))
            })
            .collect();
        let mut gone: Vec<&str> = Vec::new();
        for top in &tops {
            for id in std::iter::once(top.id.as_str())
                .chain(family.descendants(&top.id).iter().map(|d| d.id.as_str()))
            {
                if !gone.contains(&id) {
                    gone.push(id);
                }
            }
        }
        let dirs: Vec<PathBuf> = tops.iter().map(|r| r.dir.clone()).collect();
        let at_once = tops.len() == 1 && tops[0].parent.is_some() && gone.len() == 1;
        let question = (!at_once).then(|| match (tops.len(), gone.len() - tops.len()) {
            (1, 0) => format!("Delete run {}?", short_id(&tops[0].id)),
            (1, 1) => format!("Delete run {} and its fork?", short_id(&tops[0].id)),
            (1, n) => format!("Delete run {} and its {n} forks?", short_id(&tops[0].id)),
            (k, 0) => format!("Delete {k} runs?"),
            (k, n) => format!("Delete {k} runs and their {n} forks?"),
        });
        Some(Deletion {
            dirs,
            runs: gone.len(),
            question,
        })
    }
}

#[cfg(test)]
mod tests {
    // The panel's decisions over a family made in memory: a run from boot,
    // two forks of it, and a fork of the first fork.
    use super::*;
    use crate::family::Parent;

    /// A run of the family with this id and parent, from the failing
    /// example's manifest.
    fn entry(id: &str, parent: Option<&str>) -> RunEntry {
        let manifest = crate::examples::manifest_of(crate::examples::FAILING);
        RunEntry {
            id: id.into(),
            dir: PathBuf::from(format!("/runs/{id}")),
            parent: parent.map(|p| Parent {
                id: p.into(),
                step: 10,
            }),
            ..RunEntry::from_manifest(
                std::path::Path::new("/runs"),
                &manifest,
                std::time::SystemTime::UNIX_EPOCH,
            )
        }
    }

    fn panel() -> RunsPanel {
        let mut panel = RunsPanel::default();
        panel.set_family(Some(Family {
            runs: vec![
                entry("base", None),
                entry("f1", Some("base")),
                entry("f2", Some("base")),
                entry("f1a", Some("f1")),
            ],
        }));
        panel
    }

    #[test]
    fn ctrl_picks_shift_extends_and_a_plain_press_opens() {
        // Ctrl on two rows picks both and on one again drops it; Shift
        // picks every run from the last row Ctrl pressed to the row it
        // presses; a plain press drops the pick and is no pick itself.
        let mut panel = panel();
        let rows = panel.rows("", "");
        let at = |id: &str| rows.iter().position(|r| r.run.id == id).unwrap();
        assert!(panel.pick(&rows, at("f1"), Pick::Toggle));
        assert!(panel.pick(&rows, at("f2"), Pick::Toggle));
        assert!(panel.pick(&rows, at("f1"), Pick::Toggle));
        assert_eq!(panel.picked, vec!["f2".to_string()]);

        // Shift from base: f1 was the last row Ctrl pressed, so every run
        // from base to f1 joins f2.
        assert!(panel.pick(&rows, at("base"), Pick::Range));
        let mut picked = panel.picked.clone();
        picked.sort();
        assert_eq!(picked, vec!["base", "f1", "f2"]);

        assert!(!panel.pick(&rows, at("f1a"), Pick::Open));
        assert!(panel.picked.is_empty());
    }

    #[test]
    fn deleting_one_fork_goes_at_once_and_more_asks_first() {
        // f2, a fork with no forks, goes without a question. f1 takes its
        // fork with it; f1 and f1a picked together are f1 and its fork,
        // once; two forks are two runs; the run from boot takes all four.
        let panel = panel();
        let family_runs = |ids: &[&str]| -> Vec<RunEntry> {
            let family = panel.family.as_ref().unwrap();
            ids.iter()
                .map(|id| family.runs.iter().find(|r| r.id == *id).unwrap().clone())
                .collect()
        };
        let lone = panel.deletion(&family_runs(&["f2"])).unwrap();
        assert_eq!((lone.question, lone.runs), (None, 1));
        assert_eq!(lone.dirs, vec![PathBuf::from("/runs/f2")]);

        let ask = |ids: &[&str]| panel.deletion(&family_runs(ids)).unwrap();
        assert_eq!(
            ask(&["f1"]).question.as_deref(),
            Some("Delete run f1 and its fork?")
        );
        let together = ask(&["f1", "f1a"]);
        assert_eq!((together.runs, together.dirs.len()), (2, 1));
        assert_eq!(
            ask(&["f1a", "f2"]).question.as_deref(),
            Some("Delete 2 runs?")
        );
        assert_eq!(
            ask(&["base"]).question.as_deref(),
            Some("Delete run base and its 3 forks?")
        );
    }
}
