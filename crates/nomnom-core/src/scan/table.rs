//! The flat shape every backend hands its scan over in.
//!
//! One row per name, in whatever order the backend found them, each naming its
//! parent row by index. No row carries a path: [`crate::catalog::Catalog`]
//! links the rows into a tree and builds a path only when one is asked for.
//! Sizes and times live in a separate blob table, keyed by the file a name
//! points at, because a hard-linked file is one set of bytes behind several
//! names: each name is its own row under its own parent, and all of them point
//! at the same blob.
//!
//! The elevated helper sends this table over the pipe nearly as it lies in
//! memory, so its fields are plain fixed-size numbers plus one names buffer.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{Entry, EntryKind};

/// `Name::blob` for a row with no blob of its own (a root no backend reported).
pub const NO_BLOB: u32 = u32::MAX;

/// `name_off` indexes [`ScanTable::odd_names`] rather than the UTF-8 buffer:
/// a walked name that is not valid Unicode keeps its exact spelling.
pub const ODD_NAME: u8 = 0b01;
/// Not the blob's primary name. Its row shows the blob's size, but its bytes
/// are already counted under the primary name, so it adds nothing upward.
pub const EXTRA_LINK: u8 = 0b10;

/// One name: a file, directory or link, filed under its parent row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Name {
    /// Row index of the parent. Row 0 is the scan root, whose parent is
    /// ignored. A row whose parent chain never reaches row 0 (an orphan, a
    /// cycle in a damaged table) is dropped by the catalog and counted.
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u32,
    /// Index into [`ScanTable::blobs`], or [`NO_BLOB`].
    pub blob: u32,
    pub kind: EntryKind,
    pub flags: u8,
}

/// The facts of one file's contents, shared by every name it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blob {
    /// Logical size. Zero for directories.
    pub size: u64,
    /// Bytes on disk, `None` when the backend has no cheap answer.
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTable {
    /// Row 0 is the scan root. Its name is empty: the catalog names the root
    /// by the report's root path.
    pub nodes: Vec<Name>,
    /// Every UTF-8 name, back to back.
    pub names: String,
    pub odd_names: Vec<OsString>,
    pub blobs: Vec<Blob>,
}

impl Default for ScanTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanTable {
    /// A table holding only the root row, a directory with no blob.
    pub fn new() -> Self {
        let root = Name {
            parent: 0,
            name_off: 0,
            name_len: 0,
            blob: NO_BLOB,
            kind: EntryKind::Dir,
            flags: 0,
        };
        Self { nodes: vec![root], names: String::new(), odd_names: Vec::new(), blobs: Vec::new() }
    }

    pub fn push_blob(&mut self, blob: Blob) -> u32 {
        self.blobs.push(blob);
        (self.blobs.len() - 1) as u32
    }

    /// Appends a row and returns its index.
    pub fn push_node(
        &mut self,
        parent: u32,
        name: &str,
        blob: u32,
        kind: EntryKind,
        flags: u8,
    ) -> u32 {
        let (name_off, name_len) = self.push_name(name);
        self.nodes.push(Name { parent, name_off, name_len, blob, kind, flags: flags & !ODD_NAME });
        (self.nodes.len() - 1) as u32
    }

    fn push_os_node(&mut self, parent: u32, name: &OsStr, blob: u32, kind: EntryKind) -> u32 {
        match name.to_str() {
            Some(text) => self.push_node(parent, text, blob, kind, 0),
            None => {
                self.odd_names.push(name.to_owned());
                let name_off = (self.odd_names.len() - 1) as u32;
                self.nodes.push(Name {
                    parent,
                    name_off,
                    name_len: 0,
                    blob,
                    kind,
                    flags: ODD_NAME,
                });
                (self.nodes.len() - 1) as u32
            }
        }
    }

    fn push_name(&mut self, name: &str) -> (u32, u32) {
        // Offsets are u32 to keep rows small; four gigabytes of names is far
        // past any volume, and a name past it reads back as empty rather
        // than as some other name.
        let Ok(off) = u32::try_from(self.names.len()) else { return (u32::MAX, 0) };
        self.names.push_str(name);
        (off, name.len() as u32)
    }

    /// A row's name, empty when its range does not resolve.
    pub fn name(&self, row: &Name) -> &OsStr {
        if row.flags & ODD_NAME != 0 {
            return self
                .odd_names
                .get(row.name_off as usize)
                .map_or(OsStr::new(""), |n| n.as_os_str());
        }
        let start = row.name_off as usize;
        let text = start
            .checked_add(row.name_len as usize)
            .and_then(|end| self.names.get(start..end))
            .unwrap_or("");
        OsStr::new(text)
    }

    /// The table for a backend that produces whole paths: the walk, and test
    /// fixtures. Each path is its own blob. An entry spelled exactly as `root`
    /// fills row 0; a path repeated is kept once; every other entry is filed
    /// under its nearest ancestor that was reported, or under the root when
    /// none was, so a gap in the walk does not drop what lies below it.
    pub fn from_entries(root: &Path, entries: Vec<Entry>) -> Self {
        let mut table = Self::new();
        table.nodes.reserve(entries.len());
        table.blobs.reserve(entries.len());
        let root_bytes = root.as_os_str().as_encoded_bytes();

        // Paths keyed by their bytes: `Path`'s own hash re-parses components.
        let mut row_of: HashMap<&[u8], u32> = HashMap::with_capacity(entries.len());
        let mut pending: Vec<(usize, u32)> = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let bytes = entry.path.as_os_str().as_encoded_bytes();
            let blob = table.push_blob(Blob {
                size: entry.size,
                allocated: entry.allocated,
                modified: entry.modified,
                accessed: entry.accessed,
            });
            if bytes == root_bytes {
                table.nodes[0].blob = blob;
                table.nodes[0].kind = entry.kind;
                continue;
            }
            if row_of.contains_key(bytes) {
                continue;
            }
            let name: &OsStr = entry.path.file_name().unwrap_or(entry.path.as_os_str());
            // The parent is filled in once every path has a row.
            let row = table.push_os_node(0, name, blob, entry.kind);
            row_of.insert(bytes, row);
            pending.push((index, row));
        }
        for (index, row) in pending {
            let mut cursor = entries[index].path.as_path();
            table.nodes[row as usize].parent = loop {
                let Some(parent) = cursor.parent() else { break 0 };
                let bytes = parent.as_os_str().as_encoded_bytes();
                if bytes == root_bytes {
                    break 0;
                }
                if let Some(&found) = row_of.get(bytes) {
                    break found;
                }
                cursor = parent;
            };
        }
        table
    }

    /// Every row's full path under `root`, rows in table order. For tests and
    /// diagnostics: it builds every path, which is what the table exists to
    /// avoid. A row whose chain does not reach the root yields `None`.
    pub fn paths(&self, root: &Path) -> Vec<Option<PathBuf>> {
        (0..self.nodes.len())
            .map(|row| {
                let mut parts = Vec::new();
                let mut cursor = row;
                while cursor != 0 {
                    if parts.len() > self.nodes.len() {
                        return None;
                    }
                    let node = self.nodes.get(cursor)?;
                    parts.push(self.name(node));
                    cursor = node.parent as usize;
                }
                let mut path = root.to_path_buf();
                path.extend(parts.iter().rev());
                Some(path)
            })
            .collect()
    }
}
