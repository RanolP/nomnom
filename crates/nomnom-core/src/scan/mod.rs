//! Facts about what is on disk. No judgement, no aggregation.
//!
//! A scan always covers a whole volume: [`scan_drive`] takes a [`VolumeRoot`],
//! which only a drive root such as `C:\` parses into.
//!
//! Two backends produce the same [`ScanTable`]:
//!
//! - [`backend::mft`] reads the NTFS Master File Table off the raw volume — the
//!   mechanism WizTree uses. It enumerates the whole volume in one sequential
//!   read and files each name under its parent's file reference, so it pays
//!   neither one `stat` per file nor one path per entry. It needs
//!   Administrator (a raw `\\.\C:` handle is
//!   privileged) and an NTFS volume.
//! - [`backend::walk`] walks the tree with the `ignore` crate. Portable, needs
//!   no privileges, and is the fallback whenever the MFT path is unavailable.
//!
//! Which one runs is not the caller's choice: [`scan_drive`] takes the MFT
//! whenever it can get it and falls back to walk, recording why in
//! [`ScanReport::backend_used`] so a front-end can tell the user they are on
//! the slow path and how to get off it.

pub mod backend;
mod drives;
pub mod elevated;
mod root;
pub mod table;

pub use drives::{Volume, volumes};
pub use elevated::{is_elevated, maybe_run_helper};
pub use root::VolumeRoot;
pub use table::{Blob, ScanTable};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
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

/// Which backend [`backend::dispatch`] runs. Internal: [`scan_drive`] owns the
/// choice so no front-end can offer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// MFT where possible, walk otherwise.
    Auto,
    /// Fail rather than silently fall back: the elevated helper's mode.
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

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Honour `.gitignore` and friends. Walk backend only; the MFT sees the
    /// volume, not the repo.
    pub respect_gitignore: bool,
    pub follow_symlinks: bool,
    /// Live counters a UI on another thread reads to show how far the scan
    /// has got. See [`ScanProgress`] for what each backend fills in.
    pub progress: Option<Arc<ScanProgress>>,
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
    /// Why the MFT read was given up, set the moment [`scan_drive`] falls back
    /// to the walk, so a front-end can say so while the slower scan runs.
    pub fallback: OnceLock<String>,
    /// Set when the elevated helper an earlier scan in this process started
    /// had exited, so this scan launches a new one behind a new UAC prompt.
    pub relaunch: OnceLock<String>,
    /// The [`Stage`] running now, as its discriminant.
    stage: AtomicU8,
    stage_done: AtomicU64,
    /// 0 means the stage has no count.
    stage_total: AtomicU64,
    /// Bit per [`Stage`] this run goes through; see [`ScanProgress::planned`].
    plan: u8,
    /// The highest [`ScanProgress::overall`] handed out, as f64 bits.
    shown: AtomicU64,
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

    /// Counters for a run that goes on past the scan through `stages`, so
    /// [`ScanProgress::overall`] spreads one bar over all of them. The scan's
    /// own stages, [`Stage::Scan`] and [`Stage::Table`], are always part of it.
    pub fn planned(stages: &[Stage]) -> Self {
        let scan = 1 << Stage::Scan as u8 | 1 << Stage::Table as u8;
        let plan = stages.iter().fold(scan, |plan, &s| plan | 1 << s as u8);
        Self { plan, ..Self::default() }
    }

    /// Moves the run on to `stage`, with `total` units of work in it; 0 means
    /// the stage has no count and shows as not yet started until it ends.
    pub fn enter(&self, stage: Stage, total: u64) {
        self.stage_done.store(0, Ordering::Relaxed);
        self.stage_total.store(total, Ordering::Relaxed);
        self.stage.store(stage as u8, Ordering::Release);
    }

    /// Units of the current stage done so far. Callers report in batches of
    /// [`Stage::BATCH`] or so: the reader only polls every 100 ms.
    pub fn advance_to(&self, done: u64) {
        self.stage_done.store(done, Ordering::Relaxed);
    }

    pub fn set_stage_total(&self, total: u64) {
        self.stage_total.store(total, Ordering::Relaxed);
    }

    pub fn stage(&self) -> Stage {
        Stage::ALL[usize::from(self.stage.load(Ordering::Acquire)).min(Stage::ALL.len() - 1)]
    }

    /// Completed fraction of the whole run in `0.0..=1.0`, or `None` while the
    /// scan has no numbers yet. Each planned stage owns a segment of the bar
    /// sized by `Stage::weight`; the value never goes down, even when a
    /// failed elevated scan restarts the count on the walk.
    pub fn overall(&self, used_bytes: u64) -> Option<f64> {
        let stage = self.stage();
        let within = if stage == Stage::Scan {
            self.fraction(used_bytes)?
        } else {
            let total = self.stage_total.load(Ordering::Relaxed);
            let done = self.stage_done.load(Ordering::Relaxed);
            if total == 0 { 0.0 } else { (done as f64 / total as f64).min(1.0) }
        };
        let walk = self.entries_total.load(Ordering::Relaxed) == 0;
        let planned =
            || Stage::ALL.into_iter().filter(|&s| self.plan & 1 << s as u8 != 0 || s == stage);
        let sum: f64 = planned().map(|s| s.weight(walk)).sum();
        let before: f64 = planned().filter(|&s| s < stage).map(|s| s.weight(walk)).sum();
        let value = ((before + stage.weight(walk) * within) / sum).clamp(0.0, 1.0);
        // Non-negative f64s order the same as their bit patterns.
        let shown = self.shown.fetch_max(value.to_bits(), Ordering::Relaxed);
        Some(value.max(f64::from_bits(shown)))
    }
}

