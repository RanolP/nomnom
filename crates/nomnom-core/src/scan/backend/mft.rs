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

pub mod paths;
pub mod stream;
pub mod volume;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ntfs::attribute_value::NtfsAttributeValue;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{Ntfs, NtfsAttributeType, NtfsFileFlags, NtfsTime};

use crate::scan::{BackendUsed, Entry, EntryKind, ScanError, ScanFailure, ScanOptions, ScanReport};
use crate::timings;
use paths::{DirRecord, PathBuilder, ROOT_RECORD, respell_under};
use stream::{Chunk, ChunkReader, MftLayout, SharedVolume};
use volume::{AlignedReader, BlockSource, CountingSource, IoStats, VolumeSource};

/// Records 0–15 are the NTFS metafiles (`$MFT`, `$LogFile`, `$Bitmap`, …).
/// They are filesystem plumbing, not user data, and never appear in a scan.
const FIRST_USER_RECORD: u64 = 16;

/// Read size of the sequential pass over the table: 8192 records of 1 KiB
/// per device read.
const STREAM_CHUNK: usize = 8 << 20;

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

    let layout = MftLayout::read(&ntfs, &mut fs).map_err(ScanFailure::MftUnavailable)?;
    let started = timings::lap("MFT volume open", started);
    let mut volume = SharedVolume::new(fs, target.sector, STREAM_CHUNK);
    let pass = read_records(&ntfs, &mut volume, &layout, opts);
    if std::env::var_os(IO_STATS_ENV).is_some() {
        eprintln!("{}", io_summary(volume.source_mut().stats(), &layout));
    }
    let started = timings::lap("MFT IO + parse", started);

    let report = build_report(pass, &target, root, &root_canon);
    timings::lap("MFT path build", started);
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

/// Everything one record contributes, held until every directory is known and
/// paths can be resolved. A file's parent may sit later in the table than the
/// file itself, so paths cannot be built during the pass that reads records.
#[derive(Debug, PartialEq)]
struct RawRecord {
    number: u64,
    is_dir: bool,
    kind: EntryKind,
    size: u64,
    allocated: u64,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    /// One `(parent, name)` per `$FILE_NAME`. More than one means a hard link.
    names: Vec<(u64, String)>,
}

/// What the pass over the table collects, before any path is built.
#[derive(Debug, Default)]
struct RecordPass {
    records: Vec<RawRecord>,
    dirs: HashMap<u64, DirRecord>,
    errors: Vec<ScanError>,
}

impl RecordPass {
    fn add(&mut self, number: u64, record: Result<Option<RawRecord>, ntfs::NtfsError>) {
        match record {
            Ok(Some(record)) => {
                if record.is_dir {
                    let (parent, name) = &record.names[0];
                    self.dirs.insert(number, DirRecord { name: name.clone(), parent: *parent });
                }
                self.records.push(record);
            }
            Ok(None) => {}
            Err(err) => push_error(&mut self.errors, format!("MFT record {number}: {err}")),
        }
    }
}

