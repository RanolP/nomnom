//! NTFS Master File Table backend — the mechanism WizTree uses.
//!
//! Instead of walking the directory tree and paying one `stat` per file, this
//! opens the volume as a raw device, finds `$MFT`, and reads every file record
//! in one sequential pass. Each record carries its own name, its parent's file
//! reference, both sizes and the standard-information timestamps, so the whole
//! tree is reconstructed from data the disk hands over in bulk.
//!
//! Two things that cost nothing here are expensive for the walk backend:
//! `allocated` (the MFT stores the `$DATA` run list, so on-disk size is exact
//! for sparse and compressed files) and the absence of per-file syscalls.
//!
//! The price is a raw volume handle, which Windows grants only to an
//! Administrator process. Every way that can fail is reported as
//! [`ScanFailure::MftUnavailable`] with a reason naming the cause, because
//! [`Backend::Auto`](crate::scan::Backend::Auto) swallows the error into a
//! silent fallback and that string is all the user ever sees about it.

pub mod record;
pub mod stream;
pub mod volume;

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ntfs::Ntfs;

use crate::scan::table::{Blob, EXTRA_LINK, Name, ScanTable};
use crate::scan::{BackendUsed, EntryKind, ScanError, ScanFailure, ScanOptions, ScanReport};
use crate::timings;
use stream::{Geometry, MftLayout};
use volume::{AlignedReader, BlockSource, CountingSource, IoStats, VolumeSource};

/// Records 0–15 are the NTFS metafiles (`$MFT`, `$LogFile`, `$Bitmap`, …).
/// They are filesystem plumbing, not user data, and never appear in a scan.
const FIRST_USER_RECORD: u64 = 16;

/// The root directory's file record number, fixed on every NTFS volume.
const ROOT_RECORD: u64 = 5;

/// Read size of the sequential pass over the table: 32768 records of 1 KiB
/// per device read, large enough that per-read overhead vanishes against the
/// transfer even on NVMe.
const STREAM_CHUNK: usize = 32 << 20;

/// Block of the reader behind everything that is not the table stream: the
/// boot sector, `$Volume`, and per-record reads that land elsewhere (a
/// non-resident reparse value, an extension record). Those are scattered, so
/// a large block would only inflate each one.
const SCATTERED_BLOCK: usize = 4 << 10;

/// Set to anything to print the scan's device reads to stderr, which is how a
/// real elevated run proves the IO stays near one pass over the table.
pub const IO_STATS_ENV: &str = "NOMNOM_MFT_IO_STATS";

/// Upper bound on collected per-record errors. A pathologically damaged volume
/// must not turn a scan into an out-of-memory error report.
const MAX_ERRORS: usize = 1024;

pub(crate) fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    // `respect_gitignore` and `follow_symlinks` describe a tree walk. The MFT
    // sees the volume, not a repository, and never traverses a link. Only
    // `progress` applies here.

    let started = Instant::now();
    let canonical =
        std::fs::canonicalize(root).map_err(|_| ScanFailure::RootUnreadable(root.to_path_buf()))?;
    let root_canon = strip_verbatim(&canonical)?;
    let target = locate_volume(&root_canon)?;

    let source = VolumeSource::open(&target.device).map_err(|err| {
        // Name the mount point, not just the volume GUID: the GUID is what the
        // API wants, but `C:\` is what the user recognises.
        let mount = target.mount.display();
        let device = Path::new(&target.device).display();
        match err.raw_os_error() {
            // ERROR_ACCESS_DENIED. By far the common case, and the only one the
            // user can fix, so it gets the actionable wording.
            Some(5) => ScanFailure::MftUnavailable(format!(
                "reading the MFT of {mount} needs an elevated (Administrator) process: \
                 opening the raw volume {device} was denied (error 5)"
            )),
            Some(code) => ScanFailure::MftUnavailable(format!(
                "opening the raw volume {device} behind {mount} failed with error {code}: {err}"
            )),
            None => ScanFailure::MftUnavailable(format!(
                "opening the raw volume {device} behind {mount} failed: {err}"
            )),
        }
    })?;

    let mut fs = AlignedReader::new(CountingSource::new(source), target.sector, SCATTERED_BLOCK);
    let ntfs = Ntfs::new(&mut fs).map_err(|err| {
        ScanFailure::MftUnavailable(format!(
            "parsing the NTFS boot sector of {} failed: {err}",
            target.mount.display()
        ))
    })?;
    // Proves the superblock really describes a live NTFS volume rather than
    // something that happened to survive boot-sector parsing.
    ntfs.volume_info(&mut fs).map_err(|err| {
        ScanFailure::MftUnavailable(format!(
            "reading $Volume on {} failed: {err}",
            target.mount.display()
        ))
    })?;

    let geo = geometry(&ntfs, target.sector).map_err(ScanFailure::MftUnavailable)?;
    let layout = MftLayout::read(&mut fs, &geo).map_err(ScanFailure::MftUnavailable)?;
    let started = timings::lap("MFT volume open", started);
    // The stream gets its own uncached handle. Should the volume refuse one,
    // a cached handle still reads correctly, only with an extra copy.
    let stream = VolumeSource::open_unbuffered(&target.device)
        .or_else(|_| VolumeSource::open(&target.device))
        .map_err(|err| {
            ScanFailure::MftUnavailable(format!(
                "opening a second handle on the raw volume behind {} failed: {err}",
                target.mount.display()
            ))
        })?;
    let mut stream = CountingSource::new(stream);
    let (pass, times) = read_records(&mut fs, &mut stream, &geo, &layout, opts);
    if std::env::var_os(IO_STATS_ENV).is_some() {
        eprintln!("{}", io_summary(stream.stats(), &layout));
    }
    let started = timings::lap("MFT IO + parse", started);
    times.record();

    let report = build_table(pass, &target, root, &root_canon);
    timings::lap("MFT table build", started);
    Ok(report)
}

