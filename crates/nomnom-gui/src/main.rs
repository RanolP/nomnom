//! `nomnom-gui` — the desktop front-end over the same pipeline as `nomnom`.
//!
//! An optional first argument (or `NOMNOM_GUI_ROOT`) names a folder to open
//! and scan at launch.

// Release builds are launched from Explorer, where a console window is noise;
// debug builds keep it for the `nomnom-gui:` error log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clean;
mod packs;
mod scan;
mod session;
mod state;
mod suggest;
mod undo;

use std::path::PathBuf;

use gpui_kit::*;

use crate::app::NomnomApp;
use crate::session::Session;

fn main() {
    let root = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("NOMNOM_GUI_ROOT"))
        .map(PathBuf::from);

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(800.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions { title: Some("nomnom".into()), ..Default::default() }),
            ..Default::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let session = cx.new(|_| Session::new(root.clone()));
            if root.is_some() {
                session.update(cx, |session, cx| session.scan(cx));
            }
            cx.new(|cx| NomnomApp::new(session, window, cx))
        });
        if let Err(error) = opened {
            eprintln!("nomnom-gui: cannot open the main window: {error:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
