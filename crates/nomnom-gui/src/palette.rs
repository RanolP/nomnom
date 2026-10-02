//! One colour per file extension, shared by the treemap and the File types
//! table so a swatch there names the colour seen here.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;

use gpui_kit::{Hsla, hsla};
use nomnom_core::catalog::FileType;

/// Distinct hues for the biggest extensions, biggest first. Everything past
/// them shares [`Palette::OTHER`], since a drive has thousands of extensions
/// and more hues would stop being tellable apart.
const HUES: [f32; 16] = [
    0.60, 0.08, 0.33, 0.95, 0.15, 0.75, 0.50, 0.02, 0.42, 0.85, 0.24, 0.68, 0.55, 0.12, 0.90, 0.38,
];

pub struct Palette {
    by_ext: HashMap<String, Hsla>,
}

impl Palette {
    pub const OTHER: Hsla = hsla(0., 0., 0.55, 1.);

    pub fn new(types: &[FileType]) -> Self {
        let by_ext = types
            .iter()
            .zip(HUES)
            .enumerate()
            .map(|(rank, (ty, hue))| {
                // The second eight repeat nearby hues at another lightness.
                let lightness = if rank < 8 { 0.55 } else { 0.42 };
                (ty.ext.clone(), hsla(hue, 0.65, lightness, 1.))
            })
            .collect();
        Self { by_ext }
    }

    pub fn color(&self, ext: &str) -> Hsla {
        self.by_ext.get(ext).copied().unwrap_or(Self::OTHER)
    }

    /// Keyed the way `catalog::file_types` keys a file name.
    pub fn for_name(&self, name: &OsStr) -> Hsla {
        match Path::new(name).extension() {
            Some(ext) => self.color(&ext.to_string_lossy().to_lowercase()),
            None => self.color(FileType::NO_EXTENSION),
        }
    }
}
