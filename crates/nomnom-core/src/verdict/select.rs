//! The rule-language selector: packs in, one verdict per target out.
//!
//! Matching works the way a browser matches CSS selectors, right to left. A
//! rule's `then $p/a/target/` is read from its last literal segment — the key —
//! so a rule is only ever tried on the handful of nodes that carry that name,
//! looked up in one name index built for the whole catalog. From a key node the
//! check climbs: the parent segments, then the anchor `$p`, then the anchor's
//! constraints in order of cost. A rule with no literal segment is keyed on a
//! `has` name instead (the anchor is the parent of a hit), then on an `under`
//! name (the anchor lies inside one of that name's subtrees), and only a rule
//! with none of the three is scanned against every node — the universal
//! bucket, which gets a timing line of its own because it is the slow path a
//! pack author can fall into.
//!
//! `under` is answered from subtree ranges rather than by walking parents:
//! catalog ids are preorder, so "some strict ancestor is named `Downloads`" is
//! "the id lies strictly inside the id range of some node named `Downloads`",
//! which is one binary search over the merged ranges. A bloom filter over
//! ancestor names would need a per-node filter built top-down; the ranges fall
//! out of the name index that already exists and answer exactly, not probably.
//!
//! Nothing about what a field test *means* lives here — that is
//! `nomnom_lang::eval`. This module supplies the facts, finds the targets and
//! resolves conflicts.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Instant, SystemTime};

use globset::{GlobBuilder, GlobMatcher};
use nomnom_lang::pack::Pack;
use nomnom_lang::vocab::{Field, Ty};
use nomnom_lang::{
    ChildTest, CmpOp, Constraint, Disposition as AstDisposition, Facts, FieldTest, Literal,
    NamePattern, Rule, Value, check, render_reason,
};
use nomnom_pack::Trust;

use super::claim::{Claim, DropReason, DroppedClaim, Ownership};
use super::{Disposition, Label, Provenance, Verdict, name_eq, node_name};
use crate::scan::{ScanProgress, Stage, advance};
use crate::catalog::{Catalog, NodeId};
use crate::scan::EntryKind;
use crate::timings;

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
    /// A pack compiled into the binary. `docs/lang.md`: never capped.
    pub fn builtin(pack: Pack) -> TrustedPack {
        TrustedPack { pack, trust: Trust::Builtin }
    }

    /// Every built-in pack, in resolution order.
    pub fn builtins() -> Vec<TrustedPack> {
        super::builtin_packs().iter().cloned().map(TrustedPack::builtin).collect()
    }
}

/// Every target some rule selects, each with the one rule that won it, with
/// targets inside another target already dropped, in id order; and who owns
/// which subtree.
pub(super) fn select(
    ctx: &Catalog,
    packs: &[TrustedPack],
    progress: Option<&ScanProgress>,
) -> (Vec<(NodeId, Verdict)>, Ownership) {
    let started = Instant::now();
    // The front-end may have entered Index already, before resolving packs.
    if let Some(progress) = progress {
        match progress.stage() {
            Stage::Index => progress.set_stage_total(ctx.len() as u64),
            _ => progress.enter(Stage::Index, ctx.len() as u64),
        }
    }
    let rules = compile(packs);
    let index = NameIndex::build(ctx, &rules, progress);
    let started = timings::lap("assess: name index", started);

    // One unit per rule: keyed rules first, then the universal ones.
    if let Some(progress) = progress {
        progress.enter(Stage::Match, rules.len() as u64);
    }
    let clock = Clock { now: SystemTime::now() };
    let mut hits = Vec::new();
    let mut universal = Vec::new();
    let mut ran = 0;
    for rule in &rules {
        match &rule.key {
            Key::Universal => universal.push(rule),
            key => {
                rule.run_keyed(ctx, &index, key, &clock, &mut hits);
                ran += 1;
                advance(progress, ran);
            }
        }
    }
    let mut started = timings::lap("assess: rule match", started);
    if !universal.is_empty() {
        for rule in &universal {
            for id in 0..ctx.len() as u32 {
                rule.try_anchor(ctx, &index, NodeId(id), &clock, &mut hits);
            }
            ran += 1;
            advance(progress, ran);
        }
        started = timings::lap(
            &format!("assess: universal rules ({} with no key, full scan)", universal.len()),
            started,
        );
    }

    // Resolve and group run in well under a tenth of a second; no count.
    if let Some(progress) = progress {
        progress.enter(Stage::Group, 0);
    }
    let resolved = resolve(ctx, packs, &rules, hits, &clock);
    timings::lap("assess: resolve conflicts and nesting", started);
    resolved
}

