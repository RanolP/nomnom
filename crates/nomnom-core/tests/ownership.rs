//! Ownership (`docs/lang.md`): every short-form target is an exclusive claim,
//! nesting across packs drops, nesting within a pack layers, and the bytes
//! split into claimed and arbitrary.

mod common;

use std::path::Path;

use common::{catalog_of, write};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::verdict::{DropReason, Provenance, TrustedPack, assess};
use tempfile::TempDir;

/// `(title, then-path)` per rule.
fn pack(root: &Path, name: &str, rules: &[(&str, &str)]) -> TrustedPack {
    let dir = root.join("packs").join(name);
    write(dir.join("pack.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n").as_bytes());
    let text: String = rules
        .iter()
        .map(|(title, then)| {
            format!(
                "[[rule]]\ntitle = \"{title}\"\ndescription = \"test rule {title}\"\n\
                 kind = \"cache/v1\"\nfilter = 'then {then}'\n\n"
            )
        })
        .collect();
    write(dir.join("rules").join("main.toml"), text.as_bytes());
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
        pack(tmp.path(), "a", &[("Outer", "$p/outer/"), ("Layer", "$p/layer/")]),
        pack(tmp.path(), "b", &[("Inner", "$p/inner/")]),
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

// Catches one pack's wrong signature deciding a folder another pack also
// claims: two packs on one node must leave it unclaimed and unsuggested, with
// both claims logged as contested against each other.
#[test]
fn two_packs_claiming_one_node_leave_it_unclaimed_and_both_contested() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let loose = id(&catalog, &tmp, "loose");
    let packs = vec![
        pack(tmp.path(), "early", &[("E", "$p/loose/")]),
        pack(tmp.path(), "late", &[("L", "$p/loose/")]),
    ];
    let assessment = assess(&catalog, packs);

    assert!(assessment.ownership.claim_at(loose).is_none(), "neither claim may own it");
    assert!(assessment.ownership.owner(id(&catalog, &tmp, "loose/x.bin")).is_none());
    assert!(assessment.groups.iter().all(|g| g.entries.is_empty()), "nothing suggested");
    let mut contested: Vec<(String, DropReason)> = assessment
        .ownership
        .dropped()
        .iter()
        .map(|d| (d.claim.provenance.to_string(), d.reason.clone()))
        .collect();
    contested.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        contested,
        [
            ("early [E]".to_owned(), DropReason::Contested { with: Provenance::new("late", "L") }),
            ("late [L]".to_owned(), DropReason::Contested { with: Provenance::new("early", "E") }),
        ]
    );
}

// Catches a pack's own later, narrower rule overriding its earlier one on the
// same node: within a pack the earlier rule wins and the later is outranked,
// never contested.
#[test]
fn within_one_pack_the_earlier_rule_wins_and_the_later_is_outranked() {
    let tmp = tree();
    let catalog = catalog_of(&tmp.path().join("tree"));
    let loose = id(&catalog, &tmp, "loose");
    let packs = vec![pack(tmp.path(), "one", &[("First", "$p/loose/"), ("Second", "$p/loose/")])];
    let assessment = assess(&catalog, packs);

    assert_eq!(
        assessment.ownership.claim_at(loose).expect("loose claimed").provenance,
        Provenance::new("one", "First")
    );
    let dropped: Vec<(String, DropReason)> = assessment
        .ownership
        .dropped()
        .iter()
        .map(|d| (d.claim.provenance.to_string(), d.reason.clone()))
        .collect();
    assert_eq!(
        dropped,
        [("one [Second]".to_owned(), DropReason::Outranked { by: Provenance::new("one", "First") })]
    );
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