/// Reads every user record in table order.
///
/// One thread reads the table forward a chunk at a time; each chunk's records
/// are parsed across the rayon pool while the next chunk is read, and folded
/// into the pass in table order, so the result is the sequential one.
fn read_records<S: BlockSource + Send>(
    ntfs: &Ntfs,
    volume: &mut SharedVolume<S>,
    layout: &MftLayout,
    opts: &ScanOptions,
) -> RecordPass {
    use rayon::prelude::*;

    /// Chunks read ahead of the parse: enough to keep the device busy while
    /// one chunk is parsed, few enough to bound memory at a few chunks.
    const READ_AHEAD: usize = 2;

    let total = layout.record_count();
    opts.set_entries_total(total.saturating_sub(FIRST_USER_RECORD));

    // `Ntfs::file` re-reads record 0 on every call to find the record it was
    // asked for. Pinned, no scattered read can evict it and turn the next
    // record into an extra device read.
    let record_size = u64::from(ntfs.file_record_size());
    if let Some(at) = ntfs.mft_position().value() {
        volume.pin(at.get(), record_size as usize);
    }
    let volume = &*volume;

    let mut pass = RecordPass::default();
    std::thread::scope(|scope| {
        let (send, batches) = std::sync::mpsc::sync_channel(READ_AHEAD);
        scope.spawn(move || {
            // A batch is every record from `first` up to the next one whose
            // bytes the current chunk does not hold. A record with no span on
            // the volume stays in the batch and reads through the scattered
            // reader.
            let mut chunk = Chunk::default();
            let mut first = FIRST_USER_RECORD;
            for number in FIRST_USER_RECORD..total {
                let Some(span) = layout.span(number) else { continue };
                if chunk.holds(span.phys, record_size) {
                    continue;
                }
                let next = volume.load_chunk(span);
                if number > first && send.send((first..number, chunk)).is_err() {
                    return;
                }
                (first, chunk) = (number, next);
            }
            if first < total {
                let _ = send.send((first..total, chunk));
            }
        });

        for (numbers, chunk) in batches {
            let parsed: Vec<_> = numbers
                .clone()
                .into_par_iter()
                .map(|number| read_record(ntfs, &mut ChunkReader::new(volume, &chunk), number))
                .collect();
            // Every record counts toward progress, free and unreadable ones
            // too, because `entries_total` is the size of the table, not of
            // its live set.
            if let Some(progress) = &opts.progress {
                progress.entries.fetch_add(parsed.len() as u64, Ordering::Relaxed);
            }
            for (number, record) in numbers.zip(parsed) {
                pass.add(number, record);
            }
        }
    });
    pass
}

/// One record's contribution, `None` for a free slot or a nameless record.
fn read_record<T: Read + Seek>(
    ntfs: &Ntfs,
    fs: &mut T,
    number: u64,
) -> Result<Option<RawRecord>, ntfs::NtfsError> {
    let file = ntfs.file(fs, number)?;
    // A record not in use is a deleted file whose slot has not been reused.
    if !file.flags().contains(NtfsFileFlags::IN_USE) {
        return Ok(None);
    }

    let is_dir = file.is_directory();
    let mut names: Vec<(u64, String)> = Vec::new();
    let mut short_names: Vec<(u64, String)> = Vec::new();
    let mut size = 0u64;
    let mut allocated = 0u64;
    let mut info: Option<NtfsStandardInformation> = None;
    let mut reparse_tag: Option<u32> = None;

    // One pass over the attributes collects all four facts. Calling
    // `NtfsFile::name`, `::info` and `::data` instead would re-walk the
    // attribute list three times per record.
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(fs) {
        let Ok(item) = item else { continue };
        let Ok(attribute) = item.to_attribute() else { continue };
        let Ok(ty) = attribute.ty() else { continue };

        match ty {
            NtfsAttributeType::StandardInformation => {
                if let Ok(value) = attribute.structured_value::<_, NtfsStandardInformation>(fs) {
                    info = Some(value);
                }
            }
            NtfsAttributeType::FileName => {
                if let Ok(value) = attribute.structured_value::<_, NtfsFileName>(fs) {
                    let parent = value.parent_directory_reference().file_record_number();
                    let name = value.name().to_string_lossy();
                    // The 8.3 alias duplicates a long name that is also
                    // present; it is a fallback, never a preference.
                    if value.namespace() == NtfsFileNamespace::Dos {
                        short_names.push((parent, name));
                    } else {
                        names.push((parent, name));
                    }
                }
            }
            NtfsAttributeType::Data if attribute.name().is_ok_and(|n| n.is_empty()) => {
                size = attribute.value_length();
                allocated = allocated_bytes(&attribute, fs);
            }
            NtfsAttributeType::ReparsePoint => {
                reparse_tag = read_reparse_tag(&attribute, fs);
            }
            _ => {}
        }
    }

    if names.is_empty() {
        names = short_names;
    }
    if names.is_empty() {
        return Ok(None);
    }

    if is_dir {
        // Directories occupy index clusters, but that space is filesystem
        // bookkeeping rather than any file's content, and charging it here
        // would double-count in a size roll-up.
        size = 0;
        allocated = 0;
    }

    Ok(Some(RawRecord {
        number,
        is_dir,
        kind: classify(is_dir, reparse_tag),
        size,
        allocated,
        modified: info.as_ref().and_then(|i| to_system_time(i.modification_time())),
        accessed: info.as_ref().and_then(|i| to_system_time(i.access_time())),
        names,
    }))
}

