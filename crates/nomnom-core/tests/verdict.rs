//! Judge behaviour over real scanned fixture trees.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{catalog_of, write};
use nomnom_core::action::{Approval, Exclusions, plan_from};
use nomnom_core::catalog::{Catalog, DuplicateProgress, NodeId};
use nomnom_core::verdict::{
    Assessment, Disposition, Label, TrustedPack, Verdict, assess, builtin_pack, find_duplicates,
    rollup,
};
use tempfile::TempDir;

/// Verdicts keyed by the path they were rendered about.
fn judged(root: &Path) -> Vec<(PathBuf, Verdict)> {
    let catalog = catalog_of(root);
    builtin_verdicts(&catalog)
        .into_iter()
        .map(|(id, verdict)| (catalog.path(id), verdict))
        .collect()
}

/// Both phases, as every front-end runs them: the rules, then the duplicates
/// merged in.
fn builtin_verdicts(catalog: &Catalog) -> Vec<(NodeId, Verdict)> {
    let assessment = full_assessment(catalog);
    let mut verdicts: Vec<(NodeId, Verdict)> = assessment
        .groups
        .into_iter()
        .flat_map(|group| group.entries)
        .map(|entry| (NodeId(entry.reach.expect("catalog entry").start), entry.verdict))
        .collect();
    verdicts.sort_by_key(|(id, _)| *id);
    verdicts
}

fn full_assessment(catalog: &Catalog) -> Assessment {
    let rules = rules_only(catalog);
    let duplicates =
        find_duplicates(catalog, &rules, &DuplicateProgress::default()).expect("not cancelled");
    rules.with_duplicates(catalog, &duplicates)
}

fn rules_only(catalog: &Catalog) -> Assessment {
    assess(catalog, vec![TrustedPack::builtin(builtin_pack().clone())])
}

/// The regression: a rescan that cannot stop the old duplicate pass, which
/// then lands its stale result over the new assessment. Cancelled, the pass
/// returns nothing at all.
#[test]
fn a_cancelled_duplicate_search_returns_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let blob = vec![b'c'; 1024 * 1024 + 1];
    write(tmp.path().join("a.bin"), &blob);
    write(tmp.path().join("b.bin"), &blob);
    let catalog = catalog_of(tmp.path());
    let rules = rules_only(&catalog);

    let progress = DuplicateProgress::default();
    progress.cancel();
    assert!(find_duplicates(&catalog, &rules, &progress).is_none());

    let found = find_duplicates(&catalog, &rules, &DuplicateProgress::default());
    assert_eq!(
        found.expect("not cancelled").entries.len(),
        2,
        "the uncancelled pass finds the pair"
    );
}

/// The regression: the duplicate result arriving as a fresh assessment, which
/// a front-end answers by clearing what the user approved while the pass ran.
/// Merged, every rule entry is unchanged, so an approval made against the
/// rules alone plans exactly the same paths afterwards.
#[test]
fn merging_duplicates_keeps_every_rule_entry_and_what_an_approval_plans() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write(root.join("package.json"), b"{}");
    write(root.join("node_modules/pkg/index.js"), b"1");
    let blob = vec![b'm'; 1024 * 1024 + 9];
    write(root.join("dups/a.bin"), &blob);
    write(root.join("dups/b.bin"), &blob);
    let catalog = catalog_of(root);

    let rules = rules_only(&catalog);
    let duplicates =
        find_duplicates(&catalog, &rules, &DuplicateProgress::default()).expect("not cancelled");
    let merged = rules.with_duplicates(&catalog, &duplicates);

    let rule_paths = |assessment: &Assessment| -> Vec<String> {
        let mut paths: Vec<String> = assessment
            .groups
            .iter()
            .filter(|group| group.label != Label::DUPLICATE)
            .flat_map(|group| group.entries.iter().map(|entry| entry.path.clone()))
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(rule_paths(&rules), rule_paths(&merged));
    assert!(merged.groups.iter().any(|group| group.label == Label::DUPLICATE));

    let node_modules = rules
        .groups
        .iter()
        .flat_map(|group| &group.entries)
        .find(|entry| entry.path.ends_with("node_modules"))
        .expect("node_modules is a rule entry");
    let approval = Approval {
        rules: [node_modules.verdict.provenance.clone()].into(),
        paths: Default::default(),
    };
    let planned = |assessment: &Assessment| -> Vec<PathBuf> {
        let (plan, refused) =
            plan_from(assessment, &approval, &Exclusions::default(), false).expect("plan");
        assert!(refused.is_empty(), "{refused:?}");
        plan.actions().iter().map(|entry| entry.action.path().to_path_buf()).collect()
    };
    assert_eq!(planned(&rules), planned(&merged));
}

