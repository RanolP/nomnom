//! The view model that needs no window: which rows the tree shows, and which
//! entries a cleanup plan takes. Kept free of gpui types so the selection logic
//! that decides what gets deleted can be tested on its own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use humansize::{BINARY, format_size};
use nomnom_core::action::{
    ActionError, Approval, ExclusionError, Exclusions, Plan, RuleGroup, approved, by_rule,
    candidates, plan_from,
};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::scan::EntryKind;
use nomnom_core::verdict::{Assessment, Claim, DropReason, Ownership, Provenance};

pub fn size(bytes: u64) -> String {
    format_size(bytes, BINARY)
}

pub fn modified(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time).format("%Y-%m-%d %H:%M").to_string()
}

/// `4570123` as `4,570,123`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, c) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One visible row of the scan tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRow {
    pub id: NodeId,
    pub depth: usize,
}

/// The scan tree, flattened to the rows currently visible.
///
/// Children are sorted (by `children_by_size`) only when their parent is first
/// expanded, so a volume-sized catalog never pays for sorting subtrees nobody
/// opened.
///
/// Built with [`other_files`](Self::other_files) it is the "Other files"
/// tree, `nomnom scan --view other`: children sorted by the bytes no pack
/// recognizes, and each claim root a leaf that stands in for its subtree.
#[derive(Default)]
pub struct TreeModel {
    expanded: HashSet<NodeId>,
    sorted: HashMap<NodeId, Vec<NodeId>>,
    rows: Vec<TreeRow>,
    other: Option<Arc<Assessment>>,
}

impl TreeModel {
    /// The root's children, with the root itself expanded.
    pub fn new(catalog: &Catalog) -> Self {
        let mut model = Self::default();
        model.expanded.insert(catalog.root());
        model.rebuild(catalog);
        model
    }

    /// The Other files tree of `assessment`'s ownership.
    pub fn other_files(catalog: &Catalog, assessment: Arc<Assessment>) -> Self {
        let mut model = Self { other: Some(assessment), ..Self::default() };
        model.expanded.insert(catalog.root());
        model.rebuild(catalog);
        model
    }

    /// The ownership this tree is the Other files view of.
    pub fn ownership(&self) -> Option<&Ownership> {
        self.other.as_deref().map(|assessment| &assessment.ownership)
    }

    /// The claim `id`'s row stands in for, in the Other files tree.
    pub fn link(&self, id: NodeId) -> Option<&Claim> {
        self.ownership()?.claim_at(id)
    }

    /// The bytes this tree sizes `id` by.
    pub fn bytes(&self, catalog: &Catalog, id: NodeId) -> u64 {
        match self.ownership() {
            Some(ownership) => ownership.arbitrary(catalog, id),
            None => catalog.node(id).subtree_size,
        }
    }

    /// Whether `id`'s row can open: a non-empty folder, and in the Other files
    /// tree not a claim root.
    pub fn expandable(&self, catalog: &Catalog, id: NodeId) -> bool {
        catalog.node(id).kind == EntryKind::Dir
            && !catalog.children(id).is_empty()
            && self.link(id).is_none()
    }

    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    pub fn is_expanded(&self, id: NodeId) -> bool {
        self.expanded.contains(&id)
    }

    pub fn toggle(&mut self, catalog: &Catalog, id: NodeId) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild(catalog);
    }

    /// Expand every ancestor of `id` and return its row. In the Other files
    /// tree a recognized node has no row of its own, so the row is the
    /// outermost claim root holding it.
    pub fn reveal(&mut self, catalog: &Catalog, id: NodeId) -> Option<usize> {
        let mut id = id;
        if let Some(ownership) = self.ownership() {
            let mut cursor = Some(id);
            while let Some(node) = cursor {
                if ownership.claim_at(node).is_some() {
                    id = node;
                }
                cursor = catalog.node(node).parent;
            }
        }
        let mut cursor = catalog.node(id).parent;
        let mut opened = false;
        while let Some(ancestor) = cursor {
            opened |= self.expanded.insert(ancestor);
            cursor = catalog.node(ancestor).parent;
        }
        if opened {
            self.rebuild(catalog);
        }
        self.rows.iter().position(|row| row.id == id)
    }

    fn rebuild(&mut self, catalog: &Catalog) {
        self.rows.clear();
        let root = catalog.root();
        // Depth-first with an explicit stack: catalog depth is unbounded.
        let mut stack: Vec<TreeRow> =
            self.children(catalog, root).iter().rev().map(|&id| TreeRow { id, depth: 0 }).collect();
        while let Some(row) = stack.pop() {
            self.rows.push(row);
            if self.expanded.contains(&row.id) && self.link(row.id).is_none() {
                let depth = row.depth + 1;
                let children = self.children(catalog, row.id).clone();
                stack.extend(children.into_iter().rev().map(|id| TreeRow { id, depth }));
            }
        }
    }

    fn children(&mut self, catalog: &Catalog, id: NodeId) -> &Vec<NodeId> {
        let ownership = self.other.as_deref().map(|assessment| &assessment.ownership);
        self.sorted.entry(id).or_insert_with(|| match ownership {
            Some(ownership) => ownership.children_by_arbitrary(catalog, id),
            None => catalog.children_by_size(id),
        })
    }
}

