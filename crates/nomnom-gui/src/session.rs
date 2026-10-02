//! The one shared piece of state: which drive is open, what the scan found,
//! and what the judge made of it. Scan runs once per drive; Suggest and Clean
//! read the same assessment instead of each scanning again.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::{Context, EventEmitter};
use nomnom_core::catalog::{Catalog, FileType, NodeId, file_types, largest_files};
use nomnom_core::scan::elevated::{ElevatedScanError, scan_elevated};
use nomnom_core::scan::{
    self, Backend, ScanFailure, ScanOptions, ScanReport, Volume, VolumeRoot, is_elevated,
};
use nomnom_core::verdict::{Assessment, assess, resolve_packs};

use crate::palette::Palette;

/// The long-running phase in flight. Only one runs at a time: every phase
/// either reads the catalog another would replace or moves files another
/// would read, so the UI disables conflicting actions while this is `Some`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Assessing,
    Applying,
    Undoing,
    Packs,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Scanning => "Scanning…",
            Phase::Assessing => "Judging what each path is…",
            Phase::Applying => "Applying the plan…",
            Phase::Undoing => "Undoing…",
            Phase::Packs => "Working on packs…",
        }
    }
}

/// How far a running scan has got.
pub struct ScanProgress {
    counters: Arc<scan::ScanProgress>,
    /// Bytes in use on the volume, the walk's yardstick: it knows no entry
    /// total up front, but it does sum file sizes as it goes.
    used_bytes: u64,
    /// Set by the scanning thread the moment it falls back to the walk, so
    /// the banner shows while the slower scan is still running.
    notice: Arc<OnceLock<String>>,
    pub started: Instant,
}

impl ScanProgress {
    pub fn entries(&self) -> u64 {
        self.counters.entries.load(Ordering::Relaxed)
    }

    /// Done so far, in 0..=1; `None` until the first numbers arrive. The
    /// CLI's rule, so both front-ends show the same number.
    pub fn fraction(&self) -> Option<f64> {
        self.counters.fraction(self.used_bytes)
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.get().map(String::as_str)
    }
}

/// The MFT read is always offered: in-process when already elevated,
/// otherwise through core's elevated helper, which raises one UAC prompt per
/// scan. A declined or failed prompt falls back to the walk and puts why in
/// `notice`. `Backend::Mft` keeps the CLI's meaning, failing rather than
/// falling back.
fn scan_drive(
    root: &VolumeRoot,
    opts: ScanOptions,
    ntfs: bool,
    notice_slot: &OnceLock<String>,
) -> Result<ScanReport, ScanFailure> {
    if opts.backend == Backend::Walk || !ntfs || is_elevated() {
        return scan::scan(root, &opts);
    }
    // Lets the fallback be exercised without a UAC prompt to decline.
    let elevated = if std::env::var_os("NOMNOM_GUI_FAIL_ELEVATED").is_some() {
        Err(ElevatedScanError::Failed("NOMNOM_GUI_FAIL_ELEVATED is set".into()))
    } else {
        scan_elevated(root, &opts)
    };
    let notice = match elevated {
        Ok(report) => return Ok(report),
        Err(ElevatedScanError::Declined) => {
            "Administrator access declined — using the slower walk scan".to_string()
        }
        Err(ElevatedScanError::Failed(message)) => {
            format!("The elevated MFT scan failed ({message}) — using the slower walk scan")
        }
    };
    if opts.backend == Backend::Mft {
        return Err(ScanFailure::MftUnavailable(notice));
    }
    eprintln!("nomnom-gui: {notice}");
    let _ = notice_slot.set(notice);
    // The helper may have counted part of the MFT before it stopped.
    if let Some(progress) = &opts.progress {
        for counter in [&progress.entries, &progress.entries_total, &progress.bytes] {
            counter.store(0, Ordering::Relaxed);
        }
    }
    let walk = ScanOptions { backend: Backend::Walk, ..opts };
    scan::scan(root, &walk)
}

/// A finished scan and the drive-wide views derived from it once, off the UI
/// thread, rather than per frame.
pub struct ScanData {
    pub catalog: Arc<Catalog>,
    pub file_types: Vec<FileType>,
    pub largest: Vec<NodeId>,
    pub palette: Palette,
    /// On-disk bytes per subtree, indexed by node id; `None` when the backend
    /// reported no allocation sizes (the walk backend never does).
    pub allocated: Option<Vec<u64>>,
    pub elapsed: Duration,
}

impl ScanData {
    const LARGEST: usize = 1000;

    fn new(catalog: Catalog, started: Instant) -> Self {
        let file_types = file_types(&catalog);
        let largest = largest_files(&catalog, Self::LARGEST);
        let palette = Palette::new(&file_types);
        let allocated = subtree_allocated(&catalog);
        Self {
            catalog: Arc::new(catalog),
            file_types,
            largest,
            palette,
            allocated,
            elapsed: started.elapsed(),
        }
    }

    pub fn allocated(&self, id: NodeId) -> Option<u64> {
        self.allocated.as_ref().map(|sizes| sizes[id.0 as usize])
    }
}

/// Node ids follow path order, so every child has a higher id than its parent
/// and one reverse pass rolls sizes up.
fn subtree_allocated(catalog: &Catalog) -> Option<Vec<u64>> {
    if !catalog.nodes().any(|node| node.allocated.is_some()) {
        return None;
    }
    let mut sums: Vec<u64> = catalog.nodes().map(|node| node.allocated.unwrap_or(0)).collect();
    for ix in (0..sums.len()).rev() {
        if let Some(parent) = catalog.node(NodeId(ix as u32)).parent {
            sums[parent.0 as usize] += sums[ix];
        }
    }
    Some(sums)
}

