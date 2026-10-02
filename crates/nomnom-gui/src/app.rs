//! The window: a sidebar of screens, and a header that owns the scan root.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::plain;
use nomnom_core::scan::Backend;

use crate::clean::CleanScreen;
use crate::packs::PacksScreen;
use crate::scan::ScanScreen;
use crate::session::Session;
use crate::suggest::SuggestScreen;
use crate::undo::UndoScreen;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Scan,
    Suggest,
    Clean,
    Packs,
    Undo,
}

impl Screen {
    const ALL: [Screen; 5] =
        [Screen::Scan, Screen::Suggest, Screen::Clean, Screen::Packs, Screen::Undo];

    fn name(self) -> &'static str {
        match self {
            Screen::Scan => "Scan",
            Screen::Suggest => "Suggest",
            Screen::Clean => "Clean",
            Screen::Packs => "Packs",
            Screen::Undo => "Undo",
        }
    }
}

pub struct NomnomApp {
    session: Entity<Session>,
    screen: Screen,
    scan: Entity<ScanScreen>,
    suggest: Entity<SuggestScreen>,
    clean: Entity<CleanScreen>,
    packs: Entity<PacksScreen>,
    undo: Entity<UndoScreen>,
    prompt_error: Option<String>,
}

impl NomnomApp {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            scan: cx.new(|cx| ScanScreen::new(session.clone(), cx)),
            suggest: cx.new(|cx| SuggestScreen::new(session.clone(), cx)),
            clean: cx.new(|cx| CleanScreen::new(session.clone(), cx)),
            packs: cx.new(|cx| PacksScreen::new(session.clone(), window, cx)),
            undo: cx.new(|cx| UndoScreen::new(session.clone(), cx)),
            session,
            screen: Screen::Scan,
            prompt_error: None,
        }
    }

    fn choose_root(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Scan this folder".into()),
        });
        let session = self.session.clone();
        cx.spawn(async move |this, cx| {
            let chosen = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    let message = format!("cannot open the folder picker: {error}");
                    eprintln!("nomnom-gui: {message}");
                    let _ = this.update(cx, |this, cx| {
                        this.prompt_error = Some(message);
                        cx.notify();
                    });
                    None
                }
            };
            if let Some(root) = chosen {
                let _ = this.update(cx, |this, cx| {
                    this.prompt_error = None;
                    this.screen = Screen::Scan;
                    cx.notify();
                });
                session.update(cx, |session, cx| session.set_root(root, cx));
            }
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let busy = session.busy;
        let backend = session.backend;
        let root = session.root.as_deref().map_or_else(|| "No folder chosen".to_string(), plain);
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
                Button::new("choose-root")
                    .small()
                    .outline()
                    .label("Choose folder…")
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.choose_root(cx))),
            )
            .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(root))
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
        let sidebar = Sidebar::new("nav")
            .header(div().px_2().font_weight(FontWeight::BOLD).child("nomnom"))
            .child(SidebarMenu::new().children(Screen::ALL.map(|screen| {
                SidebarMenuItem::new(screen.name()).active(screen == current).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.screen = screen;
                        cx.notify();
                    },
                ))
            })));
        let body: AnyView = match current {
            Screen::Scan => self.scan.clone().into(),
            Screen::Suggest => self.suggest.clone().into(),
            Screen::Clean => self.clean.clone().into(),
            Screen::Packs => self.packs.clone().into(),
            Screen::Undo => self.undo.clone().into(),
        };

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
                    .when_some(self.prompt_error.clone(), |col, message| {
                        col.child(div().p_2().child(gpui_kit::component::alert::Alert::error(
                            "prompt-error",
                            message,
                        )))
                    })
                    .child(div().flex_1().min_h_0().child(body)),
            )
    }
}
