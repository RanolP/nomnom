//! `nomnom clean` — the candidates grouped by rule, the plan of the rules the
//! user approves, and only on request the act.
//!
//! Opt-in, like the GUI's rule checkboxes: with no `--rule` and no paths it
//! lists the candidates and plans nothing. A plan holds every candidate of
//! each approved rule plus any path named on its own, minus the drive's
//! persisted exclusions (`--exclude` / `--unexclude` / `--exclusions`).
//! Dry-run is the default. `--apply` is the only thing that moves a byte, and
//! it deletes the planned paths permanently: no recycle bin, no undo.

use std::collections::BTreeSet;
use std::io::{IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use humansize::{BINARY, format_size};
use nomnom_core::action::{
    Action, ApplyRecord, ApplyReport, Approval, Exclusions, Plan, RecordStatus, RuleGroup,
    apply_with, by_rule, candidates, find_rule, plain, plan_from,
};
use nomnom_core::scan::VolumeRoot;
use nomnom_core::verdict::{Assessment, Disposition, Entry, Verdict};
use serde::Serialize;

use crate::input;
use crate::suggest::disposition_marker;

pub struct Request<'a> {
    pub drive: &'a VolumeRoot,
    pub show_errors: bool,
    /// The `--pack <DIR>` arguments, in the order they were given.
    pub packs: &'a [PathBuf],
    /// Candidates the user picked one by one.
    pub paths: &'a [PathBuf],
    /// The `--rule` arguments: rules whose every candidate is picked.
    pub rules: &'a [String],
    pub edits: Edits<'a>,
    pub apply: bool,
    pub include_review: bool,
    pub json: bool,
}

/// Changes to the drive's persisted exclusion list, made before any scan.
pub struct Edits<'a> {
    pub exclude: &'a [PathBuf],
    pub unexclude: &'a [PathBuf],
    /// `--exclusions`: print the list.
    pub list: bool,
}

impl Edits<'_> {
    fn any(&self) -> bool {
        self.list || !self.exclude.is_empty() || !self.unexclude.is_empty()
    }
}

pub fn run(request: Request<'_>) -> Result<ExitCode> {
    let mode = Mode {
        paths: request.paths,
        rules: request.rules,
        apply: request.apply,
        include_review: request.include_review,
        json: request.json,
    };
    // Before the scan, so a refused command never costs a UAC prompt.
    mode.check()?;
    let root = request.drive.as_path();
    let mut out = std::io::stdout().lock();
    let exclusions = match edit_exclusions(root, &request.edits, &mode, &mut out)? {
        Edited::Done(code) => return Ok(code),
        Edited::Continue(exclusions) => exclusions,
    };
    let packs = nomnom_core::verdict::resolve_packs(root, request.packs)?;
    let (catalog, assessment) = input::load_assessed(request.drive, packs)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, request.show_errors);

    execute(assessment, &exclusions, mode, &mut out)
}

/// What to do with a catalog once it is judged.
struct Mode<'a> {
    paths: &'a [PathBuf],
    rules: &'a [String],
    apply: bool,
    include_review: bool,
    json: bool,
}

impl Mode<'_> {
    fn picks(&self) -> bool {
        !self.paths.is_empty() || !self.rules.is_empty()
    }

    /// `--apply` acts only on what the user approved; there is no "apply all".
    fn check(&self) -> Result<()> {
        if self.apply && !self.picks() {
            bail!(
                "--apply needs something approved: nomnom clean <DRIVE> --rule \"<Title>\" \
                 --apply, or name the paths (run `nomnom clean <DRIVE>` to list the candidates \
                 by rule)"
            );
        }
        Ok(())
    }
}

enum Edited {
    /// Only the exclusion list was asked about; nothing is scanned.
    Done(ExitCode),
    Continue(Exclusions),
}

