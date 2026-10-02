//! A synthetic NTFS volume in memory, built to exercise the MFT pass without a
//! raw volume handle: a fragmented `$MFT`, directories, hard links, 8.3-only
//! names, resident and non-resident `$DATA` (one with a sparse run), symlinks
//! with resident and non-resident reparse values, an attribute list whose
//! `$DATA` lives in an extension record in another fragment, free slots and a
//! corrupt record.
//!
//! Only the fields `ntfs` validates and the scan reads are filled in; the
//! layouts follow <https://flatcap.github.io/linux-ntfs/ntfs/>.

use std::io;
use std::sync::Arc;

use super::volume::BlockSource;

pub const SECTOR: u64 = 512;
pub const CLUSTER: u64 = 4096;
pub const RECORD: usize = 1024;

const ROOT: u64 = 5;
const IN_USE: u16 = 0x1;
const IS_DIR: u16 = 0x2;

const SI: u32 = 0x10;
const ATTRIBUTE_LIST: u32 = 0x20;
const FILE_NAME: u32 = 0x30;
const DATA: u32 = 0x80;
const REPARSE: u32 = 0xC0;

const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;

pub struct Image {
    pub bytes: Arc<Vec<u8>>,
    /// Bytes of the table, i.e. what one full pass has to read.
    pub mft_bytes: u64,
}

/// A device that, like a raw volume, refuses unaligned reads.
pub struct MemSource {
    bytes: Arc<Vec<u8>>,
    sector: u64,
}

impl MemSource {
    pub fn new(image: &Image, sector: u64) -> Self {
        Self { bytes: image.bytes.clone(), sector }
    }
}

impl BlockSource for MemSource {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        if !offset.is_multiple_of(self.sector) || !(out.len() as u64).is_multiple_of(self.sector) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unaligned read: offset {offset}, length {}", out.len()),
            ));
        }
        let start = (offset as usize).min(self.bytes.len());
        let n = out.len().min(self.bytes.len() - start);
        out[..n].copy_from_slice(&self.bytes[start..start + n]);
        Ok(n)
    }
}

/// `records` slots in three fragments laid out out of order on the volume, so
/// table order and disk order disagree.
pub fn build(records: u64) -> Image {
    build_fragmented(records, 3)
}

