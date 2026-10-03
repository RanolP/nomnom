//! `nomnom` — argument parsing and dispatch. Every answer comes from
//! `nomnom-core`; this crate only asks and renders.

mod classify;
mod clean;
mod drives;
mod input;
mod pack;
mod scan;
mod suggest;

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

#[derive(Debug, Parser)]
#[command(name = "nomnom", version, about = "Find what is eating your disk, and take it back.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Flags every command that scans a drive shares.
#[derive(Debug, Args)]
struct ScanArgs {
    /// Drive to examine, e.g. `C:\` or `D:`. nomnom scans whole drives only,
    /// reading the MFT behind one Administrator prompt and walking the drive
    /// if that is declined.
    drive: VolumeRoot,
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
    /// Show the drive's tree with rolled-up sizes, biggest first: every file,
    /// only what the packs recognize, or only the other files.
    Scan {
        #[command(flatten)]
        scan: ScanArgs,
        #[command(flatten)]
        packs: PackArgs,
        /// Which files to show. `recognized` and `other` load the packs.
        #[arg(long, value_enum, default_value_t = scan::View::All)]
        view: scan::View,
        /// How many levels of the tree to print.
        #[arg(long, default_value_t = 2)]
        depth: u32,
        /// Cap the children shown per level.
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show which pack owns which subtree, and how the drive splits into
    /// recognized and other bytes. Suggests nothing.
    Classify {
        #[command(flatten)]
        scan: ScanArgs,
        #[command(flatten)]
        packs: PackArgs,
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
    /// List the drive's cleanup candidates grouped by rule, or plan the rules
    /// approved. Nothing is planned unless approved; dry-run unless `--apply`
    /// is given, which deletes the planned paths permanently. This cannot be
    /// undone.
    Clean {
        #[command(flatten)]
        scan: ScanArgs,
        #[command(flatten)]
        packs: PackArgs,
        /// Single candidates to plan, as `nomnom clean <DRIVE>` lists them.
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Approve a rule: plan every candidate it matched, minus exclusions.
        /// Its `[Title]`, or `pack [Title]` when two packs share the title.
        /// Repeatable.
        #[arg(long = "rule", value_name = "RULE")]
        rules: Vec<String>,
        /// Keep a path, and everything under it, out of every plan on this
        /// drive. Persists in `<DRIVE>\.nomnom\exclusions.toml`. Repeatable.
        #[arg(long, value_name = "PATH")]
        exclude: Vec<PathBuf>,
        /// Remove a path from the drive's exclusion list. Repeatable.
        #[arg(long, value_name = "PATH")]
        unexclude: Vec<PathBuf>,
        /// List the drive's exclusions. Alone, or with only exclusion edits,
        /// nothing is scanned.
        #[arg(long)]
        exclusions: bool,
        /// Actually carry the plan out: the planned paths are permanently
        /// deleted, which cannot be undone. Requires a --rule or a PATH.
        #[arg(long)]
        apply: bool,
        /// Also act on `review` verdicts, which the evidence does not carry on
        /// its own.
        #[arg(long)]
        include_review: bool,
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
}

#[cfg(test)]
impl Command {
    fn feature(&self) -> Feature {
        match self {
            Command::Drives { .. } => Feature::Drives,
            Command::Scan { .. } => Feature::Tree,
            Command::Classify { .. } => Feature::Classify,
            Command::Suggest { .. } => Feature::Suggest,
            Command::Clean { .. } => Feature::Clean,
            Command::Pack { .. } => Feature::Packs,
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
        Feature::Classify => "classify",
        Feature::Suggest => "suggest",
        Feature::Clean => "clean",
        Feature::Packs => "pack",
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
        Command::Scan { scan, packs, view, depth, top, json } => scan::run(scan::Request {
            drive: &scan.drive,
            show_errors: scan.show_errors,
            packs: &packs.packs,
            view,
            depth,
            top,
            json,
        }),
        Command::Classify { scan, packs, json } => {
            classify::run(&scan.drive, scan.show_errors, &packs.packs, json)
        }
        Command::Suggest { scan, packs, json } => {
            suggest::run(&scan.drive, scan.show_errors, &packs.packs, json)
        }
        Command::Clean {
            scan,
            packs,
            paths,
            rules,
            exclude,
            unexclude,
            exclusions,
            apply,
            include_review,
            json,
        } => clean::run(clean::Request {
            drive: &scan.drive,
            show_errors: scan.show_errors,
            packs: &packs.packs,
            paths: &paths,
            rules: &rules,
            edits: clean::Edits { exclude: &exclude, unexclude: &unexclude, list: exclusions },
            apply,
            include_review,
            json,
        }),
        Command::Pack { drive, command } => pack::run(drive, command),
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
                Feature::Packs => &["nomnom", "pack", "list"],
                _ => &["nomnom", name, "C:\\"],
            };
            let parsed = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(parsed.command.feature(), feature, "{args:?}");
        }
    }

    /// The GUI tree's three views are one `--view` flag here; a renamed or
    /// dropped value breaks parity with the GUI's view switch.
    #[test]
    fn scan_offers_every_tree_view() {
        for (value, view) in [
            ("all", scan::View::All),
            ("recognized", scan::View::Recognized),
            ("other", scan::View::Other),
        ] {
            let args = ["nomnom", "scan", "C:\\", "--view", value];
            let parsed = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let Command::Scan { view: parsed, .. } = parsed.command else { panic!("not scan") };
            assert_eq!(parsed, view, "{value}");
        }
    }
}
