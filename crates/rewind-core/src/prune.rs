//! Removing runs: some with their forks, or the forks that add nothing because
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
use crate::run::{Run, Unreadable};

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
        Member {
            id: m.id.clone(),
            parent: m.parent.as_ref().map(|(id, _)| id.clone()),
            shares: m.shared_keyframes.clone(),
            created: m.created,
            trace_hash: m.trace_hash.clone(),
            finished,
            executing: !finished && run.executing(),
        }
    }

    /// A run whose manifest does not read: nothing is known of what it
    /// needs or ran, and its forks name it as their parent all the same.
    fn unreadable(run: &Unreadable) -> Member {
        Member {
            id: run.id.clone(),
            parent: None,
            shares: None,
            created: run.written,
            trace_hash: None,
            finished: false,
            executing: false,
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
    let family: BTreeSet<&str> = Forks::of(runs).descendants(root).into_iter().collect();

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

/// The runs forked from each run, each run's in order of age, found once
/// for every family a removal looks at.
struct Forks<'a>(BTreeMap<&'a str, Vec<&'a str>>);

impl<'a> Forks<'a> {
    fn of(runs: &'a [Member]) -> Forks<'a> {
        let mut by_age: Vec<&Member> = runs.iter().collect();
        by_age.sort_by_key(|r| (r.created, r.id.as_str()));
        let mut forks: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for r in by_age {
            let Some(parent) = r.parent.as_deref() else {
                continue;
            };
            forks.entry(parent).or_default().push(&r.id);
        }
        Forks(forks)
    }

    /// `root` and every run forked from it, from those, and so on,
    /// nearest first, and in order of age among runs at the same depth.
    /// Each run comes after the run it was forked from.
    fn descendants<'b>(&self, root: &'b str) -> Vec<&'b str>
    where
        'a: 'b,
    {
        let mut tree = vec![root];
        let mut seen = BTreeSet::from([root]);
        let mut next = 0;
        while next < tree.len() {
            let parent = tree[next];
            next += 1;
            for fork in self.0.get(parent).into_iter().flatten() {
                if seen.insert(*fork) {
                    tree.push(fork);
                }
            }
        }
        tree
    }
}

/// The runs removing `ids` takes: each run with every run that descends
/// from it, each run once. A run named that descends from another run
/// named comes with that run's family, so the set is the families of the
/// named runs whose parent is not in it, in the order they were named,
/// each its root first and then its descendants nearest first. Refused,
/// for the whole set, when one of the runs is executing, or when a run
/// outside the set reads keyframes from one inside it. A run that reads
/// keyframes through a chain reads them from the first run in it, which
/// then reads from the next, so the direct readers are the ones that
/// matter.
pub fn removal_set(ids: &[&str], runs: &[Member]) -> std::result::Result<Vec<String>, Refusal> {
    // Every named run and its descendants, whatever the order.
    let forks = Forks::of(runs);
    let all: BTreeSet<&str> = ids.iter().flat_map(|id| forks.descendants(id)).collect();

    // The named runs whose parent goes too are in that parent's family
    // already; the others are the roots of the families that go.
    let parents: BTreeMap<&str, &str> = runs
        .iter()
        .filter_map(|r| Some((r.id.as_str(), r.parent.as_deref()?)))
        .collect();
    let mut roots: BTreeSet<&str> = BTreeSet::new();
    let mut set: Vec<&str> = Vec::new();
    for id in ids {
        let forked_from_another = parents.get(id).is_some_and(|parent| all.contains(parent));
        if forked_from_another || !roots.insert(id) {
            continue;
        }
        set.extend(forks.descendants(id));
    }

    let executing: Vec<String> = runs
        .iter()
        .filter(|r| all.contains(r.id.as_str()) && r.executing)
        .map(|r| r.id.clone())
        .collect();
    if !executing.is_empty() {
        return Err(Refusal::Executing(executing));
    }

    // Every run outside the set that names one inside it as its parent is
    // in the set by construction, so only keyframe readers remain.
    let readers: Vec<(String, String, u64)> = runs
        .iter()
        .filter(|r| !all.contains(r.id.as_str()))
        .filter_map(|r| {
            let shared = r.shares.as_ref()?;
            all.contains(shared.run.as_str())
                .then(|| (r.id.clone(), shared.run.clone(), shared.through))
        })
        .collect();
    if !readers.is_empty() {
        return Err(Refusal::Needed { readers });
    }
    Ok(set.into_iter().map(String::from).collect())
}

/// Removes `ids` and their descendants among `runs`, the deepest first,
/// so a removal cut short never leaves a fork whose parent is gone.
/// Removes nothing when `removal_set` refuses any of them. Returns the ids
/// in the order `removal_set` gives them.
pub fn remove_tree(home: &Home, ids: &[&str], runs: &[Member], act: Act) -> Result<Vec<String>> {
    let set = removal_set(ids, runs).map_err(|refusal| anyhow::anyhow!(refusal.message()))?;
    if act == Act::Remove {
        for run in set.iter().rev() {
            delete(home, run)?;
        }
    }
    Ok(set)
}

/// `remove_tree` of the runs `ids` over the runs in the home, which are
/// listed once however many runs go. Runs whose manifests do not read are
/// among them, so one can be removed by its id.
pub fn remove_with_forks(home: &Home, ids: &[String], act: Act) -> Result<Vec<String>> {
    let listing = Run::list_all(home)?;
    let members: Vec<Member> = listing
        .runs
        .iter()
        .map(Member::of)
        .chain(listing.unreadable.iter().map(Member::unreadable))
        .collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    remove_tree(home, &ids, &members, act)
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
        assert_eq!(removal_set(&["a"], &runs), Ok(vec!["a".to_string()]));
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
            removal_set(&["a"], &runs),
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
        let refusal = removal_set(&["b"], &runs).unwrap_err();
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
        let refusal = removal_set(&["a"], &runs).unwrap_err();
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
            removal_set(&["a"], &runs).unwrap(),
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
        let removed = remove_tree(&home, &["a"], &runs, Act::DryRun).unwrap();
        assert_eq!(removed, vec!["a".to_string(), "b".to_string()]);
        assert!(
            ["r", "a", "b"]
                .iter()
                .all(|id| home.runs().join(id).exists())
        );
        assert!(home.source_cache().join("a").exists());

        let removed = remove_tree(&home, &["a"], &runs, Act::Remove).unwrap();
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

    #[test]
    fn an_unreadable_run_is_removed_with_its_forks() {
        // u's manifest does not read, and f, which does, is its fork:
        // removing u by its id takes both, and leaves the run beside them.
        use crate::run::tests::manifest;
        let root =
            std::env::temp_dir().join(format!("rewind-remove-unreadable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::at(root.clone()).unwrap();
        let write = |id: &str, bytes: &[u8]| {
            let dir = home.runs().join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(crate::run::MANIFEST), bytes).unwrap();
        };
        let u = "0000000000000001";
        write(u, b"{}");
        let mut fork = manifest("0000000000000002", "fork", 2);
        fork.parent = Some((u.into(), 7));
        write(&fork.id, &serde_json::to_vec(&fork).unwrap());
        let other = manifest("0000000000000003", "other", 3);
        write(&other.id, &serde_json::to_vec(&other).unwrap());

        let removed = remove_with_forks(&home, &[u.to_string()], Act::Remove).unwrap();
        assert_eq!(removed, vec![u.to_string(), fork.id.clone()]);
        assert!(!home.runs().join(u).exists());
        assert!(!home.runs().join(&fork.id).exists());
        assert!(home.runs().join(&other.id).exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn several_runs_go_with_the_union_of_their_families() {
        // b is a fork of a, so asking for b and a takes a's family once,
        // a first; y is a fork in another family and goes alone after
        // it. Naming a twice changes nothing, and r and x stay.
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("a"), 2, "h2"),
            run("c", Some("b"), 3, "h3"),
            run("x", None, 0, "hx"),
            run("y", Some("x"), 4, "hy"),
        ];
        assert_eq!(
            removal_set(&["b", "a", "y", "a"], &runs),
            Ok(vec!["a".into(), "b".into(), "c".into(), "y".into()])
        );
    }

    #[test]
    fn a_reader_removed_with_the_run_it_reads_from_does_not_refuse() {
        // k is no fork of b but reads b's keyframes. Removing b alone is
        // refused; removing b and k together leaves no reader behind.
        let mut k = run("k", None, 4, "hk");
        k.shares = Some(share("b", 400));
        let runs = [run("b", None, 0, "h1"), run("f", Some("b"), 1, "h2"), k];
        assert!(removal_set(&["b"], &runs).is_err());
        assert_eq!(
            removal_set(&["b", "k"], &runs),
            Ok(vec!["b".into(), "f".into(), "k".into()])
        );
    }

    #[test]
    fn one_refused_run_among_several_refuses_them_all() {
        // Removing a and b is refused for k, which reads b's keyframes and
        // stays; removing a and e is refused for u, which is executing
        // under e. A refused removal leaves every run in the home, a's
        // family included.
        let root = std::env::temp_dir().join(format!("rewind-remove-many-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::at(root.clone()).unwrap();
        let mut k = run("k", None, 4, "hk");
        k.shares = Some(share("b", 400));
        let mut u = run("u", Some("e"), 6, "hu");
        u.finished = false;
        u.executing = true;
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", None, 2, "h2"),
            k,
            run("e", None, 5, "he"),
            u,
        ];
        for r in &runs {
            fs::create_dir_all(home.runs().join(&r.id)).unwrap();
        }

        assert_eq!(
            removal_set(&["a", "b"], &runs),
            Err(Refusal::Needed {
                readers: vec![("k".into(), "b".into(), 400)]
            })
        );
        assert_eq!(
            removal_set(&["a", "e"], &runs),
            Err(Refusal::Executing(vec!["u".into()]))
        );
        let err = remove_tree(&home, &["a", "b"], &runs, Act::Remove).unwrap_err();
        assert_eq!(
            err.to_string(),
            "run k reads its keyframes up to step 400 from b; remove it first"
        );
        assert!(runs.iter().all(|r| home.runs().join(&r.id).exists()));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn several_runs_are_removed_deepest_first_and_a_dry_run_removes_none() {
        // Two families, a with fork b and x with fork y. A dry run of
        // removing b, a and y names a, b and y and deletes nothing; the
        // removal then deletes all three and leaves r and x.
        let root = std::env::temp_dir().join(format!("rewind-remove-dry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::at(root.clone()).unwrap();
        let runs = [
            run("r", None, 0, "h0"),
            run("a", Some("r"), 1, "h1"),
            run("b", Some("a"), 2, "h2"),
            run("x", None, 0, "hx"),
            run("y", Some("x"), 3, "hy"),
        ];
        for r in &runs {
            fs::create_dir_all(home.runs().join(&r.id)).unwrap();
        }
        let expected: Vec<String> = vec!["a".into(), "b".into(), "y".into()];

        let planned = remove_tree(&home, &["b", "a", "y"], &runs, Act::DryRun).unwrap();
        assert_eq!(planned, expected);
        assert!(runs.iter().all(|r| home.runs().join(&r.id).exists()));

        let removed = remove_tree(&home, &["b", "a", "y"], &runs, Act::Remove).unwrap();
        assert_eq!(removed, expected);
        for id in &expected {
            assert!(!home.runs().join(id).exists());
        }
        assert!(home.runs().join("r").exists());
        assert!(home.runs().join("x").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