/// One row of the Recognized view, `nomnom classify`'s listing flattened.
#[derive(Debug, Clone, PartialEq)]
pub enum RecognizedRow {
    Pack { name: String, bytes: u64, claims: usize },
    Claim { id: NodeId, path: String, class: String, rule: String, bytes: u64, nested: bool },
    DroppedHeader { count: usize },
    Dropped { path: String, provenance: String, why: String },
}

/// Pack › claim, biggest first, then the dropped claims: the same listing
/// `nomnom classify` prints.
pub fn recognized_rows(catalog: &Catalog, ownership: &Ownership) -> Vec<RecognizedRow> {
    let mut rows = Vec::new();
    for pack in ownership.recognized(catalog) {
        rows.push(RecognizedRow::Pack {
            name: pack.pack,
            bytes: pack.bytes,
            claims: pack.claims.len(),
        });
        rows.extend(pack.claims.into_iter().map(|row| RecognizedRow::Claim {
            id: row.id,
            path: row.path,
            class: row.claim.class,
            rule: row.claim.provenance.rule,
            bytes: row.bytes,
            nested: row.nested,
        }));
    }
    let dropped = ownership.dropped();
    if !dropped.is_empty() {
        rows.push(RecognizedRow::DroppedHeader { count: dropped.len() });
        rows.extend(dropped.iter().map(|claim| RecognizedRow::Dropped {
            path: claim.path.clone(),
            provenance: claim.claim.provenance.to_string(),
            why: match &claim.reason {
                DropReason::Outranked { by } => format!("outranked by {by}"),
                DropReason::Contested { with } => format!("contested with {with}; neither kept"),
                DropReason::Inside { owner } => format!("inside {owner}'s exclusive claim"),
            },
        }));
    }
    rows
}

/// What the Clean screen will hand to `plan_from`: the include-review toggle,
/// the rules the user approved, and the drive's persisted exclusions.
///
/// Opt-in: nothing is in the plan until the user approves a rule, matching
/// `nomnom clean --rule`. Approvals belong to one assessment and clear with
/// it; exclusions belong to the drive and are reloaded from
/// `.nomnom/exclusions.toml`, so they survive rescans and restarts. An
/// exclusion only ever removes a path from the plan. Include-review only
/// widens what can be approved; it never approves anything.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub include_review: bool,
    approval: Approval,
    exclusions: Exclusions,
    /// The drive the exclusions were loaded from and are saved to.
    root: Option<PathBuf>,
}

/// The dry run the Clean screen previews and Apply carries out.
pub struct Preview {
    pub plan: Plan,
    /// Paths the plan guards refused, with why.
    pub refused: Vec<(PathBuf, ActionError)>,
}

impl Selection {
    /// What a new assessment of the drive at `root` does: every approval is
    /// dropped, since a rule approved against other matches is not an
    /// approval of these, and the drive's exclusions are read back.
    pub fn reassessed(&mut self, root: &Path) -> Result<(), ExclusionError> {
        self.approval = Approval::default();
        self.root = Some(root.to_path_buf());
        self.exclusions = Exclusions::default();
        self.exclusions = Exclusions::load(root)?;
        Ok(())
    }

    /// Drop every approval, keeping the toggle and the exclusions. Called
    /// after an apply: what was approved is applied.
    pub fn clear(&mut self) {
        self.approval = Approval::default();
    }

    pub fn is_approved(&self, rule: &Provenance) -> bool {
        self.approval.rules.contains(rule)
    }

    pub fn set_approved(&mut self, rule: &Provenance, approved: bool) {
        if approved {
            self.approval.rules.insert(rule.clone());
        } else {
            self.approval.rules.remove(rule);
        }
    }

