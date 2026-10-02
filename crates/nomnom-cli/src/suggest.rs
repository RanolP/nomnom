//! `nomnom suggest` — what each path is, and the sentence that justifies it.
//!
//! Every verdict is printed with its `reason`. That sentence is what a human
//! reads before approving a deletion, so a grouping that hides it would defeat
//! the design.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::verdict::{Assessment, Disposition, Group, Label, assess, resolve_packs};
use serde::Serialize;

use crate::input::{self, BackendArg};

pub fn run(
    path: &Path,
    backend: BackendArg,
    show_errors: bool,
    explicit: &[PathBuf],
    json: bool,
) -> Result<ExitCode> {
    let packs = resolve_packs(path, explicit)?;
    let catalog = input::load(path, backend)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);

    let Assessment { groups, reclaimable_bytes: reclaimable, .. } = assess(&catalog, packs);

    if json {
        let out = Output {
            root: catalog.path(catalog.root()).display().to_string(),
            reclaimable_bytes: reclaimable,
            groups: &groups,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }

    if groups.is_empty() {
        println!("Nothing to suggest under {}.", catalog.path(catalog.root()).display());
        return Ok(ExitCode::SUCCESS);
    }

    for group in &groups {
        println!(
            "{} — {} across {} {}",
            label_name(&group.label),
            format_size(group.bytes, BINARY),
            group.entries.len(),
            if group.entries.len() == 1 { "path" } else { "paths" }
        );
        for entry in &group.entries {
            println!(
                "  [{}] {}  {}",
                disposition_name(entry.verdict.disposition),
                entry.path,
                format_size(entry.bytes, BINARY)
            );
            println!("      {}", entry.verdict.reason);
            // The rule is printed beside its sentence, not hidden behind a
            // debug flag: with packs coming from the network, "who says so" is
            // part of what a human approves on.
            println!("      — {}", entry.verdict.provenance);
            // A downgraded verdict looks exactly like one the rule wrote as
            // `review`, so without this line the cap is invisible and the user
            // has no way to know a trust grant is what is missing.
            if let Some(capped) = &entry.verdict.capped {
                println!("      ! {capped}");
            }
        }
        println!();
    }
    println!("Reclaimable: {}", format_size(reclaimable, BINARY));
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct Output<'a> {
    root: String,
    reclaimable_bytes: u64,
    groups: &'a [Group],
}

/// A label reads as prose in the heading, so its hyphens become spaces.
///
/// Labels are open — a pack introduces its own — so there is no table to look
/// one up in, and there must not be: an unknown label has to print as itself
/// rather than as "unknown".
pub fn label_name(label: &Label) -> String {
    label.as_str().replace('-', " ")
}

pub fn disposition_name(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Keep => "keep",
        Disposition::Reclaimable => "reclaimable",
        Disposition::Review => "review",
    }
}