fn build_report(
    pass: RecordPass,
    target: &VolumeTarget,
    root: &Path,
    root_canon: &Path,
) -> ScanReport {
    let RecordPass { records, dirs, mut errors } = pass;
    let mut builder = PathBuilder::new(&dirs, target.mount.clone(), ROOT_RECORD);
    let mut entries: Vec<Entry> = Vec::new();

    // The volume root has no user record of its own, so when it IS the scan
    // root it has to be added by hand — the walk backend emits its root too.
    if let Some(path) = respell_under(&target.mount, root_canon, root) {
        entries.push(Entry {
            path,
            kind: EntryKind::Dir,
            size: 0,
            allocated: Some(0),
            modified: None,
            accessed: None,
        });
    }

    for record in &records {
        if record.kind == EntryKind::Dir {
            match builder.dir_path(record.number) {
                Some(full) => {
                    if let Some(path) = respell_under(&full, root_canon, root) {
                        entries.push(to_entry(record, path));
                    }
                }
                None => unresolved(&mut errors, record),
            }
            continue;
        }

        // A hard-linked file has one name per directory it lives in. Exactly
        // one entry is emitted, so its bytes are counted once.
        let mut resolved_any = false;
        let mut emitted = false;
        for (parent, name) in &record.names {
            let Some(full) = builder.child_path(*parent, name) else { continue };
            resolved_any = true;
            if let Some(path) = respell_under(&full, root_canon, root) {
                entries.push(to_entry(record, path));
                emitted = true;
                break;
            }
        }
        if !emitted && !resolved_any {
            unresolved(&mut errors, record);
        }
    }

    ScanReport { root: root.to_path_buf(), entries, errors, backend_used: BackendUsed::Mft }
}

fn to_entry(record: &RawRecord, path: PathBuf) -> Entry {
    Entry {
        path,
        kind: record.kind,
        size: record.size,
        allocated: Some(record.allocated),
        modified: record.modified,
        accessed: record.accessed,
    }
}

