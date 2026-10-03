//! A directory on disk to a validated [`Pack`].
//!
//! ```text
//! mypack/
//!   pack.toml     name, version, the kinds it declares
//!   rules/*.nom
//! ```
//!
//! ```toml
//! name = "rust"
//! version = "0.2.0"
//!
//! [kinds."toolchain-cache/v1"]
//! disposition = "reclaimable"
//! confidence = 0.7
//! ```
//!
//! Fetching, caching and the lock file are deliberately not here: this module
//! takes a path that already exists and answers whether what is in it is a
//! usable pack. That keeps pack validation testable with a `tempfile` and no
//! network.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ast::{Disposition, Rule};
use crate::diagnostic::{Diagnostic, Source, Span};
use crate::kind::{Kind, Kinds, RESERVED, split_id};
use crate::parse::parse;

/// A loaded pack: its manifest plus every rule in `rules/`.
#[derive(Debug, Clone)]
pub struct Pack {
    pub name: String,
    pub version: String,
    /// Kinds this pack declares, beyond [`Kinds::builtin`].
    pub kinds: Vec<Kind>,
    /// Rules in load order: `rules/*.nom` sorted by file name, then by
    /// position within each file. `docs/lang.md` makes rule order the last
    /// conflict tie-break, so the order has to be a property of the directory
    /// rather than of the filesystem's readdir order.
    pub rules: Vec<LoadedRule>,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LoadedRule {
    pub rule: Rule,
    /// The `.nom` file it came from.
    pub file: PathBuf,
}

/// Why a directory is not a usable pack.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// A rule file (or `pack.toml`) is wrong, and we can point at where.
    #[error("{0}")]
    Source(#[from] Diagnostic),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    version: String,
    #[serde(default)]
    kinds: BTreeMap<String, KindDecl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KindDecl {
    disposition: String,
    confidence: f64,
}

/// Read `dir/pack.toml` and every `dir/rules/*.nom`, validating both.
pub fn load(dir: &Path) -> Result<Pack, PackError> {
    let manifest_path = dir.join("pack.toml");
    let manifest = Source::new(display(&manifest_path), read(&manifest_path)?);

    let rules_dir = dir.join("rules");
    let mut files: Vec<PathBuf> = match fs::read_dir(&rules_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "nom"))
            .collect(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(PackError::Io { path: rules_dir, source }),
    };
    files.sort();

    let mut sources = Vec::with_capacity(files.len());
    for file in files {
        let text = read(&file)?;
        sources.push((Source::new(display(&file), text), file));
    }
    from_sources(&manifest, sources, dir.to_path_buf())
}

/// [`load`], for a manifest and rule files already in memory — the built-in
/// pack, which is compiled into the binary, goes through here, so it passes
/// exactly the checks a downloaded pack does.
///
/// `rules` is in load order, each source paired with the path it came from.
pub fn from_sources(
    manifest: &Source,
    rules: impl IntoIterator<Item = (Source, PathBuf)>,
    dir: PathBuf,
) -> Result<Pack, PackError> {
    let text = &manifest.text;
    let parsed: Manifest = toml::from_str(text).map_err(|error| {
        let span = error.span().map_or(Span::new(0, text.len()), |r| Span::new(r.start, r.end));
        Diagnostic::new(manifest, span, format!("invalid pack manifest: {}", error.message()))
            .with_label("expected `name`, `version` and optional `[kinds.\"name/vN\"]` tables")
    })?;

    let mut declared = Vec::new();
    for (id, decl) in &parsed.kinds {
        declared.push(declare(manifest, id, decl)?);
    }
    let kinds = Kinds::builtin().with(declared.iter().cloned());

    let mut loaded = Vec::new();
    // Title to where it was first seen, so the duplicate message can say which
    // file already used it.
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (source, file) in rules {
        for rule in parse(&source, &kinds)? {
            if let Some(first) = seen.get(&rule.title.value) {
                return Err(Diagnostic::new(
                    &source,
                    rule.title.span,
                    format!("duplicate rule title `{}`", rule.title.value),
                )
                .with_label(format!("already used in {}", display(first)))
                .with_help("a verdict cites its rule by title, so titles must be unique in a pack")
                .into());
            }
            seen.insert(rule.title.value.clone(), file.clone());
            loaded.push(LoadedRule { rule, file: file.clone() });
        }
    }

    Ok(Pack { name: parsed.name, version: parsed.version, kinds: declared, rules: loaded, dir })
}

/// One `[kinds."name/vN"]` table to a [`Kind`].
fn declare(manifest: &Source, id: &str, decl: &KindDecl) -> Result<Kind, Diagnostic> {
    // The table header is the only place the id is written, so a diagnostic
    // about it points there.
    let span = manifest
        .text
        .find(&format!("\"{id}\""))
        .map_or(Span::new(0, 0), |at| Span::new(at, at + id.len() + 2));
    let error = |message: String| Diagnostic::new(manifest, span, message);

    let Some((name, version)) = split_id(id) else {
        return Err(error(format!("`{id}` is not a kind"))
            .with_label("expected `name/vN`, as in `toolchain-cache/v1`"));
    };
    if RESERVED.contains(&name) {
        return Err(error(format!("kind `{name}` is built in"))
            .with_label("a pack may not redefine it")
            .with_help("use it as it is, or declare a kind under a name of your own"));
    }
    let Some(disposition) = Disposition::lookup(&decl.disposition) else {
        return Err(error(format!("kind `{id}` has an unknown disposition `{}`", decl.disposition))
            .with_label("expected `keep`, `reclaimable` or `review`"));
    };
    if !(0.0..=1.0).contains(&decl.confidence) {
        return Err(error(format!("kind `{id}` has confidence {} out of range", decl.confidence))
            .with_label("expected a number between 0.0 and 1.0"));
    }
    Ok(Kind::new(name, version, disposition, decl.confidence as f32))
}

fn read(path: &Path) -> Result<String, PackError> {
    fs::read_to_string(path).map_err(|source| PackError::Io { path: path.to_path_buf(), source })
}

/// Paths go into diagnostics, so they render with forward slashes on every
/// platform and a Windows test asserts the same string a Linux one does.
fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
