//! The one shared piece of state: which drive is open, what the scan found,
//! and what the judge made of it. Scan runs once per drive, and the judging
//! starts by itself once it lands; the plan list and Reclaim read that one
//! assessment.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::{Context, EventEmitter};
use nomnom_core::catalog::{Catalog, DuplicateProgress, NodeId};
use nomnom_core::scan::{self, ScanFailure, Volume, VolumeRoot};
use nomnom_core::timings;
use nomnom_core::verdict::{Assessment, assess, find_duplicates, resolve_packs};

use crate::palette::Palette;

/// The long-running phase in flight. Only one runs at a time: every phase
/// either reads the catalog another would replace or moves files another
/// would read, so the UI disables conflicting actions while this is `Some`.
///
/// Judging is not a phase: it only reads the catalog, so it runs beside the
/// tree and is superseded, never waited on, when a new scan starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Applying,
    Packs,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Scanning => "Scanning…",
            Phase::Applying => "Applying the plan…",
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

    /// Set the moment core's scan falls back to the walk, so the banner shows
    /// while the slower scan is still running.
    pub fn notice(&self) -> Option<String> {
        self.counters.fallback.get().map(|reason| fallback_notice(reason))
    }
}

fn fallback_notice(reason: &str) -> String {
    format!("Using the slower walk scan: {reason}")
}

/// A finished scan and the drive-wide views derived from it once, off the UI
/// thread, rather than per frame.
pub struct ScanData {
    pub catalog: Arc<Catalog>,
    pub palette: Palette,
    /// On-disk bytes per subtree, indexed by node id; `None` when the backend
    /// reported no allocation sizes (the walk backend never does).
    pub allocated: Option<Vec<u64>>,
    pub elapsed: Duration,
    /// When the user started the scan, for the first-paint timing.
    pub started: Instant,
    painted: AtomicBool,
}

impl ScanData {
    fn new(catalog: Catalog, started: Instant) -> Self {
        let palette = Palette::new(&catalog);
        let allocated = subtree_allocated(&catalog);
        Self {
            catalog: Arc::new(catalog),
            palette,
            allocated,
            elapsed: started.elapsed(),
            started,
            painted: AtomicBool::new(false),
        }
    }

