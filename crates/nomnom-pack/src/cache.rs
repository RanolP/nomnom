//! The on-disk pack store: `%LOCALAPPDATA%\nomnom\packs` and what lives under it.
//!
//! ```text
//! %LOCALAPPDATA%\nomnom\packs\
//!   github.com\ranolp\nomnom-packs@a1b2c3…\   a fetched repository, one directory per commit
//!   my-hand-written-pack\pack.toml            a user-tier pack, placed by hand
//! ```
//!
//! `docs/lang.md` gives the same path for the user tier and for the fetch
//! cache, so the two share a root and are told apart by shape: a directory
//! holding `pack.toml` directly under the root is a user pack, and a fetched
//! repository is always three levels down under a host name. Nothing is ever
//! both.
//!
//! The cache is addressed by commit SHA, so an entry is never stale and never
//! invalidated — only added to. An entry that exists is used as it is, with no
//! `git` invocation at all.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::url::PackUrl;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    git: Git,
}

impl Store {
    /// The store at the platform's per-user data directory.
    pub fn open() -> Result<Store> {
        Ok(Store::at(default_root()?, Git::new()))
    }

    pub fn at(root: impl Into<PathBuf>, git: Git) -> Store {
        Store { root: root.into(), git }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Where the repository behind `url` sits when checked out at `sha`.
    pub fn entry(&self, url: &PackUrl, sha: &str) -> PathBuf {
        self.root.join(url.cache_relative(sha))
    }

    /// The cached checkout of `sha`, fetching it only if it is not there yet.
    pub fn ensure(&self, url: &PackUrl, sha: &str) -> Result<PathBuf> {
        let dest = self.entry(url, sha);
        if dest.is_dir() {
            return Ok(dest);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
        }
        std::fs::create_dir_all(&self.root).map_err(|source| Error::io(&self.root, source))?;

        // Fetch into a staging directory and rename it into place, so an
        // interrupted fetch never leaves a half-populated entry that the next
        // run would mistake for a complete one.
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&self.root)
            .map_err(|source| Error::io(&self.root, source))?;
        let staged = staging.path().join("repo");
        self.git.checkout_into(&url.git_url, sha, &staged)?;
        std::fs::rename(&staged, &dest).map_err(|source| Error::io(&dest, source))?;
        Ok(dest)
    }

    /// The directory that actually holds `pack.toml` — the checkout, plus the
    /// subdirectory the URL named.
    pub fn pack_dir(&self, url: &PackUrl, sha: &str) -> Result<PathBuf> {
        let checkout = self.ensure(url, sha)?;
        Ok(match &url.subdir {
            Some(subdir) => checkout.join(subdir.replace('/', std::path::MAIN_SEPARATOR_STR)),
            None => checkout,
        })
    }
}

/// `%LOCALAPPDATA%\nomnom\packs` on Windows, `$XDG_DATA_HOME/nomnom/packs`
/// elsewhere.
pub fn default_root() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Local"))
            })
            .ok_or_else(|| Error::NoCacheRoot { tried: "%LOCALAPPDATA%, %USERPROFILE%".into() })?
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .ok_or_else(|| Error::NoCacheRoot { tried: "$XDG_DATA_HOME, $HOME".into() })?
    };
    Ok(base.join("nomnom").join("packs"))
}
