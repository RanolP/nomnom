//! Drives: the start screen. One card per fixed drive; clicking one scans the
//! whole drive, the way WizTree opens.

use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::Feature;
use nomnom_core::action::plain;
use nomnom_core::scan::{Volume, is_elevated, volumes};

use crate::app::Navigate;
use crate::session::Session;
use crate::state::size;

pub struct DrivesScreen {
    session: Entity<Session>,
    volumes: Option<Vec<Volume>>,
}

impl EventEmitter<Navigate> for DrivesScreen {}

impl DrivesScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        // A sleeping disk can take seconds to report its label.
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { volumes() }).await;
            let _ = this.update(cx, |this, cx| {
                this.volumes = Some(found);
                cx.notify();
            });
        })
        .detach();
        Self { session, volumes: None }
    }

    fn scan(&mut self, volume: Volume, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.scan_volume(volume, cx));
        cx.emit(Navigate(Feature::Tree));
    }

    fn render_card(
        &self,
        ix: usize,
        volume: &Volume,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let used = volume.total.saturating_sub(volume.free);
        let share = if volume.total == 0 { 0. } else { used as f32 / volume.total as f32 };
        let letter = plain(&volume.root).trim_end_matches('\\').to_string();
        let label =
            if volume.label.is_empty() { "Local Disk".to_string() } else { volume.label.clone() };
        let picked = volume.clone();
        let theme = cx.theme();

        v_flex()
            .id(("drive", ix))
            .w(px(280.))
            .gap_2()
            .p_4()
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .bg(theme.secondary)
            .when(!busy, |card| {
                card.cursor_pointer()
                    .hover(|style| style.border_color(theme.primary))
                    .on_click(cx.listener(move |this, _, _, cx| this.scan(picked.clone(), cx)))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_2xl().font_weight(FontWeight::BOLD).child(letter))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(label))
                    .child(
                        div().text_xs().text_color(theme.muted_foreground).child(volume.fs.clone()),
                    ),
            )
            .child(Progress::new(("drive-used", ix)).value(share * 100.))
            .child(
                h_flex()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(format!(
                        "{} used of {}",
                        size(used),
                        size(volume.total)
                    )))
                    .child(format!("{} free", size(volume.free))),
            )
    }
}

impl Render for DrivesScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.session.read(cx).busy.is_some();
        let cards = match &self.volumes {
            None => vec![
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("Looking for drives…")
                    .into_any_element(),
            ],
            Some(found) if found.is_empty() => vec![
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No fixed drives found.")
                    .into_any_element(),
            ],
            Some(found) => found
                .iter()
                .enumerate()
                .map(|(ix, volume)| self.render_card(ix, volume, busy, cx).into_any_element())
                .collect(),
        };

        v_flex()
            .size_full()
            .gap_4()
            .p_6()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Pick a drive to scan"))
            .child(h_flex().flex_wrap().gap_4().children(cards))
            .when(!is_elevated(), |col| {
                col.child(div().max_w(px(720.)).text_sm().text_color(cx.theme().muted_foreground).child(
                    "Each scan asks for Administrator access so nomnom can read the NTFS Master \
                     File Table — much faster, and with on-disk sizes. Decline the prompt and \
                     the drive is walked file by file instead.",
                ))
            })
    }
}
