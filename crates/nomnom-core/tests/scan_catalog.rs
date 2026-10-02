//! Scan → catalog seam: aggregate roll-up, order independence, depth safety,
//! duplicate detection, error collection, and backend dispatch policy.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nomnom_core::catalog::{Catalog, file_types, largest_files};
use nomnom_core::scan::{
    Backend, BackendUsed, Entry, EntryKind, ScanError, ScanFailure, ScanOptions, ScanReport, scan,
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

fn walk_opts() -> ScanOptions {
    ScanOptions { backend: Backend::Walk, ..ScanOptions::default() }
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

    let catalog = Catalog::build(scan(root, &walk_opts()).unwrap());

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

    let catalog = Catalog::build(scan(root, &walk_opts()).unwrap());
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

/// Catches Auto turning into a hard failure when MFT is unavailable — the most
/// damaging regression in this seam. Mft must fail loudly; Auto must fall back
/// to walk and say why.
#[test]
fn auto_falls_back_to_walk_while_mft_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f.bin"), b"x").unwrap();

    let explicit =
        scan(tmp.path(), &ScanOptions { backend: Backend::Mft, ..ScanOptions::default() });
    assert!(
        matches!(explicit, Err(ScanFailure::MftUnavailable(_))),
        "Backend::Mft must not fall back, got {explicit:?}"
    );

    let auto = scan(tmp.path(), &ScanOptions { backend: Backend::Auto, ..ScanOptions::default() })
        .expect("Auto must fall back to walk rather than fail");
    match auto.backend_used {
        BackendUsed::Walk { mft_unavailable: Some(_) } => {}
        other => panic!("Auto must report why MFT was skipped, got {other:?}"),
    }
    assert!(auto.entries.iter().any(|e| e.path == tmp.path().join("f.bin")));

    let requested =
        scan(tmp.path(), &walk_opts()).expect("an explicitly requested walk must succeed");
    assert_eq!(requested.backend_used, BackendUsed::Walk { mft_unavailable: None });
}

/// Catches an unwired progress counter, which would freeze the GUI's scan
/// indicator at 0 for the whole scan.
#[test]
fn progress_counter_counts_every_scanned_entry() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
    std::fs::write(tmp.path().join("sub/b.bin"), b"b").unwrap();

    let progress = Arc::new(AtomicU64::new(0));
    let opts = ScanOptions { progress: Some(progress.clone()), ..walk_opts() };
    let report = scan(tmp.path(), &opts).unwrap();

    assert_eq!(report.entries.len(), 4, "root, sub, a.bin, sub/b.bin");
    assert_eq!(progress.load(Ordering::Relaxed), report.entries.len() as u64);
}

/// Catches directories being counted as files (twice over, once as a node and
/// once through their children): the per-extension totals must add up to the
/// root's file bytes and file count exactly.
#[test]
fn file_types_add_up_to_the_root_totals() {
    let root = PathBuf::from("/r");
    let catalog = Catalog::build(report(
        &root,
        vec![
            dir_entry("/r"),
            dir_entry("/r/src.d"),
            file_entry("/r/src.d/main.RS", 30),
            file_entry("/r/src.d/lib.rs", 20),
            file_entry("/r/Makefile", 7),
            file_entry("/r/.gitignore", 3),
            file_entry("/r/photo.jpg", 100),
        ],
    ));

    let types = file_types(&catalog);
    let root_node = catalog.node(catalog.root());
    assert_eq!(types.iter().map(|t| t.bytes).sum::<u64>(), root_node.subtree_size);
    assert_eq!(types.iter().map(|t| t.count).sum::<u64>(), root_node.file_count);
    let summary: Vec<(&str, u64, u64)> =
        types.iter().map(|t| (t.ext.as_str(), t.bytes, t.count)).collect();
    assert_eq!(summary, vec![("jpg", 100, 1), ("rs", 50, 2), ("(none)", 10, 2)]);
}

/// Catches the bounded heap evicting the wrong end (keeping the smallest
/// files) or returning them unsorted.
#[test]
fn largest_files_keeps_the_biggest_in_descending_order() {
    let root = PathBuf::from("/r");
    let catalog = Catalog::build(report(
        &root,
        vec![
            dir_entry("/r"),
            file_entry("/r/a", 5),
            file_entry("/r/b", 50),
            file_entry("/r/c", 1),
            file_entry("/r/d", 20),
            dir_entry("/r/e"),
            file_entry("/r/e/f", 40),
        ],
    ));

    let names: Vec<PathBuf> =
        largest_files(&catalog, 3).into_iter().map(|id| catalog.path(id)).collect();
    assert_eq!(names, ["/r/b", "/r/e/f", "/r/d"].map(PathBuf::from));
    assert_eq!(largest_files(&catalog, 99).len(), 5, "n beyond the file count returns all files");
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
