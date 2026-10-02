//! The files to delete: choose entries and watch the dry run follow. Reclaim,
//! in the tree's bottom bar, applies the plan this view holds, behind a
//! confirmation that names exactly what will move.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::{Action, RecordStatus, apply, plain};
use nomnom_core::verdict::{Assessment, Verdict};

use crate::session::{Assessed, Phase, Session};
use crate::state::{Preview, Selection, size};
use crate::suggest::{disposition_tag, label_name, waiting_for_assessment};

/// Three lines per row: path, reason, and the rule behind it.
const ROW_HEIGHT: f32 = 62.;

/// What one apply did, kept until the next one.
struct Outcome {
    bytes_reclaimed: u64,
    succeeded: usize,
    failures: Vec<(PathBuf, String)>,
}

/// Emitted when an apply finishes, so the window can show its outcome.
pub struct Applied;

pub struct CleanScreen {
    session: Entity<Session>,
    selection: Selection,
    preview: Option<Result<Rc<Preview>, String>>,
    outcome: Option<Result<Outcome, String>>,
}

impl EventEmitter<Applied> for CleanScreen {}

impl CleanScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&session, |this, _, _: &Assessed, cx| {
            // A new assessment has different entries; a selection carried over
            // from the old one would silently re-include or drop paths.
            this.selection.recheck_all();
            this.refresh_preview(cx);
        })
        .detach();
        let mut this =
            Self { session, selection: Selection::default(), preview: None, outcome: None };
        this.refresh_preview(cx);
        this
    }

    fn assessment(&self, cx: &App) -> Option<Arc<Assessment>> {
        self.session.read(cx).assessment.clone()
    }

    /// Recomputed on every selection change rather than per frame: building a
    /// plan canonicalizes every chosen path.
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = self.assessment(cx).map(|assessment| {
            self.selection.preview(&assessment).map(Rc::new).map_err(|error| {
                let message =
                    format!("cannot anchor a plan at {}: {error}", assessment.root.display());
                eprintln!("nomnom-gui: {message}");
                message
            })
        });
        cx.notify();
    }

    fn set_checked(&mut self, path: &str, checked: bool, cx: &mut Context<Self>) {
        self.selection.set_checked(path, checked);
        self.refresh_preview(cx);
    }

    /// The plan Reclaim would apply: its action count and total bytes, or
    /// `None` while there is nothing to apply.
    pub fn plan_summary(&self) -> Option<(usize, u64)> {
        match &self.preview {
            Some(Ok(preview)) if !preview.plan.is_empty() => {
                Some((preview.plan.len(), preview.plan.total_bytes()))
            }
            _ => None,
        }
    }

    pub fn confirm_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((len, bytes)) = self.plan_summary() else { return };
        let body =
            format!("{len} paths, {} in total, will be moved to the recycle bin.", size(bytes));
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("Apply this cleanup?")
                .child(div().text_sm().child(body.clone()))
                .button_props(
                    gpui_kit::component::dialog::DialogButtonProps::default()
                        .ok_text("Apply")
                        .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    view.update(cx, |this, cx| this.apply(cx));
                    true
                })
        });
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(preview)) = self.preview.clone() else { return };
        let session = self.session.clone();
        if !session.update(cx, |session, cx| session.begin(Phase::Applying, cx)) {
            return;
        }
        self.outcome = None;
        let plan = preview.plan.clone();
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_executor()
                .spawn(async move {
                    apply(&plan).map_err(|error| {
                        format!("apply under {} failed: {error}", plain(plan.root()))
                    })
                })
                .await;
            let outcome = applied.map(|report| Outcome {
                bytes_reclaimed: report.bytes_reclaimed(),
                succeeded: report.records().iter().filter(|r| r.succeeded()).count(),
                failures: report
                    .failures()
                    .map(|record| {
                        let message = match &record.status {
                            RecordStatus::Failed { message } => message.clone(),
                            _ => String::new(),
                        };
                        (record.source.clone(), message)
                    })
                    .collect(),
            });
            match &outcome {
                Ok(done) => {
                    for (path, message) in &done.failures {
                        eprintln!("nomnom-gui: apply could not move {}: {message}", plain(path));
                    }
                }
                Err(message) => eprintln!("nomnom-gui: {message}"),
            }
            let _ = this.update(cx, |this, cx| {
                this.outcome = Some(outcome);
                cx.emit(Applied);
                cx.notify();
            });
            // What was applied is gone from disk, so the assessment is stale.
            session.update(cx, |session, cx| {
                session.end(cx);
                session.scan(cx);
            });
        })
        .detach();
    }

    fn render_outcome(&self, cx: &App) -> Option<AnyElement> {
        let outcome = self.outcome.as_ref()?;
        Some(match outcome {
            Err(message) => Alert::error("apply-error", message.clone()).into_any_element(),
            Ok(done) => v_flex()
                .gap_1()
                .p_3()
                .border_1()
                .border_color(cx.theme().border)
                .rounded_md()
                .text_sm()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "Applied: {} actions done, {} reclaimed.",
                    done.succeeded,
                    size(done.bytes_reclaimed)
                )))
                .when(!done.failures.is_empty(), |panel| {
                    panel
                        .child(
                            div()
                                .text_color(cx.theme().danger)
                                .child(format!("{} actions failed:", done.failures.len())),
                        )
                        .children(done.failures.iter().map(|(path, message)| {
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(format!("{}: {message}", plain(path)))
                        }))
                })
                .into_any_element(),
        })
    }
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Trash { .. } => "trash",
        Action::Archive { .. } => "archive",
        Action::Move { .. } => "move",
    }
}