/// One step of the work from the click to the judged result, in run order.
/// The scan's own counters live in [`ScanProgress`]'s scan fields; every later
/// stage reports through [`ScanProgress::enter`] and
/// [`ScanProgress::advance_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Stage {
    Scan,
    /// The walk's whole paths filed under their parents. The MFT does this
    /// while it reads, so a run on the MFT never enters it.
    Table,
    /// [`Catalog::build`](crate::catalog::Catalog::build): rows linked into a
    /// preorder tree.
    Link,
    /// `Catalog::build`: sizes and counts summed up the tree.
    RollUp,
    /// A front-end's own drive-wide views, the GUI's palette and allocations.
    Aggregate,
    /// Assessment: pack resolution and the name index.
    Index,
    /// Assessment: every rule run against the index.
    Match,
    /// Assessment: conflicts resolved, verdicts grouped and sorted.
    Group,
}

impl Stage {
    pub const ALL: [Stage; 8] = [
        Stage::Scan,
        Stage::Table,
        Stage::Link,
        Stage::RollUp,
        Stage::Aggregate,
        Stage::Index,
        Stage::Match,
        Stage::Group,
    ];

    /// Loop iterations between two progress stores in a per-node loop.
    pub const BATCH: usize = 1 << 16;

    /// What a front-end names the stage: the scan, the tree, or the analysis.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Scan => "Scanning",
            Stage::Table | Stage::Link | Stage::RollUp | Stage::Aggregate => "Building the tree",
            Stage::Index | Stage::Match | Stage::Group => "Analyzing",
        }
    }

    /// The stage's share of the bar, in milliseconds a whole-`C:` run spent in
    /// it on the release GUI's MFT path (scan 9.0 s, link 0.69 s, roll-up
    /// 0.18 s, aggregates 0.66 s, name index 0.23 s, rule match 0.04 s,
    /// resolve and group 0.07 s). The walk is weighted by the CLI's walk run
    /// instead: 77 s of walk, about 4 s of it filing paths into the table,
    /// against 4.2 s of build and assessment, scaled onto the same post-scan
    /// stages (1210 ms here).
    fn weight(self, walk: bool) -> f64 {
        match self {
            Stage::Scan if walk => 73.0 / 4.2 * 1210.0,
            Stage::Scan => 9000.0,
            Stage::Table if walk => 4.0 / 4.2 * 1210.0,
            Stage::Table => 0.0,
            Stage::Link => 690.0,
            Stage::RollUp => 180.0,
            Stage::Aggregate => 660.0,
            Stage::Index => 230.0,
            Stage::Match => 40.0,
            Stage::Group => 70.0,
        }
    }
}

