//! Where the table sits on the volume, read off `$MFT`'s own `$DATA`.
//!
//! The pass reads the table in large sequential device reads, one contiguous
//! [`Extent`] at a time, and parses the bytes it gets with the hand-rolled
//! [`super::record`] parser. Nothing here goes through the `ntfs` crate's
//! per-file objects: `Ntfs::file` re-reads record 0 and re-walks the whole
//! `$MFT` run list for every record it opens, and it cannot follow an `$MFT`
//! whose run list spilled into an attribute list — which a multi-gigabyte,
//! long-lived table is exactly the kind to need.

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use super::record::{
    self, ATTR_ATTRIBUTE_LIST, ATTR_DATA, Attributes, for_each_run, u16_at, u64_at,
};
use super::volume::{AlignedReader, BlockSource};

/// Largest `$ATTRIBUTE_LIST` value read for `$MFT`. Real ones are a few KiB;
/// anything near this is corrupt, and must not become an allocation of
/// whatever length the record claims.
const MAX_ATTRIBUTE_LIST: u64 = 16 << 20;

const REFERENCE_MASK: u64 = (1 << 48) - 1;

/// Volume facts the boot sector gives, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub record_size: u64,
    pub cluster: u64,
    pub sector: u64,
    /// Volume byte offset of record 0.
    pub mft_pos: u64,
}

/// Where each part of the table sits on the volume.
pub struct MftLayout {
    record_size: u64,
    /// Total length of the table in bytes, `$DATA`'s logical length.
    len: u64,
    /// In VCN order, so `vbyte` is increasing.
    runs: Vec<Run>,
}

#[derive(Debug, Clone, Copy)]
struct Run {
    /// Offset of the run's first byte within the table.
    vbyte: u64,
    /// Volume byte offset, `None` for a sparse run.
    phys: Option<u64>,
    len: u64,
}

/// Records `first..first + count` sit back to back on the volume from `phys`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub first: u64,
    pub count: u64,
    pub phys: u64,
}

impl MftLayout {
    /// Reads `$MFT`'s run list, following its attribute list into extension
    /// records when it has one. Errors carry the wording the scan reports.
    pub fn read<S: BlockSource>(fs: &mut AlignedReader<S>, geo: &Geometry) -> Result<Self, String> {
        let rs = geo.record_size;
        if rs < 512 || !rs.is_power_of_two() || geo.cluster == 0 {
            return Err(format!(
                "boot sector reports a {rs}-byte record and a {}-byte cluster",
                geo.cluster
            ));
        }
        let record0 = read_fixed(fs, geo.mft_pos, rs)
            .map_err(|why| format!("reading $MFT (record 0) failed: {why}"))?;

        let mut pieces: Vec<Piece> = Vec::new();
        let mut len = None;
        let list = data_pieces(&record0, &mut pieces, &mut len);

        if let Some(list) = list {
            let list = attribute_list_value(fs, list, geo)?;
            let mut pending: Vec<u64> = list_data_records(&list);
            pending.sort_unstable();
            pending.dedup();
            pending.retain(|&n| n != 0);
            // An extension record of `$MFT` is located through the runs read
            // so far; one that sits in a part not yet known waits a round.
            while !pending.is_empty() {
                let runs = runs_of(&mut pieces.clone(), geo.cluster);
                let before = pending.len();
                let mut waiting = Vec::new();
                for number in pending {
                    let Some(phys) = number.checked_mul(rs).and_then(|v| phys_at(&runs, v)) else {
                        waiting.push(number);
                        continue;
                    };
                    let ext = read_fixed(fs, phys, rs).map_err(|why| {
                        format!("reading $MFT extension record {number} failed: {why}")
                    })?;
                    let _ = data_pieces(&ext, &mut pieces, &mut len);
                }
                if waiting.len() == before {
                    return Err(format!(
                        "$MFT extension record {} lies outside every run of the table found so far",
                        waiting[0]
                    ));
                }
                pending = waiting;
            }
        }

        let len = len.ok_or_else(|| "$MFT has no unnamed $DATA attribute".to_string())?;
        let runs = runs_of(&mut pieces, geo.cluster);
        Ok(Self { record_size: rs, len, runs })
    }

    pub fn record_count(&self) -> u64 {
        self.len / self.record_size
    }

    /// Bytes of table a full pass has to read.
    pub fn bytes(&self) -> u64 {
        self.len
    }

    /// The contiguous record ranges from `from` on, plus the record ranges no
    /// extent holds (a sparse run, a record straddling two runs, a run list
    /// shorter than the table), which the pass reports as errors.
    pub fn extents(&self, from: u64) -> (Vec<Extent>, Vec<Range<u64>>) {
        let rs = self.record_size;
        let total = self.record_count();
        let mut extents = Vec::new();
        for run in &self.runs {
            let Some(phys) = run.phys else { continue };
            let first = run.vbyte.div_ceil(rs).max(from);
            let end = (run.vbyte.saturating_add(run.len) / rs).min(total);
            if first < end {
                extents.push(Extent {
                    first,
                    count: end - first,
                    phys: phys + (first * rs - run.vbyte),
                });
            }
        }
        extents.sort_by_key(|e| e.first);

        let mut gaps = Vec::new();
        let mut cursor = from;
        for e in &extents {
            if e.first > cursor {
                gaps.push(cursor..e.first);
            }
            cursor = cursor.max(e.first + e.count);
        }
        if cursor < total {
            gaps.push(cursor..total);
        }
        (extents, gaps)
    }
}

