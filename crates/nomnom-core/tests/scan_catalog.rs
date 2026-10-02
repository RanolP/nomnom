//! Scan → catalog seam: aggregate roll-up, order independence, depth safety,
//! duplicate detection, error collection, and the drive-only scan policy.
//! Backend dispatch on real trees is tested inside the crate, beside it.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use common::report_of;
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{
    BackendUsed, Entry, EntryKind, ScanError, ScanFailure, ScanProgress, ScanReport, VolumeRoot,
};

fn file_entry(path: impl Into<PathBuf>, size: u64) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::File,
        size,
        allocated: None,
        modified: None,
        accessed: None,
    }
}

fn dir_entry(path: impl Into<PathBuf>) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::Dir,
        size: 0,
        allocated: None,
        modified: None,
        accessed: None,
    }
}

fn report(root: impl Into<PathBuf>, entries: Vec<Entry>) -> ScanReport {
    ScanReport {
        root: root.into(),
        entries,
        errors: Vec::new(),
        backend_used: BackendUsed::Walk { mft_unavailable: None },
    }
}

/// Catches aggregate roll-up drifting: subtree_size, file_count and dir_count
/// must be the exact byte and node counts of a fixture tree with known sizes.
#[test]
fn rollup_matches_known_fixture_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("a")).unwrap();
    std::fs::create_dir(root.join("a/inner")).unwrap();
    std::fs::create_dir(root.join("b")).unwrap();
    std::fs::write(root.join("top.bin"), vec![0u8; 100]).unwrap();
    std::fs::write(root.join("a/one.bin"), vec![0u8; 10]).unwrap();
    std::fs::write(root.join("a/inner/two.bin"), vec![0u8; 20]).unwrap();
    std::fs::write(root.join("a/inner/three.bin"), vec![0u8; 3]).unwrap();
    std::fs::write(root.join("b/four.bin"), vec![0u8; 7]).unwrap();

    let catalog = Catalog::build(report_of(root));

    let root_node = catalog.node(catalog.root());
    assert_eq!(root_node.subtree_size, 140);
    assert_eq!(root_node.file_count, 5);
    assert_eq!(root_node.dir_count, 3);

    let a = catalog.find(&root.join("a")).unwrap();
    let a_node = catalog.node(a);
    assert_eq!(a_node.subtree_size, 33);
    assert_eq!(a_node.file_count, 3);
    assert_eq!(a_node.dir_count, 1);
    assert_eq!(a_node.depth, 1);
    assert_eq!(catalog.path(a), root.join("a"));
}

/// Catches `build` silently depending on emission order. The MFT backend emits
/// in MFT-record order, where a child routinely precedes its parent, so a
/// shuffled feed must produce the identical tree as a sorted one.
#[test]
fn build_is_independent_of_entry_order() {
    let root = PathBuf::from("/r");
    let sorted = vec![
        dir_entry("/r"),
        dir_entry("/r/a"),
        dir_entry("/r/a/b"),
        file_entry("/r/a/b/deep.bin", 9),
        file_entry("/r/a/mid.bin", 4),
        dir_entry("/r/c"),
        file_entry("/r/c/leaf.bin", 1),
    ];
    // Children before their parents, dirs last.
    let shuffled = vec![
        file_entry("/r/c/leaf.bin", 1),
        file_entry("/r/a/b/deep.bin", 9),
        dir_entry("/r/a/b"),
        file_entry("/r/a/mid.bin", 4),
        dir_entry("/r/c"),
        dir_entry("/r/a"),
        dir_entry("/r"),
    ];

    let from_sorted = Catalog::build(report(&root, sorted));
    let from_shuffled = Catalog::build(report(&root, shuffled));

    assert_eq!(from_sorted.len(), from_shuffled.len());
    for path in ["/r", "/r/a", "/r/a/b", "/r/a/b/deep.bin", "/r/a/mid.bin", "/r/c", "/r/c/leaf.bin"]
    {
        let a = from_sorted.node(from_sorted.find(Path::new(path)).unwrap());
        let b = from_shuffled.node(from_shuffled.find(Path::new(path)).unwrap());
        assert_eq!(
            (a.subtree_size, a.file_count, a.dir_count, a.depth, a.children.len()),
            (b.subtree_size, b.file_count, b.dir_count, b.depth, b.children.len()),
            "{path} differs between sorted and shuffled builds"
        );
    }
    let root_node = from_shuffled.node(from_shuffled.root());
    assert_eq!((root_node.subtree_size, root_node.file_count, root_node.dir_count), (14, 3, 3));
}

