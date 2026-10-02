//! The tree model with rolled-up aggregates.
//!
//! Nodes live in one `Vec` arena addressed by [`NodeId`] rather than behind
//! `Rc`/`RefCell`: a volume scan is millions of nodes, and a flat arena keeps
//! them contiguous, makes every reference a `Copy` `u32`, and lets traversals
//! stay iterative — which is what keeps a 2000-deep chain from blowing the
//! stack where a recursive `Rc` tree would.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::scan::{BackendUsed, Entry, EntryKind, ScanError, ScanReport};
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
    pub children: Vec<NodeId>,
    /// File-name component. The root holds its full root path instead, so
    /// [`Catalog::path`] can rebuild absolute paths from names alone.
    pub name: OsString,
    pub kind: EntryKind,
    /// Own size; 0 for directories.
    pub size: u64,
    /// Own on-disk size, as [`Entry::allocated`](crate::scan::Entry::allocated)
    /// reported it: `None` when the backend had no cheap answer.
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// Rolled up over the subtree, inclusive of self.
    pub subtree_size: u64,
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
}

pub struct Catalog {
    nodes: Vec<Node>,
    root: NodeId,
    errors: Vec<ScanError>,
    backend_used: BackendUsed,
}

impl Catalog {
    /// Build the tree from a scan.
    ///
    /// Entry order is not assumed: the MFT backend emits entries in MFT-record
    /// order, so a child routinely arrives before its parent. Entries are
    /// sorted so every subtree is one contiguous run right after its root,
    /// which lets parents be resolved with a stack of open ancestors instead of
    /// a path index -- an index would pin every full path in memory for the
    /// catalog's lifetime, the bulk of a volume scan's footprint.
    pub fn build(report: ScanReport) -> Self {
        let ScanReport { root: root_path, mut entries, errors, backend_used } = report;

        let started = Instant::now();
        sort_subtrees(&mut entries);
        let started = timings::lap("Catalog::build sort", started);

        let mut nodes: Vec<Node> = Vec::with_capacity(entries.len() + 1);
        let root = NodeId(0);
        nodes.push(blank_node(root, root_path.clone().into_os_string(), EntryKind::Dir));

        // The previous entry and its scanned ancestors, root at the bottom. The
        // root is never popped, so an entry outside it still lands somewhere.
        let mut open: Vec<(PathBuf, NodeId)> = vec![(root_path.clone(), root)];

        // Paths compare as bytes from here on. Both backends spell every path
        // by joining names onto the root, so a byte prefix ending at a
        // separator is exactly a component prefix, and `Path`'s component-wise
        // comparisons, which re-parse both paths, are pure overhead: they were
        // two thirds of building a 4.5M-entry catalog.
        let root_bytes = root_path.as_os_str().as_encoded_bytes().to_vec();
        for entry in entries {
            let path = entry.path.as_os_str().as_encoded_bytes();
            if path == root_bytes.as_slice() {
                let node = &mut nodes[root.index()];
                node.kind = entry.kind;
                node.size = entry.size;
                node.allocated = entry.allocated;
                node.modified = entry.modified;
                node.accessed = entry.accessed;
                continue;
            }
            // A path repeated by the backend must not become a second node: its
            // bytes would be counted twice. Sorting made repeats adjacent, so
            // the repeat is always the top of the stack.
            if open.len() > 1 && bytes_of(&open[open.len() - 1].0) == path {
                continue;
            }
            // Attach to the nearest ANCESTOR that was actually scanned, so an
            // entry whose parent fell outside the set still lands somewhere sane
            // instead of being dropped.
            while open.len() > 1 && !is_under(bytes_of(&open[open.len() - 1].0), path) {
                open.pop();
            }
            let parent = open[open.len() - 1].1;

            let name = entry
                .path
                .file_name()
                .map(OsString::from)
                .unwrap_or_else(|| entry.path.clone().into_os_string());
            let id = NodeId(nodes.len() as u32);
            let mut node = blank_node(id, name, entry.kind);
            node.parent = Some(parent);
            node.size = entry.size;
            node.allocated = entry.allocated;
            node.modified = entry.modified;
            node.accessed = entry.accessed;
            nodes.push(node);
            // Ids are handed out in sorted order, so every parent's children
            // arrive already sorted and sibling order is reproducible.
            nodes[parent.index()].children.push(id);
            open.push((entry.path, id));
        }

        let started = timings::lap("Catalog::build link", started);
        let mut catalog = Self { nodes, root, errors, backend_used };
        catalog.recompute();
        timings::lap("Catalog::build roll-up", started);
        catalog
    }

    /// Breadth-first order (parents before children), then aggregates rolled up
    /// over its reverse. Iterative throughout — depth is unbounded in practice.
    fn recompute(&mut self) {
        let mut order: Vec<NodeId> = Vec::with_capacity(self.nodes.len());
        order.push(self.root);
        self.nodes[self.root.index()].depth = 0;
        let mut cursor = 0;
        while cursor < order.len() {
            let id = order[cursor];
            cursor += 1;
            let depth = self.nodes[id.index()].depth;
            let children = std::mem::take(&mut self.nodes[id.index()].children);
            for &child in &children {
                self.nodes[child.index()].depth = depth + 1;
                order.push(child);
            }
            self.nodes[id.index()].children = children;
        }

        for &id in order.iter().rev() {
            let (mut subtree_size, mut file_count, mut dir_count, mut max_modified) = {
                let node = &self.nodes[id.index()];
                (node.size, u64::from(node.kind == EntryKind::File), 0, node.modified)
            };
            let children = std::mem::take(&mut self.nodes[id.index()].children);
            for &child in &children {
                let child = &self.nodes[child.index()];
                subtree_size += child.subtree_size;
                file_count += child.file_count;
                dir_count += child.dir_count + u64::from(child.kind == EntryKind::Dir);
                // `None` sorts below every `Some`, so this is "the newest known
                // stamp, or nothing if nothing in the subtree has one".
                max_modified = max_modified.max(child.max_modified);
            }
            let node = &mut self.nodes[id.index()];
            node.children = children;
            node.subtree_size = subtree_size;
            node.file_count = file_count;
            node.dir_count = dir_count;
            node.max_modified = max_modified;
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.index()].children
    }

