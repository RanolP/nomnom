//! `nomnom` — argument parsing and dispatch. Every answer comes from
//! `nomnom-core`; this crate only asks and renders.

mod clean;
mod input;
mod pack;
mod scan;
mod suggest;
mod undo;

// The commands' own tests render from fixture catalogs instead of scanning a
// drive, so they borrow the workspace's fixture helpers by path rather than
// copying them.
#[cfg(test)]
#[path = "../../nomnom-pack/tests/common/mod.rs"]
mod pack_fixtures;
#[cfg(test)]
#[path = "../../nomnom-core/tests/common/mod.rs"]
mod scan_fixtures;
#[cfg(test)]
mod test_support;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use nomnom_core::scan::VolumeRoot;

use input::BackendArg;

#[derive(Debug, Parser)]
#[command(name = "nomnom", version, about = "Find what is eating your disk, and take it back.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Flags every command that scans a drive shares.
#[derive(Debug, Args)]
struct ScanArgs {
    /// Drive to examine, e.g. `C:\` or `D:`. nomnom scans whole drives only.
    drive: VolumeRoot,
    /// Scanning backend. `mft` fails rather than falling back.
    #[arg(long, value_enum, default_value_t = BackendArg::Auto, global = true)]
    backend: BackendArg,
    /// List every entry the scan could not read.
    #[arg(long, short = 'v', global = true)]
    show_errors: bool,
    /// Load an explicit pack directory. Repeatable; `docs/lang.md` puts these
    /// last in resolution order, so a later one overrides an earlier one and
    /// both override the built-in, user and project packs.
    #[arg(long = "pack", value_name = "DIR")]
    packs: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show the drive's tree with rolled-up sizes, biggest first.
    Scan {
        #[command(flatten)]
        scan: ScanArgs,
        /// How many levels of the tree to print.
        #[arg(long, default_value_t = 2)]
        depth: u32,
        /// Cap the children shown per level.
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// Say what each path on the drive is and whether it can go.
    Suggest {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(long)]
        json: bool,
    },
    /// Plan a cleanup of the drive. Dry-run unless `--apply` is given.
    Clean {
        #[command(flatten)]
        scan: ScanArgs,
        /// Actually carry the plan out. Without this nothing is touched.
        #[arg(long)]
        apply: bool,
        /// Also act on `review` verdicts, which the evidence does not carry on
        /// its own.
        #[arg(long)]
        include_review: bool,
        /// Move trashed paths into this directory instead of the recycle bin.
        #[arg(long, value_name = "DIR")]
        stage: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Add, list and trust the rule packs a drive's runs load.
    Pack {
        /// The drive whose `.nomnom\packs.lock` to use. Defaults to the drive
        /// holding the working directory.
        #[arg(long, value_name = "DRIVE", global = true)]
        drive: Option<VolumeRoot>,
        #[command(subcommand)]
        command: pack::PackCommand,
    },
    /// Reverse an apply, using the journal it wrote.
    Undo {
        /// Path of the journal `clean --apply` printed.
        journal: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    // The elevated MFT helper is this binary relaunched behind UAC.
    if let Some(code) = nomnom_core::scan::maybe_run_helper() {
        return code;
    }
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Scan { scan, depth, top, json } => {
            scan::run(&scan.drive, scan.backend, scan.show_errors, depth, top, json)
        }
        Command::Suggest { scan, json } => {
            suggest::run(&scan.drive, scan.backend, scan.show_errors, &scan.packs, json)
        }
        Command::Clean { scan, apply, include_review, stage, json } => clean::run(clean::Request {
            drive: &scan.drive,
            backend: scan.backend,
            show_errors: scan.show_errors,
            packs: &scan.packs,
            apply,
            include_review,
            stage,
            json,
        }),
        Command::Pack { drive, command } => pack::run(drive, command),
        Command::Undo { journal, json } => undo::run(&journal, json),
    }
}
