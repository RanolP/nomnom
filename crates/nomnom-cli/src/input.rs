//! Turning a drive plus flags into a [`Catalog`], and telling the user the two
//! things a scan can quietly get wrong: which backend actually ran, and how
//! many entries it could not read.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{BackendUsed, ScanProgress, Stage, VolumeRoot, scan_drive, volumes};
use nomnom_core::timings;
use nomnom_core::verdict::{Assessment, TrustedPack, assess_with};

/// Set to `1`, the progress meter draws even when stderr is not a terminal, so
/// its output can be captured and read.
pub const FORCE_METER_ENV: &str = "NOMNOM_FORCE_METER";

/// Scans through core's [`scan_drive`], the GUI's scan too: MFT behind one UAC
/// prompt when needed, the walk when that is declined, which [`warn_backend`]
/// then prints.
pub fn load(drive: &VolumeRoot) -> Result<Catalog> {
    let progress = ScanProgress::planned(&[Stage::Link, Stage::RollUp]);
    let (catalog, _meter) = scan_and_build(drive, progress)?;
    Ok(catalog)
}

/// [`load`], then the assessment, under one progress meter that runs from
/// the scan's start to the judged result: the GUI's bar, on a terminal.
pub fn load_assessed(
    drive: &VolumeRoot,
    packs: Vec<TrustedPack>,
) -> Result<(Catalog, Assessment)> {
    let progress = ScanProgress::planned(&[
        Stage::Link,
        Stage::RollUp,
        Stage::Index,
        Stage::Match,
        Stage::Group,
    ]);
    let (catalog, meter) = scan_and_build(drive, progress)?;
    let assessment = assess_with(&catalog, packs, Some(&meter.progress));
    drop(meter);
    Ok((catalog, assessment))
}

fn scan_and_build(drive: &VolumeRoot, progress: ScanProgress) -> Result<(Catalog, Meter)> {
    let volume = volumes().into_iter().find(|volume| volume.root == drive.as_path());
    let progress = Arc::new(progress);
    let used = volume.as_ref().map_or(0, |volume| volume.total.saturating_sub(volume.free));

    timings::start_scan("cli");
    let started = Instant::now();
    let meter = Meter::start(Arc::clone(&progress), used);
    let report = scan_drive(drive, Some(progress)).with_context(|| format!("cannot scan {drive}"))?;
    let catalog_started = timings::lap("cli start -> report in hand", started);
    let catalog = Catalog::build_with(report, Some(&meter.progress));
    timings::lap("Catalog::build total", catalog_started);
    Ok((catalog, meter))
}

/// A scan of a whole drive takes long enough that silence reads as a hang,
/// so a terminal gets one rewritten line: percent, stage, entries, elapsed.
/// The percent is the core's rule over every stage of the run, shared with
/// the GUI's progress bar. The line is cleared when the meter drops, which
/// also covers a scan that failed.
struct Meter {
    progress: Arc<ScanProgress>,
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Meter {
    const WIDTH: usize = 64;

    fn start(progress: Arc<ScanProgress>, used_bytes: u64) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let forced = std::env::var_os(FORCE_METER_ENV).is_some_and(|v| v == "1");
        let thread = (forced || std::io::stderr().is_terminal()).then(|| {
            let (done, progress) = (Arc::clone(&done), Arc::clone(&progress));
            thread::spawn(move || {
                let started = Instant::now();
                while !done.load(Ordering::Relaxed) {
                    let line = Self::line(&progress, used_bytes, started.elapsed());
                    eprint!("\r{line:<width$}", width = Self::WIDTH);
                    let _ = std::io::stderr().flush();
                    thread::sleep(Duration::from_millis(200));
                }
                eprint!("\r{:width$}\r", "", width = Self::WIDTH);
            })
        });
        Self { progress, done, thread }
    }

    fn line(progress: &ScanProgress, used_bytes: u64, elapsed: Duration) -> String {
        let percent = progress
            .overall(used_bytes)
            .map_or_else(|| "  …".to_string(), |f| format!("{:3.0}%", f * 100.0));
        format!(
            "{percent}  {}  {} entries  {:.1}s",
            progress.stage().label(),
            progress.entries.load(Ordering::Relaxed),
            elapsed.as_secs_f64()
        )
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A user who does not know the MFT path needs Administrator just experiences
/// nomnom as slow. This line is the difference.
pub fn warn_backend(catalog: &Catalog) {
    if let BackendUsed::Walk { mft_unavailable: Some(reason) } = catalog.backend_used() {
        eprintln!("note: the fast MFT scan was skipped ({reason}); walked the drive instead.");
        eprintln!("      accept the Administrator prompt for the fast path.");
    }
}

/// A cleanup tool that silently skipped unreadable directories has lied about
/// its totals, so the count is never optional — only the detail is.
pub fn report_errors(catalog: &Catalog, show: bool) {
    let errors = catalog.errors();
    if errors.is_empty() {
        return;
    }
    eprintln!("note: {} entries could not be read; totals are under-counted", errors.len());
    if show {
        for error in errors {
            match &error.path {
                Some(path) => eprintln!("  {}: {}", path.display(), error.message),
                None => eprintln!("  {}", error.message),
            }
        }
    } else {
        eprintln!("      re-run with --show-errors to list them.");
    }
}
