//! `nomnom scan` — the tree, biggest first.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::scan::{BackendUsed, EntryKind, ScanError};
use serde::Serialize;

use crate::input::{self, BackendArg};

pub fn run(
    path: &Path,
    backend: BackendArg,
    show_errors: bool,
    depth: u32,
    top: usize,
    json: bool,
) -> Result<ExitCode> {
    let catalog = input::load(path, backend)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);

    let root = catalog.root();
    if json {
        let out = Output {
            backend_used: catalog.backend_used(),
            errors: catalog.errors(),
            tree: tree(&catalog, root, depth, top),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }

    let node = catalog.node(root);
    println!(
        "{}  {}  ({} files, {} dirs)",
        catalog.path(root).display(),
        format_size(node.subtree_size, BINARY),
        node.file_count,
        node.dir_count
    );
    print_children(&catalog, root, depth, top, "");
    Ok(ExitCode::SUCCESS)
}

fn print_children(catalog: &Catalog, id: NodeId, depth: u32, top: usize, prefix: &str) {
    if depth == 0 {
        return;
    }
    let children = catalog.children_by_size(id);
    let shown = children.len().min(top);
    let hidden = children.len() - shown;
    for (index, &child) in children.iter().take(shown).enumerate() {
        let last = index + 1 == shown && hidden == 0;
        let (branch, carry) = if last { ("`- ", "   ") } else { ("|- ", "|  ") };
        let node = catalog.node(child);
        let marker = if node.kind == EntryKind::Dir { "/" } else { "" };
        println!(
            "{prefix}{branch}{}{marker}  {}",
            node.name.to_string_lossy(),
            format_size(node.subtree_size, BINARY)
        );
        print_children(catalog, child, depth - 1, top, &format!("{prefix}{carry}"));
    }
    if hidden > 0 {
        println!("{prefix}`- ... {hidden} more");
    }
}

#[derive(Serialize)]
struct Output<'a> {
    backend_used: &'a BackendUsed,
    errors: &'a [ScanError],
    tree: TreeNode,
}

/// A rendering view of [`nomnom_core::catalog::Node`], which is not itself
/// `Serialize` and carries arena ids a JSON consumer cannot resolve.
#[derive(Serialize)]
struct TreeNode {
    name: String,
    path: String,
    kind: EntryKind,
    size: u64,
    subtree_size: u64,
    file_count: u64,
    dir_count: u64,
    children: Vec<TreeNode>,
}

fn tree(catalog: &Catalog, id: NodeId, depth: u32, top: usize) -> TreeNode {
    let node = catalog.node(id);
    let children = if depth == 0 {
        Vec::new()
    } else {
        catalog
            .children_by_size(id)
            .into_iter()
            .take(top)
            .map(|child| tree(catalog, child, depth - 1, top))
            .collect()
    };
    TreeNode {
        name: node.name.to_string_lossy().into_owned(),
        path: catalog.path(id).display().to_string(),
        kind: node.kind,
        size: node.size,
        subtree_size: node.subtree_size,
        file_count: node.file_count,
        dir_count: node.dir_count,
        children,
    }
}
