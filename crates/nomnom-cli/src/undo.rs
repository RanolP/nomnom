//! `nomnom undo` — put back what an apply moved, and say what it could not.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};

use crate::paths::plain;

pub fn run(journal: &Path, json: bool) -> Result<ExitCode> {
    let report = nomnom_core::action::undo(journal)
        .with_context(|| format!("cannot undo {}", journal.display()))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Journal: {}", plain(&report.journal_path));
        for restored in &report.restored {
            println!("restored  {}", plain(&restored.path));
            println!("      {}", restored.reason);
        }
        for skipped in &report.skipped {
            println!("skipped   {}  ({})", plain(&skipped.path), skipped.reason);
        }
        for conflict in &report.conflicts {
            println!("CONFLICT  {}  {}", plain(&conflict.path), conflict.message);
        }
        for failure in &report.failures {
            println!("FAILED    {}  {}", plain(&failure.path), failure.message);
        }
        println!();
        println!(
            "Restored {} across {} paths.",
            format_size(report.bytes_restored, BINARY),
            report.restored.len()
        );
        if !report.is_clean() {
            println!(
                "{} conflicts, {} failures — those paths are still where the apply left them.",
                report.conflicts.len(),
                report.failures.len()
            );
        }
    }

    Ok(if report.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