    pub fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    /// The exclusion keeping `path` out of plans, itself or an ancestor.
    pub fn excluded_by(&self, path: &str) -> Option<&Path> {
        self.exclusions.covering(Path::new(path))
    }

    /// Adds `path` to, or removes it from, the drive's exclusion list and
    /// saves it. Memory changes only once the file is written, so what the
    /// screen shows is what the next session loads.
    pub fn set_excluded(&mut self, path: &Path, excluded: bool) -> Result<(), ExclusionError> {
        let Some(root) = self.root.clone() else { return Ok(()) };
        let mut next = self.exclusions.clone();
        if excluded {
            next.add(&root, path)?;
        } else {
            next.remove(path);
        }
        next.save(&root)?;
        self.exclusions = next;
        Ok(())
    }

    /// How many paths the approvals put in the plan, after exclusions.
    pub fn planned_count(&self, assessment: &Assessment) -> usize {
        approved(assessment, &self.approval, &self.exclusions, self.include_review).len()
    }

    /// The candidates grouped by rule, biggest rule first.
    pub fn groups<'a>(&self, assessment: &'a Assessment) -> Vec<RuleGroup<'a>> {
        by_rule(&candidates(assessment, self.include_review))
    }

    pub fn preview(&self, assessment: &Assessment) -> Result<Preview, ActionError> {
        let (plan, refused) =
            plan_from(assessment, &self.approval, &self.exclusions, self.include_review)?;
        Ok(Preview { plan, refused })
    }
}

#[cfg(test)]
mod tests {
    use nomnom_core::verdict::{Disposition, Entry, Group, Label, Verdict};

    use super::*;

    fn entry(path: &Path, disposition: Disposition, rule: &str) -> Entry {
        std::fs::create_dir_all(path).unwrap();
        Entry {
            path: path.display().to_string(),
            bytes: 1,
            verdict: Verdict {
                label: Label::CACHE,
                disposition,
                reason: "test".into(),
                provenance: Provenance::new("p", rule),
                capped: None,
            },
            reach: None,
        }
    }

    fn assessment(root: &Path, entries: Vec<Entry>) -> Assessment {
        Assessment {
            root: root.to_path_buf(),
            groups: vec![Group { label: Label::CACHE, bytes: entries.len() as u64, entries }],
            reclaimable_bytes: 0,
            shared: Vec::new(),
            ownership: Default::default(),
        }
    }

    fn planned(selection: &Selection, assessment: &Assessment) -> Vec<String> {
        let mut names: Vec<String> = selection
            .preview(assessment)
            .unwrap()
            .plan
            .actions()
            .iter()
            .map(|e| {
                let path = e.action.path();
                let parent = path.parent().unwrap().file_name().unwrap().to_string_lossy();
                format!("{parent}/{}", path.file_name().unwrap().to_string_lossy())
            })
            .collect();
        names.sort();
        names
    }

    fn rule(name: &str) -> Provenance {
        Provenance::new("p", name)
    }

    /// Two Cargo-like `target/` matches of one rule, and a cache of another.
    fn fixture(root: &Path) -> Assessment {
        assessment(
            root,
            vec![
                entry(&root.join("alpha").join("target"), Disposition::Reclaimable, "target"),
                entry(&root.join("beta").join("target"), Disposition::Reclaimable, "target"),
                entry(&root.join("web").join("cache"), Disposition::Reclaimable, "cache"),
            ],
        )
    }