/// Catches a recursive roll-up or a recursive traversal: a 2000-deep chain must
/// build and be walked without overflowing the stack.
#[test]
fn deep_chain_builds_without_stack_overflow() {
    const DEPTH: usize = 2000;
    let root = PathBuf::from("/deep");
    let mut entries = vec![dir_entry(root.clone())];
    let mut path = root.clone();
    for level in 0..DEPTH {
        path = path.join(format!("d{level}"));
        entries.push(dir_entry(path.clone()));
    }
    entries.push(file_entry(path.join("leaf.bin"), 5));
    entries.reverse();

    let catalog = Catalog::build(report(&root, entries));

    let root_node = catalog.node(catalog.root());
    assert_eq!(root_node.subtree_size, 5);
    assert_eq!(root_node.file_count, 1);
    assert_eq!(root_node.dir_count, DEPTH as u64);
    assert_eq!(catalog.descendants(catalog.root()).len(), DEPTH + 2);
    assert_eq!(catalog.node(catalog.find(&path).unwrap()).depth, DEPTH as u32);
}

/// Catches size-only matching being mistaken for duplicate detection: a third
/// file of the same size but different contents must not join the pair.
#[test]
fn duplicate_groups_require_matching_contents_not_just_size() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("one.bin"), b"identical contents").unwrap();
    std::fs::write(root.join("two.bin"), b"identical contents").unwrap();
    std::fs::write(root.join("three.bin"), b"DIFFERENT contentz").unwrap();
    assert_eq!(b"identical contents".len(), b"DIFFERENT contentz".len());

    let catalog = Catalog::build(report_of(root));
    let groups = catalog.duplicate_groups(1);

    assert_eq!(groups.len(), 1, "expected exactly one duplicate group, got {groups:?}");
    let mut paths: Vec<PathBuf> = groups[0].iter().map(|&id| catalog.path(id)).collect();
    paths.sort();
    assert_eq!(paths, vec![root.join("one.bin"), root.join("two.bin")]);
}

/// Catches a per-entry failure being promoted to a fatal one: errors must ride
/// along in the report while the readable part of the tree is still catalogued.
///
/// The stronger form — making a real directory unreadable — is not portable
/// here: on Windows it needs an `icacls` DENY whose effect depends on the
/// account and whose leftover ACL can block the tempdir cleanup, so this
/// asserts the same invariant on a report that carries errors.
#[test]
fn per_entry_errors_are_collected_not_propagated() {
    let root = PathBuf::from("/r");
    let mut scan_report = report(
        &root,
        vec![dir_entry("/r"), dir_entry("/r/locked"), file_entry("/r/readable.bin", 12)],
    );
    scan_report.errors.push(ScanError {
        path: Some(PathBuf::from("/r/locked")),
        message: "permission denied".into(),
    });

    let catalog = Catalog::build(scan_report);

    assert_eq!(catalog.errors().len(), 1);
    assert_eq!(catalog.errors()[0].path.as_deref(), Some(Path::new("/r/locked")));
    assert_eq!(catalog.node(catalog.root()).subtree_size, 12);
    assert!(catalog.find(Path::new("/r/readable.bin")).is_some());
}

/// Catches the drive-only policy being bypassed: the one public scan entry
/// point takes a `VolumeRoot`, and a folder must never parse into one.
#[test]
fn a_folder_is_not_a_volume_root() {
    let tmp = tempfile::tempdir().unwrap();
    match VolumeRoot::new(tmp.path()) {
        Err(ScanFailure::NotAVolumeRoot(path)) => assert_eq!(path, tmp.path()),
        other => panic!("a temp folder parsed as a scan root: {other:?}"),
    }
}

/// Catches the two front-ends drifting apart on the shown percentage: a known
/// total wins over bytes, and the byte estimate never claims completion.
#[test]
fn progress_fraction_prefers_the_record_total_and_caps_the_byte_estimate() {
    let progress = ScanProgress::default();
    assert_eq!(progress.fraction(1000), None, "nothing scanned yet");

    progress.bytes.store(2000, Ordering::Relaxed);
    assert_eq!(progress.fraction(1000), Some(0.99), "sparse overshoot must not read as done");
    assert_eq!(progress.fraction(0), None, "unknown used bytes");

    progress.entries_total.store(200, Ordering::Relaxed);
    progress.entries.store(50, Ordering::Relaxed);
    assert_eq!(progress.fraction(1000), Some(0.25));
}

/// Catches the drive picker coming up empty or garbled: the system drive is
/// always a fixed drive, so it must be listed with a real capacity.
#[cfg(windows)]
#[test]
fn volumes_lists_the_system_drive() {
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let volumes = nomnom_core::scan::volumes();
    let volume = volumes
        .iter()
        .find(|v| v.root == Path::new(&format!("{system}\\")))
        .unwrap_or_else(|| panic!("{system}\\ missing from {volumes:?}"));
    assert!(volume.total > 0 && volume.free <= volume.total, "{volume:?}");
    assert!(!volume.fs.is_empty(), "{volume:?}");
}
