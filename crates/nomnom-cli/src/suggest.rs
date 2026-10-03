//! `nomnom suggest` — what each path is, and the sentence that justifies it.
//!
//! Every verdict is printed with its `reason`. That sentence is what a human
//! reads before approving a deletion, so a grouping that hides it would defeat
//! the design.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::VolumeRoot;
use nomnom_core::verdict::{
    Assessment, Disposition, Group, Label, TrustedPack, assess, resolve_packs,
};
use serde::Serialize;

use crate::input;

pub fn run(
    drive: &VolumeRoot,
    show_errors: bool,
    explicit: &[PathBuf],
    json: bool,
) -> Result<ExitCode> {
    let packs = resolve_packs(drive.as_path(), explicit)?;
    let catalog = input::load(drive)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);

    render(&catalog, packs, json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn render(
    catalog: &Catalog,
    packs: Vec<TrustedPack>,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let Assessment { groups, reclaimable_bytes, .. } = assess(catalog, packs);

    if json {
        let report = Output {
            root: catalog.path(catalog.root()).display().to_string(),
            reclaimable_bytes,
            groups: &groups,
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(());
    }

    if groups.is_empty() {
        writeln!(out, "Nothing to suggest under {}.", catalog.path(catalog.root()).display())?;
        return Ok(());
    }
    for group in &groups {
        print_group(out, &label_name(&group.label), group)?;
    }
    writeln!(out, "Reclaimable: {}", format_size(reclaimable_bytes, BINARY))?;
    Ok(())
}

fn print_group(out: &mut dyn Write, heading: &str, group: &Group) -> Result<()> {
    writeln!(
        out,
        "{heading} — {} across {} {}",
        format_size(group.bytes, BINARY),
        group.entries.len(),
        if group.entries.len() == 1 { "path" } else { "paths" }
    )?;
    for entry in &group.entries {
        writeln!(
            out,
            "  [{}] {}  {}",
            disposition_name(entry.verdict.disposition),
            entry.path,
            format_size(entry.bytes, BINARY)
        )?;
        writeln!(out, "      {}", entry.verdict.reason)?;
        // The rule is printed beside its sentence, not hidden behind a
        // debug flag: with packs coming from the network, "who says so" is
        // part of what a human approves on.
        writeln!(out, "      — {}", entry.verdict.provenance)?;
        // A downgraded verdict looks exactly like one the rule wrote as
        // `review`, so without this line the cap is invisible and the user
        // has no way to know a trust grant is what is missing.
        if let Some(capped) = &entry.verdict.capped {
            writeln!(out, "      ! {capped}")?;
        }
    }
    writeln!(out)?;
    Ok(())
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

/// Rendered from fixture catalogs: the public scan takes whole drives only.
#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::pack;
    use crate::scan_fixtures::catalog_of;
    use crate::test_support::{isolated_store, local_pack, marked_tree, node_fixture, pack_ok};

    fn suggest(project: &Path, explicit: &[PathBuf], json: bool) -> String {
        isolated_store();
        let packs = resolve_packs(project, explicit).expect("packs resolve");
        let mut out = Vec::new();
        render(&catalog_of(project), packs, json, &mut out).expect("suggest renders");
        String::from_utf8(out).expect("utf-8 output")
    }

    /// The JSON surface must stay the core's serde types, and a verdict without
    /// a reason must never reach a front-end: the reason is what a human
    /// approves on.
    #[test]
    fn suggest_json_carries_a_reason_for_every_verdict() {
        let dir = node_fixture();
        let parsed: serde_json::Value =
            serde_json::from_str(&suggest(dir.path(), &[], true)).expect("valid JSON");
        let groups = parsed["groups"].as_array().expect("groups array");
        assert!(!groups.is_empty(), "no verdicts on a fixture that has node_modules");

        let mut verdicts = 0;
        for group in groups {
            for entry in group["entries"].as_array().expect("entries array") {
                let reason = entry["verdict"]["reason"].as_str().expect("reason string");
                assert!(!reason.is_empty(), "empty reason for {}", entry["path"]);
                verdicts += 1;
            }
        }
        assert!(verdicts > 0, "no verdicts in the JSON");
    }

    /// The core judges `node_modules` beside a `package.json` correctly; this
    /// catches the CLI failing to surface what it judged.
    #[test]
    fn suggest_names_node_modules() {
        let dir = node_fixture();
        let text = suggest(dir.path(), &[], false);
        assert!(text.contains("node_modules"), "output did not name node_modules:\n{text}");
        assert!(text.contains("npm install"), "output did not carry the reason:\n{text}");
    }

    /// `docs/lang.md`: an untrusted pack's `reclaimable` "is downgraded, and
    /// the CLI says why". The regression that matters is not the downgrade — it
    /// is a downgrade the user cannot see, which is indistinguishable from a
    /// rule that wrote `review` itself and leaves them with no way to know a
    /// trust grant is what is missing.
    #[test]
    fn an_untrusted_packs_reclaimable_reaches_suggest_as_review_with_the_explanation() {
        let project = tempfile::tempdir().unwrap();
        marked_tree(project.path());
        let pack = local_pack(project.path(), "vendor", "reclaimable");

        let text = suggest(project.path(), &[pack], false);

        assert!(text.contains("[review]"), "{text}");
        assert!(text.contains("capped at review"), "{text}");
        assert!(text.contains("nomnom pack trust vendor"), "{text}");
        assert!(text.contains("Reclaimable: 0 B"), "a capped verdict must not be counted:\n{text}");
    }

    /// The grant has to change what the next suggest does, through the lock.
    /// The regression: recording trust somewhere `suggest` does not read, which
    /// makes `nomnom pack trust` a no-op the user cannot detect.
    #[test]
    fn pack_trust_lifts_the_cap_for_the_next_suggest() {
        let project = tempfile::tempdir().unwrap();
        marked_tree(project.path());
        let pack_dir = local_pack(project.path(), "vendor", "reclaimable");
        let explicit = [pack_dir.clone()];

        let granting = pack_ok(
            project.path(),
            pack::PackCommand::Trust {
                name: "vendor".into(),
                packs: explicit.to_vec(),
                json: false,
            },
        );
        // `docs/lang.md` makes trust "granted per pack, deliberately, once", so
        // the sentence on screen has to name the pack being trusted, not just
        // echo it.
        assert!(granting.contains("Trusting pack `vendor`"), "{granting}");
        assert!(
            granting.contains(pack_dir.to_str().unwrap()),
            "the grant must name where the pack came from:\n{granting}"
        );

        let text = suggest(project.path(), &explicit, false);
        assert!(text.contains("[reclaimable]"), "{text}");
        assert!(!text.contains("capped at review"), "{text}");

        let revoked = pack_ok(
            project.path(),
            pack::PackCommand::Untrust {
                name: "vendor".into(),
                packs: explicit.to_vec(),
                json: false,
            },
        );
        assert!(revoked.contains("Untrusted `vendor`"), "{revoked}");
        let after = suggest(project.path(), &explicit, false);
        assert!(after.contains("[review]"), "revoking must put the cap back:\n{after}");
    }
}
