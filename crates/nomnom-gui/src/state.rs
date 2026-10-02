//! The view model that needs no window: which rows the tree shows, and which
//! entries a cleanup plan takes. Kept free of gpui types so the selection logic
//! that decides what gets trashed can be tested on its own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use humansize::{BINARY, format_size};
use nomnom_core::action::{ActionError, Plan, candidates, plan_from};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::verdict::{Assessment, Entry};

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
#[derive(Default)]
pub struct TreeModel {
    expanded: HashSet<NodeId>,
    sorted: HashMap<NodeId, Vec<NodeId>>,
    rows: Vec<TreeRow>,
}

impl TreeModel {
    /// The root's children, with the root itself expanded.
    pub fn new(catalog: &Catalog) -> Self {
        let mut model = Self::default();
        model.expanded.insert(catalog.root());
        model.rebuild(catalog);
        model
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

    /// Expand every ancestor of `id` and return its row.
    pub fn reveal(&mut self, catalog: &Catalog, id: NodeId) -> Option<usize> {
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
            if self.expanded.contains(&row.id) {
                let depth = row.depth + 1;
                let children = self.children(catalog, row.id).clone();
                stack.extend(children.into_iter().rev().map(|id| TreeRow { id, depth }));
            }
        }
    }

    fn children(&mut self, catalog: &Catalog, id: NodeId) -> &Vec<NodeId> {
        self.sorted.entry(id).or_insert_with(|| catalog.children_by_size(id))
    }
}

/// What the Clean screen will hand to `plan_from`: the include-review toggle
/// and the entries the user checked.
///
/// Opt-in: nothing is in the plan until the user checks it, matching
/// `nomnom clean`, which plans only the paths named on its command line.
/// Include-review only widens what can be checked; it never checks anything.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub include_review: bool,
    checked: HashSet<PathBuf>,
}

/// The dry run the Clean screen previews and Apply carries out.
pub struct Preview {
    pub plan: Plan,
    /// Paths the plan guards refused, with why.
    pub refused: Vec<(PathBuf, ActionError)>,
}

impl Selection {
    pub fn is_checked(&self, path: &str) -> bool {
        self.checked.contains(Path::new(path))
    }

    pub fn set_checked(&mut self, path: &str, checked: bool) {
        if checked {
            self.checked.insert(PathBuf::from(path));
        } else {
            self.checked.remove(Path::new(path));
        }
    }

    /// Uncheck everything, keeping the include-review toggle. Called when a new
    /// assessment lands and after an apply: a choice made against other
    /// entries is not a choice about these.
    pub fn clear(&mut self) {
        self.checked.clear();
    }

    /// How many of the current candidates the user checked.
    pub fn checked_count(&self, assessment: &Assessment) -> usize {
        self.candidates(assessment).iter().filter(|entry| self.is_checked(&entry.path)).count()
    }

    /// Every entry the dispositions allow the user to check, biggest group
    /// first.
    pub fn candidates<'a>(&self, assessment: &'a Assessment) -> Vec<&'a Entry> {
        candidates(assessment, self.include_review)
    }

    pub fn preview(&self, assessment: &Assessment) -> Result<Preview, ActionError> {
        let (plan, refused) = plan_from(assessment, &self.checked, self.include_review)?;
        Ok(Preview { plan, refused })
    }
}

#[cfg(test)]
mod tests {
    use nomnom_core::verdict::{Disposition, Group, Label, Provenance, Verdict};

    use super::*;

    fn entry(path: &Path, disposition: Disposition) -> Entry {
        std::fs::create_dir_all(path).unwrap();
        Entry {
            path: path.display().to_string(),
            bytes: 1,
            verdict: Verdict {
                label: Label::CACHE,
                disposition,
                confidence: 1.0,
                reason: "test".into(),
                unit: true,
                provenance: Provenance::new("p", "r"),
                capped: None,
            },
        }
    }

    fn assessment(root: &Path, entries: Vec<Entry>) -> Assessment {
        Assessment {
            root: root.to_path_buf(),
            groups: vec![Group { label: Label::CACHE, bytes: entries.len() as u64, entries }],
            reclaimable_bytes: 0,
        }
    }

    fn planned(selection: &Selection, assessment: &Assessment) -> Vec<String> {
        let mut names: Vec<String> = selection
            .preview(assessment)
            .unwrap()
            .plan
            .actions()
            .iter()
            .map(|e| e.action.path().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // Catches "Files to delete" filling itself with every reclaimable verdict
    // before the user checks anything, or again once a new assessment lands —
    // the opt-out selection that put unpicked paths behind Reclaim.
    #[test]
    fn fresh_and_reassessed_selections_plan_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = entry(&root.join("a"), Disposition::Reclaimable);
        let a_path = a.path.clone();
        let assessment =
            assessment(&root, vec![a, entry(&root.join("b"), Disposition::Reclaimable)]);

        let mut selection = Selection::default();
        let plan = selection.preview(&assessment).unwrap().plan;
        assert!(plan.is_empty());
        assert_eq!(plan.total_bytes(), 0);
        assert_eq!(selection.checked_count(&assessment), 0);

        selection.set_checked(&a_path, true);
        selection.include_review = true;
        // What the Clean screen does when `Assessed` fires.
        selection.clear();
        let plan = selection.preview(&assessment).unwrap().plan;
        assert!(plan.is_empty());
        assert_eq!(plan.total_bytes(), 0);
        assert!(selection.include_review, "clearing must keep the toggle");
    }

    // Catches a checked box not reaching the plan, or an unchecked one still
    // trashing its path on Apply.
    #[test]
    fn only_checked_entries_reach_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = entry(&root.join("a"), Disposition::Reclaimable);
        let b = entry(&root.join("b"), Disposition::Reclaimable);
        let (a_path, b_path) = (a.path.clone(), b.path.clone());
        let assessment = assessment(&root, vec![a, b]);

        let mut selection = Selection::default();
        selection.set_checked(&a_path, true);
        assert_eq!(planned(&selection, &assessment), ["a"]);
        assert_eq!(selection.checked_count(&assessment), 1);

        selection.set_checked(&b_path, true);
        assert_eq!(planned(&selection, &assessment), ["a", "b"]);

        selection.set_checked(&a_path, false);
        assert_eq!(planned(&selection, &assessment), ["b"]);
    }

    // Catches "include review" checking review entries on its own, and a
    // checked `review` entry reaching the plan with the toggle off.
    #[test]
    fn include_review_widens_candidates_but_checks_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let maybe = entry(&root.join("maybe"), Disposition::Review);
        let maybe_path = maybe.path.clone();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("gone"), Disposition::Reclaimable),
                maybe,
                entry(&root.join("kept"), Disposition::Keep),
            ],
        );

        let mut selection = Selection::default();
        assert_eq!(selection.candidates(&assessment).len(), 1);

        selection.include_review = true;
        assert_eq!(selection.candidates(&assessment).len(), 2);
        assert!(planned(&selection, &assessment).is_empty());

        selection.set_checked(&maybe_path, true);
        assert_eq!(planned(&selection, &assessment), ["maybe"]);

        selection.include_review = false;
        assert!(planned(&selection, &assessment).is_empty());
        assert_eq!(selection.checked_count(&assessment), 0);
    }
}