    /// Reconstructed by walking parents, since nodes store only their name.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            let node = &self.nodes[current.index()];
            parts.push(node.name.as_os_str());
            cursor = node.parent;
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(part);
        }
        path
    }

    /// Walks `path` down from the root one component at a time, scanning the
    /// siblings at each level: fine for a lookup, wrong for resolving every node.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let root_path = Path::new(&self.nodes[self.root.index()].name);
        let rest = path.strip_prefix(root_path).ok()?;
        let mut cursor = self.root;
        for component in rest.components() {
            let name = component.as_os_str();
            cursor = *self.children(cursor).iter().find(|&&child| self.node(child).name == name)?;
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

    /// Every node in the subtree of `id`, `id` included, parents before children.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![id];
        let mut cursor = 0;
        while cursor < out.len() {
            let current = out[cursor];
            cursor += 1;
            out.extend_from_slice(&self.nodes[current.index()].children);
        }
        out
    }

    /// Children of `id` sorted by `subtree_size` descending.
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = self.nodes[id.index()].children.clone();
        out.sort_by(|a, b| {
            self.nodes[b.index()]
                .subtree_size
                .cmp(&self.nodes[a.index()].subtree_size)
                .then(a.cmp(b))
        });
        out
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
    /// with another file's get hashed, so the whole tree is never read.
    pub fn duplicate_groups(&self, min_size: u64) -> Vec<Vec<NodeId>> {
        let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for node in &self.nodes {
            if node.kind == EntryKind::File && node.size >= min_size {
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

/// Puts entries in the order [`Catalog::build`] links them in. Input already
/// in this order sorts in one linear pass.
pub(crate) fn sort_subtrees(entries: &mut [Entry]) {
    entries.par_sort_unstable_by(|a, b| subtree_order(&a.path, &b.path));
}

/// Byte order with every path separator ranked below every other byte, so a
/// directory is followed immediately by its whole subtree: `a/b`, `a/b/c`,
/// `a/b-x`. Plain byte order puts `a/b-x` between `a/b` and `a/b/c`, and the
/// stack in [`Catalog::build`] would pop `a/b` before reaching its child.
fn subtree_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    // Separators are ASCII and every byte of a multi-byte character is >= 0x80,
    // so testing single bytes cannot mistake part of a character for one.
    let key = |byte: u8| if is_separator(byte) { 0 } else { byte };
    let (a, b) = (bytes_of(a), bytes_of(b));
    // Sorted neighbours share most of their path, so the shared prefix is
    // skipped eight bytes at a time before any byte is mapped; this is the
    // same order as comparing the mapped sequences, at a third of the cost.
    let shared = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= shared && a[i..i + 8] == b[i..i + 8] {
        i += 8;
    }
    while i < shared {
        if a[i] != b[i] {
            // Two different separators map to the same key; keep comparing.
            match key(a[i]).cmp(&key(b[i])) {
                std::cmp::Ordering::Equal => {}
                unequal => return unequal,
            }
        }
        i += 1;
    }
    a.len().cmp(&b.len())
}

fn bytes_of(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

fn is_separator(byte: u8) -> bool {
    std::path::is_separator(char::from(byte))
}

/// Whether `path` lies strictly below `dir`, both spelled the way a backend
/// spells them (see [`Catalog::build`]).
fn is_under(dir: &[u8], path: &[u8]) -> bool {
    path.len() > dir.len()
        && path.starts_with(dir)
        && (dir.last().copied().is_some_and(is_separator) || is_separator(path[dir.len()]))
}

fn blank_node(id: NodeId, name: OsString, kind: EntryKind) -> Node {
    Node {
        id,
        parent: None,
        children: Vec::new(),
        name,
        kind,
        size: 0,
        allocated: None,
        modified: None,
        accessed: None,
        subtree_size: 0,
        file_count: 0,
        dir_count: 0,
        max_modified: None,
        depth: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the byte-compared link treating `C:\a\b-x` as inside
    /// `C:\a\b` because its bytes start with it, or `C:\a\b\c` as outside it,
    /// or counting a repeated path twice.
    #[test]
    fn byte_link_respects_component_boundaries() {
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
        let catalog = Catalog::build(ScanReport {
            root: PathBuf::from(r"C:\"),
            entries,
            errors: Vec::new(),
            backend_used: BackendUsed::Mft,
        });
        let parent = |path: &str| {
            let id = catalog.find(Path::new(path)).unwrap_or_else(|| panic!("{path} missing"));
            catalog.path(catalog.node(id).parent.unwrap())
        };
        assert_eq!(parent(r"C:\a\b-x"), Path::new(r"C:\a"));
        assert_eq!(parent(r"C:\a\b\c"), Path::new(r"C:\a\b"));
        assert_eq!(parent(r"C:\ab"), Path::new(r"C:\"));
        assert_eq!(catalog.node(catalog.root()).subtree_size, 7);
    }
}
