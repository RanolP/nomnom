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
    use std::path::PathBuf;
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
        assert!(auto.entries.iter().any(|e| e.path == tmp.path().join("f.bin")));

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

        assert_eq!(report.entries.len(), 4, "root, sub, a.bin, sub/b.bin");
        assert_eq!(progress.entries.load(Ordering::Relaxed), report.entries.len() as u64);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 5);
        assert_eq!(progress.entries_total.load(Ordering::Relaxed), 0);
    }

    /// Catches the MFT backend spelling a path even slightly differently from
    /// the walk backend — a separator, a case change, a prefix — which fragments
    /// the catalog into orphans that all re-attach to the root.
    #[cfg(windows)]
    #[test]
    fn reconstructed_paths_are_spelled_exactly_as_the_walk_backend_spells_them() {
        use std::collections::{HashMap, HashSet};

        use mft::paths::{DirRecord, PathBuilder, ROOT_RECORD, respell_under};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("Alpha").join("Beta")).unwrap();
        std::fs::write(root.join("two.txt"), b"..").unwrap();
        std::fs::write(root.join("Alpha").join("one.txt"), b".").unwrap();
        std::fs::write(root.join("Alpha").join("Beta").join("deep.txt"), b"...").unwrap();

        let walked: HashSet<PathBuf> =
            run(&root, Backend::Walk).unwrap().entries.into_iter().map(|e| e.path).collect();

        // The same tree as the MFT would hand it over: names and parent
        // references, with the temp directory standing in for the volume root.
        let dirs: HashMap<u64, DirRecord> = [(20, "Alpha", ROOT_RECORD), (21, "Beta", 20)]
            .into_iter()
            .map(|(n, name, parent)| (n, DirRecord { name: name.into(), parent }))
            .collect();
        let mut builder = PathBuilder::new(&dirs, root.clone(), ROOT_RECORD);

        let mut rebuilt = HashSet::new();
        rebuilt.insert(respell_under(&root, &root, &root).expect("the root is its own subtree"));
        for record in [20u64, 21] {
            let full = builder.dir_path(record).unwrap();
            rebuilt.insert(respell_under(&full, &root, &root).unwrap());
        }
        for (parent, name) in [(ROOT_RECORD, "two.txt"), (20, "one.txt"), (21, "deep.txt")] {
            let full = builder.child_path(parent, name).unwrap();
            rebuilt.insert(respell_under(&full, &root, &root).unwrap());
        }

        assert_eq!(rebuilt, walked, "MFT-style paths differ from the walk backend's paths");
    }
}
