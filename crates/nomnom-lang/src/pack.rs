//! A directory on disk to a validated [`Pack`].
//!
//! ```text
//! mypack/
//!   pack.toml     name, version, the labels it introduces
//!   rules/*.nom
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

use crate::ast::Rule;
use crate::diagnostic::{Diagnostic, Source, Span};
use crate::parse::parse;

/// Labels the built-in pack already defines, which any pack may use without
/// declaring. Mirrors `nomnom_core::verdict::Label` by name only — see
/// `crate::ast` for why this crate does not depend on core.
pub const BUILTIN_LABELS: &[&str] = &["build-output", "cache", "duplicate", "stale-download"];

/// A loaded pack: its manifest plus every rule in `rules/`.
#[derive(Debug, Clone)]
pub struct Pack {
    pub name: String,
    pub version: String,
    /// Labels this pack introduces, beyond [`BUILTIN_LABELS`].
    pub labels: Vec<String>,
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
    /// The `.nom` file it came from, for provenance in a verdict.
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
struct Manifest {
    name: String,
    version: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Read `dir/pack.toml` and every `dir/rules/*.nom`, validating both.
pub fn load(dir: &Path) -> Result<Pack, PackError> {
    let manifest_path = dir.join("pack.toml");
    let manifest_text = read(&manifest_path)?;
    let manifest_source = Source::new(display(&manifest_path), manifest_text.clone());
    let manifest: Manifest = toml::from_str(&manifest_text).map_err(|error| {
        let span =
            error.span().map_or(Span::new(0, manifest_text.len()), |r| Span::new(r.start, r.end));
        Diagnostic::new(
            &manifest_source,
            span,
            format!("invalid pack manifest: {}", error.message()),
        )
        .with_label("expected `name`, `version` and an optional `labels` list")
    })?;

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

    let declared: Vec<&str> =
        BUILTIN_LABELS.iter().copied().chain(manifest.labels.iter().map(String::as_str)).collect();

    let mut rules = Vec::new();
    // Rule name to where it was first seen, so the duplicate message can say
    // which file already used it.
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();

    for file in files {
        let text = read(&file)?;
        let source = Source::new(display(&file), text);
        for rule in parse(&source)? {
            let label = &rule.then.label;
            if !declared.iter().any(|known| *known == label.value) {
                return Err(Diagnostic::new(
                    &source,
                    label.span,
                    format!("rule uses the undeclared label `{}`", label.value),
                )
                .with_label(format!(
                    "`{}` is not declared by pack `{}`",
                    label.value, manifest.name
                ))
                .with_help(format!(
                    "add it to `labels` in pack.toml, or use a built-in label ({})",
                    BUILTIN_LABELS.join(", ")
                ))
                .into());
            }
            if let Some(first) = seen.get(&rule.name.value) {
                return Err(Diagnostic::new(
                    &source,
                    rule.name.span,
                    format!("duplicate rule name `{}`", rule.name.value),
                )
                .with_label(format!("already defined in {}", display(first)))
                .with_help("a verdict cites its rule by name, so names must be unique in a pack")
                .into());
            }
            seen.insert(rule.name.value.clone(), file.clone());
            rules.push(LoadedRule { rule, file: file.clone() });
        }
    }

    Ok(Pack {
        name: manifest.name,
        version: manifest.version,
        labels: manifest.labels,
        rules,
        dir: dir.to_path_buf(),
    })
}

fn read(path: &Path) -> Result<String, PackError> {
    fs::read_to_string(path).map_err(|source| PackError::Io { path: path.to_path_buf(), source })
}

/// Paths go into diagnostics, so they render with forward slashes on every
/// platform and a Windows test asserts the same string a Linux one does.
fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