fn io_summary(stats: IoStats, layout: &MftLayout) -> String {
    const MIB: f64 = (1u64 << 20) as f64;
    let ratio = stats.bytes as f64 / layout.bytes().max(1) as f64;
    format!(
        "MFT IO: {} reads, {:.1} MiB read ({} re-reads, {:.1} MiB) for a {:.1} MiB table \
         of {} records: {ratio:.2}x the table",
        stats.reads,
        stats.bytes as f64 / MIB,
        stats.rereads,
        stats.reread_bytes as f64 / MIB,
        layout.bytes() as f64 / MIB,
        layout.record_count(),
    )
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Everything one named record contributes, held until every directory is
/// known. A file's parent may sit later in the table than the file itself, so
/// rows cannot be linked during the pass that reads records.
#[derive(Debug, Clone, Copy)]
struct Rec {
    number: u64,
    is_dir: bool,
    kind: EntryKind,
    size: u64,
    allocated: u64,
    /// `(modified, accessed)` in NT ticks.
    times: Option<(u64, u64)>,
    /// `RecordPass::links[names..names + count]`, never empty: Win32 names
    /// first, then POSIX ones, or the 8.3 aliases when it has nothing else.
    /// More than one means a hard link.
    names: usize,
    count: usize,
}

/// What the pass over the table collects, records in record-number order.
#[derive(Debug, Default)]
struct RecordPass {
    records: Vec<Rec>,
    links: Vec<record::Link>,
    /// Every name the pass decoded, back to back; `links` index it.
    names: String,
    errors: Vec<ScanError>,
}

impl RecordPass {
    fn names_of(&self, record: &Rec) -> &[record::Link] {
        self.links.get(record.names..record.names + record.count).unwrap_or(&[])
    }

    fn first(&self, record: &Rec) -> Option<&record::Link> {
        self.links.get(record.names)
    }

    fn text(&self, link: &record::Link) -> &str {
        let start = link.off as usize;
        self.names.get(start..start + link.len as usize).unwrap_or("")
    }
}

fn geometry(ntfs: &Ntfs, sector: u64) -> Result<Geometry, String> {
    let mft_pos = ntfs
        .mft_position()
        .value()
        .ok_or_else(|| "boot sector gives no $MFT position".to_string())?
        .get();
    Ok(Geometry {
        record_size: u64::from(ntfs.file_record_size()),
        cluster: u64::from(ntfs.cluster_size()),
        sector: sector.max(1),
        mft_pos,
    })
}

/// Where the pass spent its time, so the next real run separates device time
/// from parse time instead of reporting one lump.
#[derive(Debug, Default)]
struct PassTimes {
    /// Summed duration of the reader thread's device reads.
    io: Duration,
    /// Wall time the main thread spent parsing chunks across the pool.
    parse: Duration,
    /// Wall time the main thread sat waiting for the next chunk to land.
    wait: Duration,
    /// Main-thread time filing each parsed chunk's records and names.
    gather: Duration,
    /// Sorting the bases and merging extension records into them.
    merge: Duration,
    /// Non-resident reparse values, one scattered read each.
    reparse: Duration,
    reparse_reads: usize,
    /// Ordering each record's names and assembling the records.
    assemble: Duration,
    bytes: u64,
}

impl PassTimes {
    fn record(&self) {
        let mib_s = self.bytes as f64 / (1 << 20) as f64 / self.io.as_secs_f64().max(1e-9);
        timings::record(&format!("MFT raw IO, reader thread ({mib_s:.0} MiB/s)"), self.io);
        timings::record("MFT parse (CPU, pool wall)", self.parse);
        timings::record("MFT parse waiting on IO", self.wait);
        timings::record("MFT gather parsed chunks", self.gather);
        timings::record("MFT extension merge", self.merge);
        timings::record(&format!("MFT reparse tag reads ({})", self.reparse_reads), self.reparse);
        timings::record("MFT record assemble", self.assemble);
    }
}

/// One device read's worth of the table.
struct Batch {
    first: u64,
    count: u64,
    buf: Vec<u8>,
    /// Where record `first` starts in `buf` (the read was sector-aligned).
    offset: usize,
    /// Bytes of records the read delivered from `offset` on.
    got: usize,
    failed: Option<String>,
}

/// A base record as the pass collects it, before extensions are merged.
struct Base {
    number: u64,
    sequence: u16,
    facts: record::Facts,
}

/// Reads every user record.
///
/// One thread reads the table forward in large sequential device reads, one
/// extent at a time; each chunk is parsed in place across the rayon pool while
/// the next one is read. Chunk buffers go back to the reader, so the steady
/// state allocates nothing per chunk. Extension records are merged into their
/// base records once the whole table is in.
fn read_records<S: BlockSource, T: BlockSource + Send>(
    fs: &mut AlignedReader<S>,
    source: &mut T,
    geo: &Geometry,
    layout: &MftLayout,
    opts: &ScanOptions,
) -> (RecordPass, PassTimes) {
    use rayon::prelude::*;

    /// Chunks read ahead of the parse: enough to keep the device busy while
    /// one chunk is parsed, few enough to bound memory at a few chunks.
    const READ_AHEAD: usize = 2;
    const MEMORY_ALIGN: usize = 4096;
    /// Records per parse job: enough to amortise a job's buffers, few enough
    /// that a 32 MiB chunk still spreads over the pool.
    const PARSE_RUN: usize = 2048;

    let total = layout.record_count();
    opts.set_entries_total(total.saturating_sub(FIRST_USER_RECORD));
    let rs = geo.record_size as usize;
    let sector = geo.sector;
    let per_chunk = (STREAM_CHUNK / rs).max(1) as u64;
    let (extents, gaps) = layout.extents(FIRST_USER_RECORD);

    let mut times = PassTimes::default();
    let mut errors = Vec::new();
    for gap in gaps {
        push_error(
            &mut errors,
            format!("MFT records {}..{}: no clusters on the volume hold them", gap.start, gap.end),
        );
    }
    // Reserved for the whole table up front: growing a vector of millions by
    // doubling copies it over and over. Capped, since the count comes off the
    // volume.
    let mut bases: Vec<Base> = Vec::with_capacity(total.min(1 << 24) as usize);
    let mut extensions: Vec<(u64, u16, record::Facts)> = Vec::new();
    let mut names = String::new();
    let mut links: Vec<record::Link> = Vec::new();

    std::thread::scope(|scope| {
        let (send, batches) = std::sync::mpsc::sync_channel::<Batch>(READ_AHEAD);
        let (give_back, recycled) = std::sync::mpsc::channel::<Vec<u8>>();
        let reader = scope.spawn(move || {
            let mut io = Duration::ZERO;
            let mut bytes = 0u64;
            for extent in &extents {
                let end = extent.first + extent.count;
                let mut first = extent.first;
                while first < end {
                    let count = per_chunk.min(end - first);
                    let phys = extent.phys + (first - extent.first) * rs as u64;
                    let start = phys - phys % sector;
                    let into = (phys - start) as usize;
                    let want = (into + count as usize * rs).next_multiple_of(sector as usize);
                    let mut buf = recycled.try_recv().unwrap_or_default();
                    if buf.len() < want + MEMORY_ALIGN {
                        buf.resize(want + MEMORY_ALIGN, 0);
                    }
                    // An unbuffered handle also wants the buffer's address
                    // sector-aligned; a page boundary is aligned for any sector.
                    let pad = buf.as_ptr().align_offset(MEMORY_ALIGN).min(MEMORY_ALIGN);
                    let started = Instant::now();
                    let read = source.read_at(start, &mut buf[pad..pad + want]);
                    io += started.elapsed();
                    let offset = pad + into;
                    let (got, failed) = match read {
                        Ok(n) => (n.saturating_sub(into).min(count as usize * rs), None),
                        Err(err) => (0, Some(err.to_string())),
                    };
                    bytes += got as u64;
                    let batch = Batch { first, count, buf, offset, got, failed };
                    if send.send(batch).is_err() {
                        return (io, bytes);
                    }
                    first += count;
                }
            }
            (io, bytes)
        });

        let mut waited = Instant::now();
        for mut batch in batches {
            times.wait += waited.elapsed();
            let started = Instant::now();
            let whole = batch.got - batch.got % rs;
            let bytes = &mut batch.buf[batch.offset..batch.offset + whole];
            // Each job parses a run of records into its own names buffer, so
            // the pool shares nothing and no record allocates.
            let parts: Vec<(Vec<record::Slot>, String, Vec<record::Link>)> = bytes
                .par_chunks_mut(rs * PARSE_RUN)
                .map(|run| {
                    let mut names = String::with_capacity(run.len() / 32);
                    let mut links = Vec::with_capacity(run.len() / rs * 2);
                    let slots = run
                        .chunks_mut(rs)
                        .map(|r| record::parse(r, geo.cluster, &mut names, &mut links))
                        .collect();
                    (slots, names, links)
                })
                .collect();
            times.parse += started.elapsed();
            let started = Instant::now();
            // Every record counts toward progress, free and unreadable ones
            // too, because `entries_total` is the size of the table, not of
            // its live set.
            if let Some(progress) = &opts.progress {
                progress.entries.fetch_add(batch.count, Ordering::Relaxed);
            }
            let mut number = batch.first;
            for (slots, run_names, run_links) in parts {
                let (name_base, link_base) = (names.len(), links.len());
                names.push_str(&run_names);
                links.extend(run_links.into_iter().map(|link| record::Link {
                    off: u32::try_from(name_base + link.off as usize).unwrap_or(u32::MAX),
                    ..link
                }));
                let refile = |mut facts: record::Facts| {
                    facts.links_start =
                        u32::try_from(link_base + facts.links_start as usize).unwrap_or(u32::MAX);
                    facts
                };
                for slot in slots {
                    match slot {
                        record::Slot::Free => {}
                        record::Slot::Corrupt(why) => {
                            push_error(&mut errors, format!("MFT record {number}: {why}"));
                        }
                        record::Slot::Base { sequence, facts } => {
                            bases.push(Base { number, sequence, facts: refile(facts) });
                        }
                        record::Slot::Extension { base, base_sequence, facts } => {
                            extensions.push((base, base_sequence, refile(facts)));
                        }
                    }
                    number += 1;
                }
            }
            let parsed = number - batch.first;
            times.gather += started.elapsed();
            if parsed < batch.count {
                let why = batch.failed.take().unwrap_or_else(|| "short read".into());
                let (a, b) = (batch.first + parsed, batch.first + batch.count);
                push_error(&mut errors, format!("MFT records {a}..{b}: reading failed: {why}"));
            }
            let _ = give_back.send(batch.buf);
            waited = Instant::now();
        }
        (times.io, times.bytes) = reader.join().unwrap_or_default();
    });

    let started = Instant::now();
    // Bases arrive in extent order; sorting makes the merge a binary search
    // even when the table's runs are out of order on the volume.
    if !bases.is_sorted_by_key(|b| b.number) {
        bases.sort_unstable_by_key(|b| b.number);
    }
    // Extension names stay in `links`; each is filed under its base by index,
    // in extension order, which is the order they join the base's names in.
    let mut extra: Vec<(usize, record::Facts)> = Vec::new();
    for (base, sequence, facts) in extensions {
        // An extension whose base is gone or reused is a stale leftover.
        if let Ok(i) = bases.binary_search_by_key(&base, |b| b.number)
            && bases[i].sequence == sequence
        {
            record::merge(&mut bases[i].facts, &facts);
            if facts.links_len > 0 {
                extra.push((i, facts));
            }
        }
    }
    extra.sort_by_key(|(i, _)| *i);
    let started = timings_mark(&mut times.merge, started);

    // Sorted by volume offset, so the scattered reads at least run forward.
    let mut deferred: Vec<(u64, usize)> = bases
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b.facts.reparse {
            Some(record::Reparse::At(at)) => Some((at, i)),
            _ => None,
        })
        .collect();
    deferred.sort_unstable();
    times.reparse_reads = deferred.len();
    for (at, i) in deferred {
        bases[i].facts.reparse =
            Some(read_tag_at(fs, at).map_or(record::Reparse::Unreadable, record::Reparse::Tag));
    }
    let started = timings_mark(&mut times.reparse, started);

    let mut ordered = Vec::with_capacity(links.len());
    let mut records = Vec::with_capacity(bases.len());
    let mut scratch: Vec<record::Link> = Vec::new();
    let mut next_extra = extra.iter().peekable();
    for (i, base) in bases.iter().enumerate() {
        scratch.clear();
        scratch.extend_from_slice(links_of(&links, &base.facts));
        while let Some((_, facts)) = next_extra.next_if(|(at, _)| *at == i) {
            scratch.extend_from_slice(links_of(&links, facts));
        }
        records.extend(assemble(base.number, &base.facts, &mut scratch, &mut ordered));
    }
    timings_mark(&mut times.assemble, started);
    (RecordPass { records, links: ordered, names, errors }, times)
}

