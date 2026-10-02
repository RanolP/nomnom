//! Clean: choose entries, watch the dry run follow, then apply behind a
//! confirmation that names exactly what will move and where it goes.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::{
    Action, ApplyOptions, RecordStatus, TrashPolicy, apply, plain, trash_policy,
};
use nomnom_core::verdict::Assessment;

use crate::session::{Assessed, Phase, Session};
use crate::state::{Preview, Selection, size};
use crate::suggest::{disposition_tag, waiting_for_assessment};

const ROW_HEIGHT: f32 = 46.;

/// What one apply did, kept until the next one.
struct Outcome {
    journal: PathBuf,
    bytes_reclaimed: u64,
    succeeded: usize,
    failures: Vec<(PathBuf, String)>,
}

pub struct CleanScreen {
    session: Entity<Session>,
    selection: Selection,
    /// The CLI's `--stage DIR`: trashed paths move here instead of the
    /// recycle bin.
    stage: Option<PathBuf>,
    preview: Option<Result<Rc<Preview>, String>>,
    outcome: Option<Result<Outcome, String>>,
    picker_error: Option<String>,
}

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
        let mut this = Self {
            session,
            selection: Selection::default(),
            stage: None,
            preview: None,
            outcome: None,
            picker_error: None,
        };
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

    fn confirm_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(preview)) = self.preview.clone() else { return };
        if preview.plan.is_empty() {
            return;
        }
        let policy = trash_policy(self.stage.clone());
        let body = format!(
            "{} paths, {} in total, will be {}.\n\nA journal is written before anything moves; \
             the Undo screen can reverse this apply from it.",
            preview.plan.len(),
            size(preview.plan.total_bytes()),
            describe(&policy),
        );
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let policy = policy.clone();
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
                    view.update(cx, |this, cx| this.apply(policy.clone(), cx));
                    true
                })
        });
    }

    fn choose_stage(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Stage trashed paths here".into()),
        });
        cx.spawn(async move |this, cx| {
            let outcome = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next().map(Ok),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => Some(Err(format!("cannot open the folder picker: {error}"))),
            };
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Some(Ok(dir)) => {
                        this.stage = Some(dir);
                        this.picker_error = None;
                    }
                    Some(Err(message)) => {
                        eprintln!("nomnom-gui: {message}");
                        this.picker_error = Some(message);
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_stage(&self, busy: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let target = match &self.stage {
            Some(dir) => format!("Trashed paths move into {}", plain(dir)),
            None => "Trashed paths go to the recycle bin".to_string(),
        };
        h_flex()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(target),
            )
            .child(
                Button::new("choose-stage")
                    .small()
                    .outline()
                    .label("Stage into folder…")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_stage(cx))),
            )
            .when(self.stage.is_some(), |row| {
                row.child(
                    Button::new("clear-stage")
                        .small()
                        .ghost()
                        .label("Use recycle bin")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.stage = None;
                            cx.notify();
                        })),
                )
            })
    }

    fn apply(&mut self, policy: TrashPolicy, cx: &mut Context<Self>) {
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
                    let opts = ApplyOptions { trash_policy: policy, ..ApplyOptions::default() };
                    apply(&plan, &opts).map_err(|error| {
                        format!("apply under {} failed: {error}", plain(plan.root()))
                    })
                })
                .await;
            let outcome = applied.map(|journal| Outcome {
                journal: journal.path().to_path_buf(),
                bytes_reclaimed: journal.bytes_reclaimed(),
                succeeded: journal.records().iter().filter(|r| r.succeeded()).count(),
                failures: journal
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
                .child(format!("Journal: {}", plain(&done.journal)))
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

fn describe(policy: &TrashPolicy) -> String {
    match policy {
        TrashPolicy::Recycle => "moved to the recycle bin".to_string(),
        TrashPolicy::Stage { dir } => format!("moved into the staging directory {}", plain(dir)),
    }
}

impl Render for CleanScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        if let Some(waiting) = waiting_for_assessment(&self.session, cx) {
            return v_flex()
                .size_full()
                .children(self.render_outcome(cx))
                .child(waiting)
                .into_any_element();
        }
        let session = self.session.read(cx);
        let busy = session.busy;
        let assessment = session.assessment.clone().expect("checked above");
        let candidates: Rc<Vec<(String, u64, nomnom_core::verdict::Disposition, String)>> = Rc::new(
            self.selection
                .candidates(&assessment)
                .into_iter()
                .map(|e| (e.path.clone(), e.bytes, e.verdict.disposition, e.verdict.reason.clone()))
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

        let (summary, can_apply) = match &self.preview {
            Some(Ok(preview)) if preview.plan.is_empty() => {
                ("Nothing to clean.".to_string(), false)
            }
            Some(Ok(preview)) => (
                format!(
                    "Dry run: {} actions, {} to reclaim. Nothing has been touched.",
                    preview.plan.len(),
                    size(preview.plan.total_bytes())
                ),
                true,
            ),
            Some(Err(message)) => (message.clone(), false),
            None => (String::new(), false),
        };
        let apply_button = Button::new("apply")
            .danger()
            .label("Apply…")
            .disabled(!can_apply || busy.is_some())
            .on_click(cx.listener(|this, _, window, cx| this.confirm_apply(window, cx)));

        let checklist = {
            let candidates = candidates.clone();
            uniform_list(
                "clean-candidates",
                candidates.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    range
                        .map(|ix| {
                            let (path, bytes, disposition, reason) = &candidates[ix];
                            let checked = this.selection.is_checked(path);
                            let toggled = path.clone();
                            h_flex()
                                .h(px(ROW_HEIGHT))
                                .w_full()
                                .gap_2()
                                .px_2()
                                .text_sm()
                                .child(Checkbox::new(("pick", ix)).checked(checked).on_change(
                                    cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_checked(&toggled, *checked, cx)
                                    }),
                                ))
                                .child(disposition_tag(*disposition))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .overflow_hidden()
                                        .child(div().whitespace_nowrap().child(path.clone()))
                                        .child(
                                            div()
                                                .whitespace_nowrap()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(reason.clone()),
                                        ),
                                )
                                .child(div().child(size(*bytes)))
                                .into_any_element()
                        })
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
            .child(self.render_stage(busy.is_some(), cx))
            .when_some(self.picker_error.clone(), |col, message| {
                col.child(Alert::error("stage-picker-error", message))
            })
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(summary))
                    .when(busy == Some(Phase::Applying), |row| {
                        row.child(Spinner::new().small()).child(Phase::Applying.label())
                    })
                    .child(apply_button),
            )
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
