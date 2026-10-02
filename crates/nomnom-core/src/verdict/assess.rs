//! One judged pass over a catalog, grouped the way a human reads it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::{Disposition, DslJudge, Judge, Label, TrustedPack, Verdict, assess_all};
use crate::catalog::Catalog;

pub struct Assessment {
    /// The catalog root the entries' paths lie under, which is also the fence
    /// a cleanup plan built from this assessment is anchored at.
    pub root: PathBuf,
    /// Grouped by label, groups biggest first.
    pub groups: Vec<Group>,
    pub reclaimable_bytes: u64,
    /// Hard-linked files whose counted name lies under some entry: trashing
    /// that entry frees their bytes only if every other name goes too.
    pub shared: Vec<SharedFile>,
}

/// One hard-linked file, as node ids of the catalog the assessment came from.
#[derive(Debug, Clone)]
pub struct SharedFile {
    pub bytes: u64,
    /// The name its bytes are counted under.
    pub primary: u32,
    /// Every name in the catalog, `primary` included.
    pub names: Vec<u32>,
    /// False when some of its names are not in the catalog at all, so no plan
    /// built from it can remove every one.
    pub complete: bool,
}

#[derive(Serialize)]
pub struct Group {
    pub label: Label,
    pub bytes: u64,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
pub struct Entry {
    pub path: String,
    pub bytes: u64,
    pub verdict: Verdict,
    /// Where the entry sits in the catalog. `None` for an entry built by hand,
    /// which is charged its `bytes`.
    #[serde(skip)]
    pub reach: Option<Reach>,
}

/// An entry's subtree as the catalog's id range, and the bytes it adds to its
/// parent.
#[derive(Debug, Clone, Copy)]
pub struct Reach {
    pub start: u32,
    pub end: u32,
    pub rolled: u64,
}

/// What trashing every one of `entries` together frees, per entry in the same
/// order.
///
/// An entry inside another one frees nothing more. A hard-linked file frees
/// its bytes only when every name it has is trashed: removing one name of a
/// two-name file leaves the bytes on disk under the other, so the entry
/// holding its counted name is charged that much less.
pub fn charges(entries: &[&Entry], shared: &[SharedFile]) -> Vec<u64> {
    let mut out: Vec<u64> = entries.iter().map(|e| e.bytes).collect();
    let mut placed: Vec<(Reach, usize)> =
        entries.iter().enumerate().filter_map(|(i, e)| Some((e.reach?, i))).collect();
    placed.sort_by_key(|(reach, _)| (reach.start, std::cmp::Reverse(reach.end)));
    // Outermost spans only, disjoint and in id order.
    let mut outer: Vec<(Reach, usize)> = Vec::with_capacity(placed.len());
    for (reach, i) in placed {
        if outer.last().is_some_and(|(last, _)| reach.start < last.end) {
            out[i] = 0;
            continue;
        }
        out[i] = reach.rolled;
        outer.push((reach, i));
    }
    let holder = |id: u32| {
        let at = outer.partition_point(|(reach, _)| reach.start <= id).checked_sub(1)?;
        let (reach, i) = outer[at];
        (id < reach.end).then_some(i)
    };
    for file in shared {
        let Some(i) = holder(file.primary) else { continue };
        if !(file.complete && file.names.iter().all(|&name| holder(name).is_some())) {
            out[i] = out[i].saturating_sub(file.bytes);
        }
    }
    out
}

/// One pass of the judge over the catalog. Built once because `DslJudge`
/// resolves the unit set and hashes every size-colliding file to find
/// duplicates.
///
/// `packs` arrives already in resolution order, built-in first — see
/// [`super::resolve_packs`].
pub fn assess(catalog: &Catalog, packs: Vec<TrustedPack>) -> Assessment {
    let judge: &dyn Judge = &DslJudge::with_packs(catalog, packs);
    let verdicts = assess_all(judge, catalog);

    let mut spans: Vec<(u32, u32)> = verdicts
        .iter()
        .map(|(id, _)| {
            let range = catalog.subtree(*id);
            (range.start, range.end)
        })
        .collect();
    spans.sort_unstable();
    let mut outer: Vec<(u32, u32)> = Vec::with_capacity(spans.len());
    for span in spans {
        if outer.last().is_none_or(|last| span.0 >= last.1) {
            outer.push(span);
        }
    }
    let covered = |id: u32| {
        let at = outer.partition_point(|span| span.0 <= id);
        at > 0 && id < outer[at - 1].1
    };
    let shared: Vec<SharedFile> = catalog
        .link_groups()
        .filter(|group| covered(group.primary.0))
        .map(|group| SharedFile {
            bytes: group.bytes,
            primary: group.primary.0,
            names: group.nodes.iter().map(|id| id.0).collect(),
            complete: group.complete,
        })
        .collect();

    let mut by_label: BTreeMap<Label, Vec<Entry>> = BTreeMap::new();
    for (id, verdict) in verdicts {
        let node = catalog.node(id);
        let range = catalog.subtree(id);
        by_label.entry(verdict.label.clone()).or_default().push(Entry {
            path: catalog.path(id).display().to_string(),
            bytes: node.subtree_size,
            verdict,
            reach: Some(Reach { start: range.start, end: range.end, rolled: node.rolled_size() }),
        });
    }
    let mut groups: Vec<Group> = by_label
        .into_iter()
        .map(|(label, mut entries)| {
            entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
            Group { label, bytes: entries.iter().map(|e| e.bytes).sum(), entries }
        })
        .collect();
    groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));

    let reclaimable: Vec<&Entry> = groups
        .iter()
        .flat_map(|group| &group.entries)
        .filter(|entry| entry.verdict.disposition == Disposition::Reclaimable)
        .collect();
    let reclaimable_bytes = charges(&reclaimable, &shared).iter().sum();
    Assessment { root: catalog.path(catalog.root()), groups, reclaimable_bytes, shared }
}
