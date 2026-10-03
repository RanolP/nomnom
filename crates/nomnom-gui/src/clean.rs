//! The suggestions awaiting review, the panel at the tree's right: approve
//! rules, open one to see what it matched and exclude what must stay, and
//! watch the dry run follow. Reclaim, at the panel's foot, applies the plan
//! this panel holds, behind a confirmation that names exactly what will move.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::{Action, ActionKind, ApplyRecord, RecordStatus, apply_with, plain};
use nomnom_core::verdict::{Assessment, Disposition, Provenance, Verdict};

use crate::pack_icon;
use crate::session::{Assessed, Phase, Session};
use crate::state::{Preview, Selection, size};

/// The panel's fixed width; the tree and treemap take the rest.
pub const PANEL_WIDTH: f32 = 420.;

/// Three lines per row: a rule's title, pack and tally, or a match's path,
/// reason and exclusion state.
const ROW_HEIGHT: f32 = 62.;

/// Asks the window to select `0` in the tree, opening its ancestors.
pub struct Reveal(pub PathBuf);

pub struct SuggestPanel {
    session: Entity<Session>,
    selection: Selection,
    /// How many paths the approved rules plan; kept beside `preview`.
    checked: usize,
    preview: Option<Result<Rc<Preview>, String>>,
    /// The running or last apply's log, until dismissed.
    log: Option<ApplyLog>,
    /// Rules whose matches are listed under them.
    expanded: HashSet<Provenance>,
    /// The last exclusion list read or write that failed.
    exclusion_error: Option<String>,
}

impl EventEmitter<Reveal> for SuggestPanel {}

impl SuggestPanel {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&session, |this, _, _: &Assessed, cx| this.reassessed(cx)).detach();
        let mut this = Self {
            session,
            selection: Selection::default(),
            checked: 0,
            preview: None,
            log: None,
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

    /// Whether Reclaim has anything the user checked to apply.
    fn can_apply(&self) -> bool {
        matches!(&self.preview, Some(Ok(preview)) if !preview.plan.is_empty())
    }

    /// The dialog lists every action of the dry run, so what the user
    /// confirms is exactly what will move.
    fn confirm_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(preview)) = &self.preview else { return };
        if preview.plan.is_empty() {
            return;
        }
        let preview = preview.clone();
        let (len, bytes) = (preview.plan.len(), preview.plan.total_bytes());
        let body = format!(
            "{len} paths, {} in total, will be permanently deleted. This cannot be undone.",
            size(bytes)
        );
        let view = cx.entity();
        // An alert dialog, because only it renders the default `DialogFooter`
        // (right-aligned Cancel, then OK); a plain `Dialog` drops `button_props`.
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let view = view.clone();
            dialog
                .title("Apply this cleanup?")
                .description(body.clone())
                .child(render_plan(&preview, cx))
                .width(px(448.))
                .confirm()
                .ok_text("Delete permanently")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
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
        self.log = Some(ApplyLog::new(&preview));
        let plan = preview.plan.clone();
        let (sender, records) = mpsc::channel::<ApplyRecord>();
        let task = cx.background_executor().spawn(async move {
            apply_with(&plan, |record| {
                let _ = sender.send(record.clone());
            })
            .map_err(|error| format!("apply under {} failed: {error}", plain(plan.root())))
        });
        cx.spawn(async move |this, cx| {
            // Drained on a short timer, one repaint per batch, so a run of many
            // small deletions does not repaint once per path. The sender drops
            // when the apply returns, which ends the loop after the last batch.
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let mut batch = Vec::new();
                let finished = loop {
                    match records.try_recv() {
                        Ok(record) => batch.push(record),
                        Err(TryRecvError::Empty) => break false,
                        Err(TryRecvError::Disconnected) => break true,
                    }
                };
                if !batch.is_empty() {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(log) = &mut this.log {
                            log.record(batch);
                        }
                        cx.notify();
                    });
                }
                if finished {
                    break;
                }
            }
            let applied = task.await;
            match &applied {
                Ok(report) => {
                    for record in report.failures() {
                        if let RecordStatus::Failed { message } = &record.status {
                            eprintln!("nomnom-gui: apply failed on {}: {message}", plain(&record.source));
                        }
                    }
                }
                Err(message) => eprintln!("nomnom-gui: {message}"),
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(log) = &mut this.log {
                    log.finished = Some(applied.map(|_| ()));
                    log.scroll.scroll_to_bottom();
                }
                // What was approved is applied; the next plan starts empty.
                this.selection.clear();
                this.refresh_preview(cx);
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

    /// The live apply log: a bar over paths and bytes done, every path's
    /// outcome as it lands, and once finished a summary and Dismiss.
    fn render_log(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let log = self.log.as_ref()?;
        let fraction = if log.total_bytes > 0 {
            log.bytes_done as f32 / log.total_bytes as f32
        } else if log.total > 0 {
            log.done as f32 / log.total as f32
        } else {
            1.
        };
        let count = |wanted: fn(&LogStatus) -> bool| log.lines.iter().filter(|l| wanted(&l.status)).count();
        let (done, failed, skipped) = (
            count(|s| matches!(s, LogStatus::Done)),
            count(|s| matches!(s, LogStatus::Failed(_))),
            count(|s| matches!(s, LogStatus::Skipped(_))),
        );
        let headline = match &log.finished {
            None => format!(
                "Deleting… {}/{} paths · {} of {}",
                log.done,
                log.total,
                size(log.bytes_done),
                size(log.total_bytes)
            ),
            Some(Ok(())) => format!(
                "Finished: {done} done, {failed} failed, {skipped} skipped · {} freed",
                size(log.freed)
            ),
            Some(Err(message)) => message.clone(),
        };
        let theme = cx.theme();
        let (muted, danger, warning, border) =
            (theme.muted_foreground, theme.danger, theme.warning, theme.border);
        let failed_run = matches!(log.finished, Some(Err(_))) || failed > 0;
        Some(
            v_flex()
                .gap_2()
                .p_3()
                .border_1()
                .border_color(border)
                .rounded_md()
                .text_sm()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .font_weight(FontWeight::SEMIBOLD)
                                .when(failed_run, |d| d.text_color(danger))
                                .child(headline),
                        )
                        .when(log.finished.is_some(), |row| {
                            row.child(
                                Button::new("dismiss-apply-log")
                                    .small()
                                    .ghost()
                                    .label("Dismiss")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.log = None;
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .child(Progress::new("apply-progress").value(fraction * 100.))
                .child(
                    div()
                        .id("apply-log")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .track_scroll(&log.scroll)
                        .text_xs()
                        .children(log.lines.iter().map(|line| {
                            let (label, color, note) = match &line.status {
                                LogStatus::Done => (line.done_label, None, None),
                                LogStatus::Failed(message) => ("failed", Some(danger), Some(message)),
                                LogStatus::Skipped(message) => ("skipped", Some(warning), Some(message)),
                            };
                            v_flex()
                                .py_0p5()
                                .when_some(color, |row, color| row.text_color(color))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().w(px(56.)).child(label))
                                        .child(
                                            div()
                                                .flex_1()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(plain(&line.path)),
                                        )
                                        .child(div().text_color(muted).child(size(line.bytes))),
                                )
                                .when_some(note, |row, note| {
                                    row.child(div().pl(px(64.)).child(note.clone()))
                                })
                        })),
                )
                .into_any_element(),
        )
    }
}

