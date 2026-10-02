//! The byte stream the elevated helper sends its parent over the pipe.
//!
//! ```text
//! stream   = "NNEH" version:u8 frame*
//! frame    = 1 entries:u64 total:u64 bytes:u64      progress, little-endian
//!          | 2 report                               the result; ends the stream
//!          | 3 text                                 the helper's failure; ends it
//! report   = path backend names odd_names blobs nodes errors
//! names    = len:varint utf8{len}
//! odd      = count:varint (units)*
//! blobs    = count:varint (size:u64 allocated:u64 modified:u64 accessed:u64 has:u8)*
//! nodes    = count:varint (parent:u32 name_off:u32 name_len:u32 blob:u32 kind_flags:u8)*
//! errors   = count:varint (0 | 1 units) text)*
//! ```
//!
//! The report is the scan's [`ScanTable`] as it lies in memory: fixed-size
//! little-endian rows and one names buffer, in the order the backend produced
//! them, so encoding is a copy and nothing is sorted or rebuilt on either side.
//! Odd names and error paths travel as UTF-16 code units, exactly as
//! `encode_wide` gives them, so a name that is not valid Unicode survives the
//! trip. Times are 100 ns ticks since 1601, the resolution Windows keeps them
//! in; `has` says which of `allocated`, `modified` and `accessed` are present.
//!
//! The parent decodes this from another process, so every count and length is
//! checked against a hard limit before it is trusted, every index a row holds
//! (parent, name range, blob) is checked against what came before it, and a
//! malformed or truncated stream is an `InvalidData` / `UnexpectedEof` error,
//! never a panic or an unbounded allocation. Rows are only bounds-checked: a
//! cycle or an orphan among valid indices is the catalog's to drop.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::scan::table::{Blob, EXTRA_LINK, NO_BLOB, Name, ODD_NAME, ScanTable};
use crate::scan::{BackendUsed, EntryKind, ScanError, ScanReport};

const MAGIC: &[u8; 4] = b"NNEH";
const VERSION: u8 = 2;

const TAG_PROGRESS: u8 = 1;
const TAG_REPORT: u8 = 2;
const TAG_FAILURE: u8 = 3;

const KIND_MASK: u8 = 0b0000_0011;
const FLAGS_SHIFT: u8 = 2;
const KNOWN_FLAGS: u8 = ODD_NAME | EXTRA_LINK;

const HAS_ALLOCATED: u8 = 0b001;
const HAS_MODIFIED: u8 = 0b010;
const HAS_ACCESSED: u8 = 0b100;

const NODE_BYTES: usize = 17;
const BLOB_BYTES: usize = 33;

/// Windows caps a path at 32,767 UTF-16 units; twice that leaves room for a
/// verbatim prefix without admitting a nonsense length.
const MAX_PATH_UNITS: u64 = 1 << 16;
const MAX_TEXT_BYTES: u64 = 1 << 20;
/// Far above the largest NTFS volumes nomnom meets (tens of millions of
/// records), far below what would exhaust memory before the stream ran dry.
const MAX_ROWS: u64 = 1 << 30;
const MAX_ERRORS: u64 = 1 << 26;
/// Rows index the names buffer with `u32`.
const MAX_NAMES_BYTES: u64 = u32::MAX as u64;
/// A claimed count only reserves this much up front; the rest grows as rows
/// actually arrive, so a lying count cannot allocate on its own.
const PREALLOC_CAP: u64 = 1 << 12;
/// Rows encoded or decoded per pipe read or write.
const ROWS_PER_CHUNK: usize = 1 << 15;

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

fn kind_code(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::File => 0,
        EntryKind::Dir => 1,
        EntryKind::Symlink => 2,
    }
}

