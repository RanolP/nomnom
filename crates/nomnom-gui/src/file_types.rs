//! File types: the drive broken down by extension, each swatch the colour
//! that extension has in the treemap.

use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::session::{ScanData, Session};
use crate::state::{count, size};

const ROW_HEIGHT: f32 = 26.;
const SIZE_W: f32 = 100.;
const PERCENT_W: f32 = 80.;
const COUNT_W: f32 = 100.;

pub struct FileTypesScreen {
    session: Entity<Session>,
}

impl FileTypesScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }
}

impl Render for FileTypesScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(data) = self.session.read(cx).scan.clone() else {
            return v_flex()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("No scan yet.")
                .into_any_element();
        };
        let total = data.catalog.node(data.catalog.root()).subtree_size;
        let rows = data.file_types.len();
        let list_data = data.clone();
        let list = uniform_list(
            "file-types",
            rows,
            cx.processor(move |_, range: Range<usize>, _, cx| {
                range.map(|ix| render_row(&list_data, total, ix, cx)).collect::<Vec<_>>()
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
                    .child(format!("{} file types", count(rows as u64))),
            )
            .child(
                h_flex()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().w(px(24.)))
                    .child(div().flex_1().child("Extension"))
                    .child(div().w(px(SIZE_W)).text_right().child("Size"))
                    .child(div().w(px(PERCENT_W)).text_right().child("%"))
                    .child(div().w(px(COUNT_W)).text_right().child("Files")),
            )
            .child(div().flex_1().min_h_0().child(list))
            .into_any_element()
    }
}

fn render_row(data: &Arc<ScanData>, total: u64, ix: usize, cx: &App) -> AnyElement {
    let ty = &data.file_types[ix];
    let share = if total == 0 { 0. } else { ty.bytes as f64 * 100. / total as f64 };
    h_flex()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .text_sm()
        .hover(|style| style.bg(cx.theme().list_hover))
        .child(
            div()
                .w(px(24.))
                .child(div().size(px(12.)).rounded_sm().bg(data.palette.color(&ty.ext))),
        )
        .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(ty.ext.clone()))
        .child(div().w(px(SIZE_W)).text_right().child(size(ty.bytes)))
        .child(div().w(px(PERCENT_W)).text_right().child(format!("{share:.1} %")))
        .child(div().w(px(COUNT_W)).text_right().child(count(ty.count)))
        .into_any_element()
}
