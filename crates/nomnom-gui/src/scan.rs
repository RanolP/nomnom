//! Tree: WizTree's split, the tree table on top and the treemap of the scan
//! root below, sharing one selection.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::plain;
use nomnom_core::catalog::NodeId;
use nomnom_core::scan::{BackendUsed, EntryKind};

use crate::session::{Phase, ScanData, Session};
use crate::state::{TreeModel, count, modified, size};
use crate::treemap::Treemap;

const ROW_HEIGHT: f32 = 24.;
const PERCENT_W: f32 = 130.;
const SIZE_W: f32 = 90.;
const COUNT_W: f32 = 80.;
const MODIFIED_W: f32 = 130.;

pub struct ScanScreen {
    session: Entity<Session>,
    /// The scan the tree was built from, so a rescan is noticed by pointer.
    built_from: Option<Arc<ScanData>>,
    tree: TreeModel,
    selected: Option<NodeId>,
    hovered: Option<NodeId>,
    treemap: Rc<RefCell<Treemap>>,
    scroll: UniformListScrollHandle,
    /// The CLI's `--show-errors`: list every entry the scan could not read.
    show_errors: bool,
}

impl ScanScreen {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            built_from: None,
            tree: TreeModel::default(),
            selected: None,
            hovered: None,
            treemap: Rc::default(),
            scroll: UniformListScrollHandle::new(),
            show_errors: false,
        }
    }

    fn sync(&mut self, cx: &App) -> Option<Arc<ScanData>> {
        let data = self.session.read(cx).scan.clone()?;
        if !self.built_from.as_ref().is_some_and(|built| Arc::ptr_eq(built, &data)) {
            self.tree = TreeModel::new(&data.catalog);
            self.built_from = Some(data.clone());
            self.selected = None;
            self.hovered = None;
            self.show_errors = false;
        }
        Some(data)
    }

    /// Select `id`, open the tree down to it and scroll it into view.
    pub fn reveal(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let Some(data) = self.sync(cx) else { return };
        self.selected = Some(id);
        if let Some(row) = self.tree.reveal(&data.catalog, id) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Repainted by the session's 100 ms ticker while the scan runs.
    fn render_progress(&self, session: &Session, cx: &App) -> AnyElement {
        let root = session.root.as_deref().map(plain).unwrap_or_default();
        let muted = cx.theme().muted_foreground;
        let (fraction, line) = match &session.progress {
            Some(progress) => (
                progress.fraction(),
                format!(
                    "{} entries · {:.1} s",
                    count(progress.entries()),
                    progress.started.elapsed().as_secs_f32()
                ),
            ),
            None => (None, String::new()),
        };
        let status = match fraction {
            // The MFT counts every record before the tree is assembled.
            Some(done) if done >= 1. => "Building the tree…".to_string(),
            Some(done) => format!("{:.1} %", done * 100.),
            None => "Starting…".to_string(),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("Scanning {root}")))
            .when_some(session.progress.as_ref().and_then(|p| p.notice()), |col, notice| {
                col.child(
                    div().w(px(480.)).child(Alert::warning("scan-notice", notice.to_string())),
                )
            })
            .map(|col| match fraction {
                Some(done) => col.child(
                    div()
                        .w(px(480.))
                        .child(Progress::new("scan-progress").value(done as f32 * 100.)),
                ),
                None => col.child(Spinner::new().large()),
            })
            .child(div().text_lg().child(status))
            .child(div().text_color(muted).child(line))
            .into_any_element()
    }

    fn render_banners(
        &self,
        data: &ScanData,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut banners = v_flex().gap_2();
        if let Some(notice) = notice {
            banners = banners.child(Alert::warning("scan-notice", notice));
        } else if let BackendUsed::Walk { mft_unavailable: Some(reason) } =
            data.catalog.backend_used()
        {
            banners = banners.child(Alert::warning(
                "mft-skipped",
                format!("The fast MFT scan was skipped ({reason}); the drive was walked instead."),
            ));
        }
        let errors = data.catalog.errors();
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
                    .child(div().flex_1().child(Alert::warning(
                        "scan-errors",
                        format!(
                            "{} entries could not be read; totals are under-counted.",
                            errors.len()
                        ),
                    )))
                    .child(toggle),
            );
            if self.show_errors {
                banners = banners.child(
                    v_flex()
                        .id("error-list")
                        .max_h(px(120.))
                        .overflow_y_scrollbar()
                        .text_xs()
                        .children(errors.iter().map(|error| match &error.path {
                            Some(path) => format!("{}: {}", path.display(), error.message),
                            None => error.message.clone(),
                        })),
                );
            }
        }
        banners
    }

    fn render_treemap(&self, data: &Arc<ScanData>, cx: &mut Context<Self>) -> AnyElement {
        let root = data.catalog.root();
        let (prepare_map, paint_map) = (self.treemap.clone(), self.treemap.clone());
        let (prepare_data, paint_data) = (data.clone(), data.clone());
        let (selected, hovered) = (self.selected, self.hovered);
        let map = canvas(
            move |bounds, _, _| prepare_map.borrow_mut().prepare(&prepare_data, root, bounds),
            move |_, (), window, _| {
                paint_map.borrow().paint(&paint_data.catalog, selected, hovered, window);
                paint_data.note_painted();
            },
        )
        .size_full();

        let status = match self.hovered {
            Some(id) => format!(
                "{}  —  {}",
                plain(&data.catalog.path(id)),
                size(data.catalog.node(id).subtree_size)
            ),
            None => "Hover the map to see a path; click to find it in the tree.".to_string(),
        };

        v_flex()
            .size_full()
            .gap_1()
            .child(
                div()
                    .id("treemap")
                    .flex_1()
                    .min_h_0()
                    .cursor_pointer()
                    .child(map)
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        let hit = this.treemap.borrow().hit(event.position);
                        if hit != this.hovered {
                            this.hovered = hit;
                            cx.notify();
                        }
                    }))
                    .on_hover(cx.listener(|this, inside: &bool, _, cx| {
                        if !inside && this.hovered.take().is_some() {
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            let hit = this.treemap.borrow().hit(event.position);
                            if let Some(id) = hit {
                                this.reveal(id, cx);
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .h(px(20.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            )
            .into_any_element()
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
        if session.busy == Some(Phase::Scanning) {
            return self.render_progress(session, cx);
        }
        let notice = session.scan_notice.clone();
        let Some(data) = self.sync(cx) else {
            return v_flex()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("Pick a drive on the Drives screen.")
                .into_any_element();
        };

        let catalog = &data.catalog;
        let root = catalog.node(catalog.root());
        let backend = match catalog.backend_used() {
            BackendUsed::Mft => "MFT",
            BackendUsed::Walk { .. } => "walk",
        };
        let summary = format!(
            "{}  —  {}  ({} files, {} dirs)",
            plain(&catalog.path(catalog.root())),
            size(root.subtree_size),
            count(root.file_count),
            count(root.dir_count)
        );
        let timing = format!("{backend} scan in {:.1} s", data.elapsed.as_secs_f32());
        let has_allocated = data.allocated.is_some();

        let rows = self.tree.rows().len();
        let list_data = data.clone();
        let list = uniform_list(
            "scan-tree",
            rows,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                range.map(|ix| this.render_row(&list_data, ix, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();

        let muted = cx.theme().muted_foreground;
        let header = h_flex()
            .px_2()
            .text_xs()
            .text_color(muted)
            .child(div().flex_1().child("Name"))
            .child(div().w(px(PERCENT_W)).child("% of parent"))
            .child(div().w(px(SIZE_W)).text_right().child("Size"))
            .when(has_allocated, |row| {
                row.child(div().w(px(SIZE_W)).text_right().child("Allocated"))
            })
            .child(div().w(px(COUNT_W)).text_right().child("Files"))
            .child(div().w(px(COUNT_W)).text_right().child("Dirs"))
            .child(div().w(px(MODIFIED_W)).text_right().child("Modified"));

        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(summary))
                    .child(div().text_xs().text_color(muted).child(timing)),
            )
            .child(self.render_banners(&data, notice, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(
                        v_flex()
                            .h(relative(0.55))
                            .min_h_0()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded_md()
                            .child(header)
                            .child(div().flex_1().min_h_0().child(list)),
                    )
                    .child(div().flex_1().min_h_0().child(self.render_treemap(&data, cx))),
            )
            .into_any_element()
    }
}

impl ScanScreen {
    fn render_row(&self, data: &Arc<ScanData>, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self.tree.rows()[ix];
        let id = row.id;
        let catalog = &data.catalog;
        let node = catalog.node(id);
        let expanded = self.tree.is_expanded(id);
        let is_dir = node.kind == EntryKind::Dir && !catalog.children(id).is_empty();
        let marker = match (is_dir, expanded) {
            (false, _) => "  ",
            (true, false) => "▸ ",
            (true, true) => "▾ ",
        };
        let suffix = if node.kind == EntryKind::Dir { "\\" } else { "" };
        let parent_size = node.parent.map_or(0, |parent| catalog.node(parent).subtree_size);
        let share =
            if parent_size == 0 { 0. } else { node.subtree_size as f32 / parent_size as f32 };
        let selected = self.selected == Some(id);
        let theme = cx.theme();
        let toggle_data = data.clone();

        h_flex()
            .id(("tree-row", ix))
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .text_sm()
            .cursor_pointer()
            .when(selected, |row| row.bg(theme.list_active))
            .when(!selected, |row| row.hover(|style| style.bg(theme.list_hover)))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                this.selected = Some(id);
                if is_dir && event.click_count() == 2 {
                    this.tree.toggle(&toggle_data.catalog, id);
                }
                cx.notify();
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .pl(px(row.depth as f32 * 16.))
                    .child(div().id(("tree-toggle", ix)).w(px(16.)).flex_none().child(marker).when(
                        is_dir,
                        |marker| {
                            let toggle_data = data.clone();
                            marker.on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.tree.toggle(&toggle_data.catalog, id);
                                cx.notify();
                            }))
                        },
                    ))
                    .child(format!("{}{suffix}", catalog.name(id).to_string_lossy())),
            )
            .child(
                h_flex()
                    .w(px(PERCENT_W))
                    .gap_1()
                    .child(
                        div()
                            .w(px(70.))
                            .h(px(10.))
                            .bg(theme.muted)
                            .child(div().h_full().w(relative(share)).bg(theme.chart_1)),
                    )
                    .child(div().text_xs().child(format!("{:.1} %", share * 100.))),
            )
            .child(div().w(px(SIZE_W)).text_right().child(size(node.subtree_size)))
            .when_some(data.allocated(id), |row, allocated| {
                row.child(div().w(px(SIZE_W)).text_right().child(size(allocated)))
            })
            .child(div().w(px(COUNT_W)).text_right().child(count(node.file_count)))
            .child(div().w(px(COUNT_W)).text_right().child(count(node.dir_count)))
            .child(
                div()
                    .w(px(MODIFIED_W))
                    .text_right()
                    .text_xs()
                    .child(node.max_modified.map(modified).unwrap_or_default()),
            )
            .into_any_element()
    }
}
