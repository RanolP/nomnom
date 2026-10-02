//! `nomnom clean` — the plan, and only on request the act.
//!
//! Dry-run is the default. `--apply` is the only thing that moves a byte, and
//! when it does, the journal path is printed loudly: that path is the whole of
//! what makes the operation reversible.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};
use nomnom_core::action::{
    Action, ApplyOptions, Journal, Plan, RecordStatus, plain, plan_from, trash_policy,
};
use nomnom_core::verdict::assess;
use serde::Serialize;

use crate::input::{self, BackendArg};

pub struct Request<'a> {
    pub path: &'a Path,
    pub backend: BackendArg,
    pub show_errors: bool,
    /// The `--pack <DIR>` arguments, in the order they were given.
    pub packs: &'a [PathBuf],
    pub apply: bool,
    pub include_review: bool,
    pub stage: Option<PathBuf>,
    pub json: bool,
}

pub fn run(request: Request<'_>) -> Result<ExitCode> {
    let packs = nomnom_core::verdict::resolve_packs(request.path, request.packs)?;
    let catalog = input::load(request.path, request.backend)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, request.show_errors);

    let assessment = assess(&catalog, packs);
    let (plan, refused) = plan_from(&assessment, None, request.include_review)
        .with_context(|| format!("cannot anchor a plan at {}", assessment.root.display()))?;
    // A guard refusal is information, not a stop: the other actions are still
    // sound, and the user can act on the named path.
    for (path, error) in refused {
        eprintln!("skipping {}: {error}", path.display());
    }

    if request.apply {
        let policy = trash_policy(request.stage);
        let opts = ApplyOptions { trash_policy: policy, ..ApplyOptions::default() };
        let journal = nomnom_core::action::apply(&plan, &opts).context("apply failed")?;
        return report_apply(&journal, request.json);
    }

    report_plan(&plan, request.include_review, request.json)
}

fn report_plan(plan: &Plan, include_review: bool, json: bool) -> Result<ExitCode> {
    if json {
        let out = PlanOutput {
            root: plan.root().display().to_string(),
            applied: false,
            total_bytes: plan.total_bytes(),
            entries: plan.actions(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }

    println!("Dry run — nothing has been touched. Add --apply to carry this out.");
    println!("Root: {}", plain(plan.root()));
    if plan.is_empty() {
        println!("Nothing to clean.");
        if !include_review {
            println!("(--include-review would also consider paths the evidence does not carry.)");
        }
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    for entry in plan.actions() {
        println!(
            "{} {}  {}",
            verb(&entry.action),
            plain(entry.action.path()),
            format_size(entry.bytes, BINARY)
        );
        if let Some(destination) = entry.action.destination() {
            println!("      -> {}", plain(destination));
        }
        println!("      {}", entry.reason);
    }
    println!();
    println!("{} actions, {} reclaimed", plan.len(), format_size(plan.total_bytes(), BINARY));
    if !include_review {
        println!("(--include-review would also consider paths the evidence does not carry.)");
    }
    Ok(ExitCode::SUCCESS)
}

fn report_apply(journal: &Journal, json: bool) -> Result<ExitCode> {
    let failures = journal.failures().count();
    if json {
        println!("{}", serde_json::to_string_pretty(&ApplyOutput { applied: true, journal })?);
    } else {
        for record in journal.records() {
            let status = match &record.status {
                RecordStatus::Succeeded => "done".to_string(),
                RecordStatus::Failed { message } => format!("FAILED: {message}"),
                RecordStatus::Planned => "not performed".to_string(),
                RecordStatus::Undone => "undone".to_string(),
            };
            println!(
                "{} {}  {}  [{status}]",
                kind_verb(record.kind),
                plain(&record.source),
                format_size(record.bytes, BINARY)
            );
            println!("      {}", record.reason);
        }
        println!();
        println!("Reclaimed {}.", format_size(journal.bytes_reclaimed(), BINARY));
        println!();
        println!("Journal: {}", plain(journal.path()));
        println!("Undo with: nomnom undo \"{}\"", plain(journal.path()));
        if failures > 0 {
            println!();
            println!("{failures} actions failed; see the journal.");
        }
    }
    Ok(if failures > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Trash { .. } => "trash  ",
        Action::Archive { .. } => "archive",
        Action::Move { .. } => "move   ",
    }
}

fn kind_verb(kind: nomnom_core::action::ActionKind) -> &'static str {
    use nomnom_core::action::ActionKind;
    match kind {
        ActionKind::Trash => "trash  ",
        ActionKind::Archive => "archive",
        ActionKind::Move => "move   ",
    }
}

#[derive(Serialize)]
struct PlanOutput<'a> {
    root: String,
    applied: bool,
    total_bytes: u64,
    /// The core's own entries, reason included — nothing is stitched on here.
    entries: &'a [nomnom_core::action::PlanEntry],
}

#[derive(Serialize)]
struct ApplyOutput<'a> {
    applied: bool,
    journal: &'a Journal,
}
