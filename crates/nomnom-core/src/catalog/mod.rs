//! The tree model with rolled-up aggregates.
//!
//! Nodes live in one `Vec` arena addressed by [`NodeId`] rather than behind
//! `Rc`/`RefCell`: a volume scan is millions of nodes, and a flat arena keeps
//! them contiguous, makes every reference a `Copy` `u32`, and lets traversals
//! stay iterative — which is what keeps a 2000-deep chain from blowing the
//! stack where a recursive `Rc` tree would.
//!
//! Ids are handed out in preorder, so a node's whole subtree is the id range
//! `id..end` ([`Catalog::subtree`]). That one fact carries the roll-up (a single
//! reverse pass sees every child before its parent), subtree containment for a
//! cleanup plan (two integer compares), and [`Catalog::descendants`].
//!
//! Nodes store no paths. A path is built on demand from the names, so a volume
//! of millions of entries does not pin millions of full paths in memory.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::scan::table::{Blob, EXTRA_LINK, ODD_NAME, ScanTable};
use crate::scan::{BackendUsed, EntryKind, ScanError, ScanReport};
use crate::timings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

impl NodeId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub kind: EntryKind,
    /// Own size; 0 for directories. A hard-linked file shows its full size on
    /// every name it has.
    pub size: u64,
    /// Own on-disk size, as [`Entry::allocated`](crate::scan::Entry::allocated)
    /// reported it: `None` when the backend had no cheap answer.
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// Rolled up over the subtree, inclusive of self. A file's bytes count
    /// once, under its primary name: an [`extra_link`](Self::extra_link) node
    /// adds nothing to its ancestors.
    pub subtree_size: u64,
    /// `allocated` rolled up the same way, a missing value counting as 0.
    pub subtree_allocated: u64,
    /// Files in the subtree, inclusive of self if this is a file.
    pub file_count: u64,
    /// Directories in the subtree, excluding self.
    pub dir_count: u64,
    /// Newest `modified` anywhere in the subtree, inclusive of self.
    ///
    /// A directory's own mtime reflects only its entry list: it changes when a
    /// file is added or removed and says nothing about whether the files inside
    /// are in active use. This is the aggregate a staleness rule actually
    /// means.
    pub max_modified: Option<SystemTime>,
    pub depth: u32,
    /// How many names the scan found for this node's file, itself included: 1
    /// for anything that is not hard-linked.
    pub links: u32,
    /// A second (or later) name of a hard-linked file, whose bytes are
    /// counted under another node.
    pub extra_link: bool,
    end: u32,
    name: NameRef,
    blob: u32,
}

impl Node {
    /// What this node adds to its parent's `subtree_size`.
    pub fn rolled_size(&self) -> u64 {
        if self.extra_link { self.subtree_size - self.size } else { self.subtree_size }
    }
}

#[derive(Debug, Clone, Copy)]
struct NameRef {
    off: u32,
    len: u32,
    odd: bool,
}

/// The names one hard-linked file has in the catalog.
#[derive(Debug, Clone)]
pub struct LinkGroup {
    /// The node its bytes are counted under.
    pub primary: NodeId,
    /// Every name reachable from the root, `primary` included, in id order.
    pub nodes: Vec<NodeId>,
    /// False when the scan found names that are not in the catalog (filed
    /// under an unreachable parent, or outside the scanned subtree): trashing
    /// every node here still leaves the file alive.
    pub complete: bool,
    pub bytes: u64,
}

pub struct Catalog {
    nodes: Vec<Node>,
    root: NodeId,
    root_path: PathBuf,
    names: String,
    odd_names: Vec<OsString>,
    /// Children of node `i` are `child_ids[child_start[i]..child_start[i + 1]]`.
    child_start: Vec<u32>,
    child_ids: Vec<NodeId>,
    /// Multi-link files: names of group `g` are
    /// `link_ids[link_start[g]..link_start[g + 1]]`.
    link_groups: Vec<(NodeId, bool, u64)>,
    link_start: Vec<u32>,
    link_ids: Vec<NodeId>,
    errors: Vec<ScanError>,
    backend_used: BackendUsed,
}

