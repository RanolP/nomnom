//! External packs reaching the judge: resolution order, and the trust cap.

mod common;

use std::path::{Path, PathBuf};

use common::{catalog_of, write};
use nomnom_core::verdict::{Disposition, TrustedPack, Verdict, judge};
use nomnom_pack::Trust;
use tempfile::TempDir;

/// A pack directory holding one rule, so a test can say what a pack concludes
/// without a rule file on the side.
fn pack_dir(root: &Path, name: &str, rule: &str) -> PathBuf {
    let dir = root.join(name);
    write(dir.join("pack.toml"), format!("name = \"{name}\"\nversion = \"0.1.0\"\n").as_bytes());
    write(dir.join("rules").join("main.toml"), rule.as_bytes());
    dir
}

/// One rule matching a directory named `blobs`.
fn blobs_rule(rule_name: &str, disposition: &str, reason: &str) -> String {
    format!(
        "[[rule]]\ntitle = \"{rule_name}\"\ndescription = \"{reason}\"\nkind = \"cache/v1\"\n\
         disposition = \"{disposition}\"\nfilter = 'then $p/blobs/'\n"
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
        &blobs_rule("blob-cache", "reclaimable","a blob cache, refilled on demand"),
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
        &blobs_rule("blob-cache", "reclaimable","a blob cache, refilled on demand"),
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
        &blobs_rule("blob-cache", "review","a blob cache, refilled on demand"),
    );

    let verdict = judge_blobs(&tmp, vec![load(&dir, Trust::Untrusted)]);

    assert_eq!(verdict.disposition, Disposition::Review);
    assert_eq!(verdict.capped, None);
}

/// The built-in pack is `Trust::Builtin` and is never capped. The regression:
/// capping everything uniformly, which turns every `reclaimable` the tool ships
/// with into a `review` and leaves `clean` with nothing to do out of the box.
#[test]
fn the_builtin_packs_are_never_capped() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("tree");
    write(root.join("package.json"), b"{}");
    write(root.join("node_modules").join("left-pad").join("index.js"), b"1");
    write(root.join("node_modules").join(".package-lock.json"), b"{}");

    let catalog = catalog_of(&root);
    let id = catalog.find(&root.join("node_modules")).expect("node_modules node");
    let verdict = judge(&catalog, &TrustedPack::builtins())
        .into_iter()
        .find(|(judged, _)| *judged == id)
        .map(|(_, verdict)| verdict)
        .expect("judged");

    assert_eq!(verdict.disposition, Disposition::Reclaimable);
    assert_eq!(verdict.capped, None);
}