/// Emitted when a new assessment lands, so screens holding per-assessment
/// state (the Clean selection) can reset it.
pub struct Assessed;

pub struct Session {
    /// The drive open, as `Volume.root`; the GUI scans whole drives only.
    pub root: Option<PathBuf>,
    volume: Option<Volume>,
    pub backend: Backend,
    /// The CLI's `--pack DIR` list, in the order added: loaded last, so a
    /// later one overrides an earlier one and both override the other tiers.
    pub explicit_packs: Vec<PathBuf>,
    pub scan: Option<Arc<ScanData>>,
    /// Set while a scan runs.
    pub progress: Option<ScanProgress>,
    pub assessment: Option<Arc<Assessment>>,
    pub busy: Option<Phase>,
    pub scan_error: Option<String>,
    /// Why the scan fell back to the walk instead of the elevated MFT read.
    pub scan_notice: Option<String>,
    /// Pack resolution or judging failed; the tree is still valid without it.
    pub assess_error: Option<String>,
}

impl EventEmitter<Assessed> for Session {}

impl Session {
    pub fn new() -> Self {
        Self {
            root: None,
            volume: None,
            backend: Backend::Auto,
            explicit_packs: Vec::new(),
            scan: None,
            progress: None,
            assessment: None,
            busy: None,
            scan_error: None,
            scan_notice: None,
            assess_error: None,
        }
    }

    pub fn scan_volume(&mut self, volume: Volume, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.root = Some(volume.root.clone());
        self.volume = Some(volume);
        self.assessment = None;
        self.scan(cx);
    }

    /// Claim the single phase slot. `false` means something else is running
    /// and the caller must not start.
    pub fn begin(&mut self, phase: Phase, cx: &mut Context<Self>) -> bool {
        if self.busy.is_some() {
            return false;
        }
        self.busy = Some(phase);
        cx.notify();
        true
    }

    pub fn end(&mut self, cx: &mut Context<Self>) {
        self.busy = None;
        cx.notify();
    }

    pub fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(volume) = self.volume.clone() else { return };
        let root = volume.root.clone();
        if !self.begin(Phase::Scanning, cx) {
            return;
        }
        // Judging a whole drive hashes every duplicate candidate and takes
        // minutes, so it runs again only for a rescan the user had analyzed.
        let reassess = self.assessment.is_some();
        self.scan = None;
        self.assessment = None;
        self.scan_error = None;
        self.scan_notice = None;
        self.assess_error = None;
        let backend = self.backend;
        let counters = Arc::new(scan::ScanProgress::default());
        let notice = Arc::new(OnceLock::new());
        let started = Instant::now();
        self.progress = Some(ScanProgress {
            counters: counters.clone(),
            used_bytes: volume.total.saturating_sub(volume.free),
            notice: notice.clone(),
            started,
        });

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let ticking = this.update(cx, |this, cx| {
                    cx.notify();
                    this.busy == Some(Phase::Scanning)
                });
                if !matches!(ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let scan_root = root.clone();
            let slot = notice.clone();
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    let opts =
                        ScanOptions { backend, progress: Some(counters), ..ScanOptions::default() };
                    let ntfs = volume.fs.eq_ignore_ascii_case("NTFS");
                    let report = VolumeRoot::new(&scan_root)
                        .and_then(|root| scan_drive(&root, opts, ntfs, &slot))?;
                    Ok::<_, ScanFailure>(Arc::new(ScanData::new(Catalog::build(report), started)))
                })
                .await;
            let notice = notice.get().cloned();
            let data = match scanned {
                Ok(data) => data,
                Err(error) => {
                    let message = format!("cannot scan {}: {error}", root.display());
                    eprintln!("nomnom-gui: {message}");
                    let _ = this.update(cx, |this, cx| {
                        this.scan_error = Some(message);
                        this.scan_notice = notice;
                        this.progress = None;
                        this.end(cx);
                    });
                    return;
                }
            };
            let landed = this.update(cx, |this, cx| {
                this.scan = Some(data);
                this.scan_notice = notice;
                this.progress = None;
                this.end(cx);
                if reassess {
                    this.assess(cx);
                }
            });
            if landed.is_err() {
                eprintln!(
                    "nomnom-gui: session dropped before the scan of {} landed",
                    root.display()
                );
            }
        })
        .detach();
    }

    /// Re-judge after a pack change, but only a catalog the user already had
    /// analyzed: an unasked-for full-drive judgement would hold the busy slot
    /// for minutes.
    pub fn reassess(&mut self, cx: &mut Context<Self>) {
        if self.assessment.is_some() {
            self.assess(cx);
        }
    }

    /// Judge the current catalog again — after a scan, and after a pack
    /// change alters which rules load or how far they are trusted.
    pub fn assess(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(catalog)) =
            (self.root.clone(), self.scan.as_ref().map(|scan| scan.catalog.clone()))
        else {
            return;
        };
        if !self.begin(Phase::Assessing, cx) {
            return;
        }
        self.assess_error = None;
        let explicit = self.explicit_packs.clone();

        cx.spawn(async move |this, cx| {
            let pack_root = root.clone();
            let judged = cx
                .background_executor()
                .spawn(async move {
                    let packs = resolve_packs(&pack_root, &explicit)?;
                    Ok::<_, nomnom_pack::Error>(Arc::new(assess(&catalog, packs)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match judged {
                    Ok(assessment) => {
                        this.assessment = Some(assessment);
                        cx.emit(Assessed);
                    }
                    Err(error) => {
                        let message =
                            format!("cannot load the rule packs for {}: {error}", root.display());
                        eprintln!("nomnom-gui: {message}");
                        this.assess_error = Some(message);
                    }
                }
                this.end(cx);
            });
        })
        .detach();
    }
}
