//! Likely copies, found after the rules and merged in when ready.
//!
//! The second phase of an assessment: [`assess`](super::assess) settles the
//! rules first and is shown at once, then [`find_duplicates`] samples every
//! same-size file and [`Assessment::with_duplicates`] merges what it found.
//! Rule verdicts never depend on duplicates, so the split changes nothing the
//! rules decide.
//!
//! A group is decided once, for the whole group: the oldest copy is the
//! original and is kept, every other copy is reclaimable. Deciding copy by
//! copy is how a group loses all of its members. The grouping is by sample,
//! so each copy carries the original it was matched to, and the plan
//! verifies the two byte for byte before anything can be applied.

use std::collections::HashSet;
use std::time::SystemTime;

use super::{Assessment, Disposition, Entry, Label, Provenance, Reach, Verdict, under_unit};
use crate::catalog::{Catalog, DuplicateProgress, NodeId};

/// Files below this are not sampled and not reported.
///
/// The payoff below it is noise: a pair of 1 MiB copies reclaims one
/// megabyte, which is not worth a line of a human's attention or the risk of
/// them approving it by reflex. 1 MiB is the point where a group of a handful
/// of copies starts to be worth naming. It also keeps the pass off the
/// millions of tiny files a volume holds.
pub const MIN_DUPLICATE_SIZE: u64 = 1024 * 1024;

/// Duplicate detection is not expressible in the rule language, so there is no
/// `.nom` file to point at. It still has provenance — a human asking "why does
/// nomnom want to delete this" gets an answer down to the analysis that
/// produced it, which is the whole point of the field.
const PACK: &str = "built-in";
const RULE_ORIGINAL: &str = "duplicate-original";
const RULE_COPY: &str = "duplicate-copy";

/// The rule every reclaimable copy cites, which a user approves to plan them.
pub fn duplicate_copy_rule() -> Provenance {
    Provenance::new(PACK, RULE_COPY)
}

/// What [`find_duplicates`] found: one entry per group member, the copies
/// naming the original they were matched to.
pub struct Duplicates {
    pub entries: Vec<Entry>,
}

/// The second phase: likely copies outside every rule target of
/// `assessment`. `None` when `progress` was cancelled.
///
/// A rule target and anything inside one take no part. A copy inside a
/// condemned `node_modules` disappears with its directory, so treating it as
/// the original would propose deleting the copy that actually survives; and a
/// file a rule already decided stays that rule's, so approving a rule means
/// the same before and after the merge.
pub fn find_duplicates(
    catalog: &Catalog,
    assessment: &Assessment,
    progress: &DuplicateProgress,
) -> Option<Duplicates> {
    let units: HashSet<NodeId> = assessment
        .groups
        .iter()
        .flat_map(|group| &group.entries)
        .filter_map(|entry| Some(NodeId(entry.reach?.start)))
        .collect();
    let groups = catalog.likely_duplicate_groups(MIN_DUPLICATE_SIZE, progress)?;
    let mut entries = Vec::new();
    for group in groups {
        let mut members: Vec<NodeId> = group
            .into_iter()
            .filter(|id| !units.contains(id) && !under_unit(catalog, *id, &units))
            .collect();
        if members.len() < 2 {
            continue;
        }
        // Unknown mtime is not evidence of being the original, so those
        // sort last. The path breaks ties, and it has to be the path rather
        // than the `NodeId`: a `NodeId` is scan order, which the MFT backend
        // and the walk backend produce differently and neither repeats run to
        // run. Four copies written in one `cp -r` share a byte-identical
        // mtime, and on the id the kept copy flips between runs of the same
        // command over the same tree.
        members.sort_by_cached_key(|&id| {
            let node = catalog.node(id);
            (
                node.modified.is_none(),
                node.modified.unwrap_or(SystemTime::UNIX_EPOCH),
                catalog.path(id),
            )
        });
        let original = members[0];
        let size = catalog.node(original).size;
        let count = members.len();
        let kept = catalog.path(original).display().to_string();
        entries.push(entry(
            catalog,
            original,
            Verdict {
                label: Label::DUPLICATE,
                disposition: Disposition::Keep,
                confidence: 0.9,
                reason: format!(
                    "oldest of {count} likely copies ({size} bytes each, same sampled head, \
                     middle and tail); kept as the original"
                ),
                provenance: Provenance::new(PACK, RULE_ORIGINAL),
                capped: None,
            },
            None,
        ));
        for &copy in &members[1..] {
            entries.push(entry(
                catalog,
                copy,
                Verdict {
                    label: Label::DUPLICATE,
                    disposition: Disposition::Reclaimable,
                    confidence: 0.85,
                    reason: format!(
                        "likely copy: {count} files of {size} bytes with the same sampled head, \
                         middle and tail; the oldest is kept at {kept}. Checked byte for byte \
                         before anything is trashed"
                    ),
                    provenance: duplicate_copy_rule(),
                    capped: None,
                },
                Some(kept.clone()),
            ));
        }
    }
    Some(Duplicates { entries })
}

fn entry(catalog: &Catalog, id: NodeId, verdict: Verdict, copy_of: Option<String>) -> Entry {
    let node = catalog.node(id);
    let range = catalog.subtree(id);
    Entry {
        path: catalog.path(id).display().to_string(),
        bytes: node.subtree_size,
        verdict,
        reach: Some(Reach { start: range.start, end: range.end, rolled: node.rolled_size() }),
        copy_of,
    }
}