    /// Records the click-to-treemap time the first time the map paints.
    pub fn note_painted(&self) {
        if !self.painted.swap(true, Ordering::Relaxed) {
            timings::record("gui click -> first treemap paint", self.started.elapsed());
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

/// Emitted when the duplicate pass merges its likely copies into the current
/// assessment. Distinct from [`Assessed`] because nothing the user decided is
/// invalidated: every rule entry is carried over unchanged.
pub struct DuplicatesMerged;

pub struct Session {
    /// The drive open, as `Volume.root`; the GUI scans whole drives only.
    pub root: Option<PathBuf>,
    volume: Option<Volume>,
    /// The CLI's `--pack DIR` list, in the order added: loaded last, so a
    /// later one overrides an earlier one and both override the other tiers.
    pub explicit_packs: Vec<PathBuf>,
    pub scan: Option<Arc<ScanData>>,
    /// Set while a scan runs.
    pub progress: Option<ScanProgress>,
    pub assessment: Option<Arc<Assessment>>,
    /// Set while the judging of the current catalog runs in the background.
    pub assessing: bool,
    /// Set while the duplicate pass over the current assessment runs; the
    /// rules' answer is already on screen by then.
    pub duplicates: Option<Arc<DuplicateProgress>>,
    /// Bumped by every scan and every judging run, so a result that lands
    /// after a newer one started is dropped rather than shown.
    assess_generation: u64,
    pub busy: Option<Phase>,
    pub scan_error: Option<String>,
    /// Why the scan fell back to the walk instead of the elevated MFT read.
    pub scan_notice: Option<String>,
    /// Pack resolution or judging failed; the tree is still valid without it.
    pub assess_error: Option<String>,
}

impl EventEmitter<Assessed> for Session {}
impl EventEmitter<DuplicatesMerged> for Session {}

impl Session {
    pub fn new() -> Self {
        Self {
            root: None,
            volume: None,
            explicit_packs: Vec::new(),
            scan: None,
            progress: None,
            assessment: None,
            assessing: false,
            duplicates: None,
            assess_generation: 0,
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
        self.scan = None;
        self.assessment = None;
        self.assessing = false;
        // Any judging or duplicate pass still running is for the catalog this
        // scan replaces.
        self.assess_generation += 1;
        self.cancel_duplicates();
        self.scan_error = None;
        self.scan_notice = None;
        self.assess_error = None;
        timings::start_scan("gui");
        let counters = Arc::new(scan::ScanProgress::default());
        let started = Instant::now();
        self.progress = Some(ScanProgress {
            counters: counters.clone(),
            used_bytes: volume.total.saturating_sub(volume.free),
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
            let progress = counters.clone();
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    let report = VolumeRoot::new(&scan_root)
                        .and_then(|root| scan::scan_drive(&root, Some(progress)))?;
                    let catalog_started = timings::lap("gui click -> report in hand", started);
                    let catalog = Catalog::build(report);
                    let aggregates_started = timings::lap("Catalog::build total", catalog_started);
                    let data = ScanData::new(catalog, started);
                    timings::lap("gui aggregates (ScanData::new)", aggregates_started);
                    Ok::<_, ScanFailure>(Arc::new(data))
                })
                .await;
            let notice = counters.fallback.get().map(|reason| fallback_notice(reason));
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
                this.assess(cx);
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

    /// Re-judge after a pack change alters which rules load or how far they
    /// are trusted.
    pub fn reassess(&mut self, cx: &mut Context<Self>) {
        self.assess(cx);
    }

    /// Judge the current catalog in the background. Starts by itself after
    /// every scan; a newer run supersedes an older one still in flight.
    fn assess(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(catalog)) =
            (self.root.clone(), self.scan.as_ref().map(|scan| scan.catalog.clone()))
        else {
            return;
        };
        self.assess_generation += 1;
        self.cancel_duplicates();
        let generation = self.assess_generation;
        self.assessing = true;
        self.assess_error = None;
        cx.notify();
        let explicit = self.explicit_packs.clone();

        cx.spawn(async move |this, cx| {
            let pack_root = root.clone();
            let started = Instant::now();
            let judged = cx
                .background_executor()
                .spawn(async move {
                    let packs = resolve_packs(&pack_root, &explicit)?;
                    Ok::<_, nomnom_pack::Error>(Arc::new(assess(&catalog, packs)))
                })
                .await;
            let elapsed = started.elapsed();
            timings::record("gui assess", elapsed);
            eprintln!("nomnom-gui: assessed {} in {:.1?}", root.display(), elapsed);
            let _ = this.update(cx, |this, cx| {
                if this.assess_generation != generation {
                    return;
                }
                this.assessing = false;
                match judged {
                    Ok(assessment) => {
                        this.assessment = Some(assessment.clone());
                        cx.emit(Assessed);
                        this.find_duplicates(generation, assessment, cx);
                    }
                    Err(error) => {
                        let message =
                            format!("cannot load the rule packs for {}: {error}", root.display());
                        eprintln!("nomnom-gui: {message}");
                        this.assess_error = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stops the duplicate pass in flight, if any; its result is dropped.
    fn cancel_duplicates(&mut self) {
        if let Some(progress) = self.duplicates.take() {
            progress.cancel();
        }
    }

    /// The second phase: find likely copies outside the rules' targets and
    /// merge them into `rules`, which is already on screen.
    fn find_duplicates(&mut self, generation: u64, rules: Arc<Assessment>, cx: &mut Context<Self>) {
        let Some(catalog) = self.scan.as_ref().map(|scan| scan.catalog.clone()) else { return };
        let progress = Arc::new(DuplicateProgress::default());
        self.duplicates = Some(progress.clone());
        cx.notify();

        let running = progress.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let ticking = this.update(cx, |this, cx| {
                    cx.notify();
                    this.duplicates.as_ref().is_some_and(|it| Arc::ptr_eq(it, &running))
                });
                if !matches!(ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let worker = progress.clone();
            let merged = cx
                .background_executor()
                .spawn(async move {
                    let found = find_duplicates(&catalog, &rules, &worker)?;
                    Some(Arc::new(rules.with_duplicates(&catalog, &found)))
                })
                .await;
            timings::record("gui duplicates", started.elapsed());
            let _ = this.update(cx, |this, cx| {
                let current = this.duplicates.as_ref().is_some_and(|it| Arc::ptr_eq(it, &progress));
                if this.assess_generation != generation || !current {
                    return;
                }
                this.duplicates = None;
                if let Some(merged) = merged {
                    this.assessment = Some(merged);
                    cx.emit(DuplicatesMerged);
                }
                cx.notify();
            });
        })
        .detach();
    }
}