/// Adds the time since `started` to `slot` and starts the next span.
fn timings_mark(slot: &mut Duration, started: Instant) -> Instant {
    let now = Instant::now();
    *slot += now - started;
    now
}

fn links_of<'a>(links: &'a [record::Link], facts: &record::Facts) -> &'a [record::Link] {
    let start = facts.links_start as usize;
    links.get(start..start + facts.links_len as usize).unwrap_or(&[])
}

/// A non-resident reparse value's tag. Rare enough (junctions with long
/// targets) that one scattered read each is fine.
fn read_tag_at<S: BlockSource>(fs: &mut AlignedReader<S>, at: u64) -> Option<u32> {
    let mut tag = [0u8; 4];
    fs.seek(SeekFrom::Start(at)).ok()?;
    fs.read_exact(&mut tag).ok()?;
    Some(u32::from_le_bytes(tag))
}

/// One base record's contribution, `None` for a nameless record. `names` is
/// every `$FILE_NAME` it has, its own then its extensions'; the ones it keeps
/// are appended to `ordered`.
fn assemble(
    number: u64,
    facts: &record::Facts,
    names: &mut [record::Link],
    ordered: &mut Vec<record::Link>,
) -> Option<Rec> {
    // Stable, so names of one rank keep their on-disk order. The 8.3 alias
    // duplicates a long name that is also present; it is a fallback, never a
    // preference.
    names.sort_by_key(record::Link::rank);
    let long = names.iter().take_while(|l| l.rank() < 2).count();
    let count = if long > 0 { long } else { names.len() };
    if count == 0 {
        return None;
    }
    let start = ordered.len();
    ordered.extend_from_slice(&names[..count]);
    let is_dir = facts.is_dir;
    let tag = match facts.reparse {
        Some(record::Reparse::Tag(tag)) => Some(tag),
        _ => None,
    };
    // Directories occupy index clusters, but that space is filesystem
    // bookkeeping rather than any file's content, and charging it here would
    // double-count in a size roll-up.
    let (size, allocated) =
        if is_dir { (0, 0) } else { (facts.size.unwrap_or(0), facts.allocated) };
    Some(Rec {
        number,
        is_dir,
        kind: classify(is_dir, tag),
        size,
        allocated,
        times: facts.times,
        names: start,
        count,
    })
}

