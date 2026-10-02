//! `github.com/ranolp/nomnom-packs/rust` to a repository, a subdirectory and a ref.
//!
//! Both forms `docs/lang.md` shows are one grammar:
//!
//! ```text
//! [scheme://]host/<path...>[@<ref>]
//! ```
//!
//! The path has to be split into "the repository" and "a directory inside it",
//! and there is no delimiter for that, so two rules decide it. A segment
//! ending in `.git` is the repository and everything after it is the
//! subdirectory — that is how `https://git.example/packs.git/rust` names a
//! pack inside `packs.git`. Absent a `.git` segment the first two segments are
//! the owner and the repository, which is what every forge URL looks like, and
//! the rest is the subdirectory.

use crate::error::{Error, Result};

/// A pack URL, split into the parts the cache path and the `git` invocations need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackUrl {
    /// Exactly what the user typed, ref suffix included. This is what goes
    /// into the lock, because the lock has to be able to say where a pack came
    /// from in the words its owner used.
    pub raw: String,
    /// `github.com`, or `file` for a local repository.
    pub host: String,
    /// The path segment above the repository — `_` when the repository sits at
    /// the root of the host.
    pub owner: String,
    /// The repository name, without any `.git` suffix.
    pub repo: String,
    /// The directory inside the repository that holds `pack.toml`, if the URL
    /// named one.
    pub subdir: Option<String>,
    /// The `@<ref>` suffix: a branch, a tag or a commit. `None` means the
    /// repository's default branch.
    pub reference: Option<String>,
    /// What gets handed to `git ls-remote` and `git fetch`.
    pub git_url: String,
}

const SCHEMES: [&str; 5] = ["https", "http", "ssh", "git", "file"];

impl PackUrl {
    pub fn parse(text: &str) -> Result<PackUrl> {
        let raw = text.trim().to_string();
        if raw.is_empty() {
            return Err(bad(&raw, "it is empty"));
        }

        let (without_ref, reference) = split_ref(&raw)?;

        let (scheme, rest) = match without_ref.split_once("://") {
            Some((scheme, rest)) => {
                if !SCHEMES.contains(&scheme) {
                    return Err(bad(
                        &raw,
                        format!(
                            "`{scheme}://` is not a scheme git can fetch (expected one of {})",
                            SCHEMES.join(", ")
                        ),
                    ));
                }
                (scheme, rest)
            }
            // The documented short form. `github.com/ranolp/nomnom-packs` is
            // https, the same way every forge's own copy button writes it.
            None => ("https", without_ref),
        };

        if scheme == "file" {
            return from_file(&raw, reference, rest);
        }

        let mut segments = rest.split('/').filter(|s| !s.is_empty());
        let host = segments.next().unwrap_or_default().to_string();
        if !host.contains('.') && host != "localhost" {
            return Err(bad(&raw, format!("`{host}` does not look like a host name")));
        }
        let path: Vec<&str> = segments.collect();
        let (owner, repo, subdir, repo_idx) = split_path(&raw, &path)?;

        let base = path[..=repo_idx].join("/");
        let git_url = format!("{scheme}://{host}/{base}");
        Ok(PackUrl { raw: raw.clone(), host, owner, repo, subdir, reference, git_url })
    }

    /// The cache directory name for this repository at `sha`:
    /// `<host>/<owner>/<repo>@<sha>`, each component made safe to be one.
    pub fn cache_relative(&self, sha: &str) -> std::path::PathBuf {
        std::path::Path::new(&component(&self.host)).join(component(&self.owner)).join(format!(
            "{}@{}",
            component(&self.repo),
            component(sha)
        ))
    }
}

/// A local repository, used by the tests and by anyone vendoring a pack on the
/// same machine. The whole path is the repository unless a `.git` segment says
/// otherwise, and the owner is the directory it sits in — enough to keep two
/// same-named checkouts apart in the cache.
fn from_file(raw: &str, reference: Option<String>, rest: &str) -> Result<PackUrl> {
    let path: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if path.is_empty() {
        return Err(bad(raw, "a `file://` URL needs a path to a repository"));
    }
    let repo_idx = path.iter().position(|s| s.ends_with(".git")).unwrap_or(path.len() - 1);
    let repo = path[repo_idx].trim_end_matches(".git").to_string();
    let owner = if repo_idx == 0 { "_".to_string() } else { path[repo_idx - 1].to_string() };
    let subdir = join_subdir(&path[repo_idx + 1..]);
    let base = path[..=repo_idx].join("/");
    Ok(PackUrl {
        raw: raw.to_string(),
        host: "file".to_string(),
        owner,
        repo,
        subdir,
        reference,
        git_url: format!("file:///{base}"),
    })
}

/// `(owner, repo, subdir, index of the repo segment)`.
fn split_path(raw: &str, path: &[&str]) -> Result<(String, String, Option<String>, usize)> {
    if let Some(idx) = path.iter().position(|s| s.ends_with(".git")) {
        let owner = if idx == 0 { "_".to_string() } else { path[idx - 1].to_string() };
        let repo = path[idx].trim_end_matches(".git").to_string();
        return Ok((owner, repo, join_subdir(&path[idx + 1..]), idx));
    }
    match path {
        [] => Err(bad(raw, "it names a host but no repository")),
        [only] => Err(bad(
            raw,
            format!(
                "`{only}` is an owner with no repository — expected `<host>/<owner>/<repo>[/<subdir>]`"
            ),
        )),
        [owner, repo, subdir @ ..] => {
            Ok((owner.to_string(), repo.to_string(), join_subdir(subdir), 1))
        }
    }
}

fn join_subdir(segments: &[&str]) -> Option<String> {
    (!segments.is_empty()).then(|| segments.join("/"))
}

/// Splits a trailing `@<ref>`. An `@` with a `/` after it belongs to a
/// userinfo part (`ssh://git@host/org/repo`), not to a ref.
fn split_ref(raw: &str) -> Result<(&str, Option<String>)> {
    match raw.rfind('@') {
        Some(at) if !raw[at + 1..].contains('/') => {
            let reference = &raw[at + 1..];
            if reference.is_empty() {
                return Err(bad(raw, "it ends in `@` with no ref after it"));
            }
            Ok((&raw[..at], Some(reference.to_string())))
        }
        _ => Ok((raw, None)),
    }
}

/// Makes one URL part usable as one directory name. A port in a host and a
/// `/` in a nested owner are the realistic cases; `..` is the one that matters.
fn component(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') { "_".to_string() } else { cleaned }
}

fn bad(url: &str, reason: impl Into<String>) -> Error {
    Error::Url { url: url.to_string(), reason: reason.into() }
}
