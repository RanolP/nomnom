//! A pack's icon as a row shows it, on the Packs screen and beside each rule
//! in the suggestion panel.
//!
//! Logos keep their own colours, so they are drawn as images rather than as
//! theme-tinted glyphs, on a light tile that a black mark (Rust, Next.js)
//! stays readable on in the dark theme too. A pack with no usable icon gets
//! the same tile with a neutral glyph, so rows stay aligned.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme as _, Icon};
use gpui_kit::*;
use nomnom_core::verdict::{PackIcon, TrustedPack};

/// Lucide `package` (ISC), https://cdn.jsdelivr.net/npm/lucide-static@1.51.0/icons/package.svg
const FALLBACK: &[u8] = include_bytes!("../assets/pack-fallback.svg");

const TILE: f32 = 20.;

/// The icon to draw, or `None` for the fallback glyph. A broken icon was
/// already turned into a warning where the pack was listed.
pub fn image(icon: Option<&PackIcon>) -> Option<Arc<Image>> {
    let svg = icon?.svg.as_ref().ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Svg, svg.to_vec())))
}

/// Pack name to icon, for every pack a judging run loaded. Packs later in
/// resolution order override earlier ones of the same name, as their rules do.
pub fn images(packs: &[TrustedPack]) -> HashMap<String, Arc<Image>> {
    packs
        .iter()
        .filter_map(|pack| Some((pack.pack.name.clone(), image(pack.pack.icon.as_ref())?)))
        .collect()
}

pub fn tile(image: Option<Arc<Image>>, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let tile = div()
        .flex_none()
        .size(px(TILE))
        .p(px(2.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().border);
    match image {
        Some(image) => tile.bg(white()).child(
            img(image)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(move || fallback(muted).into_any_element()),
        ),
        None => tile.child(fallback(muted)),
    }
}

fn fallback(color: Hsla) -> Icon {
    Icon::default().data(FALLBACK).text_color(color).size(px(TILE - 4.))
}
