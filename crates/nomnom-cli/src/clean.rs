//! `nomnom clean` — the candidates, the plan of the paths the user names, and
//! only on request the act.
//!
//! Opt-in, like the GUI's checkboxes: with no paths it lists the candidates
//! and plans nothing; a plan holds only the paths named on the command line.
//! Dry-run is the default. `--apply` is the only thing that moves a byte, and
//! it sends trashed paths to the recycle bin, where they can be restored from.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use humansize::{BINARY, format_size};
use nomnom_core::action::{Action, ApplyReport, Plan, RecordStatus, candidates, plain, plan_from};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::VolumeRoot;
use nomnom_core::verdict::{Disposition, Entry, TrustedPack, assess};
use serde::Serialize;

use crate::input;
use crate::suggest::disposition_name;

pub struct Request<'a> {
    pub drive: &'a VolumeRoot,
    pub show_errors: bool,
    /// The `--pack <DIR>` arguments, in the order they were given.
    pub packs: &'a [PathBuf],
    /// The candidates the user picked; empty lists them and plans nothing.
    pub paths: &'a [PathBuf],
    pub apply: bool,
    pub include_review: bool,
    pub json: bool,
}

pub fn run(request: Request<'_>) -> Result<ExitCode> {
    let mode = Mode {
        paths: request.paths,
        apply: request.apply,
        include_review: request.include_review,
        json: request.json,
    };
    // Before the scan, so a refused command never costs a UAC prompt.
    mode.check()?;
    let packs = nomnom_core::verdict::resolve_packs(request.drive.as_path(), request.packs)?;
    let catalog = input::load(request.drive)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, request.show_errors);

    execute(&catalog, packs, mode, &mut std::io::stdout().lock())
}

/// What to do with a catalog once it is judged.
struct Mode<'a> {
    paths: &'a [PathBuf],
    apply: bool,
    include_review: bool,
    json: bool,
}

impl Mode<'_> {
    /// `--apply` acts only on paths the user named; there is no "apply all".
    fn check(&self) -> Result<()> {
        if self.apply && self.paths.is_empty() {
            bail!(
                "--apply needs the paths to delete: nomnom clean <DRIVE> <PATH>... --apply \
                 (run `nomnom clean <DRIVE>` to list the candidates)"
            );
        }
        Ok(())
    }
}

fn execute(
    catalog: &Catalog,
    packs: Vec<TrustedPack>,
    mode: Mode<'_>,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    mode.check()?;
    let assessment = assess(catalog, packs);
    let offered = candidates(&assessment, mode.include_review);
    if mode.paths.is_empty() {
        return report_candidates(&assessment.root, &offered, mode.include_review, mode.json, out);
    }
    let selection = select(&offered, &candidates(&assessment, true), mode.paths)?;
    let (plan, refused) = plan_from(&assessment, &selection, mode.include_review)
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

    report_plan(&plan, mode.json, out)
}

/// The entries of `offered` the user named, by the path the assessment holds.
///
/// Every named path must be a candidate under the current dispositions; one
/// that is not fails the whole command, naming it, rather than being skipped —
/// a silent skip would apply a plan the user did not write.
fn select(offered: &[&Entry], widened: &[&Entry], named: &[PathBuf]) -> Result<HashSet<PathBuf>> {
    let mut selection = HashSet::new();
    let mut rejected = Vec::new();
    for path in named {
        if let Some(entry) = find(offered, path) {
            selection.insert(PathBuf::from(&entry.path));
        } else if find(widened, path)
            .is_some_and(|entry| entry.verdict.disposition == Disposition::Review)
        {
            rejected.push(format!(
                "{}: a `review` verdict; add --include-review to pick it",
                path.display()
            ));
        } else {
            rejected.push(format!("{}: not a cleanup candidate on this drive", path.display()));
        }
    }
    if !rejected.is_empty() {
        bail!(
            "refusing to plan paths that are not candidates (run `nomnom clean <DRIVE>` to list \
             them):\n  {}",
            rejected.join("\n  ")
        );
    }
    Ok(selection)
}

/// The candidate `path` names: by its exact text first, then by where both
/// resolve on disk, so `d:\proj\node_modules\` finds `D:\proj\node_modules`.
fn find<'a>(entries: &[&'a Entry], path: &Path) -> Option<&'a Entry> {
    if let Some(entry) = entries.iter().find(|entry| Path::new(&entry.path) == path) {
        return Some(entry);
    }
    let resolved = path.canonicalize().ok()?;
    entries
        .iter()
        .find(|entry| Path::new(&entry.path).canonicalize().is_ok_and(|it| it == resolved))
        .copied()
}

