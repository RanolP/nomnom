//! Largest files: the biggest single files anywhere in the scan. Clicking one
//! finds it in the Tree.

use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::plain;
use nomnom_core::catalog::NodeId;

use crate::session::{ScanData, Session};
use crate::state::{modified, size};

const ROW_HEIGHT: f32 = 26.;
const SIZE_W: f32 = 100.;
const MODIFIED_W: f32 = 140.;

/// Asks the window to select a node in the Tree screen.
pub struct Reveal(pub NodeId);

pub struct LargestScreen {
    session: Entity<Session>,
}

impl EventEmitter<Reveal> for LargestScreen {}

impl LargestScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }
}

impl Render for LargestScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(data) = self.session.read(cx).scan.clone() else {
            return v_flex()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("No scan yet.")
                .into_any_element();
        };
        let rows = data.largest.len();
        let list_data = data.clone();
        let list = uniform_list(
            "largest-files",
            rows,
            cx.processor(move |_, range: Range<usize>, _, cx| {
                range.map(|ix| render_row(&list_data, ix, cx)).collect::<Vec<_>>()
            }),
        )
        .size_full();

        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("The {rows} largest files — click one to find it in the tree")),
            )
            .child(
                h_flex()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().flex_1().child("Path"))
                    .child(div().w(px(SIZE_W)).text_right().child("Size"))
                    .child(div().w(px(MODIFIED_W)).text_right().child("Modified")),
            )
            .child(div().flex_1().min_h_0().child(list))
            .into_any_element()
    }
}

fn render_row(data: &Arc<ScanData>, ix: usize, cx: &mut Context<LargestScreen>) -> AnyElement {
    let id = data.largest[ix];
    let node = data.catalog.node(id);
    h_flex()
        .id(("largest", ix))
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .on_click(cx.listener(move |_, _, _, cx| cx.emit(Reveal(id))))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(plain(&data.catalog.path(id))),
        )
        .child(div().w(px(SIZE_W)).text_right().child(size(node.size)))
        .child(
            div()
                .w(px(MODIFIED_W))
                .text_right()
                .text_xs()
                .child(node.modified.map(modified).unwrap_or_default()),
        )
        .into_any_element()
}