/// Where a directory record stands relative to the scan root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Unknown,
    /// On the chain being resolved right now; meeting it again is a cycle.
    Visiting,
    Inside,
    /// Under a metafile (`$Extend` and below), or outside a subtree scan:
    /// left out on purpose, silently.
    Outside,
    /// Its parent chain dead-ends or loops. Kept, so the catalog drops it
    /// and counts it as damage.
    Broken,
}

/// Turns the pass into the flat table: one row per name, in record order,
/// each filed under its parent's row. No path is built. Record numbers map to
/// rows through a plain vector indexed by record number.
///
/// A directory contributes its first name. A file contributes every name it
/// has, each under its own parent, all sharing one blob; every name past the
/// first is flagged as an extra link, so its bytes count once.
fn build_table(
    pass: RecordPass,
    target: &VolumeTarget,
    root: &Path,
    root_canon: &Path,
) -> ScanReport {
    let mut table = ScanTable::new();
    let Some(root_record) = root_record(&pass, &target.mount, root_canon) else {
        return ScanReport {
            root: root.to_path_buf(),
            table,
            errors: pass.errors,
            backend_used: BackendUsed::Mft,
        };
    };
    let records = &pass.records;
    // Row 0's empty name is offset 0, length 0 of any buffer.
    let parent_of = |record: &Rec| pass.first(record).map_or(u64::MAX, |l| l.parent);

    // Records arrive sorted by number, so the last one bounds the index.
    let span = records.last().map_or(0, |r| r.number as usize + 1);
    let mut slot = vec![u32::MAX; span];
    for (i, record) in records.iter().enumerate() {
        slot[record.number as usize] = i as u32;
    }
    let dir_at = |number: u64| {
        let i = *slot.get(number as usize)?;
        (i != u32::MAX && records[i as usize].is_dir).then_some(i as usize)
    };

    // Which directories lie under the root, each resolved once: a chain is
    // followed up to the first directory already placed, then every
    // directory on it takes that answer.
    let mut place = vec![Place::Unknown; records.len()];
    let mut chain = Vec::new();
    for start in 0..records.len() {
        if !records[start].is_dir || place[start] != Place::Unknown {
            continue;
        }
        let mut cursor = start;
        let found = loop {
            if records[cursor].number == root_record {
                break Place::Inside;
            }
            match place[cursor] {
                Place::Unknown => {}
                Place::Visiting => break Place::Broken,
                known => break known,
            }
            place[cursor] = Place::Visiting;
            chain.push(cursor);
            let parent = parent_of(&records[cursor]);
            if parent == root_record {
                break Place::Inside;
            }
            if parent < FIRST_USER_RECORD {
                break Place::Outside;
            }
            match dir_at(parent) {
                Some(up) => cursor = up,
                None => break Place::Broken,
            }
        };
        for i in chain.drain(..) {
            place[i] = found;
        }
    }
    // Where a name filed under `parent` goes: `Some(row)`, or `None` to leave
    // it out. A dangling parent gives `u32::MAX`; such a row is pointed at
    // itself, which no walk from the root reaches.
    let filed = |parent: u64, node_of: &[u32]| -> Option<u32> {
        if parent == root_record {
            return Some(0);
        }
        if parent < FIRST_USER_RECORD {
            return None;
        }
        match dir_at(parent).map(|d| place[d]) {
            Some(Place::Inside | Place::Broken) => Some(node_of[parent as usize]),
            Some(_) => None,
            None => Some(u32::MAX),
        }
    };

    table.nodes.reserve(records.len());
    table.blobs.reserve(records.len());
    let blob_of = |record: &Rec| Blob {
        size: record.size,
        allocated: Some(record.allocated),
        modified: record.times.and_then(|(m, _)| to_system_time(m)),
        accessed: record.times.and_then(|(_, a)| to_system_time(a)),
    };
    // Rows name their text in place: the pass's buffer becomes the table's.
    let row = |parent: u32, link: &record::Link, blob: u32, kind: EntryKind, flags: u8| Name {
        parent,
        name_off: link.off,
        name_len: link.len,
        blob,
        kind,
        flags,
    };
    let mut node_of = vec![u32::MAX; span];
    for (i, record) in records.iter().enumerate() {
        if !record.is_dir || !matches!(place[i], Place::Inside | Place::Broken) {
            continue;
        }
        let blob = table.push_blob(blob_of(record));
        if record.number == root_record {
            table.nodes[0].blob = blob;
            node_of[record.number as usize] = 0;
            continue;
        }
        let Some(first) = pass.first(record) else { continue };
        node_of[record.number as usize] = table.nodes.len() as u32;
        table.nodes.push(row(u32::MAX, first, blob, record.kind, 0));
    }
    if root_record == ROOT_RECORD {
        // The volume root has no user record; the walk reports its root too.
        table.nodes[0].blob =
            table.push_blob(Blob { size: 0, allocated: Some(0), modified: None, accessed: None });
    }
    for record in records {
        if record.is_dir {
            let at = node_of[record.number as usize];
            if at != 0 && at != u32::MAX {
                let parent = filed(parent_of(record), &node_of).unwrap_or(u32::MAX);
                table.nodes[at as usize].parent = if parent == u32::MAX { at } else { parent };
            }
            continue;
        }
        let mut blob = None;
        for (k, link) in pass.names_of(record).iter().enumerate() {
            let Some(parent) = filed(link.parent, &node_of) else { continue };
            let blob = *blob.get_or_insert_with(|| table.push_blob(blob_of(record)));
            let flags = if k == 0 { 0 } else { EXTRA_LINK };
            let at = table.nodes.len() as u32;
            let parent = if parent == u32::MAX { at } else { parent };
            table.nodes.push(row(parent, link, blob, record.kind, flags));
        }
    }
    table.names = pass.names;
    ScanReport {
        root: root.to_path_buf(),
        table,
        errors: pass.errors,
        backend_used: BackendUsed::Mft,
    }
}

