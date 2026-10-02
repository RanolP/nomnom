//! Facts about what is on disk. No judgement, no aggregation.
//!
//! A scan always covers a whole volume: [`scan`] takes a [`VolumeRoot`], which
//! only a drive root such as `C:\` parses into.
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
mod drives;
mod root;

pub use drives::{Volume, volumes};
pub use root::VolumeRoot;

use std::path::PathBuf;
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
    /// Live counters a UI on another thread reads to show how far the scan
    /// has got. See [`ScanProgress`] for what each backend fills in.
    pub progress: Option<Arc<ScanProgress>>,
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
            progress.entries.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn add_bytes(&self, bytes: u64) {
        if let Some(progress) = &self.progress {
            progress.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn set_entries_total(&self, total: u64) {
        if let Some(progress) = &self.progress {
            progress.entries_total.store(total, Ordering::Relaxed);
        }
    }
}

/// How far a running scan has got, written by the scanning thread and read by
/// anyone holding the `Arc`.
///
/// The two backends know different things up front, so they fill different
/// fields:
/// - MFT knows the table's record count before reading it, so it sets
///   `entries_total` first and then counts every record it processes in
///   `entries`: `entries / entries_total` is an honest fraction.
/// - Walk cannot know how many entries a tree holds until it has walked it, so
///   `entries_total` stays 0 (unknown). It counts visited entries in `entries`
///   and adds each file's logical size to `bytes`, which against the volume's
///   used bytes gives an estimate.
///
/// [`ScanProgress::fraction`] applies that rule, so every front-end shows the
/// same number.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub entries: AtomicU64,
    /// 0 means unknown.
    pub entries_total: AtomicU64,
    pub bytes: AtomicU64,
}

impl ScanProgress {
    /// Completed fraction in `0.0..=1.0`, or `None` when nothing supports an
    /// estimate yet. `used_bytes` is the scanned volume's total minus free.
    ///
    /// The byte estimate is capped below 1: logical sizes overshoot used bytes
    /// for sparse and compressed files, and "100%" while still scanning is a
    /// lie the record count never tells.
    pub fn fraction(&self, used_bytes: u64) -> Option<f64> {
        let total = self.entries_total.load(Ordering::Relaxed);
        if total > 0 {
            let done = self.entries.load(Ordering::Relaxed);
            return Some((done as f64 / total as f64).min(1.0));
        }
        let bytes = self.bytes.load(Ordering::Relaxed);
        if bytes == 0 || used_bytes == 0 {
            return None;
        }
        Some((bytes as f64 / used_bytes as f64).min(0.99))
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

/// Scan the volume at `root`, honouring `opts`.
///
/// Returns `Err` only when the scan could not start at all (root missing, or
/// [`Backend::Mft`] demanded and unavailable). Per-entry failures land in
/// [`ScanReport::errors`].
pub fn scan(root: &VolumeRoot, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    backend::dispatch(root.as_path(), opts)
}

#[derive(Debug, thiserror::Error)]
pub enum ScanFailure {
    #[error("{0} is not a volume root; nomnom scans whole drives only, e.g. C:\\")]
    NotAVolumeRoot(PathBuf),
    #[error("scan root {0} does not exist or is not readable")]
    RootUnreadable(PathBuf),
    #[error("MFT backend unavailable: {0}")]
    MftUnavailable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