/// Loads the drive's exclusions and applies `edits`, saving once and only when
/// every edit is valid.
fn edit_exclusions(
    root: &Path,
    edits: &Edits<'_>,
    mode: &Mode<'_>,
    out: &mut dyn Write,
) -> Result<Edited> {
    let mut exclusions = Exclusions::load(root)?;
    let mut notes = Vec::new();
    for path in edits.exclude {
        if exclusions.add(root, path)? {
            notes.push(format!("excluded {}", path.display()));
        } else {
            notes.push(format!("already excluded: {}", path.display()));
        }
    }
    let mut missing = Vec::new();
    for path in edits.unexclude {
        if exclusions.remove(path) {
            notes.push(format!("no longer excluded: {}", path.display()));
        } else {
            missing.push(path.display().to_string());
        }
    }
    if !missing.is_empty() {
        bail!(
            "not on the exclusion list of {}: {}\n(run `nomnom clean <DRIVE> --exclusions` to \
             list it)",
            plain(root),
            missing.join(", ")
        );
    }
    if !edits.exclude.is_empty() || !edits.unexclude.is_empty() {
        exclusions.save(root)?;
    }

    // An edit or a listing alone is answered without a scan.
    if edits.any() && !mode.picks() {
        report_exclusions(root, &exclusions, &notes, mode.json, out)?;
        return Ok(Edited::Done(ExitCode::SUCCESS));
    }
    for note in notes {
        eprintln!("{note}");
    }
    Ok(Edited::Continue(exclusions))
}

