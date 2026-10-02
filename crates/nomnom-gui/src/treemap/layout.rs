//! Squarified treemap layout (Bruls, Huizing, van Wijk), free of gpui so it
//! can be tested on its own. Coordinates are logical pixels.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::scan::EntryKind;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn inset(&self, by: f32) -> Rect {
        Rect { x: self.x + by, y: self.y + by, w: self.w - 2. * by, h: self.h - 2. * by }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tile {
    pub id: NodeId,
    pub rect: Rect,
    pub dir: bool,
}

pub struct Limits {
    /// A directory whose rect is narrower than this on either side is drawn
    /// as one block instead of being opened.
    pub min_side: f32,
    /// Hard cap on tiles, so a multi-million-node drive still paints quickly.
    pub max_tiles: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { min_side: 3., max_tiles: 40_000 }
    }
}

/// Tiles in paint order: every directory comes before what is drawn inside
/// it, so the last tile containing a point is the deepest one there.
pub fn layout(catalog: &Catalog, root: NodeId, bounds: Rect, limits: &Limits) -> Vec<Tile> {
    let mut tiles = Vec::new();
    // Biggest open directory first, so the tile budget runs out on the small
    // corners rather than on whatever a depth-first walk reached first.
    let mut open = BinaryHeap::new();
    open.push(Open { area: bounds.w * bounds.h, id: root, rect: bounds });

    while let Some(Open { id, rect, .. }) = open.pop() {
        let budget = limits.max_tiles.saturating_sub(tiles.len());
        if budget == 0 {
            break;
        }
        let children: Vec<NodeId> = catalog
            .children_by_size(id)
            .into_iter()
            .take_while(|&child| catalog.node(child).subtree_size > 0)
            .collect();
        let total: u64 = children.iter().map(|&child| catalog.node(child).subtree_size).sum();
        if total == 0 {
            continue;
        }

        // Children too small to cover a pixel, and those past the budget, are
        // laid out as one undrawn remainder so the drawn ones keep their true
        // share of the area.
        let px_per_byte = f64::from(rect.w) * f64::from(rect.h) / total as f64;
        let drawn = children
            .iter()
            .take(budget)
            .take_while(|&&child| catalog.node(child).subtree_size as f64 * px_per_byte >= 1.)
            .count();
        let mut sizes: Vec<u64> =
            children[..drawn].iter().map(|&child| catalog.node(child).subtree_size).collect();
        sizes.push(total - sizes.iter().sum::<u64>());

        for (&child, child_rect) in children[..drawn].iter().zip(squarify(&sizes, rect)) {
            let node = catalog.node(child);
            let dir = node.kind == EntryKind::Dir;
            tiles.push(Tile { id: child, rect: child_rect, dir });
            if dir && child_rect.w.min(child_rect.h) >= limits.min_side + 2. {
                let inner = child_rect.inset(1.);
                open.push(Open { area: inner.w * inner.h, id: child, rect: inner });
            }
        }
    }
    tiles
}

/// The deepest tile under a point.
pub fn tile_at(tiles: &[Tile], x: f32, y: f32) -> Option<usize> {
    tiles.iter().rposition(|tile| tile.rect.contains(x, y))
}

struct Open {
    area: f32,
    id: NodeId,
    rect: Rect,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Open {}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        self.area.total_cmp(&other.area).then_with(|| other.id.cmp(&self.id))
    }
}

/// One rect per size, in input order, tiling `bounds` with areas proportional
/// to the sizes. Sizes should be sorted biggest first for squarish rects; a
/// zero size gets an empty rect.
pub fn squarify(sizes: &[u64], bounds: Rect) -> Vec<Rect> {
    let mut out = vec![Rect { x: bounds.x, y: bounds.y, w: 0., h: 0. }; sizes.len()];
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    if total <= 0. || bounds.w <= 0. || bounds.h <= 0. {
        return out;
    }
    let scale = f64::from(bounds.w) * f64::from(bounds.h) / total;
    let items: Vec<(usize, f64)> = sizes
        .iter()
        .enumerate()
        .filter(|(_, s)| **s > 0)
        .map(|(ix, &s)| (ix, s as f64 * scale))
        .collect();

    let (mut x, mut y) = (f64::from(bounds.x), f64::from(bounds.y));
    let (mut w, mut h) = (f64::from(bounds.w), f64::from(bounds.h));
    let mut start = 0;
    while start < items.len() {
        let short = w.min(h);
        let (mut end, mut sum, mut min, mut max) = (start, 0., f64::INFINITY, 0f64);
        let mut best = f64::INFINITY;
        while end < items.len() {
            let area = items[end].1;
            let (s, mn, mx) = (sum + area, min.min(area), max.max(area));
            let worst = (short * short * mx / (s * s)).max(s * s / (short * short * mn));
            if end > start && worst > best {
                break;
            }
            (best, sum, min, max, end) = (worst, s, mn, mx, end + 1);
        }

        let last_row = end == items.len();
        let vertical = w >= h;
        // The last row takes whatever is left, so rounding never leaves a gap.
        let thickness = match (last_row, vertical) {
            (true, true) => w,
            (true, false) => h,
            (false, _) => sum / short,
        };
        let mut along = 0.;
        for (k, &(ix, area)) in items[start..end].iter().enumerate() {
            let length = if k + 1 == end - start { short - along } else { area / thickness };
            out[ix] = if vertical {
                rect(x, y + along, thickness, length)
            } else {
                rect(x + along, y, length, thickness)
            };
            along += length;
        }
        if vertical {
            x += thickness;
            w -= thickness;
        } else {
            y += thickness;
            h -= thickness;
        }
        start = end;
    }
    out
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x: x as f32, y: y as f32, w: w.max(0.) as f32, h: h.max(0.) as f32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap(a: &Rect, b: &Rect) -> f32 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        w.max(0.) * h.max(0.)
    }

    // Catches entries hidden or overdrawn: a rect spilling out of its parent,
    // two siblings painted over each other, or an area that misstates size.
    #[test]
    fn squarified_rects_tile_the_bounds_proportionally() {
        let bounds = Rect { x: 12., y: 34., w: 800., h: 450. };
        let mut seed = 0x9e37_79b9_u64;
        for n in [1, 2, 3, 7, 50, 200] {
            let mut sizes: Vec<u64> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    1 + (seed >> 33) % 1000
                })
                .collect();
            sizes.sort_unstable_by(|a, b| b.cmp(a));
            let total: u64 = sizes.iter().sum();
            let rects = squarify(&sizes, bounds);

            for (size, r) in sizes.iter().zip(&rects) {
                let eps = 1e-3;
                assert!(r.x >= bounds.x - eps && r.y >= bounds.y - eps, "{r:?} starts outside");
                assert!(r.x + r.w <= bounds.x + bounds.w + eps, "{r:?} spills right");
                assert!(r.y + r.h <= bounds.y + bounds.h + eps, "{r:?} spills down");
                let expected = (bounds.w * bounds.h) as f64 * *size as f64 / total as f64;
                let actual = f64::from(r.w * r.h);
                assert!(
                    (actual - expected).abs() <= expected * 0.01,
                    "n={n}: area {actual} for size {size}, expected {expected}"
                );
            }
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    // f32 edges round by ~1e-4 px; a real overlap is a pixel or more.
                    assert!(overlap(a, b) < 0.1, "n={n}: {a:?} overlaps {b:?}");
                }
            }
        }
    }
}