    // Catches "Files to delete" filling itself before the user approves any
    // rule — the opt-out selection that put unpicked paths behind Reclaim.
    #[test]
    fn a_fresh_selection_plans_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = fixture(&root);
        let mut selection = Selection::default();
        selection.reassessed(&root).unwrap();
        let plan = selection.preview(&assessment).unwrap().plan;
        assert!(plan.is_empty());
        assert_eq!(selection.planned_count(&assessment), 0);
    }

    // Catches an approved rule not reaching the plan, pulling in another
    // rule's matches, or an excluded match still being deleted on Apply.
    #[test]
    fn an_approved_rule_plans_its_matches_minus_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = fixture(&root);
        let mut selection = Selection::default();
        selection.reassessed(&root).unwrap();

        selection.set_approved(&rule("target"), true);
        assert_eq!(planned(&selection, &assessment), ["alpha/target", "beta/target"]);

        selection.set_excluded(&root.join("beta").join("target"), true).unwrap();
        assert_eq!(planned(&selection, &assessment), ["alpha/target"]);
        assert_eq!(selection.planned_count(&assessment), 1);

        selection.set_excluded(&root.join("beta").join("target"), false).unwrap();
        assert_eq!(planned(&selection, &assessment), ["alpha/target", "beta/target"]);

        selection.set_approved(&rule("target"), false);
        assert!(planned(&selection, &assessment).is_empty());
    }

    // Catches a rescan carrying an approval over to matches the user never
    // saw, and a rescan (or restart) forgetting an exclusion so the user's
    // active `target/` is back on the next plan.
    #[test]
    fn a_new_assessment_clears_approvals_but_keeps_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = fixture(&root);
        let mut selection = Selection::default();
        selection.reassessed(&root).unwrap();
        selection.include_review = true;
        selection.set_approved(&rule("target"), true);
        selection.set_excluded(&root.join("beta"), true).unwrap();

        // What the Clean screen does when `Assessed` fires.
        selection.reassessed(&root).unwrap();
        assert!(!selection.is_approved(&rule("target")));
        assert!(planned(&selection, &assessment).is_empty());
        assert!(selection.include_review, "a new assessment must keep the toggle");
        assert!(
            selection
                .excluded_by(&root.join("beta").join("target").display().to_string())
                .is_some()
        );

        // A fresh session reads the same list off disk.
        let mut restarted = Selection::default();
        restarted.reassessed(&root).unwrap();
        restarted.set_approved(&rule("target"), true);
        assert_eq!(planned(&restarted, &assessment), ["alpha/target"]);
    }

    // Catches the Other files tree opening a recognized folder (its files are
    // not "other"), sizing rows by full bytes, or a reveal of a recognized
    // file landing on no row.
    #[test]
    fn the_other_files_tree_stops_at_claim_roots() {
        use nomnom_core::verdict::{TrustedPack, assess};

        use crate::scan_fixtures::{catalog_of, write};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root.join("package.json"), br#"{"name":"fixture"}"#);
        write(root.join("src/index.js"), b"console.log('hi');\n");
        write(root.join("node_modules/.package-lock.json"), br#"{"name":"fixture"}"#);
        write(root.join("node_modules/left-pad/index.js"), b"module.exports = 1;\n");
        write(root.join("node_modules/left-pad/package.json"), br#"{"name":"left-pad"}"#);
        let catalog = catalog_of(root);
        let assessment = Arc::new(assess(&catalog, TrustedPack::builtins()));
        let modules = catalog.find(&root.join("node_modules")).unwrap();
        assert!(assessment.ownership.claim_at(modules).is_some(), "node_modules unclaimed");

        let mut tree = TreeModel::other_files(&catalog, assessment);
        assert!(!tree.expandable(&catalog, modules));
        tree.toggle(&catalog, modules);
        assert!(tree.rows().iter().all(|row| catalog.node(row.id).parent == Some(catalog.root())));
        assert_eq!(tree.bytes(&catalog, modules), 0);
        let src = catalog.find(&root.join("src")).unwrap();
        assert_eq!(tree.bytes(&catalog, src), catalog.node(src).subtree_size);

        let inside = catalog.find(&root.join("node_modules/left-pad/index.js")).unwrap();
        let row = tree.reveal(&catalog, inside).expect("a recognized file reveals its claim");
        assert_eq!(tree.rows()[row].id, modules);
    }

    // Catches "include review" approving review entries on its own, and an
    // approved rule's `review` match reaching the plan with the toggle off.
    #[test]
    fn include_review_widens_candidates_but_approves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("a").join("gone"), Disposition::Reclaimable, "r"),
                entry(&root.join("a").join("maybe"), Disposition::Review, "m"),
                entry(&root.join("a").join("kept"), Disposition::Keep, "k"),
            ],
        );

        let mut selection = Selection::default();
        selection.reassessed(&root).unwrap();
        assert_eq!(selection.groups(&assessment).len(), 1);

        selection.include_review = true;
        assert_eq!(selection.groups(&assessment).len(), 2);
        assert!(planned(&selection, &assessment).is_empty());

        selection.set_approved(&rule("m"), true);
        assert_eq!(planned(&selection, &assessment), ["a/maybe"]);

        selection.include_review = false;
        assert!(planned(&selection, &assessment).is_empty());
        assert_eq!(selection.planned_count(&assessment), 0);
    }
}
