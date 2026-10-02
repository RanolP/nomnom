//! One colour per file extension, so the treemap's tiles group by type.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;

use gpui_kit::{Hsla, hsla};
use nomnom_core::catalog::Catalog;
use nomnom_core::scan::EntryKind;

/// Distinct hues for the biggest extensions, biggest first. Everything past
/// them shares [`Palette::OTHER`], since a drive has thousands of extensions
/// and more hues would stop being tellable apart.
const HUES: [f32; 16] = [
    0.60, 0.08, 0.33, 0.95, 0.15, 0.75, 0.50, 0.02, 0.42, 0.85, 0.24, 0.68, 0.55, 0.12, 0.90, 0.38,
];

const NO_EXTENSION: &str = "(none)";

pub struct Palette {
    by_ext: HashMap<String, Hsla>,
}

impl Palette {
    pub const OTHER: Hsla = hsla(0., 0., 0.55, 1.);

    /// Ranks extensions by the bytes their files hold, so the hues go to what
    /// covers the most treemap area.
    pub fn new(catalog: &Catalog) -> Self {
        let mut bytes: HashMap<String, u64> = HashMap::new();
        for node in catalog.nodes().filter(|node| node.kind == EntryKind::File) {
            *bytes.entry(ext_key(&node.name)).or_default() += node.size;
        }
        let mut ranked: Vec<(String, u64)> = bytes.into_iter().collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let by_ext = ranked
            .into_iter()
            .zip(HUES)
            .enumerate()
            .map(|(rank, ((ext, _), hue))| {
                // The second eight repeat nearby hues at another lightness.
                let lightness = if rank < 8 { 0.55 } else { 0.42 };
                (ext, hsla(hue, 0.65, lightness, 1.))
            })
            .collect();
        Self { by_ext }
    }

    pub fn for_name(&self, name: &OsStr) -> Hsla {
        self.by_ext.get(&ext_key(name)).copied().unwrap_or(Self::OTHER)
    }
}

fn ext_key(name: &OsStr) -> String {
    match Path::new(name).extension() {
        Some(ext) => ext.to_string_lossy().to_lowercase(),
        None => NO_EXTENSION.to_string(),
    }
}
