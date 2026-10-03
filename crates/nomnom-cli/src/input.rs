//! Turning a drive plus flags into a [`Catalog`], and telling the user the two
//! things a scan can quietly get wrong: which backend actually ran, and how
//! many entries it could not read.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nomnom_core::catalog::{Catalog, DuplicateProgress};
use nomnom_core::scan::{BackendUsed, ScanProgress, VolumeRoot, scan_drive, volumes};
use nomnom_core::timings;
use nomnom_core::verdict::{Assessment, find_duplicates};

/// Scans through core's [`scan_drive`], the GUI's scan too: MFT behind one UAC
/// prompt when needed, the walk when that is declined, which [`warn_backend`]
/// then prints.
pub fn load(drive: &VolumeRoot) -> Result<Catalog> {
    let volume = volumes().into_iter().find(|volume| volume.root == drive.as_path());
    let progress = Arc::new(ScanProgress::default());
    let used = volume.as_ref().map_or(0, |volume| volume.total.saturating_sub(volume.free));

    timings::start_scan("cli");
    let started = Instant::now();
    let meter = Meter::start(Arc::clone(&progress), used);
    let report = scan_drive(drive, Some(progress));
    meter.stop();
    let report = report.with_context(|| format!("cannot scan {drive}"))?;
    let catalog_started = timings::lap("cli start -> report in hand", started);
    let catalog = Catalog::build(report);
    timings::lap("Catalog::build total", catalog_started);
    Ok(catalog)
}

/// A scan of a whole drive takes long enough that silence reads as a hang,
/// so a terminal gets one rewritten line: percent, entries, elapsed. The
/// percent is the core's rule, shared with the GUI's progress bar.
struct Meter {
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Meter {
    const WIDTH: usize = 60;

    fn start(progress: Arc<ScanProgress>, used_bytes: u64) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let thread = std::io::stderr().is_terminal().then(|| {
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let started = Instant::now();
                while !done.load(Ordering::Relaxed) {
                    let percent = progress
                        .fraction(used_bytes)
                        .map_or_else(|| "  …".to_string(), |f| format!("{:3.0}%", f * 100.0));
                    let line = format!(
                        "{percent}  {} entries  {:.1}s",
                        progress.entries.load(Ordering::Relaxed),
                        started.elapsed().as_secs_f64()
                    );
                    eprint!("\r{line:<width$}", width = Self::WIDTH);
                    let _ = std::io::stderr().flush();
                    thread::sleep(Duration::from_millis(200));
                }
                eprint!("\r{:width$}\r", "", width = Self::WIDTH);
            })
        });
        Self { done, thread }
    }

    fn stop(mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The second phase, after the rules' answer is already printed: core's
/// [`find_duplicates`], merged into `rules`. A terminal gets the same
/// "Finding duplicates…" line, files done of total, that the GUI shows.
pub fn with_duplicates(catalog: &Catalog, rules: &Assessment) -> Assessment {
    let progress = Arc::new(DuplicateProgress::default());
    let done = Arc::new(AtomicBool::new(false));
    let meter = std::io::stderr().is_terminal().then(|| {
        let (progress, done) = (Arc::clone(&progress), Arc::clone(&done));
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let line = progress.status();
                eprint!("\r{line:<width$}", width = Meter::WIDTH);
                let _ = std::io::stderr().flush();
                thread::sleep(Duration::from_millis(200));
            }
            eprint!("\r{:width$}\r", "", width = Meter::WIDTH);
        })
    });
    let found = find_duplicates(catalog, rules, &progress);
    done.store(true, Ordering::Relaxed);
    if let Some(meter) = meter {
        let _ = meter.join();
    }
    let found = found.expect("the CLI never cancels the duplicate pass");
    rules.with_duplicates(catalog, &found)
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
