//! The byte stream the elevated helper sends its parent over the pipe.
//!
//! ```text
//! stream   = "NNEH" version:u8 frame*
//! frame    = 1 entries:u64 total:u64 bytes:u64      progress, little-endian
//!          | 2 report                               the result; ends the stream
//!          | 3 text                                 the helper's failure; ends it
//! report   = path backend count:varint entry* count:varint error*
//! entry    = flags:u8 shared:varint len:varint unit{len} size:varint
//!            [allocated:varint] [modified:u64] [accessed:u64]
//! ```
//!
//! Paths travel as UTF-16 code units, exactly as `encode_wide` gives them, so
//! a name that is not valid Unicode survives the trip. Each entry's path
//! carries only what differs from the previous entry's: `shared` units are
//! kept from it and `len` new ones follow, which is what keeps millions of
//! entries under one directory cheap to send. Times are 100 ns ticks since
//! 1601, the resolution Windows keeps them in.
//!
//! The parent decodes this from another process, so every count and length is
//! checked against a hard limit before it is trusted, and a malformed or
//! truncated stream is an `InvalidData` / `UnexpectedEof` error, never a
//! panic or an unbounded allocation.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::scan::{BackendUsed, Entry, EntryKind, ScanError, ScanReport};

const MAGIC: &[u8; 4] = b"NNEH";
const VERSION: u8 = 1;

const TAG_PROGRESS: u8 = 1;
const TAG_REPORT: u8 = 2;
const TAG_FAILURE: u8 = 3;

const KIND_MASK: u8 = 0b0000_0011;
const HAS_ALLOCATED: u8 = 0b0000_0100;
const HAS_MODIFIED: u8 = 0b0000_1000;
const HAS_ACCESSED: u8 = 0b0001_0000;

/// Windows caps a path at 32,767 UTF-16 units; twice that leaves room for a
/// verbatim prefix without admitting a nonsense length.
const MAX_PATH_UNITS: u64 = 1 << 16;
const MAX_TEXT_BYTES: u64 = 1 << 20;
/// Far above the largest NTFS volumes nomnom meets (tens of millions of
/// records), far below what would exhaust memory before the stream ran dry.
const MAX_ENTRIES: u64 = 1 << 30;
const MAX_ERRORS: u64 = 1 << 26;
/// A claimed count only reserves this much up front; the rest grows as
/// entries actually arrive, so a lying count cannot allocate on its own.
const PREALLOC_CAP: u64 = 1 << 12;

/// Seconds from 1601-01-01 to 1970-01-01.
const UNIX_FROM_1601: Duration = Duration::from_secs(11_644_473_600);
const TICKS_PER_SEC: u64 = 10_000_000;

#[derive(Debug)]
pub(crate) enum Frame {
    Progress { entries: u64, total: u64, bytes: u64 },
    Report(ScanReport),
    Failure(String),
}

pub(crate) fn write_header(w: &mut impl Write) -> io::Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&[VERSION])
}

pub(crate) fn write_progress(
    w: &mut impl Write,
    entries: u64,
    total: u64,
    bytes: u64,
) -> io::Result<()> {
    let mut frame = [0u8; 25];
    frame[0] = TAG_PROGRESS;
    frame[1..9].copy_from_slice(&entries.to_le_bytes());
    frame[9..17].copy_from_slice(&total.to_le_bytes());
    frame[17..25].copy_from_slice(&bytes.to_le_bytes());
    w.write_all(&frame)
}

pub(crate) fn write_failure(w: &mut impl Write, message: &str) -> io::Result<()> {
    let mut buf = vec![TAG_FAILURE];
    put_text(&mut buf, message);
    w.write_all(&buf)
}