/// `build` with the table split into `count` fragments. Three fragments are
/// laid out A, C, B on disk; more are laid out in reverse table order. Many
/// fragments is what a long-lived system volume's `$MFT` looks like, and what
/// makes any per-record walk of its run list expensive.
pub fn build_fragmented(records: u64, count: u64) -> Image {
    assert!(records >= 64, "room for the metafiles and every special case");
    assert!((3..=120).contains(&count), "the run list has to fit in record 0");
    let per_cluster = CLUSTER / RECORD as u64;
    let clusters = records.div_ceil(per_cluster);
    assert!(clusters > 4 * count, "every fragment needs a few clusters");
    // Fragment lengths in clusters: deliberately not multiples of anything.
    let lens: Vec<u64> = if count == 3 {
        let (a, b) = (clusters / 3 + 1, clusters / 4 + 3);
        vec![a, b, clusters - a - b]
    } else {
        let even = clusters / count;
        let mut lens: Vec<u64> = (0..count).map(|i| even - 1 + i % 3).collect();
        let used: u64 = lens.iter().sum();
        *lens.last_mut().unwrap() += clusters - used;
        lens
    };
    // Disk order: boot, then the fragments in `order`, each followed by a gap,
    // then a data area for non-resident values.
    let order: Vec<usize> =
        if count == 3 { vec![0, 2, 1] } else { (0..count as usize).rev().collect() };
    let mut lcns = vec![0u64; lens.len()];
    let mut next = 8u64;
    for (k, &i) in order.iter().enumerate() {
        lcns[i] = next;
        next += lens[i] + [5, 7, 3][k % 3];
    }
    let data_lcn = next;
    let reparse_values = records / 23 / 5 + 1;
    let total_clusters = data_lcn + 8 + reparse_values;
    let lcn_a = lcns[0];

    let fragments: Vec<(u64, u64)> = lcns.iter().copied().zip(lens.iter().copied()).collect();
    let record_pos = |n: u64| -> usize {
        let mut cluster = n / per_cluster;
        for &(lcn, len) in &fragments {
            if cluster < len {
                return ((lcn + cluster) * CLUSTER + (n % per_cluster) * RECORD as u64) as usize;
            }
            cluster -= len;
        }
        unreachable!("record {n} is past the table");
    };
    let mut writes: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut put = |n: u64, record: [u8; RECORD]| writes.push((record_pos(n), record.to_vec()));

    // Record 0, `$MFT`: its `$DATA` run list is the fragment list.
    let runs: Vec<(u64, Option<u64>)> =
        fragments.iter().map(|&(lcn, len)| (len, Some(lcn))).collect();
    let mft_runs = encode_runs(&runs);
    let mft_len = records * RECORD as u64;
    put(0, record(IN_USE, 0, &[non_resident(DATA, 1, &mft_runs, clusters * CLUSTER, mft_len)]));
    for n in 1..16 {
        let flags = if n == ROOT { IN_USE | IS_DIR } else { IN_USE };
        put(n, record(flags, 0, &[resident(FILE_NAME, 1, &file_name(ROOT, "$meta", 3))]));
    }

    // The extension record of the attribute-list case sits in the last
    // fragment, far from its base record in the first.
    let extension = records - 1;
    let ext_runs = encode_runs(&[(3, Some(data_lcn))]);
    put(
        extension,
        record(IN_USE, 31, &[non_resident(DATA, 4, &ext_runs, 3 * CLUSTER, 3 * CLUSTER - 10)]),
    );

    let mut dirs = vec![ROOT];
    let mut next_value_cluster = data_lcn + 8;
    let mut values: Vec<(usize, Vec<u8>)> = Vec::new();
    for n in 16..extension {
        let parent = dirs[(n as usize) % dirs.len()];
        let si = resident(SI, 0, &standard_information(n));
        let name = file_name(parent, &format!("f{n}.bin"), 1);
        let attributes = match n {
            // Base record whose `$DATA` lives in the extension record.
            31 => {
                let list = [
                    list_entry(SI, n, 0),
                    list_entry(FILE_NAME, n, 1),
                    list_entry(DATA, extension, 4),
                ]
                .concat();
                vec![si, resident(ATTRIBUTE_LIST, 2, &list), resident(FILE_NAME, 1, &name)]
            }
            _ => match n % 23 {
                0 => {
                    dirs.push(n);
                    let name = file_name(parent, &format!("d{n}"), 1);
                    put(n, record(IN_USE | IS_DIR, 0, &[si, resident(FILE_NAME, 1, &name)]));
                    continue;
                }
                1 => {
                    put(n, record(0, 0, &[si, resident(FILE_NAME, 1, &name)]));
                    continue;
                }
                2 => {
                    let mut corrupt = record(IN_USE, 0, &[si]);
                    corrupt[..4].copy_from_slice(b"BAAD");
                    put(n, corrupt);
                    continue;
                }
                // A hard link: one name here, one in the root.
                3 => vec![
                    si,
                    resident(FILE_NAME, 1, &name),
                    resident(FILE_NAME, 2, &file_name(ROOT, &format!("link{n}.bin"), 1)),
                    resident(DATA, 3, &[7u8; 13]),
                ],
                // Only an 8.3 name.
                4 => vec![
                    si,
                    resident(FILE_NAME, 1, &file_name(parent, &format!("F{n}~1.BIN"), 2)),
                    resident(DATA, 2, &[1u8; 3]),
                ],
                // A sparse middle run: allocated must skip it.
                5 => {
                    let runs =
                        encode_runs(&[(2, Some(data_lcn)), (5, None), (1, Some(data_lcn + 3))]);
                    vec![
                        si,
                        resident(FILE_NAME, 1, &name),
                        non_resident(DATA, 2, &runs, 8 * CLUSTER, 7 * CLUSTER + 100),
                    ]
                }
                6 => vec![
                    si,
                    resident(FILE_NAME, 1, &name),
                    resident(REPARSE, 2, &reparse_value(TAG_SYMLINK)),
                ],
                // A junction whose reparse value is non-resident: the tag has
                // to be fetched from the data area, outside the table.
                7 if n % 5 == 0 => {
                    let cluster = next_value_cluster;
                    next_value_cluster += 1;
                    values.push(((cluster * CLUSTER) as usize, reparse_value(TAG_MOUNT_POINT)));
                    let runs = encode_runs(&[(1, Some(cluster))]);
                    let name = file_name(parent, &format!("j{n}"), 1);
                    put(
                        n,
                        record(
                            IN_USE | IS_DIR,
                            0,
                            &[
                                si,
                                resident(FILE_NAME, 1, &name),
                                non_resident(REPARSE, 2, &runs, CLUSTER, 16),
                            ],
                        ),
                    );
                    continue;
                }
                _ => vec![
                    si,
                    resident(FILE_NAME, 1, &file_name(parent, &format!("f{n}.bin"), 3)),
                    resident(DATA, 2, &vec![0u8; (n % 97) as usize]),
                ],
            },
        };
        put(n, record(IN_USE, 0, &attributes));
    }

    let mut bytes = vec![0u8; (total_clusters * CLUSTER) as usize];
    write_boot(&mut bytes, total_clusters, lcn_a);
    for (at, value) in writes.into_iter().chain(values) {
        bytes[at..at + value.len()].copy_from_slice(&value);
    }
    Image { bytes: Arc::new(bytes), mft_bytes: mft_len }
}

