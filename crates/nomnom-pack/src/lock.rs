//! `.nomnom/packs.lock` — what a project loads, and the only thing it loads.
//!
//! **Location.** The lock sits at `<scan root>/.nomnom/packs.lock`, beside
//! `<scan root>/.nomnom/packs/` which `docs/lang.md` already fixes as the
//! project tier. The current working directory is deliberately not used: a
//! scan is run against a root given on the command line as often as against
//! `.`, and resolving the lock from the shell's location would mean the same
//! command loaded different rules depending on where it was typed.
//!
//! **The lock is what is loaded.** Resolving a pack reads the pinned SHA from
//! here and goes to the network only for a pack the lock does not have, or
//! when the user explicitly asks for an update. A pack whose content no longer
//! matches the recorded checksum is an error — see [`crate::Error::Drift`].
//!
//! ```toml
//! version = 1
//!
//! [[pack]]
//! name = "rust"
//! url = "github.com/ranolp/nomnom-packs/rust@main"
//! sha = "1c0ff33e1c0ff33e1c0ff33e1c0ff33e1c0ff33e"
//! subdir = "rust"
//! checksum = "blake3:9f2b…"
//! trusted = false
//! ```
//!
//! An entry with only `name` and `trusted` is a pack that lives in a directory
//! rather than in git — a hand-placed user pack, or one under
//! `.nomnom/packs/`. It is in the lock because trust is recorded per pack and
//! those packs can be trusted too.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::trust::Trust;

pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    #[serde(default, rename = "pack")]
    packs: Vec<LockedPack>,
}

impl Default for Lock {
    fn default() -> Self {
        Lock { version: LOCK_VERSION, packs: Vec::new() }
    }
}

/// One row of the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPack {
    /// The pack's own name, out of its `pack.toml`. This is the key: it is
    /// what `nomnom pack trust <name>` takes and what a verdict cites.
    pub name: String,
    /// The URL as the user wrote it, ref suffix and all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The commit the pack is pinned to. Always a full SHA — a pack is never
    /// loaded from a branch name, so the ref in `url` is a record of what was
    /// asked for, not of what is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The directory inside the repository holding `pack.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// `blake3:<hex>` over the pack's files, as of the commit above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Whether the user ran `nomnom pack trust <name>`.
    pub trusted: bool,
}

impl LockedPack {
    /// A trust-only row for a pack that lives in a directory.
    pub fn local(name: impl Into<String>) -> LockedPack {
        LockedPack {
            name: name.into(),
            url: None,
            sha: None,
            subdir: None,
            checksum: None,
            trusted: false,
        }
    }

    pub fn trust(&self) -> Trust {
        if self.trusted { Trust::Trusted } else { Trust::Untrusted }
    }

    /// `(url, sha)` when this row names a git pack.
    pub fn git(&self) -> Option<(&str, &str)> {
        Some((self.url.as_deref()?, self.sha.as_deref()?))
    }
}

impl Lock {
    /// `<root>/.nomnom/packs.lock`.
    pub fn path_in(root: &Path) -> PathBuf {
        root.join(".nomnom").join("packs.lock")
    }

    /// The lock for `root`. A project that has never added a pack has no lock
    /// file, and that is an empty lock rather than an error.
    pub fn load(root: &Path) -> Result<Lock> {
        let path = Lock::path_in(root);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Lock::default());
            }
            Err(source) => return Err(Error::io(path, source)),
        };
        let lock: Lock = toml::from_str(&text)
            .map_err(|error| Error::Lock { path: path.clone(), message: error.to_string() })?;
        if lock.version != LOCK_VERSION {
            return Err(Error::Lock {
                path,
                message: format!(
                    "lock format version {} is not version {LOCK_VERSION}, which this nomnom writes",
                    lock.version
                ),
            });
        }
        Ok(lock)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Lock::path_in(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| Error::Lock { path: path.clone(), message: error.to_string() })?;
        fs::write(&path, text).map_err(|source| Error::io(path, source))
    }

    pub fn packs(&self) -> &[LockedPack] {
        &self.packs
    }

    pub fn get(&self, name: &str) -> Option<&LockedPack> {
        self.packs.iter().find(|pack| pack.name == name)
    }

    /// The trust state to apply to a pack by name. A pack the lock has never
    /// heard of is untrusted, which is the direction that cannot lose data.
    pub fn trust_of(&self, name: &str) -> Trust {
        self.get(name).map_or(Trust::Untrusted, LockedPack::trust)
    }

    /// Adds or replaces a row, keeping whatever trust was already granted:
    /// re-adding a pack at a newer commit is not a reason to make the user
    /// trust it again, and rows stay sorted by name so the file does not churn.
    pub fn upsert(&mut self, mut pack: LockedPack) {
        if let Some(existing) = self.get(&pack.name) {
            pack.trusted = pack.trusted || existing.trusted;
        }
        self.packs.retain(|existing| existing.name != pack.name);
        self.packs.push(pack);
        self.packs.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.packs.len();
        self.packs.retain(|pack| pack.name != name);
        self.packs.len() != before
    }

    /// Records trust for a pack. A directory pack that is not in the lock yet
    /// gets a trust-only row, because trust is the one fact the lock holds
    /// about it.
    pub fn set_trust(&mut self, name: &str, trusted: bool) {
        match self.packs.iter_mut().find(|pack| pack.name == name) {
            Some(pack) => pack.trusted = trusted,
            None => {
                let mut pack = LockedPack::local(name);
                pack.trusted = trusted;
                self.upsert(pack);
            }
        }
    }

    /// The `nomnom pack trust <name>` side.
    pub fn trust(&mut self, name: &str) {
        self.set_trust(name, true);
    }

    /// Revoking is not a removal: the pack stays pinned, it just stops being
    /// allowed to propose deletions.
    pub fn revoke(&mut self, name: &str) {
        self.set_trust(name, false);
    }
}
