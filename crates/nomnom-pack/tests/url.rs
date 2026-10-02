//! Splitting a pack URL into a repository, a subdirectory and a ref.

use nomnom_pack::{Error, PackUrl};

fn reason(url: &str) -> String {
    match PackUrl::parse(url) {
        Ok(parsed) => panic!("`{url}` should not parse, got {parsed:?}"),
        Err(Error::Url { reason, .. }) => reason,
        Err(other) => panic!("expected a URL error, got {other}"),
    }
}

/// The short form in `docs/lang.md`. The regression: reading `rust` as part of
/// the repository name, which makes the fetch ask for a repository that does
/// not exist and loses the subdirectory the pack actually lives in.
#[test]
fn the_short_form_splits_owner_repo_and_subdirectory() {
    let url = PackUrl::parse("github.com/ranolp/nomnom-packs/rust").expect("parses");
    assert_eq!(url.host, "github.com");
    assert_eq!(url.owner, "ranolp");
    assert_eq!(url.repo, "nomnom-packs");
    assert_eq!(url.subdir.as_deref(), Some("rust"));
    assert_eq!(url.reference, None);
    assert_eq!(url.git_url, "https://github.com/ranolp/nomnom-packs");
}

/// The second form in `docs/lang.md`. The regression: swallowing `@a1b2c3d`
/// into the repository name, which would pin nothing and fetch nothing.
#[test]
fn an_explicit_scheme_keeps_the_dot_git_repo_and_lifts_the_ref() {
    let url = PackUrl::parse("https://git.example/packs.git@a1b2c3d").expect("parses");
    assert_eq!(url.host, "git.example");
    assert_eq!(url.owner, "_");
    assert_eq!(url.repo, "packs");
    assert_eq!(url.subdir, None);
    assert_eq!(url.reference.as_deref(), Some("a1b2c3d"));
    assert_eq!(url.git_url, "https://git.example/packs.git");
    assert_eq!(url.raw, "https://git.example/packs.git@a1b2c3d");
}

/// A `.git` segment marks the repository, so anything after it is inside it.
#[test]
fn a_dot_git_segment_marks_where_the_subdirectory_starts() {
    let url = PackUrl::parse("https://git.example/team/packs.git/rust/nightly@v2").expect("parses");
    assert_eq!(url.owner, "team");
    assert_eq!(url.repo, "packs");
    assert_eq!(url.subdir.as_deref(), Some("rust/nightly"));
    assert_eq!(url.git_url, "https://git.example/team/packs.git");
}

/// The regression: an `ssh://git@host/org/repo` URL whose userinfo `@` gets
/// read as a ref, pinning the pack to a branch called `git` that cannot exist.
#[test]
fn an_at_sign_before_a_slash_is_userinfo_and_not_a_ref() {
    let url = PackUrl::parse("ssh://git@github.com/ranolp/nomnom-packs").expect("parses");
    assert_eq!(url.reference, None);
    assert_eq!(url.git_url, "ssh://git@github.com/ranolp/nomnom-packs");
}

/// Malformed URLs have to say what is wrong; `pack add` is typed by hand.
#[test]
fn malformed_urls_name_what_is_missing() {
    assert!(reason("github.com").contains("no repository"), "{}", reason("github.com"));
    assert!(reason("github.com/ranolp").contains("owner with no repository"));
    assert!(reason("github.com/ranolp/packs@").contains("no ref after it"));
    assert!(reason("ranolp/nomnom-packs").contains("does not look like a host name"));
    assert!(reason("ftp://git.example/packs.git").contains("not a scheme git can fetch"));
    assert!(reason("   ").contains("empty"));
}

/// The cache path in `docs/lang.md` is `<host>/<org>/<repo>@<sha>`. The
/// regression: a `..` or a `/` reaching the path join and writing a checkout
/// outside the cache root.
#[test]
fn cache_components_cannot_escape_the_cache_root() {
    let url = PackUrl::parse("github.com/ranolp/nomnom-packs/rust").expect("parses");
    let relative = url.cache_relative("a".repeat(40).as_str());
    assert_eq!(
        relative.to_string_lossy().replace('\\', "/"),
        format!("github.com/ranolp/nomnom-packs@{}", "a".repeat(40))
    );

    let evil = PackUrl::parse("evil.com/../../windows/system32").expect("parses");
    let escaped = evil.cache_relative("b");
    assert!(!escaped.to_string_lossy().contains(".."), "{}", escaped.display());
}