/// One rule, ready to match.
struct Compiled<'a> {
    /// Position in the compiled list, which is what a [`Hit`] cites.
    id: usize,
    pack: usize,
    /// Position within the pack, the last conflict tie-break.
    order: usize,
    rule: &'a Rule,
    segments: Vec<Pattern>,
    key: Key,
    /// Cheapest first: `under` ranges, then non-string fields, then one pass
    /// over the children for every `has`/`lacks`, then string fields, which
    /// allocate.
    under: Vec<String>,
    cheap_fields: Vec<&'a FieldTest>,
    children: Vec<Children<'a>>,
    string_fields: Vec<&'a FieldTest>,
}

enum Key {
    /// Seeded from nodes named this (folded); `depth` is the segment's index.
    Name {
        name: String,
        depth: usize,
    },
    /// Seeded from nodes named this (folded), each its own anchor: the rule
    /// pins the anchor's own name with `$v.name == "x"`.
    Anchor(String),
    /// Seeded from the parents of nodes named any of these (folded).
    Has(Vec<String>),
    /// Seeded from every node strictly inside a subtree named this (folded).
    Under(String),
    Universal,
}

enum Pattern {
    Literal(String),
    Glob(GlobMatcher),
}

impl Pattern {
    fn of(pattern: &NamePattern) -> Pattern {
        match pattern {
            NamePattern::Literal(name) => Pattern::Literal(name.clone()),
            NamePattern::Glob(glob) => Pattern::Glob(compile_glob(glob)),
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Pattern::Literal(literal) => name_eq(name, literal),
            Pattern::Glob(glob) => glob.is_match(name),
        }
    }
}

struct Children<'a> {
    test: &'a ChildTest,
    alternatives: Vec<Pattern>,
}

fn compile(packs: &[TrustedPack]) -> Vec<Compiled<'_>> {
    let mut out = Vec::new();
    for (pack, source) in packs.iter().enumerate() {
        for (order, loaded) in source.pack.rules.iter().enumerate() {
            let rule = &loaded.rule;
            let mut compiled = Compiled {
                id: out.len(),
                pack,
                order,
                rule,
                segments: rule.filter.then.segments.iter().map(|s| Pattern::of(&s.value)).collect(),
                key: Key::Universal,
                under: Vec::new(),
                cheap_fields: Vec::new(),
                children: Vec::new(),
                string_fields: Vec::new(),
            };
            for constraint in &rule.filter.constraints {
                match constraint {
                    Constraint::Under(name) => compiled.under.push(name.value.clone()),
                    Constraint::Field(test) if test.field.value.ty() == Ty::Str => {
                        compiled.string_fields.push(test)
                    }
                    Constraint::Field(test) => compiled.cheap_fields.push(test),
                    Constraint::Children(test) => compiled.children.push(Children {
                        test,
                        alternatives: test.names.iter().map(|n| Pattern::of(&n.value)).collect(),
                    }),
                }
            }
            compiled.key = key_of(rule);
            out.push(compiled);
        }
    }
    out
}

/// The most selective literal the rule offers, in the order the selector can
/// exploit it.
fn key_of(rule: &Rule) -> Key {
    let segments = &rule.filter.then.segments;
    if let Some((depth, name)) =
        segments.iter().enumerate().rev().find_map(|(i, s)| Some((i, s.value.literal()?)))
    {
        return Key::Name { name: fold(name), depth };
    }
    let constraints = &rule.filter.constraints;
    if let Some(name) = constraints.iter().find_map(|c| match c {
        Constraint::Field(test) if !test.negated => anchor_name(test),
        _ => None,
    }) {
        return Key::Anchor(fold(name));
    }
    let has = constraints.iter().find_map(|c| match c {
        Constraint::Children(test) if !test.negated => {
            test.names.iter().map(|n| n.value.literal().map(fold)).collect::<Option<Vec<_>>>()
        }
        _ => None,
    });
    if let Some(names) = has {
        return Key::Has(names);
    }
    if let Some(name) = constraints.iter().find_map(|c| match c {
        Constraint::Under(name) => Some(fold(&name.value)),
        _ => None,
    }) {
        return Key::Under(name);
    }
    Key::Universal
}