/// A row's copy of an assessment entry, owned so the virtualized list can
/// hold it across frames.
struct Candidate {
    path: String,
    bytes: u64,
    verdict: Verdict,
}

/// One candidate: the path, its reason, and the `pack/rule` behind it, never
/// behind a click — with packs coming from the network, "who says so" is part
/// of what a human approves on.
fn render_candidate(
    this: &CleanScreen,
    ix: usize,
    entry: &Candidate,
    cx: &mut Context<CleanScreen>,
) -> AnyElement {
    let verdict = &entry.verdict;
    let muted = cx.theme().muted_foreground;
    let toggled = entry.path.clone();
    h_flex()
        .h(px(ROW_HEIGHT))
        .w_full()
        .gap_2()
        .px_2()
        .text_sm()
        .child(
            Checkbox::new(("pick", ix)).checked(this.selection.is_checked(&entry.path)).on_change(
                cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_checked(&toggled, *checked, cx)
                }),
            ),
        )
        .child(disposition_tag(verdict.disposition))
        .child(
            v_flex()
                .flex_1()
                .overflow_hidden()
                .child(div().whitespace_nowrap().child(entry.path.clone()))
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(muted)
                        .child(verdict.reason.clone()),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{} — {}", label_name(&verdict.label), verdict.provenance))
                        // A capped verdict looks exactly like one written as
                        // `review`; without this the missing trust grant is
                        // invisible.
                        .when_some(verdict.capped.clone(), |row, capped| {
                            row.child(
                                div().text_color(cx.theme().warning).child(format!("! {capped}")),
                            )
                        }),
                ),
        )
        .child(div().child(size(entry.bytes)))
        .into_any_element()
}

impl Render for CleanScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(waiting) = waiting_for_assessment(&self.session, cx) {
            return v_flex()
                .size_full()
                .children(self.render_outcome(cx))
                .child(waiting)
                .into_any_element();
        }
        let assessment = self.session.read(cx).assessment.clone().expect("checked above");
        let candidates: Rc<Vec<Candidate>> = Rc::new(
            self.selection
                .candidates(&assessment)
                .into_iter()
                .map(|entry| Candidate {
                    path: entry.path.clone(),
                    bytes: entry.bytes,
                    verdict: entry.verdict.clone(),
                })
                .collect(),
        );

        let include_review = self.selection.include_review;
        let toggle = Checkbox::new("include-review")
            .label("Include review — also act on paths the evidence does not carry on its own")
            .checked(include_review)
            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                this.selection.include_review = *checked;
                this.refresh_preview(cx);
            }));

        let summary = match &self.preview {
            Some(Ok(preview)) if preview.plan.is_empty() => "Nothing to clean.".to_string(),
            Some(Ok(preview)) => format!(
                "Dry run: {} actions, {} to reclaim. Nothing has been touched.",
                preview.plan.len(),
                size(preview.plan.total_bytes())
            ),
            Some(Err(message)) => message.clone(),
            None => String::new(),
        };

        let checklist = {
            let candidates = candidates.clone();
            uniform_list(
                "clean-candidates",
                candidates.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    range
                        .map(|ix| render_candidate(this, ix, &candidates[ix], cx))
                        .collect::<Vec<_>>()
                }),
            )
            .size_full()
        };

        let plan_list = match &self.preview {
            Some(Ok(preview)) => {
                let preview = preview.clone();
                v_flex()
                    .id("plan-list")
                    .size_full()
                    .overflow_y_scrollbar()
                    .text_xs()
                    .children(preview.plan.actions().iter().map(|entry| {
                        h_flex()
                            .gap_2()
                            .py_0p5()
                            .child(div().w(px(56.)).child(verb(&entry.action)))
                            .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(
                                match entry.action.destination() {
                                    Some(to) => {
                                        format!("{} → {}", plain(entry.action.path()), plain(to))
                                    }
                                    None => plain(entry.action.path()),
                                },
                            ))
                            .child(div().child(size(entry.bytes)))
                    }))
                    .children(preview.refused.iter().map(|(path, error)| {
                        div()
                            .text_color(cx.theme().warning)
                            .child(format!("skipping {}: {error}", plain(path)))
                    }))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        };

        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .children(self.render_outcome(cx))
            .child(toggle)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(summary))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_4()
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("Candidates ({})", candidates.len())),
                            )
                            .child(div().flex_1().min_h_0().child(checklist)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Plan preview"),
                            )
                            .child(div().flex_1().min_h_0().child(plan_list)),
                    ),
            )
            .into_any_element()
    }
}
