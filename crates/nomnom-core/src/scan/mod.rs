//! Facts about what is on disk. No judgement, no aggregation.
//!
//! Two backends produce the same [`Entry`] stream:
//!
//! - [`backend::mft`] reads the NTFS Master File Table off the raw volume — the
//!   mechanism WizTree uses. It enumerates the whole volume in one sequential
//!   read and reconstructs paths from parent file references, so it does not
//!   pay one `stat` per file. It needs Administrator (a raw `\\.\C:` handle is
//!   privileged) and an NTFS volume.
//! - [`backend::walk`] walks the tree with the `ignore` crate. Portable, needs
//!   no privileges, and is the fallback whenever the MFT path is unavailable.
//!
//! [`Backend::Auto`] tries MFT and falls back to walk, recording why in
//! [`ScanReport::backend_used`] so the CLI can tell the user they are on the
//! slow path and how to get off it.

pub mod backend;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// What a single filesystem entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One entry's facts, as reported by a backend. Sizes are in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// Logical size: what the file claims to be. Zero for directories.
    pub size: u64,
    /// Size actually occupied on disk, which differs from `size` for sparse and
    /// compressed files and for anything smaller than a cluster. `None` when the
    /// backend cannot cheaply answer.
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Often `None` on Windows: last-access updates are disabled by default.
    pub accessed: Option<SystemTime>,
}

/// A per-entry failure. Never fatal — a cleanup tool that dies on one
/// unreadable directory is useless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanError {
    pub path: Option<PathBuf>,
    pub message: String,
}

/// Which backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Backend {
    /// MFT where possible, walk otherwise.
    #[default]
    Auto,
    /// Fail rather than silently fall back — for benchmarking and for proving
    /// the MFT path actually engaged.
    Mft,
    Walk,
}

/// Which backend actually ran, and why that one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendUsed {
    Mft,
    /// Carries the reason the MFT path was not taken, so the CLI can say
    /// "run elevated for a faster scan" rather than just being slow.
    Walk {
        mft_unavailable: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub backend: Backend,
    /// Honour `.gitignore` and friends. Walk backend only; the MFT sees the
    /// volume, not the repo.
    pub respect_gitignore: bool,
    pub follow_symlinks: bool,
    /// Bumped once per entry as the scan runs, so a UI on another thread can
    /// show the scan is alive. The walk backend counts every visited entry,
    /// unreadable ones included. The MFT backend counts in-use records of the
    /// whole volume while reading the table, which outnumbers the entries of a
    /// scan rooted below the volume root.
    pub progress: Option<Arc<AtomicU64>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            respect_gitignore: false,
            follow_symlinks: false,
            progress: None,
        }
    }
}

impl ScanOptions {
    pub(crate) fn tick(&self) {
        if let Some(progress) = &self.progress {
            progress.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Everything one scan produced.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub errors: Vec<ScanError>,
    pub backend_used: BackendUsed,
}

/// Scan `root`, honouring `opts`.
///
/// Returns `Err` only when the scan could not start at all (root missing, or
/// [`Backend::Mft`] demanded and unavailable). Per-entry failures land in
/// [`ScanReport::errors`].
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    backend::dispatch(root, opts)
}

#[derive(Debug, thiserror::Error)]
pub enum ScanFailure {
    #[error("scan root {0} does not exist or is not readable")]
    RootUnreadable(PathBuf),
    #[error("MFT backend unavailable: {0}")]
    MftUnavailable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
