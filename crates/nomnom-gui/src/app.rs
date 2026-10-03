//! The window: a stack of screens with Drives at the root. Picking a drive
//! pushes its tree; the tree's bottom bar opens the files to delete and
//! reclaims them; Packs, a setting rather than a place, opens from the header.
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

use crate::clean::{Applied, CleanScreen};
use crate::drives::DrivesScreen;
use crate::packs::PacksScreen;
use crate::scan::ScanScreen;
use crate::session::Session;
use crate::state::size;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Drives,
    Tree,
    /// The files to delete, pushed over the tree.
    Plan,
    Packs,
}

/// Where each feature lives. Drives is the root; the tree is pushed by
/// picking a drive; Suggest is the bottom bar's list button and Clean its
/// Reclaim button, which applies from the tree itself; Packs is the header
/// button.
fn screen(feature: Feature) -> Screen {
    match feature {
        Feature::Drives => Screen::Drives,
        Feature::Tree => Screen::Tree,
        Feature::Suggest => Screen::Plan,
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
    clean: Entity<CleanScreen>,
    packs: Entity<PacksScreen>,
}

impl NomnomApp {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let drives = cx.new(|cx| DrivesScreen::new(session.clone(), cx));
        cx.subscribe(&drives, |this, _, Navigate(feature), cx| this.open(*feature, cx)).detach();
        let clean = cx.new(|cx| CleanScreen::new(session.clone(), cx));
        cx.observe(&clean, |_, _, cx| cx.notify()).detach();
        // The outcome of a Reclaim shows on the list it applied.
        cx.subscribe(&clean, |this, _, _: &Applied, cx| this.open(Feature::Suggest, cx)).detach();
        Self {
            drives,
            tree: cx.new(|cx| ScanScreen::new(session.clone(), cx)),
            clean,
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

    fn view(&self, screen: Screen) -> AnyView {
        match screen {
            Screen::Drives => self.drives.clone().into(),
            Screen::Tree => self.tree.clone().into(),
            Screen::Plan => self.clean.clone().into(),
            Screen::Packs => self.packs.clone().into(),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let busy = session.busy;
        let duplicates = session
            .duplicates
            .as_ref()
            .filter(|_| busy.is_none())
            .map(|progress| progress.status());
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
            .when_some(duplicates, |row, status| {
                row.child(Spinner::new().small())
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(status))
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

    /// The tree's bottom bar: the files to delete on the left, Reclaim on the
    /// right. Shown once a scan has landed.
    fn render_bottom_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.session.read(cx);
        if self.top() != Screen::Tree || session.scan.is_none() {
            return None;
        }
        let busy = session.busy.is_some();
        let assessing = session.assessing;
        let clean = self.clean.read(cx);
        let (checked, bytes) = clean.plan_summary();
        let can_apply = clean.can_apply();

        let list = Button::new("files-to-delete")
            .outline()
            .label(format!("Files to delete ({checked})"))
            .on_click(cx.listener(|this, _, _, cx| this.open(Feature::Suggest, cx)));

        let reclaim_label =
            if assessing { "Analyzing…".to_string() } else { format!("Reclaim {}", size(bytes)) };
        let reclaim = Button::new("reclaim")
            .danger()
            .label(reclaim_label)
            .disabled(assessing || busy || !can_apply)
            .on_click(cx.listener(|this, _, window, cx| {
                this.clean.update(cx, |clean, cx| clean.confirm_apply(window, cx))
            }));

        Some(
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(list)
                .child(div().flex_1())
                .child(reclaim),
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
            .child(div().flex_1().min_h_0().child(self.view(self.top())))
            .children(self.render_bottom_bar(cx))
    }
}