/// The literal a `$v.name == "x"` test pins the anchor's own name to. All
/// three name fields compare by the platform's name equality, so a node that
/// carries this name in the index is exactly a node the test can pass.
fn anchor_name(test: &FieldTest) -> Option<&str> {
    let (op, literal) = test.compare.as_ref()?;
    match (&test.field.value, &op.value, &literal.value) {
        (Field::Name | Field::DirName | Field::FileName, CmpOp::Eq, Literal::Str(name)) => {
            Some(name)
        }
        _ => None,
    }
}

/// The platform's name equality as a hash key: ASCII case folded on Windows,
/// exactly what `name_eq` ignores.
fn fold(name: &str) -> String {
    if cfg!(windows) { name.to_ascii_lowercase() } else { name.to_owned() }
}

/// The nodes carrying each name some rule keys on or tests `under`, from one
/// pass over the catalog.
struct NameIndex {
    by_name: HashMap<String, Vec<NodeId>>,
    /// Per `under` name: the outermost subtrees of nodes with that name, as
    /// disjoint id ranges in order.
    ranges: HashMap<String, Vec<(u32, u32)>>,
}

impl NameIndex {
    fn build(ctx: &Catalog, rules: &[Compiled], progress: Option<&ScanProgress>) -> NameIndex {
        let mut by_name: HashMap<String, Vec<NodeId>> = HashMap::new();
        for rule in rules {
            match &rule.key {
                Key::Name { name, .. } | Key::Anchor(name) => {
                    by_name.entry(name.clone()).or_default();
                }
                Key::Has(names) => {
                    for name in names {
                        by_name.entry(name.clone()).or_default();
                    }
                }
                Key::Under(_) | Key::Universal => {}
            }
            for name in &rule.under {
                by_name.entry(fold(name)).or_default();
            }
        }

        // Most names in a catalog are not wanted, and their length alone says
        // so without hashing them.
        let longest = by_name.keys().map(String::len).max().unwrap_or(0);
        let mut wanted_len = vec![false; longest + 1];
        for name in by_name.keys() {
            wanted_len[name.len()] = true;
        }
        let root = ctx.root();
        let mut folded = String::new();
        for id in (0..ctx.len() as u32).map(NodeId) {
            if (id.0 as usize).is_multiple_of(Stage::BATCH) {
                advance(progress, id.0 as usize);
            }
            let owned;
            let name = if id == root {
                owned = node_name(ctx, id);
                owned.as_str()
            } else {
                match ctx.name(id).to_str() {
                    Some(name) => name,
                    None => continue,
                }
            };
            if !wanted_len.get(name.len()).copied().unwrap_or(false) {
                continue;
            }
            folded.clear();
            folded.push_str(name);
            if cfg!(windows) {
                folded.make_ascii_lowercase();
            }
            if let Some(ids) = by_name.get_mut(folded.as_str()) {
                ids.push(id);
            }
        }

        let mut ranges = HashMap::new();
        for rule in rules {
            for name in &rule.under {
                let name = fold(name);
                if ranges.contains_key(&name) {
                    continue;
                }
                // Ids come out of the pass in order, so the first range that
                // contains a later one is already behind it.
                let mut outer: Vec<(u32, u32)> = Vec::new();
                for &id in &by_name[&name] {
                    let span = ctx.subtree(id);
                    if outer.last().is_none_or(|last| span.start >= last.1) {
                        outer.push((span.start, span.end));
                    }
                }
                ranges.insert(name, outer);
            }
        }
        NameIndex { by_name, ranges }
    }

    fn named(&self, name: &str) -> &[NodeId] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether some strict ancestor of `id` is named `name`.
    fn under(&self, name: &str, id: NodeId) -> bool {
        let Some(ranges) = self.ranges.get(&fold(name)) else { return false };
        let at = ranges.partition_point(|range| range.0 < id.0);
        at > 0 && id.0 < ranges[at - 1].1
    }
}

