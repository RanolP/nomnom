//! Drive-level breakdown by file extension.

use std::collections::HashMap;
use std::path::Path;

use super::Catalog;
use crate::scan::EntryKind;

/// Every file sharing one extension. Sizes are in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    /// Lowercase, without the dot; [`FileType::NO_EXTENSION`] for files that
    /// have none (`Makefile`, `.gitignore`).
    pub ext: String,
    pub bytes: u64,
    /// On-disk bytes, or `None` when any member's backend could not report
    /// them (the walk backend never can).
    pub allocated: Option<u64>,
    pub count: u64,
}

impl FileType {
    pub const NO_EXTENSION: &'static str = "(none)";
}

/// Files grouped by extension, biggest total first, ties by extension.
/// Directories and symlinks are not files and never counted.
pub fn file_types(catalog: &Catalog) -> Vec<FileType> {
    let mut by_ext: HashMap<String, FileType> = HashMap::new();
    for node in catalog.nodes().filter(|node| node.kind == EntryKind::File) {
        let ext = Path::new(&node.name).extension().map_or_else(
            || FileType::NO_EXTENSION.to_owned(),
            |ext| ext.to_string_lossy().to_lowercase(),
        );
        let entry = by_ext.entry(ext).or_insert_with_key(|ext| FileType {
            ext: ext.clone(),
            bytes: 0,
            allocated: Some(0),
            count: 0,
        });
        entry.bytes += node.size;
        entry.allocated = entry.allocated.zip(node.allocated).map(|(sum, own)| sum + own);
        entry.count += 1;
    }
    let mut out: Vec<FileType> = by_ext.into_values().collect();
    out.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.ext.cmp(&b.ext)));
    out
}