pub(crate) fn write_report(w: &mut impl Write, report: &ScanReport) -> io::Result<()> {
    let table = &report.table;
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
    put_varint(&mut buf, table.names.len() as u64);
    w.write_all(&buf)?;
    w.write_all(table.names.as_bytes())?;

    buf.clear();
    put_varint(&mut buf, table.odd_names.len() as u64);
    for name in &table.odd_names {
        put_units(&mut buf, &name.encode_wide().collect::<Vec<u16>>());
    }
    put_varint(&mut buf, table.blobs.len() as u64);
    w.write_all(&buf)?;
    write_rows(w, &table.blobs, BLOB_BYTES, |blob, out| {
        let modified = blob.modified.and_then(ticks);
        let accessed = blob.accessed.and_then(ticks);
        let mut has = 0;
        if blob.allocated.is_some() {
            has |= HAS_ALLOCATED;
        }
        if modified.is_some() {
            has |= HAS_MODIFIED;
        }
        if accessed.is_some() {
            has |= HAS_ACCESSED;
        }
        out.extend_from_slice(&blob.size.to_le_bytes());
        out.extend_from_slice(&blob.allocated.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&modified.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&accessed.unwrap_or(0).to_le_bytes());
        out.push(has);
    })?;

    buf.clear();
    put_varint(&mut buf, table.nodes.len() as u64);
    w.write_all(&buf)?;
    write_rows(w, &table.nodes, NODE_BYTES, |node, out| {
        out.extend_from_slice(&node.parent.to_le_bytes());
        out.extend_from_slice(&node.name_off.to_le_bytes());
        out.extend_from_slice(&node.name_len.to_le_bytes());
        out.extend_from_slice(&node.blob.to_le_bytes());
        out.push(kind_code(node.kind) | (node.flags << FLAGS_SHIFT));
    })?;

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

/// Writes fixed-size rows a chunk at a time through one reused buffer.
fn write_rows<T>(
    w: &mut impl Write,
    rows: &[T],
    size: usize,
    put: impl Fn(&T, &mut Vec<u8>),
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(size * ROWS_PER_CHUNK.min(rows.len()));
    for chunk in rows.chunks(ROWS_PER_CHUNK) {
        buf.clear();
        for row in chunk {
            put(row, &mut buf);
        }
        w.write_all(&buf)?;
    }
    Ok(())
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

    let len = read_count(r, MAX_NAMES_BYTES, "names length")?;
    // `take` + `read_to_end` grows with the bytes that actually arrive, so a
    // lying length cannot allocate on its own.
    let mut bytes = Vec::new();
    r.by_ref().take(len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "names buffer cut short"));
    }
    let names = String::from_utf8(bytes).map_err(|e| invalid(format!("names: {e}")))?;

    let count = read_count(r, MAX_ROWS, "odd name count")?;
    let mut odd_names = Vec::with_capacity(count.min(PREALLOC_CAP) as usize);
    for _ in 0..count {
        odd_names.push(OsString::from_wide(&read_units(r, MAX_PATH_UNITS, "odd name")?));
    }

    let count = read_count(r, MAX_ROWS, "blob count")?;
    let blobs = read_rows(r, count, BLOB_BYTES, |row, index| {
        let u = |at: usize| u64::from_le_bytes(row[at..at + 8].try_into().unwrap_or_default());
        let has = row[32];
        if has & !(HAS_ALLOCATED | HAS_MODIFIED | HAS_ACCESSED) != 0 {
            return Err(invalid(format!("blob {index}: unknown flag bits {has:#04x}")));
        }
        Ok(Blob {
            size: u(0),
            allocated: (has & HAS_ALLOCATED != 0).then(|| u(8)),
            modified: if has & HAS_MODIFIED != 0 { time_from(u(16)) } else { None },
            accessed: if has & HAS_ACCESSED != 0 { time_from(u(24)) } else { None },
        })
    })?;

    let count = read_count(r, MAX_ROWS, "node count")?;
    if count == 0 {
        return Err(invalid("a table without its root row"));
    }
    let nodes = read_rows(r, count, NODE_BYTES, |row, index| {
        let u = |at: usize| u32::from_le_bytes(row[at..at + 4].try_into().unwrap_or_default());
        let (parent, name_off, name_len, blob, code) = (u(0), u(4), u(8), u(12), row[16]);
        let kind = match code & KIND_MASK {
            0 => EntryKind::File,
            1 => EntryKind::Dir,
            2 => EntryKind::Symlink,
            other => return Err(invalid(format!("node {index}: unknown kind {other}"))),
        };
        let flags = code >> FLAGS_SHIFT;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(invalid(format!("node {index}: unknown flag bits {flags:#04x}")));
        }
        if u64::from(parent) >= count {
            return Err(invalid(format!("node {index}: parent {parent} past {count} rows")));
        }
        let named = if flags & ODD_NAME != 0 {
            (name_off as usize) < odd_names.len()
        } else {
            (name_off as usize).checked_add(name_len as usize).is_some_and(|end| end <= names.len())
        };
        if !named {
            return Err(invalid(format!("node {index}: name {name_off}+{name_len} out of range")));
        }
        if blob != NO_BLOB && blob as usize >= blobs.len() {
            return Err(invalid(format!("node {index}: blob {blob} past {} blobs", blobs.len())));
        }
        Ok(Name { parent, name_off, name_len, blob, kind, flags })
    })?;

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

    let table = ScanTable { nodes, names, odd_names, blobs };
    Ok(ScanReport { root, table, errors, backend_used })
}

