//! `nomnom undo` — put back what an apply moved, and say what it could not.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};
use nomnom_core::action::plain;

pub fn run(journal: &Path, json: bool) -> Result<ExitCode> {
    run_to(journal, json, &mut std::io::stdout().lock())
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