/// The record of the directory the scan is rooted at: the volume root, or a
/// directory below it found by its names, compared the way NTFS compares
/// them. `None` when the root lies outside the volume's table.
fn root_record(pass: &RecordPass, mount: &Path, root_canon: &Path) -> Option<u64> {
    let same =
        |a: &OsStr, b: &OsStr| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
    let mut below = root_canon.components();
    for part in mount.components() {
        if !same(below.next()?.as_os_str(), part.as_os_str()) {
            return None;
        }
    }
    let mut cursor = ROOT_RECORD;
    for part in below {
        let name = part.as_os_str();
        cursor = pass
            .records
            .iter()
            .find(|r| {
                r.is_dir
                    && pass
                        .first(r)
                        .is_some_and(|l| l.parent == cursor && same(OsStr::new(pass.text(l)), name))
            })?
            .number;
    }
    Some(cursor)
}

fn push_error(errors: &mut Vec<ScanError>, message: String) {
    match errors.len() {
        n if n < MAX_ERRORS => errors.push(ScanError { path: None, message }),
        n if n == MAX_ERRORS => errors.push(ScanError {
            path: None,
            message: format!("more than {MAX_ERRORS} per-record failures; the rest are omitted"),
        }),
        _ => {}
    }
}

/// Only the two tags `std::fs` calls a symlink are reported as one, so this
/// backend agrees with the walk backend about what an entry is. Every other
/// reparse point (OneDrive placeholders, dedup stubs, WCI containers) is an
/// ordinary file or directory as far as a size scan is concerned.
fn classify(is_dir: bool, reparse_tag: Option<u32>) -> EntryKind {
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    match reparse_tag {
        Some(IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK) => EntryKind::Symlink,
        _ if is_dir => EntryKind::Dir,
        _ => EntryKind::File,
    }
}

/// NT timestamps count 100-nanosecond ticks from 1601-01-01 UTC.
fn to_system_time(ticks: u64) -> Option<SystemTime> {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_1601_TO_1970: u64 = 11_644_473_600;

    if ticks == 0 {
        return None;
    }
    let since_1601 =
        Duration::new(ticks / TICKS_PER_SECOND, ((ticks % TICKS_PER_SECOND) * 100) as u32);
    let epoch_gap = Duration::from_secs(SECONDS_1601_TO_1970);
    if since_1601 >= epoch_gap {
        UNIX_EPOCH.checked_add(since_1601 - epoch_gap)
    } else {
        UNIX_EPOCH.checked_sub(epoch_gap - since_1601)
    }
}

// ---------------------------------------------------------------------------
// Volume discovery
// ---------------------------------------------------------------------------

struct VolumeTarget {
    /// Device path to hand `CreateFileW`, e.g. `\\?\Volume{...}`.
    device: OsString,
    /// Where the volume is mounted, e.g. `C:\`. Path reconstruction starts here.
    mount: PathBuf,
    /// Required read alignment, in bytes.
    sector: u64,
}