/// One rule matching one target.
struct Hit {
    target: NodeId,
    anchor: NodeId,
    rule: usize,
    /// `as $m` bindings: the variable and the on-disk name it captured.
    captures: Vec<(usize, String)>,
}

impl Compiled<'_> {
    fn run_keyed(
        &self,
        ctx: &Catalog,
        index: &NameIndex,
        key: &Key,
        clock: &Clock,
        hits: &mut Vec<Hit>,
    ) {
        match key {
            Key::Name { name, depth } => {
                for &seed in index.named(name) {
                    let Some(anchor) = self.climb(ctx, seed, *depth) else { continue };
                    let Some(captures) = self.anchor_holds(ctx, index, anchor, clock) else {
                        continue;
                    };
                    self.descend(ctx, seed, depth + 1, anchor, &captures, hits);
                }
            }
            Key::Anchor(name) => {
                for &anchor in index.named(name) {
                    self.try_anchor(ctx, index, anchor, clock, hits);
                }
            }
            Key::Has(names) => {
                let mut anchors: Vec<NodeId> = names
                    .iter()
                    .flat_map(|name| index.named(name))
                    .filter_map(|&hit| ctx.node(hit).parent)
                    .collect();
                anchors.sort_unstable();
                anchors.dedup();
                for anchor in anchors {
                    self.try_anchor(ctx, index, anchor, clock, hits);
                }
            }
            Key::Under(name) => {
                for &(start, end) in index.ranges.get(name).map_or(&[][..], Vec::as_slice) {
                    for id in start + 1..end {
                        self.try_anchor(ctx, index, NodeId(id), clock, hits);
                    }
                }
            }
            Key::Universal => unreachable!("the universal bucket is scanned by the caller"),
        }
    }

    fn try_anchor(
        &self,
        ctx: &Catalog,
        index: &NameIndex,
        anchor: NodeId,
        clock: &Clock,
        hits: &mut Vec<Hit>,
    ) {
        if let Some(captures) = self.anchor_holds(ctx, index, anchor, clock) {
            self.descend(ctx, anchor, 0, anchor, &captures, hits);
        }
    }

    /// The anchor of a key node matching segment `depth`, when the segments
    /// above it match its ancestors.
    fn climb(&self, ctx: &Catalog, seed: NodeId, depth: usize) -> Option<NodeId> {
        let mut cursor = seed;
        for segment in self.segments[..depth].iter().rev() {
            cursor = ctx.node(cursor).parent?;
            if !segment.matches(&node_name(ctx, cursor)) {
                return None;
            }
        }
        ctx.node(cursor).parent
    }

    /// Every target below `from`, which already matched the segments before
    /// `depth`.
    fn descend(
        &self,
        ctx: &Catalog,
        from: NodeId,
        depth: usize,
        anchor: NodeId,
        captures: &[(usize, String)],
        hits: &mut Vec<Hit>,
    ) {
        let Some(segment) = self.segments.get(depth) else {
            let then = &self.rule.filter.then;
            // A scan root is the fence a cleanup plan is anchored at, never a
            // thing on the plan.
            if ctx.node(from).parent.is_none()
                || (then.dir && ctx.node(from).kind != EntryKind::Dir)
            {
                return;
            }
            hits.push(Hit { target: from, anchor, rule: self.id, captures: captures.to_vec() });
            return;
        };
        for &child in ctx.children(from) {
            if segment.matches(&node_name(ctx, child)) {
                self.descend(ctx, child, depth + 1, anchor, captures, hits);
            }
        }
    }

    /// The anchor's constraints, cheapest first. `Some` with the captures when
    /// every one holds.
    fn anchor_holds(
        &self,
        ctx: &Catalog,
        index: &NameIndex,
        anchor: NodeId,
        clock: &Clock,
    ) -> Option<Vec<(usize, String)>> {
        if !self.under.iter().all(|name| index.under(name, anchor)) {
            return None;
        }
        let facts = NodeFacts { ctx, id: anchor, now: clock.now };
        if !self.cheap_fields.iter().all(|test| check(test, &facts)) {
            return None;
        }
        let captures = self.children_hold(ctx, anchor)?;
        if !self.string_fields.iter().all(|test| check(test, &facts)) {
            return None;
        }
        Some(captures)
    }

    /// Every `has` and `lacks`, answered from one pass over the children.
    ///
    /// A capture reports the first alternative in written order that some
    /// child matches, and among the children matching it the smallest name, so
    /// the sentence a human reads does not depend on scan order.
    fn children_hold(&self, ctx: &Catalog, anchor: NodeId) -> Option<Vec<(usize, String)>> {
        if self.children.is_empty() {
            return Some(Vec::new());
        }
        // Per test: the best alternative seen so far and the name it matched.
        let mut best: Vec<Option<(usize, String)>> = vec![None; self.children.len()];
        for &child in ctx.children(anchor) {
            let name = node_name(ctx, child);
            for (slot, test) in best.iter_mut().zip(&self.children) {
                let limit = slot.as_ref().map_or(test.alternatives.len(), |(alt, _)| alt + 1);
                let Some(alt) = test.alternatives[..limit].iter().position(|p| p.matches(&name))
                else {
                    continue;
                };
                let better = match slot {
                    None => true,
                    Some((best_alt, best_name)) => alt < *best_alt || name < *best_name,
                };
                if better {
                    *slot = Some((alt, name.clone()));
                }
            }
        }
        let mut captures = Vec::new();
        for (i, (slot, test)) in best.into_iter().zip(&self.children).enumerate() {
            match (slot, test.test.negated) {
                (Some(_), true) | (None, false) => return None,
                (Some((_, name)), false) if test.test.capture.is_some() => captures.push((i, name)),
                _ => {}
            }
        }
        Some(captures)
    }
}

