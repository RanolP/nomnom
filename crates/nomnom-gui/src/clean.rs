//! The files to delete: approve rules, open one to see what it matched and
//! exclude what must stay, and watch the dry run follow. Reclaim, in the
//! tree's bottom bar, applies the plan this view holds, behind a confirmation
//! that names exactly what will move.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::{Action, RecordStatus, apply, plain};
use nomnom_core::verdict::{Assessment, Disposition, Provenance, Verdict};

use crate::session::{Assessed, Phase, Session};
use crate::state::{Preview, Selection, size};
use crate::suggest::{disposition_tag, label_name, waiting_for_assessment};

/// Three lines per row: a rule's title, pack and tally, or a match's path,
/// reason and exclusion state.
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
    /// How many paths the approved rules plan; kept beside `preview`.
    checked: usize,
    preview: Option<Result<Rc<Preview>, String>>,
    outcome: Option<Result<Outcome, String>>,
    /// Rules whose matches are listed under them.
    expanded: HashSet<Provenance>,
    /// The last exclusion list read or write that failed.
    exclusion_error: Option<String>,
}

impl EventEmitter<Applied> for CleanScreen {}

impl CleanScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&session, |this, _, _: &Assessed, cx| this.reassessed(cx)).detach();
        let mut this = Self {
            session,
            selection: Selection::default(),
            checked: 0,
            preview: None,
            outcome: None,
            expanded: HashSet::new(),
            exclusion_error: None,
        };
        this.reassessed(cx);
        this
    }

    /// A new assessment (rescan, pack change, post-apply rescan) has
    /// different matches; a rule approved against the old ones is not an
    /// approval of these, so the user starts from nothing approved. The
    /// drive's exclusions are read back, so they hold across scans.
    fn reassessed(&mut self, cx: &mut Context<Self>) {
        if let Some(assessment) = self.assessment(cx) {
            self.exclusion_error = self.selection.reassessed(&assessment.root).err().map(|error| {
                let message = format!("exclusions not loaded: {error}");
                eprintln!("nomnom-gui: {message}");
                message
            });
        } else {
            self.selection.clear();
        }
        self.refresh_preview(cx);
    }

    fn assessment(&self, cx: &App) -> Option<Arc<Assessment>> {
        self.session.read(cx).assessment.clone()
    }

    /// Recomputed on every selection change rather than per frame: building a
    /// plan canonicalizes every chosen path.
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let assessment = self.assessment(cx);
        self.checked = assessment.as_ref().map_or(0, |a| self.selection.planned_count(a));
        self.preview = assessment.map(|assessment| {
            self.selection.preview(&assessment).map(Rc::new).map_err(|error| {
                let message =
                    format!("cannot anchor a plan at {}: {error}", assessment.root.display());
                eprintln!("nomnom-gui: {message}");
                message
            })
        });
        cx.notify();
    }

    fn set_approved(&mut self, rule: &Provenance, approved: bool, cx: &mut Context<Self>) {
        self.selection.set_approved(rule, approved);
        self.refresh_preview(cx);
    }

    fn toggle_expanded(&mut self, rule: &Provenance, cx: &mut Context<Self>) {
        if !self.expanded.remove(rule) {
            self.expanded.insert(rule.clone());
        }
        cx.notify();
    }

    fn set_excluded(&mut self, path: &Path, excluded: bool, cx: &mut Context<Self>) {
        self.exclusion_error = self.selection.set_excluded(path, excluded).err().map(|error| {
            let message = format!("exclusion not saved: {error}");
            eprintln!("nomnom-gui: {message}");
            message
        });
        self.refresh_preview(cx);
    }

    /// What the bottom bar shows: how many paths the approved rules plan, and
    /// the bytes of that plan — `(0, 0)` until a rule is approved.
    pub fn plan_summary(&self) -> (usize, u64) {
        let bytes = match &self.preview {
            Some(Ok(preview)) => preview.plan.total_bytes(),
            _ => 0,
        };
        (self.checked, bytes)
    }

    /// Whether Reclaim has anything the user checked to apply.
    pub fn can_apply(&self) -> bool {
        matches!(&self.preview, Some(Ok(preview)) if !preview.plan.is_empty())
    }

    pub fn confirm_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(preview)) = &self.preview else { return };
        if preview.plan.is_empty() {
            return;
        }
        let (len, bytes) = (preview.plan.len(), preview.plan.total_bytes());
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
                // What was approved is applied; the next plan starts empty.
                this.selection.clear();
                this.refresh_preview(cx);
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

