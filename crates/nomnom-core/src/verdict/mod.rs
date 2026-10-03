//! What a path IS, and whether it should go.
//!
//! Rule packs written in the nomnom rule language select targets ([`select`],
//! via [`judge`] and [`assess`]) from catalog metadata alone. Every verdict carries the sentence a human approves it on and the pack
//! and rule that produced it.

mod assess;
mod builtin;
mod packs;
mod select;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::catalog::{Catalog, NodeId};

pub use assess::{Assessment, Entry, Group, Reach, SharedFile, assess, assess_with, charges};
pub use builtin::builtin_pack;
pub use packs::{
    KnownPack, PackLookupError, PackRow, find_pack, pack_inventory, resolve_packs, resolve_sources,
};
pub use select::TrustedPack;

/// What the path IS — an open, interned name rather than a closed enum.
///
/// A pack introduces its own labels (`docs/lang.md`: "An identifier; packs may
/// introduce their own"), so this cannot be a Rust enum without making every
/// new toolchain a recompile. The four built-in names are `&'static str` and
/// therefore usable as associated consts; anything a pack introduces is an
/// `Arc<str>`, which keeps a clone a refcount bump on a value that is cloned
/// once per verdict over millions of nodes.
///
/// Equality, ordering and hashing all go through the name, so the two
/// representations of `"cache"` are one label, and [`Rollup::by_label`] orders
/// alphabetically rather than by whatever a pack happened to load first.
#[derive(Clone)]
pub struct Label(Repr);

#[derive(Clone)]
enum Repr {
    Static(&'static str),
    Owned(Arc<str>),
}

impl Label {
    /// Regenerated from source by a toolchain.
    pub const BUILD_OUTPUT: Label = Label(Repr::Static("build-output"));
    /// Refillable by re-fetching or re-computing.
    pub const CACHE: Label = Label(Repr::Static("cache"));
    /// Downloaded once and untouched since.
    pub const STALE_DOWNLOAD: Label = Label(Repr::Static("stale-download"));

    const BUILTIN: [Label; 3] = [Label::BUILD_OUTPUT, Label::CACHE, Label::STALE_DOWNLOAD];

    /// A built-in name resolves to its `&'static str` form, so the common case
    /// never allocates.
    pub fn new(name: &str) -> Label {
        match Label::BUILTIN.iter().find(|known| known.as_str() == name) {
            Some(known) => known.clone(),
            None => Label(Repr::Owned(Arc::from(name))),
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Static(name) => name,
            Repr::Owned(name) => name,
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Label({:?})", self.as_str())
    }
}

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Label {}

impl PartialOrd for Label {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Label {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl std::hash::Hash for Label {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

/// A plain string, so the JSON output stays readable.
impl Serialize for Label {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Label::new(&String::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Disposition {
    Keep,
    Reclaimable,
    /// Plausibly removable, but the evidence does not carry a deletion on its
    /// own. A human decides.
    Review,
}

/// Which pack and which rule produced a verdict.
///
/// `docs/lang.md`: "With packs coming from the network, 'why does nomnom want
/// to delete this' must be answerable down to the rule, so provenance is part
/// of the verdict rather than a debugging aid." It therefore travels inside the
/// verdict, into the plan and into the apply report, and is not reconstructible
/// after the fact from anything else.
///
/// It is also the rule's identity: unique across one run's packs, so a
/// consumer can group entries by it and act on every entry one rule produced.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Provenance {
    /// The pack's `name` from its `pack.toml`.
    pub pack: String,
    /// The rule's `[Title]`, unique within that pack.
    pub rule: String,
}

impl Provenance {
    pub fn new(pack: impl Into<String>, rule: impl Into<String>) -> Self {
        Self { pack: pack.into(), rule: rule.into() }
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.pack, self.rule)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub label: Label,
    pub disposition: Disposition,
    /// 0.0..=1.0.
    pub confidence: f32,
    /// The sentence a human reads before approving a deletion, and the slot a
    /// model fills from milestone 3 on. Never empty, and it names the concrete
    /// evidence rather than restating the label.
    pub reason: String,
    pub provenance: Provenance,
    /// Why this verdict is weaker than the rule that produced it asked for.
    ///
    /// `docs/lang.md`: a rule from an untrusted pack "is downgraded, and the
    /// CLI says why". The sentence lives beside [`Provenance`] rather than in a
    /// channel of its own because everything downstream — `--json`, the plan,
    /// the apply report — already carries the verdict and nothing else; a parallel
    /// map would have to be re-joined by `NodeId` at every one of them, and the
    /// first consumer that forgot would silently drop the explanation while
    /// still showing the downgraded disposition.
    ///
    /// `None` is the ordinary case: the rule got the disposition it wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped: Option<String>,
}

/// Every rule verdict over the whole catalog, in id order.
///
/// `packs` in resolution order: built-in first, then user, project and
/// `--pack`, each overriding the last. A rule target is one decision for its
/// whole subtree, so no verdict lies inside another rule verdict's subtree —
/// which is what keeps [`Rollup::reclaimable_bytes`] sound.
pub fn judge(ctx: &Catalog, packs: &[TrustedPack]) -> Vec<(NodeId, Verdict)> {
    select::select(ctx, packs, None)
}

/// What the CLI prints after an assessment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rollup {
    /// Sum of `subtree_size` over every [`Disposition::Reclaimable`] verdict.
    /// Sound only because [`judge`] never puts a verdict inside a rule
    /// target's subtree.
    pub reclaimable_bytes: u64,
    pub by_label: BTreeMap<Label, Vec<NodeId>>,
}

pub fn rollup(ctx: &Catalog, verdicts: &[(NodeId, Verdict)]) -> Rollup {
    let mut out = Rollup::default();
    for (id, verdict) in verdicts {
        if verdict.disposition == Disposition::Reclaimable {
            out.reclaimable_bytes += ctx.node(*id).rolled_size();
        }
        out.by_label.entry(verdict.label.clone()).or_default().push(*id);
    }
    out
}

/// The node's own file-name component. The root's name is its full path, so
/// a plain compare would never match there.
fn node_name(ctx: &Catalog, id: NodeId) -> String {
    let name = ctx.name(id);
    Path::new(name).file_name().unwrap_or(name).to_string_lossy().into_owned()
}

/// NTFS and the Windows API are case-insensitive, so `Node_Modules` is the
/// same directory there and a different one on ext4.
#[cfg(windows)]
fn name_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(not(windows))]
fn name_eq(a: &str, b: &str) -> bool {
    a == b
}
