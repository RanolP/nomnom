//! Turning a drive plus flags into a [`Catalog`], and telling the user the two
//! things a scan can quietly get wrong: which backend actually ran, and how
//! many entries it could not read.

use anyhow::{Context, Result};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{Backend, BackendUsed, ScanOptions, VolumeRoot, scan};

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
    let opts = ScanOptions { backend: backend.into(), ..ScanOptions::default() };
    let report = scan(drive, &opts).with_context(|| format!("cannot scan {drive}"))?;
    Ok(Catalog::build(report))
}

/// A user who does not know the MFT path needs Administrator just experiences
/// nomnom as slow. This line is the difference.
pub fn warn_backend(catalog: &Catalog) {
    if let BackendUsed::Walk { mft_unavailable: Some(reason) } = catalog.backend_used() {
        eprintln!("note: the fast MFT scan was skipped ({reason})");
        eprintln!("      re-run in an Administrator shell for the fast path.");
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