/// Counts, prefix sums and scatters `(parent, child)` pairs into a compressed
/// child list: O(n), no hashing, no sorting. Children keep the order the pairs
/// arrive in.
fn csr(n: usize, pairs: impl Iterator<Item = (u32, u32)> + Clone) -> (Vec<u32>, Vec<u32>) {
    let mut start = vec![0u32; n + 1];
    for (parent, _) in pairs.clone() {
        start[parent as usize + 1] += 1;
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut ids = vec![0u32; start[n] as usize];
    for (parent, child) in pairs {
        let slot = &mut fill[parent as usize];
        ids[*slot as usize] = child;
        *slot += 1;
    }
    (start, ids)
}

impl Catalog {
    /// Build the tree from a scan.
    ///
    /// Row order is not assumed: the MFT backend emits rows in record order,
    /// where a child routinely comes before its parent. The rows are linked by
    /// a compressed child list, then renumbered in preorder by one walk down
    /// from the root. A row that walk never reaches — an orphan, or a cycle a
    /// damaged table can hold — is dropped and counted in one error, so no
    /// visited set and no cycle check is needed: every row has exactly one
    /// parent, so the walk from the root can meet each row once at most.
    pub fn build(report: ScanReport) -> Self {
        let ScanReport { root: root_path, table, mut errors, backend_used } = report;
        let ScanTable { nodes: rows, names, odd_names, blobs } = table;
        let started = Instant::now();

        let n = rows.len();
        if n == 0 {
            return Self::build(ScanReport {
                root: root_path,
                table: ScanTable::new(),
                errors,
                backend_used,
            });
        }

        // Children in table space. Row 0 is the root and never anyone's child;
        // a row naming itself or an index past the table has no parent here.
        let pairs = rows
            .iter()
            .enumerate()
            .skip(1)
            .filter(|&(i, row)| (row.parent as usize) < n && row.parent as usize != i)
            .map(|(i, row)| (row.parent, i as u32));
        let (start, kids) = csr(n, pairs);

        // Preorder: `order[new] = row`. Children are pushed reversed so the
        // first child is visited first and siblings keep table order.
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut parent_of: Vec<u32> = Vec::with_capacity(n);
        let mut depth_of: Vec<u32> = Vec::with_capacity(n);
        let mut stack: Vec<(u32, u32, u32)> = vec![(0, 0, 0)];
        while let Some((row, parent, depth)) = stack.pop() {
            let new = order.len() as u32;
            order.push(row);
            parent_of.push(parent);
            depth_of.push(depth);
            let range = start[row as usize] as usize..start[row as usize + 1] as usize;
            stack.extend(kids[range].iter().rev().map(|&child| (child, new, depth + 1)));
        }
        drop(kids);
        drop(start);
        let reached = order.len();

        // A blob's bytes count under its first name not flagged as an extra
        // link, in preorder, or under its first reachable name when every
        // name is flagged.
        let mut links = vec![0u32; blobs.len()];
        for row in &rows {
            if let Some(count) = links.get_mut(row.blob as usize) {
                *count += 1;
            }
        }
        let mut primary = vec![u32::MAX; blobs.len()];
        for pass_extra in [false, true] {
            for (new, &row) in order.iter().enumerate() {
                let r = &rows[row as usize];
                let b = r.blob as usize;
                if b < blobs.len()
                    && primary[b] == u32::MAX
                    && (pass_extra || r.flags & EXTRA_LINK == 0)
                {
                    primary[b] = new as u32;
                }
            }
        }

        let empty = Blob { size: 0, allocated: None, modified: None, accessed: None };
        let mut nodes: Vec<Node> = order
            .iter()
            .enumerate()
            .map(|(new, &row)| {
                let r = &rows[row as usize];
                let b = r.blob;
                let facts = blobs.get(b as usize).copied().unwrap_or(empty);
                let size = if r.kind == EntryKind::Dir { 0 } else { facts.size };
                let extra_link = primary.get(b as usize).is_some_and(|&p| p != new as u32);
                Node {
                    id: NodeId(new as u32),
                    parent: (new != 0).then_some(NodeId(parent_of[new])),
                    kind: r.kind,
                    size,
                    allocated: facts.allocated,
                    modified: facts.modified,
                    accessed: facts.accessed,
                    subtree_size: size,
                    subtree_allocated: facts.allocated.unwrap_or(0),
                    file_count: u64::from(r.kind == EntryKind::File),
                    dir_count: 0,
                    max_modified: facts.modified,
                    depth: depth_of[new],
                    links: links.get(b as usize).copied().unwrap_or(1),
                    extra_link,
                    end: new as u32 + 1,
                    name: NameRef {
                        off: r.name_off,
                        len: r.name_len,
                        odd: r.flags & ODD_NAME != 0,
                    },
                    blob: b,
                }
            })
            .collect();
        drop(depth_of);
        drop(rows);
        let started = timings::lap("Catalog::build link", started);

        // One reverse pass: every child has a higher id than its parent.
        for new in (1..nodes.len()).rev() {
            let child = &nodes[new];
            let (size, allocated) = if child.extra_link {
                (
                    child.subtree_size - child.size,
                    child.subtree_allocated - child.allocated.unwrap_or(0),
                )
            } else {
                (child.subtree_size, child.subtree_allocated)
            };
            let (files, dirs, newest, end) = (
                child.file_count,
                child.dir_count + u64::from(child.kind == EntryKind::Dir),
                child.max_modified,
                child.end,
            );
            let parent = &mut nodes[parent_of[new] as usize];
            parent.subtree_size += size;
            parent.subtree_allocated += allocated;
            parent.file_count += files;
            parent.dir_count += dirs;
            // `None` sorts below every `Some`, so this is "the newest known
            // stamp, or nothing if nothing in the subtree has one".
            parent.max_modified = parent.max_modified.max(newest);
            parent.end = parent.end.max(end);
        }

        let m = nodes.len();
        let (child_start, child_ids) =
            csr(m, (1..m as u32).map(|new| (parent_of[new as usize], new)));
        let child_ids = child_ids.into_iter().map(NodeId).collect();

        // Names of every multi-link file, grouped in id order.
        let mut group_of = vec![u32::MAX; blobs.len()];
        let mut link_groups = Vec::new();
        for node in &nodes {
            let b = node.blob as usize;
            if node.links > 1 && b < blobs.len() && group_of[b] == u32::MAX {
                group_of[b] = link_groups.len() as u32;
                link_groups.push((NodeId(primary[b]), false, blobs[b].size));
            }
        }
        let pairs = nodes.iter().filter_map(|node| {
            let g = *group_of.get(node.blob as usize)?;
            (g != u32::MAX).then_some((g, node.id.0))
        });
        let (link_start, link_ids) = csr(link_groups.len(), pairs);
        for (g, group) in link_groups.iter_mut().enumerate() {
            let found = link_start[g + 1] - link_start[g];
            group.1 = found == nodes[group.0.index()].links;
        }
        let link_ids = link_ids.into_iter().map(NodeId).collect();

        let unreachable = n - reached;
        if unreachable > 0 {
            errors.push(ScanError {
                path: None,
                message: format!(
                    "{unreachable} entries are not reachable from the root (orphaned or cyclic \
                     parent reference); their bytes are not counted"
                ),
            });
        }
        timings::lap("Catalog::build roll-up", started);

        Self {
            nodes,
            root: NodeId(0),
            root_path,
            names,
            odd_names,
            child_start,
            child_ids,
            link_groups,
            link_start,
            link_ids,
            errors,
            backend_used,
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    /// Children in the order the scan found them.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let i = id.index();
        &self.child_ids[self.child_start[i] as usize..self.child_start[i + 1] as usize]
    }

    /// The node's file-name component. The root's name is its full path.
    pub fn name(&self, id: NodeId) -> &OsStr {
        if id == self.root {
            return self.root_path.as_os_str();
        }
        let name = self.nodes[id.index()].name;
        if name.odd {
            return self.odd_names.get(name.off as usize).map_or(OsStr::new(""), |n| n.as_os_str());
        }
        let start = name.off as usize;
        let text = start
            .checked_add(name.len as usize)
            .and_then(|end| self.names.get(start..end))
            .unwrap_or("");
        OsStr::new(text)
    }

    /// Writes the node's absolute path into `out`, reusing its buffer.
    pub fn path_into(&self, id: NodeId, out: &mut PathBuf) {
        let mut chain = Vec::with_capacity(self.nodes[id.index()].depth as usize);
        let mut cursor = id;
        while let Some(parent) = self.nodes[cursor.index()].parent {
            chain.push(cursor);
            cursor = parent;
        }
        out.clear();
        out.push(&self.root_path);
        for &part in chain.iter().rev() {
            out.push(self.name(part));
        }
    }

    /// Built by walking parents, since nodes store only their name.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut out = PathBuf::new();
        self.path_into(id, &mut out);
        out
    }

    /// Walks `path` down from the root one component at a time, scanning the
    /// siblings at each level: fine for a lookup, wrong for resolving every node.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let rest = path.strip_prefix(&self.root_path).ok()?;
        let mut cursor = self.root;
        for component in rest.components() {
            let name = component.as_os_str();
            cursor = *self.children(cursor).iter().find(|&&child| self.name(child) == name)?;
        }
        Some(cursor)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn errors(&self) -> &[ScanError] {
        &self.errors
    }

    pub fn backend_used(&self) -> &BackendUsed {
        &self.backend_used
    }

    /// The ids of `id`'s subtree, `id` included.
    pub fn subtree(&self, id: NodeId) -> Range<u32> {
        id.0..self.nodes[id.index()].end
    }

    /// Every node in the subtree of `id`, `id` included, parents before children.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        self.subtree(id).map(NodeId).collect()
    }

    /// Children of `id` sorted by `subtree_size` descending.
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = self.children(id).to_vec();
        out.sort_by(|a, b| {
            self.nodes[b.index()]
                .subtree_size
                .cmp(&self.nodes[a.index()].subtree_size)
                .then(a.cmp(b))
        });
        out
    }

    /// Every file the scan found under more than one name.
    pub fn link_groups(&self) -> impl Iterator<Item = LinkGroup> + '_ {
        self.link_groups.iter().enumerate().map(|(g, &(primary, complete, bytes))| {
            let range = self.link_start[g] as usize..self.link_start[g + 1] as usize;
            LinkGroup { primary, nodes: self.link_ids[range].to_vec(), complete, bytes }
        })
    }

    /// blake3 of the file's contents. Computed on demand, never during
    /// [`Catalog::build`]. `None` for directories and unreadable files.
    pub fn content_hash(&self, id: NodeId) -> Option<blake3::Hash> {
        if self.nodes[id.index()].kind != EntryKind::File {
            return None;
        }
        let file = std::fs::File::open(self.path(id)).ok()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(file).ok()?;
        Some(hasher.finalize())
    }

    /// Groups of 2+ nodes with identical size AND identical content hash.
    ///
    /// Size is the cheap discriminator: only files whose size already collides
    /// with another file's get hashed, so the whole tree is never read. A
    /// hard link's extra names are the same file, not a copy of it, so only
    /// its primary name takes part.
    pub fn duplicate_groups(&self, min_size: u64) -> Vec<Vec<NodeId>> {
        let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for node in &self.nodes {
            if node.kind == EntryKind::File && !node.extra_link && node.size >= min_size {
                by_size.entry(node.size).or_default().push(node.id);
            }
        }
        let candidates: Vec<NodeId> =
            by_size.into_values().filter(|group| group.len() >= 2).flatten().collect();

        let hashed: Vec<(NodeId, [u8; 32])> = candidates
            .par_iter()
            .filter_map(|&id| self.content_hash(id).map(|h| (id, *h.as_bytes())))
            .collect();

        let mut by_hash: HashMap<(u64, [u8; 32]), Vec<NodeId>> = HashMap::new();
        for (id, hash) in hashed {
            by_hash.entry((self.nodes[id.index()].size, hash)).or_default().push(id);
        }

        let mut groups: Vec<Vec<NodeId>> = by_hash
            .into_values()
            .filter(|group| group.len() >= 2)
            .map(|mut group| {
                group.sort_unstable();
                group
            })
            .collect();
        groups.sort_unstable();
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Entry;
    use crate::scan::table::NO_BLOB;

    /// Regression: linking `C:\a\b-x` inside `C:\a\b` because its bytes start
    /// with it, or `C:\a\b\c` outside it, or counting a repeated path twice.
    #[test]
    fn whole_paths_link_on_component_boundaries() {
        let entry = |path: &str, kind, size| Entry {
            path: PathBuf::from(path),
            kind,
            size,
            allocated: None,
            modified: None,
            accessed: None,
        };
        let entries = vec![
            entry(r"C:\a\b-x", EntryKind::File, 1),
            entry(r"C:\a\b\c", EntryKind::File, 2),
            entry(r"C:\a", EntryKind::Dir, 0),
            entry(r"C:\a\b", EntryKind::Dir, 0),
            entry(r"C:\a\b\c", EntryKind::File, 2),
            entry(r"C:\ab", EntryKind::File, 4),
        ];
        let catalog = Catalog::build(ScanReport::from_entries(
            PathBuf::from(r"C:\"),
            entries,
            Vec::new(),
            BackendUsed::Mft,
        ));
        let parent = |path: &str| {
            let id = catalog.find(Path::new(path)).unwrap_or_else(|| panic!("{path} missing"));
            catalog.path(catalog.node(id).parent.unwrap())
        };
        assert_eq!(parent(r"C:\a\b-x"), Path::new(r"C:\a"));
        assert_eq!(parent(r"C:\a\b\c"), Path::new(r"C:\a\b"));
        assert_eq!(parent(r"C:\ab"), Path::new(r"C:\"));
        assert_eq!(catalog.node(catalog.root()).subtree_size, 7);
    }

    fn blob(size: u64) -> Blob {
        Blob { size, allocated: Some(size), modified: None, accessed: None }
    }

    /// Regression: a damaged table looping a chain of rows back on itself, or
    /// naming a parent that does not exist, either hanging the build, panicking,
    /// or counting those bytes into the tree.
    #[test]
    fn orphans_and_cycles_are_dropped_and_counted() {
        let mut table = ScanTable::new();
        let file = table.push_blob(blob(10));
        let lost = table.push_blob(blob(1000));
        let dir = table.push_node(0, "dir", NO_BLOB, EntryKind::Dir, 0);
        table.push_node(dir, "kept.bin", file, EntryKind::File, 0);
        // Rows 3 and 4 name each other; row 5 names a parent past the table;
        // row 6 names itself.
        table.push_node(4, "loop-a", NO_BLOB, EntryKind::Dir, 0);
        table.push_node(3, "loop-b", lost, EntryKind::File, 0);
        table.push_node(99, "orphan", lost, EntryKind::File, 0);
        table.push_node(6, "self", lost, EntryKind::File, 0);

        let catalog = Catalog::build(ScanReport {
            root: PathBuf::from(r"C:\"),
            table,
            errors: Vec::new(),
            backend_used: BackendUsed::Mft,
        });
        assert_eq!(catalog.len(), 3, "root, dir, kept.bin");
        assert_eq!(catalog.node(catalog.root()).subtree_size, 10);
        assert_eq!(catalog.errors().len(), 1);
        assert!(catalog.errors()[0].message.starts_with("4 entries"), "{:?}", catalog.errors());
        assert_eq!(catalog.path(NodeId(2)), Path::new(r"C:\dir\kept.bin"));
        assert_eq!(catalog.subtree(NodeId(1)), 1..3);
    }

    /// Regression: a hard-linked file counted once per name in a directory
    /// total, which inflates every ancestor by its size again for each link.
    #[test]
    fn a_hard_link_counts_its_bytes_once_under_its_primary_name() {
        let mut table = ScanTable::new();
        let shared = table.push_blob(blob(100));
        let a = table.push_node(0, "a", NO_BLOB, EntryKind::Dir, 0);
        let b = table.push_node(0, "b", NO_BLOB, EntryKind::Dir, 0);
        table.push_node(b, "second.bin", shared, EntryKind::File, EXTRA_LINK);
        table.push_node(a, "first.bin", shared, EntryKind::File, 0);

        let catalog = Catalog::build(ScanReport {
            root: PathBuf::from(r"C:\"),
            table,
            errors: Vec::new(),
            backend_used: BackendUsed::Mft,
        });
        let first = catalog.find(Path::new(r"C:\a\first.bin")).unwrap();
        let second = catalog.find(Path::new(r"C:\b\second.bin")).unwrap();
        assert_eq!(catalog.node(catalog.root()).subtree_size, 100);
        assert_eq!(catalog.node(catalog.root()).subtree_allocated, 100);
        assert!(!catalog.node(first).extra_link && catalog.node(second).extra_link);
        assert_eq!(catalog.node(second).size, 100, "an extra name still shows the size");
        assert_eq!(catalog.node(catalog.node(second).parent.unwrap()).subtree_size, 0);
        assert_eq!(catalog.node(first).links, 2);
        let groups: Vec<LinkGroup> = catalog.link_groups().collect();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].primary, first);
        assert!(groups[0].complete);
        assert_eq!(groups[0].nodes.len(), 2);
    }
}
