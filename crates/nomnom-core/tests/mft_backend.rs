//! Tests for the NTFS MFT backend.
//!
//! Every test here runs on an ordinary unelevated machine. The parts that need
//! a raw volume handle — the enumeration itself — cannot be proven without
//! Administrator, so what is pinned instead is everything that decides whether
//! the enumeration would be correct: the alignment arithmetic under the reader,
//! and the parent-chasing loop that rebuilds paths. The spelling of the paths
//! that come out and the failure reported when the volume is closed to us run
//! a folder through the dispatcher, so they live in the crate's own tests
//! (`scan::backend::tests`), out of reach of the drive-only public scan.

// The backend module only exists on Windows, which is also the only platform
// that has an MFT to read.
#![cfg(windows)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nomnom_core::scan::backend::mft::paths::{DirRecord, PathBuilder, ROOT_RECORD, respell_under};
use nomnom_core::scan::backend::mft::{strip_verbatim, volume};
use nomnom_core::scan::elevated::scan_elevated;
use nomnom_core::scan::{BackendUsed, ScanOptions, VolumeRoot};
use volume::{AlignedReader, FileSource};

// ---------------------------------------------------------------------------
// Sector-aligned reader
// ---------------------------------------------------------------------------

/// Regression: the alignment arithmetic being off by a sector or a block, which
/// would hand `ntfs` shifted bytes and corrupt every record it parses. Proven
/// against an ordinary file, where the correct answer is just `fs::read`.
#[test]
fn aligned_reader_matches_a_plain_file_read_at_unaligned_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.bin");

    // A deterministic, position-dependent pattern: a shifted read cannot
    // accidentally match it.
    let truth: Vec<u8> =
        (0..40_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    fs::File::create(&path).unwrap().write_all(&truth).unwrap();

    let cases = [
        (0u64, 1usize),
        (1, 1),
        (511, 2),
        (512, 512),
        (513, 1023),
        (1000, 5000),
        (4095, 2),
        (4096, 4096),
        (39_000, 999),
        (0, 40_000),
    ];

    for (sector, block) in [(512u64, 4096usize), (512, 512), (4096, 1 << 16)] {
        for (offset, len) in cases {
            let file = fs::File::open(&path).unwrap();
            let mut reader = AlignedReader::new(FileSource::new(file), sector, block);
            reader.seek(SeekFrom::Start(offset)).unwrap();
            let mut got = vec![0u8; len];
            reader.read_exact(&mut got).unwrap();
            let want = &truth[offset as usize..offset as usize + len];
            assert_eq!(got, want, "sector={sector} block={block} offset={offset} len={len}");
            assert_eq!(reader.stream_position().unwrap(), offset + len as u64);
        }
    }
}

/// Regression: a read that runs off the end of the device looping forever or
/// reporting bytes it never read.
#[test]
fn aligned_reader_stops_at_the_end_of_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short.bin");
    fs::write(&path, vec![7u8; 600]).unwrap();

    let file = fs::File::open(&path).unwrap();
    let mut reader = AlignedReader::new(FileSource::new(file), 512, 512);
    reader.seek(SeekFrom::Start(590)).unwrap();

    let mut got = vec![0u8; 100];
    let n = reader.read(&mut got).unwrap();
    assert_eq!(n, 10);
    assert_eq!(&got[..n], &[7u8; 10]);
    assert_eq!(reader.read(&mut got).unwrap(), 0);
}

/// Regression: interleaved seeks re-reading a stale cached block, which is the
/// failure mode the MFT's back-and-forth between `$MFT` and a record would hit.
#[test]
fn aligned_reader_survives_seeking_backwards_between_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.bin");
    let truth: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &truth).unwrap();

    let file = fs::File::open(&path).unwrap();
    let mut reader = AlignedReader::new(FileSource::new(file), 512, 4096);

    for offset in [0u64, 65_000, 3, 40_000, 4, 69_990] {
        reader.seek(SeekFrom::Start(offset)).unwrap();
        let len = (truth.len() as u64 - offset).min(9) as usize;
        let mut got = vec![0u8; len];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, &truth[offset as usize..offset as usize + len], "offset={offset}");
    }
}

// ---------------------------------------------------------------------------
// Path reconstruction
// ---------------------------------------------------------------------------

fn dir_map(entries: &[(u64, &str, u64)]) -> HashMap<u64, DirRecord> {
    entries
        .iter()
        .map(|(number, name, parent)| {
            (*number, DirRecord { name: (*name).to_string(), parent: *parent })
        })
        .collect()
}

