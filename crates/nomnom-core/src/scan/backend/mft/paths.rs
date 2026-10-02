//! Path reconstruction by chasing parent file references.
//!
//! The MFT stores a name and a parent reference per record, never a full path.
//! Rebuilding one path per file by walking to the volume root would be
//! quadratic, so every directory's resolved path is cached: a directory is
//! resolved once and every file inside it then costs one `join`.
//!
//! This module deliberately knows nothing about NTFS or Windows. It takes a
//! `record -> (name, parent)` map, which makes the parent-chasing loop — the
//! part most likely to break — testable without a raw volume handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The root directory's file record number, fixed on every NTFS volume.
pub const ROOT_RECORD: u64 = 5;

/// Longest parent chain followed before the chain is declared broken. NTFS
/// caps paths far below this; a longer chain means a cycle in a corrupt or
/// concurrently-modified MFT, and the loop must terminate rather than hang.
const MAX_DEPTH: usize = 512;

/// The two facts path reconstruction needs about a directory record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRecord {
    pub name: String,
    pub parent: u64,
}

/// Resolves record numbers to full paths, caching every directory it resolves.
pub struct PathBuilder<'a> {
    dirs: &'a HashMap<u64, DirRecord>,
    root_path: PathBuf,
    root_record: u64,
    /// `None` marks a record whose parent chain does not reach the root, so a
    /// broken chain is walked once rather than once per child.
    cache: HashMap<u64, Option<PathBuf>>,
}

impl<'a> PathBuilder<'a> {
    /// `root_path` is the path that `root_record` stands for — `C:\` for a real
    /// volume, any absolute path for a synthetic map.
    pub fn new(dirs: &'a HashMap<u64, DirRecord>, root_path: PathBuf, root_record: u64) -> Self {
        Self { dirs, root_path, root_record, cache: HashMap::new() }
    }

    /// Full path of a directory record, or `None` when its parent chain does
    /// not reach the root (a missing parent, or a cycle).
    pub fn dir_path(&mut self, record: u64) -> Option<PathBuf> {
        if record == self.root_record {
            return Some(self.root_path.clone());
        }
        if let Some(cached) = self.cache.get(&record) {
            return cached.clone();
        }

        // Walk up to the nearest ancestor whose path is already known, stacking
        // the names on the way, then unwind and cache each level.
        let mut chain: Vec<(u64, &str)> = Vec::new();
        let mut cursor = record;
        let resolved = loop {
            if cursor == self.root_record {
                break Some(self.root_path.clone());
            }
            if let Some(cached) = self.cache.get(&cursor) {
                break cached.clone();
            }
            if chain.len() >= MAX_DEPTH {
                break None;
            }
            match self.dirs.get(&cursor) {
                // A record that is its own parent is corrupt; treating it as a
                // dead chain is what keeps the loop finite.
                Some(dir) if dir.parent != cursor => {
                    chain.push((cursor, dir.name.as_str()));
                    cursor = dir.parent;
                }
                _ => break None,
            }
        };

        let mut acc = resolved;
        for (number, name) in chain.into_iter().rev() {
            acc = acc.map(|parent| parent.join(name));
            self.cache.insert(number, acc.clone());
        }
        acc
    }

    /// Full path of a non-directory record given the parent it is filed under.
    ///
    /// Takes the parent explicitly rather than the child's own record number,
    /// because a hard-linked file has one `$FILE_NAME` per directory it appears
    /// in and each of them names a different path for the same record.
    pub fn child_path(&mut self, parent: u64, name: &str) -> Option<PathBuf> {
        self.dir_path(parent).map(|dir| dir.join(name))
    }

    /// Number of directories resolved and cached so far. Lets a caller prove
    /// the cache is doing its job instead of assuming it.
    pub fn cached_len(&self) -> usize {
        self.cache.len()
    }
}

/// Re-spells `full` — a volume-absolute path built from on-disk names — under
/// `root_spelling`, or returns `None` when `full` is not inside `root_canon`.
///
/// The comparison is case-insensitive because NTFS is, but the result keeps the
/// caller's spelling of the root and the volume's spelling of everything below
/// it. That byte-for-byte agreement with the walk backend is what lets
/// `Catalog::build` re-attach children to parents by path prefix.
pub fn respell_under(full: &Path, root_canon: &Path, root_spelling: &Path) -> Option<PathBuf> {
    let mut full_parts = full.components();
    for root_part in root_canon.components() {
        let full_part = full_parts.next()?;
        let (a, b) = (full_part.as_os_str(), root_part.as_os_str());
        if !a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()) {
            return None;
        }
    }

    // Nothing left to append means `full` IS the root, which the walk backend
    // emits as an entry too, so it is a hit rather than a miss.
    let mut out = root_spelling.to_path_buf();
    for part in full_parts {
        out.push(part.as_os_str());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(entries: &[(u64, &str, u64)]) -> HashMap<u64, DirRecord> {
        entries
            .iter()
            .map(|(n, name, parent)| (*n, DirRecord { name: (*name).to_string(), parent: *parent }))
            .collect()
    }

    #[test]
    fn deep_chain_resolves_and_missing_parent_yields_none() {
        let map = dirs(&[(20, "a", ROOT_RECORD), (21, "b", 20), (22, "c", 21), (30, "x", 999)]);
        let mut b = PathBuilder::new(&map, PathBuf::from("C:\\"), ROOT_RECORD);
        assert_eq!(b.dir_path(22), Some(PathBuf::from("C:\\a\\b\\c")));
        assert_eq!(b.child_path(22, "f.txt"), Some(PathBuf::from("C:\\a\\b\\c\\f.txt")));
        assert_eq!(b.dir_path(30), None);
    }

    #[test]
    fn cycle_terminates() {
        let map = dirs(&[(20, "a", 21), (21, "b", 20)]);
        let mut b = PathBuilder::new(&map, PathBuf::from("C:\\"), ROOT_RECORD);
        assert_eq!(b.dir_path(20), None);
    }
}