/// One clock for the whole assessment, so every age a verdict tests and every
/// age its sentence prints are read at the same instant.
struct Clock {
    now: SystemTime,
}

/// Highest confidence, then the later-resolved pack, then the earlier rule in
/// that pack (`docs/lang.md`); the trust cap is applied to the winner only, so
/// an untrusted pack's downgraded rule cannot lose to a weaker one and change
/// which rule a human is shown.
///
/// Every winner is an exclusive claim (`docs/lang.md`, "Ownership"). A claim
/// inside another pack's claim is dropped; inside its own pack's claim it is
/// kept, and the innermost claim owns the node. Then one suggestion per claim,
/// an outer one swallowing every one inside it — the directory is one
/// decision. So every suggestion lies inside a claim, and the arbitrary tree
/// gets none.
fn resolve(
    ctx: &Catalog,
    packs: &[TrustedPack],
    rules: &[Compiled],
    hits: Vec<Hit>,
    clock: &Clock,
) -> (Vec<(NodeId, Verdict)>, Ownership) {
    let mut best: HashMap<NodeId, Hit> = HashMap::new();
    // Every rule that lost the conflict on some node, as (node, rule).
    let mut outranked: Vec<(NodeId, usize)> = Vec::new();
    for hit in hits {
        match best.get(&hit.target) {
            Some(current) if !outranks(&rules[hit.rule], &rules[current.rule]) => {
                outranked.push((hit.target, hit.rule));
            }
            _ => {
                if let Some(old) = best.insert(hit.target, hit) {
                    outranked.push((old.target, old.rule));
                }
            }
        }
    }
    let mut winners: Vec<Hit> = best.into_values().collect();
    winners.sort_unstable_by_key(|hit| hit.target);

    let claim_of = |rule: usize| {
        let compiled = &rules[rule];
        let pack = &packs[compiled.pack].pack;
        Claim {
            class: format!("{}:{}", pack.name, compiled.rule.kind.value),
            provenance: Provenance::new(&pack.name, &compiled.rule.title.value),
            confidence: compiled.rule.confidence,
            exclusive: true,
        }
    };
    let mut dropped: Vec<DroppedClaim> = Vec::new();
    // In id order: (node, claim, enclosing claim); beside it each claim's pack
    // and the end of its id range.
    let mut claims: Vec<(NodeId, Claim, Option<usize>)> = Vec::new();
    let mut claim_pack: Vec<usize> = Vec::new();
    let mut claim_end: Vec<u32> = Vec::new();
    // The claims containing the current node, innermost last.
    let mut open: Vec<usize> = Vec::new();

    let mut out = Vec::new();
    let mut fence = 0;
    for hit in winners {
        while open.last().is_some_and(|&top| hit.target.0 >= claim_end[top]) {
            open.pop();
        }
        let compiled = &rules[hit.rule];
        // Every open claim is from one pack: a claim from another pack never
        // gets on the stack.
        if let Some(&owner) = open.last()
            && claim_pack[owner] != compiled.pack
        {
            dropped.push(DroppedClaim {
                path: ctx.path(hit.target).display().to_string(),
                claim: claim_of(hit.rule),
                reason: DropReason::Inside { owner: claims[owner].1.provenance.clone() },
            });
            continue;
        }
        let end = ctx.subtree(hit.target).end;
        claims.push((hit.target, claim_of(hit.rule), open.last().copied()));
        claim_pack.push(compiled.pack);
        claim_end.push(end);
        open.push(claims.len() - 1);

        if hit.target.0 < fence {
            continue;
        }
        fence = end;
        let rule = compiled.rule;
        let source = &packs[compiled.pack];
        let capped = source.trust.cap(rule.disposition);
        let anchor_name = node_name(ctx, hit.anchor);
        let mut vars: Vec<(&str, &str)> = vec![(rule.filter.var.value.as_str(), &anchor_name)];
        for (i, name) in &hit.captures {
            if let Some(var) = &compiled.children[*i].test.capture {
                vars.push((var.value.as_str(), name));
            }
        }
        let facts = NodeFacts { ctx, id: hit.target, now: clock.now };
        out.push((
            hit.target,
            Verdict {
                label: Label::new(&rule.kind.value.name),
                disposition: disposition_of(capped.disposition),
                confidence: rule.confidence,
                reason: render_reason(&rule.description.value, &facts, &vars),
                provenance: Provenance::new(&source.pack.name, &rule.title.value),
                capped: capped.explanation(&source.pack.name),
            },
        ));
    }

    // A loser on a node whose winner was itself dropped as nested is not
    // logged twice: the nesting already says why nothing there is owned.
    for (node, rule) in outranked {
        let Ok(ix) = claims.binary_search_by_key(&node, |(id, ..)| *id) else { continue };
        dropped.push(DroppedClaim {
            path: ctx.path(node).display().to_string(),
            claim: claim_of(rule),
            reason: DropReason::Outranked { by: claims[ix].1.provenance.clone() },
        });
    }
    dropped.sort_by(|a, b| {
        a.path.cmp(&b.path).then_with(|| a.claim.provenance.cmp(&b.claim.provenance))
    });
    (out, Ownership::new(ctx, claims, dropped))
}