fn write_boot(bytes: &mut [u8], total_clusters: u64, mft_lcn: u64) {
    bytes[3..11].copy_from_slice(b"NTFS    ");
    bytes[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    bytes[13] = (CLUSTER / SECTOR) as u8;
    bytes[21] = 0xF8;
    let sectors = total_clusters * (CLUSTER / SECTOR);
    bytes[40..48].copy_from_slice(&sectors.to_le_bytes());
    bytes[48..56].copy_from_slice(&mft_lcn.to_le_bytes());
    bytes[56..64].copy_from_slice(&2u64.to_le_bytes());
    bytes[64] = (-10i8) as u8; // 2^10-byte file records
    bytes[68] = 1;
    bytes[72..80].copy_from_slice(&0x1234_5678u64.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
}

/// A FILE record with its update-sequence fixup applied, as it sits on disk.
fn record(flags: u16, base: u64, attributes: &[Vec<u8>]) -> [u8; RECORD] {
    const USA_OFFSET: usize = 0x30;
    const FIRST_ATTRIBUTE: usize = 0x38;
    let sectors = RECORD / SECTOR as usize;

    let mut r = [0u8; RECORD];
    r[..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
    r[6..8].copy_from_slice(&(sectors as u16 + 1).to_le_bytes());
    r[16..18].copy_from_slice(&1u16.to_le_bytes());
    r[18..20].copy_from_slice(&1u16.to_le_bytes());
    r[20..22].copy_from_slice(&(FIRST_ATTRIBUTE as u16).to_le_bytes());
    r[22..24].copy_from_slice(&flags.to_le_bytes());
    r[28..32].copy_from_slice(&(RECORD as u32).to_le_bytes());
    // An extension record names its base with the base's sequence number,
    // which every record here has as 1.
    let base_ref = if base == 0 { 0 } else { base | (1 << 48) };
    r[32..40].copy_from_slice(&base_ref.to_le_bytes());

    let mut at = FIRST_ATTRIBUTE;
    for attribute in attributes {
        r[at..at + attribute.len()].copy_from_slice(attribute);
        at += attribute.len();
    }
    r[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let used = at + 8;
    r[24..28].copy_from_slice(&(used as u32).to_le_bytes());

    let usn = [0x2A, 0x00];
    r[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&usn);
    for s in 0..sectors {
        let tail = (s + 1) * SECTOR as usize - 2;
        let slot = USA_OFFSET + 2 + 2 * s;
        let original = [r[tail], r[tail + 1]];
        r[slot..slot + 2].copy_from_slice(&original);
        r[tail..tail + 2].copy_from_slice(&usn);
    }
    r
}

fn resident(ty: u32, instance: u16, value: &[u8]) -> Vec<u8> {
    let len = (24 + value.len()).next_multiple_of(8);
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[14..16].copy_from_slice(&instance.to_le_bytes());
    a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[20..22].copy_from_slice(&24u16.to_le_bytes());
    a[24..24 + value.len()].copy_from_slice(value);
    a
}

fn non_resident(ty: u32, instance: u16, runs: &[u8], allocated: u64, size: u64) -> Vec<u8> {
    let len = (64 + runs.len()).next_multiple_of(8);
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 1;
    a[14..16].copy_from_slice(&instance.to_le_bytes());
    let highest_vcn = (allocated / CLUSTER).saturating_sub(1);
    a[24..32].copy_from_slice(&highest_vcn.to_le_bytes());
    a[32..34].copy_from_slice(&64u16.to_le_bytes());
    a[40..48].copy_from_slice(&allocated.to_le_bytes());
    a[48..56].copy_from_slice(&size.to_le_bytes());
    a[56..64].copy_from_slice(&size.to_le_bytes());
    a[64..64 + runs.len()].copy_from_slice(runs);
    a
}

/// `(length in clusters, absolute LCN or None for sparse)` to the on-disk
/// delta encoding.
fn encode_runs(runs: &[(u64, Option<u64>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0i64;
    for &(len, lcn) in runs {
        let len_bytes = minimal_unsigned(len);
        let delta_bytes = match lcn {
            Some(lcn) => {
                let delta = lcn as i64 - previous;
                previous = lcn as i64;
                minimal_signed(delta)
            }
            None => Vec::new(),
        };
        out.push(((delta_bytes.len() as u8) << 4) | len_bytes.len() as u8);
        out.extend(len_bytes);
        out.extend(delta_bytes);
    }
    out.push(0);
    out
}

fn minimal_unsigned(v: u64) -> Vec<u8> {
    let n = (8 - (v.leading_zeros() / 8) as usize).max(1);
    v.to_le_bytes()[..n].to_vec()
}

fn minimal_signed(v: i64) -> Vec<u8> {
    let n = (1..=8usize)
        .find(|&n| {
            let shift = 64 - 8 * n as u32;
            (v << shift) >> shift == v
        })
        .unwrap_or(8);
    v.to_le_bytes()[..n].to_vec()
}

fn standard_information(n: u64) -> Vec<u8> {
    // 2021-01-01 in NT ticks, plus a per-record offset so a swapped field or
    // record shows up as a wrong timestamp.
    let base = 132_539_328_000_000_000u64 + n * 10_000_000;
    let mut v = vec![0u8; 48];
    v[0..8].copy_from_slice(&base.to_le_bytes());
    v[8..16].copy_from_slice(&(base + 1).to_le_bytes());
    v[16..24].copy_from_slice(&(base + 2).to_le_bytes());
    v[24..32].copy_from_slice(&(base + 3).to_le_bytes());
    v
}

fn file_name(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut v = vec![0u8; 66 + 2 * units.len()];
    v[0..8].copy_from_slice(&(parent | (1 << 48)).to_le_bytes());
    v[64] = units.len() as u8;
    v[65] = namespace;
    for (i, unit) in units.iter().enumerate() {
        v[66 + 2 * i..68 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    v
}

fn reparse_value(tag: u32) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    v[0..4].copy_from_slice(&tag.to_le_bytes());
    v
}

fn list_entry(ty: u32, record: u64, instance: u16) -> Vec<u8> {
    let mut e = vec![0u8; 32];
    e[0..4].copy_from_slice(&ty.to_le_bytes());
    e[4..6].copy_from_slice(&32u16.to_le_bytes());
    e[7] = 26;
    e[16..24].copy_from_slice(&(record | (1 << 48)).to_le_bytes());
    e[24..26].copy_from_slice(&instance.to_le_bytes());
    e
}
