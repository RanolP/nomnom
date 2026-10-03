//! `nomnom scan` — the tree, biggest first, as every file (`--view all`), as
//! the recognized subtrees (`--view recognized`), or as the other files
//! (`--view other`): the GUI tree's three views.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::ValueEnum;
use humansize::{BINARY, format_size};
use nomnom_core::catalog::{Catalog, NodeId};
use nomnom_core::scan::{BackendUsed, EntryKind, ScanError, VolumeRoot};
use nomnom_core::verdict::{Claim, Ownership, resolve_packs};
use serde::Serialize;

use crate::{classify, input};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum View {
    /// Every file, sized by its bytes.
    #[default]
    All,
    /// Pack › claim: the subtrees a pack recognizes, biggest first.
    Recognized,
    /// The files no pack recognizes, sized by those bytes only; each
    /// recognized folder shows as one link row.
    Other,
}

pub struct Request<'a> {
    pub drive: &'a VolumeRoot,
    pub show_errors: bool,
    pub packs: &'a [PathBuf],
    pub view: View,
    pub depth: u32,
    pub top: usize,
    pub json: bool,
}

pub fn run(request: Request) -> Result<ExitCode> {
    let Request { drive, show_errors, packs, view, depth, top, json } = request;
    if view == View::All {
        let catalog = input::load(drive)?;
        input::warn_backend(&catalog);
        input::report_errors(&catalog, show_errors);
        print_tree(&catalog, None, depth, top, json)?;
        return Ok(ExitCode::SUCCESS);
    }

    let packs = resolve_packs(drive.as_path(), packs)?;
    let (catalog, assessment) = input::load_assessed(drive, packs)?;
    input::warn_backend(&catalog);
    input::report_errors(&catalog, show_errors);
    let ownership = &assessment.ownership;
    match view {
        View::Recognized => {
            classify::render(&catalog, ownership, json, &mut std::io::stdout().lock())?
        }
        _ => print_tree(&catalog, Some(ownership), depth, top, json)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The whole tree, or with `other` the Other files tree.
fn print_tree(
    catalog: &Catalog,
    other: Option<&Ownership>,
    depth: u32,
    top: usize,
    json: bool,
) -> Result<()> {
    let root = catalog.root();
    if json {
        let out = Output {
            backend_used: catalog.backend_used(),
            errors: catalog.errors(),
            tree: tree(catalog, other, root, depth, top),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let node = catalog.node(root);
    match other {
        None => println!(
            "{}  {}  ({} files, {} dirs)",
            catalog.path(root).display(),
            format_size(node.subtree_size, BINARY),
            node.file_count,
            node.dir_count
        ),
        Some(ownership) => println!(
            "{}  other files {} of {}",
            catalog.path(root).display(),
            format_size(ownership.arbitrary(catalog, root), BINARY),
            format_size(node.subtree_size, BINARY),
        ),
    }
    print_children(catalog, other, root, depth, top, "");
    Ok(())
}

/// Biggest first by what the view sizes a node by.
fn children(catalog: &Catalog, other: Option<&Ownership>, id: NodeId) -> Vec<NodeId> {
    match other {
        None => catalog.children_by_size(id),
        Some(ownership) => ownership.children_by_arbitrary(catalog, id),
    }
}

fn print_children(
    catalog: &Catalog,
    other: Option<&Ownership>,
    id: NodeId,
    depth: u32,
    top: usize,
    prefix: &str,
) {
    if depth == 0 {
        return;
    }
    let children = children(catalog, other, id);
    let shown = children.len().min(top);
    let hidden = children.len() - shown;
    for (index, &child) in children.iter().take(shown).enumerate() {
        let last = index + 1 == shown && hidden == 0;
        let (branch, carry) = if last { ("`- ", "   ") } else { ("|- ", "|  ") };
        let node = catalog.node(child);
        let marker = if node.kind == EntryKind::Dir { "/" } else { "" };
        let name = catalog.name(child).to_string_lossy();
        // A recognized folder stands in for its whole subtree: one link row,
        // never descended, because nothing inside it is "other".
        if let Some(claim) = other.and_then(|ownership| ownership.claim_at(child)) {
            println!(
                "{prefix}{branch}{name}{marker}  -> recognized: {} [{}]  {}",
                claim.class,
                claim.provenance.rule,
                format_size(node.subtree_size, BINARY)
            );
            continue;
        }
        let bytes = match other {
            None => node.subtree_size,
            Some(ownership) => ownership.arbitrary(catalog, child),
        };
        println!("{prefix}{branch}{name}{marker}  {}", format_size(bytes, BINARY));
        print_children(catalog, other, child, depth - 1, top, &format!("{prefix}{carry}"));
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
    /// `--view other`: the subtree's bytes no pack recognizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    other_bytes: Option<u64>,
    /// `--view other`: the claim this link row stands in for.
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
    children: Vec<TreeNode>,
}

fn tree(
    catalog: &Catalog,
    other: Option<&Ownership>,
    id: NodeId,
    depth: u32,
    top: usize,
) -> TreeNode {
    let node = catalog.node(id);
    let claim = other.and_then(|ownership| ownership.claim_at(id)).cloned();
    let children = if depth == 0 || claim.is_some() {
        Vec::new()
    } else {
        children(catalog, other, id)
            .into_iter()
            .take(top)
            .map(|child| tree(catalog, other, child, depth - 1, top))
            .collect()
    };
    TreeNode {
        name: catalog.name(id).to_string_lossy().into_owned(),
        path: catalog.path(id).display().to_string(),
        kind: node.kind,
        size: node.size,
        subtree_size: node.subtree_size,
        file_count: node.file_count,
        dir_count: node.dir_count,
        other_bytes: other.map(|ownership| ownership.arbitrary(catalog, id)),
        claim,
        children,
    }
}
