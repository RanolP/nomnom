//! The cap `docs/lang.md` puts on a pack the user has not trusted.

use nomnom_lang::ast::Disposition;
use nomnom_pack::{Lock, LockedPack, Trust};

/// The core of "untrusted packs cannot delete". The regression: an untrusted
/// pack's `reclaimable` reaching a deletion plan, which is a stranger's
/// repository choosing what to remove from the user's disk.
#[test]
fn an_untrusted_pack_cannot_propose_a_deletion() {
    let capped = Trust::Untrusted.cap(Disposition::Reclaimable);
    assert_eq!(capped.disposition, Disposition::Review);
    assert_eq!(capped.downgraded_from, Some(Disposition::Reclaimable));

    let trusted = Trust::Trusted.cap(Disposition::Reclaimable);
    assert_eq!(trusted.disposition, Disposition::Reclaimable);
    assert!(!trusted.was_downgraded());

    assert_eq!(Trust::Builtin.cap(Disposition::Reclaimable).disposition, Disposition::Reclaimable);
}

/// The cap only ever moves toward safety. The regression: capping `keep` into
/// `review`, which would push files a pack explicitly protected back in front
/// of a human as deletion candidates.
#[test]
fn the_cap_leaves_keep_and_review_alone() {
    for disposition in [Disposition::Keep, Disposition::Review] {
        let capped = Trust::Untrusted.cap(disposition);
        assert_eq!(capped.disposition, disposition);
        assert!(!capped.was_downgraded(), "{disposition:?} should pass through");
        assert_eq!(capped.explanation("rust"), None);
    }
}

/// `docs/lang.md`: "A pack that declares `reclaimable` is downgraded, and the
/// CLI says why." The regression: capping in place, leaving the CLI unable to
/// tell a capped verdict from one the rule wrote as `review` itself.
#[test]
fn a_downgrade_carries_the_sentence_the_cli_prints() {
    let capped = Trust::Untrusted.cap(Disposition::Reclaimable);
    let text = capped.explanation("rust").expect("a downgrade explains itself");
    assert!(text.contains("not trusted"), "{text}");
    assert!(text.contains("reclaimable"), "{text}");
    assert!(text.contains("nomnom pack trust rust"), "{text}");
}

/// Trust is per pack and defaults to off. The regression: an unknown pack
/// defaulting to trusted, which would make the cap depend on the lock having
/// been written rather than on the user having said yes.
#[test]
fn trust_defaults_to_untrusted_and_survives_a_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let mut lock = Lock::default();
    assert_eq!(lock.trust_of("never-seen"), Trust::Untrusted);

    lock.upsert(LockedPack {
        name: "rust".into(),
        url: Some("github.com/ranolp/nomnom-packs/rust".into()),
        sha: Some("a".repeat(40)),
        subdir: Some("rust".into()),
        checksum: Some("blake3:00".into()),
        trusted: false,
    });
    assert_eq!(lock.trust_of("rust"), Trust::Untrusted);

    lock.trust("rust");
    lock.save(root).expect("save");
    let reloaded = Lock::load(root).expect("load");
    assert_eq!(reloaded.trust_of("rust"), Trust::Trusted);

    let mut reloaded = reloaded;
    reloaded.revoke("rust");
    assert_eq!(reloaded.trust_of("rust"), Trust::Untrusted);
    // Revoking is not a removal: the pin stays.
    assert!(reloaded.get("rust").is_some());
}

/// Trusting a pack once should not have to be repeated every time its pin
/// moves. The regression: `pack update` silently clearing the trust flag.
#[test]
fn re_adding_a_pack_keeps_trust_already_granted() {
    let mut lock = Lock::default();
    lock.upsert(LockedPack::local("rust"));
    lock.trust("rust");

    let mut moved = LockedPack::local("rust");
    moved.sha = Some("b".repeat(40));
    lock.upsert(moved);

    assert_eq!(lock.trust_of("rust"), Trust::Trusted);
}