/// One `$DATA` attribute of `$MFT`: where in the table it starts, and its runs.
#[derive(Clone)]
struct Piece {
    lowest_vcn: u64,
    runs: Vec<(u64, Option<i64>)>,
}

/// Adds every unnamed `$DATA` piece of a `$MFT` record to `pieces`, takes the
/// table length from the first piece, and returns the attribute-list
/// attribute's bytes when the record has one.
fn data_pieces(record: &[u8], pieces: &mut Vec<Piece>, len: &mut Option<u64>) -> Option<ListValue> {
    let mut list = None;
    for attr in Attributes::new(record) {
        match attr.ty {
            ATTR_DATA if attr.name_len == 0 => {
                let Some(nr) = attr.non_resident() else { continue };
                if nr.lowest_vcn == 0 {
                    *len = Some(nr.data_size);
                }
                let mut runs = Vec::new();
                for_each_run(nr.runs, |n, lcn| runs.push((n, lcn)));
                pieces.push(Piece { lowest_vcn: nr.lowest_vcn, runs });
            }
            ATTR_ATTRIBUTE_LIST => {
                list = Some(match (attr.resident_value(), attr.non_resident()) {
                    (Some(v), _) => ListValue::Resident(v.to_vec()),
                    (None, Some(nr)) => {
                        let mut runs = Vec::new();
                        for_each_run(nr.runs, |n, lcn| runs.push((n, lcn)));
                        ListValue::NonResident { runs, len: nr.data_size }
                    }
                    (None, None) => continue,
                });
            }
            _ => {}
        }
    }
    list
}

enum ListValue {
    Resident(Vec<u8>),
    NonResident { runs: Vec<(u64, Option<i64>)>, len: u64 },
}

/// The attribute list's value bytes, reading them off the volume when the
/// list is non-resident.
fn attribute_list_value<S: BlockSource>(
    fs: &mut AlignedReader<S>,
    list: ListValue,
    geo: &Geometry,
) -> Result<Vec<u8>, String> {
    let (runs, len) = match list {
        ListValue::Resident(bytes) => return Ok(bytes),
        ListValue::NonResident { runs, len } => (runs, len),
    };
    if len > MAX_ATTRIBUTE_LIST {
        return Err(format!("$MFT attribute list claims {len} bytes"));
    }
    let mut out = Vec::with_capacity(len as usize);
    for (clusters, lcn) in runs {
        let bytes = clusters.saturating_mul(geo.cluster).min(len - out.len() as u64) as usize;
        match lcn.filter(|&l| l >= 0) {
            None => out.resize(out.len() + bytes, 0),
            Some(lcn) => {
                let mut buf = vec![0; bytes];
                fs.seek(SeekFrom::Start((lcn as u64).saturating_mul(geo.cluster)))
                    .and_then(|_| fs.read_exact(&mut buf))
                    .map_err(|err| format!("reading the $MFT attribute list failed: {err}"))?;
                out.extend(buf);
            }
        }
        if out.len() as u64 >= len {
            break;
        }
    }
    Ok(out)
}

/// Record numbers the attribute list files an unnamed `$DATA` piece under.
fn list_data_records(list: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let (Some(ty), Some(entry_len)) = (record::u32_at(list, at), u16_at(list, at + 4)) {
        if entry_len < 26 {
            break;
        }
        if ty == ATTR_DATA
            && list.get(at + 6) == Some(&0)
            && let Some(reference) = u64_at(list, at + 16)
        {
            out.push(reference & REFERENCE_MASK);
        }
        at += entry_len as usize;
    }
    out
}

fn runs_of(pieces: &mut [Piece], cluster: u64) -> Vec<Run> {
    pieces.sort_by_key(|p| p.lowest_vcn);
    let mut runs = Vec::new();
    for piece in pieces.iter() {
        let mut vcn = piece.lowest_vcn;
        for &(clusters, lcn) in &piece.runs {
            let (Some(vbyte), Some(len)) =
                (vcn.checked_mul(cluster), clusters.checked_mul(cluster))
            else {
                break;
            };
            let phys = lcn.filter(|&l| l >= 0).and_then(|l| (l as u64).checked_mul(cluster));
            runs.push(Run { vbyte, phys, len });
            vcn = vcn.saturating_add(clusters);
        }
    }
    runs
}

fn phys_at(runs: &[Run], vbyte: u64) -> Option<u64> {
    let run = runs.iter().find(|r| vbyte >= r.vbyte && vbyte - r.vbyte < r.len)?;
    Some(run.phys? + (vbyte - run.vbyte))
}

/// One record read through the scattered reader, fixed up.
fn read_fixed<S: BlockSource>(
    fs: &mut AlignedReader<S>,
    pos: u64,
    rs: u64,
) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; rs as usize];
    fs.seek(SeekFrom::Start(pos))
        .and_then(|_| fs.read_exact(&mut bytes))
        .map_err(|err| err.to_string())?;
    if bytes.get(..4) != Some(b"FILE") {
        return Err("bad FILE signature".into());
    }
    record::fixup(&mut bytes)?;
    Ok(bytes)
}
