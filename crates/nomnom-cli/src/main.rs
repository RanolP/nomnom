//! `nomnom` — argument parsing and dispatch. Every answer comes from
//! `nomnom-core`; this crate only asks and renders.

mod clean;
mod drives;
mod input;
mod largest;
mod pack;
mod scan;
mod suggest;
mod types;
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
#[cfg(test)]
use nomnom_core::Feature;
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
    /// Scanning backend. The default reads the MFT, asking for Administrator
    /// through UAC when needed, and walks if that is declined; `mft` fails
    /// rather than falling back; `walk` never prompts.
    #[arg(long, value_enum, default_value_t = BackendArg::Auto, global = true)]
    backend: BackendArg,
    /// List every entry the scan could not read.
    #[arg(long, short = 'v', global = true)]
    show_errors: bool,
}

/// Explicit pack directories, for the commands that judge paths.
#[derive(Debug, Args)]
struct PackArgs {
    /// Load an explicit pack directory. Repeatable; `docs/lang.md` puts these
    /// last in resolution order, so a later one overrides an earlier one and
    /// both override the built-in, user and project packs.
    #[arg(long = "pack", value_name = "DIR")]
    packs: Vec<PathBuf>,
}

/// One subcommand per `nomnom_core::Feature`, the parity contract with the
/// GUI; `Command::feature` and `subcommand_name` keep the two in step.
#[derive(Debug, Subcommand)]
enum Command {
    /// List the fixed drives, with label, filesystem, used, free and total.
    Drives {
        #[arg(long)]
        json: bool,
    },
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
    /// Break the drive's bytes down by file extension.
    Types {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(long)]
        json: bool,
    },
    /// List the drive's largest files, biggest first.
    Largest {
        #[command(flatten)]
        scan: ScanArgs,
        /// How many files to list.
        #[arg(short = 'n', long = "count", default_value_t = 1000)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// Say what each path on the drive is and whether it can go.
    Suggest {
        #[command(flatten)]
        scan: ScanArgs,
        #[command(flatten)]
        packs: PackArgs,
        #[arg(long)]
        json: bool,
    },
    /// Plan a cleanup of the drive. Dry-run unless `--apply` is given.
    Clean {
        #[command(flatten)]
        scan: ScanArgs,
        #[command(flatten)]
        packs: PackArgs,
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
    /// Reverse an apply, using the journal it wrote; with no journal, list the
    /// journals there are to undo.
    Undo {
        /// Path of the journal `clean --apply` printed.
        journal: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
impl Command {
    fn feature(&self) -> Feature {
        match self {
            Command::Drives { .. } => Feature::Drives,
            Command::Scan { .. } => Feature::Tree,
            Command::Types { .. } => Feature::FileTypes,
            Command::Largest { .. } => Feature::LargestFiles,
            Command::Suggest { .. } => Feature::Suggest,
            Command::Clean { .. } => Feature::Clean,
            Command::Pack { .. } => Feature::Packs,
            Command::Undo { .. } => Feature::Undo,
        }
    }
}

/// The parity contract with the GUI: every [`Feature`] names the subcommand
/// that offers it. No wildcard arm, so a new feature fails to compile the
/// test build until the CLI offers it, and the test below proves each name
/// parses into the command for that feature.
#[cfg(test)]
fn subcommand_name(feature: Feature) -> &'static str {
    match feature {
        Feature::Drives => "drives",
        Feature::Tree => "scan",
        Feature::FileTypes => "types",
        Feature::LargestFiles => "largest",
        Feature::Suggest => "suggest",
        Feature::Clean => "clean",
        Feature::Packs => "pack",
        Feature::Undo => "undo",
    }
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
        Command::Drives { json } => drives::run(json),
        Command::Scan { scan, depth, top, json } => {
            scan::run(&scan.drive, scan.backend, scan.show_errors, depth, top, json)
        }
        Command::Types { scan, json } => {
            types::run(&scan.drive, scan.backend, scan.show_errors, json)
        }
        Command::Largest { scan, n, json } => {
            largest::run(&scan.drive, scan.backend, scan.show_errors, n, json)
        }
        Command::Suggest { scan, packs, json } => {
            suggest::run(&scan.drive, scan.backend, scan.show_errors, &packs.packs, json)
        }
        Command::Clean { scan, packs, apply, include_review, stage, json } => {
            clean::run(clean::Request {
                drive: &scan.drive,
                backend: scan.backend,
                show_errors: scan.show_errors,
                packs: &packs.packs,
                apply,
                include_review,
                stage,
                json,
            })
        }
        Command::Pack { drive, command } => pack::run(drive, command),
        Command::Undo { journal, json } => undo::run(journal.as_deref(), json),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    /// The CLI/GUI parity contract: a feature whose subcommand was renamed or
    /// dropped, or that routes to another feature's subcommand, fails here.
    #[test]
    fn every_feature_has_its_own_subcommand() {
        let cli = Cli::command();
        for feature in Feature::ALL {
            let name = subcommand_name(feature);
            assert!(cli.find_subcommand(name).is_some(), "{feature:?}: no `{name}` subcommand");
            let args: &[&str] = match feature {
                Feature::Drives => &["nomnom", "drives"],
                Feature::Undo => &["nomnom", "undo"],
                Feature::Packs => &["nomnom", "pack", "list"],
                _ => &["nomnom", name, "C:\\"],
            };
            let parsed = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(parsed.command.feature(), feature, "{args:?}");
        }
    }
}