/// One line of the rule list, owned so the virtualized list can hold it
/// across frames: a rule, or one of the matches of an expanded rule.
enum Row {
    Rule {
        provenance: Provenance,
        /// The label and disposition of its first match; one rule writes one
        /// of each, short of a trust cap.
        disposition: Disposition,
        label: String,
        matches: usize,
        excluded: usize,
        bytes: u64,
    },
    Match {
        path: String,
        bytes: u64,
        verdict: Verdict,
        /// The exclusion keeping it out of every plan, and whether that is
        /// the match itself (undoable here) or a directory above it (undone
        /// from the exclusion list).
        excluded_by: Option<(String, bool)>,
    },
}

fn rows(this: &CleanScreen, assessment: &Assessment) -> Vec<Row> {
    let mut rows = Vec::new();
    for group in this.selection.groups(assessment) {
        let excluded_by = |path: &str| {
            let itself = this.selection.exclusions().lists(Path::new(path));
            this.selection.excluded_by(path).map(|by| (plain(by), itself))
        };
        let first = &group.entries[0].verdict;
        rows.push(Row::Rule {
            provenance: group.provenance.clone(),
            disposition: first.disposition,
            label: label_name(&first.label),
            matches: group.entries.len(),
            excluded: group.entries.iter().filter(|e| excluded_by(&e.path).is_some()).count(),
            bytes: group.bytes,
        });
        if this.expanded.contains(group.provenance) {
            rows.extend(group.entries.iter().map(|entry| Row::Match {
                path: entry.path.clone(),
                bytes: entry.bytes,
                verdict: entry.verdict.clone(),
                excluded_by: excluded_by(&entry.path),
            }));
        }
    }
    rows
}

/// A rule: approve it to plan every match it made. Its pack is on the row,
/// never behind a click — with packs coming from the network, "who says so"
/// is part of what a human approves on.
fn render_row(
    this: &CleanScreen,
    ix: usize,
    row: &Row,
    cx: &mut Context<CleanScreen>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    match row {
        Row::Rule { provenance, disposition, label, matches, excluded, bytes } => {
            let approve = provenance.clone();
            let expand = provenance.clone();
            let open = this.expanded.contains(provenance);
            let tally = if *excluded > 0 {
                format!("{matches} matches, {excluded} excluded")
            } else {
                format!("{matches} matches")
            };
            h_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .gap_2()
                .px_2()
                .text_sm()
                .child(
                    Checkbox::new(("approve", ix))
                        .checked(this.selection.is_approved(provenance))
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_approved(&approve, *checked, cx)
                        })),
                )
                .child(
                    Button::new(("expand", ix))
                        .small()
                        .ghost()
                        .label(if open { "▾" } else { "▸" })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_expanded(&expand, cx)),
                        ),
                )
                .child(disposition_tag(*disposition))
                .child(
                    v_flex()
                        .flex_1()
                        .overflow_hidden()
                        .child(
                            div()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(provenance.rule.clone()),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{label} — pack {} — {tally}", provenance.pack)),
                        ),
                )
                .child(div().child(size(*bytes)))
                .into_any_element()
        }
        Row::Match { path, bytes, verdict, excluded_by } => {
            let target = PathBuf::from(path);
            let is_excluded = excluded_by.is_some();
            // Only an exclusion of the match itself is undone here; one on a
            // directory above it covers other paths too.
            let by_ancestor = matches!(excluded_by, Some((_, false)));
            let state = match excluded_by {
                Some((_, true)) => "excluded — kept out of every plan".to_string(),
                Some((by, false)) => format!("excluded by {by} — undo it in Exclusions below"),
                None => verdict.reason.clone(),
            };
            h_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .gap_2()
                .pl(px(40.))
                .pr_2()
                .text_sm()
                .child(
                    Checkbox::new(("exclude", ix))
                        .label("Exclude")
                        .checked(is_excluded)
                        .disabled(by_ancestor)
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_excluded(&target, *checked, cx)
                        })),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .overflow_hidden()
                        .when(is_excluded, |col| col.text_color(muted))
                        .child(div().whitespace_nowrap().child(path.clone()))
                        .child(div().whitespace_nowrap().text_xs().text_color(muted).child(state))
                        // A capped verdict looks exactly like one written as
                        // `review`; without this the missing trust grant is
                        // invisible.
                        .when_some(verdict.capped.clone(), |col, capped| {
                            col.child(
                                div()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(format!("! {capped}")),
                            )
                        }),
                )
                .child(div().child(size(*bytes)))
                .into_any_element()
        }
    }
}

