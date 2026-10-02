//! The rule-language judge: packs in, verdicts out.
//!
//! [`DslJudge`] holds loaded packs in resolution order and answers [`Judge`] by
//! evaluating every rule's `when` against a [`Facts`] view of one catalog node.
//! Nothing about what a rule *means* lives here — that is `nomnom_lang::eval`.
//! This module supplies the facts and resolves conflicts.
//!
//! Duplicates stay in Rust. Duplicate detection needs a whole-catalog
//! size-then-hash pass and the path of the copy that survives, neither of which
//! the language can express (`docs/lang.md`: "the language sees one node at a
//! time, and Rust supplies the facts that a single node cannot know about
//! itself"). So this judge composes: [`DuplicateFacts`] answers first for the
//! nodes it knows, the packs answer for everything else.
//!
//! The trust cap lives here too, for the reason `nomnom_pack::trust` gives: it
//! is a value applied to the winning rule's disposition, never an edit to the
//! loaded pack, so [`Verdict::capped`] can still say what the rule asked for.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use nomnom_lang::ast::{Disposition as AstDisposition, Expr, Literal};
use nomnom_lang::eval::{Facts, Value, eval, render_reason};
use nomnom_lang::pack::{LoadedRule, Pack};
use nomnom_lang::vocab::{Field, Predicate};
use nomnom_pack::Trust;

use super::duplicate::{DuplicateFacts, MIN_DUPLICATE_SIZE};
use super::{Disposition, Judge, Label, Provenance, Verdict, builtin_pack, name_eq, node_name};
use crate::catalog::{Catalog, NodeId};
use crate::scan::EntryKind;

/// A loaded pack plus the trust that decides whether its rules may propose a
/// deletion.
///
/// The two travel together because the cap is applied at the moment a rule
/// wins, and a pack separated from its trust state would be capped by whatever
/// the caller remembered — which is the direction that loses data.
#[derive(Debug, Clone)]
pub struct TrustedPack {
    pub pack: Pack,
    pub trust: Trust,
}

impl TrustedPack {
    /// The pack compiled into the binary. `docs/lang.md`: never capped.
    pub fn builtin(pack: Pack) -> TrustedPack {
        TrustedPack { pack, trust: Trust::Builtin }
    }
}

/// Bound to the catalog it was built from: both the unit set and the duplicate
/// groups are whole-catalog answers, resolved once here rather than re-derived
/// per node.
pub struct DslJudge {
    /// Resolution order, later overriding earlier — `docs/lang.md` makes a
    /// later-resolved pack win a confidence tie.
    packs: Vec<TrustedPack>,
    /// Every glob any rule mentions, compiled once. The arguments are literals,
    /// so the whole set is knowable before a single node is judged.
    globs: HashMap<String, GlobMatcher>,
    /// Nodes whose winning verdict is a `unit`. Consulted by [`under_unit`].
    units: HashSet<NodeId>,
    duplicates: DuplicateFacts,
}

impl DslJudge {
    /// The built-in pack alone.
    pub fn new(ctx: &Catalog) -> Self {
        Self::with_packs(ctx, vec![TrustedPack::builtin(builtin_pack().clone())])
    }

    /// `packs` in resolution order: built-in first, then user, project and
    /// `--pack`, each overriding the last.
    pub fn with_packs(ctx: &Catalog, packs: Vec<TrustedPack>) -> Self {
        let mut globs = HashMap::new();
        for source in &packs {
            for loaded in &source.pack.rules {
                collect_globs(&loaded.rule.when, &mut globs);
            }
        }

        // The unit set has to exist before `DuplicateFacts::build`, which drops
        // any copy sitting inside a condemned subtree. It is therefore resolved
        // with no duplicate facts available, which is why `is_duplicate` reads
        // `false` during this pass: a rule cannot make a subtree a unit on the
        // strength of a duplicate, and the alternative is a cycle.
        let mut units = HashSet::new();
        let mut queue = vec![ctx.root()];
        let mut cursor = 0;
        let probe =
            Self { packs, globs, units: HashSet::new(), duplicates: DuplicateFacts::empty() };
        while cursor < queue.len() {
            let id = queue[cursor];
            cursor += 1;
            if probe.rule_verdict(ctx, id, false).is_some_and(|verdict| verdict.unit) {
                units.insert(id);
                continue;
            }
            queue.extend_from_slice(ctx.children(id));
        }

        let duplicates = DuplicateFacts::build(ctx, MIN_DUPLICATE_SIZE, &units);
        Self { packs: probe.packs, globs: probe.globs, units, duplicates }
    }

    /// The nodes whose verdict speaks for their whole subtree.
    pub fn units(&self) -> &HashSet<NodeId> {
        &self.units
    }

