//! `nomnom-gui` — the desktop front-end over the same pipeline as `nomnom`.

// Release builds are launched from Explorer, where a console window is noise;
// debug builds keep it for the `nomnom-gui:` error log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clean;
mod drives;
mod packs;
mod palette;
mod scan;
mod session;
mod state;
mod treemap;

use std::process::ExitCode;

use gpui_kit::*;

use crate::app::NomnomApp;
use crate::session::Session;

/// The stock backdrop (5% black light, 20% dark) barely separates a modal
/// from the tree behind it. Both theme configs carry the override, so it
/// survives a system light/dark switch reloading the mode's config.
fn darken_dialog_backdrop(cx: &mut App) {
    use gpui_kit::component::Theme;
    let theme = Theme::global_mut(cx);
    for config in [&mut theme.light_theme, &mut theme.dark_theme] {
        std::rc::Rc::make_mut(config).colors.overlay = Some("#0000008c".into());
    }
    let mode = theme.mode;
    Theme::change(mode, None, cx);
}

fn main() -> ExitCode {
    // The elevated MFT helper is this same binary relaunched behind UAC.
    if let Some(code) = nomnom_core::scan::maybe_run_helper() {
        return code;
    }

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        darken_dialog_backdrop(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1400.), px(900.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions { title: Some("nomnom".into()), ..Default::default() }),
            ..Default::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let session = cx.new(|_| Session::new());
            cx.new(|cx| NomnomApp::new(session, window, cx))
        });
        if let Err(error) = opened {
            eprintln!("nomnom-gui: cannot open the main window: {error:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    ExitCode::SUCCESS
}