/// The drive's persisted exclusions, each with its undo — the one place an
/// exclusion on a directory, or on a path no rule matches today, can be seen.
fn render_exclusions(this: &CleanScreen, cx: &mut Context<CleanScreen>) -> Option<AnyElement> {
    let exclusions = this.selection.exclusions();
    if exclusions.is_empty() && this.exclusion_error.is_none() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    Some(
        v_flex()
            .id("exclusions")
            .gap_1()
            .max_h(px(140.))
            .overflow_y_scrollbar()
            .text_xs()
            .when_some(this.exclusion_error.clone(), |col, message| {
                col.child(div().text_color(cx.theme().danger).child(message))
            })
            .child(div().text_color(muted).child(format!(
                "Exclusions ({}) — kept out of every plan on this drive, across scans",
                exclusions.paths().len()
            )))
            .children(exclusions.paths().enumerate().map(|(ix, path)| {
                let target = path.to_path_buf();
                h_flex()
                    .gap_2()
                    .child(Button::new(("unexclude", ix)).small().ghost().label("Remove").on_click(
                        cx.listener(move |this, _, _, cx| this.set_excluded(&target, false, cx)),
                    ))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(plain(path)))
            }))
            .into_any_element(),
    )
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
        let rows: Rc<Vec<Row>> = Rc::new(rows(self, &assessment));
        let rule_count = rows.iter().filter(|row| matches!(row, Row::Rule { .. })).count();

        let include_review = self.selection.include_review;
        let toggle = Checkbox::new("include-review")
            .label("Include review — also act on paths the evidence does not carry on its own")
            .checked(include_review)
            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                this.selection.include_review = *checked;
                this.refresh_preview(cx);
            }));

        let summary = match &self.preview {
            Some(Ok(preview)) if preview.plan.is_empty() => {
                "Nothing approved. Approve the rules whose matches should go; open one to \
                 see its matches and exclude any that must stay."
                    .to_string()
            }
            Some(Ok(preview)) => format!(
                "Dry run: {} actions, {} to reclaim. Nothing has been touched.",
                preview.plan.len(),
                size(preview.plan.total_bytes())
            ),
            Some(Err(message)) => message.clone(),
            None => String::new(),
        };

        let checklist = {
            let rows = rows.clone();
            uniform_list(
                "clean-rules",
                rows.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    range.map(|ix| render_row(this, ix, &rows[ix], cx)).collect::<Vec<_>>()
                }),
            )
            .size_full()
        };
        let exclusions = render_exclusions(self, cx);

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
            .children(exclusions)
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
                                    .child(format!("Rules ({rule_count})")),
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
