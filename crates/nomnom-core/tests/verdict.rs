//! Judge behaviour over real scanned fixture trees.

mod common;

use std::path::{Path, PathBuf};

use common::{catalog_of, write};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::verdict::{Assessment, Disposition, Label, TrustedPack, Verdict, assess, rollup};
use tempfile::TempDir;

/// Verdicts keyed by the path they were rendered about.
fn judged(root: &Path) -> Vec<(PathBuf, Verdict)> {
    let catalog = catalog_of(root);
    builtin_verdicts(&catalog)
        .into_iter()
        .map(|(id, verdict)| (catalog.path(id), verdict))
        .collect()
}

/// The built-in pack's verdicts, as every front-end assesses them.
fn builtin_verdicts(catalog: &Catalog) -> Vec<(NodeId, Verdict)> {
    let assessment = builtin_assessment(catalog);
    let mut verdicts: Vec<(NodeId, Verdict)> = assessment
        .groups
        .into_iter()
        .flat_map(|group| group.entries)
        .map(|entry| (NodeId(entry.reach.expect("catalog entry").start), entry.verdict))
        .collect();
    verdicts.sort_by_key(|(id, _)| *id);
    verdicts
}

fn builtin_assessment(catalog: &Catalog) -> Assessment {
    assess(catalog, TrustedPack::builtins())
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
    write(root.join("node_modules/.package-lock.json"), b"{}");
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
    // A cache directory inside, which would otherwise attract a verdict of
    // its own.
    write(root.join("node_modules/.cache/babel/a.bin"), &vec![b'x'; 1024 * 1024 + 7]);
    write(root.join("node_modules/pkg/__pycache__/m.cpython-312.pyc"), b"bytecode");
    write(root.join("node_modules/.package-lock.json"), b"{}");
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

/// `target` is also an ordinary word, and a `Cargo.toml` beside it proves
/// nothing about what is inside. Judging by name would propose deleting
/// someone's `target/` data directory; only Cargo's own files inside count.
#[test]
fn target_is_judged_only_when_cargo_s_signature_is_inside() {
    let bare = TempDir::new().expect("tempdir");
    write(bare.path().join("Cargo.toml"), b"[package]\nname = \"x\"\n");
    write(bare.path().join("target/measurements.csv"), b"1,2,3");
    write(bare.path().join("notes.txt"), b"shooting range data");
    assert!(verdict_for(&judged(bare.path()), "target").is_none(), "a bare target/ was judged");

    let cargo = TempDir::new().expect("tempdir");
    write(cargo.path().join("Cargo.toml"), b"[package]\nname = \"x\"\n");
    write(cargo.path().join("target/.rustc_info.json"), b"{}");
    write(cargo.path().join("target/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55");
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
    write(root.join("node_modules/.package-lock.json"), b"{}");
    write(root.join("node_modules/pkg/index.js"), b"1");
    write(root.join("target/.rustc_info.json"), b"{}");
    write(root.join("target/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55");
    write(root.join("__pycache__/m.cpython-312.pyc"), b"bytecode");

    let judged = judged(root);
    assert!(judged.len() >= 3,"fixture must exercise several rules: {judged:?}");
    for (path, verdict) in &judged {
        assert!(!verdict.reason.trim().is_empty(), "empty reason for {}", path.display());
    }
}

