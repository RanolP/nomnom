//! `nomnom types` — a drive's bytes and file counts per extension.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::{Catalog, FileType, file_types};
use nomnom_core::scan::VolumeRoot;
use serde::Serialize;

use crate::input::{self, BackendArg};

pub fn run(
    drive: &VolumeRoot,
    backend: BackendArg,
    show_errors: bool,
    json: bool,
) -> Result<ExitCode> {
    let catalog = input::load(drive, backend)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);
    render(&catalog, json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn render(catalog: &Catalog, json: bool, out: &mut dyn Write) -> Result<()> {
    let types = file_types(catalog);
    let total: u64 = types.iter().map(|t| t.bytes).sum();
    let percent =
        |t: &FileType| if total == 0 { 0.0 } else { t.bytes as f64 * 100.0 / total as f64 };

    if json {
        let rows: Vec<Row> = types
            .iter()
            .map(|t| Row {
                ext: &t.ext,
                bytes: t.bytes,
                allocated_bytes: t.allocated,
                percent: percent(t),
                count: t.count,
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }
    for t in &types {
        writeln!(
            out,
            "{:<12} {:>12} {:>6.2}%  {} {}",
            t.ext,
            format_size(t.bytes, BINARY),
            percent(t),
            t.count,
            if t.count == 1 { "file" } else { "files" }
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Row<'a> {
    ext: &'a str,
    bytes: u64,
    allocated_bytes: Option<u64>,
    percent: f64,
    count: u64,
}
