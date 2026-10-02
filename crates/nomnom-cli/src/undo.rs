//! `nomnom undo` — put back what an apply moved, and say what it could not.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};
use nomnom_core::action::{JournalEntry, list_journals, plain};
use serde::Serialize;

/// With a journal, undo that apply; without one, list the journals there are
/// to undo, as the GUI's Undo screen does.
pub fn run(journal: Option<&Path>, json: bool) -> Result<ExitCode> {
    let out = &mut std::io::stdout().lock();
    match journal {
        Some(journal) => run_to(journal, json, out),
        None => {
            list_to(&list_journals().context("cannot list journals")?, json, out)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn list_to(journals: &[JournalEntry], json: bool, out: &mut dyn Write) -> Result<()> {
    if json {
        let rows: Vec<JournalRow> = journals
            .iter()
            .map(|entry| {
                let (summary, error) = match &entry.summary {
                    Ok(summary) => (Some(summary), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                JournalRow {
                    path: plain(&entry.path),
                    started_at: entry.started_at,
                    root: summary.map(|s| plain(&s.root)),
                    actions: summary.map(|s| s.actions),
                    succeeded: summary.map(|s| s.succeeded),
                    failed: summary.map(|s| s.failed),
                    undone: summary.map(|s| s.undone),
                    bytes_reclaimed: summary.map(|s| s.bytes_reclaimed),
                    error,
                }
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }
    if journals.is_empty() {
        writeln!(out, "No journals: no apply has run yet.")?;
        return Ok(());
    }
    for entry in journals {
        let when = chrono::DateTime::from_timestamp(entry.started_at as i64, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into());
        writeln!(out, "{when}  {}", plain(&entry.path))?;
        match &entry.summary {
            Ok(s) => writeln!(
                out,
                "      {}: {} reclaimed, {} of {} actions done, {} failed, {} undone",
                plain(&s.root),
                format_size(s.bytes_reclaimed, BINARY),
                s.succeeded,
                s.actions,
                s.failed,
                s.undone
            )?,
            Err(error) => writeln!(out, "      unreadable: {error}")?,
        }
    }
    writeln!(out)?;
    writeln!(out, "Undo one with: nomnom undo <journal>")?;
    Ok(())
}

#[derive(Serialize)]
struct JournalRow {
    path: String,
    started_at: u64,
    root: Option<String>,
    actions: Option<usize>,
    succeeded: Option<usize>,
    failed: Option<usize>,
    undone: Option<usize>,
    bytes_reclaimed: Option<u64>,
    /// Set for a journal this version cannot read; the row stays listed.
    error: Option<String>,
}

pub(crate) fn run_to(journal: &Path, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let report = nomnom_core::action::undo(journal)
        .with_context(|| format!("cannot undo {}", journal.display()))?;

    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        writeln!(out, "Journal: {}", plain(&report.journal_path))?;
        for restored in &report.restored {
            writeln!(out, "restored  {}", plain(&restored.path))?;
            writeln!(out, "      {}", restored.reason)?;
        }
        for skipped in &report.skipped {
            writeln!(out, "skipped   {}  ({})", plain(&skipped.path), skipped.reason)?;
        }
        for conflict in &report.conflicts {
            writeln!(out, "CONFLICT  {}  {}", plain(&conflict.path), conflict.message)?;
        }
        for failure in &report.failures {
            writeln!(out, "FAILED    {}  {}", plain(&failure.path), failure.message)?;
        }
        writeln!(out)?;
        writeln!(
            out,
            "Restored {} across {} paths.",
            format_size(report.bytes_restored, BINARY),
            report.restored.len()
        )?;
        if !report.is_clean() {
            writeln!(
                out,
                "{} conflicts, {} failures — those paths are still where the apply left them.",
                report.conflicts.len(),
                report.failures.len()
            )?;
        }
    }

    Ok(if report.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
