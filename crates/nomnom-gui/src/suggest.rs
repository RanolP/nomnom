//! What the judge says about a path, rendered the same wherever a verdict
//! shows: the disposition tag, the label's name, and the states shown before
//! an assessment exists.

use gpui_kit::component::alert::Alert;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::verdict::Disposition;

use crate::session::{Phase, Session};

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

/// The states of a view that needs an assessment before it has one. Judging
/// starts by itself when a scan lands, so there is nothing to click here.
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
    let panel = if state.assessing {
        panel.child(
            h_flex().gap_2().child(Spinner::new().small()).child("Judging what each path is…"),
        )
    } else if let Some(error) = state.assess_error.clone() {
        panel.child(Alert::error("assess-error", error))
    } else if state.busy == Some(Phase::Scanning) {
        panel.text_color(muted).child("Waiting for the scan to finish…")
    } else {
        panel.text_color(muted).child("Scan a drive first.")
    };
    Some(panel.into_any_element())
}
