//! Scan: the tree, biggest first, expanded one level at a time.

use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::scan::{BackendUsed, EntryKind};

use crate::session::Session;
use crate::state::{TreeModel, size};

pub struct ScanScreen {
    session: Entity<Session>,
    /// The catalog the tree was built from, so a rescan is noticed by pointer.
    built_from: Option<Arc<Catalog>>,
    tree: TreeModel,
    show_errors: bool,
}

impl ScanScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session, built_from: None, tree: TreeModel::default(), show_errors: false }
    }

    fn sync(&mut self, cx: &App) -> Option<Arc<Catalog>> {
        let catalog = self.session.read(cx).catalog.clone()?;
        if !self.built_from.as_ref().is_some_and(|built| Arc::ptr_eq(built, &catalog)) {
            self.tree = TreeModel::new(&catalog);
            self.built_from = Some(catalog.clone());
            self.show_errors = false;
        }
        Some(catalog)
    }
}

impl Render for ScanScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        if let Some(error) = &session.scan_error {
            return v_flex()
                .p_4()
                .child(Alert::error("scan-error", error.clone()))
                .into_any_element();
        }
        let Some(catalog) = self.sync(cx) else {
            let message = if self.session.read(cx).root.is_none() {
                "Choose a folder to scan."
            } else {
                "No scan yet."
            };
            return v_flex()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(message)
                .into_any_element();
        };

        let root = catalog.node(catalog.root());
        let summary = format!(
            "{}  —  {}  ({} files, {} dirs)",
            catalog.path(catalog.root()).display(),
            size(root.subtree_size),
            root.file_count,
            root.dir_count
        );

        let mut banners = v_flex().gap_2();
        if let BackendUsed::Walk { mft_unavailable: Some(reason) } = catalog.backend_used() {
            banners = banners.child(Alert::warning(
                "mft-skipped",
                format!(
                    "The fast MFT scan was skipped ({reason}). Run nomnom-gui as Administrator \
                     for the fast path."
                ),
            ));
        }
        let errors = catalog.errors();
        if !errors.is_empty() {
            let toggle = Button::new("toggle-errors")
                .small()
                .ghost()
                .label(if self.show_errors { "Hide" } else { "Show" })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_errors = !this.show_errors;
                    cx.notify();
                }));
            banners = banners.child(
                h_flex()
                    .gap_2()
                    .child(Alert::warning(
                        "scan-errors",
                        format!(
                            "{} entries could not be read; totals are under-counted.",
                            errors.len()
                        ),
                    ))
                    .child(toggle),
            );
            if self.show_errors {
                banners = banners.child(
                    v_flex()
                        .id("error-list")
                        .max_h(px(160.))
                        .overflow_y_scrollbar()
                        .text_xs()
                        .children(errors.iter().map(|error| match &error.path {
                            Some(path) => format!("{}: {}", path.display(), error.message),
                            None => error.message.clone(),
                        })),
                );
            }
        }

        let rows = self.tree.rows().len();
        let list = uniform_list(
            "scan-tree",
            rows,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range.map(|ix| this.render_row(&catalog, ix, cx)).collect::<Vec<_>>()
            }),
        )
        .size_full();

        v_flex()
            .size_full()
            .gap_2()
            .p_4()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(summary))
            .child(banners)
            .child(
                h_flex()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().flex_1().child("Name"))
                    .child(div().w(px(110.)).text_right().child("Size"))
                    .child(div().w(px(90.)).text_right().child("Files"))
                    .child(div().w(px(90.)).text_right().child("Dirs")),
            )
            .child(div().flex_1().min_h_0().child(list))
            .into_any_element()
    }
}

impl ScanScreen {
    fn render_row(&self, catalog: &Arc<Catalog>, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self.tree.rows()[ix];
        tree_row(catalog, row.id, row.depth, self.tree.is_expanded(row.id), ix, cx)
    }
}

fn tree_row(
    catalog: &Arc<Catalog>,
    id: NodeId,
    depth: usize,
    expanded: bool,
    ix: usize,
    cx: &mut Context<ScanScreen>,
) -> AnyElement {
    let node = catalog.node(id);
    let is_dir = node.kind == EntryKind::Dir && !node.children.is_empty();
    let marker = match (is_dir, expanded) {
        (false, _) => "  ",
        (true, false) => "▸ ",
        (true, true) => "▾ ",
    };
    let suffix = if node.kind == EntryKind::Dir { "/" } else { "" };
    let toggle_catalog = catalog.clone();
    h_flex()
        .id(("tree-row", ix))
        .h(px(26.))
        .w_full()
        .px_2()
        .text_sm()
        .hover(|style| style.bg(cx.theme().list_hover))
        .when(is_dir, |row| {
            row.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                this.tree.toggle(&toggle_catalog, id);
                cx.notify();
            }))
        })
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .pl(px(depth as f32 * 16.))
                .child(format!("{marker}{}{suffix}", node.name.to_string_lossy())),
        )
        .child(div().w(px(110.)).text_right().child(size(node.subtree_size)))
        .child(div().w(px(90.)).text_right().child(node.file_count.to_string()))
        .child(div().w(px(90.)).text_right().child(node.dir_count.to_string()))
        .into_any_element()
}