/// [`ScanProgress::advance_to`] on an optional handle, for the build and
/// assessment loops that run with or without a front-end watching.
pub(crate) fn advance(progress: Option<&ScanProgress>, done: usize) {
    if let Some(progress) = progress {
        progress.advance_to(done as u64);
    }
}

/// Everything one scan produced.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub root: PathBuf,
    /// Every name found, filed under its parent by index; row 0 is `root`.
    pub table: ScanTable,
    pub errors: Vec<ScanError>,
    pub backend_used: BackendUsed,
}

impl ScanReport {
    /// A report from whole paths, the walk's shape: see
    /// [`ScanTable::from_entries`].
    pub fn from_entries(
        root: PathBuf,
        entries: Vec<Entry>,
        errors: Vec<ScanError>,
        backend_used: BackendUsed,
    ) -> Self {
        let table = ScanTable::from_entries(&root, entries);
        Self { root, table, errors, backend_used }
    }
}

/// Set to any value, [`scan_drive`] treats the elevated scan as failed without
/// launching it, so the walk fallback can be exercised with no UAC prompt to
/// decline.
pub const DEBUG_FAIL_ELEVATED_ENV: &str = "NOMNOM_GUI_FAIL_ELEVATED";

/// Scan the drive at `root`: the one scan every front-end runs.
///
/// The MFT read is always tried first on NTFS: in-process when already
/// elevated, otherwise through [`elevated::scan_elevated`], which raises one
/// UAC prompt. A declined or failed prompt walks the drive instead, sets
/// [`ScanProgress::fallback`] as it starts, and records the reason in
/// [`BackendUsed::Walk`].
///
/// Returns `Err` only when the scan could not start at all. Per-entry failures
/// land in [`ScanReport::errors`].
pub fn scan_drive(
    root: &VolumeRoot,
    progress: Option<Arc<ScanProgress>>,
) -> Result<ScanReport, ScanFailure> {
    let opts = ScanOptions { progress, ..ScanOptions::default() };
    let ntfs = volumes()
        .into_iter()
        .find(|volume| volume.root == root.as_path())
        .is_none_or(|volume| volume.fs.eq_ignore_ascii_case("NTFS"));
    if !ntfs || is_elevated() {
        return backend::dispatch(root.as_path(), Backend::Auto, &opts);
    }
    let elevated = if std::env::var_os(DEBUG_FAIL_ELEVATED_ENV).is_some() {
        Err(elevated::ElevatedScanError::Failed(format!("{DEBUG_FAIL_ELEVATED_ENV} is set")))
    } else {
        elevated::scan_elevated(root, &opts)
    };
    let reason = match elevated {
        Ok(report) => return Ok(report),
        Err(elevated::ElevatedScanError::Declined) => {
            "Administrator access was declined".to_string()
        }
        Err(elevated::ElevatedScanError::Failed(message)) => {
            format!("the elevated scan failed: {message}")
        }
    };
    if let Some(progress) = &opts.progress {
        // The helper may have counted part of the MFT before it stopped.
        for counter in [&progress.entries, &progress.entries_total, &progress.bytes] {
            counter.store(0, Ordering::Relaxed);
        }
        let _ = progress.fallback.set(reason.clone());
    }
    let mut report = backend::dispatch(root.as_path(), Backend::Walk, &opts)?;
    report.backend_used = BackendUsed::Walk { mft_unavailable: Some(reason) };
    Ok(report)
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
