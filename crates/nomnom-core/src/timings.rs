//! Phase timings of one whole-drive scan, so a slow scan is read off measured
//! numbers instead of guessed at.
//!
//! A front-end calls [`start_scan`] when the user starts a scan; that truncates
//! `%LOCALAPPDATA%\nomnom\last-scan-timings.txt` and turns recording on for the
//! process. The elevated helper calls [`join_scan`], which appends to the same
//! file, so one GUI click leaves the helper's phases and the parent's phases in
//! one file, in the order they finished. With `NOMNOM_TIMINGS=1` every line is
//! also printed to stderr.
//!
//! Recording is off until one of the two is called, so library users and tests
//! never touch the file.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Set to `1` to also print every phase line to stderr.
pub const ENV: &str = "NOMNOM_TIMINGS";

const FILE_NAME: &str = "last-scan-timings.txt";

/// The role recorded on every line, `None` while recording is off.
static ROLE: Mutex<Option<&'static str>> = Mutex::new(None);

/// Begins a fresh timing file for the scan this process is about to run.
pub fn start_scan(role: &'static str) {
    enable(role);
    let path = file_path();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let header = format!("# nomnom scan timings, unix time {stamp}\n");
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let _ = std::fs::write(&path, header);
}

/// Records into the timing file a parent process already started.
pub fn join_scan(role: &'static str) {
    enable(role);
}

fn enable(role: &'static str) {
    *ROLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(role);
}

/// Appends one phase line; a no-op while recording is off.
pub fn record(phase: &str, took: Duration) {
    let Some(role) = *ROLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) else {
        return;
    };
    let line = format!("{role:<7} {phase:<44} {:>10.1} ms\n", took.as_secs_f64() * 1000.0);
    if std::env::var_os(ENV).is_some_and(|v| v == "1") {
        eprint!("{line}");
    }
    let path = file_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Records the phase that ran from `since` until now, and returns now so the
/// next phase can start from it.
pub fn lap(phase: &str, since: Instant) -> Instant {
    let now = Instant::now();
    record(phase, now - since);
    now
}

/// `%LOCALAPPDATA%\nomnom\last-scan-timings.txt`, or the temp directory when
/// that variable is missing.
pub fn file_path() -> PathBuf {
    let base =
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("nomnom").join(FILE_NAME)
}
