//! End-to-end tests against the real binary. The commands' behaviour is tested
//! inside the crate on fixture catalogs, because the binary only scans whole
//! drives; what is left here is the edge only the binary has.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_nomnom");

/// The regression: a folder argument slipping past the drive-only rule and
/// scanning anyway. It must be refused before any scan, with a nonzero exit
/// and an error that names the path and says what to pass instead.
#[test]
fn a_folder_argument_is_refused_with_the_drive_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let folder = dir.path().to_str().unwrap();
    for command in ["scan", "suggest", "clean"] {
        let output = Command::new(BIN).args([command, folder]).output().expect("run nomnom");
        assert!(!output.status.success(), "`{command} {folder}` succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not a volume root"), "{command}: {stderr}");
        assert!(stderr.contains(folder), "{command}: the error must name the path:\n{stderr}");
    }
}
