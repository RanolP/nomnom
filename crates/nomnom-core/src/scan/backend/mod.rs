//! Backend selection. The policy lives here so [`crate::scan::scan`] stays a
//! one-liner and the fallback rules are testable on their own.

pub mod walk;

#[cfg(windows)]
pub mod mft;

use std::path::Path;

use crate::scan::{Backend, BackendUsed, ScanFailure, ScanOptions, ScanReport};

pub(crate) fn dispatch(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    match opts.backend {
        Backend::Walk => walk_with(root, opts, None),
        Backend::Mft => mft_scan(root, opts),
        Backend::Auto => match mft_scan(root, opts) {
            Ok(report) => Ok(report),
            // Only unavailability falls back: a root that does not exist is the
            // same answer on either backend, so re-walking it just wastes time.
            Err(ScanFailure::MftUnavailable(reason)) => walk_with(root, opts, Some(reason)),
            Err(other) => Err(other),
        },
    }
}

fn walk_with(
    root: &Path,
    opts: &ScanOptions,
    mft_unavailable: Option<String>,
) -> Result<ScanReport, ScanFailure> {
    let mut report = walk::scan(root, opts)?;
    report.backend_used = BackendUsed::Walk { mft_unavailable };
    Ok(report)
}

#[cfg(windows)]
fn mft_scan(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    mft::scan(root, opts)
}

#[cfg(not(windows))]
fn mft_scan(_root: &Path, _opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    Err(ScanFailure::MftUnavailable("not a Windows target".into()))
}
