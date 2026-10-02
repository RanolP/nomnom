//! Which drives there are to scan, for a front-end's drive picker.

use std::path::PathBuf;

/// A fixed drive and its capacity. Sizes are in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Drive root as a scan accepts it, e.g. `C:\`.
    pub root: PathBuf,
    /// Volume label; empty when the volume has none or it could not be read.
    pub label: String,
    /// Filesystem name, e.g. `NTFS`; empty when it could not be read.
    pub fs: String,
    pub total: u64,
    pub free: u64,
}

/// Every fixed drive whose capacity can be read, in drive-letter order.
/// Removable, network and optical drives are left out, as is a drive that
/// does not answer (a locked BitLocker volume, say). Empty off Windows.
#[cfg(windows)]
pub fn volumes() -> Vec<Volume> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    };

    use crate::scan::backend::mft::{from_wide, wide};

    const DRIVE_FIXED: u32 = 3;

    // "C:\<NUL>D:\<NUL>...<NUL>"; 26 letters of 4 units each fit comfortably.
    let mut buffer = [0u16; 256];
    // SAFETY: `buffer` is writable for the element count passed alongside it.
    let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) } as usize;
    if len == 0 || len > buffer.len() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for root in buffer[..len].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let root = from_wide(root);
        let root_w = wide(root.as_os_str());

        // SAFETY: `root_w` is a NUL-terminated drive root.
        if unsafe { GetDriveTypeW(root_w.as_ptr()) } != DRIVE_FIXED {
            continue;
        }

        let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: all three out parameters are live locals of the expected type.
        let ok =
            unsafe { GetDiskFreeSpaceExW(root_w.as_ptr(), &mut available, &mut total, &mut free) };
        if ok == 0 {
            continue;
        }

        let mut label = [0u16; 261];
        let mut fs = [0u16; 64];
        let (mut serial, mut max_component, mut flags) = (0u32, 0u32, 0u32);
        // SAFETY: `label` and `fs` are written for at most the element counts
        // passed with them; the other out parameters are live locals.
        let ok = unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                &mut max_component,
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        let text = |buffer: &[u16]| {
            if ok == 0 { String::new() } else { from_wide(buffer).to_string_lossy().into_owned() }
        };

        out.push(Volume { label: text(&label), fs: text(&fs), root, total, free });
    }
    out
}

#[cfg(not(windows))]
pub fn volumes() -> Vec<Volume> {
    Vec::new()
}
