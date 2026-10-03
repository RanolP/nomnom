//! Who owns a node, and how the disk splits into recognized and arbitrary
//! bytes (`docs/lang.md`, "Ownership").
//!
//! Every short-form rule target is an exclusive claim with class
//! `<pack>:<kind>`. A node's owner is the innermost claim containing it. A
//! claim inside another pack's exclusive claim is dropped; one inside the same
//! pack's claim is kept, so a pack may describe its own structure in layers.
//! A node no claim contains is arbitrary, and nothing there is ever suggested.

use serde::{Deserialize, Serialize};

use super::Provenance;
use crate::catalog::{Catalog, NodeId};

/// "This subtree is X, and pack P owns it."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// `<pack>:<kind>`, e.g. `builtin.cargo:build-output/v1`.
    pub class: String,
    pub provenance: Provenance,
    pub confidence: f32,
    /// Every short-form claim is exclusive; open claims arrive with the long
    /// form.
    pub exclusive: bool,
}

/// Why a claim some rule made does not own anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum DropReason {
    /// Another claim on the same node won the conflict.
    Outranked { by: Provenance },
    /// It lies inside another pack's exclusive claim.
    Inside { owner: Provenance },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroppedClaim {
    pub path: String,
    pub claim: Claim,
    #[serde(flatten)]
    pub reason: DropReason,
}

/// Every surviving claim of one assessment, and the claimed bytes of every
/// node.
///
/// `Default` is the ownership of an assessment built by hand: no claims, and
/// every node arbitrary.
#[derive(Debug, Clone, Default)]
pub struct Ownership {
    /// In id order, so claims are disjoint or nested and an owner is found by
    /// binary search.
    claims: Vec<(NodeId, Claim)>,
    /// Per claim: the end of its id range, and the claim enclosing it.
    ends: Vec<u32>,
    enclosing: Vec<Option<usize>>,
    /// Per node: how many of its subtree's bytes some claim contains.
    claimed: Vec<u64>,
    dropped: Vec<DroppedClaim>,
}

impl Ownership {
    /// `claims` in id order with each one's enclosing claim, as the resolver
    /// produced them.
    pub(super) fn new(
        ctx: &Catalog,
        claims: Vec<(NodeId, Claim, Option<usize>)>,
        dropped: Vec<DroppedClaim>,
    ) -> Ownership {
        let ends = claims.iter().map(|(id, ..)| ctx.subtree(*id).end).collect();
        let enclosing = claims.iter().map(|(.., parent)| *parent).collect();
        let claims: Vec<(NodeId, Claim)> =
            claims.into_iter().map(|(id, claim, _)| (id, claim)).collect();

        // One reverse-preorder pass: children come after their parent, so by
        // the time a node is reached every child has added to it.
        let mut is_root = vec![false; ctx.len()];
        for (id, _) in &claims {
            is_root[id.0 as usize] = true;
        }
        let mut claimed = vec![0u64; ctx.len()];
        for ix in (0..ctx.len()).rev() {
            let node = ctx.node(NodeId(ix as u32));
            let adds = if is_root[ix] {
                claimed[ix] = node.subtree_size;
                node.rolled_size()
            } else {
                claimed[ix]
            };
            if let Some(parent) = node.parent {
                claimed[parent.0 as usize] += adds;
            }
        }
        Ownership { claims, ends, enclosing, claimed, dropped }
    }

    /// Every surviving claim, in id order.
    pub fn claims(&self) -> &[(NodeId, Claim)] {
        &self.claims
    }

    /// Claims some rule made that own nothing, with why.
    pub fn dropped(&self) -> &[DroppedClaim] {
        &self.dropped
    }

    /// The innermost claim containing `id`, `None` when `id` is arbitrary.
    pub fn owner(&self, id: NodeId) -> Option<&(NodeId, Claim)> {
        let mut at = self.claims.partition_point(|(start, _)| start.0 <= id.0).checked_sub(1);
        while let Some(ix) = at {
            if id.0 < self.ends[ix] {
                return Some(&self.claims[ix]);
            }
            at = self.enclosing[ix];
        }
        None
    }

    /// The claim rooted exactly at `id`.
    pub fn claim_at(&self, id: NodeId) -> Option<&Claim> {
        let ix = self.claims.binary_search_by_key(&id, |(start, _)| *start).ok()?;
        Some(&self.claims[ix].1)
    }

    /// Whether the claim at index `ix` of [`claims`](Self::claims) lies inside
    /// another one.
    pub fn is_nested(&self, ix: usize) -> bool {
        self.enclosing[ix].is_some()
    }

    /// The bytes of `id`'s subtree some claim contains.
    pub fn claimed(&self, id: NodeId) -> u64 {
        self.claimed.get(id.0 as usize).copied().unwrap_or(0)
    }

    /// The bytes of `id`'s subtree no claim contains: what "Other files"
    /// sizes it by.
    pub fn arbitrary(&self, ctx: &Catalog, id: NodeId) -> u64 {
        ctx.node(id).subtree_size.saturating_sub(self.claimed(id))
    }

    /// The "Other files" order of `id`'s children: biggest arbitrary bytes
    /// first, so a fully recognized folder sorts after every folder holding
    /// other files, by its full size. One order for the CLI and the GUI.
    pub fn children_by_arbitrary(&self, ctx: &Catalog, id: NodeId) -> Vec<NodeId> {
        let mut out = ctx.children(id).to_vec();
        out.sort_by_key(|&child| {
            (
                std::cmp::Reverse(self.arbitrary(ctx, child)),
                std::cmp::Reverse(ctx.node(child).subtree_size),
                child,
            )
        });
        out
    }

    /// The Recognized view: claims grouped by pack, biggest pack first, each
    /// pack's claims biggest first. One shape for the CLI and the GUI.
    pub fn recognized(&self, ctx: &Catalog) -> Vec<PackClaims> {
        let mut packs: Vec<PackClaims> = Vec::new();
        for (ix, (id, claim)) in self.claims.iter().enumerate() {
            let bytes = ctx.node(*id).subtree_size;
            let row = ClaimRow {
                id: *id,
                path: ctx.path(*id).display().to_string(),
                bytes,
                nested: self.is_nested(ix),
                claim: claim.clone(),
            };
            let pack = match packs.iter_mut().find(|p| p.pack == claim.provenance.pack) {
                Some(pack) => pack,
                None => {
                    packs.push(PackClaims {
                        pack: claim.provenance.pack.clone(),
                        bytes: 0,
                        claims: Vec::new(),
                    });
                    packs.last_mut().expect("just pushed")
                }
            };
            // A nested claim lies inside its own pack's outer claim, whose
            // bytes already count it.
            if !row.nested {
                pack.bytes += bytes;
            }
            pack.claims.push(row);
        }
        for pack in &mut packs {
            pack.claims.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        }
        packs.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.pack.cmp(&b.pack)));
        packs
    }
}

/// One pack's claims in the Recognized view.
#[derive(Debug, Clone, Serialize)]
pub struct PackClaims {
    pub pack: String,
    /// The pack's claimed bytes, each byte counted once.
    pub bytes: u64,
    pub claims: Vec<ClaimRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimRow {
    #[serde(skip)]
    pub id: NodeId,
    pub path: String,
    pub bytes: u64,
    /// Inside another claim of the same pack.
    pub nested: bool,
    pub claim: Claim,
}
