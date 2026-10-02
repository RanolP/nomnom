//! `nomnom pack`, end to end against the real binary.
//!
//! The git-touching tests drive a real repository in a `TempDir`, reusing the
//! fixture helpers `nomnom-pack`'s own acquire tests already run against the
//! real `git` binary with no network. They are included by path rather than
//! copied so there is one definition of "a local pack repository" in the
//! workspace.
#[path = "../../nomnom-pack/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_nomnom");

/// A project directory, plus a pack store of its own.
///
/// The store root comes from `%LOCALAPPDATA%` / `$XDG_DATA_HOME`, so pointing
/// both at a temp directory is what keeps a test run out of the developer's
/// real pack cache — and what keeps a pack they happen to have installed out of
/// the test's `pack list`.
struct Project {
    dir: TempDir,
    store: PathBuf,
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).expect("store root");
        Project { dir, store }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn nomnom(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.path())
            .env("LOCALAPPDATA", &self.store)
            .env("XDG_DATA_HOME", &self.store)
            .output()
            .expect("run nomnom")
    }

    /// Runs and fails the test with the child's own stderr, which is where a
    /// `nomnom-pack` error's detail lives.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.nomnom(args);
        assert!(
            output.status.success(),
            "nomnom {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

/// A pack directory the project can load with `--pack`.
fn local_pack(dir: &Path, name: &str, disposition: &str) -> PathBuf {
    let path = dir.join(name);
    common::write_pack(&path, name, &common::rule("marked-dir", disposition));
    path
}

/// The tree `common::rule` matches: a directory holding a `marker` file.
fn marked_tree(root: &Path) {
    std::fs::create_dir_all(root.join("blobs")).expect("tree");
    std::fs::write(root.join("blobs").join("marker"), b"x").expect("marker");
}

/// `docs/lang.md`: an untrusted pack's `reclaimable` "is downgraded, and the
/// CLI says why". The regression that matters is not the downgrade — it is a
/// downgrade the user cannot see, which is indistinguishable from a rule that
/// wrote `review` itself and leaves them with no way to know a trust grant is
/// what is missing.
#[test]
fn an_untrusted_packs_reclaimable_reaches_suggest_as_review_with_the_explanation() {
    let project = Project::new();
    marked_tree(project.path());
    let pack = local_pack(project.path(), "vendor", "reclaimable");

    let text = project.ok(&["suggest", ".", "--pack", pack.to_str().unwrap()]);

    assert!(text.contains("[review]"), "{text}");
    assert!(text.contains("capped at review"), "{text}");
    assert!(text.contains("nomnom pack trust vendor"), "{text}");
    assert!(text.contains("Reclaimable: 0 B"), "a capped verdict must not be counted:\n{text}");
}

/// The grant has to change what the next scan does, through the lock, in a
/// separate process. The regression: recording trust somewhere `suggest` does
/// not read, which makes `nomnom pack trust` a no-op the user cannot detect.
#[test]
fn pack_trust_lifts_the_cap_for_the_next_suggest() {
    let project = Project::new();
    marked_tree(project.path());
    let pack = local_pack(project.path(), "vendor", "reclaimable");
    let dir = pack.to_str().unwrap();

    let granting = project.ok(&["pack", "trust", "vendor", "--pack", dir]);
    // `docs/lang.md` makes trust "granted per pack, deliberately, once", so the
    // sentence on screen has to name the pack being trusted, not just echo it.
    assert!(granting.contains("Trusting pack `vendor`"), "{granting}");
    assert!(granting.contains(dir), "the grant must name where the pack came from:\n{granting}");

    let text = project.ok(&["suggest", ".", "--pack", dir]);
    assert!(text.contains("[reclaimable]"), "{text}");
    assert!(!text.contains("capped at review"), "{text}");

    let revoked = project.ok(&["pack", "untrust", "vendor", "--pack", dir]);
    assert!(revoked.contains("Untrusted `vendor`"), "{revoked}");
    let after = project.ok(&["suggest", ".", "--pack", dir]);
    assert!(after.contains("[review]"), "revoking must put the cap back:\n{after}");
}

/// `pack list` is how a user answers "what rules is this run loading?". The
/// regression: a listing that omits the built-in pack or loses a pack's tier or
/// trust state, which are the two facts that explain a verdict they did not
/// expect.
#[test]
fn pack_list_names_every_resolved_pack_with_its_tier_and_trust_in_both_forms() {
    let project = Project::new();
    let explicit = local_pack(project.path(), "vendor", "reclaimable");
    common::write_pack(
        &project.path().join(".nomnom").join("packs").join("house"),
        "house-style",
        &common::rule("house-rule", "review"),
    );
    let dir = explicit.to_str().unwrap();

    let text = project.ok(&["pack", "list", "--pack", dir]);
    for expected in ["NAME", "TIER", "TRUST", "PINNED", "built-in", "house-style", "project"] {
        assert!(text.contains(expected), "`{expected}` missing from:\n{text}");
    }
    assert!(text.contains("untrusted"), "{text}");

    let json: serde_json::Value =
        serde_json::from_str(&project.ok(&["pack", "list", "--pack", dir, "--json"]))
            .expect("valid JSON");
    let packs = json["packs"].as_array().expect("packs array");
    let rows: Vec<(&str, &str, &str)> = packs
        .iter()
        .map(|row| {
            (
                row["name"].as_str().expect("name"),
                row["tier"].as_str().expect("tier"),
                row["trust"].as_str().expect("trust"),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("builtin", "built-in", "built-in"),
            ("house-style", "project", "untrusted"),
            ("vendor", "explicit", "untrusted"),
        ],
        "resolution order is built-in, user, project, --pack"
    );
    // Every row carries the pin slot even when it is empty, so a consumer never
    // has to tell "no key" from "not pinned".
    assert!(packs.iter().all(|row| row.get("sha").is_some()), "{packs:?}");
}

/// `docs/lang.md`: a pack is "pinned to a commit, never to a branch". The
/// regression: `add` acknowledging the branch name the user typed, which leaves
/// them believing the lock tracks that branch.
#[test]
fn pack_add_prints_the_commit_sha_it_pinned_to_not_the_ref_that_was_asked_for() {
    let project = Project::new();
    let repo = project.path().join("packs.git");
    common::init_repo(&repo);
    common::write_pack(&repo.join("rust"), "rust", &common::rule("cargo-target", "reclaimable"));
    let sha = common::commit(&repo, "the pack");
    let url = format!("{}/rust@main", common::file_url(&repo));

    let text = project.ok(&["pack", "add", &url]);

    assert!(text.contains(&sha), "the resolved SHA is missing:\n{text}");
    assert!(text.contains("never to a branch"), "{text}");
    assert!(text.contains("not trusted"), "a freshly added pack is untrusted:\n{text}");

    let listed = project.ok(&["pack", "list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&listed).expect("valid JSON");
    let rust = json["packs"]
        .as_array()
        .expect("packs")
        .iter()
        .find(|row| row["name"] == "rust")
        .expect("the added pack is listed");
    assert_eq!(rust["sha"].as_str(), Some(sha.as_str()));
    assert_eq!(rust["tier"].as_str(), Some("user"));

    // `update` is the only operation allowed to move the pin, and `remove`
    // takes the row back out again.
    assert!(project.ok(&["pack", "update", "rust"]).contains(&sha));
    assert!(project.ok(&["pack", "remove", "rust", "--json"]).contains("\"removed\": \"rust\""));
    let after: serde_json::Value =
        serde_json::from_str(&project.ok(&["pack", "list", "--json"])).expect("valid JSON");
    assert_eq!(after["packs"].as_array().expect("packs").len(), 1, "only the built-in is left");
}

/// The regression: flattening a `nomnom-pack` error to "could not add pack".
/// An auth failure and a missing repository both exit 128 from `git`, so the
/// command and its stderr are what separate them, and `main.rs` prints
/// `{error:#}` — which only helps if the detail survives the chain.
#[test]
fn a_failed_git_fetch_surfaces_the_command_and_its_stderr() {
    let project = Project::new();
    let missing = common::file_url(&project.path().join("no-such-repository"));

    let output = project.nomnom(&["pack", "add", &missing]);
    assert!(!output.status.success(), "adding a missing repository must fail");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ls-remote"), "the git command is missing:\n{stderr}");
    assert!(stderr.contains("repository"), "git's own stderr is missing:\n{stderr}");
    assert!(stderr.contains("command:"), "{stderr}");
    assert!(stderr.contains("stderr:"), "{stderr}");
}

/// Trust is keyed by a pack's own name, so a typo has no pack to attach to. The
/// regression: writing a trust row for a name nothing resolves to, which
/// reports success and leaves the real pack still capped.
#[test]
fn trusting_a_name_no_pack_resolves_to_fails_and_lists_what_does() {
    let project = Project::new();
    let pack = local_pack(project.path(), "vendor", "reclaimable");

    let output = project.nomnom(&["pack", "trust", "vender", "--pack", pack.to_str().unwrap()]);
    assert!(!output.status.success(), "a typo must not record a trust grant");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no pack named `vender`"), "{stderr}");
    assert!(stderr.contains("vendor"), "the error must name what does resolve:\n{stderr}");
    assert!(!project.path().join(".nomnom").join("packs.lock").exists(), "a lock was written");
}
