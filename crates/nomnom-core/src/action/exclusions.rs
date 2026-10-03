//! `.nomnom/exclusions.toml` — paths the user keeps out of every plan.
//!
//! Approving a rule is per assessment and clears with it; an exclusion is the
//! opposite kind of choice ("never this one, it is the `target/` of the project
//! I am working on") and survives rescans, because it can only ever shrink a
//! plan. It sits beside `.nomnom/packs.lock` at the drive root, in the same
//! TOML shape:
//!
//! ```toml
//! version = 1
//! paths = ['D:\work\nomnom\target', 'D:\work\active']
//! ```
//!
//! An excluded directory excludes everything under it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const EXCLUSIONS_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ExclusionError {
    #[error("cannot read or write {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a valid exclusion list: {message}")]
    Format { path: PathBuf, message: String },
    #[error("cannot exclude {path}: it is not under the drive root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusions {
    version: u32,
    #[serde(default)]
    paths: BTreeSet<PathBuf>,
}

impl Default for Exclusions {
    fn default() -> Self {
        Exclusions { version: EXCLUSIONS_VERSION, paths: BTreeSet::new() }
    }
}

impl Exclusions {
    /// `<root>/.nomnom/exclusions.toml`.
    pub fn path_in(root: &Path) -> PathBuf {
        root.join(".nomnom").join("exclusions.toml")
    }

    /// The list for the drive at `root`; no file is an empty list.
    pub fn load(root: &Path) -> Result<Exclusions, ExclusionError> {
        let path = Exclusions::path_in(root);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Exclusions::default());
            }
            Err(source) => return Err(ExclusionError::Io { path, source }),
        };
        let list: Exclusions = toml::from_str(&text).map_err(|error| ExclusionError::Format {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if list.version != EXCLUSIONS_VERSION {
            return Err(ExclusionError::Format {
                path,
                message: format!(
                    "format version {} is not version {EXCLUSIONS_VERSION}, which this nomnom writes",
                    list.version
                ),
            });
        }
        Ok(list)
    }

    pub fn save(&self, root: &Path) -> Result<(), ExclusionError> {
        let path = Exclusions::path_in(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| ExclusionError::Io { path: parent.to_path_buf(), source })?;
        }
        let text = toml::to_string_pretty(self).map_err(|error| ExclusionError::Format {
            path: path.clone(),
            message: error.to_string(),
        })?;
        fs::write(&path, text).map_err(|source| ExclusionError::Io { path, source })
    }

    pub fn paths(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.paths.iter().map(PathBuf::as_path)
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Adds `path`, made absolute, when it lies under `root`. `Ok(false)` when
    /// the same path (by [`same`]) is already listed.
    pub fn add(&mut self, root: &Path, path: &Path) -> Result<bool, ExclusionError> {
        let absolute = tidy(path);
        if !under(&absolute, root) {
            return Err(ExclusionError::OutsideRoot { path: absolute, root: root.to_path_buf() });
        }
        if self.paths.iter().any(|listed| same(listed, &absolute)) {
            return Ok(false);
        }
        self.paths.insert(absolute);
        Ok(true)
    }

    /// Removes the listed path that is `path`; `false` when none is.
    pub fn remove(&mut self, path: &Path) -> bool {
        let absolute = tidy(path);
        let before = self.paths.len();
        self.paths.retain(|listed| !same(listed, &absolute));
        self.paths.len() != before
    }

    /// The listed path that keeps `path` out of plans: `path` itself or a
    /// directory above it.
    pub fn covering(&self, path: &Path) -> Option<&Path> {
        self.paths.iter().map(PathBuf::as_path).find(|listed| under(path, listed))
    }

    /// Whether `path` itself is listed, rather than covered by an ancestor.
    pub fn lists(&self, path: &Path) -> bool {
        self.paths.iter().any(|listed| same(listed, path))
    }

    pub fn excludes(&self, path: &Path) -> bool {
        self.covering(path).is_some()
    }
}

/// Absolute, with no trailing separator, so `d:\proj\target\` and
/// `D:\proj\target` compare as one path.
fn tidy(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute.components().collect()
}

/// Component-wise, ASCII case-insensitive on Windows where NTFS is.
fn key(path: &Path) -> Vec<String> {
    tidy(path)
        .components()
        .map(|part| {
            let text = part.as_os_str().to_string_lossy();
            if cfg!(windows) { text.to_ascii_lowercase() } else { text.into_owned() }
        })
        .collect()
}

fn same(a: &Path, b: &Path) -> bool {
    key(a) == key(b)
}

/// Whether `path` is `ancestor` or lies below it.
fn under(path: &Path, ancestor: &Path) -> bool {
    let (path, ancestor) = (key(path), key(ancestor));
    path.len() >= ancestor.len() && path[..ancestor.len()] == ancestor[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches an exclusion that is lost when the app restarts or a rescan
    // reloads the list, which would put the user's active `target/` back on a
    // plan they approve by rule.
    #[test]
    fn an_exclusion_survives_a_reload_and_covers_its_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut list = Exclusions::load(root).unwrap();
        assert!(list.is_empty());
        assert!(list.add(root, &root.join("active").join("target")).unwrap());
        assert!(!list.add(root, &root.join("active").join("target").join("")).unwrap());
        list.save(root).unwrap();

        let reloaded = Exclusions::load(root).unwrap();
        assert_eq!(reloaded, list);
        assert!(reloaded.excludes(&root.join("active").join("target").join("debug")));
        assert!(!reloaded.excludes(&root.join("active")));
        assert!(!reloaded.excludes(&root.join("other").join("target")));

        let mut reloaded = reloaded;
        assert!(reloaded.remove(&root.join("active").join("target")));
        assert!(!reloaded.excludes(&root.join("active").join("target")));
    }

    // Catches an exclusion off the drive being stored, where it could never
    // match and would only clutter the list the user audits.
    #[test]
    fn an_exclusion_outside_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut list = Exclusions::default();
        assert!(list.add(tmp.path(), elsewhere.path()).is_err());
        assert!(list.is_empty());
    }
}
