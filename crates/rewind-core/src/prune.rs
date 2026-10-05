//! Removing runs: one at a time, or the forks that add nothing because
//! they ran exactly as an older run in the same family did.
//!
//! A run another run on this machine names as its parent, or reads
//! keyframes from, is never removed while that run stays. Removing a run
//! deletes its directory, its imported inputs and its source file cache;
//! pages its keyframes named stay in the page store.
//!
//! A family is a run and every run forked from it, from those, and so on.
//! Two runs with the same trace hash did the same thing, so of each such
//! group in a family only the oldest is worth keeping; the run the family
//! is named by is always kept. A run that others need stays whatever its
//! trace, unless every run that needs it is being removed too.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result};

use crate::home::Home;
use crate::keyframes::Shared;
use crate::run::{Run, TRACE};

/// What pruning needs to know about one run.
#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    /// The run it was forked from.
    pub parent: Option<String>,
    /// The run it reads keyframes from, and up to which step.
    pub shares: Option<Shared>,
    pub created: u64,
    pub trace_hash: Option<String>,
    /// Whether it ran to the end; a run still going may yet differ.
    pub finished: bool,
    /// Whether a process is executing it now. An unfinished run that is
    /// not executing was interrupted.
    pub executing: bool,
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
            shares: m.shared_keyframes.clone(),
            created: m.created,
            trace_hash,
            finished,
            executing: !finished && run.executing(),
        }
    }

    /// The runs this one cannot do without.
    fn needs(&self) -> impl Iterator<Item = &String> {
        self.parent.iter().chain(self.shares.iter().map(|s| &s.run))
    }
}

/// The runs in `root`'s family, other than `root`, whose trace is the
/// same as an older member's and that no remaining run needs, given every
/// run on the machine. In order of age.
pub fn identical(root: &str, runs: &[Member]) -> Vec<Removal> {
    // The family: the root and everything forked from it, transitively.
    let family: BTreeSet<&str> = descendants(root, runs).into_iter().collect();

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

/// Deletes the runs `identical` chose.
pub fn remove(home: &Home, removals: &[Removal]) -> Result<()> {
    for r in removals {
        delete(home, &r.id)?;
    }
    Ok(())
}

/// Whether `remove_tree` deletes anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Only say what would go.
    DryRun,
    Remove,
}

/// Why a run and its descendants cannot be removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Another process is executing these runs among them now.
    Executing(Vec<String>),
    /// Runs that stay read keyframes from runs that would go: the reader,
    /// the run it reads from, and the last step it reads.
    Needed { readers: Vec<(String, String, u64)> },
}