fn unresolved(errors: &mut Vec<ScanError>, record: &RawRecord) {
    // Records filed under a metafile directory (`$Extend` and its children)
    // always dead-end here by design; reporting them would bury real breakage.
    if record.names.iter().all(|(parent, _)| *parent < FIRST_USER_RECORD) {
        return;
    }
    let parent = record.names[0].0;
    push_error(
        errors,
        format!(
            "MFT record {}: parent reference {parent} does not lead to the volume root",
            record.number
        ),
    );
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

/// On-disk bytes of a `$DATA` attribute.
///
/// Sparse runs are skipped, which is exactly what makes this worth reading: a
/// sparse or compressed file's allocation is the runs that actually have
/// clusters behind them, not its logical length. A resident value lives inside
/// the MFT record and occupies no clusters of its own.
fn allocated_bytes<T: Read + Seek>(attribute: &ntfs::NtfsAttribute<'_, '_>, fs: &mut T) -> u64 {
    match attribute.value(fs) {
        Ok(NtfsAttributeValue::NonResident(value)) => {
            let mut total = 0u64;
            for run in value.data_runs() {
                let Ok(run) = run else { break };
                if run.data_position().value().is_some() {
                    total = total.saturating_add(run.allocated_size());
                }
            }
            total
        }
        _ => 0,
    }
}

/// First 4 bytes of a `$REPARSE_POINT` value: the reparse tag.
fn read_reparse_tag<T: Read + Seek>(
    attribute: &ntfs::NtfsAttribute<'_, '_>,
    fs: &mut T,
) -> Option<u32> {
    use ntfs::NtfsReadSeek;

    let mut value = attribute.value(fs).ok()?;
    let mut tag = [0u8; 4];
    value.read_exact(fs, &mut tag).ok()?;
    Some(u32::from_le_bytes(tag))
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
fn to_system_time(time: NtfsTime) -> Option<SystemTime> {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_1601_TO_1970: u64 = 11_644_473_600;

    let ticks = time.nt_timestamp();
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

    /// The pass as it ran before streaming: one `Ntfs::file` per record over a
    /// single-block reader. Slow, but every byte is fetched where `ntfs` asks.
    fn reference_pass(image: &Image, block: usize) -> (RecordPass, IoStats) {
        let source = CountingSource::new(MemSource::new(image, SECTOR));
        let mut fs = AlignedReader::new(source, SECTOR, block);
        let ntfs = Ntfs::new(&mut fs).unwrap();
        let layout = MftLayout::read(&ntfs, &mut fs).unwrap();
        let mut pass = RecordPass::default();
        for number in FIRST_USER_RECORD..layout.record_count() {
            pass.add(number, read_record(&ntfs, &mut fs, number));
        }
        (pass, fs.source_mut().stats())
    }

    fn streamed_pass(image: &Image, sector: u64) -> (RecordPass, IoStats) {
        let source = CountingSource::new(MemSource::new(image, sector));
        let mut fs = AlignedReader::new(source, sector, SCATTERED_BLOCK);
        let ntfs = Ntfs::new(&mut fs).unwrap();
        let layout = MftLayout::read(&ntfs, &mut fs).unwrap();
        let mut volume = SharedVolume::new(fs, sector, STREAM_CHUNK);
        let pass = read_records(&ntfs, &mut volume, &layout, &ScanOptions::default());
        (pass, volume.source_mut().stats())
    }

    /// Times the pass on a large image on one thread and on the full pool.
    /// `cargo test -p nomnom-core --release --lib mft_pass_time -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, not a check"]
    fn mft_pass_time_one_thread_vs_pool() {
        let image = build(400_000);
        let timed = |threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            let started = std::time::Instant::now();
            let (pass, _) = pool.install(|| streamed_pass(&image, SECTOR));
            (started.elapsed(), pass.records.len())
        };
        let (one, records) = timed(1);
        let (all, same) = timed(0);
        assert_eq!(records, same);
        eprintln!(
            "{records} records of {} MiB: 1 thread {one:.2?}, {} threads {all:.2?}",
            image.mft_bytes >> 20,
            rayon::current_num_threads(),
        );
    }

    fn messages(pass: &RecordPass) -> Vec<&str> {
        pass.errors.iter().map(|e| e.message.as_str()).collect()
    }

    /// Regression: the streamed pass serving `ntfs` the wrong bytes — a wrong
    /// fragment mapping, a chunk boundary, a straddled record, a misaligned
    /// device read — which would silently change entries, sizes or errors.
    /// The per-record reader is the oracle.
    #[test]
    fn streamed_pass_matches_per_record_reads_on_a_fragmented_table() {
        let image = build(3000);
        let (want, _) = reference_pass(&image, 4096);

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
            let (got, _) = streamed_pass(&image, sector);
            assert_eq!(got.records, want.records, "sector {sector}");
            assert_eq!(got.dirs, want.dirs, "sector {sector}");
            assert_eq!(messages(&got), messages(&want), "sector {sector}");
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
