//! Suggest: what each path is, and the sentence and rule behind the verdict.
//!
//! The reason and the `pack/rule` provenance are shown on every row, never
//! behind a click: with packs coming from the network, "who says so" is part
//! of what a human approves on.

use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::verdict::{Assessment, Disposition};

use crate::session::{Phase, Session};
use crate::state::size;

/// Every row is the same height so the list can be virtualized.
const ROW_HEIGHT: f32 = 78.;

#[derive(Clone, Copy)]
enum Row {
    Group(usize),
    Entry(usize, usize),
}

pub struct SuggestScreen {
    session: Entity<Session>,
}

impl SuggestScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }
}

fn rows(assessment: &Assessment) -> Vec<Row> {
    let mut rows = Vec::new();
    for (g, group) in assessment.groups.iter().enumerate() {
        rows.push(Row::Group(g));
        rows.extend((0..group.entries.len()).map(|e| Row::Entry(g, e)));
    }
    rows
}

pub fn disposition_tag(disposition: Disposition) -> Tag {
    match disposition {
        Disposition::Reclaimable => Tag::success().small().child("reclaimable"),
        Disposition::Review => Tag::warning().small().child("review"),
        Disposition::Keep => Tag::secondary().small().child("keep"),
    }
}

/// Labels are open — a pack can introduce its own — so they render as
/// themselves, hyphens read as spaces.
pub fn label_name(label: &nomnom_core::verdict::Label) -> String {
    label.as_str().replace('-', " ")
}

/// Shared by Suggest and Clean: the states of a screen that needs an
/// assessment before it has one, including the Analyze button that starts it.
/// Scans never judge on their own, because judging a whole drive hashes every
/// duplicate candidate.
pub fn waiting_for_assessment(session: &Entity<Session>, cx: &App) -> Option<AnyElement> {
    let state = session.read(cx);
    if let Some(error) = &state.scan_error {
        return Some(
            v_flex().p_4().child(Alert::error("scan-error", error.clone())).into_any_element(),
        );
    }
    if state.assessment.is_some() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    let panel = v_flex().p_4().gap_3().max_w(px(640.));
    let panel = match state.busy {
        Some(Phase::Assessing) => panel.child(
            h_flex()
                .gap_2()
                .child(Spinner::new().small())
                .child(Phase::Assessing.label())
                .child(div().text_color(muted).child("Duplicate hashing can take minutes.")),
        ),
        Some(Phase::Scanning) => panel.text_color(muted).child("Waiting for the scan to finish…"),
        _ if state.scan.is_none() => panel.text_color(muted).child("Scan a drive first."),
        busy => {
            let session = session.clone();
            panel
                .when_some(state.assess_error.clone(), |panel, error| {
                    panel.child(Alert::error("assess-error", error))
                })
                .child(div().text_color(muted).child(
                    "Suggest and Clean need an analysis: every path is judged against the rule \
                     packs, and files of equal size are hashed to find duplicates. On a whole \
                     drive this can take several minutes.",
                ))
                .child(
                    Button::new("analyze")
                        .primary()
                        .label("Analyze")
                        .disabled(busy.is_some())
                        .on_click(move |_, _, cx| {
                            session.update(cx, |session, cx| session.assess(cx))
                        }),
                )
        }
    };
    Some(panel.into_any_element())
}

impl Render for SuggestScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(waiting) = waiting_for_assessment(&self.session, cx) {
            return waiting;
        }
        let session = self.session.read(cx);
        let assessment = session.assessment.clone().expect("checked above");
        if assessment.groups.is_empty() {
            return v_flex()
                .p_4()
                .child(format!("Nothing to suggest under {}.", assessment.root.display()))
                .into_any_element();
        }

        let rows = Arc::new(rows(&assessment));
        let list_assessment = assessment.clone();
        let list = uniform_list(
            "suggest-list",
            rows.len(),
            cx.processor(move |_, range: Range<usize>, _, cx| {
                range.map(|ix| render_row(&list_assessment, rows[ix], cx)).collect::<Vec<_>>()
            }),
        )
        .size_full();

        v_flex()
            .size_full()
            .gap_2()
            .p_4()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Reclaimable: {}", size(assessment.reclaimable_bytes))),
            )
            .child(div().flex_1().min_h_0().child(list))
            .into_any_element()
    }
}

fn render_row(assessment: &Assessment, row: Row, cx: &App) -> AnyElement {
    match row {
        Row::Group(g) => {
            let group = &assessment.groups[g];
            let count = group.entries.len();
            h_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .items_end()
                .pb_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!(
                    "{} — {} across {count} {}",
                    label_name(&group.label),
                    size(group.bytes),
                    if count == 1 { "path" } else { "paths" }
                ))
                .into_any_element()
        }
        Row::Entry(g, e) => {
            let entry = &assessment.groups[g].entries[e];
            let verdict = &entry.verdict;
            v_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .justify_center()
                .px_2()
                .gap_0p5()
                .text_sm()
                .child(
                    h_flex()
                        .gap_2()
                        .child(disposition_tag(verdict.disposition))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(entry.path.clone()),
                        )
                        .child(div().child(size(entry.bytes))),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(cx.theme().muted_foreground)
                        .child(verdict.reason.clone()),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("— {}", verdict.provenance))
                        // A capped verdict looks exactly like one written as
                        // `review`; without this the missing trust grant is
                        // invisible.
                        .when_some(verdict.capped.clone(), |row, capped| {
                            row.child(
                                div().text_color(cx.theme().warning).child(format!("! {capped}")),
                            )
                        }),
                )
                .into_any_element()
        }
    }
}
