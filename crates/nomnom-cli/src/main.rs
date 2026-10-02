//! `nomnom` — argument parsing and dispatch. Every answer comes from
//! `nomnom-core`; this crate only asks and renders.

mod clean;
mod input;
mod pack;
mod paths;
mod scan;
mod suggest;
mod undo;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use input::BackendArg;

#[derive(Debug, Parser)]
#[command(name = "nomnom", version, about = "Find what is eating your disk, and take it back.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Flags every command that scans a tree shares.
#[derive(Debug, Args)]
struct ScanArgs {
    /// Directory to examine.
    path: PathBuf,
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
    /// Show the tree with rolled-up sizes, biggest first.
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
    /// Say what each path is and whether it can go.
    Suggest {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(long)]
        json: bool,
    },
    /// Plan a cleanup. Dry-run unless `--apply` is given.
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
    /// Add, list and trust the rule packs a run loads.
    Pack {
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
            scan::run(&scan.path, scan.backend, scan.show_errors, depth, top, json)
        }
        Command::Suggest { scan, json } => {
            suggest::run(&scan.path, scan.backend, scan.show_errors, &scan.packs, json)
        }
        Command::Clean { scan, apply, include_review, stage, json } => clean::run(clean::Request {
            path: &scan.path,
            backend: scan.backend,
            show_errors: scan.show_errors,
            packs: &scan.packs,
            apply,
            include_review,
            stage,
            json,
        }),
        Command::Pack { command } => pack::run(command),
        Command::Undo { journal, json } => undo::run(&journal, json),
    }
}