pub(crate) fn write_report(w: &mut impl Write, report: &ScanReport) -> io::Result<()> {
    let mut buf = vec![TAG_REPORT];
    put_units(&mut buf, &units(&report.root));
    match &report.backend_used {
        BackendUsed::Mft => buf.push(0),
        BackendUsed::Walk { mft_unavailable: None } => buf.push(1),
        BackendUsed::Walk { mft_unavailable: Some(reason) } => {
            buf.push(2);
            put_text(&mut buf, reason);
        }
    }
    put_varint(&mut buf, report.entries.len() as u64);
    w.write_all(&buf)?;

    // One scratch buffer, one write per entry: the encoder runs once per
    // record of the volume, so it allocates nothing it does not have to.
    let mut previous: Vec<u16> = Vec::new();
    let mut current: Vec<u16> = Vec::new();
    for entry in &report.entries {
        buf.clear();
        current.clear();
        current.extend(entry.path.as_os_str().encode_wide());
        let shared = previous.iter().zip(&current).take_while(|(a, b)| a == b).count();
        let modified = entry.modified.and_then(ticks);
        let accessed = entry.accessed.and_then(ticks);

        let mut flags = match entry.kind {
            EntryKind::File => 0,
            EntryKind::Dir => 1,
            EntryKind::Symlink => 2,
        };
        if entry.allocated.is_some() {
            flags |= HAS_ALLOCATED;
        }
        if modified.is_some() {
            flags |= HAS_MODIFIED;
        }
        if accessed.is_some() {
            flags |= HAS_ACCESSED;
        }
        buf.push(flags);
        put_varint(&mut buf, shared as u64);
        put_units(&mut buf, &current[shared..]);
        put_varint(&mut buf, entry.size);
        if let Some(allocated) = entry.allocated {
            put_varint(&mut buf, allocated);
        }
        for time in [modified, accessed].into_iter().flatten() {
            buf.extend_from_slice(&time.to_le_bytes());
        }
        w.write_all(&buf)?;
        std::mem::swap(&mut previous, &mut current);
    }

    buf.clear();
    put_varint(&mut buf, report.errors.len() as u64);
    for error in &report.errors {
        match &error.path {
            Some(path) => {
                buf.push(1);
                put_units(&mut buf, &units(path));
            }
            None => buf.push(0),
        }
        put_text(&mut buf, &error.message);
    }
    w.write_all(&buf)
}

pub(crate) fn read_header(r: &mut impl Read) -> io::Result<()> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    if &header[..4] != MAGIC {
        return Err(invalid(format!("not a nomnom helper stream (starts {:02x?})", &header[..4])));
    }
    if header[4] != VERSION {
        return Err(invalid(format!(
            "the helper speaks wire version {}, this build speaks {VERSION}",
            header[4]
        )));
    }
    Ok(())
}

pub(crate) fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    match read_u8(r)? {
        TAG_PROGRESS => {
            Ok(Frame::Progress { entries: read_u64(r)?, total: read_u64(r)?, bytes: read_u64(r)? })
        }
        TAG_REPORT => read_report(r).map(Frame::Report),
        TAG_FAILURE => read_text(r).map(Frame::Failure),
        tag => Err(invalid(format!("unknown frame tag {tag}"))),
    }
}

