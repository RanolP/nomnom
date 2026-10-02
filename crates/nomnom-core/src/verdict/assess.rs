//! One judged pass over a catalog, grouped the way a human reads it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::{DslJudge, Judge, Label, TrustedPack, Verdict, assess_all, rollup};
use crate::catalog::Catalog;

pub struct Assessment {
    /// The catalog root the entries' paths lie under, which is also the fence
    /// a cleanup plan built from this assessment is anchored at.
    pub root: PathBuf,
    /// Grouped by label, groups biggest first.
    pub groups: Vec<Group>,
    pub reclaimable_bytes: u64,
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
    let reclaimable_bytes = rollup(catalog, &verdicts).reclaimable_bytes;

    let mut by_label: BTreeMap<Label, Vec<Entry>> = BTreeMap::new();
    for (id, verdict) in verdicts {
        by_label.entry(verdict.label.clone()).or_default().push(Entry {
            path: catalog.path(id).display().to_string(),
            bytes: catalog.node(id).subtree_size,
            verdict,
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
    Assessment { root: catalog.path(catalog.root()), groups, reclaimable_bytes }
}
