//! Removing forks that add nothing: ones that ran exactly as an older run
//! in the same family did.
//!
//! A family is a run and every run forked from it, from those, and so on.
//! Two runs with the same trace hash did the same thing, so of each such
//! group in a family only the oldest is worth keeping; the run the family
//! is named by is always kept. A run another run on this machine names as
//! its parent, or reads keyframes from, stays whatever its trace, unless
//! every such run is being removed too. Pages the removed runs' keyframes
//! named stay in the page store.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result};

use crate::home::Home;
use crate::run::{Run, TRACE};

/// What pruning needs to know about one run.
#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    /// The run it was forked from.
    pub parent: Option<String>,
    /// The run it reads keyframes from.
    pub shares: Option<String>,
    pub created: u64,
    pub trace_hash: Option<String>,
    /// Whether it ran to the end; a run still going may yet differ.
    pub finished: bool,
}

/// A run to remove, and the run with the same trace that stays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub id: String,
    pub same_as: String,
}

impl Member {
    fn of(run: &Run) -> Member {
        let m = &run.manifest;
        let finished = m.outcome.is_some();

        // Runs made before manifests kept the hash get it from the trace.
        let trace_hash = m.trace_hash.clone().or_else(|| {
            finished
                .then(|| crate::image::hash_file(&run.dir.join(TRACE)).ok())
                .flatten()
        });
        Member {
            id: m.id.clone(),
            parent: m.parent.as_ref().map(|(id, _)| id.clone()),
            shares: m.shared_keyframes.as_ref().map(|s| s.run.clone()),
            created: m.created,
            trace_hash,
            finished,
        }
    }

    /// The runs this one cannot do without.
    fn needs(&self) -> impl Iterator<Item = &String> {
        self.parent.iter().chain(self.shares.iter())
    }
}

/// The runs in `root`'s family, other than `root`, whose trace is the
/// same as an older member's and that no remaining run needs, given every
/// run on the machine. In order of age.
pub fn identical(root: &str, runs: &[Member]) -> Vec<Removal> {
    // The family: the root and everything forked from it, transitively.
    let mut family: BTreeSet<&str> = BTreeSet::from([root]);
    let mut grew = true;
    while grew {
        grew = false;
        for r in runs {
            let forked_from_family = r.parent.as_deref().is_some_and(|p| family.contains(p));
            if forked_from_family && family.insert(&r.id) {
                grew = true;
            }
        }
    }

    // The member each trace keeps: the root if it has that trace, else
    // the oldest, ties broken by id so the choice is stable.
    let mut members: Vec<&Member> = runs
        .iter()
        .filter(|r| family.contains(r.id.as_str()) && r.finished)
        .collect();
    members.sort_by_key(|r| (r.id != root, r.created, r.id.clone()));
    let mut keepers: BTreeMap<&str, &str> = BTreeMap::new();
    let mut removable: BTreeMap<&str, &str> = BTreeMap::new();
    for r in &members {
        let Some(hash) = r.trace_hash.as_deref() else {
            continue;
        };
        match keepers.get(hash) {
            Some(keeper) => {
                removable.insert(&r.id, keeper);
            }
            None => {
                keepers.insert(hash, &r.id);
            }
        }
    }

    // A run stays while a run that stays needs it. Each pass can free
    // nothing new, only keep more, so this ends.
    loop {
        let needed: Vec<&str> = runs
            .iter()
            .filter(|r| !removable.contains_key(r.id.as_str()))
            .flat_map(|r| r.needs().map(String::as_str))
            .filter(|need| removable.contains_key(need))
            .collect();
        if needed.is_empty() {
            break;
        }
        for need in needed {
            removable.remove(need);
        }
    }

    members
        .iter()
        .filter_map(|r| {
            removable.get(r.id.as_str()).map(|same_as| Removal {
                id: r.id.clone(),
                same_as: same_as.to_string(),
            })
        })
        .collect()
}

/// The forks of `root` that `identical` would remove, among the runs in
/// the home.
pub fn plan_identical(home: &Home, root: &Run) -> Result<Vec<Removal>> {
    let members: Vec<Member> = Run::list(home)?.iter().map(Member::of).collect();
    Ok(identical(&root.manifest.id, &members))
}

/// Deletes the runs' directories.
pub fn remove(home: &Home, removals: &[Removal]) -> Result<()> {
    for r in removals {
        let dir = home.runs().join(&r.id);
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // identical() over hand-made families of runs: which forks go, which
    // stay because they are the oldest of their trace, and which stay
    // because another run still names them.
    use super::*;

    /// A finished run made at `created` with a trace hash.
    fn run(id: &str, parent: Option<&str>, created: u64, hash: &str) -> Member {
        Member {
            id: id.into(),
            parent: parent.map(Into::into),
            shares: parent.map(Into::into),
            created,
            trace_hash: Some(hash.into()),
            finished: true,
        }
    }

    fn removal(id: &str, same_as: &str) -> Removal {
        Removal {
            id: id.into(),
            same_as: same_as.into(),
        }
    }

    #[test]
    fn the_newer_of_two_identical_forks_goes() {
        // a and b ran the same; b is newer. c ran differently and stays.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("r"), 2, "h1"),
            run("c", Some("r"), 3, "h2"),
        ];
        assert_eq!(identical("r", &runs), vec![removal("b", "a")]);
    }

    #[test]
    fn a_fork_identical_to_the_run_itself_goes_and_the_run_stays() {
        // The run is never removed, even when a fork matches it, and it is
        // what such a fork is the same as.
        let runs = [run("r", None, 5, "h0"), run("a", Some("r"), 6, "h0")];
        assert_eq!(identical("r", &runs), vec![removal("a", "r")]);
        assert_eq!(identical("a", &runs), vec![]);
    }

    #[test]
    fn grandchildren_are_in_the_family_and_strangers_are_not() {
        // g is a fork of a fork and matches a; s matches too but descends
        // from another run, so it is left alone.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("g", Some("a"), 2, "h1"),
            run("x", None, 0, "hx"),
            run("s", Some("x"), 3, "h1"),
        ];
        assert_eq!(identical("r", &runs), vec![removal("g", "a")]);
    }

    #[test]
    fn a_run_another_run_needs_stays() {
        // b matches a but d, which differs, is a fork of b: b stays.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("r"), 2, "h1"),
            run("d", Some("b"), 3, "h2"),
        ];
        assert_eq!(identical("r", &runs), vec![]);
    }

    #[test]
    fn a_run_needed_only_by_runs_that_go_goes_too() {
        // b matches a, and b's only fork e matches a as well: both go.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("r"), 2, "h1"),
            run("e", Some("b"), 3, "h1"),
        ];
        assert_eq!(
            identical("r", &runs),
            vec![removal("b", "a"), removal("e", "a")]
        );
    }

    #[test]
    fn keyframes_shared_from_outside_the_family_keep_a_run() {
        // k is no fork of b, but reads b's keyframes, so b stays.
        let mut k = run("k", None, 4, "hk");
        k.shares = Some("b".into());
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("r"), 2, "h1"),
            k,
        ];
        assert_eq!(identical("r", &runs), vec![]);
    }

    #[test]
    fn unfinished_runs_are_neither_removed_nor_kept_for() {
        // u is still running: its trace hash is unknown, so it cannot be a
        // duplicate, and it still needs its parent b.
        let mut u = run("u", Some("b"), 3, "h1");
        u.finished = false;
        u.trace_hash = None;
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("r"), 2, "h1"),
            u,
        ];
        assert_eq!(identical("r", &runs), vec![]);
    }
}