fn read_report(r: &mut impl Read) -> io::Result<ScanReport> {
    let root = path_from(&read_units(r, MAX_PATH_UNITS, "root path")?);
    let backend_used = match read_u8(r)? {
        0 => BackendUsed::Mft,
        1 => BackendUsed::Walk { mft_unavailable: None },
        2 => BackendUsed::Walk { mft_unavailable: Some(read_text(r)?) },
        tag => return Err(invalid(format!("unknown backend tag {tag}"))),
    };

    let count = read_count(r, MAX_ENTRIES, "entry count")?;
    let mut entries = Vec::with_capacity(count.min(PREALLOC_CAP) as usize);
    let mut path_units: Vec<u16> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    for index in 0..count {
        let flags = read_u8(r)?;
        if flags & !(KIND_MASK | HAS_ALLOCATED | HAS_MODIFIED | HAS_ACCESSED) != 0 {
            return Err(invalid(format!("entry {index}: unknown flag bits {flags:#04x}")));
        }
        let kind = match flags & KIND_MASK {
            0 => EntryKind::File,
            1 => EntryKind::Dir,
            2 => EntryKind::Symlink,
            other => return Err(invalid(format!("entry {index}: unknown kind {other}"))),
        };
        let shared = read_varint(r)?;
        if shared > path_units.len() as u64 {
            return Err(invalid(format!(
                "entry {index}: shares {shared} units with a {}-unit previous path",
                path_units.len()
            )));
        }
        path_units.truncate(shared as usize);
        // Into reused buffers: two allocations per entry were a fifth of the
        // decode.
        let len = read_count(r, MAX_PATH_UNITS - shared, "path suffix")? as usize;
        scratch.resize(len * 2, 0);
        r.read_exact(&mut scratch)?;
        path_units
            .extend(scratch.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])));

        let size = read_varint(r)?;
        let allocated = if flags & HAS_ALLOCATED != 0 { Some(read_varint(r)?) } else { None };
        let modified = if flags & HAS_MODIFIED != 0 { time_from(read_u64(r)?) } else { None };
        let accessed = if flags & HAS_ACCESSED != 0 { time_from(read_u64(r)?) } else { None };
        entries.push(Entry {
            path: path_from(&path_units),
            kind,
            size,
            allocated,
            modified,
            accessed,
        });
    }

    let count = read_count(r, MAX_ERRORS, "error count")?;
    let mut errors = Vec::with_capacity(count.min(PREALLOC_CAP) as usize);
    for index in 0..count {
        let path = match read_u8(r)? {
            0 => None,
            1 => Some(path_from(&read_units(r, MAX_PATH_UNITS, "error path")?)),
            other => return Err(invalid(format!("error {index}: bad path marker {other}"))),
        };
        errors.push(ScanError { path, message: read_text(r)? });
    }

    Ok(ScanReport { root, entries, errors, backend_used })
}

fn units(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

fn path_from(units: &[u16]) -> PathBuf {
    OsString::from_wide(units).into()
}

/// `None` for a time outside what 64-bit ticks since 1601 can hold, which no
/// filesystem nomnom reads produces.
fn ticks(time: SystemTime) -> Option<u64> {
    let since_1601 = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.checked_add(UNIX_FROM_1601)?,
        Err(before) => UNIX_FROM_1601.checked_sub(before.duration())?,
    };
    u64::try_from(since_1601.as_nanos() / 100).ok()
}

fn time_from(ticks: u64) -> Option<SystemTime> {
    let since_1601 = Duration::from_secs(ticks / TICKS_PER_SEC)
        + Duration::from_nanos(ticks % TICKS_PER_SEC * 100);
    match since_1601.checked_sub(UNIX_FROM_1601) {
        Some(after) => UNIX_EPOCH.checked_add(after),
        None => UNIX_EPOCH.checked_sub(UNIX_FROM_1601 - since_1601),
    }
}