fn locate_volume(root_canon: &Path) -> Result<VolumeTarget, ScanFailure> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceW, GetDriveTypeW, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
        GetVolumePathNameW,
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;

    let path = wide(root_canon.as_os_str());

    let mut mount_buf = [0u16; 261];
    // SAFETY: `path` is NUL-terminated and outlives the call; `mount_buf` is
    // writable for the element count passed alongside it.
    let ok = unsafe {
        GetVolumePathNameW(path.as_ptr(), mount_buf.as_mut_ptr(), mount_buf.len() as u32)
    };
    if ok == 0 {
        return Err(ScanFailure::MftUnavailable(format!(
            "could not determine which volume holds {}: {}",
            root_canon.display(),
            std::io::Error::last_os_error()
        )));
    }
    let mount = from_wide(&mount_buf);
    let mount_w = wide(mount.as_os_str());

    // SAFETY: `mount_w` is a NUL-terminated volume root path.
    match unsafe { GetDriveTypeW(mount_w.as_ptr()) } {
        DRIVE_REMOTE => {
            return Err(ScanFailure::MftUnavailable(format!(
                "{} is on a network drive, which has no local MFT to read",
                mount.display()
            )));
        }
        DRIVE_REMOVABLE | DRIVE_CDROM => {
            return Err(ScanFailure::MftUnavailable(format!(
                "{} is on a removable drive; the MFT backend only reads fixed volumes",
                mount.display()
            )));
        }
        _ => {}
    }

    let mut fs_name = [0u16; 64];
    let mut serial = 0u32;
    let mut max_component = 0u32;
    let mut flags = 0u32;
    // SAFETY: every out parameter is a live local of the declared type, and
    // `fs_name` is written for at most the element count passed with it. The
    // volume-label buffer is null, which the API documents as "not wanted".
    let ok = unsafe {
        GetVolumeInformationW(
            mount_w.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(ScanFailure::MftUnavailable(format!(
            "could not read filesystem information for {}: {}",
            mount.display(),
            std::io::Error::last_os_error()
        )));
    }
    let filesystem = from_wide(&fs_name).to_string_lossy().into_owned();
    if !filesystem.eq_ignore_ascii_case("NTFS") {
        return Err(ScanFailure::MftUnavailable(format!(
            "{} is formatted {filesystem}, not NTFS, so it has no Master File Table",
            mount.display()
        )));
    }

    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;
    // SAFETY: all four out parameters are live locals of the expected type.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            mount_w.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    // A failure here costs only alignment granularity, and 512 is the smallest
    // any NTFS volume uses, so it is a safe floor rather than a fatal error.
    let sector = if ok == 0 { 512 } else { u64::from(bytes_per_sector).max(512) };

    // The volume GUID path addresses a volume mounted on a folder as well as a
    // lettered one, so it is preferred over `\\.\C:`.
    let mut guid = [0u16; 64];
    // SAFETY: `mount_w` is a NUL-terminated mount point; `guid` is written for
    // at most the element count passed alongside it.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount_w.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
    };
    let device = if ok == 0 {
        let text = mount.to_string_lossy();
        let letter = text.trim_end_matches(['\\', '/']);
        OsString::from(format!("\\\\.\\{letter}"))
    } else {
        // CreateFileW rejects the trailing separator this API returns.
        let text = from_wide(&guid).to_string_lossy().into_owned();
        OsString::from(text.trim_end_matches('\\').to_owned())
    };

    Ok(VolumeTarget { device, mount, sector })
}

/// Turns `\\?\C:\x` back into `C:\x`.
///
/// `canonicalize` returns the extended-length form, but every `Entry::path` has
/// to be spelled the way the walk backend spells it: `Catalog::build` re-attaches
/// children to parents by exact path prefix, so one stray `\\?\` fragments the
/// tree and every roll-up below it is wrong.
pub fn strip_verbatim(path: &Path) -> Result<PathBuf, ScanFailure> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(ScanFailure::MftUnavailable(format!(
            "{} has no volume prefix",
            path.display()
        )));
    };
    match prefix.kind() {
        std::path::Prefix::Disk(_) => Ok(path.to_path_buf()),
        std::path::Prefix::VerbatimDisk(letter) => {
            let mut out = PathBuf::from(format!("{}:\\", letter as char));
            out.extend(path.components().skip(1).filter_map(|c| match c {
                Component::RootDir => None,
                other => Some(other.as_os_str()),
            }));
            Ok(out)
        }
        _ => Err(ScanFailure::MftUnavailable(format!(
            "{} is not on a local drive-lettered volume",
            path.display()
        ))),
    }
}

pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn from_wide(buffer: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    PathBuf::from(OsString::from_wide(&buffer[..len]))
}

#[cfg(test)]
mod test_image;

#[cfg(test)]
mod tests {
    use super::*;
    use test_image::{Image, MemSource, SECTOR, build};

    use ntfs::attribute_value::NtfsAttributeValue;
    use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
    use ntfs::{NtfsAttributeType, NtfsFileFlags};

    /// One named record, as both parsers can spell it.
    #[derive(Debug, PartialEq)]
    struct RawRecord {
        number: u64,
        is_dir: bool,
        kind: EntryKind,
        size: u64,
        allocated: u64,
        modified: Option<SystemTime>,
        accessed: Option<SystemTime>,
        names: Vec<(u64, String)>,
    }

    struct RefPass {
        records: Vec<RawRecord>,
        errors: Vec<ScanError>,
    }

    fn raw_records(pass: &RecordPass) -> Vec<RawRecord> {
        pass.records
            .iter()
            .map(|r| RawRecord {
                number: r.number,
                is_dir: r.is_dir,
                kind: r.kind,
                size: r.size,
                allocated: r.allocated,
                modified: r.times.and_then(|(m, _)| to_system_time(m)),
                accessed: r.times.and_then(|(_, a)| to_system_time(a)),
                names: pass
                    .names_of(r)
                    .iter()
                    .map(|l| (l.parent, pass.text(l).to_owned()))
                    .collect(),
            })
            .collect()
    }

    /// The pass as it ran before the hand-rolled parser: one `Ntfs::file` per
    /// record over a single-block reader. Slow, but it is the `ntfs` crate's
    /// reading of every record, which makes it the oracle.
    fn reference_pass(image: &Image, block: usize) -> RefPass {
        let source = CountingSource::new(MemSource::new(image, SECTOR));
        let mut fs = AlignedReader::new(source, SECTOR, block);
        let ntfs = Ntfs::new(&mut fs).unwrap();
        let geo = geometry(&ntfs, SECTOR).unwrap();
        let layout = MftLayout::read(&mut fs, &geo).unwrap();
        let mut pass = RefPass { records: Vec::new(), errors: Vec::new() };
        for number in FIRST_USER_RECORD..layout.record_count() {
            match ntfs_record(&ntfs, &mut fs, number) {
                Ok(record) => pass.records.extend(record),
                Err(err) => push_error(&mut pass.errors, format!("MFT record {number}: {err}")),
            }
        }
        pass
    }

