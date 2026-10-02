//! Fetching, pinning, caching and re-resolving, driven against a real local
//! repository so the real `git` binary, SHA resolution and checkout all run
//! with no network.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use nomnom_pack::{Error, Git, Lock, PackUrl, Resolver, Store, Tier, Trust};
use tempfile::TempDir;

struct Fixture {
    _home: TempDir,
    repo: PathBuf,
    store: Store,
    project: PathBuf,
}

/// A repository named `packs.git` holding one pack in `rust/`, plus an empty
/// store and project root.
fn fixture() -> (Fixture, String) {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = home.path().join("packs.git");
    common::init_repo(&repo);
    common::write_pack(&repo.join("rust"), "rust", &common::rule("cargo-target", "reclaimable"));
    let sha = common::commit(&repo, "the pack");

    let store = Store::at(home.path().join("store"), Git::new());
    let project = home.path().join("project");
    fs::create_dir_all(&project).expect("project root");
    (Fixture { _home: home, repo, store, project }, sha)
}

fn pack_url(repo: &Path, suffix: &str) -> String {
    format!("{}/rust{suffix}", common::file_url(repo))
}

/// `docs/lang.md`: packs are "pinned to a commit, never to a branch". The
/// regression: writing `main` into the lock, which makes what gets loaded
/// depend on when it was loaded and defeats the checksum entirely.
#[test]
fn a_branch_ref_is_recorded_as_the_commit_sha_it_resolved_to() {
    let (fx, sha) = fixture();
    let mut lock = Lock::default();
    let url = pack_url(&fx.repo, "@main");

    let added = nomnom_pack::add(&fx.store, &mut lock, &url).expect("add");

    assert_eq!(added.name, "rust", "the name comes out of pack.toml, not out of the URL");
    let recorded = added.sha.clone().expect("a git pack records a sha");
    assert_eq!(recorded.len(), 40, "recorded `{recorded}`");
    assert!(recorded.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(recorded, sha);
    assert_eq!(added.url.as_deref(), Some(url.as_str()), "the URL is kept as written");
    assert_eq!(added.subdir.as_deref(), Some("rust"));
    assert!(added.checksum.as_deref().expect("a checksum").starts_with("blake3:"));
    assert!(!added.trusted, "a freshly added pack is untrusted");
}

/// A tag resolves the same way, through the peeled `^{}` entry.
#[test]
fn a_tag_resolves_to_the_commit_it_points_at() {
    let (fx, sha) = fixture();
    common::git(&fx.repo, &["tag", "v1"]);
    let mut lock = Lock::default();

    let added = nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@v1")).expect("add");
    assert_eq!(added.sha.as_deref(), Some(sha.as_str()));
}

/// The cache is content-addressed by SHA, so it is never invalidated. The
/// regression: re-fetching on every resolve, which turns every scan into a
/// network round trip and makes an offline machine unable to run at all.
#[test]
fn a_second_resolve_of_the_same_pack_does_not_run_git_again() {
    let (fx, _) = fixture();
    let mut lock = Lock::default();
    let added = nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@main")).expect("add");
    let after_add = fx.store.git().invocations();
    assert!(after_add > 0, "adding a pack has to run git at least once");

    let dir = nomnom_pack::materialize(&fx.store, &added).expect("materialize");

    assert!(dir.join("pack.toml").is_file(), "{}", dir.display());
    assert_eq!(
        fx.store.git().invocations(),
        after_add,
        "a cached pack must be resolved without touching the network"
    );
}

/// `docs/lang.md`: "A pack that changes under a fixed reference is a
/// supply-chain event, so the lock is what is loaded and a drifting remote is
/// an error rather than an upgrade." The regression: treating the new content
/// as an upgrade and silently loading rules nobody reviewed.
#[test]
fn content_drift_under_a_fixed_sha_is_an_error_that_says_supply_chain() {
    let (fx, _) = fixture();
    let mut lock = Lock::default();
    let added = nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@main")).expect("add");
    let dir = nomnom_pack::materialize(&fx.store, &added).expect("materialize");

    fs::write(dir.join("rules").join("main.nom"), common::rule("sneaky", "reclaimable"))
        .expect("tamper with the cached pack");

    let error = nomnom_pack::materialize(&fx.store, &added).expect_err("drift must be refused");
    let text = error.to_string();
    assert!(text.contains("supply-chain"), "{text}");
    assert!(text.contains("not an upgrade"), "{text}");
    assert!(text.contains("rust"), "{text}");
    assert!(matches!(error, Error::Drift(_)), "{error:?}");
}

/// The regression: reporting "could not fetch pack" when git is simply not
/// installed, which sends the user looking at their network instead of their
/// PATH.
#[test]
fn a_missing_git_binary_says_so_and_names_what_it_was_doing() {
    let home = tempfile::tempdir().expect("tempdir");
    let store = Store::at(home.path(), Git::with_program("nomnom-git-that-does-not-exist"));
    let url = PackUrl::parse("github.com/ranolp/nomnom-packs/rust").expect("parses");

    let error = store.git().resolve_sha(&url.git_url, Some("main")).expect_err("no such binary");
    let text = error.to_string();
    assert!(matches!(error, Error::GitMissing { .. }), "{error:?}");
    assert!(text.contains("nomnom-git-that-does-not-exist"), "{text}");
    assert!(text.contains("is not on PATH"), "{text}");
    assert!(text.contains("resolving `main`"), "{text}");
}

/// The regression: a failed `git` invocation reported as a bare exit code. An
/// auth failure and a bad ref are both exit 128 and are entirely different
/// problems, so the stderr has to survive.
#[test]
fn a_failed_git_invocation_surfaces_its_stderr() {
    let home = tempfile::tempdir().expect("tempdir");
    let git = Git::new();
    let missing = common::file_url(&home.path().join("no-such-repository"));

    let error = git.resolve_sha(&missing, None).expect_err("no such repository");
    match error {
        Error::Git { stderr, command, .. } => {
            assert!(!stderr.is_empty(), "stderr was dropped");
            assert!(stderr.contains("repository"), "{stderr}");
            assert!(command.contains("ls-remote"), "{command}");
        }
        other => panic!("expected a git error, got {other}"),
    }
}

/// A ref that does not exist exits 0 with no output, so a naive check reports
/// success. The regression: pinning a pack to an empty SHA.
#[test]
fn a_ref_that_does_not_exist_is_an_error_even_though_git_exits_zero() {
    let (fx, _) = fixture();
    let error = fx
        .store
        .git()
        .resolve_sha(&common::file_url(&fx.repo), Some("no-such-branch"))
        .expect_err("a missing ref must fail");
    assert!(matches!(error, Error::RefNotFound { .. }), "{error:?}");
}

/// `docs/lang.md` fixes the order user, project, `--pack`, later overriding
/// earlier. The regression: a project pack losing to a user pack, which would
/// make a repository unable to correct a rule for its own tree.
#[test]
fn the_resolver_returns_the_three_tiers_in_the_documented_order() {
    let (fx, _) = fixture();
    let mut lock = Lock::default();
    nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@main")).expect("add");
    lock.trust("rust");

    common::write_pack(
        &fx.store.root().join("hand-placed"),
        "hand-placed",
        &common::rule("user-rule", "review"),
    );
    common::write_pack(
        &fx.project.join(".nomnom").join("packs").join("local"),
        "project-local",
        &common::rule("project-rule", "reclaimable"),
    );
    let explicit = fx.project.join("explicit-pack");
    common::write_pack(&explicit, "explicit", &common::rule("explicit-rule", "review"));

    let sources = Resolver::new(fx.store.clone(), &fx.project)
        .with_explicit([explicit.clone()])
        .resolve(&lock)
        .expect("resolve");

    let seen: Vec<(&str, Tier, Trust)> =
        sources.iter().map(|s| (s.name.as_str(), s.tier, s.trust)).collect();
    assert_eq!(
        seen,
        vec![
            ("rust", Tier::User, Trust::Trusted),
            ("hand-placed", Tier::User, Trust::Untrusted),
            ("project-local", Tier::Project, Trust::Untrusted),
            ("explicit", Tier::Explicit, Trust::Untrusted),
        ]
    );

    // The fetch cache shares the user root, and must never be mistaken for a
    // pack directory sitting in it.
    assert!(sources.iter().all(|s| s.name != "file"), "{sources:?}");
    for source in &sources {
        source.load().expect("every resolved directory is a loadable pack");
    }
}

/// A moving branch must not move the pin on its own. The regression: a resolve
/// re-running `ls-remote` and quietly picking up a new commit, which is exactly
/// the drifting-remote upgrade `docs/lang.md` refuses.
#[test]
fn only_an_explicit_update_moves_a_pin() {
    let (fx, first) = fixture();
    let mut lock = Lock::default();
    let added = nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@main")).expect("add");
    lock.trust("rust");

    fs::write(fx.repo.join("rust").join("rules").join("later.nom"), common::rule("later", "keep"))
        .expect("a new rule upstream");
    let second = common::commit(&fx.repo, "upstream moves on");
    assert_ne!(first, second);

    // Resolving again stays on the old commit.
    let still = nomnom_pack::materialize(&fx.store, &added).expect("materialize");
    assert!(!still.join("rules").join("later.nom").exists());

    let moved = nomnom_pack::update(&fx.store, &mut lock, "rust").expect("update");
    assert_eq!(moved.sha.as_deref(), Some(second.as_str()));
    assert!(moved.trusted, "an explicit update does not re-ask for trust");
    assert_ne!(moved.checksum, added.checksum);
}

/// A lock written by one run has to be readable by the next. The regression:
/// a serialised shape that does not round-trip, which strands every pinned
/// pack in the project.
#[test]
fn the_lock_round_trips_through_its_file() {
    let (fx, _) = fixture();
    let mut lock = Lock::default();
    nomnom_pack::add(&fx.store, &mut lock, &pack_url(&fx.repo, "@main")).expect("add");
    lock.trust("rust");
    lock.save(&fx.project).expect("save");

    let path = Lock::path_in(&fx.project);
    assert!(path.ends_with(Path::new(".nomnom").join("packs.lock")), "{}", path.display());
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("[[pack]]"), "{text}");
    assert!(text.contains("trusted = true"), "{text}");

    let reloaded = Lock::load(&fx.project).expect("load");
    assert_eq!(reloaded.packs(), lock.packs());
}