fn verdict_for(judged: &[(PathBuf, Verdict)], suffix: impl AsRef<Path>) -> Option<&Verdict> {
    let suffix = suffix.as_ref();
    judged.iter().find(|(path, _)| path.ends_with(suffix)).map(|(_, verdict)| verdict)
}

/// A node_modules beside a package.json is the core rule; if it ever inverts,
/// the tool proposes deleting `src` and keeping the dependencies.
#[test]
fn node_modules_is_reclaimable_and_source_is_not() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write(root.join("package.json"), b"{}");
    write(root.join("node_modules/left-pad/index.js"), b"module.exports = 1;");
    write(root.join("src/main.js"), b"console.log(1);");

    let judged = judged(root);

    let node_modules = verdict_for(&judged, "node_modules").expect("node_modules judged");
    assert_eq!(node_modules.disposition, Disposition::Reclaimable);
    assert_eq!(node_modules.label, Label::BUILD_OUTPUT);

    match verdict_for(&judged, "src") {
        None => {}
        Some(verdict) => assert_eq!(verdict.disposition, Disposition::Keep, "src must not be cut"),
    }
}

/// Judging inside a flagged directory would count its bytes once for the
/// directory and again for every file in it — a reclaimable total inflated by
/// orders of magnitude, which is the most user-visible failure this domain has.
#[test]
fn files_inside_a_flagged_directory_produce_no_verdicts() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write(root.join("package.json"), b"{}");
    // A cache directory and a duplicate pair inside, both of which would
    // otherwise attract verdicts of their own.
    let blob = vec![b'x'; 1024 * 1024 + 7];
    write(root.join("node_modules/.cache/babel/a.bin"), &blob);
    write(root.join("node_modules/pkg/a.bin"), &blob);
    write(root.join("node_modules/pkg/index.js"), b"1");

    let judged = judged(root);

    let inside: Vec<&PathBuf> = judged
        .iter()
        .map(|(path, _)| path)
        .filter(|path| path.strip_prefix(root).is_ok_and(|rel| rel.starts_with("node_modules")))
        .filter(|path| !path.ends_with("node_modules"))
        .collect();
    assert!(inside.is_empty(), "verdicts leaked inside node_modules: {inside:?}");

    let catalog = catalog_of(root);
    let verdicts = builtin_verdicts(&catalog);
    let total = rollup(&catalog, &verdicts).reclaimable_bytes;
    assert_eq!(
        total,
        catalog
            .node(catalog.find(&root.join("node_modules")).expect("node_modules node"))
            .subtree_size,
        "reclaimable bytes must equal the flagged subtree exactly, counted once"
    );
}

/// `target` is also an ordinary word. Dropping the corroboration guard would
/// let the tool propose deleting someone's `target/` data directory.
#[test]
fn generic_target_needs_a_cargo_toml_beside_it() {
    let bare = TempDir::new().expect("tempdir");
    write(bare.path().join("target/measurements.csv"), b"1,2,3");
    write(bare.path().join("notes.txt"), b"shooting range data");
    let bare_verdict = verdict_for(&judged(bare.path()), "target").expect("target judged").clone();
    assert_eq!(bare_verdict.disposition, Disposition::Review);

    let cargo = TempDir::new().expect("tempdir");
    write(cargo.path().join("Cargo.toml"), b"[package]\nname = \"x\"\n");
    write(cargo.path().join("target/debug/x.exe"), b"binary");
    let cargo_verdict =
        verdict_for(&judged(cargo.path()), "target").expect("target judged").clone();
    assert_eq!(cargo_verdict.disposition, Disposition::Reclaimable);
}

