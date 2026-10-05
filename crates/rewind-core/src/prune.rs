//! Removing runs from the home, by the rules in rewind_trace::prune: some
//! with their forks, or the forks that add nothing because they ran
//! exactly as an older run in the same family did. Removing a run deletes
//! its directory, its imported inputs and its source file cache; pages its
//! keyframes named stay in the page store.

use std::fs;

use anyhow::{Context, Result};

use crate::home::Home;
use crate::run::{Run, Unreadable};

pub use rewind_trace::prune::{Member, Reads, Refusal, Removal, identical, removal_set};

/// What pruning needs to know about the run `run`.
fn member(run: &Run) -> Member {
    let m = &run.manifest;
    let finished = m.outcome.is_some();
    Member {
        id: m.id.to_string(),
        parent: m.parent.as_ref().map(|p| p.run.to_string()),
        shares: m.shared_keyframes.as_ref().map(|s| Reads {
            run: s.run.to_string(),
            through: s.through,
        }),
        created: m.created,
        trace_hash: m.trace_hash.clone(),
        finished,
        executing: !finished && run.executing(),
    }
}

/// A run whose manifest does not read: nothing is known of what it needs
/// or ran, and its forks name it as their parent all the same.
fn unreadable_member(run: &Unreadable) -> Member {
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

/// The forks of `root` that `identical` would remove, among the runs in
/// the home.
pub fn plan_identical(home: &Home, root: &Run) -> Result<Vec<Removal>> {
    let members: Vec<Member> = Run::list(home)?.iter().map(member).collect();
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
        .map(member)
        .chain(listing.unreadable.iter().map(unreadable_member))
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
    // Removals carried out in a home in a temporary directory: what goes
    // from disk with a run, and what a dry run leaves.
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

    fn share(run: &str, through: u64) -> Reads {
        Reads {
            run: run.into(),
            through,
        }
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
        fork.parent = Some(crate::run::Parent {
            run: crate::run::RunId::parse(u).unwrap(),
            step: 7,
        });
        write(&fork.id, &serde_json::to_vec(&fork).unwrap());
        let other = manifest("0000000000000003", "other", 3);
        write(&other.id, &serde_json::to_vec(&other).unwrap());

        let removed = remove_with_forks(&home, &[u.to_string()], Act::Remove).unwrap();
        assert_eq!(removed, vec![u.to_string(), fork.id.to_string()]);
        assert!(!home.runs().join(u).exists());
        assert!(!home.runs().join(&fork.id).exists());
        assert!(home.runs().join(&other.id).exists());
        fs::remove_dir_all(&root).unwrap();
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
