//! The view model that needs no window: which rows the tree shows, and which
//! entries a cleanup plan takes. Kept free of gpui types so the selection logic
//! that decides what gets trashed can be tested on its own.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use humansize::{BINARY, format_size};
use nomnom_core::action::{ActionError, Plan, plan_from};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::verdict::{Assessment, Disposition, Entry};

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
/// and the entries the user unchecked.
///
/// Unchecked entries are remembered rather than checked ones, so every
/// candidate starts checked — matching `nomnom clean`, which plans them all —
/// and toggling include-review on brings the review entries in already checked.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub include_review: bool,
    unchecked: HashSet<String>,
}

/// The dry run the Clean screen previews and Apply carries out.
pub struct Preview {
    pub plan: Plan,
    /// Paths the plan guards refused, with why.
    pub refused: Vec<(PathBuf, ActionError)>,
}

impl Selection {
    pub fn is_checked(&self, path: &str) -> bool {
        !self.unchecked.contains(path)
    }

    pub fn set_checked(&mut self, path: &str, checked: bool) {
        if checked {
            self.unchecked.remove(path);
        } else {
            self.unchecked.insert(path.to_string());
        }
    }

    /// Start over for a new assessment, keeping the include-review toggle.
    pub fn recheck_all(&mut self) {
        self.unchecked.clear();
    }

    /// Every entry the dispositions allow onto a plan, biggest group first.
    pub fn candidates<'a>(&self, assessment: &'a Assessment) -> Vec<&'a Entry> {
        assessment
            .groups
            .iter()
            .flat_map(|group| &group.entries)
            .filter(|entry| match entry.verdict.disposition {
                Disposition::Reclaimable => true,
                Disposition::Review => self.include_review,
                Disposition::Keep => false,
            })
            .collect()
    }

    pub fn preview(&self, assessment: &Assessment) -> Result<Preview, ActionError> {
        let chosen: HashSet<PathBuf> = self
            .candidates(assessment)
            .into_iter()
            .filter(|entry| self.is_checked(&entry.path))
            .map(|entry| PathBuf::from(&entry.path))
            .collect();
        let (plan, refused) = plan_from(assessment, Some(&chosen), self.include_review)?;
        Ok(Preview { plan, refused })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nomnom_core::verdict::{Group, Label, Provenance, Verdict};

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

    // Catches an unchecked box still trashing its path on Apply.
    #[test]
    fn unchecking_an_entry_removes_its_action_from_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = entry(&root.join("a"), Disposition::Reclaimable);
        let b = entry(&root.join("b"), Disposition::Reclaimable);
        let a_path = a.path.clone();
        let assessment = assessment(&root, vec![a, b]);

        let mut selection = Selection::default();
        assert_eq!(planned(&selection, &assessment), ["a", "b"]);

        selection.set_checked(&a_path, false);
        assert_eq!(planned(&selection, &assessment), ["b"]);

        selection.set_checked(&a_path, true);
        assert_eq!(planned(&selection, &assessment), ["a", "b"]);
    }

    // Catches a `review` verdict reaching the plan without the user turning on
    // "include review" — the GUI equivalent of `--include-review`.
    #[test]
    fn review_entries_stay_out_unless_include_review_is_on() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("gone"), Disposition::Reclaimable),
                entry(&root.join("maybe"), Disposition::Review),
                entry(&root.join("kept"), Disposition::Keep),
            ],
        );

        let mut selection = Selection::default();
        assert_eq!(selection.candidates(&assessment).len(), 1);
        assert_eq!(planned(&selection, &assessment), ["gone"]);

        selection.include_review = true;
        assert_eq!(planned(&selection, &assessment), ["gone", "maybe"]);
    }
}
