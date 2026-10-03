//! `nomnom classify` — which pack owns which subtree, and how the drive splits
//! into recognized and other bytes. The GUI's "Recognized" view.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::VolumeRoot;
use nomnom_core::verdict::{DropReason, DroppedClaim, Ownership, PackClaims, resolve_packs};
use serde::Serialize;

use crate::input;

pub fn run(
    drive: &VolumeRoot,
    show_errors: bool,
    explicit: &[PathBuf],
    json: bool,
) -> Result<ExitCode> {
    let packs = resolve_packs(drive.as_path(), explicit)?;
    let (catalog, assessment) = input::load_assessed(drive, packs)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);
    render(&catalog, &assessment.ownership, json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct Output<'a> {
    root: String,
    total_bytes: u64,
    claimed_bytes: u64,
    other_bytes: u64,
    packs: Vec<PackClaims>,
    dropped: &'a [DroppedClaim],
}

pub fn render(
    catalog: &Catalog,
    ownership: &Ownership,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let root = catalog.root();
    let packs = ownership.recognized(catalog);
    if json {
        let report = Output {
            root: catalog.path(root).display().to_string(),
            total_bytes: catalog.node(root).subtree_size,
            claimed_bytes: ownership.claimed(root),
            other_bytes: ownership.arbitrary(catalog, root),
            packs,
            dropped: ownership.dropped(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(());
    }

    writeln!(
        out,
        "{}  {}: recognized {}, other files {}",
        catalog.path(root).display(),
        format_size(catalog.node(root).subtree_size, BINARY),
        format_size(ownership.claimed(root), BINARY),
        format_size(ownership.arbitrary(catalog, root), BINARY),
    )?;
    writeln!(out)?;
    print_recognized(&packs, out)?;
    print_dropped(ownership.dropped(), out)
}

/// Pack › claim, biggest first: what `nomnom scan --view recognized` prints
/// too.
pub fn print_recognized(packs: &[PackClaims], out: &mut dyn Write) -> Result<()> {
    if packs.is_empty() {
        writeln!(out, "Nothing recognized.")?;
        return Ok(());
    }
    writeln!(out, "Recognized")?;
    for pack in packs {
        writeln!(
            out,
            "  {} — {} across {} {}",
            pack.pack,
            format_size(pack.bytes, BINARY),
            pack.claims.len(),
            if pack.claims.len() == 1 { "claim" } else { "claims" }
        )?;
        for row in &pack.claims {
            let nested = if row.nested { " (inside another of this pack's claims)" } else { "" };
            writeln!(out, "    {}  {}{nested}", row.path, format_size(row.bytes, BINARY))?;
            writeln!(out, "        {} — [{}]", row.claim.class, row.claim.provenance.rule)?;
        }
    }
    Ok(())
}

fn print_dropped(dropped: &[DroppedClaim], out: &mut dyn Write) -> Result<()> {
    if dropped.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    writeln!(out, "Dropped claims")?;
    for claim in dropped {
        let why = match &claim.reason {
            DropReason::Outranked { by } => format!("outranked by {by}"),
            DropReason::Contested { with } => format!("contested with {with}; neither kept"),
            DropReason::Inside { owner } => format!("inside {owner}'s exclusive claim"),
        };
        writeln!(out, "  {}  {} — {why}", claim.path, claim.claim.provenance)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nomnom_core::verdict::{assess, resolve_packs};

    use super::*;
    use crate::scan_fixtures::catalog_of;
    use crate::test_support::{isolated_store, node_fixture};

    // Catches `classify` printing a split that does not add up to the drive,
    // or dropping the claim a built-in pack made.
    #[test]
    fn classify_json_splits_the_total_and_names_the_claim() {
        isolated_store();
        let dir = node_fixture();
        let packs = resolve_packs(dir.path(), &[]).expect("packs resolve");
        let catalog = catalog_of(dir.path());
        let assessment = assess(&catalog, packs);
        let mut out = Vec::new();
        render(&catalog, &assessment.ownership, true, &mut out).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
        let total = parsed["total_bytes"].as_u64().unwrap();
        let claimed = parsed["claimed_bytes"].as_u64().unwrap();
        let other = parsed["other_bytes"].as_u64().unwrap();
        assert_eq!(claimed + other, total);
        assert!(claimed > 0, "node_modules was not claimed: {parsed:#}");
        let class = parsed["packs"][0]["claims"][0]["claim"]["class"].as_str().unwrap();
        assert!(class.ends_with(":build-output/v1"), "{class}");
    }
}