fn report_exclusions(
    root: &Path,
    exclusions: &Exclusions,
    notes: &[String],
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let file = Exclusions::path_in(root);
    if json {
        let report = ExclusionsOutput {
            file: plain(&file),
            exclusions: exclusions.paths().map(plain).collect(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(());
    }
    for note in notes {
        writeln!(out, "{note}")?;
    }
    writeln!(out, "Exclusions in {} ({}):", plain(&file), exclusions.paths().len())?;
    for path in exclusions.paths() {
        writeln!(out, "  {}", plain(path))?;
    }
    if exclusions.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        writeln!(out, "Undo one: nomnom clean <DRIVE> --unexclude <PATH>")?;
    }
    Ok(())
}

fn execute(
    assessment: Assessment,
    exclusions: &Exclusions,
    mode: Mode<'_>,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    mode.check()?;
    if !mode.picks() {
        let groups = by_rule(&candidates(&assessment, mode.include_review));
        if mode.json {
            return report_candidates_json(&assessment.root, &groups, exclusions, out);
        }
        report_candidates(&assessment.root, &groups, exclusions, out)?;
        return report_hints(groups.is_empty(), exclusions, mode, out);
    }
    let offered = candidates(&assessment, mode.include_review);
    let groups = by_rule(&offered);
    let approval = Approval {
        rules: approve_rules(&groups, &by_rule(&candidates(&assessment, true)), mode.rules)?,
        paths: select(&offered, &candidates(&assessment, true), exclusions, mode.paths)?,
    };
    let (plan, refused) = plan_from(&assessment, &approval, exclusions, mode.include_review)
        .with_context(|| format!("cannot anchor a plan at {}", assessment.root.display()))?;
    // A guard refusal is information, not a stop: the other actions are still
    // sound, and the user can act on the named path.
    for (path, error) in refused {
        eprintln!("skipping {}: {error}", path.display());
    }

    if mode.apply {
        // Each path's line prints the moment it is done, the GUI's live log.
        let mut meter = ApplyMeter::start(&plan);
        let mut written = Ok(());
        let report = apply_with(&plan, |record| {
            meter.clear();
            if !mode.json && written.is_ok() {
                written = write_record(out, record);
            }
            meter.advance(record);
        })
        .context("apply failed")?;
        drop(meter);
        written?;
        return report_apply(&report, mode.json, out);
    }

    report_plan(&plan, &approval, mode.json, out)
}

/// The rules each `--rule` names. Every one must have candidates under the
/// current dispositions; one that does not fails the whole command, naming
/// it, rather than being skipped.
fn approve_rules(
    groups: &[RuleGroup<'_>],
    widened: &[RuleGroup<'_>],
    named: &[String],
) -> Result<BTreeSet<nomnom_core::verdict::Provenance>> {
    let mut rules = BTreeSet::new();
    let mut rejected = Vec::new();
    for text in named {
        match find_rule(groups, text) {
            Ok(rule) => {
                rules.insert(rule);
            }
            Err(error) => match find_rule(widened, text) {
                Ok(rule) => rejected.push(format!(
                    "{rule}: its candidates are `review` verdicts; add --include-review to \
                     approve it"
                )),
                Err(_) => rejected.push(error.to_string()),
            },
        }
    }
    if !rejected.is_empty() {
        let known: Vec<String> = groups.iter().map(|group| group.provenance.to_string()).collect();
        bail!(
            "refusing to approve rules with no candidates:\n  {}\nrules with candidates here: {}",
            rejected.join("\n  "),
            if known.is_empty() { "(none)".to_string() } else { known.join(", ") }
        );
    }
    Ok(rules)
}

/// The entries of `offered` the user named, by the path the assessment holds.
///
/// Every named path must be a candidate under the current dispositions and
/// not excluded; one that is not fails the whole command, naming it, rather
/// than being skipped — a silent skip would apply a plan the user did not
/// write.
fn select(
    offered: &[&Entry],
    widened: &[&Entry],
    exclusions: &Exclusions,
    named: &[PathBuf],
) -> Result<BTreeSet<PathBuf>> {
    let mut selection = BTreeSet::new();
    let mut rejected = Vec::new();
    for path in named {
        if let Some(entry) = find(offered, path) {
            match exclusions.covering(Path::new(&entry.path)) {
                Some(by) => rejected.push(format!(
                    "{}: excluded by {}; run `nomnom clean <DRIVE> --unexclude {}` first",
                    path.display(),
                    plain(by),
                    plain(by)
                )),
                None => {
                    selection.insert(PathBuf::from(&entry.path));
                }
            }
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

fn report_candidates_json(
    root: &Path,
    groups: &[RuleGroup<'_>],
    exclusions: &Exclusions,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    let excluded_by = |entry: &Entry| exclusions.covering(Path::new(&entry.path)).map(plain);
    let rules: Vec<RuleOutput<'_>> = groups
        .iter()
        .map(|group| RuleOutput {
            rule: group.provenance.to_string(),
            pack: &group.provenance.pack,
            title: &group.provenance.rule,
            bytes: group.bytes,
            matches: group
                .entries
                .iter()
                .map(|entry| MatchOutput {
                    path: &entry.path,
                    bytes: entry.bytes,
                    verdict: &entry.verdict,
                    excluded_by: excluded_by(entry),
                })
                .collect(),
        })
        .collect();
    let report = CandidatesOutput {
        root: plain(root),
        applied: false,
        total_bytes: 0,
        entries: &[],
        rules,
        exclusions: exclusions.paths().map(plain).collect(),
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    Ok(ExitCode::SUCCESS)
}

/// The first phase: the rules' candidates.
fn report_candidates(
    root: &Path,
    groups: &[RuleGroup<'_>],
    exclusions: &Exclusions,
    out: &mut dyn Write,
) -> Result<()> {
    writeln!(out, "Dry run — no rule is approved and nothing has been touched.")?;
    writeln!(out, "Root: {}", plain(root))?;
    if groups.is_empty() {
        writeln!(out, "No rule has candidates.")?;
        return Ok(());
    }
    writeln!(out)?;
    writeln!(out, "Rules with candidates ({}):", groups.len())?;
    write_groups(groups, exclusions, out)
}

fn write_groups(
    groups: &[RuleGroup<'_>],
    exclusions: &Exclusions,
    out: &mut dyn Write,
) -> Result<()> {
    let excluded_by = |entry: &Entry| exclusions.covering(Path::new(&entry.path)).map(plain);
    for group in groups {
        let excluded = group.entries.iter().filter(|entry| excluded_by(entry).is_some()).count();
        let tally = if excluded > 0 {
            format!("{} matches, {excluded} excluded", group.entries.len())
        } else {
            format!("{} matches", group.entries.len())
        };
        writeln!(out, "  {}  {tally}  {}", group.provenance, format_size(group.bytes, BINARY))?;
        for entry in &group.entries {
            let tag = match excluded_by(entry) {
                Some(_) => Some("excluded"),
                None => disposition_marker(entry.verdict.disposition),
            };
            let tag = tag.map_or_else(String::new, |tag| format!("[{tag}] "));
            writeln!(out, "    {tag}{}  {}", entry.path, format_size(entry.bytes, BINARY))?;
            writeln!(out, "        {}", entry.verdict.reason)?;
            if let Some(by) = excluded_by(entry) {
                writeln!(out, "        kept out of every plan by the exclusion {by}")?;
            }
            if let Some(capped) = &entry.verdict.capped {
                writeln!(out, "        ! {capped}")?;
            }
        }
    }
    Ok(())
}

fn report_hints(
    nothing: bool,
    exclusions: &Exclusions,
    mode: Mode<'_>,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    if !nothing {
        writeln!(out)?;
        writeln!(out, "Approve a rule: nomnom clean <DRIVE> --rule \"<Title>\" [--apply]")?;
        writeln!(out, "Keep a path out of every plan: nomnom clean <DRIVE> --exclude <PATH>")?;
    }
    if !exclusions.is_empty() {
        writeln!(
            out,
            "({} exclusions on this drive; --exclusions lists them.)",
            exclusions.paths().len()
        )?;
    }
    if !mode.include_review {
        writeln!(out, "(--include-review would also offer paths the evidence does not carry.)")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report_plan(
    plan: &Plan,
    approval: &Approval,
    json: bool,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    if json {
        let report = PlanOutput {
            root: plan.root().display().to_string(),
            applied: false,
            approved_rules: approval.rules.iter().map(ToString::to_string).collect(),
            total_bytes: plan.total_bytes(),
            entries: plan.actions(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(
        out,
        "Dry run — nothing has been touched. Add --apply to delete these permanently; \
         this cannot be undone."
    )?;
    writeln!(out, "Root: {}", plain(plan.root()))?;
    for rule in &approval.rules {
        writeln!(out, "Approved: {rule}")?;
    }
    if plan.is_empty() {
        writeln!(out, "Nothing to clean: every match is excluded or was refused above.")?;
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
        writeln!(out)?;
        writeln!(out, "Reclaimed {}.", format_size(report.bytes_reclaimed(), BINARY))?;
        if failures > 0 {
            writeln!(out)?;
            writeln!(out, "{failures} actions failed; each is marked FAILED above.")?;
        }
    }
    Ok(if failures > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn write_record(out: &mut dyn Write, record: &ApplyRecord) -> std::io::Result<()> {
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
    writeln!(out, "      {}", record.reason)
}

/// `--apply` on a terminal (or under [`input::FORCE_METER_ENV`]): one
/// rewritten stderr line under the log, paths and bytes done of the plan's
/// totals — the GUI's apply bar. Cleared before each log line and on drop.
struct ApplyMeter {
    enabled: bool,
    total: usize,
    total_bytes: u64,
    done: usize,
    bytes: u64,
}

impl ApplyMeter {
    const WIDTH: usize = 72;
    const BAR: usize = 24;

    fn start(plan: &Plan) -> Self {
        let forced = std::env::var_os(input::FORCE_METER_ENV).is_some_and(|v| v == "1");
        let meter = Self {
            enabled: forced || std::io::stderr().is_terminal(),
            total: plan.len(),
            total_bytes: plan.total_bytes(),
            done: 0,
            bytes: 0,
        };
        meter.draw();
        meter
    }

    fn advance(&mut self, record: &ApplyRecord) {
        self.done += 1;
        self.bytes += record.bytes;
        self.draw();
    }

    fn draw(&self) {
        if self.enabled {
            eprint!("\r{:<width$}", self.line(), width = Self::WIDTH);
            let _ = std::io::stderr().flush();
        }
    }

    fn clear(&self) {
        if self.enabled {
            eprint!("\r{:width$}\r", "", width = Self::WIDTH);
        }
    }

    fn line(&self) -> String {
        let filled = (self.done * Self::BAR).checked_div(self.total).unwrap_or(Self::BAR);
        format!(
            "[{}{}] {}/{} paths  {} / {}",
            "#".repeat(filled),
            ".".repeat(Self::BAR - filled),
            self.done,
            self.total,
            format_size(self.bytes, BINARY),
            format_size(self.total_bytes, BINARY)
        )
    }
}

impl Drop for ApplyMeter {
    fn drop(&mut self) {
        self.clear();
    }
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Delete { .. } => "delete ",
        Action::Archive { .. } => "archive",
        Action::Move { .. } => "move   ",
    }
}

fn kind_verb(kind: nomnom_core::action::ActionKind) -> &'static str {
    use nomnom_core::action::ActionKind;
    match kind {
        ActionKind::Delete => "delete ",
        ActionKind::Archive => "archive",
        ActionKind::Move => "move   ",
    }
}

#[derive(Serialize)]
struct PlanOutput<'a> {
    root: String,
    applied: bool,
    approved_rules: Vec<String>,
    total_bytes: u64,
    /// The core's own entries, reason included — nothing is stitched on here.
    entries: &'a [nomnom_core::action::PlanEntry],
}

/// A plan's shape with nothing approved, plus what the user may approve.
#[derive(Serialize)]
struct CandidatesOutput<'a> {
    root: String,
    applied: bool,
    total_bytes: u64,
    entries: &'a [nomnom_core::action::PlanEntry],
    rules: Vec<RuleOutput<'a>>,
    exclusions: Vec<String>,
}

#[derive(Serialize)]
struct RuleOutput<'a> {
    /// `pack [Title]`, what `--rule` takes.
    rule: String,
    pack: &'a str,
    title: &'a str,
    bytes: u64,
    matches: Vec<MatchOutput<'a>>,
}

#[derive(Serialize)]
struct MatchOutput<'a> {
    path: &'a str,
    bytes: u64,
    verdict: &'a Verdict,
    /// The exclusion keeping this match out of every plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_by: Option<String>,
}

#[derive(Serialize)]
struct ExclusionsOutput {
    file: String,
    exclusions: Vec<String>,
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

    use nomnom_core::verdict::{assess, resolve_packs};

    use super::*;
    use crate::scan_fixtures::{catalog_of, write};
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

    fn clean_with(
        root: &Path,
        paths: &[PathBuf],
        rules: &[String],
        apply: bool,
    ) -> Result<(ExitCode, String)> {
        isolated_store();
        let packs = resolve_packs(root, &[]).expect("packs resolve");
        let exclusions = Exclusions::load(root).expect("exclusions load");
        let mode = Mode { paths, rules, apply, include_review: false, json: false };
        let mut out = Vec::new();
        let catalog = catalog_of(root);
        let code = execute(assess(&catalog, packs), &exclusions, mode, &mut out)?;
        Ok((code, String::from_utf8(out).expect("utf-8 output")))
    }

    fn clean(root: &Path, paths: &[PathBuf], apply: bool) -> Result<(ExitCode, String)> {
        clean_with(root, paths, &[], apply)
    }

    fn dry_run(root: &Path) -> (ExitCode, String) {
        clean(root, &[], false).expect("clean runs")
    }

    /// Two Cargo projects, each with a `target/`.
    fn cargo_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for project in ["alpha", "beta"] {
            let root = dir.path().join(project);
            write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n");
            write(root.join("src/main.rs"), b"fn main() {}\n");
            write(root.join("target/.rustc_info.json"), b"{}");
            write(root.join("target/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55");
            write(root.join("target/debug/x.exe"), b"binary");
        }
        dir
    }

    /// The opt-in contract: `clean` must never act on what the user did not
    /// approve. Catches `--apply` with no rule and no paths deleting every
    /// candidate, and a named non-candidate (here the project's own
    /// `package.json`, or a typo) reaching the plan instead of failing the
    /// command with its name — both checked with `--apply` on, and the tree
    /// compared afterwards.
    #[test]
    fn apply_with_nothing_approved_and_non_candidate_paths_are_rejected() {
        let dir = node_fixture();
        let before = snapshot(dir.path());

        let error = clean(dir.path(), &[], true).expect_err("--apply with nothing approved ran");
        assert!(format!("{error:#}").contains("--apply needs something approved"), "{error:#}");

        let manifest = dir.path().join("package.json");
        let typo = dir.path().join("node_modulez");
        let error = clean(dir.path(), &[manifest.clone(), typo.clone()], true)
            .expect_err("a non-candidate path was planned");
        let message = format!("{error:#}");
        assert!(message.contains(&manifest.display().to_string()), "{message}");
        assert!(message.contains(&typo.display().to_string()), "{message}");

        let error = clean_with(dir.path(), &[], &["no such rule".into()], true)
            .expect_err("an unknown rule was approved");
        assert!(format!("{error:#}").contains("no such rule"), "{error:#}");

        assert_eq!(before, snapshot(dir.path()), "a rejected clean modified the tree");
    }

    /// Catches a named candidate failing to reach the plan, which would make
    /// picking a single path unable to clean anything.
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

    /// Catches `--rule` by bare title not planning every match of the rule,
    /// and an `--exclude`d project's `target/` still reaching the plan.
    #[test]
    fn an_approved_rule_plans_its_matches_minus_exclusions() {
        let dir = cargo_fixture();
        let root = dir.path();
        let rule = ["Cargo target directory".to_string()];

        let (_, text) = clean_with(root, &[], &rule, false).expect("clean runs");
        assert!(text.contains("2 actions"), "{text}");

        let edits = Edits { exclude: &[root.join("beta")], unexclude: &[], list: false };
        let mode =
            Mode { paths: &[], rules: &rule, apply: false, include_review: false, json: false };
        assert!(matches!(
            edit_exclusions(root, &edits, &mode, &mut Vec::new()).unwrap(),
            Edited::Continue(_)
        ));
        let (_, text) = clean_with(root, &[], &rule, false).expect("clean runs");
        assert!(text.contains("1 actions"), "{text}");
        assert!(text.contains(&root.join("alpha").join("target").display().to_string()), "{text}");
        assert!(!text.contains(&root.join("beta").join("target").display().to_string()), "{text}");

        let (_, listing) = dry_run(root);
        assert!(listing.contains("[excluded]"), "{listing}");
    }

    /// Catches `--unexclude` of a path that is not on the list succeeding
    /// silently, and an exclusion-only command scanning the drive.
    #[test]
    fn exclusion_edits_alone_answer_without_a_plan() {
        let dir = cargo_fixture();
        let root = dir.path();
        let mode =
            Mode { paths: &[], rules: &[], apply: false, include_review: false, json: false };
        let mut out = Vec::new();
        let edits = Edits { exclude: &[root.join("beta")], unexclude: &[], list: false };
        assert!(matches!(
            edit_exclusions(root, &edits, &mode, &mut out).unwrap(),
            Edited::Done(ExitCode::SUCCESS)
        ));
        assert!(String::from_utf8(out).unwrap().contains("Exclusions in"));

        let edits = Edits { exclude: &[], unexclude: &[root.join("alpha")], list: false };
        assert!(edit_exclusions(root, &edits, &mode, &mut Vec::new()).is_err());

        let edits = Edits { exclude: &[], unexclude: &[root.join("beta")], list: false };
        edit_exclusions(root, &edits, &mode, &mut Vec::new()).unwrap();
        assert!(Exclusions::load(root).unwrap().is_empty());
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
        assert!(text.contains("no rule is approved"), "{text}");

        assert_eq!(before, snapshot(dir.path()), "dry run modified the tree");
    }
}
