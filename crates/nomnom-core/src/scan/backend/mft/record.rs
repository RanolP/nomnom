//! A minimal FILE-record parser: the handful of fields a size scan needs, read
//! straight out of the record bytes.
//!
//! The `ntfs` crate builds a file object per record, re-reads record 0 and
//! re-walks `$MFT`'s run list for every record it opens, and reads each
//! attribute value through a `Read + Seek`. On a table of millions of records
//! that bookkeeping, not the bytes, is the scan. This parser does the update
//! sequence fixup in place, walks the attribute headers once, and allocates
//! nothing per record: names are decoded onto the end of a caller's buffer.
//!
//! Layouts follow <https://flatcap.github.io/linux-ntfs/ntfs/>. Every read is
//! bounds-checked: a malformed record yields an error or a short attribute
//! walk, never a panic.

/// One `$FILE_NAME`: its parent and where its UTF-8 text sits in the names
/// buffer the record was parsed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub parent: u64,
    pub off: u32,
    pub len: u32,
    pub namespace: u8,
}

impl Link {
    /// Win32 and Win32+DOS names first, POSIX next, the DOS 8.3 alias last:
    /// the alias duplicates a long name that is also present, so it is only
    /// ever a fallback.
    pub fn rank(&self) -> u8 {
        match self.namespace {
            NAMESPACE_POSIX => 1,
            NAMESPACE_DOS => 2,
            _ => 0,
        }
    }
}

/// Facts one FILE record contributes. An extension record carries the same
/// kinds of facts and is merged into its base record after the pass.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Facts {
    pub is_dir: bool,
    /// `(modified, accessed)` in NT ticks, from `$STANDARD_INFORMATION`.
    pub times: Option<(u64, u64)>,
    /// This record's `$FILE_NAME`s: `links[links_start..links_start + links_len]`
    /// of the list it was parsed into, in on-disk order.
    pub links_start: u32,
    pub links_len: u32,
    /// Logical length of the unnamed `$DATA`, from its first piece.
    pub size: Option<u64>,
    /// Bytes of real clusters behind the unnamed `$DATA`, summed over every
    /// piece, sparse runs excluded.
    pub allocated: u64,
    pub reparse: Option<Reparse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reparse {
    Tag(u32),
    /// A non-resident reparse value: its tag is the first 4 bytes at this
    /// volume byte offset, read after the pass.
    At(u64),
    Unreadable,
}

