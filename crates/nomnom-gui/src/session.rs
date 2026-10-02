//! The one shared piece of state: which root is open, what the scan found, and
//! what the judge made of it. Scan runs once per root; Suggest and Clean read
//! the same assessment instead of each scanning again.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{Context, EventEmitter};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::{Backend, ScanOptions, scan};
use nomnom_core::verdict::{Assessment, assess, resolve_packs};

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

/// Emitted when a new assessment lands, so screens holding per-assessment
/// state (the Clean selection) can reset it.
pub struct Assessed;

pub struct Session {
    pub root: Option<PathBuf>,
    pub backend: Backend,
    pub catalog: Option<Arc<Catalog>>,
    pub assessment: Option<Arc<Assessment>>,
    pub busy: Option<Phase>,
    pub scan_error: Option<String>,
    /// Pack resolution or judging failed; the tree is still valid without it.
    pub assess_error: Option<String>,
}

impl EventEmitter<Assessed> for Session {}

impl Session {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            backend: Backend::Auto,
            catalog: None,
            assessment: None,
            busy: None,
            scan_error: None,
            assess_error: None,
        }
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = Some(root);
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
        let Some(root) = self.root.clone() else { return };
        if !self.begin(Phase::Scanning, cx) {
            return;
        }
        self.catalog = None;
        self.assessment = None;
        self.scan_error = None;
        self.assess_error = None;
        let backend = self.backend;

        cx.spawn(async move |this, cx| {
            let scan_root = root.clone();
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    let opts = ScanOptions { backend, ..ScanOptions::default() };
                    scan(&scan_root, &opts).map(|report| Arc::new(Catalog::build(report)))
                })
                .await;
            let catalog = match scanned {
                Ok(catalog) => catalog,
                Err(error) => {
                    let message = format!("cannot scan {}: {error}", root.display());
                    eprintln!("nomnom-gui: {message}");
                    let _ = this.update(cx, |this, cx| {
                        this.scan_error = Some(message);
                        this.end(cx);
                    });
                    return;
                }
            };
            let continued = this.update(cx, |this, cx| {
                this.catalog = Some(catalog.clone());
                this.busy = None;
                this.assess(cx);
            });
            if continued.is_err() {
                eprintln!(
                    "nomnom-gui: session dropped before the scan of {} landed",
                    root.display()
                );
            }
        })
        .detach();
    }

    /// Judge the current catalog again — after a scan, and after a pack
    /// change alters which rules load or how far they are trusted.
    pub fn assess(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(catalog)) = (self.root.clone(), self.catalog.clone()) else {
            return;
        };
        if !self.begin(Phase::Assessing, cx) {
            return;
        }
        self.assess_error = None;

        cx.spawn(async move |this, cx| {
            let pack_root = root.clone();
            let judged = cx
                .background_executor()
                .spawn(async move {
                    let packs = resolve_packs(&pack_root, &[])?;
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
