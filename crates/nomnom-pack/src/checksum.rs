//! A content checksum over a pack directory.
//!
//! The lock records this so that a pack changing under a fixed commit is
//! detectable. That makes determinism the whole requirement: the same files
//! must hash the same on Windows and on Linux, across two checkouts of the
//! same commit, and on any readdir order. So the walk is sorted by relative
//! path with forward slashes, and every path and every length goes into the
//! hash with its byte length in front of it, which keeps two different trees
//! from serialising to the same byte stream.
//!
//! `.git` is excluded: it is the checkout's bookkeeping, not the pack's
//! content, and it differs between a shallow fetch and a full clone of the
//! same commit.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// `blake3:<64 hex>` over every file under `dir`.
pub fn of_dir(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for relative in &files {
        let bytes = relative.as_bytes();
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
        let contents =
            fs::read(dir.join(relative)).map_err(|source| Error::io(dir.join(relative), source))?;
        hasher.update(&(contents.len() as u64).to_le_bytes());
        hasher.update(&contents);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|source| Error::io(dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(dir, source))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| Error::io(&path, source))?;
        if kind.is_dir() {
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            collect(root, &path, out)?;
        } else if kind.is_file() {
            out.push(relative(root, &path));
        }
        // A symlink inside a pack is neither content nor something to follow.
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    let tail: PathBuf = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    tail.to_string_lossy().replace('\\', "/")
}