/// On an equal confidence a later pack takes over and a later rule in the
/// same pack does not, which is why this cannot be one `>=`.
fn outranks(challenger: &Compiled, holder: &Compiled) -> bool {
    let (a, b) = (challenger.rule.confidence, holder.rule.confidence);
    a > b
        || (a == b
            && (challenger.pack > holder.pack
                || (challenger.pack == holder.pack && challenger.order < holder.order)))
}

fn disposition_of(disposition: AstDisposition) -> Disposition {
    match disposition {
        AstDisposition::Keep => Disposition::Keep,
        AstDisposition::Reclaimable => Disposition::Reclaimable,
        AstDisposition::Review => Disposition::Review,
    }
}

/// Case folding follows the platform, exactly as `name_eq` does: a glob is a
/// name comparison with holes in it, so `*.CSPROJ` has to match `app.csproj` on
/// NTFS for the same reason `Node_Modules` matches `node_modules` there.
///
/// A pattern that will not compile matches nothing. A rule that never fires is
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
    now: SystemTime,
}

impl NodeFacts<'_> {
    fn name(&self) -> String {
        node_name(self.ctx, self.id)
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
}

impl Facts for NodeFacts<'_> {
    fn field(&self, field: Field) -> Value {
        let node = self.ctx.node(self.id);
        match field {
            Field::Name => Value::Str(self.name()),
            // Absent on the wrong kind, which is how `$d.dir_name == target`
            // stays directory-only: an absent value makes every comparison
            // false.
            Field::DirName => match node.kind {
                EntryKind::Dir => Value::Str(self.name()),
                _ => Value::Absent,
            },
            Field::FileName => match node.kind {
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
            Field::ModifiedAge => self.age(node.modified),
            Field::AccessedAge => self.age(node.accessed),
            Field::MaxDescendantAge => self.age(node.max_modified),
            Field::HasAccessed => Value::Bool(node.accessed.is_some()),
        }
    }
}
