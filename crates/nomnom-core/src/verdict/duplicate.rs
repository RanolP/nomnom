//! Byte-identical copies.
//!
//! A group is decided once, for the whole group: the oldest copy is the
//! original and is kept, every other copy is reclaimable. Deciding copy by
//! copy is how a group loses all of its members.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use super::{Disposition, Label, Provenance, Verdict, under_unit};
use crate::catalog::{Catalog, NodeId};

/// Files below this are not hashed and not reported.
///
/// Two reasons, both hard. Hashing is a full read, so a floor is what keeps a
/// duplicate pass off the millions of tiny files a volume holds. And the
/// payoff below it is noise: a pair of 1 MiB copies reclaims one megabyte,
/// which is not worth a line of a human's attention or the risk of them
/// approving it by reflex. 1 MiB is the point where a group of a handful of
/// copies starts to be worth naming.
pub const MIN_DUPLICATE_SIZE: u64 = 1024 * 1024;

/// Every duplicate decision in the catalog, resolved up front.
pub(super) struct DuplicateFacts {
    by_node: HashMap<NodeId, Verdict>,
}

/// The pack and rule name a duplicate verdict cites.
///
/// Duplicate detection is not expressible in the rule language, so there is no
/// `.nom` file to point at. It still has provenance — a human asking "why does
/// nomnom want to delete this" gets an answer down to the analysis that
/// produced it, which is the whole point of the field.
const PACK: &str = "built-in";
const RULE_ORIGINAL: &str = "duplicate-original";
const RULE_COPY: &str = "duplicate-copy";

impl DuplicateFacts {
    /// No duplicates known. Used while the unit set is still being resolved,
    /// before a duplicate pass is even possible.
    pub(super) fn empty() -> Self {
        Self { by_node: HashMap::new() }
    }

    pub(super) fn build(ctx: &Catalog, min_size: u64, units: &HashSet<NodeId>) -> Self {
        let mut by_node = HashMap::new();
        for group in ctx.duplicate_groups(min_size) {
            // A copy inside a condemned node_modules is not a candidate: it
            // disappears with its directory, so treating it as the original
            // would propose deleting the copy that actually survives.
            let mut members: Vec<NodeId> =
                group.into_iter().filter(|&id| !under_unit(ctx, id, units)).collect();
            if members.len() < 2 {
                continue;
            }
            // Unknown mtime is not evidence of being the original, so those
            // sort last. The path breaks ties, and it has to be the path
            // rather than the `NodeId`: a `NodeId` is scan order, which the MFT
            // backend and the walk backend produce differently and neither
            // repeats run to run. Four copies written in one `cp -r` share a
            // byte-identical mtime, and on the id the kept copy flips between
            // runs of the same command over the same tree.
            members.sort_by_cached_key(|&id| {
                let node = ctx.node(id);
                (
                    node.modified.is_none(),
                    node.modified.unwrap_or(SystemTime::UNIX_EPOCH),
                    ctx.path(id),
                )
            });
            let original = members[0];
            let size = ctx.node(original).size;
            let count = members.len();
            by_node.insert(
                original,
                Verdict {
                    label: Label::DUPLICATE,
                    disposition: Disposition::Keep,
                    confidence: 0.9,
                    reason: format!(
                        "oldest of {count} blake3-identical copies ({size} bytes each); kept as \
                         the original"
                    ),
                    unit: false,
                    provenance: Provenance::new(PACK, RULE_ORIGINAL),
                    capped: None,
                },
            );
            let kept = ctx.path(original).display().to_string();
            for &copy in &members[1..] {
                by_node.insert(
                    copy,
                    Verdict {
                        label: Label::DUPLICATE,
                        disposition: Disposition::Reclaimable,
                        confidence: 0.85,
                        reason: format!(
                            "{count} files, {size} bytes each, identical blake3 content; the \
                             oldest copy is kept at {kept}"
                        ),
                        unit: false,
                        provenance: Provenance::new(PACK, RULE_COPY),
                        capped: None,
                    },
                );
            }
        }
        Self { by_node }
    }

    pub(super) fn verdict(&self, id: NodeId) -> Option<Verdict> {
        self.by_node.get(&id).cloned()
    }

    /// Whether this node participates in a duplicate group, which is what the
    /// language's `is_duplicate` field reads.
    pub(super) fn contains(&self, id: NodeId) -> bool {
        self.by_node.contains_key(&id)
    }
}
