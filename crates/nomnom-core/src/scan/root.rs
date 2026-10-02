//! The one thing nomnom scans: a whole volume.
//!
//! Scanning is drive-level only. That is engine policy, not a front-end
//! choice, so it lives in the type every scan entry point takes: a
//! [`VolumeRoot`] can only be built from a path that names a volume root, and
//! no scan accepts a bare path.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::scan::ScanFailure;

/// A volume root, normalised to one spelling: `C:\` on Windows, `/` elsewhere.
///
/// `C:`, `c:/` and `\\?\C:\` all parse to `C:\`. A folder, a relative path or
/// a network share is rejected with [`ScanFailure::NotAVolumeRoot`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VolumeRoot(PathBuf);

impl VolumeRoot {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ScanFailure> {
        let path = path.as_ref();
        normalise(path).map(Self).ok_or_else(|| ScanFailure::NotAVolumeRoot(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(windows)]
fn normalise(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let letter = match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return None,
        },
        _ => return None,
    };
    match (components.next(), components.next()) {
        (None, None) | (Some(Component::RootDir), None) => {}
        _ => return None,
    }
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(PathBuf::from(format!("{}:\\", letter.to_ascii_uppercase() as char)))
}

#[cfg(not(windows))]
fn normalise(path: &Path) -> Option<PathBuf> {
    (path == Path::new("/")).then(|| PathBuf::from("/"))
}

impl AsRef<Path> for VolumeRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for VolumeRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl FromStr for VolumeRoot {
    type Err = ScanFailure;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Catches the policy leaking: a folder, a share or a relative path must
    /// never become a scan root, and every spelling of a drive must agree.
    #[test]
    fn only_drive_roots_parse_and_all_spellings_agree() {
        for ok in ["C:", "C:\\", "c:/", "\\\\?\\C:\\"] {
            assert_eq!(VolumeRoot::new(ok).unwrap().as_path(), Path::new("C:\\"), "{ok}");
        }
        for bad in
            ["C:\\Users", "C:\\Users\\", "src", ".", "\\\\server\\share", "\\\\?\\UNC\\s\\x", ""]
        {
            assert!(
                matches!(VolumeRoot::new(bad), Err(ScanFailure::NotAVolumeRoot(p)) if p == Path::new(bad)),
                "{bad:?} parsed as a volume root"
            );
        }
    }
}
