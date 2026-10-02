//! Fixture helpers shared by the verdict tests.

use std::fs;
use std::path::Path;

use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{Backend, ScanOptions, scan};

/// Write a fixture file, creating the directories it needs.
pub fn write(path: impl AsRef<Path>, contents: &[u8]) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().expect("file has a parent")).expect("create parent");
    fs::write(path, contents).expect("write fixture file");
}

/// Scan a fixture tree with the portable backend, so the test says the same
/// thing on every platform.
pub fn catalog_of(root: &Path) -> Catalog {
    let opts = ScanOptions { backend: Backend::Walk, ..ScanOptions::default() };
    Catalog::build(scan(root, &opts).expect("scan fixture"))
}