/// What became of one path, as the apply log shows it.
enum LogStatus {
    Done,
    Failed(String),
    /// Refused by the plan's guards before the apply started.
    Skipped(String),
}

struct LogLine {
    /// The past tense of the action, shown for `Done`.
    done_label: &'static str,
    path: PathBuf,
    bytes: u64,
    status: LogStatus,
}

/// One apply's live log, kept on screen past the post-apply rescan until the
/// user dismisses it.
struct ApplyLog {
    total: usize,
    total_bytes: u64,
    done: usize,
    bytes_done: u64,
    /// Bytes of the actions that succeeded.
    freed: u64,
    lines: Vec<LogLine>,
    /// `None` while the plan runs; `Err` when it was refused before anything
    /// moved.
    finished: Option<Result<(), String>>,
    scroll: ScrollHandle,
}

impl ApplyLog {
    fn new(preview: &Preview) -> Self {
        let lines = preview
            .refused
            .iter()
            .map(|(path, error)| LogLine {
                done_label: "",
                path: path.clone(),
                bytes: 0,
                status: LogStatus::Skipped(error.to_string()),
            })
            .collect();
        Self {
            total: preview.plan.len(),
            total_bytes: preview.plan.total_bytes(),
            done: 0,
            bytes_done: 0,
            freed: 0,
            lines,
            finished: None,
            scroll: ScrollHandle::new(),
        }
    }

    fn record(&mut self, records: Vec<ApplyRecord>) {
        for record in records {
            self.done += 1;
            self.bytes_done += record.bytes;
            if record.succeeded() {
                self.freed += record.bytes;
            }
            self.lines.push(LogLine {
                done_label: match record.kind {
                    ActionKind::Delete => "deleted",
                    ActionKind::Archive => "archived",
                    ActionKind::Move => "moved",
                },
                path: record.source,
                bytes: record.bytes,
                status: match record.status {
                    RecordStatus::Succeeded => LogStatus::Done,
                    RecordStatus::Failed { message } => LogStatus::Failed(message),
                },
            });
        }
        self.scroll.scroll_to_bottom();
    }
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Delete { .. } => "delete",
        Action::Archive { .. } => "archive",
        Action::Move { .. } => "move",
    }
}

/// The dry run's actions, then the paths its guards refused.
fn render_plan(preview: &Preview, cx: &App) -> AnyElement {
    v_flex()
        .id("plan-list")
        .max_h(px(320.))
        .overflow_y_scrollbar()
        .text_xs()
        .children(preview.plan.actions().iter().map(|entry| {
            h_flex()
                .gap_2()
                .py_0p5()
                .child(div().w(px(56.)).child(verb(&entry.action)))
                .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(
                    match entry.action.destination() {
                        Some(to) => format!("{} → {}", plain(entry.action.path()), plain(to)),
                        None => plain(entry.action.path()),
                    },
                ))
                .child(div().child(size(entry.bytes)))
        }))
        .children(preview.refused.iter().map(|(path, error)| {
            div().text_color(cx.theme().warning).child(format!("skipping {}: {error}", plain(path)))
        }))
        .into_any_element()
}

