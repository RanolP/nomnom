//! `nomnom drives` — the fixed drives a scan can take, with their capacity.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::scan::{Volume, volumes};
use serde::Serialize;

pub fn run(json: bool) -> Result<ExitCode> {
    render(&volumes(), json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn render(volumes: &[Volume], json: bool, out: &mut dyn Write) -> Result<()> {
    if json {
        let rows: Vec<Row> = volumes
            .iter()
            .map(|volume| Row {
                root: volume.root.display().to_string(),
                label: &volume.label,
                fs: &volume.fs,
                used_bytes: volume.total.saturating_sub(volume.free),
                free_bytes: volume.free,
                total_bytes: volume.total,
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }
    if volumes.is_empty() {
        writeln!(out, "No fixed drives found.")?;
        return Ok(());
    }
    for volume in volumes {
        let used = volume.total.saturating_sub(volume.free);
        let label = if volume.label.is_empty() { "(no label)" } else { &volume.label };
        writeln!(
            out,
            "{}  {label}  {}  {} used, {} free of {}",
            volume.root.display(),
            if volume.fs.is_empty() { "?" } else { &volume.fs },
            format_size(used, BINARY),
            format_size(volume.free, BINARY),
            format_size(volume.total, BINARY),
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Row<'a> {
    root: String,
    label: &'a str,
    fs: &'a str,
    used_bytes: u64,
    free_bytes: u64,
    total_bytes: u64,
}