    /// One record read through the `ntfs` crate.
    fn ntfs_record<T: Read + Seek>(
        ntfs: &Ntfs,
        fs: &mut T,
        number: u64,
    ) -> Result<Option<RawRecord>, ntfs::NtfsError> {
        let file = ntfs.file(fs, number)?;
        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            return Ok(None);
        }
        let is_dir = file.is_directory();
        let mut times = None;
        let (mut size, mut allocated) = (None, 0u64);
        let (mut long, mut win32_names, mut short) = (Vec::new(), 0usize, Vec::new());
        let mut tag = None;
        let mut attributes = file.attributes();
        while let Some(item) = attributes.next(fs) {
            let Ok(item) = item else { continue };
            let Ok(attribute) = item.to_attribute() else { continue };
            let Ok(ty) = attribute.ty() else { continue };
            match ty {
                NtfsAttributeType::StandardInformation => {
                    if let Ok(v) = attribute.structured_value::<_, NtfsStandardInformation>(fs) {
                        let ticks =
                            (v.modification_time().nt_timestamp(), v.access_time().nt_timestamp());
                        times = Some(ticks);
                    }
                }
                NtfsAttributeType::FileName => {
                    if let Ok(v) = attribute.structured_value::<_, NtfsFileName>(fs) {
                        let entry = (
                            v.parent_directory_reference().file_record_number(),
                            v.name().to_string_lossy(),
                        );
                        match v.namespace() {
                            NtfsFileNamespace::Dos => short.push(entry),
                            NtfsFileNamespace::Posix => long.push(entry),
                            NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos => {
                                long.insert(win32_names, entry);
                                win32_names += 1;
                            }
                        }
                    }
                }
                NtfsAttributeType::Data if attribute.name().is_ok_and(|n| n.is_empty()) => {
                    size = Some(attribute.value_length());
                    if let Ok(NtfsAttributeValue::NonResident(value)) = attribute.value(fs) {
                        for run in value.data_runs() {
                            let Ok(run) = run else { break };
                            if run.data_position().value().is_some() {
                                allocated += run.allocated_size();
                            }
                        }
                    }
                }
                NtfsAttributeType::ReparsePoint => {
                    use ntfs::NtfsReadSeek;
                    let mut bytes = [0u8; 4];
                    if let Ok(mut value) = attribute.value(fs)
                        && value.read_exact(fs, &mut bytes).is_ok()
                    {
                        tag = Some(u32::from_le_bytes(bytes));
                    }
                }
                _ => {}
            }
        }
        let names = if long.is_empty() { short } else { long };
        if names.is_empty() {
            return Ok(None);
        }
        let (size, allocated) = if is_dir { (0, 0) } else { (size.unwrap_or(0), allocated) };
        Ok(Some(RawRecord {
            number,
            is_dir,
            kind: classify(is_dir, tag),
            size,
            allocated,
            modified: times.and_then(|(m, _)| to_system_time(m)),
            accessed: times.and_then(|(_, a)| to_system_time(a)),
            names,
        }))
    }

    fn streamed_pass(image: &Image, sector: u64) -> (RecordPass, IoStats) {
        let (pass, stats, _) = timed_pass(image, sector);
        (pass, stats)
    }

    fn timed_pass(image: &Image, sector: u64) -> (RecordPass, IoStats, PassTimes) {
        let source = CountingSource::new(MemSource::new(image, sector));
        let mut fs = AlignedReader::new(source, sector, SCATTERED_BLOCK);
        let ntfs = Ntfs::new(&mut fs).unwrap();
        let geo = geometry(&ntfs, sector).unwrap();
        let layout = MftLayout::read(&mut fs, &geo).unwrap();
        let mut stream = CountingSource::new(MemSource::new(image, sector));
        let (pass, times) =
            read_records(&mut fs, &mut stream, &geo, &layout, &ScanOptions::default());
        let (a, b) = (fs.source_mut().stats(), stream.stats());
        let stats = IoStats {
            reads: a.reads + b.reads,
            bytes: a.bytes + b.bytes,
            rereads: a.rereads + b.rereads,
            reread_bytes: a.reread_bytes + b.reread_bytes,
        };
        (pass, stats, times)
    }

    fn env_or(name: &str, default: u64) -> u64 {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    /// Times both parsers over the same large image, on one thread and on the
    /// full pool. `NOMNOM_BENCH_RECORDS` (default 1,000,000) slots in
    /// `NOMNOM_BENCH_FRAGMENTS` (default 3) table fragments.
    /// `cargo test -p nomnom-core --release --lib mft_pass_time -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, not a check"]
    fn mft_pass_time_one_thread_vs_pool() {
        let n = env_or("NOMNOM_BENCH_RECORDS", 1_000_000);
        let fragments = env_or("NOMNOM_BENCH_FRAGMENTS", 3);
        let image = test_image::build_fragmented(n, fragments);
        let pool =
            |threads: usize| rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let timed = |threads: usize| {
            let started = Instant::now();
            let (pass, _, times) = pool(threads).install(|| timed_pass(&image, SECTOR));
            (started.elapsed(), pass.records.len(), times)
        };
        let (one, records, one_times) = timed(1);
        let (all, same, all_times) = timed(0);
        assert_eq!(records, same);
        {
            let (pass, _, _) = timed_pass(&image, SECTOR);
            let started = Instant::now();
            let report = table_of(pass);
            let built = started.elapsed();
            let stats = link_stats(&report.table);
            let rows = report.table.nodes.len();
            let started = Instant::now();
            let catalog = crate::catalog::Catalog::build(report);
            eprintln!(
                "table build: {built:.2?} for {rows} rows; Catalog::build {:.2?} for {} nodes\n\
                 names per blob: {stats:?}",
                started.elapsed(),
                catalog.len()
            );
        }
        let started = Instant::now();
        let reference = reference_pass(&image, 4096);
        let oracle = started.elapsed();
        assert_eq!(reference.records.len(), records);
        let per = |d: Duration| d.as_nanos() as f64 / n as f64;
        eprintln!(
            "{n} slots in {fragments} fragments, {records} records, {} MiB table\n\
             hand-rolled pass, 1 thread: {one:.2?} ({:.0} ns/slot) {one_times:?}\n\
             hand-rolled pass, {} threads: {all:.2?} ({:.0} ns/slot) {all_times:?}\n\
             ntfs-crate per-record pass, 1 thread: {oracle:.2?} ({:.0} ns/slot)",
            image.mft_bytes >> 20,
            per(one),
            rayon::current_num_threads(),
            per(all),
            per(oracle),
        );
    }

    fn table_of(pass: RecordPass) -> ScanReport {
        let target =
            VolumeTarget { device: "x".into(), mount: PathBuf::from("C:\\"), sector: SECTOR };
        build_table(pass, &target, Path::new("C:\\"), Path::new("C:\\"))
    }

    /// How many blobs have each count of names in the table.
    fn link_stats(table: &ScanTable) -> std::collections::BTreeMap<usize, usize> {
        let mut names = vec![0usize; table.blobs.len()];
        for row in &table.nodes {
            if let Some(count) = names.get_mut(row.blob as usize) {
                *count += 1;
            }
        }
        let mut out = std::collections::BTreeMap::new();
        for count in names {
            *out.entry(count).or_default() += 1;
        }
        out
    }

    /// Regression: the table dropping a hard link's second name (what the old
    /// path build did: one entry per file, at its first name that resolved),
    /// counting its bytes twice in a directory total, or leaving rows the
    /// catalog cannot reach from the root on an undamaged table.
    #[test]
    fn table_files_every_name_and_counts_a_hard_link_once() {
        use crate::catalog::Catalog;

        let image = build(3000);
        let (pass, _) = streamed_pass(&image, SECTOR);
        let pass_errors = pass.errors.len();
        let file_bytes: u64 = pass.records.iter().filter(|r| !r.is_dir).map(|r| r.size).sum();
        let report = table_of(pass);
        let stats = link_stats(&report.table);
        assert!(stats.get(&2).is_some_and(|&n| n > 50), "hard links in the image: {stats:?}");

        let catalog = Catalog::build(report);
        assert_eq!(catalog.errors().len(), pass_errors, "{:?}", catalog.errors().last());
        let root = catalog.node(catalog.root());
        assert_eq!(root.subtree_size, file_bytes, "every file's bytes, each counted once");

        let group = catalog.link_groups().next().expect("a hard-linked file");
        assert!(group.complete && group.nodes.len() == 2);
        let extra = group.nodes.iter().find(|&&id| id != group.primary).unwrap();
        assert!(catalog.node(*extra).extra_link);
        assert_eq!(catalog.node(*extra).size, group.bytes);
        let paths: Vec<PathBuf> = group.nodes.iter().map(|&id| catalog.path(id)).collect();
        assert!(paths.iter().any(|p| p.parent() == Some(Path::new("C:\\"))), "{paths:?}");
    }

    /// The record numbers the errors name, which is what both parsers have to
    /// agree on; the wording of a corrupt-record message is each parser's own.
    fn error_records(errors: &[ScanError]) -> Vec<String> {
        errors.iter().map(|e| e.message.split(':').next().unwrap_or_default().to_string()).collect()
    }

    /// Regression: the hand-rolled parser disagreeing with the `ntfs` crate
    /// about any record — a wrong fixup, a misread `$FILE_NAME` namespace or
    /// parent, a `$DATA` size or sparse-run allocation, a directory flag, an
    /// extension record not merged into its base, a wrong fragment mapping or
    /// chunk boundary — which would silently change entries, sizes or errors.
    /// The `ntfs` crate's per-record reading is the oracle.
    #[test]
    fn streamed_pass_matches_per_record_reads_on_a_fragmented_table() {
        let image = build(3000);
        let mut want = reference_pass(&image, 4096);
        // The one known disagreement, and it is the oracle's: `ntfs` hands a
        // `$DATA` reached through an attribute list over as a value with no
        // run list, so the old pass reported 0 allocated bytes for every such
        // file. Record 31's `$DATA` lives in an extension record and has three
        // real clusters.
        let r31 = want.records.iter_mut().find(|r| r.number == 31).unwrap();
        assert_eq!(r31.allocated, 0, "the oracle still has its blind spot");
        r31.allocated = 3 * test_image::CLUSTER;

        // The image really holds every case this is meant to cover.
        let records = &want.records;
        let kinds = |kind| records.iter().filter(|r| r.kind == kind).count();
        assert!(kinds(EntryKind::Dir) > 50 && kinds(EntryKind::File) > 1000);
        assert!(records.iter().any(|r| r.kind == EntryKind::Symlink && r.is_dir), "junction");
        assert!(records.iter().any(|r| r.kind == EntryKind::Symlink && !r.is_dir), "symlink");
        assert!(records.iter().any(|r| r.names.len() == 2), "hard link");
        assert!(records.iter().any(|r| r.names[0].1.contains('~')), "8.3-only name");
        assert!(records.iter().any(|r| r.allocated == 3 * test_image::CLUSTER), "sparse run");
        assert!(records.iter().any(|r| r.number == 31 && r.size == 3 * test_image::CLUSTER - 10));
        assert!(records.iter().all(|r| r.modified.is_some() && r.accessed.is_some()));
        assert!(!want.errors.is_empty(), "corrupt records");

        // 4096 forces stream chunks to start up to three records early.
        for sector in [SECTOR, 4096] {
            let (pass, _) = streamed_pass(&image, sector);
            let got = raw_records(&pass);
            // The first differing record, not a dump of three thousand.
            let diff = got.iter().zip(&want.records).find(|(g, w)| g != w);
            assert_eq!(diff, None, "sector {sector}");
            assert_eq!(got.len(), want.records.len(), "sector {sector}");
            assert_eq!(error_records(&pass.errors), error_records(&want.errors), "sector {sector}");
        }
    }

    /// Regression: the C: scan that read a 1 MiB block twice per record — 17x
    /// the table and counting — because `Ntfs::file` reads record 0 before
    /// every record and the two evicted each other from a one-block cache.
    #[test]
    fn mft_pass_reads_the_table_about_once_not_once_per_record() {
        let image = build(20_000);
        let (pass, stats) = streamed_pass(&image, SECTOR);
        assert!(pass.records.len() > 15_000);

        let budget = image.mft_bytes + image.mft_bytes / 10 + (256 << 10);
        assert!(
            stats.bytes <= budget,
            "read {} bytes ({} reads, {} re-reads) for a {}-byte table; budget {budget}",
            stats.bytes,
            stats.reads,
            stats.rereads,
            image.mft_bytes,
        );
        // Bytes alone pass with a small block read once per few records, which
        // caps the scan at the device's ops/s instead. The stream needs a
        // handful of reads; the rest are the image's ~175 non-resident reparse
        // values at one scattered read each.
        assert!(stats.reads <= 400, "{} device reads for 20,000 records", stats.reads);
    }
}
