//! Backend dispatch, kept apart from [`crate::scan::scan_drive`]'s elevation
//! policy so the fallback rules are testable on their own.

pub mod walk;

#[cfg(windows)]
pub mod mft;

use std::path::Path;

use crate::scan::{Backend, BackendUsed, ScanFailure, ScanOptions, ScanReport};

pub(crate) fn dispatch(
    root: &Path,
    backend: Backend,
    opts: &ScanOptions,
) -> Result<ScanReport, ScanFailure> {
    match backend {
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
    let started = std::time::Instant::now();
    let mut report = walk::scan(root, opts)?;
    crate::timings::lap("walk scan", started);
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

/// Dispatch on real trees. The public [`crate::scan::scan_drive`] only takes a
/// whole volume, so these run the root-agnostic dispatcher on small temp trees
/// from inside the crate.
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::scan::ScanProgress;

    fn run(root: &Path, backend: Backend) -> Result<ScanReport, ScanFailure> {
        dispatch(root, backend, &ScanOptions::default())
    }

    /// Catches Auto turning into a hard failure when MFT is unavailable — the
    /// most damaging regression in this seam. Mft must fail loudly with a reason
    /// that names the obstacle; Auto must fall back to walk and say why.
    #[test]
    fn auto_falls_back_to_walk_while_mft_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.bin"), b"x").unwrap();

        match run(tmp.path(), Backend::Mft) {
            // Elevated: the backend really ran, and says so.
            Ok(report) => assert_eq!(report.backend_used, BackendUsed::Mft),
            Err(ScanFailure::MftUnavailable(reason)) => assert!(
                reason.contains("volume") || reason.contains("NTFS") || reason.contains("MFT"),
                "reason does not name the obstacle: {reason}"
            ),
            Err(other) => panic!("Backend::Mft must not fall back, got {other:?}"),
        }

        let auto =
            run(tmp.path(), Backend::Auto).expect("Auto must fall back to walk rather than fail");
        match &auto.backend_used {
            BackendUsed::Mft => {}
            BackendUsed::Walk { mft_unavailable: Some(reason) } => assert!(!reason.is_empty()),
            other => panic!("Auto must report why MFT was skipped, got {other:?}"),
        }
        assert!(auto.table.paths(&auto.root).contains(&Some(auto.root.join("f.bin"))));

        let requested =
            run(tmp.path(), Backend::Walk).expect("an explicitly requested walk must succeed");
        assert_eq!(requested.backend_used, BackendUsed::Walk { mft_unavailable: None });
    }

    /// Catches a missing root reported as "MFT unavailable", which would send
    /// Auto off to re-walk a path that does not exist.
    #[test]
    fn missing_root_is_root_unreadable_not_mft_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-directory");
        for backend in [Backend::Mft, Backend::Auto] {
            match run(&missing, backend) {
                Err(ScanFailure::RootUnreadable(path)) => assert_eq!(path, missing),
                other => panic!("{backend:?}: expected RootUnreadable, got {other:?}"),
            }
        }
    }

    /// Catches unwired progress counters, which would freeze the front-ends'
    /// scan indicator at 0: walk must count every entry and add every file's
    /// bytes, and leave the total at 0 (unknown) rather than invent one.
    #[test]
    fn walk_progress_counts_every_entry_and_its_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"aaa").unwrap();
        std::fs::write(tmp.path().join("sub/b.bin"), b"bb").unwrap();

        let progress = Arc::new(ScanProgress::default());
        let opts = ScanOptions { progress: Some(progress.clone()), ..ScanOptions::default() };
        let report = dispatch(tmp.path(), Backend::Walk, &opts).unwrap();

        assert_eq!(report.table.nodes.len(), 4, "root, sub, a.bin, sub/b.bin");
        assert_eq!(progress.entries.load(Ordering::Relaxed), report.table.nodes.len() as u64);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 5);
        assert_eq!(progress.entries_total.load(Ordering::Relaxed), 0);
    }
}
