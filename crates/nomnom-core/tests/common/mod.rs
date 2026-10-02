//! Fixture helpers shared by the catalog and verdict tests.
//!
//! The public scan only accepts a whole volume, so a fixture tree in a temp
//! directory is turned into a [`ScanReport`] here, by a plain recursive
//! `read_dir`, rather than through `scan`. That keeps the catalog and verdict
//! logic covered on small trees without reopening the folder-scan hole.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{BackendUsed, Entry, EntryKind, ScanReport};

/// Write a fixture file, creating the directories it needs.
pub fn write(path: impl AsRef<Path>, contents: &[u8]) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().expect("file has a parent")).expect("create parent");
    fs::write(path, contents).expect("write fixture file");
}

/// Every entry under `root`, `root` included, spelled the way the walk backend
/// spells them: `root` joined with each name.
pub fn report_of(root: &Path) -> ScanReport {
    let mut entries = vec![entry(root)];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for child in fs::read_dir(&dir).expect("read fixture dir") {
            let path = child.expect("fixture dir entry").path();
            let entry = entry(&path);
            if entry.kind == EntryKind::Dir {
                stack.push(path);
            }
            entries.push(entry);
        }
    }
    ScanReport {
        root: root.to_path_buf(),
        entries,
        errors: Vec::new(),
        backend_used: BackendUsed::Walk { mft_unavailable: None },
    }
}

fn entry(path: &Path) -> Entry {
    let meta = fs::symlink_metadata(path).expect("stat fixture entry");
    let kind = if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };
    Entry {
        path: path.to_path_buf(),
        kind,
        size: if kind == EntryKind::Dir { 0 } else { meta.len() },
        allocated: None,
        modified: meta.modified().ok(),
        accessed: meta.accessed().ok(),
    }
}

pub fn catalog_of(root: &Path) -> Catalog {
    Catalog::build(report_of(root))
}
