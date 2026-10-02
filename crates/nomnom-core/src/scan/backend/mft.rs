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
pub mod volume;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ntfs::attribute_value::NtfsAttributeValue;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{Ntfs, NtfsAttributeType, NtfsFileFlags, NtfsTime};

use crate::scan::{BackendUsed, Entry, EntryKind, ScanError, ScanFailure, ScanOptions, ScanReport};
use paths::{DirRecord, PathBuilder, ROOT_RECORD, respell_under};
use volume::{AlignedReader, VolumeSource};

/// Records 0–15 are the NTFS metafiles (`$MFT`, `$LogFile`, `$Bitmap`, …).
/// They are filesystem plumbing, not user data, and never appear in a scan.
const FIRST_USER_RECORD: u64 = 16;

/// Read-ahead block. MFT enumeration is front-to-back, so a large block turns
/// ~1000 record reads into one device read.
const READ_BLOCK: usize = 1 << 20;

/// Upper bound on collected per-record errors. A pathologically damaged volume
/// must not turn a scan into an out-of-memory error report.
const MAX_ERRORS: usize = 1024;

pub(crate) fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    // `respect_gitignore` and `follow_symlinks` describe a tree walk. The MFT
    // sees the volume, not a repository, and never traverses a link. Only
    // `progress` applies here.

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

    let mut fs = AlignedReader::new(source, target.sector, READ_BLOCK);
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

    enumerate(&ntfs, &mut fs, &target, root, &root_canon, opts)
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Everything one record contributes, held until every directory is known and
/// paths can be resolved. A file's parent may sit later in the table than the
/// file itself, so paths cannot be built during the pass that reads records.
struct RawRecord {
    number: u64,
    kind: EntryKind,
    size: u64,
    allocated: u64,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    /// One `(parent, name)` per `$FILE_NAME`. More than one means a hard link.
    names: Vec<(u64, String)>,
}

fn enumerate(
    ntfs: &Ntfs,
    fs: &mut AlignedReader<VolumeSource>,
    target: &VolumeTarget,
    root: &Path,
    root_canon: &Path,
    opts: &ScanOptions,
) -> Result<ScanReport, ScanFailure> {
    let total = mft_record_count(ntfs, fs)?;

    let mut errors: Vec<ScanError> = Vec::new();
    let mut dirs: HashMap<u64, DirRecord> = HashMap::new();
    let mut records: Vec<RawRecord> = Vec::new();

    for number in FIRST_USER_RECORD..total {
        let file = match ntfs.file(fs, number) {
            Ok(file) => file,
            Err(err) => {
                push_error(&mut errors, format!("MFT record {number}: {err}"));
                continue;
            }
        };
        // A record not in use is a deleted file whose slot has not been reused.
        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            continue;
        }
        opts.tick();

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
                    if let Ok(value) = attribute.structured_value::<_, NtfsStandardInformation>(fs)
                    {
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
            continue;
        }

        if is_dir {
            // Directories occupy index clusters, but that space is filesystem
            // bookkeeping rather than any file's content, and charging it here
            // would double-count in a size roll-up.
            size = 0;
            allocated = 0;
            dirs.insert(number, DirRecord { name: names[0].1.clone(), parent: names[0].0 });
        }

        records.push(RawRecord {
            number,
            kind: classify(is_dir, reparse_tag),
            size,
            allocated,
            modified: info.as_ref().and_then(|i| to_system_time(i.modification_time())),
            accessed: info.as_ref().and_then(|i| to_system_time(i.access_time())),
            names,
        });
    }

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

    Ok(ScanReport { root: root.to_path_buf(), entries, errors, backend_used: BackendUsed::Mft })
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

/// How many file records the table holds, read off `$MFT`'s own `$DATA` length.
fn mft_record_count(ntfs: &Ntfs, fs: &mut AlignedReader<VolumeSource>) -> Result<u64, ScanFailure> {
    let mft = ntfs
        .file(fs, 0)
        .map_err(|err| ScanFailure::MftUnavailable(format!("reading $MFT failed: {err}")))?;
    let data = mft
        .data(fs, "")
        .ok_or_else(|| ScanFailure::MftUnavailable("$MFT has no $DATA attribute".into()))?
        .map_err(|err| ScanFailure::MftUnavailable(format!("reading $MFT $DATA failed: {err}")))?;
    let attribute = data
        .to_attribute()
        .map_err(|err| ScanFailure::MftUnavailable(format!("parsing $MFT $DATA failed: {err}")))?;
    let record_size = u64::from(ntfs.file_record_size());
    if record_size == 0 {
        return Err(ScanFailure::MftUnavailable("boot sector reports a zero record size".into()));
    }
    Ok(attribute.value_length() / record_size)
}

/// On-disk bytes of a `$DATA` attribute.
///
/// Sparse runs are skipped, which is exactly what makes this worth reading: a
/// sparse or compressed file's allocation is the runs that actually have
/// clusters behind them, not its logical length. A resident value lives inside
/// the MFT record and occupies no clusters of its own.
fn allocated_bytes(
    attribute: &ntfs::NtfsAttribute<'_, '_>,
    fs: &mut AlignedReader<VolumeSource>,
) -> u64 {
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
fn read_reparse_tag(
    attribute: &ntfs::NtfsAttribute<'_, '_>,
    fs: &mut AlignedReader<VolumeSource>,
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

fn wide(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buffer: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    PathBuf::from(OsString::from_wide(&buffer[..len]))
}