fn put_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(value as u8 | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn put_units(buf: &mut Vec<u8>, units: &[u16]) {
    put_varint(buf, units.len() as u64);
    for unit in units {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
}

fn put_text(buf: &mut Vec<u8>, text: &str) {
    // A message longer than the decoder admits is cut at a char boundary
    // rather than making the whole stream undecodable.
    let mut end = text.len().min(MAX_TEXT_BYTES as usize);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    put_varint(buf, end as u64);
    buf.extend_from_slice(&text.as_bytes()[..end]);
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0u8; 1];
    r.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    r.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_varint(r: &mut impl Read) -> io::Result<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = read_u8(r)?;
        // The tenth byte holds bit 63 alone.
        if index == 9 && byte > 1 {
            return Err(invalid("varint overflows 64 bits"));
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("varint longer than 10 bytes"))
}

fn read_count(r: &mut impl Read, limit: u64, what: &str) -> io::Result<u64> {
    let count = read_varint(r)?;
    if count > limit {
        return Err(invalid(format!("{what} {count} exceeds the limit of {limit}")));
    }
    Ok(count)
}

fn read_units(r: &mut impl Read, limit: u64, what: &str) -> io::Result<Vec<u16>> {
    let len = read_count(r, limit, what)? as usize;
    let mut bytes = vec![0u8; len * 2];
    r.read_exact(&mut bytes)?;
    Ok(bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect())
}

fn read_text(r: &mut impl Read) -> io::Result<String> {
    let len = read_count(r, MAX_TEXT_BYTES, "text length")? as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|e| invalid(format!("text is not UTF-8: {e}")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn entry(path: PathBuf, kind: EntryKind, size: u64) -> Entry {
        Entry { path, kind, size, allocated: None, modified: None, accessed: None }
    }

    fn odd_report() -> ScanReport {
        let root = PathBuf::from("C:\\");
        // An unpaired surrogate: a legal NTFS name that is not Unicode, which a
        // UTF-8 round trip would replace with U+FFFD.
        let lone = OsString::from_wide(&[0x0062, 0xD800, 0x0063]);
        let deep = (0..300).fold(root.clone(), |path, i| path.join(format!("d{i}")));
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(1200);

        let mut full = entry(root.join("사진").join("😀 photo.jpg"), EntryKind::File, 7);
        full.allocated = Some(4096);
        full.modified = Some(at(1_700_000_000));
        full.accessed = Some(UNIX_EPOCH - Duration::from_secs(86_400 * 365 * 100));

        let mut report = ScanReport {
            root: root.clone(),
            entries: vec![
                entry(root.clone(), EntryKind::Dir, 0),
                entry(root.join("사진"), EntryKind::Dir, 0),
                full,
                // Shorter than its predecessor, sharing only part of it.
                entry(root.join("사"), EntryKind::Symlink, 0),
                entry(root.join(&lone), EntryKind::File, u64::MAX),
                entry(deep.join("leaf.bin"), EntryKind::File, 1 << 40),
                // Shares nothing with the deep path before it.
                entry(PathBuf::from("D:\\elsewhere"), EntryKind::File, 0),
            ],
            errors: vec![
                ScanError { path: Some(root.join(&lone)), message: "접근 거부".into() },
                ScanError { path: None, message: String::new() },
            ],
            backend_used: BackendUsed::Walk { mft_unavailable: Some("needs Administrator".into()) },
        };
        report.entries[5].allocated = Some(0);
        report
    }

    fn encode(report: &ScanReport) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_header(&mut bytes).unwrap();
        write_progress(&mut bytes, 3, 9, 27).unwrap();
        write_report(&mut bytes, report).unwrap();
        bytes
    }

    fn decode(mut bytes: &[u8]) -> io::Result<(Vec<Frame>, usize)> {
        let total = bytes.len();
        read_header(&mut bytes)?;
        let mut frames = Vec::new();
        loop {
            let frame = read_frame(&mut bytes)?;
            let last = !matches!(frame, Frame::Progress { .. });
            frames.push(frame);
            if last {
                return Ok((frames, total - bytes.len()));
            }
        }
    }

    /// Catches a lossy encoder: a non-ASCII name, a lone surrogate, a path
    /// longer than MAX_PATH, a path shorter than the one before it, and every
    /// optional field must come back exactly as they went in.
    #[test]
    fn odd_paths_and_every_field_survive_the_round_trip() {
        let report = odd_report();
        let bytes = encode(&report);
        let (frames, used) = decode(&bytes).expect("a well-formed stream decodes");
        assert_eq!(used, bytes.len(), "the report frame must end the stream exactly");

        let [Frame::Progress { entries: 3, total: 9, bytes: 27 }, Frame::Report(back)] =
            frames.as_slice()
        else {
            panic!("unexpected frames {frames:?}");
        };
        assert_eq!(back.root, report.root);
        assert_eq!(back.backend_used, report.backend_used);
        assert_eq!(back.entries, report.entries);
        let errors = |r: &ScanReport| -> Vec<(Option<PathBuf>, String)> {
            r.errors.iter().map(|e| (e.path.clone(), e.message.clone())).collect()
        };
        assert_eq!(errors(back), errors(&report));

        let mut failure = Vec::new();
        write_header(&mut failure).unwrap();
        write_failure(&mut failure, "not a volume root: C:\\Users").unwrap();
        match decode(&failure).unwrap().0.as_slice() {
            [Frame::Failure(message)] => assert_eq!(message, "not a volume root: C:\\Users"),
            other => panic!("unexpected frames {other:?}"),
        }
    }

    /// Catches the parent trusting the pipe: every truncation of a valid
    /// stream, and every single-byte corruption of it, must decode to an error
    /// or a value — never a panic or a runaway allocation.
    #[test]
    fn truncated_or_corrupted_streams_are_errors_not_panics() {
        let bytes = encode(&odd_report());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "a {len}-byte prefix decoded");
        }
        for at in 0..bytes.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut corrupt = bytes.clone();
                corrupt[at] ^= flip;
                let _ = decode(&corrupt);
            }
        }

        let mut hostile = Vec::new();
        write_header(&mut hostile).unwrap();
        hostile.push(TAG_REPORT);
        put_units(&mut hostile, &units(Path::new("C:\\")));
        hostile.push(0);
        put_varint(&mut hostile, MAX_ENTRIES);
        // One entry claiming to share 5 units with a path that does not exist.
        hostile.extend([0, 5, 0, 0]);
        let error = decode(&hostile).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");

        let mut huge = Vec::new();
        write_header(&mut huge).unwrap();
        huge.push(TAG_FAILURE);
        put_varint(&mut huge, u64::MAX);
        assert_eq!(decode(&huge).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// The encoder runs inside the elevated scan, so it must stay a small
    /// fraction of the MFT read. Run with
    /// `cargo test -p nomnom-core --release wire -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing measurement, not a check"]
    fn encode_and_decode_time_at_volume_scale() {
        // NOMNOM_TIMINGS=1 prints the catalog's phases as well.
        crate::timings::join_scan("bench");
        let mut report = volume_scale_report(4_500_000);
        let n = report.entries.len();

        // As the helper does before it sends.
        let started = Instant::now();
        crate::catalog::sort_subtrees(&mut report.entries);
        let sorted = started.elapsed();
        let started = Instant::now();
        let mut bytes = Vec::new();
        write_report(&mut bytes, &report).unwrap();
        let encoded = started.elapsed();
        let started = Instant::now();
        let mut slice = bytes.as_slice();
        let Frame::Report(back) = read_frame(&mut slice).unwrap() else { panic!() };
        let decoded = started.elapsed();
        assert_eq!(back.entries.len(), n);
        let started = Instant::now();
        let catalog = crate::catalog::Catalog::build(back);
        let built = started.elapsed();
        assert!(catalog.len() > n / 2);
        println!(
            "{n} entries: helper sort {sorted:?}, {} MiB on the wire, encode {encoded:?}, \
             decode {decoded:?}, Catalog::build {built:?}",
            bytes.len() >> 20
        );
    }

    /// A C:-shaped report: about one directory per ten files, depth up to a
    /// dozen, in creation order the way MFT record order is: each entry lands
    /// in one of the 2000 most recent directories, so neighbours share some
    /// of their path but the whole is far from sorted.
    fn volume_scale_report(n: usize) -> ScanReport {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let root = PathBuf::from("C:\\");
        let now = SystemTime::now();
        let mut dirs: Vec<(PathBuf, u32)> = vec![(root.clone(), 0)];
        let mut entries = Vec::with_capacity(n);
        let exts = ["dll", "exe", "txt", "json", "png", "js", "pyc", "dat", "Manifest", "mui"];
        for i in 0..n {
            let r = next();
            // Parents favour recent directories, which is what gives depth.
            let span = dirs.len().min(2000) as u64;
            let (parent, depth) = dirs[dirs.len() - 1 - (r % span) as usize].clone();
            let entry = if i % 10 == 0 && depth < 12 {
                let path = parent.join(format!("Folder_{:x}", r >> 40));
                dirs.push((path.clone(), depth + 1));
                entry(path, EntryKind::Dir, 0)
            } else {
                let ext = exts[(r >> 8) as usize % exts.len()];
                entry(
                    parent.join(format!("file-{i}-{:x}.{ext}", r >> 50)),
                    EntryKind::File,
                    r >> 44,
                )
            };
            entries.push(Entry {
                allocated: Some(entry.size.next_multiple_of(4096)),
                modified: Some(now),
                accessed: Some(now),
                ..entry
            });
        }
        ScanReport { root, entries, errors: Vec::new(), backend_used: BackendUsed::Mft }
    }
}
