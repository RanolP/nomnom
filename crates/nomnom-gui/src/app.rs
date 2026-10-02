//! The window: a sidebar with one screen per [`Feature`] (the drive views
//! first, cleanup after), and a header that owns the open drive.
//!
//! `Feature` is the CLI/GUI parity contract. Every match on it here is
//! exhaustive with no `_` arm, so a feature added to core without a screen
//! breaks this build.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::Feature;
use nomnom_core::action::plain;
use nomnom_core::scan::Backend;

use crate::clean::CleanScreen;
use crate::drives::DrivesScreen;
use crate::file_types::FileTypesScreen;
use crate::largest::{LargestScreen, Reveal};
use crate::packs::PacksScreen;
use crate::scan::ScanScreen;
use crate::session::Session;
use crate::suggest::SuggestScreen;
use crate::undo::UndoScreen;

const DRIVE_VIEWS: [Feature; 4] =
    [Feature::Drives, Feature::Tree, Feature::FileTypes, Feature::LargestFiles];
const CLEANUP: [Feature; 4] = [Feature::Suggest, Feature::Clean, Feature::Packs, Feature::Undo];

fn name(feature: Feature) -> &'static str {
    match feature {
        Feature::Drives => "Drives",
        Feature::Tree => "Tree",
        Feature::FileTypes => "File types",
        Feature::LargestFiles => "Largest files",
        Feature::Suggest => "Suggest",
        Feature::Clean => "Clean",
        Feature::Packs => "Packs",
        Feature::Undo => "Undo",
    }
}

/// Asks the window to switch screens.
pub struct Navigate(pub Feature);

pub struct NomnomApp {
    session: Entity<Session>,
    screen: Feature,
    drives: Entity<DrivesScreen>,
    tree: Entity<ScanScreen>,
    file_types: Entity<FileTypesScreen>,
    largest: Entity<LargestScreen>,
    suggest: Entity<SuggestScreen>,
    clean: Entity<CleanScreen>,
    packs: Entity<PacksScreen>,
    undo: Entity<UndoScreen>,
}

impl NomnomApp {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let drives = cx.new(|cx| DrivesScreen::new(session.clone(), cx));
        cx.subscribe(&drives, |this, _, Navigate(screen), cx| {
            this.screen = *screen;
            cx.notify();
        })
        .detach();
        let tree = cx.new(|cx| ScanScreen::new(session.clone(), cx));
        let largest = cx.new(|cx| LargestScreen::new(session.clone(), cx));
        cx.subscribe(&largest, |this, _, Reveal(id), cx| {
            let id = *id;
            this.tree.update(cx, |tree, cx| tree.reveal(id, cx));
            this.screen = Feature::Tree;
            cx.notify();
        })
        .detach();
        Self {
            drives,
            tree,
            file_types: cx.new(|cx| FileTypesScreen::new(session.clone(), cx)),
            largest,
            suggest: cx.new(|cx| SuggestScreen::new(session.clone(), cx)),
            clean: cx.new(|cx| CleanScreen::new(session.clone(), cx)),
            packs: cx.new(|cx| PacksScreen::new(session.clone(), window, cx)),
            undo: cx.new(|cx| UndoScreen::new(session.clone(), cx)),
            session,
            screen: Feature::Drives,
        }
    }

    fn view(&self, feature: Feature) -> AnyView {
        match feature {
            Feature::Drives => self.drives.clone().into(),
            Feature::Tree => self.tree.clone().into(),
            Feature::FileTypes => self.file_types.clone().into(),
            Feature::LargestFiles => self.largest.clone().into(),
            Feature::Suggest => self.suggest.clone().into(),
            Feature::Clean => self.clean.clone().into(),
            Feature::Packs => self.packs.clone().into(),
            Feature::Undo => self.undo.clone().into(),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let busy = session.busy;
        let backend = session.backend;
        let root =
            session.root.as_deref().map_or_else(|| "No drive scanned yet".to_string(), plain);
        let backend_button = |id: &'static str, label: &'static str, choice: Backend| {
            Button::new(id)
                .small()
                .label(label)
                .selected(backend == choice)
                .disabled(busy.is_some())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.session.update(cx, |session, cx| {
                        session.backend = choice;
                        cx.notify();
                    });
                }))
        };

        h_flex()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(root),
            )
            .when_some(busy, |row, phase| {
                row.child(Spinner::new().small()).child(
                    div().text_sm().text_color(cx.theme().muted_foreground).child(phase.label()),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Backend"))
                    .child(backend_button("backend-auto", "Auto", Backend::Auto))
                    .child(backend_button("backend-mft", "MFT", Backend::Mft))
                    .child(backend_button("backend-walk", "Walk", Backend::Walk)),
            )
            .child(
                Button::new("rescan")
                    .small()
                    .primary()
                    .label("Rescan")
                    .disabled(busy.is_some() || session.root.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.session.update(cx, |session, cx| session.scan(cx))
                    })),
            )
    }
}

impl Render for NomnomApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.screen;
        let menu = |features: [Feature; 4], cx: &mut Context<Self>| {
            SidebarMenu::new().children(features.map(|feature| {
                SidebarMenuItem::new(name(feature)).active(feature == current).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.screen = feature;
                        cx.notify();
                    }),
                )
            }))
        };
        let sidebar = Sidebar::new("nav")
            .header(div().px_2().font_weight(FontWeight::BOLD).child("nomnom"))
            .child(SidebarGroup::new("Drive").child(menu(DRIVE_VIEWS, cx)))
            .child(SidebarGroup::new("Cleanup").child(menu(CLEANUP, cx)));

        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar)
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(self.render_header(cx))
                    .child(div().flex_1().min_h_0().child(self.view(current))),
            )
    }
}
