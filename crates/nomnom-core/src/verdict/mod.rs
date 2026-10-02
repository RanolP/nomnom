//! What a path IS, and whether it should go.
//!
//! The whole domain is one trait. Milestone 1 answered with hand-written Rust;
//! milestone 2 answers with [`DslJudge`], which evaluates rule packs written in
//! the nomnom rule language, and milestone 3 swaps in a reasoning model. Each
//! implements the same [`Judge`] and fills the same [`Verdict::reason`] slot.
//! Nothing else in the codebase needs to know which one answered, which is the
//! point: the ladder costs one trait and one mandatory field.

mod assess;
mod builtin;
mod dsl;
mod duplicate;
mod packs;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::catalog::{Catalog, NodeId};

pub use assess::{Assessment, Entry, Group, Reach, SharedFile, assess, charges};
pub use builtin::builtin_pack;
pub use dsl::{DslJudge, TrustedPack};
pub use packs::{
    KnownPack, PackLookupError, PackRow, find_pack, pack_inventory, resolve_packs, resolve_sources,
};

/// Answers "what is this path, and should it go?" for one node.
///
/// Returning `None` means "no opinion" — the common case. A judge is expected
/// to stay silent rather than guess, because every verdict costs a human a
/// read.
pub trait Judge {
    fn assess(&self, ctx: &Catalog, node: NodeId) -> Option<Verdict>;
}

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
    /// Byte-identical to another file in the catalog.
    pub const DUPLICATE: Label = Label(Repr::Static("duplicate"));
    /// Downloaded once and untouched since.
    pub const STALE_DOWNLOAD: Label = Label(Repr::Static("stale-download"));

    const BUILTIN: [Label; 4] =
        [Label::BUILD_OUTPUT, Label::CACHE, Label::DUPLICATE, Label::STALE_DOWNLOAD];

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The pack's `name` from its `pack.toml`.
    pub pack: String,
    /// The rule's name, unique within that pack.
    pub rule: String,
}

impl Provenance {
    pub fn new(pack: impl Into<String>, rule: impl Into<String>) -> Self {
        Self { pack: pack.into(), rule: rule.into() }
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.pack, self.rule)
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
    /// This verdict speaks for the whole subtree: the directory is one
    /// decision, not one per file inside it. Straight from the rule's `unit`
    /// field, never inferred from the label — a pack chooses its own labels, so
    /// a label cannot carry this meaning.
    pub unit: bool,
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

/// Run `judge` over the whole catalog.
///
/// Breadth-first from the root, and a node whose verdict is a
/// [`unit`](Verdict::unit) ends the descent there: the directory as a whole is
/// the verdict, so its children never produce their own. That is what keeps
/// [`Rollup::reclaimable_bytes`] sound — a child verdict inside an already
/// counted `subtree_size` would add the same bytes a second time.
pub fn assess_all(judge: &dyn Judge, ctx: &Catalog) -> Vec<(NodeId, Verdict)> {
    let mut out = Vec::new();
    let mut queue = vec![ctx.root()];
    let mut cursor = 0;
    while cursor < queue.len() {
        let id = queue[cursor];
        cursor += 1;
        let mut prune = false;
        if let Some(verdict) = judge.assess(ctx, id) {
            prune = verdict.unit;
            out.push((id, verdict));
        }
        if !prune {
            queue.extend_from_slice(ctx.children(id));
        }
    }
    out
}

/// What the CLI prints after an assessment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rollup {
    /// Sum of `subtree_size` over every [`Disposition::Reclaimable`] verdict.
    /// Sound only because [`assess_all`] never judges inside a subtree it
    /// already judged as a unit.
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

/// Whether any ancestor of `id` is judged as a whole unit.
///
/// `units` is the set [`DslJudge`] resolves in one top-down pass before
/// anything else runs. Re-deriving it per ancestor would mean re-evaluating
/// every rule once per level of every path in the catalog.
fn under_unit(ctx: &Catalog, id: NodeId, units: &HashSet<NodeId>) -> bool {
    let mut cursor = ctx.node(id).parent;
    while let Some(current) = cursor {
        if units.contains(&current) {
            return true;
        }
        cursor = ctx.node(current).parent;
    }
    false
}