    /// The highest-priority rule verdict for one node, or `None` when no rule
    /// fires.
    fn rule_verdict(&self, ctx: &Catalog, id: NodeId, is_duplicate: bool) -> Option<Verdict> {
        let facts = NodeFacts { ctx, id, now: SystemTime::now(), is_duplicate, globs: &self.globs };

        // `docs/lang.md`: highest confidence, then pack precedence (later-
        // resolved pack wins), then rule order within the pack. The pack index
        // is what carries the second rule: on an equal confidence a later pack
        // takes over and a later rule in the *same* pack does not, which is why
        // this cannot be one `>=` — that would hand a confidence tie to the
        // last rule of the winning pack instead of its first.
        let mut best: Option<(usize, &TrustedPack, &LoadedRule)> = None;
        for (index, source) in self.packs.iter().enumerate() {
            for loaded in &source.pack.rules {
                if !eval(&loaded.rule.when, &facts) {
                    continue;
                }
                let confidence = loaded.rule.then.confidence.value;
                let better = match best {
                    None => true,
                    Some((best_index, _, current)) => {
                        let best_confidence = current.rule.then.confidence.value;
                        confidence > best_confidence
                            || (confidence == best_confidence && index > best_index)
                    }
                };
                if better {
                    best = Some((index, source, loaded));
                }
            }
        }

        let (_, source, loaded) = best?;
        let conclusion = &loaded.rule.then;
        // The cap is applied to the winner, not to the candidates: conflict
        // resolution is confidence, then pack precedence, then rule order
        // (`docs/lang.md`), and letting an untrusted pack's downgraded rule lose
        // to a weaker one would change which rule a human is shown.
        let capped = source.trust.cap(conclusion.disposition.value);
        Some(Verdict {
            label: Label::new(&conclusion.label.value),
            disposition: disposition_of(capped.disposition),
            confidence: conclusion.confidence.value,
            reason: render_reason(&conclusion.reason.value, &facts),
            unit: conclusion.unit.value,
            provenance: Provenance::new(&source.pack.name, &loaded.rule.name.value),
            capped: capped.explanation(&source.pack.name),
        })
    }
}

impl Judge for DslJudge {
    fn assess(&self, ctx: &Catalog, node: NodeId) -> Option<Verdict> {
        // Duplicates first, matching what the hand-written judge did: a
        // duplicate is a file, and every rule that could also fire on a file is
        // weaker evidence than byte-identical content.
        self.duplicates
            .verdict(node)
            .or_else(|| self.rule_verdict(ctx, node, self.duplicates.contains(node)))
    }
}

fn disposition_of(disposition: AstDisposition) -> Disposition {
    match disposition {
        AstDisposition::Keep => Disposition::Keep,
        AstDisposition::Reclaimable => Disposition::Reclaimable,
        AstDisposition::Review => Disposition::Review,
    }
}

/// Every glob literal any `matches` or `sibling_matches` call mentions.
fn collect_globs(expr: &Expr, out: &mut HashMap<String, GlobMatcher>) {
    match expr {
        Expr::Call { predicate, args, .. } => {
            if !matches!(predicate.value, Predicate::Matches | Predicate::SiblingMatches) {
                return;
            }
            for arg in args {
                if let Literal::Str(pattern) = &arg.value {
                    out.entry(pattern.clone()).or_insert_with(|| compile_glob(pattern));
                }
            }
        }
        Expr::Not { operand, .. } => collect_globs(operand, out),
        Expr::And { lhs, rhs } | Expr::Or { lhs, rhs } => {
            collect_globs(lhs, out);
            collect_globs(rhs, out);
        }
        Expr::Bool(_) | Expr::Field(_) | Expr::Compare { .. } => {}
    }
}

/// Case folding follows the platform, exactly as `name_eq` does: a glob is a
/// name comparison with holes in it, so `*.CSPROJ` has to match `app.csproj` on
/// NTFS for the same reason `Node_Modules` matches `node_modules` there.
///
/// A pattern that will not compile matches nothing. The parser cannot reject it
/// — a glob is a plain string to the language — and a rule that never fires is
/// a far better failure than a panic in a walk over a million paths.
fn compile_glob(pattern: &str) -> GlobMatcher {
    let build = |pattern: &str| {
        GlobBuilder::new(pattern)
            .case_insensitive(cfg!(windows))
            .literal_separator(false)
            .build()
            .map(|glob| glob.compile_matcher())
    };
    build(pattern).unwrap_or_else(|_| build("").expect("the empty glob always compiles"))
}

/// One catalog node, seen through the language's vocabulary.
struct NodeFacts<'a> {
    ctx: &'a Catalog,
    id: NodeId,
    /// Sampled once per node so every age in one verdict is read off the same
    /// clock, and a reason cannot say "89 days" while the rule that produced it
    /// tested for 90.
    now: SystemTime,
    is_duplicate: bool,
    globs: &'a HashMap<String, GlobMatcher>,
}

