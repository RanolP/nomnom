//! The three `git` operations acquiring a pack needs, over the installed binary.
//!
//! No git library: `ls-remote`, a shallow fetch of one commit and a checkout
//! are the whole surface, and shelling out keeps the user's credential helpers,
//! proxies and SSH config working without nomnom reimplementing any of it.
//!
//! Every failure carries the command and its stderr. A clone that failed on
//! authentication and one that failed on a bad ref exit with the same code, so
//! the status alone tells a user nothing about which of the two happened.
//!
//! [`Git::invocations`] counts calls to the binary. The cache's promise is
//! that a pack already on disk is used without touching the network, and a
//! counter is the only way a test can hold it to that.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Git {
    program: OsString,
    calls: Arc<AtomicUsize>,
}

impl Default for Git {
    fn default() -> Self {
        Git::new()
    }
}

impl Git {
    /// The `git` on `PATH`.
    pub fn new() -> Git {
        Git::with_program("git")
    }

    /// A specific binary. Tests point this at a name that does not exist to
    /// exercise the "git is not installed" path.
    pub fn with_program(program: impl Into<OsString>) -> Git {
        Git { program: program.into(), calls: Arc::new(AtomicUsize::new(0)) }
    }

    /// How many times the binary has been run through this handle. Clones
    /// share the count.
    pub fn invocations(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// A ref — or the default branch, when `reference` is `None` — to the full
    /// 40-hex commit SHA a pack is pinned to.
    ///
    /// A reference that already is a commit SHA is returned as written: it is
    /// already a pin, and asking the remote to confirm it would spend a
    /// network round trip to learn nothing the fetch will not tell us.
    pub fn resolve_sha(&self, url: &str, reference: Option<&str>) -> Result<String> {
        if let Some(reference) = reference
            && is_sha(reference)
        {
            return Ok(reference.to_ascii_lowercase());
        }
        let target = reference.unwrap_or("HEAD");
        let attempting = format!("resolving `{target}` in {url} to a commit");
        let out = self.run(&attempting, None, &["ls-remote", url, target])?;

        // `ls-remote` exits 0 with no output for a ref that does not exist, so
        // an empty answer is the error rather than a non-zero status.
        let mut candidates: Vec<(&str, &str)> = Vec::new();
        for line in out.lines() {
            if let Some((sha, name)) = line.split_once('\t') {
                candidates.push((sha.trim(), name.trim()));
            }
        }
        let pick = candidates
            .iter()
            .find(|(_, name)| *name == format!("refs/tags/{target}^{{}}"))
            .or_else(|| candidates.iter().find(|(_, name)| *name == format!("refs/heads/{target}")))
            .or_else(|| candidates.iter().find(|(_, name)| *name == target))
            .or(candidates.first())
            .ok_or_else(|| Error::RefNotFound {
                url: url.to_string(),
                reference: target.to_string(),
            })?;

        if !is_sha(pick.0) {
            return Err(Error::NotACommit {
                url: url.to_string(),
                reference: target.to_string(),
                got: pick.0.to_string(),
            });
        }
        Ok(pick.0.to_ascii_lowercase())
    }

    /// Puts the tree of `sha` into `dest`, which must not exist yet.
    ///
    /// A shallow fetch of the one commit: the pack is a handful of text files
    /// and its history is of no interest to anything downstream.
    pub fn checkout_into(&self, url: &str, sha: &str, dest: &Path) -> Result<()> {
        let attempting = format!("fetching commit {sha} from {url}");
        let dest_text = dest.display().to_string();
        self.run(&attempting, None, &["init", "--quiet", &dest_text])?;
        self.run(&attempting, Some(dest), &["remote", "add", "origin", url])?;
        self.run(&attempting, Some(dest), &["fetch", "--quiet", "--depth", "1", "origin", sha])?;
        self.run(&attempting, Some(dest), &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
        Ok(())
    }

    fn run(&self, attempting: &str, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut command = Command::new(&self.program);
        if let Some(cwd) = cwd {
            command.arg("-C").arg(cwd);
        }
        command.args(args);
        let rendered = render(&self.program, cwd, args);

        let output = command.output().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::GitMissing {
                    program: self.program.to_string_lossy().into_owned(),
                    attempting: attempting.to_string(),
                }
            } else {
                Error::Git {
                    attempting: attempting.to_string(),
                    command: rendered.clone(),
                    status: source.kind().to_string(),
                    stderr: source.to_string(),
                }
            }
        })?;

        if !output.status.success() {
            return Err(Error::Git {
                attempting: attempting.to_string(),
                command: rendered,
                status: match output.status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => "killed by a signal".to_string(),
                },
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn render(program: &OsString, cwd: Option<&Path>, args: &[&str]) -> String {
    let mut parts = vec![program.to_string_lossy().into_owned()];
    if let Some(cwd) = cwd {
        parts.push("-C".to_string());
        parts.push(cwd.display().to_string());
    }
    parts.extend(args.iter().map(|a| (*a).to_string()));
    parts.join(" ")
}

/// A full commit SHA, which is the only thing a pack is ever pinned to.
pub fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}
