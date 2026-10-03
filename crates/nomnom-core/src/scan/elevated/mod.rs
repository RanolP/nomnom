//! The MFT read from an unelevated process: relaunch this same binary behind
//! a UAC prompt once, and have it serve every scan this process asks for over
//! a named pipe.
//!
//! The first scan creates a pipe with a random name that only this user and
//! Administrators can open, and starts `current_exe() --nomnom-elevated-scan
//! <pipe> <mft|walk> <parent pid> <token>` with the `runas` verb. Each side
//! checks the other's PID on the pipe, and the parent's first bytes must be
//! the random token. Each scan then sends a volume root and reads the answer
//! [`wire`] defines: progress frames it mirrors into
//! [`ScanOptions::progress`], then the report. The helper accepts nothing but
//! a volume root to scan; deleting stays in the unelevated process.
//!
//! The helper lives as long as its parent: it waits on the parent's process
//! handle and exits when it signals, and a broken pipe (the parent closed it,
//! or dropped a helper whose stream broke) ends it too. A helper that died is
//! relaunched by the next scan, which costs one new prompt and says so in
//! [`ScanProgress::relaunch`](crate::scan::ScanProgress::relaunch).
//!
//! Every binary that may call [`scan_elevated`] must call [`maybe_run_helper`]
//! first thing in `main`, because the helper is that binary relaunched.
//!
//! `NOMNOM_DEBUG_HELPER_WALK=1` launches the helper without `runas` and has it
//! walk instead of reading the MFT: the whole launch, pipe and wire path runs
//! with no UAC prompt, which is how it is tested.

use std::process::ExitCode;

use super::{ScanOptions, ScanReport, VolumeRoot};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod wire;

/// Why [`scan_elevated`] produced no report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElevatedScanError {
    /// The user dismissed the UAC prompt. Callers fall back to the walk.
    #[error("Administrator access was declined")]
    Declined,
    /// The helper could not start, could not scan, or broke the protocol.
    #[error("{0}")]
    Failed(String),
}

/// Scan `root` by reading its MFT with Administrator rights: in-process when
/// this process already has them, otherwise in a helper launched behind one
/// UAC prompt. Progress lands in `opts.progress` either way.
pub fn scan_elevated(
    root: &VolumeRoot,
    opts: &ScanOptions,
) -> Result<ScanReport, ElevatedScanError> {
    #[cfg(windows)]
    {
        windows::scan_elevated(root, opts)
    }
    #[cfg(not(windows))]
    {
        let _ = (root, opts);
        Err(ElevatedScanError::Failed("unsupported".into()))
    }
}

/// Whether this process already runs with Administrator rights, so the MFT is
/// readable in-process with no prompt.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Run as the elevated helper when this process was launched as one, and
/// return the exit code `main` must return; `None` for an ordinary launch.
pub fn maybe_run_helper() -> Option<ExitCode> {
    #[cfg(windows)]
    {
        windows::maybe_run_helper()
    }
    #[cfg(not(windows))]
    {
        None
    }
}
