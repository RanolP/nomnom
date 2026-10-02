//! Undo: every journal an apply wrote, newest first, and what reversing one
//! actually put back.

use std::path::PathBuf;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::{JournalEntry, UndoReport, list_journals, plain, undo};

use crate::session::{Phase, Session};
use crate::state::size;

pub struct UndoScreen {
    session: Entity<Session>,
    journals: Option<Result<Vec<JournalEntry>, String>>,
    report: Option<Result<UndoReport, String>>,
    /// The session's phase at the last observation, so the list reloads when
    /// an apply finishes and a new journal exists.
    last_busy: Option<Phase>,
}

impl UndoScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |this, session, cx| {
            let busy = session.read(cx).busy;
            if this.last_busy.is_some() && busy.is_none() {
                this.reload(cx);
            }
            this.last_busy = busy;
            cx.notify();
        })
        .detach();
        let mut this = Self { session, journals: None, report: None, last_busy: None };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let listed =
                cx.background_executor().spawn(async move { list_journals() }).await.map_err(
                    |error| {
                        let message = format!("cannot list journals: {error}");
                        eprintln!("nomnom-gui: {message}");
                        message
                    },
                );
            let _ = this.update(cx, |this, cx| {
                this.journals = Some(listed);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_undo(&mut self, journal: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        let description = format!(
            "Every path this apply moved is put back where it was. A path that has \
             since been re-created is reported as a conflict and left alone.\n\n{}",
            plain(&journal)
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            let journal = journal.clone();
            alert
                .title("Undo this apply?")
                .description(description.clone())
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    view.update(cx, |this, cx| this.undo(journal.clone(), cx));
                    true
                })
        });
    }

    fn undo(&mut self, journal: PathBuf, cx: &mut Context<Self>) {
        let session = self.session.clone();
        if !session.update(cx, |session, cx| session.begin(Phase::Undoing, cx)) {
            return;
        }
        self.report = None;
        cx.spawn(async move |this, cx| {
            let undo_path = journal.clone();
            let report = cx
                .background_executor()
                .spawn(async move { undo(&undo_path) })
                .await
                .map_err(|error| format!("cannot undo {}: {error}", plain(&journal)));
            match &report {
                Ok(report) => {
                    for conflict in &report.conflicts {
                        eprintln!(
                            "nomnom-gui: undo conflict at {}: {}",
                            plain(&conflict.path),
                            conflict.message
                        );
                    }
                    for failure in &report.failures {
                        eprintln!(
                            "nomnom-gui: undo failed for {}: {}",
                            plain(&failure.path),
                            failure.message
                        );
                    }
                }
                Err(message) => eprintln!("nomnom-gui: {message}"),
            }
            let _ = this.update(cx, |this, cx| {
                this.report = Some(report);
                cx.notify();
            });
            // Restored paths change what the open scan shows.
            session.update(cx, |session, cx| {
                session.end(cx);
                if session.catalog.is_some() {
                    session.scan(cx);
                }
            });
        })
        .detach();
    }

    fn render_report(&self, cx: &App) -> Option<AnyElement> {
        Some(match self.report.as_ref()? {
            Err(message) => Alert::error("undo-error", message.clone()).into_any_element(),
            Ok(report) => {
                let line = |kind: &'static str, path: &PathBuf, detail: &str, color: Hsla| {
                    div()
                        .text_xs()
                        .text_color(color)
                        .child(format!("{kind:<9} {}  {detail}", plain(path)))
                };
                let theme = cx.theme();
                v_flex()
                    .id("undo-report")
                    .max_h(px(260.))
                    .overflow_y_scrollbar()
                    .gap_0p5()
                    .p_3()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                        "Restored {} across {} paths; {} skipped, {} conflicts, {} failures.",
                        size(report.bytes_restored),
                        report.restored.len(),
                        report.skipped.len(),
                        report.conflicts.len(),
                        report.failures.len()
                    )))
                    .child(
                        div().text_xs().child(format!("Journal: {}", plain(&report.journal_path))),
                    )
                    .children(
                        report
                            .restored
                            .iter()
                            .map(|r| line("restored", &r.path, &r.reason, theme.foreground)),
                    )
                    .children(
                        report
                            .skipped
                            .iter()
                            .map(|r| line("skipped", &r.path, &r.reason, theme.muted_foreground)),
                    )
                    .children(
                        report
                            .conflicts
                            .iter()
                            .map(|r| line("CONFLICT", &r.path, &r.message, theme.warning)),
                    )
                    .children(
                        report
                            .failures
                            .iter()
                            .map(|r| line("FAILED", &r.path, &r.message, theme.danger)),
                    )
                    .into_any_element()
            }
        })
    }
}

/// `YYYY-MM-DD HH:MM UTC` from Unix seconds, without a date crate.
fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3_600, rem % 3_600 / 60)
}

impl Render for UndoScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.session.read(cx).busy;
        let header = h_flex()
            .gap_2()
            .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Journals"))
            .when(busy == Some(Phase::Undoing), |row| {
                row.child(Spinner::new().small()).child(Phase::Undoing.label())
            })
            .child(
                Button::new("reload-journals")
                    .small()
                    .ghost()
                    .label("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );

        let list = match &self.journals {
            None => div().child(Spinner::new()).into_any_element(),
            Some(Err(message)) => {
                Alert::error("journals-error", message.clone()).into_any_element()
            }
            Some(Ok(journals)) if journals.is_empty() => div()
                .text_color(cx.theme().muted_foreground)
                .child("No journals yet: nothing has been applied.")
                .into_any_element(),
            Some(Ok(journals)) => v_flex()
                .id("journal-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .children(journals.iter().enumerate().map(|(ix, entry)| {
                    let (summary, undoable) = match &entry.summary {
                        Ok(s) => (
                            format!(
                                "{} — {} actions: {} done, {} failed, {} undone; {} reclaimed",
                                plain(&s.root),
                                s.actions,
                                s.succeeded,
                                s.failed,
                                s.undone,
                                size(s.bytes_reclaimed)
                            ),
                            s.succeeded > 0,
                        ),
                        Err(error) => (format!("unreadable: {error}"), false),
                    };
                    let journal = entry.path.clone();
                    h_flex()
                        .gap_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .text_sm()
                        .child(
                            v_flex()
                                .flex_1()
                                .overflow_hidden()
                                .child(div().child(utc(entry.started_at)))
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(summary),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .whitespace_nowrap()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(plain(&entry.path)),
                                ),
                        )
                        .child(
                            Button::new(("undo", ix))
                                .small()
                                .outline()
                                .label("Undo…")
                                .disabled(!undoable || busy.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_undo(journal.clone(), window, cx)
                                })),
                        )
                }))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .children(self.render_report(cx))
            .child(header)
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    // Catches journals listed under the wrong day, which makes picking the
    // apply to reverse a guess.
    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(super::utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::utc(951_782_400), "2000-02-29 00:00 UTC");
    }
}