/// Every panel item is a deletion suggestion, so only the exception that
/// asks for extra care is tagged.
fn disposition_tag(disposition: Disposition) -> Option<Tag> {
    match disposition {
        Disposition::Review => Some(Tag::warning().small().child("review")),
        Disposition::Reclaimable | Disposition::Keep => None,
    }
}

/// Labels are open — a pack can introduce its own — so they render as
/// themselves, hyphens read as spaces.
fn label_name(label: &nomnom_core::verdict::Label) -> String {
    label.as_str().replace('-', " ")
}

/// The panel's body before an assessment exists. Judging starts by itself
/// when a scan lands, so there is nothing to click here.
fn waiting_for_assessment(session: &Session, cx: &App) -> Option<AnyElement> {
    if session.assessment.is_some() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    let body = v_flex().gap_3().text_sm();
    Some(
        if session.assessing {
            body.child(
                h_flex()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child("Analyzing — judging what each path is…"),
            )
        } else if let Some(error) = session.assess_error.clone() {
            body.child(Alert::error("assess-error", error))
        } else {
            body.text_color(muted).child("No analysis yet.")
        }
        .into_any_element(),
    )
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

fn rows(this: &SuggestPanel, assessment: &Assessment) -> Vec<Row> {
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
    this: &SuggestPanel,
    ix: usize,
    row: &Row,
    cx: &mut Context<SuggestPanel>,
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
                .children(disposition_tag(*disposition))
                .child(pack_icon::tile(
                    this.session.read(cx).pack_icons.get(&provenance.pack).cloned(),
                    cx,
                ))
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
            let reveal = target.clone();
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
                        .id(("reveal", ix))
                        .flex_1()
                        .overflow_hidden()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().list_hover))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(Reveal(reveal.clone()))))
                        .when(is_excluded, |col| col.text_color(muted))
                        .child(div().whitespace_nowrap().text_ellipsis().child(path.clone()))
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
fn render_exclusions(this: &SuggestPanel, cx: &mut Context<SuggestPanel>) -> Option<AnyElement> {
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

impl Render for SuggestPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let session = self.session.read(cx);
        let busy = session.busy.is_some();
        let waiting = waiting_for_assessment(session, cx);
        let assessment = session.assessment.clone();

        let panel = v_flex()
            .size_full()
            .gap_3()
            .p_3()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Suggestions to review"))
            .children(self.render_log(cx));
        let Some(assessment) = assessment.filter(|_| waiting.is_none()) else {
            return panel.children(waiting).into_any_element();
        };

        let rows: Rc<Vec<Row>> = Rc::new(rows(self, &assessment));
        let rule_count = rows.iter().filter(|row| matches!(row, Row::Rule { .. })).count();

        let toggle = Checkbox::new("include-review")
            .label("Include review")
            .checked(self.selection.include_review)
            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                this.selection.include_review = *checked;
                this.refresh_preview(cx);
            }));

        let list = if rows.is_empty() {
            div()
                .flex_1()
                .text_sm()
                .text_color(muted)
                .child("Nothing matched: no rule suggests anything on this drive.")
                .into_any_element()
        } else {
            let rows = rows.clone();
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "suggest-rules",
                        rows.len(),
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range.map(|ix| render_row(this, ix, &rows[ix], cx)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
                .into_any_element()
        };

        let summary = match &self.preview {
            Some(Ok(preview)) if preview.plan.is_empty() => {
                "Nothing approved. Approve the rules whose matches should go; open one to                  see its matches and exclude any that must stay."
                    .to_string()
            }
            Some(Ok(preview)) => format!(
                "{} paths approved. Dry run: {} actions. Nothing has been touched.",
                self.checked,
                preview.plan.len(),
            ),
            Some(Err(message)) => message.clone(),
            None => String::new(),
        };
        let bytes = match &self.preview {
            Some(Ok(preview)) => preview.plan.total_bytes(),
            _ => 0,
        };
        let reclaim = Button::new("reclaim")
            .danger()
            .label(format!("Reclaim {}", size(bytes)))
            .disabled(busy || !self.can_apply())
            .on_click(cx.listener(|this, _, window, cx| this.confirm_apply(window, cx)));

        panel
            .child(
                v_flex()
                    .gap_1()
                    .child(toggle)
                    .child(div().text_xs().text_color(muted).child(
                        "Also list paths the evidence does not carry on its own.",
                    )),
            )
            .child(div().text_xs().text_color(muted).child(format!("Rules ({rule_count})")))
            .child(list)
            .children(render_exclusions(self, cx))
            .child(
                v_flex()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child(summary))
                    .child(h_flex().child(div().flex_1()).child(reclaim)),
            )
            .into_any_element()
    }
}
