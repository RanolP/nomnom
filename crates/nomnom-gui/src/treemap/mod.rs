//! The treemap under the tree: every file a rect sized by its bytes and
//! coloured by its extension, every directory a frame around its contents.

pub mod layout;

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::*;
use nomnom_core::catalog::{Catalog, NodeId};

use crate::session::ScanData;
use crate::treemap::layout::{Limits, Rect, Tile, layout, tile_at};

const DIR_FILL: Hsla = hsla(0., 0., 0.22, 1.);
const FILE_EDGE: Hsla = hsla(0., 0., 0., 0.35);
const HOVER: Hsla = hsla(0., 0., 1., 0.9);
const SELECTED: Hsla = hsla(0.14, 1., 0.55, 1.);

/// The last layout, kept until the scan, the root or the size changes:
/// a drive-sized layout is too costly to redo every frame a hover repaints.
#[derive(Default)]
pub struct Treemap {
    key: Option<(usize, NodeId, (f32, f32))>,
    tiles: Vec<Tile>,
    fills: Vec<Hsla>,
    index: HashMap<NodeId, usize>,
    origin: Point<Pixels>,
}

impl Treemap {
    pub fn prepare(&mut self, data: &ScanData, root: NodeId, bounds: Bounds<Pixels>) {
        self.origin = bounds.origin;
        let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let key = (Arc::as_ptr(&data.catalog) as usize, root, size);
        if self.key == Some(key) {
            return;
        }
        let catalog = &data.catalog;
        let area = Rect { x: 0., y: 0., w: size.0, h: size.1 };
        self.tiles = layout(catalog, root, area, &Limits::default());
        self.fills = self
            .tiles
            .iter()
            .map(
                |tile| {
                    if tile.dir { DIR_FILL } else { data.palette.for_name(catalog.name(tile.id)) }
                },
            )
            .collect();
        self.index = self.tiles.iter().enumerate().map(|(ix, tile)| (tile.id, ix)).collect();
        self.key = Some(key);
    }

    /// The deepest node under a window position.
    pub fn hit(&self, position: Point<Pixels>) -> Option<NodeId> {
        let x = f32::from(position.x - self.origin.x);
        let y = f32::from(position.y - self.origin.y);
        tile_at(&self.tiles, x, y).map(|ix| self.tiles[ix].id)
    }

    /// The tile drawn for `id`, or for its nearest ancestor when `id` itself
    /// was too small to get one.
    fn tile_of(&self, catalog: &Catalog, id: NodeId) -> Option<&Tile> {
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            if let Some(&ix) = self.index.get(&current) {
                return Some(&self.tiles[ix]);
            }
            cursor = catalog.node(current).parent;
        }
        None
    }

    pub fn paint(
        &self,
        catalog: &Catalog,
        selected: Option<NodeId>,
        hovered: Option<NodeId>,
        window: &mut Window,
    ) {
        for (tile, &color) in self.tiles.iter().zip(&self.fills) {
            let bounds = self.bounds(&tile.rect);
            if !tile.dir && tile.rect.w >= 4. && tile.rect.h >= 4. {
                window.paint_quad(quad(
                    bounds,
                    px(0.),
                    color,
                    px(1.),
                    FILE_EDGE,
                    BorderStyle::Solid,
                ));
            } else {
                window.paint_quad(fill(bounds, color));
            }
        }
        if let Some(tile) = hovered.and_then(|id| self.tile_of(catalog, id)) {
            window.paint_quad(outline(self.bounds(&tile.rect), HOVER, BorderStyle::Solid));
        }
        if let Some(tile) = selected.and_then(|id| self.tile_of(catalog, id)) {
            let bounds = self.bounds(&tile.rect);
            window.paint_quad(quad(
                bounds,
                px(0.),
                transparent_black(),
                px(2.),
                SELECTED,
                BorderStyle::Solid,
            ));
        }
    }

    fn bounds(&self, rect: &Rect) -> Bounds<Pixels> {
        Bounds::new(
            point(self.origin.x + px(rect.x), self.origin.y + px(rect.y)),
            size(px(rect.w), px(rect.h)),
        )
    }
}
