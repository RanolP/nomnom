//! Turning a drive plus flags into a [`Catalog`], and telling the user the two
//! things a scan can quietly get wrong: which backend actually ran, and how
//! many entries it could not read.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::elevated::{ElevatedScanError, scan_elevated};
use nomnom_core::scan::{
    Backend, BackendUsed, ScanOptions, ScanProgress, ScanReport, VolumeRoot, is_elevated, scan,
    volumes,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BackendArg {
    #[default]
    Auto,
    Mft,
    Walk,
}

impl From<BackendArg> for Backend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Auto => Backend::Auto,
            BackendArg::Mft => Backend::Mft,
            BackendArg::Walk => Backend::Walk,
        }
    }
}

pub fn load(drive: &VolumeRoot, backend: BackendArg) -> Result<Catalog> {
    let volume = volumes().into_iter().find(|volume| volume.root == drive.as_path());
    let progress = Arc::new(ScanProgress::default());
    let opts = ScanOptions {
        backend: backend.into(),
        progress: Some(Arc::clone(&progress)),
        ..ScanOptions::default()
    };
    let used = volume.as_ref().map_or(0, |volume| volume.total.saturating_sub(volume.free));
    let ntfs = volume.as_ref().is_none_or(|volume| volume.fs.eq_ignore_ascii_case("NTFS"));

    let meter = Meter::start(progress, used);
    let report = scan_drive(drive, opts, ntfs);
    meter.stop();
    Ok(Catalog::build(report?))
}

/// The MFT read is always offered, as the GUI offers it: in-process when
/// already elevated, otherwise through one UAC prompt. A declined or failed
/// prompt walks instead and records why, which [`warn_backend`] prints.
/// `--backend mft` fails rather than falling back; `--backend walk` never
/// prompts.
fn scan_drive(drive: &VolumeRoot, opts: ScanOptions, ntfs: bool) -> Result<ScanReport> {
    let cannot = || format!("cannot scan {drive}");
    if opts.backend == Backend::Walk || !ntfs || is_elevated() {
        return scan(drive, &opts).with_context(cannot);
    }
    let reason = match scan_elevated(drive, &opts) {
        Ok(report) => return Ok(report),
        Err(ElevatedScanError::Declined) => "Administrator access was declined".to_string(),
        Err(ElevatedScanError::Failed(message)) => format!("the elevated scan failed: {message}"),
    };
    if opts.backend == Backend::Mft {
        return Err(anyhow!("MFT backend unavailable: {reason}")).with_context(cannot);
    }
    // The helper may have counted part of the MFT before it stopped.
    if let Some(progress) = &opts.progress {
        for counter in [&progress.entries, &progress.entries_total, &progress.bytes] {
            counter.store(0, Ordering::Relaxed);
        }
    }
    let walk = ScanOptions { backend: Backend::Walk, ..opts };
    let mut report = scan(drive, &walk).with_context(cannot)?;
    report.backend_used = BackendUsed::Walk { mft_unavailable: Some(reason) };
    Ok(report)
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
