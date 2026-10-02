//! `nomnom clean` — the plan, and only on request the act.
//!
//! Dry-run is the default. `--apply` is the only thing that moves a byte, and
//! it sends trashed paths to the recycle bin, where they can be restored from.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};
use nomnom_core::action::{Action, ApplyReport, Plan, RecordStatus, plain, plan_from};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::VolumeRoot;
use nomnom_core::verdict::{TrustedPack, assess};
use serde::Serialize;

use crate::input;

pub struct Request<'a> {
    pub drive: &'a VolumeRoot,
    pub show_errors: bool,
    /// The `--pack <DIR>` arguments, in the order they were given.
    pub packs: &'a [PathBuf],
    pub apply: bool,
    pub include_review: bool,
    pub json: bool,
}

pub fn run(request: Request<'_>) -> Result<ExitCode> {
    let packs = nomnom_core::verdict::resolve_packs(request.drive.as_path(), request.packs)?;
    let catalog = input::load(request.drive)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, request.show_errors);

    let mode =
        Mode { apply: request.apply, include_review: request.include_review, json: request.json };
    execute(&catalog, packs, mode, &mut std::io::stdout().lock())
}

/// What to do with a catalog once it is judged.
struct Mode {
    apply: bool,
    include_review: bool,
    json: bool,
}

fn execute(
    catalog: &Catalog,
    packs: Vec<TrustedPack>,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    let assessment = assess(catalog, packs);
    let (plan, refused) = plan_from(&assessment, None, mode.include_review)
        .with_context(|| format!("cannot anchor a plan at {}", assessment.root.display()))?;
    // A guard refusal is information, not a stop: the other actions are still
    // sound, and the user can act on the named path.
    for (path, error) in refused {
        eprintln!("skipping {}: {error}", path.display());
    }

    if mode.apply {
        let report = nomnom_core::action::apply(&plan).context("apply failed")?;
        return report_apply(&report, mode.json, out);
    }

    report_plan(&plan, mode.include_review, mode.json, out)
}

fn report_plan(
    plan: &Plan,
    include_review: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    if json {
        let report = PlanOutput {
            root: plan.root().display().to_string(),
            applied: false,
            total_bytes: plan.total_bytes(),
            entries: plan.actions(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(out, "Dry run — nothing has been touched. Add --apply to carry this out.")?;
    writeln!(out, "Root: {}", plain(plan.root()))?;
    if plan.is_empty() {
        writeln!(out, "Nothing to clean.")?;
        if !include_review {
            writeln!(
                out,
                "(--include-review would also consider paths the evidence does not carry.)"
            )?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out)?;
    for entry in plan.actions() {
        writeln!(
            out,
            "{} {}  {}",
            verb(&entry.action),
            plain(entry.action.path()),
            format_size(entry.bytes, BINARY)
        )?;
        if let Some(destination) = entry.action.destination() {
            writeln!(out, "      -> {}", plain(destination))?;
        }
        writeln!(out, "      {}", entry.reason)?;
    }
    writeln!(out)?;
    writeln!(out, "{} actions, {} reclaimed", plan.len(), format_size(plan.total_bytes(), BINARY))?;
    if !include_review {
        writeln!(out, "(--include-review would also consider paths the evidence does not carry.)")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report_apply(report: &ApplyReport, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let failures = report.failures().count();
    if json {
        let output = ApplyOutput { applied: true, report };
        writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
    } else {
        for record in report.records() {
            let status = match &record.status {
                RecordStatus::Succeeded => "done".to_string(),
                RecordStatus::Failed { message } => format!("FAILED: {message}"),
            };
            writeln!(
                out,
                "{} {}  {}  [{status}]",
                kind_verb(record.kind),
                plain(&record.source),
                format_size(record.bytes, BINARY)
            )?;
            writeln!(out, "      {}", record.reason)?;
        }
        writeln!(out)?;
        writeln!(out, "Reclaimed {}.", format_size(report.bytes_reclaimed(), BINARY))?;
        if failures > 0 {
            writeln!(out)?;
            writeln!(out, "{failures} actions failed; each is marked FAILED above.")?;
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
    #[serde(flatten)]
    report: &'a ApplyReport,
}

/// Run on fixture catalogs: the public scan takes whole drives only, and a
/// test that applied a plan to a real drive would be the bug it guards.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use nomnom_core::verdict::resolve_packs;

    use super::*;
    use crate::scan_fixtures::catalog_of;
    use crate::test_support::{isolated_store, node_fixture};

    /// Every path under `root`, relative, with file contents. Directories map
    /// to `None`. This is the thing a dry run must leave identical.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let entry = entry.expect("entry");
                let path = entry.path();
                let relative = path.strip_prefix(root).expect("relative").to_path_buf();
                if entry.file_type().expect("file_type").is_dir() {
                    out.insert(relative, None);
                    stack.push(path);
                } else {
                    out.insert(relative, Some(std::fs::read(&path).expect("read")));
                }
            }
        }
        out
    }

    fn dry_run(root: &Path) -> (ExitCode, String) {
        isolated_store();
        let packs = resolve_packs(root, &[]).expect("packs resolve");
        let mode = Mode { apply: false, include_review: false, json: false };
        let mut out = Vec::new();
        let code = execute(&catalog_of(root), packs, mode, &mut out).expect("clean runs");
        (code, String::from_utf8(out).expect("utf-8 output"))
    }

    /// A dry run that is not dry is the single worst bug this tool could
    /// ship: `clean` without `--apply` must leave every path and every byte
    /// where it was.
    #[test]
    fn clean_without_apply_touches_nothing() {
        let dir = node_fixture();
        let before = snapshot(dir.path());

        let (code, text) = dry_run(dir.path());
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(text.contains("node_modules"), "plan did not name node_modules:\n{text}");

        assert_eq!(before, snapshot(dir.path()), "dry run modified the tree");
    }
}