/// Regression: the parent-chasing loop losing a level, hanging on a cycle, or
/// inventing a path for a record whose parent is not in the table.
#[test]
fn parent_chasing_resolves_deep_chains_and_refuses_broken_ones() {
    let dirs = dir_map(&[
        (20, "Users", ROOT_RECORD),
        (21, "ranolp", 20),
        (22, "Projects", 21),
        (23, "nomnom", 22),
        // Parent 900 is not in the table: a record whose chain dead-ends.
        (40, "orphan", 900),
        // A two-record cycle, which a naive loop would follow forever.
        (50, "loop-a", 51),
        (51, "loop-b", 50),
    ]);
    let mut builder = PathBuilder::new(&dirs, PathBuf::from("C:\\"), ROOT_RECORD);

    assert_eq!(builder.dir_path(ROOT_RECORD), Some(PathBuf::from("C:\\")));
    assert_eq!(builder.dir_path(20), Some(PathBuf::from("C:\\Users")));
    assert_eq!(builder.dir_path(23), Some(PathBuf::from("C:\\Users\\ranolp\\Projects\\nomnom")));
    assert_eq!(
        builder.child_path(23, "Cargo.toml"),
        Some(PathBuf::from("C:\\Users\\ranolp\\Projects\\nomnom\\Cargo.toml"))
    );

    assert_eq!(builder.dir_path(40), None, "a missing parent must not produce a path");
    assert_eq!(builder.child_path(40, "x.txt"), None);
    assert_eq!(builder.dir_path(50), None, "a cycle must terminate, not hang");
}

/// Regression: the directory cache being bypassed, turning path reconstruction
/// quadratic — the exact cost the MFT backend exists to avoid.
#[test]
fn resolved_directories_are_cached_once_each() {
    let dirs = dir_map(&[(20, "a", ROOT_RECORD), (21, "b", 20), (22, "c", 21)]);
    let mut builder = PathBuilder::new(&dirs, PathBuf::from("C:\\"), ROOT_RECORD);

    for i in 0..1000 {
        assert!(builder.child_path(22, &format!("file{i}.bin")).is_some());
    }
    // Three directories walked, three cache entries — not one per file.
    assert_eq!(builder.cached_len(), 3);
}

/// Regression: emitting the extended-length `\\?\C:\...` spelling that
/// `fs::canonicalize` returns. `Catalog::build` re-attaches children to parents
/// by exact path prefix and does no normalisation, so a stray `\\?\` on one
/// entry orphans it and every aggregate above it goes wrong.
#[test]
fn canonical_roots_are_stripped_back_to_plain_drive_paths() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(dir.path()).unwrap();
    assert!(
        canonical.as_os_str().to_string_lossy().starts_with("\\\\?\\"),
        "precondition: canonicalize returns the verbatim form"
    );

    let plain = strip_verbatim(&canonical).unwrap();
    let text = plain.as_os_str().to_string_lossy().into_owned();
    assert!(!text.starts_with("\\\\?\\"), "verbatim prefix survived: {text}");
    assert_eq!(plain, dir.path());
}

/// Regression: the subtree filter admitting a sibling whose path merely starts
/// with the same characters, or rejecting the scan root itself.
#[test]
fn subtree_filter_keeps_the_root_and_rejects_near_misses() {
    let root_canon = Path::new("C:\\Users\\ranolp\\Projects");
    let spelling = Path::new("C:\\Users\\ranolp\\projects");

    assert_eq!(
        respell_under(root_canon, root_canon, spelling),
        Some(spelling.to_path_buf()),
        "the scan root must be emitted, as the walk backend emits it"
    );
    assert_eq!(
        respell_under(Path::new("C:\\Users\\ranolp\\PROJECTS\\nomnom"), root_canon, spelling),
        Some(spelling.join("nomnom")),
        "NTFS is case-insensitive, but the caller's spelling of the root must win"
    );
    assert_eq!(
        respell_under(Path::new("C:\\Users\\ranolp\\Projects2\\x"), root_canon, spelling),
        None,
        "a sibling sharing a character prefix is not inside the root"
    );
    assert_eq!(respell_under(Path::new("C:\\Users\\ranolp"), root_canon, spelling), None);
    assert_eq!(respell_under(Path::new("D:\\Users\\ranolp\\Projects"), root_canon, spelling), None);
}

// ---------------------------------------------------------------------------
// Needs elevation
// ---------------------------------------------------------------------------

/// Needs an elevated process: a raw `\\.\C:` handle is denied with error 5
/// otherwise. Run with:
///
/// ```text
/// cargo test -p nomnom-core --test mft_backend -- --ignored
/// ```
///
/// Regression: the real enumeration producing no entries, or producing paths
/// that do not exist on disk.
#[test]
#[ignore = "needs an elevated process for a raw volume handle"]
fn real_volume_enumeration_produces_paths_that_exist() {
    let root = VolumeRoot::new(std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()))
        .expect("the system drive is a volume root");
    let report = scan_elevated(&root, &ScanOptions::default()).expect("the MFT read succeeds");
    let root = root.as_path();

    assert_eq!(report.backend_used, BackendUsed::Mft);
    assert!(!report.entries.is_empty());
    for entry in report.entries.iter().take(200) {
        assert!(entry.path.starts_with(root), "{} escaped the root", entry.path.display());
        assert!(entry.path.symlink_metadata().is_ok(), "{} does not exist", entry.path.display());
    }
}
