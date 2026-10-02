//! Fixtures the command tests share.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

use crate::pack::{self, PackCommand};
use crate::pack_fixtures;
use crate::scan_fixtures::write;

/// Points the pack store at a temp directory for the whole test binary.
///
/// The store root comes from `%LOCALAPPDATA%` / `$XDG_DATA_HOME`, so this is
/// what keeps a test run out of the developer's real pack cache, and a pack
/// they happen to have installed out of a test's listing. Every test that
/// reaches `Store::open` calls this first; it sets the variables once, to the
/// same value, before any of those tests reads them.
pub fn isolated_store() {
    static STORE: OnceLock<TempDir> = OnceLock::new();
    STORE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("store tempdir");
        // SAFETY: std serialises its own environment access, and nothing in
        // this test binary reads the environment outside std.
        unsafe {
            std::env::set_var("LOCALAPPDATA", dir.path());
            std::env::set_var("XDG_DATA_HOME", dir.path());
        }
        dir
    });
}

/// A tree that trips exactly one rule: `node_modules` beside a `package.json`.
pub fn node_fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(root.join("package.json"), br#"{"name":"fixture"}"#);
    write(root.join("src/index.js"), b"console.log('hi');\n");
    write(root.join("node_modules/left-pad/index.js"), b"module.exports = 1;\n");
    write(root.join("node_modules/left-pad/package.json"), br#"{"name":"left-pad"}"#);
    write(root.join("node_modules/.bin/left-pad"), b"#!/bin/sh\n");
    dir
}

/// A pack directory a run can load with `--pack`.
pub fn local_pack(dir: &Path, name: &str, disposition: &str) -> PathBuf {
    let path = dir.join(name);
    pack_fixtures::write_pack(&path, name, &pack_fixtures::rule("marked-dir", disposition));
    path
}

/// The tree `pack_fixtures::rule` matches: a directory holding a `marker` file.
pub fn marked_tree(root: &Path) {
    write(root.join("blobs").join("marker"), b"x");
}

/// Runs a pack command against `root`'s lock and returns what it printed,
/// failing the test with the error chain `main` would print.
pub fn pack_ok(root: &Path, command: PackCommand) -> String {
    isolated_store();
    let mut out = Vec::new();
    if let Err(error) = pack::dispatch(root, command, &mut out) {
        panic!("pack command failed: {error:#}");
    }
    String::from_utf8(out).expect("utf-8 output")
}

/// Runs a pack command that must fail, and returns the error chain as `main`
/// prints it.
pub fn pack_err(root: &Path, command: PackCommand) -> String {
    isolated_store();
    match pack::dispatch(root, command, &mut Vec::new()) {
        Ok(_) => panic!("pack command succeeded"),
        Err(error) => format!("{error:#}"),
    }
}