/// What one record slot holds.
#[derive(Debug, PartialEq)]
pub enum Slot {
    /// Never used, or a deleted file whose slot has not been reused.
    Free,
    Corrupt(&'static str),
    Base {
        sequence: u16,
        facts: Facts,
    },
    Extension {
        base: u64,
        base_sequence: u16,
        facts: Facts,
    },
}

const IN_USE: u16 = 0x1;
const IS_DIR: u16 = 0x2;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const NAMESPACE_POSIX: u8 = 0;
const NAMESPACE_DOS: u8 = 2;
const REFERENCE_MASK: u64 = (1 << 48) - 1;

/// Parses one record in place (the fixup rewrites the sector tails). Its
/// names are appended to `names` and `links`, which the returned facts index.
pub fn parse(record: &mut [u8], cluster: u64, names: &mut String, links: &mut Vec<Link>) -> Slot {
    match &record.get(..4) {
        Some(b"FILE") => {}
        // A slot the table grew into but never formatted.
        Some([0, 0, 0, 0]) => return Slot::Free,
        _ => return Slot::Corrupt("bad FILE signature"),
    }
    if let Err(why) = fixup(record) {
        return Slot::Corrupt(why);
    }
    let flags = u16_at(record, 22).unwrap_or(0);
    if flags & IN_USE == 0 {
        return Slot::Free;
    }
    let sequence = u16_at(record, 16).unwrap_or(0);
    let base_ref = u64_at(record, 32).unwrap_or(0);
    let links_start = links.len() as u32;
    let mut facts = Facts { is_dir: flags & IS_DIR != 0, links_start, ..Facts::default() };
    for attr in Attributes::new(record) {
        collect(&mut facts, &attr, cluster, names, links);
    }
    facts.links_len = links.len() as u32 - links_start;
    match base_ref & REFERENCE_MASK {
        0 => Slot::Base { sequence, facts },
        base => Slot::Extension { base, base_sequence: (base_ref >> 48) as u16, facts },
    }
}

/// Folds an extension record's scalar facts into its base record's. Its names
/// stay where they are; the caller files them under the base.
pub fn merge(into: &mut Facts, ext: &Facts) {
    into.times = into.times.or(ext.times);
    into.size = into.size.or(ext.size);
    into.allocated = into.allocated.saturating_add(ext.allocated);
    into.reparse = into.reparse.or(ext.reparse);
}

fn collect(
    facts: &mut Facts,
    attr: &Attr<'_>,
    cluster: u64,
    names: &mut String,
    links: &mut Vec<Link>,
) {
    match attr.ty {
        ATTR_STANDARD_INFORMATION => {
            // 48 bytes is the shortest `$STANDARD_INFORMATION` NTFS writes.
            if let Some(v) = attr.resident_value().filter(|v| v.len() >= 48) {
                facts.times = Some((u64_at(v, 8).unwrap_or(0), u64_at(v, 24).unwrap_or(0)));
            }
        }
        ATTR_FILE_NAME => {
            let Some(v) = attr.resident_value() else { return };
            let (Some(parent), Some(&len), Some(&namespace)) = (u64_at(v, 0), v.get(64), v.get(65))
            else {
                return;
            };
            let Some(raw) = v.get(66..66 + 2 * len as usize) else { return };
            let units = raw.chunks_exact(2).map(|u| u16::from_le_bytes([u[0], u[1]]));
            // A buffer past 4 GiB cannot be indexed by a link; such a name
            // reads back empty rather than as some other name.
            let Ok(off) = u32::try_from(names.len()) else { return };
            names.extend(
                char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
            );
            let len = (names.len() - off as usize) as u32;
            links.push(Link { parent: parent & REFERENCE_MASK, off, len, namespace });
        }
        ATTR_DATA if attr.name_len == 0 => {
            if let Some(v) = attr.resident_value() {
                facts.size = Some(v.len() as u64);
            } else if let Some(nr) = attr.non_resident() {
                if nr.lowest_vcn == 0 {
                    facts.size = Some(nr.data_size);
                }
                let mut clusters = 0u64;
                for_each_run(nr.runs, |len, lcn| {
                    if lcn.is_some() {
                        clusters = clusters.saturating_add(len);
                    }
                });
                facts.allocated = facts.allocated.saturating_add(clusters.saturating_mul(cluster));
            }
        }
        ATTR_REPARSE_POINT => {
            facts.reparse = Some(if let Some(v) = attr.resident_value() {
                u32_at(v, 0).map_or(Reparse::Unreadable, Reparse::Tag)
            } else if let Some(nr) = attr.non_resident() {
                let mut first = None;
                for_each_run(nr.runs, |_, lcn| {
                    first.get_or_insert(lcn);
                });
                match first.flatten() {
                    Some(lcn) if lcn >= 0 && nr.data_size >= 4 => {
                        (lcn as u64).checked_mul(cluster).map_or(Reparse::Unreadable, Reparse::At)
                    }
                    _ => Reparse::Unreadable,
                }
            } else {
                Reparse::Unreadable
            });
        }
        _ => {}
    }
}

/// Applies the update sequence: every 512-byte stride ends in the sequence
/// number, and the real two bytes sit in the update sequence array.
pub fn fixup(record: &mut [u8]) -> Result<(), &'static str> {
    let (Some(offset), Some(count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return Err("record shorter than its header");
    };
    let (offset, count) = (offset as usize, count as usize);
    if count < 2 || offset + 2 * count > record.len() || !record.len().is_multiple_of(count - 1) {
        return Err("bad update sequence array");
    }
    let stride = record.len() / (count - 1);
    if stride < 2 || !stride.is_multiple_of(2) {
        return Err("bad update sequence array");
    }
    let usn = [record[offset], record[offset + 1]];
    for i in 1..count {
        let tail = i * stride - 2;
        if record[tail..tail + 2] != usn {
            return Err("torn write: update sequence mismatch");
        }
        let slot = offset + 2 * i;
        let original = [record[slot], record[slot + 1]];
        record[tail..tail + 2].copy_from_slice(&original);
    }
    Ok(())
}

/// One attribute's header and its bytes.
pub struct Attr<'a> {
    pub ty: u32,
    pub name_len: u8,
    bytes: &'a [u8],
}