fn report_candidates(
    root: &Path,
    offered: &[&Entry],
    include_review: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    if json {
        let report = CandidatesOutput {
            root: plain(root),
            applied: false,
            total_bytes: 0,
            entries: &[],
            candidates: offered,
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(out, "Dry run — nothing is selected and nothing has been touched.")?;
    writeln!(out, "Root: {}", plain(root))?;
    if offered.is_empty() {
        writeln!(out, "No candidates.")?;
    } else {
        writeln!(out)?;
        writeln!(out, "Candidates ({}):", offered.len())?;
        for entry in offered {
            writeln!(
                out,
                "  [{}] {}  {}",
                disposition_name(entry.verdict.disposition),
                entry.path,
                format_size(entry.bytes, BINARY)
            )?;
            writeln!(out, "      {}", entry.verdict.reason)?;
            writeln!(out, "      — {}", entry.verdict.provenance)?;
            if let Some(capped) = &entry.verdict.capped {
                writeln!(out, "      ! {capped}")?;
            }
        }
        writeln!(out)?;
        writeln!(out, "Name the ones to delete: nomnom clean <DRIVE> <PATH>... [--apply]")?;
    }
    if !include_review {
        writeln!(out, "(--include-review would also offer paths the evidence does not carry.)")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report_plan(plan: &Plan, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
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
        writeln!(out, "Nothing to clean: every named path was refused above.")?;
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

/// A plan's shape with nothing selected, plus what the user may pick.
#[derive(Serialize)]
struct CandidatesOutput<'a> {
    root: String,
    applied: bool,
    total_bytes: u64,
    entries: &'a [nomnom_core::action::PlanEntry],
    candidates: &'a [&'a Entry],
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

    fn clean(root: &Path, paths: &[PathBuf], apply: bool) -> Result<(ExitCode, String)> {
        isolated_store();
        let packs = resolve_packs(root, &[]).expect("packs resolve");
        let mode = Mode { paths, apply, include_review: false, json: false };
        let mut out = Vec::new();
        let code = execute(&catalog_of(root), packs, mode, &mut out)?;
        Ok((code, String::from_utf8(out).expect("utf-8 output")))
    }

    fn dry_run(root: &Path) -> (ExitCode, String) {
        clean(root, &[], false).expect("clean runs")
    }

    /// The opt-in contract: `clean` must never act on a path the user did not
    /// name. Catches `--apply` with no paths deleting every candidate, and a
    /// named non-candidate (here the project's own `package.json`, or a typo)
    /// reaching the plan instead of failing the command with its name — both
    /// checked with `--apply` on, and the tree compared afterwards.
    #[test]
    fn apply_without_paths_and_non_candidate_paths_are_rejected() {
        let dir = node_fixture();
        let before = snapshot(dir.path());

        let error = clean(dir.path(), &[], true).expect_err("--apply with no paths ran");
        assert!(format!("{error:#}").contains("--apply needs the paths"), "{error:#}");

        let manifest = dir.path().join("package.json");
        let typo = dir.path().join("node_modulez");
        let error = clean(dir.path(), &[manifest.clone(), typo.clone()], true)
            .expect_err("a non-candidate path was planned");
        let message = format!("{error:#}");
        assert!(message.contains(&manifest.display().to_string()), "{message}");
        assert!(message.contains(&typo.display().to_string()), "{message}");

        assert_eq!(before, snapshot(dir.path()), "a rejected clean modified the tree");
    }

    /// Catches a named candidate failing to reach the plan, which would make
    /// the opt-in CLI unable to clean anything.
    #[test]
    fn a_named_candidate_is_planned_alone() {
        let dir = node_fixture();
        let before = snapshot(dir.path());
        let (code, text) =
            clean(dir.path(), &[dir.path().join("node_modules")], false).expect("clean runs");
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(text.contains("1 actions"), "{text}");
        assert_eq!(before, snapshot(dir.path()), "dry run modified the tree");
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
        assert!(text.contains("node_modules"), "candidates did not name node_modules:\n{text}");
        assert!(text.contains("nothing is selected"), "{text}");

        assert_eq!(before, snapshot(dir.path()), "dry run modified the tree");
    }
}