/// `reason` is the sentence a human reads before approving a deletion, and the
/// slot a model fills from milestone 2 on. A rule shipping without it breaks
/// the whole ladder, so this asserts over every verdict the fixture produces.
#[test]
fn every_verdict_carries_a_reason() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write(root.join("package.json"), b"{}");
    write(root.join("node_modules/pkg/index.js"), b"1");
    write(root.join("target/data.csv"), b"1,2");
    write(root.join(".cache/blob.bin"), b"cached");
    let blob = vec![b'y'; 1024 * 1024 + 3];
    write(root.join("dups/a.bin"), &blob);
    write(root.join("dups/b.bin"), &blob);

    let judged = judged(root);
    assert!(judged.len() >= 4, "fixture must exercise several rules: {judged:?}");
    for (path, verdict) in &judged {
        assert!(!verdict.reason.trim().is_empty(), "empty reason for {}", path.display());
        assert!((0.0..=1.0).contains(&verdict.confidence), "confidence out of range for {path:?}");
    }
}

/// A group that keeps none of its copies deletes the file outright; a group
/// that keeps the newest keeps the wrong one.
#[test]
fn duplicates_keep_exactly_the_oldest_copy() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let blob = vec![b'z'; 1024 * 1024 + 11];
    let now = SystemTime::now();
    // Increasing age with the index, so the last name written is the oldest.
    for (index, name) in ["newest.bin", "middle.bin", "oldest.bin"].iter().enumerate() {
        let path = root.join("dups").join(name);
        write(&path, &blob);
        let age = Duration::from_secs(3600 * (index as u64 + 1) * 24);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen fixture")
            .set_modified(now - age)
            .expect("set mtime");
    }

    let judged = judged(root);
    let dups: Vec<&(PathBuf, Verdict)> =
        judged.iter().filter(|(_, v)| v.label == Label::DUPLICATE).collect();
    assert_eq!(dups.len(), 3, "all three copies judged: {judged:?}");

    let kept: Vec<&PathBuf> = dups
        .iter()
        .filter(|(_, v)| v.disposition == Disposition::Keep)
        .map(|(path, _)| path)
        .collect();
    assert_eq!(kept.len(), 1, "exactly one copy survives");
    assert!(kept[0].ends_with("oldest.bin"), "the oldest copy is kept, got {:?}", kept[0]);
}

/// Byte-identical mtimes are the common case, not the corner one: `cp -r`,
/// `robocopy` and an unzip all stamp every copy the same. The regression:
/// breaking that tie on `NodeId`, which is scan order — different between the
/// MFT and walk backends and not repeated run to run — so which copy survives
/// flips between two runs of the same command over the same tree. The path is
/// a property of the tree, so it decides the same way every time.
///
/// `Catalog::build` now hands out ids in path order, so the fixture can no
/// longer make the two orders disagree; it still pins which copy survives in
/// case id assignment ever goes back to following scan order.
#[test]
fn a_duplicate_group_with_identical_mtimes_keeps_the_lowest_path_not_the_lowest_node_id() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let blob = vec![b'q'; 1024 * 1024 + 5];
    let stamp = SystemTime::now() - Duration::from_secs(86_400);
    // `a/copy.bin` sorts before `b.bin` by path; a walk reaches `b.bin` first.
    for name in ["a/copy.bin", "b.bin"] {
        let path = root.join(name);
        write(&path, &blob);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen fixture")
            .set_modified(stamp)
            .expect("set mtime");
    }

    let catalog = catalog_of(root);
    let dups: Vec<(NodeId, Verdict)> = builtin_verdicts(&catalog)
        .into_iter()
        .filter(|(_, verdict)| verdict.label == Label::DUPLICATE)
        .collect();
    assert_eq!(dups.len(), 2, "both copies judged: {dups:?}");

    let (kept_id, _) = dups
        .iter()
        .find(|(_, verdict)| verdict.disposition == Disposition::Keep)
        .expect("exactly one copy survives");
    assert!(
        catalog.path(*kept_id).ends_with(Path::new("a").join("copy.bin")),
        "kept {}, but the lowest path in the group is a/copy.bin",
        catalog.path(*kept_id).display()
    );
}
