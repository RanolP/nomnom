//! `nomnom largest` — a drive's biggest files, wherever they sit.

use std::io::Write;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::{Catalog, largest_files};
use nomnom_core::scan::VolumeRoot;
use serde::Serialize;

use crate::input::{self, BackendArg};

pub fn run(
    drive: &VolumeRoot,
    backend: BackendArg,
    show_errors: bool,
    n: usize,
    json: bool,
) -> Result<ExitCode> {
    let catalog = input::load(drive, backend)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);
    render(&catalog, n, json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn render(catalog: &Catalog, n: usize, json: bool, out: &mut dyn Write) -> Result<()> {
    let largest = largest_files(catalog, n);
    if json {
        let rows: Vec<Row> = largest
            .iter()
            .map(|&id| {
                let node = catalog.node(id);
                Row {
                    path: catalog.path(id).display().to_string(),
                    bytes: node.size,
                    allocated_bytes: node.allocated,
                    modified: node.modified.map(|time| local(time).to_rfc3339()),
                }
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }
    for &id in &largest {
        let node = catalog.node(id);
        let modified = node
            .modified
            .map_or_else(|| "-".repeat(16), |t| local(t).format("%Y-%m-%d %H:%M").to_string());
        writeln!(
            out,
            "{:>12}  {modified}  {}",
            format_size(node.size, BINARY),
            catalog.path(id).display()
        )?;
    }
    Ok(())
}

fn local(time: SystemTime) -> chrono::DateTime<chrono::Local> {
    time.into()
}

#[derive(Serialize)]
struct Row {
    path: String,
    bytes: u64,
    allocated_bytes: Option<u64>,
    modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_fixtures::{catalog_of, write};

    /// Catches the CLI ignoring `-n` or losing the core's biggest-first order,
    /// either of which puts the wrong files in front of the user.
    #[test]
    fn largest_honours_n_biggest_first() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path().join("a/small.bin"), &[0; 10]);
        write(dir.path().join("b/big.bin"), &[0; 300]);
        write(dir.path().join("mid.bin"), &[0; 100]);

        let mut out = Vec::new();
        render(&catalog_of(dir.path()), 2, true, &mut out).unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0]["path"].as_str().unwrap().ends_with("big.bin"), "{rows:?}");
        assert_eq!(rows[0]["bytes"], 300);
        assert!(rows[1]["path"].as_str().unwrap().ends_with("mid.bin"), "{rows:?}");
        assert!(rows[0]["modified"].is_string(), "{rows:?}");
    }
}