pub struct NonResident<'a> {
    pub lowest_vcn: u64,
    pub data_size: u64,
    pub runs: &'a [u8],
}

impl<'a> Attr<'a> {
    pub fn resident_value(&self) -> Option<&'a [u8]> {
        if self.bytes.get(8) != Some(&0) {
            return None;
        }
        let len = u32_at(self.bytes, 16)? as usize;
        let at = u16_at(self.bytes, 20)? as usize;
        self.bytes.get(at..at.checked_add(len)?)
    }

    pub fn non_resident(&self) -> Option<NonResident<'a>> {
        if self.bytes.get(8) != Some(&1) {
            return None;
        }
        let runs_at = u16_at(self.bytes, 32)? as usize;
        Some(NonResident {
            lowest_vcn: u64_at(self.bytes, 16)?,
            data_size: u64_at(self.bytes, 48)?,
            runs: self.bytes.get(runs_at..)?,
        })
    }
}

/// The attributes of a fixed-up record, in on-disk order. Stops at the end
/// marker or at the first header that does not fit.
pub struct Attributes<'a> {
    record: &'a [u8],
    at: usize,
}

impl<'a> Attributes<'a> {
    pub fn new(record: &'a [u8]) -> Self {
        let used = u32_at(record, 24).map_or(0, |u| (u as usize).min(record.len()));
        let at = u16_at(record, 20).map_or(usize::MAX, usize::from);
        Self { record: &record[..used], at }
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Attr<'a>;

    fn next(&mut self) -> Option<Attr<'a>> {
        let ty = u32_at(self.record, self.at)?;
        if ty == ATTR_END {
            return None;
        }
        let len = u32_at(self.record, self.at.checked_add(4)?)? as usize;
        let bytes = (len >= 16).then(|| self.record.get(self.at..self.at + len)).flatten();
        let Some(bytes) = bytes else {
            self.at = usize::MAX;
            return None;
        };
        self.at += len;
        Some(Attr { ty, name_len: bytes[9], bytes })
    }
}

/// Calls `f(length in clusters, absolute LCN or None for sparse)` per run of
/// a mapping-pairs array. Stops quietly at a malformed run.
pub fn for_each_run(runs: &[u8], mut f: impl FnMut(u64, Option<i64>)) {
    let mut i = 0usize;
    let mut lcn = 0i64;
    while let Some(&header) = runs.get(i) {
        if header == 0 {
            return;
        }
        let (len_size, off_size) = ((header & 0xF) as usize, (header >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return;
        }
        i += 1;
        let Some(len) = runs.get(i..i + len_size) else { return };
        let len = len.iter().rev().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        i += len_size;
        let target = if off_size == 0 {
            None
        } else {
            let Some(delta) = runs.get(i..i + off_size) else { return };
            // Sign-extend from the top byte.
            let mut v = if delta[off_size - 1] & 0x80 != 0 { -1i64 } else { 0 };
            for &b in delta.iter().rev() {
                v = (v << 8) | i64::from(b);
            }
            i += off_size;
            let Some(next) = lcn.checked_add(v) else { return };
            lcn = next;
            Some(lcn)
        };
        f(len, target);
    }
}

pub fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

pub fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

pub fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}
