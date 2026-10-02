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
mod suggest;
mod treemap;

use std::process::ExitCode;

use gpui_kit::*;

use crate::app::NomnomApp;
use crate::session::Session;

fn main() -> ExitCode {
    // The elevated MFT helper is this same binary relaunched behind UAC.
    if let Some(code) = nomnom_core::scan::maybe_run_helper() {
        return code;
    }

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
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
