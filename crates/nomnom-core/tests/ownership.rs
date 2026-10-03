//! Ownership (`docs/lang.md`): every short-form target is an exclusive claim,
//! nesting across packs drops, nesting within a pack layers, and the bytes
//! split into claimed and arbitrary.

mod common;

use std::path::Path;

use common::{catalog_of, write};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::verdict::{DropReason, Provenance, TrustedPack, assess};
use tempfile::TempDir;

/// `(title, confidence, then-path)` per rule.
fn pack(root: &Path, name: &str, rules: &[(&str, &str, &str)]) -> TrustedPack {
    let dir = root.join("packs").join(name);
    write(dir.join("pack.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n").as_bytes());
    let text: String = rules
        .iter()
        .map(|(title, confidence, then)| {
            format!(
                "[{title}]\ndescription = test rule {title}\nkind = cache/v1\n\
                 confidence = {confidence}\nfilter {{\n  then {then}\n}}\n\n"
            )
        })
        .collect();
    write(dir.join("rules").join("main.nom"), text.as_bytes());
    TrustedPack::builtin(nomnom_lang::pack::load(&dir).expect("a loadable pack"))
}

/// `tree/outer/{inner,layer}/…` beside the unclaimed `tree/loose/`.
fn tree() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let tree = tmp.path().join("tree");
    write(tree.join("outer").join("own.bin"), b"0123456789");
    write(tree.join("outer").join("inner").join("i.bin"), b"inner");
    write(tree.join("outer").join("layer").join("f.bin"), b"layered");
    write(tree.join("loose").join("x.bin"), b"somebody's file");
    tmp
}

fn id(catalog: &Catalog, tmp: &TempDir, rel: &str) -> NodeId {
    catalog.find(&tmp.path().join("tree").join(rel)).unwrap_or_else(|| panic!("{rel} scanned"))
}

fn layered_packs(tmp: &TempDir) -> Vec<TrustedPack> {
    vec![
        pack(tmp.path(), "a", &[("Outer", "0.9", "$p/outer/"), ("Layer", "0.9", "$p/layer/")]),
        pack(tmp.path(), "b", &[("Inner", "0.99", "$p/inner/")]),
    ]
}

// Catches another pack's `cache` rule firing inside a folder a pack already
// claimed exclusively — the Steam-game-holds-a-`cache/` case.
#[test]
fn a_claim_inside_another_packs_claim_is_dropped() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let assessment = assess(&catalog, layered_packs(&tmp));
    let ownership = &assessment.ownership;

    let inner = id(&catalog, &tmp, "outer/inner");
    assert!(ownership.claim_at(inner).is_none(), "b's claim inside a's must not survive");
    let owner = ownership.owner(inner).expect("inner lies inside a's claim");
    assert_eq!(owner.1.provenance, Provenance::new("a", "Outer"));
    let dropped = ownership
        .dropped()
        .iter()
        .find(|d| d.claim.provenance == Provenance::new("b", "Inner"))
        .expect("the dropped claim is logged");
    assert_eq!(dropped.reason, DropReason::Inside { owner: Provenance::new("a", "Outer") });
}

// Catches a pack losing its own inner layer (the Steam library inside the
// Steam client folder) to the cross-pack nesting rule.
#[test]
fn a_claim_inside_its_own_packs_claim_is_kept_and_owns_its_subtree() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let assessment = assess(&catalog, layered_packs(&tmp));
    let ownership = &assessment.ownership;

    let layer = id(&catalog, &tmp, "outer/layer");
    let file = id(&catalog, &tmp, "outer/layer/f.bin");
    assert_eq!(ownership.claim_at(layer).map(|c| c.class.as_str()), Some("a:cache/v1"));
    assert_eq!(ownership.owner(file).unwrap().1.provenance, Provenance::new("a", "Layer"));
    assert_eq!(
        ownership.owner(id(&catalog, &tmp, "outer/own.bin")).unwrap().1.provenance,
        Provenance::new("a", "Outer")
    );
    // One suggestion per directory: the outer claim's covers the layer.
    let suggested: Vec<&str> = assessment
        .groups
        .iter()
        .flat_map(|g| &g.entries)
        .map(|e| e.verdict.provenance.rule.as_str())
        .collect();
    assert_eq!(suggested, ["Outer"]);
}

// Catches the claim resolver ordering conflicts differently from verdicts:
// confidence first, then the later pack, then the earlier rule in a pack.
#[test]
fn conflicting_claims_resolve_by_confidence_then_later_pack_then_earlier_rule() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let loose = id(&catalog, &tmp, "loose");
    let winner = |packs: Vec<TrustedPack>| {
        assess(&catalog, packs).ownership.claim_at(loose).expect("loose claimed").provenance.clone()
    };

    let strong_first = vec![
        pack(tmp.path(), "first", &[("Strong", "0.9", "$p/loose/")]),
        pack(tmp.path(), "second", &[("Weak", "0.5", "$p/loose/")]),
    ];
    assert_eq!(winner(strong_first), Provenance::new("first", "Strong"));

    let tied = vec![
        pack(tmp.path(), "early", &[("E", "0.7", "$p/loose/")]),
        pack(tmp.path(), "late", &[("L1", "0.7", "$p/loose/"), ("L2", "0.7", "$p/loose/")]),
    ];
    let assessment = assess(&catalog, tied);
    assert_eq!(
        assessment.ownership.claim_at(loose).unwrap().provenance,
        Provenance::new("late", "L1")
    );
    let mut losers: Vec<String> = assessment
        .ownership
        .dropped()
        .iter()
        .filter(|d| d.reason == DropReason::Outranked { by: Provenance::new("late", "L1") })
        .map(|d| d.claim.provenance.to_string())
        .collect();
    losers.sort();
    assert_eq!(losers, ["early [E]", "late [L2]"]);
}

// Catches a suggestion landing in "Other files": nothing a pack has not
// claimed may ever be proposed for removal.
#[test]
fn every_suggestion_lies_inside_a_claim() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let assessment = assess(&catalog, layered_packs(&tmp));
    let entries: Vec<_> = assessment.groups.iter().flat_map(|g| &g.entries).collect();
    assert!(!entries.is_empty());
    for entry in entries {
        let node = catalog.find(Path::new(&entry.path)).expect("entry scanned");
        assert!(assessment.ownership.owner(node).is_some(), "{} suggested unclaimed", entry.path);
    }
    let loose = id(&catalog, &tmp, "loose/x.bin");
    assert!(assessment.ownership.owner(loose).is_none());
}

// Catches the byte split double-counting a nested claim, or losing bytes
// between "Recognized" and "Other files".
#[test]
fn claimed_and_arbitrary_bytes_sum_to_the_total() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let assessment = assess(&catalog, layered_packs(&tmp));
    let ownership = &assessment.ownership;
    let root = catalog.root();
    let outer = id(&catalog, &tmp, "outer");
    let loose = id(&catalog, &tmp, "loose");

    assert_eq!(
        ownership.claimed(root) + ownership.arbitrary(&catalog, root),
        catalog.node(root).subtree_size
    );
    assert_eq!(ownership.claimed(root), catalog.node(outer).subtree_size);
    assert_eq!(ownership.arbitrary(&catalog, outer), 0);
    assert_eq!(ownership.arbitrary(&catalog, loose), catalog.node(loose).subtree_size);
    let recognized = ownership.recognized(&catalog);
    assert_eq!(recognized.len(), 1);
    assert_eq!(recognized[0].bytes, ownership.claimed(root), "a nested claim counted twice");
}
