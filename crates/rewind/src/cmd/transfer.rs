//! The `rewind` subcommands that move a run to another machine in a `.rwd`
//! file.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use rewind_core::{Home, Run, export};

use crate::{RUN_HELP, RUN_LONG_HELP, color, show};

/// The arguments of `rewind export`.
#[derive(clap::Args)]
pub(crate) struct ExportArgs {
    #[arg(help = RUN_HELP, long_help = RUN_LONG_HELP)]
    pub(crate) run: String,
    /// Where to write it; <id>.rwd by default.
    #[arg(long, short)]
    pub(crate) output: Option<PathBuf>,
    /// Include keyframes, their pages, the input image and the VM's
    /// kernel, so another machine with a compatible CPU can replay the
    /// run.
    #[arg(long)]
    pub(crate) replayable: bool,
    /// Replay the run and pack in, at the step it failed on, at each
    /// assertion it broke and at each bookmark, where every live thread
    /// of the job was and the files the job had written by then.
    ///
    /// For a reader without a machine to replay on, such as the browser
    /// viewer. Source and file contents can say more of a job than its
    /// trace does, so an export carries them only when asked; its
    /// answers.json says they are inside. A file is kept up to 1 MiB and
    /// the files up to 32 MiB between them; the export says which were
    /// cut or left out. Takes gdb with Python on PATH, as `rewind where`
    /// does.
    #[arg(long)]
    pub(crate) with_answers: bool,
}

pub(crate) fn export(home: &Home, args: ExportArgs) -> Result<ExitCode> {
    let ExportArgs {
        run,
        output,
        replayable,
        with_answers,
    } = args;
    let run = Run::find(home, &run)?;
    let out = output.unwrap_or_else(|| PathBuf::from(format!("{}.rwd", run.manifest.id)));
    let contents = if replayable {
        export::Contents::Replayable
    } else {
        export::Contents::View
    };
    if with_answers {
        let packed = crate::answers::gather(home, &run)?;
        export::export_with_answers(home, &run, contents, &packed, &out)?;
    } else {
        export::export(home, &run, contents, &out)?;
    }
    let size = std::fs::metadata(&out)?.len();
    eprintln!(
        "{} wrote {} ({})",
        color::stderr().prefix(),
        out.display(),
        show::size(size)
    );
    Ok(ExitCode::SUCCESS)
}

/// The arguments of `rewind import`.
#[derive(clap::Args)]
pub(crate) struct ImportArgs {
    /// A .rwd file, or an http or https URL of one, which is unpacked
    /// as it downloads.
    pub(crate) file: String,
    /// Print the run as one JSON object on standard output, for
    /// programs such as the desktop app: its id, its directory, and
    /// whether it has the keyframes and inputs to replay.
    #[arg(long)]
    pub(crate) json: bool,
}

pub(crate) fn import(home: &Home, args: ImportArgs) -> Result<ExitCode> {
    let ImportArgs { file, json } = args;
    let run = if export::is_url(&file) {
        eprintln!("{} downloading {file}", color::stderr().prefix());
        export::import_url(home, &file)?
    } else {
        export::import(home, std::path::Path::new(&file))?
    };
    if json {
        let replayable = run.has_keyframes();
        println!(
            "{}",
            serde_json::json!({
                "id": run.manifest.id,
                "dir": run.dir,
                "replayable": replayable,
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", show::summary(&run, color::stdout()));
    Ok(ExitCode::SUCCESS)
}
