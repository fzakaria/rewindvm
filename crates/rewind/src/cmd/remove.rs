//! The `rewind` subcommands that remove runs, and the images and pages no
//! run uses any more.

use std::process::ExitCode;

use anyhow::{Result, bail};
use rewind_core::{Home, Run};

use crate::{RUN_HELP, RUN_LONG_HELP, show};

/// The arguments of `rewind prune`.
#[derive(clap::Args)]
pub struct PruneArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// Remove forks whose trace is the same as an older fork's in the
    /// family, or the run's own.
    #[arg(long)]
    pub(crate) identical: bool,
    /// Show what would be removed and remove nothing.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Print the ids removed as one JSON array on standard output, for
    /// programs such as the desktop app.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn prune(home: &Home, args: PruneArgs) -> Result<ExitCode> {
    let PruneArgs {
        run,
        identical,
        dry_run,
        json,
    } = args;
    if !identical {
        bail!("say what to prune: --identical removes forks that ran the same");
    }
    let root = Run::find(home, &run)?;
    let removals = rewind_core::prune::plan_identical(home, &root)?;
    if !dry_run {
        rewind_core::prune::remove(home, &removals)?;
    }
    if json {
        let ids: Vec<&str> = removals.iter().map(|r| r.id.as_str()).collect();
        println!("{}", serde_json::json!(ids));
        return Ok(ExitCode::SUCCESS);
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for r in &removals {
        println!("{verb} {}: the same trace as {}", r.id, r.same_as);
    }
    if removals.is_empty() {
        eprintln!("rewind: no fork of {} needs pruning", root.manifest.id);
        return Ok(ExitCode::SUCCESS);
    }
    if !dry_run {
        print_gc_hint(home);
    }
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind remove`.
#[derive(clap::Args)]
pub struct RemoveArgs {
    #[arg(value_name = "RUN", required = true, help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) runs: Vec<String>,
    /// Show what would be removed and remove nothing.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Print {"removed": [ids]} on standard output, each run named
    /// before its forks, for programs such as the desktop app.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn remove(home: &Home, args: RemoveArgs) -> Result<ExitCode> {
    let RemoveArgs {
        runs,
        dry_run,
        json,
    } = args;
    // Every run named must resolve before any is removed. One whose
    // manifest does not read is named by its id or a prefix of it.
    let runs = runs
        .iter()
        .map(|run| match Run::find(home, run) {
            Ok(found) => Ok(found.manifest.id.to_string()),
            Err(e) => Run::find_unreadable(home, run).map(|u| u.id).ok_or(e),
        })
        .collect::<Result<Vec<String>>>()?;
    let act = if dry_run {
        rewind_core::prune::Act::DryRun
    } else {
        rewind_core::prune::Act::Remove
    };
    let removed = rewind_core::prune::remove_with_forks(home, &runs, act)?;
    if json {
        println!("{}", serde_json::json!({ "removed": removed }));
        return Ok(ExitCode::SUCCESS);
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for id in &removed {
        println!("{verb} {id}");
    }
    if !dry_run {
        print_gc_hint(home);
    }
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind gc`.
#[derive(clap::Args)]
pub struct GcArgs {
    /// Show what would be removed and remove nothing.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Print what was removed as one JSON object on standard output:
    /// {"images": [{"path", "bytes"}], "source_caches": [paths], "pages",
    /// "page_bytes", "bytes"}.
    #[arg(long)]
    pub(crate) json: bool,
}

pub fn gc(home: &Home, args: GcArgs) -> Result<ExitCode> {
    let GcArgs { dry_run, json } = args;
    let act = if dry_run {
        rewind_core::gc::Act::DryRun
    } else {
        rewind_core::gc::Act::Remove
    };
    let garbage = rewind_core::gc::collect(home, act)?;
    if json {
        let images: Vec<serde_json::Value> = garbage
            .images
            .iter()
            .map(|i| serde_json::json!({ "path": i.path, "bytes": i.bytes }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "images": images,
                "source_caches": garbage.source_caches,
                "pages": garbage.pages.pages,
                "page_bytes": garbage.pages.bytes,
                "bytes": garbage.bytes(),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    let (verb, freed) = if dry_run {
        ("would remove", "would free")
    } else {
        ("removed", "freed")
    };
    for image in &garbage.images {
        println!(
            "{verb} {} ({})",
            image.path.display(),
            show::size(image.bytes)
        );
    }
    for cache in &garbage.source_caches {
        println!("{verb} {}", cache.display());
    }
    println!(
        "{verb} {} pages ({})",
        garbage.pages.pages,
        show::size(garbage.pages.bytes)
    );
    if garbage.unreadable_keyframes > 0 {
        eprintln!(
            "rewind: {} keyframes do not read back, as from another build of rewind; \
                 the pages only they name were not kept",
            garbage.unreadable_keyframes
        );
    }
    println!("{freed} {}", show::size(garbage.bytes()));
    Ok(ExitCode::SUCCESS)
}

/// After runs are removed, says how much of the image cache `rewind gc`
/// would free, which takes reading every manifest. Counting the pages too
/// would take reading every keyframe and the whole page index, which is
/// too slow to do after each removal, so the line only mentions them. A
/// failure to count is not the removal's, and prints nothing.
fn print_gc_hint(home: &Home) {
    let Ok(images) = rewind_core::gc::unused_images(home) else {
        return;
    };
    let bytes: u64 = images.iter().map(|i| i.bytes).sum();
    if bytes == 0 {
        eprintln!("rewind: `rewind gc` removes the pages no run uses any more");
        return;
    }
    eprintln!(
        "rewind: images no run uses take {}; `rewind gc` removes them and the pages no run uses",
        show::size(bytes)
    );
}
