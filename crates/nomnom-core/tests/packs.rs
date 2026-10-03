//! External packs reaching the judge: resolution order, and the trust cap.

mod common;

use std::path::{Path, PathBuf};

use common::{catalog_of, write};
use nomnom_core::verdict::{Disposition, TrustedPack, Verdict, builtin_pack, judge};
use nomnom_pack::Trust;
use tempfile::TempDir;

/// A pack directory holding one rule, so a test can say what a pack concludes
/// without a `.nom` file on the side.
fn pack_dir(root: &Path, name: &str, rule: &str) -> PathBuf {
    let dir = root.join(name);
    write(dir.join("pack.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n").as_bytes());
    write(dir.join("rules").join("main.nom"), rule.as_bytes());
    dir
}

/// One rule matching a directory named `blobs`.
fn blobs_rule(rule_name: &str, disposition: &str, confidence: &str, reason: &str) -> String {
    format!(
        "[{rule_name}]\ndescription = {reason}\nkind = cache/v1\n\
         disposition = {disposition}\nconfidence = {confidence}\n\
         filter {{\n  then $p/blobs/\n}}\n"
    )
}

fn load(dir: &Path, trust: Trust) -> TrustedPack {
    TrustedPack { pack: nomnom_lang::pack::load(dir).expect("a loadable pack"), trust }
}

/// A tree with one `blobs` directory for the packs to argue over.
fn tree() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    write(tmp.path().join("tree").join("blobs").join("a.bin"), b"payload");
    tmp
}

fn judge_blobs(tmp: &TempDir, packs: Vec<TrustedPack>) -> Verdict {
    let catalog = catalog_of(&tmp.path().join("tree"));
    judge(&catalog, &packs)
        .into_iter()
        .find(|(id, _)| catalog.path(*id).ends_with("blobs"))
        .map(|(_, verdict)| verdict)
        .expect("blobs judged")
}

/// `docs/lang.md`: "A rule from any pack other than the built-in one is capped
/// at `disposition = review` until the user runs `nomnom pack trust <name>`."
/// The regression: a stranger's repository choosing what `clean` deletes.
#[test]
fn an_untrusted_packs_reclaimable_becomes_review_and_says_why() {
    let tmp = tree();
    let dir = pack_dir(
        tmp.path(),
        "vendor",
        &blobs_rule("blob-cache", "reclaimable", "0.99", "a blob cache, refilled on demand"),
    );

    let verdict = judge_blobs(&tmp, vec![load(&dir, Trust::Untrusted)]);

    assert_eq!(verdict.disposition, Disposition::Review);
    let capped = verdict.capped.as_deref().expect("a downgrade has to be explained");
    assert!(capped.contains("vendor"), "{capped}");
    assert!(capped.contains("reclaimable"), "{capped}");
    assert!(capped.contains("nomnom pack trust vendor"), "{capped}");
}

/// The cap must be a value applied to the verdict, not an edit to the pack:
/// trusting the same pack has to lift it with no reload and no re-parse.
#[test]
fn trusting_the_same_pack_lifts_the_cap() {
    let tmp = tree();
    let dir = pack_dir(
        tmp.path(),
        "vendor",
        &blobs_rule("blob-cache", "reclaimable", "0.99", "a blob cache, refilled on demand"),
    );

    let verdict = judge_blobs(&tmp, vec![load(&dir, Trust::Trusted)]);

    assert_eq!(verdict.disposition, Disposition::Reclaimable);
    assert_eq!(verdict.capped, None, "nothing was downgraded, so nothing to explain");
}

/// A pack that writes `keep` or `review` itself must not be reported as
/// downgraded. The regression: a "capped at review" line under every verdict an
/// untrusted pack produces, which trains the user to ignore the line that
/// matters.
#[test]
fn an_untrusted_packs_review_is_not_reported_as_a_downgrade() {
    let tmp = tree();
    let dir = pack_dir(
        tmp.path(),
        "vendor",
        &blobs_rule("blob-cache", "review", "0.99", "a blob cache, refilled on demand"),
    );

    let verdict = judge_blobs(&tmp, vec![load(&dir, Trust::Untrusted)]);

    assert_eq!(verdict.disposition, Disposition::Review);
    assert_eq!(verdict.capped, None);
}

/// `docs/lang.md` resolves a conflict by "1. highest confidence 2. pack
/// precedence (later-resolved pack wins)". The regression: taking only a
/// strictly-greater confidence, which hands every tie to the EARLIER pack and
/// makes a project pack unable to correct a user pack at the same confidence —
/// the exact thing the resolution order exists to allow.
#[test]
fn a_later_pack_wins_a_confidence_tie_against_an_earlier_one() {
    let tmp = tree();
    let earlier =
        pack_dir(tmp.path(), "earlier", &blobs_rule("earlier-rule", "keep", "0.8", "earlier says"));
    let later =
        pack_dir(tmp.path(), "later", &blobs_rule("later-rule", "review", "0.8", "later says"));

    let verdict =
        judge_blobs(&tmp, vec![load(&earlier, Trust::Trusted), load(&later, Trust::Trusted)]);

    assert_eq!(verdict.provenance.pack, "later", "later-resolved pack must win the tie");
    assert_eq!(verdict.disposition, Disposition::Review);
}

/// Confidence still outranks pack order, so a later pack does not silently
/// override a rule that was more certain than it.
#[test]
fn a_higher_confidence_earlier_rule_beats_a_later_pack() {
    let tmp = tree();
    let earlier =
        pack_dir(tmp.path(), "earlier", &blobs_rule("earlier-rule", "keep", "0.9", "earlier says"));
    let later =
        pack_dir(tmp.path(), "later", &blobs_rule("later-rule", "review", "0.8", "later says"));

    let verdict =
        judge_blobs(&tmp, vec![load(&earlier, Trust::Trusted), load(&later, Trust::Trusted)]);

    assert_eq!(verdict.provenance.pack, "earlier");
}

/// The built-in pack is `Trust::Builtin` and is never capped. The regression:
/// capping everything uniformly, which turns every `reclaimable` the tool ships
/// with into a `review` and leaves `clean` with nothing to do out of the box.
#[test]
fn the_builtin_pack_is_never_capped() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("tree");
    write(root.join("package.json"), b"{}");
    write(root.join("node_modules").join("left-pad").join("index.js"), b"1");

    let catalog = catalog_of(&root);
    let id = catalog.find(&root.join("node_modules")).expect("node_modules node");
    let verdict = judge(&catalog, &[TrustedPack::builtin(builtin_pack().clone())])
        .into_iter()
        .find(|(judged, _)| *judged == id)
        .map(|(_, verdict)| verdict)
        .expect("judged");

    assert_eq!(verdict.disposition, Disposition::Reclaimable);
    assert_eq!(verdict.capped, None);
}