impl NodeFacts<'_> {
    fn name(&self) -> String {
        node_name(self.ctx, self.id)
    }

    fn kind(&self) -> EntryKind {
        self.ctx.node(self.id).kind
    }

    /// How long ago `stamp` was, or [`Value::Absent`] when there is no stamp.
    ///
    /// A stamp in the future is age zero rather than absent: a clock skew is
    /// not evidence that a file is old, and every rule here asks whether
    /// something is *older* than a threshold.
    fn age(&self, stamp: Option<SystemTime>) -> Value {
        match stamp {
            Some(stamp) => {
                Value::Duration(self.now.duration_since(stamp).map(|d| d.as_secs()).unwrap_or(0))
            }
            None => Value::Absent,
        }
    }

    fn matches_glob(&self, pattern: &str, name: &str) -> bool {
        self.globs.get(pattern).is_some_and(|glob| glob.is_match(name))
    }

    fn children_of(&self, id: NodeId) -> &[NodeId] {
        self.ctx.children(id)
    }
}

impl Facts for NodeFacts<'_> {
    fn field(&self, field: Field) -> Value {
        let node = self.ctx.node(self.id);
        match field {
            Field::Name => Value::Str(self.name()),
            // Absent on the wrong kind, which is how `dir.name == "target"`
            // stays directory-only: an absent value makes every comparison
            // false.
            Field::DirName => match self.kind() {
                EntryKind::Dir => Value::Str(self.name()),
                _ => Value::Absent,
            },
            Field::FileName => match self.kind() {
                EntryKind::File => Value::Str(self.name()),
                _ => Value::Absent,
            },
            Field::Ext => match Path::new(&self.name()).extension() {
                Some(ext) => Value::Str(ext.to_string_lossy().into_owned()),
                None => Value::Absent,
            },
            Field::Path => Value::Str(self.ctx.path(self.id).display().to_string()),
            Field::Size => Value::Size(node.size),
            Field::SubtreeSize => Value::Size(node.subtree_size),
            Field::FileCount => Value::Num(node.file_count as f64),
            Field::DirCount => Value::Num(node.dir_count as f64),
            Field::Depth => Value::Num(f64::from(node.depth)),
            Field::IsDir => Value::Bool(node.kind == EntryKind::Dir),
            Field::IsFile => Value::Bool(node.kind == EntryKind::File),
            Field::IsSymlink => Value::Bool(node.kind == EntryKind::Symlink),
            Field::IsDuplicate => Value::Bool(self.is_duplicate),
            Field::ModifiedAge => self.age(node.modified),
            Field::AccessedAge => self.age(node.accessed),
            Field::MaxDescendantAge => self.age(node.max_modified),
            Field::HasAccessed => Value::Bool(node.accessed.is_some()),
        }
    }

    /// A malformed argument list is unreachable — the parser checks arity and
    /// parameter types — and answers `false` rather than panicking, for the
    /// same reason `eval` does.
    fn predicate(&self, predicate: Predicate, args: &[Literal]) -> bool {
        match predicate {
            Predicate::Sibling => self.with_str(args, |name| {
                self.ctx.node(self.id).parent.is_some_and(|parent| self.has_child(parent, name))
            }),
            Predicate::Child => self.with_str(args, |name| self.has_child(self.id, name)),
            Predicate::Ancestor => self.with_str(args, |name| {
                let mut cursor = self.ctx.node(self.id).parent;
                while let Some(current) = cursor {
                    if name_eq(&node_name(self.ctx, current), name) {
                        return true;
                    }
                    cursor = self.ctx.node(current).parent;
                }
                false
            }),
            Predicate::Matches => {
                self.with_str(args, |pattern| self.matches_glob(pattern, &self.name()))
            }
            Predicate::SiblingMatches => self.with_str(args, |pattern| {
                self.ctx.node(self.id).parent.is_some_and(|parent| {
                    self.children_of(parent)
                        .iter()
                        .any(|&child| self.matches_glob(pattern, &node_name(self.ctx, child)))
                })
            }),
            Predicate::ModifiedBefore => self.older_than(args, self.ctx.node(self.id).modified),
            Predicate::AccessedBefore => self.older_than(args, self.ctx.node(self.id).accessed),
        }
    }
}

impl NodeFacts<'_> {
    fn with_str(&self, args: &[Literal], f: impl FnOnce(&str) -> bool) -> bool {
        match args.first() {
            Some(Literal::Str(arg)) => f(arg),
            _ => false,
        }
    }

    fn has_child(&self, parent: NodeId, name: &str) -> bool {
        self.children_of(parent).iter().any(|&child| name_eq(&node_name(self.ctx, child), name))
    }

    /// Absent answers false, which is the conservative reading — an unknown
    /// access time is not evidence of staleness.
    fn older_than(&self, args: &[Literal], stamp: Option<SystemTime>) -> bool {
        let Some(Literal::Duration(threshold)) = args.first() else {
            return false;
        };
        matches!(self.age(stamp), Value::Duration(age) if age >= *threshold)
    }
}