impl Refusal {
    /// What to tell the person who asked for the removal.
    pub fn message(&self) -> String {
        match self {
            Refusal::Executing(runs) => runs
                .iter()
                .map(|id| format!("run {id} is executing; remove it once it has finished"))
                .collect::<Vec<_>>()
                .join("; "),
            Refusal::Needed { readers } => readers
                .iter()
                .map(|(reader, from, through)| {
                    format!(
                        "run {reader} reads its keyframes up to step {through} from {from}; \
                         remove it first"
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        }
    }
}

/// `root` and every run forked from it, from those, and so on, nearest
/// first, and in order of age among runs at the same depth. Each run comes
/// after the run it was forked from.
fn descendants<'a>(root: &'a str, runs: &'a [Member]) -> Vec<&'a str> {
    let mut by_age: Vec<&Member> = runs.iter().collect();
    by_age.sort_by_key(|r| (r.created, r.id.clone()));
    let mut tree = vec![root];
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next];
        next += 1;
        for r in &by_age {
            let forked_here = r.parent.as_deref() == Some(parent);
            if forked_here && !tree.contains(&r.id.as_str()) {
                tree.push(&r.id);
            }
        }
    }
    tree
}

/// The runs removing `id` takes: `id` first, then every run that descends
/// from it, nearest first. Refused when one of them is executing, or
/// when a run outside the set reads keyframes from one inside it. A run
/// that reads keyframes through a chain reads them from the first run in
/// it, which then reads from the next, so the direct readers are the ones
/// that matter.
pub fn removal_set(id: &str, runs: &[Member]) -> std::result::Result<Vec<String>, Refusal> {
    let set = descendants(id, runs);
    let executing: Vec<String> = runs
        .iter()
        .filter(|r| set.contains(&r.id.as_str()) && r.executing)
        .map(|r| r.id.clone())
        .collect();
    if !executing.is_empty() {
        return Err(Refusal::Executing(executing));
    }

    // Every run outside the set that names one inside it as its parent is
    // in the set by construction, so only keyframe readers remain.
    let readers: Vec<(String, String, u64)> = runs
        .iter()
        .filter(|r| !set.contains(&r.id.as_str()))
        .filter_map(|r| {
            let shared = r.shares.as_ref()?;
            set.contains(&shared.run.as_str())
                .then(|| (r.id.clone(), shared.run.clone(), shared.through))
        })
        .collect();
    if !readers.is_empty() {
        return Err(Refusal::Needed { readers });
    }
    Ok(set.into_iter().map(String::from).collect())
}

/// Removes `id` and its descendants among `runs`, the deepest first, so a
/// removal cut short never leaves a fork whose parent is gone. Returns the
/// ids in the order `removal_set` gives them.
pub fn remove_tree(home: &Home, id: &str, runs: &[Member], act: Act) -> Result<Vec<String>> {
    let set = removal_set(id, runs).map_err(|refusal| anyhow::anyhow!(refusal.message()))?;
    if act == Act::Remove {
        for run in set.iter().rev() {
            delete(home, run)?;
        }
    }
    Ok(set)
}

/// `remove_tree` over the runs in the home.
pub fn remove_with_forks(home: &Home, run: &Run, act: Act) -> Result<Vec<String>> {
    let members: Vec<Member> = Run::list(home)?.iter().map(Member::of).collect();
    remove_tree(home, &run.manifest.id, &members, act)
}

/// Deletes a run's directory, its imported inputs and its source file
/// cache.
fn delete(home: &Home, id: &str) -> Result<()> {
    let dir = home.runs().join(id);
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    let inputs = home.inputs().join(id);
    if inputs.exists() {
        fs::remove_dir_all(&inputs).with_context(|| format!("removing {}", inputs.display()))?;
    }
    crate::source_cache::remove(home, id)
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
            shares: parent.map(|p| share(p, 99)),
            created,
            trace_hash: Some(hash.into()),
            finished: true,
            executing: false,
        }
    }

    fn share(run: &str, through: u64) -> Shared {
        Shared {
            run: run.into(),
            through,
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
        k.shares = Some(share("b", 99));
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

    #[test]
    fn a_leaf_fork_is_removed_alone() {
        // Nothing descends from a, so the set is a alone.
        let runs = [run("r", None, 0, "h0"), run("a", Some("r"), 1, "h1")];
        assert_eq!(removal_set("a", &runs), Ok(vec!["a".to_string()]));
    }

    #[test]
    fn a_fork_goes_with_its_forks_and_theirs() {
        // a has a fork b, b has a fork c, and a has a second fork d made
        // later: the set is a, then its descendants nearest first; r and
        // the unrelated x stay.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("c", Some("b"), 3, "h3"),
            run("b", Some("a"), 2, "h2"),
            run("d", Some("a"), 4, "h4"),
            run("x", None, 0, "hx"),
        ];
        assert_eq!(
            removal_set("a", &runs),
            Ok(vec!["a".into(), "b".into(), "d".into(), "c".into()])
        );
    }

    #[test]
    fn a_run_outside_the_set_that_reads_its_keyframes_refuses() {
        // k is no descendant of b but reads b's keyframes up to step 400,
        // so neither b nor its fork goes.
        let mut k = run("k", None, 4, "hk");
        k.shares = Some(share("b", 400));
        let runs = [run("b", None, 0, "h1"), run("f", Some("b"), 1, "h2"), k];
        let refusal = removal_set("b", &runs).unwrap_err();
        assert_eq!(
            refusal,
            Refusal::Needed {
                readers: vec![("k".into(), "b".into(), 400)]
            }
        );
        assert_eq!(
            refusal.message(),
            "run k reads its keyframes up to step 400 from b; remove it first"
        );
    }

    #[test]
    fn an_executing_run_in_the_set_refuses() {
        // A run another process is executing is still being written, so
        // removing its parent waits for it too.
        let mut u = run("u", Some("a"), 3, "h1");
        u.finished = false;
        u.executing = true;
        let runs = [run("r", None, 0, "h0"), run("a", Some("r"), 1, "h2"), u];
        let refusal = removal_set("a", &runs).unwrap_err();
        assert_eq!(refusal, Refusal::Executing(vec!["u".into()]));
        assert_eq!(
            refusal.message(),
            "run u is executing; remove it once it has finished"
        );
    }

    #[test]
    fn an_interrupted_run_is_removed() {
        // A run whose execution was killed never finishes and nothing is
        // writing it, so it goes like any other.
        let mut u = run("u", Some("a"), 3, "h1");
        u.finished = false;
        u.trace_hash = None;
        let runs = [run("r", None, 0, "h0"), run("a", Some("r"), 1, "h2"), u];
        assert_eq!(
            removal_set("a", &runs).unwrap(),
            vec!["a".to_string(), "u".to_string()]
        );
    }

    #[test]
    fn removing_takes_the_runs_their_inputs_and_caches_and_a_dry_run_nothing() {
        // A home with r, its fork a and a's fork b, each with imported
        // inputs, and r and a with cached source files. A dry run of
        // removing a names a and b and deletes nothing; the removal then
        // deletes both runs, their inputs and a's cache, and leaves r's.
        use crate::source_cache::{Entry, SourceCache, Version};
        let root = std::env::temp_dir().join(format!("rewind-remove-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::at(root.clone()).unwrap();
        for id in ["r", "a", "b"] {
            fs::create_dir_all(home.runs().join(id)).unwrap();
            fs::create_dir_all(home.inputs().join(id)).unwrap();
        }
        let file = Entry::File(b"int x;\n".to_vec());
        for id in ["r", "a"] {
            SourceCache::of(&home, id)
                .put(Version::Original, "/src/a.c", &file)
                .unwrap();
        }
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("a"), 2, "h2"),
        ];
        let removed = remove_tree(&home, "a", &runs, Act::DryRun).unwrap();
        assert_eq!(removed, vec!["a".to_string(), "b".to_string()]);
        assert!(
            ["r", "a", "b"]
                .iter()
                .all(|id| home.runs().join(id).exists())
        );
        assert!(home.source_cache().join("a").exists());

        let removed = remove_tree(&home, "a", &runs, Act::Remove).unwrap();
        assert_eq!(removed, vec!["a".to_string(), "b".to_string()]);
        for id in ["a", "b"] {
            assert!(!home.runs().join(id).exists());
            assert!(!home.inputs().join(id).exists());
        }
        assert!(home.runs().join("r").exists());
        assert!(home.inputs().join("r").exists());
        assert!(!home.source_cache().join("a").exists());
        assert!(home.source_cache().join("r").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