/// Reads `count` fixed-size rows a chunk at a time, growing the result only
/// as rows arrive.
fn read_rows<T>(
    r: &mut impl Read,
    count: u64,
    size: usize,
    mut parse: impl FnMut(&[u8], u64) -> io::Result<T>,
) -> io::Result<Vec<T>> {
    let mut out = Vec::with_capacity(count.min(PREALLOC_CAP) as usize);
    let mut scratch = vec![0u8; size * (ROWS_PER_CHUNK as u64).min(count) as usize];
    let mut index = 0u64;
    while index < count {
        let n = (count - index).min(ROWS_PER_CHUNK as u64) as usize;
        let chunk = &mut scratch[..n * size];
        r.read_exact(chunk)?;
        out.reserve(n);
        for row in chunk.chunks_exact(size) {
            out.push(parse(row, index)?);
            index += 1;
        }
    }
    Ok(out)
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
    use crate::scan::Entry;

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

        let mut entries = vec![
            entry(root.clone(), EntryKind::Dir, 0),
            entry(root.join("사진"), EntryKind::Dir, 0),
            full,
            entry(root.join("사"), EntryKind::Symlink, 0),
            entry(root.join(&lone), EntryKind::File, u64::MAX),
            entry(deep.join("leaf.bin"), EntryKind::File, 1 << 40),
        ];
        entries[5].allocated = Some(0);
        let mut table = ScanTable::from_entries(&root, entries);
        // A second name of the photo, sharing its blob.
        let photo = table.nodes[2].blob;
        table.push_node(0, "photo-link.jpg", photo, EntryKind::File, EXTRA_LINK);
        ScanReport {
            root: root.clone(),
            table,
            errors: vec![
                ScanError { path: Some(root.join(&lone)), message: "접근 거부".into() },
                ScanError { path: None, message: String::new() },
            ],
            backend_used: BackendUsed::Walk { mft_unavailable: Some("needs Administrator".into()) },
        }
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
    /// longer than MAX_PATH, an extra hard-link name, and every optional field
    /// must come back exactly as they went in.
    #[test]
    fn odd_names_and_every_field_survive_the_round_trip() {
        let report = odd_report();
        assert!(!report.table.odd_names.is_empty(), "the lone surrogate is an odd name");
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
        assert_eq!(back.table, report.table);
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
    /// or a value — never a panic or a runaway allocation — and a row pointing
    /// past the table must be refused before anything indexes with it.
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

        for hostile in [
            |t: &mut ScanTable| t.nodes[1].parent = 99,
            |t: &mut ScanTable| t.nodes[1].name_off = u32::MAX,
            |t: &mut ScanTable| t.nodes[1].blob = 99,
        ] {
            let mut report = odd_report();
            hostile(&mut report.table);
            let error = decode(&encode(&report)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        }

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
        let report = volume_scale_report(4_500_000);
        let n = report.table.nodes.len();

        let started = Instant::now();
        let mut bytes = Vec::new();
        write_report(&mut bytes, &report).unwrap();
        let encoded = started.elapsed();
        let started = Instant::now();
        let mut slice = bytes.as_slice();
        let Frame::Report(back) = read_frame(&mut slice).unwrap() else { panic!() };
        let decoded = started.elapsed();
        assert_eq!(back.table.nodes.len(), n);
        let started = Instant::now();
        let catalog = crate::catalog::Catalog::build(back);
        let built = started.elapsed();
        assert!(catalog.len() > n / 2);
        println!(
            "{n} rows: {} MiB on the wire, encode {encoded:?}, decode {decoded:?}, \
             Catalog::build {built:?}",
            bytes.len() >> 20
        );
    }

    /// A C:-shaped report: about one directory per ten files, depth up to a
    /// dozen, in creation order the way MFT record order is.
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
        ScanReport::from_entries(root, entries, Vec::new(), BackendUsed::Mft)
    }
}
