//! The window: a stack of screens with Drives at the root. Picking a drive
//! pushes its tree, with the suggestions awaiting review in a panel at its
//! right, where Reclaim also lives; Packs, a setting rather than a place,
//! opens from the header.
//!
//! `Feature` is the CLI/GUI parity contract. [`screen`] routes every feature
//! with no `_` arm, so a feature added to core without a home here breaks
//! this build.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::Feature;
use nomnom_core::action::plain;

use crate::clean::{PANEL_WIDTH, Reveal, SuggestPanel};
use crate::drives::DrivesScreen;
use crate::packs::PacksScreen;
use crate::scan::ScanScreen;
use crate::session::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Drives,
    Tree,
    Packs,
}

/// Where each feature lives. Drives is the root; the tree is pushed by
/// picking a drive; Suggest is the panel beside the tree and Clean the
/// Reclaim button at that panel's foot; Packs is the header button.
fn screen(feature: Feature) -> Screen {
    match feature {
        Feature::Drives => Screen::Drives,
        Feature::Tree => Screen::Tree,
        Feature::Suggest => Screen::Tree,
        Feature::Clean => Screen::Tree,
        Feature::Packs => Screen::Packs,
    }
}

/// Asks the window to show a feature.
pub struct Navigate(pub Feature);

pub struct NomnomApp {
    session: Entity<Session>,
    /// Never empty: `Screen::Drives` is always at the bottom.
    stack: Vec<Screen>,
    drives: Entity<DrivesScreen>,
    tree: Entity<ScanScreen>,
    suggest: Entity<SuggestPanel>,
    packs: Entity<PacksScreen>,
}

impl NomnomApp {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let drives = cx.new(|cx| DrivesScreen::new(session.clone(), cx));
        cx.subscribe(&drives, |this, _, Navigate(feature), cx| this.open(*feature, cx)).detach();
        let suggest = cx.new(|cx| SuggestPanel::new(session.clone(), cx));
        cx.subscribe(&suggest, |this, _, Reveal(path), cx| this.reveal(path, cx)).detach();
        Self {
            drives,
            tree: cx.new(|cx| ScanScreen::new(session.clone(), cx)),
            suggest,
            packs: cx.new(|cx| PacksScreen::new(session.clone(), window, cx)),
            session,
            stack: vec![Screen::Drives],
        }
    }

    fn top(&self) -> Screen {
        *self.stack.last().expect("the stack always holds Drives")
    }

    /// Show `feature`'s screen: back to it when it is already on the stack,
    /// pushed on top otherwise.
    fn open(&mut self, feature: Feature, cx: &mut Context<Self>) {
        let target = screen(feature);
        match self.stack.iter().position(|open| *open == target) {
            Some(ix) => self.stack.truncate(ix + 1),
            None => self.stack.push(target),
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            self.stack.pop();
            cx.notify();
        }
    }

    /// Select a suggested path in the tree. A path the scan does not hold
    /// (gone since the assessment) selects nothing.
    fn reveal(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let found = self.session.read(cx).scan.as_ref().and_then(|scan| scan.catalog.find(path));
        match found {
            Some(id) => self.tree.update(cx, |tree, cx| tree.reveal(id, cx)),
            None => eprintln!("nomnom-gui: {} is not in the scanned tree", plain(path)),
        }
    }

    /// The tree screen carries the suggestions panel at its right once a
    /// scan has landed.
    fn view(&self, screen: Screen, cx: &App) -> AnyElement {
        match screen {
            Screen::Drives => self.drives.clone().into_any_element(),
            Screen::Tree if self.session.read(cx).scan.is_some() => h_flex()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().child(self.tree.clone()))
                .child(div().w(px(PANEL_WIDTH)).flex_none().h_full().child(self.suggest.clone()))
                .into_any_element(),
            Screen::Tree => self.tree.clone().into_any_element(),
            Screen::Packs => self.packs.clone().into_any_element(),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let busy = session.busy;
        let root =
            session.root.as_deref().map_or_else(|| "No drive scanned yet".to_string(), plain);

        h_flex()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .when(self.stack.len() > 1, |row| {
                row.child(
                    Button::new("back")
                        .small()
                        .ghost()
                        .label("← Back")
                        .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                )
            })
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
                Button::new("packs")
                    .small()
                    .label("Packs")
                    .selected(self.top() == screen(Feature::Packs))
                    .on_click(cx.listener(|this, _, _, cx| this.open(Feature::Packs, cx))),
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
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.view(self.top(), cx)))
    }
}
