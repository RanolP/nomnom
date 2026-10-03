//! A directory on disk to a validated [`Pack`].
//!
//! ```text
//! mypack/
//!   pack.toml     name, version, its icon, the kinds it declares
//!   icon.svg      optional, named by `icon`
//!   rules/*.toml  `[[rule]]` tables
//! ```
//!
//! ```toml
//! name = "rust"
//! version = "0.2.0"
//! icon = "icon.svg"
//!
//! [kinds."toolchain-cache/v1"]
//! disposition = "reclaimable"
//! ```
//!
//! Fetching, caching and the lock file are deliberately not here: this module
//! takes a path that already exists and answers whether what is in it is a
//! usable pack. That keeps pack validation testable with a `tempfile` and no
//! network.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::ast::{Disposition, Rule};
use crate::diagnostic::{Diagnostic, Source, Span};
use crate::kind::{Kind, Kinds, RESERVED, split_id};
use crate::parse::{CONFIDENCE_REMOVED, parse};

/// A loaded pack: its manifest plus every rule in `rules/`.
#[derive(Debug, Clone)]
pub struct Pack {
    pub name: String,
    pub version: String,
    /// Kinds this pack declares, beyond [`Kinds::builtin`].
    pub kinds: Vec<Kind>,
    /// Rules in load order: `rules/*.toml` sorted by file name, then by
    /// position within each file's `[[rule]]` array. `docs/lang.md` makes rule order the
    /// tie-break within a pack, so the order has to be a property of the directory
    /// rather than of the filesystem's readdir order.
    pub rules: Vec<LoadedRule>,
    pub dir: PathBuf,
    /// `None` when `pack.toml` names no icon.
    pub icon: Option<PackIcon>,
}

/// The icon `pack.toml` names, as named and as read.
///
/// A broken icon never fails the pack: the rules are what a pack is for, so a
/// missing or malformed picture costs the pack its picture and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIcon {
    /// The file name `icon` gives, relative to the pack directory.
    pub file: String,
    /// The SVG, checked to parse, or why it cannot be shown — a warning for
    /// whoever lists the pack, never a load error.
    pub svg: Result<Arc<[u8]>, String>,
}

/// Above this an icon is not an icon; it is refused before it is parsed.
pub const ICON_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct LoadedRule {
    pub rule: Rule,
    /// The rule file it came from.
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
    icon: Option<String>,
    #[serde(default)]
    kinds: BTreeMap<String, KindDecl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KindDecl {
    disposition: String,
    /// Read only to refuse it with [`CONFIDENCE_REMOVED`] rather than serde's
    /// bare "unknown field".
    #[serde(default)]
    confidence: Option<toml::Value>,
}

/// Read `dir/pack.toml` and every `dir/rules/*.toml`, validating both.
pub fn load(dir: &Path) -> Result<Pack, PackError> {
    let manifest_path = dir.join("pack.toml");
    let manifest = Source::new(display(&manifest_path), read(&manifest_path)?);

    let rules_dir = dir.join("rules");
    let mut files: Vec<PathBuf> = match fs::read_dir(&rules_dir) {
        Ok(entries) => entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()).collect(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(PackError::Io { path: rules_dir, source }),
    };
    files.sort();
    // The glob below skips a `.nom` file, so a pack written before rule files
    // became TOML would load with zero rules and no word about why.
    if let Some(old) = files.iter().find(|path| path.extension().is_some_and(|ext| ext == "nom")) {
        let source = Source::new(display(old), read(old)?);
        let first_line = source.text.find('\n').unwrap_or(source.text.len());
        return Err(Diagnostic::new(&source, Span::new(0, first_line), "rule file in the old format")
            .with_label("`.nom` rule files are no longer read")
            .with_help(
                "rule files are TOML now: rename to .toml and convert to [[rule]] tables (see docs/lang.md)",
            )
            .into());
    }
    files.retain(|path| path.extension().is_some_and(|ext| ext == "toml"));

    let mut sources = Vec::with_capacity(files.len());
    for file in files {
        let text = read(&file)?;
        sources.push((Source::new(display(&file), text), file));
    }
    from_sources(&manifest, sources, dir.to_path_buf(), |file| read_icon_file(dir, file))
}

/// The icon `dir/pack.toml` names, read without loading the pack — for a
/// listing that must show a pack whose rules do not load. `None` when the
/// manifest is unreadable or names no icon; loading the pack reports why.
pub fn load_icon(dir: &Path) -> Option<PackIcon> {
    let text = fs::read_to_string(dir.join("pack.toml")).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    match table.get("icon")? {
        toml::Value::String(file) => Some(icon(file.clone(), |file| read_icon_file(dir, file))),
        other => {
            Some(PackIcon { file: other.to_string(), svg: Err("`icon` is not a string".into()) })
        }
    }
}

fn read_icon_file(dir: &Path, file: &str) -> Result<Vec<u8>, String> {
    fs::read(dir.join(file)).map_err(|error| format!("cannot read {file}: {error}"))
}

/// Check the name `icon` gives, read it through `read`, and check that it
/// parses as SVG. A pack is untrusted input, so the name may only name a file
/// directly inside the pack directory.
fn icon(file: String, read: impl FnOnce(&str) -> Result<Vec<u8>, String>) -> PackIcon {
    let svg = check_icon_name(&file).and_then(|()| read(&file)).and_then(|bytes| {
        if bytes.len() > ICON_MAX_BYTES {
            return Err(format!(
                "{file} is {} bytes, over the {ICON_MAX_BYTES}-byte limit",
                bytes.len()
            ));
        }
        usvg::Tree::from_data(&bytes, &usvg::Options::default())
            .map_err(|error| format!("{file} is not a usable SVG: {error}"))?;
        Ok(Arc::from(bytes))
    });
    PackIcon { file, svg }
}

fn check_icon_name(file: &str) -> Result<(), String> {
    let mut components = Path::new(file).components();
    // `components` reads `a/b` as two parts on every platform but `a\b` as
    // one outside Windows, so separators are refused by hand as well.
    let single = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !file.contains(['/', '\\']);
    if !single {
        return Err(format!("`icon = \"{file}\"` must name a file directly in the pack directory"));
    }
    if !file.to_ascii_lowercase().ends_with(".svg") {
        return Err(format!("`icon = \"{file}\"` must name an .svg file"));
    }
    Ok(())
}

/// [`load`], for a manifest and rule files already in memory — the built-in
/// pack, which is compiled into the binary, goes through here, so it passes
/// exactly the checks a downloaded pack does.
///
/// `rules` is in load order, each source paired with the path it came from.
/// `read_icon` is handed the file name `icon` gives, already checked to name
/// a file directly in the pack directory, and returns its bytes.
pub fn from_sources(
    manifest: &Source,
    rules: impl IntoIterator<Item = (Source, PathBuf)>,
    dir: PathBuf,
    read_icon: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Result<Pack, PackError> {
    let text = &manifest.text;
    let parsed: Manifest = toml::from_str(text).map_err(|error| {
        let span = error.span().map_or(Span::new(0, text.len()), |r| Span::new(r.start, r.end));
        Diagnostic::new(manifest, span, format!("invalid pack manifest: {}", error.message()))
            .with_label(
                "expected `name`, `version`, optional `icon` and optional `[kinds.\"name/vN\"]` tables",
            )
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

    let icon = parsed.icon.map(|file| icon(file, read_icon));
    Ok(Pack { name: parsed.name, version: parsed.version, kinds: declared, rules: loaded, dir, icon })
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
    if decl.confidence.is_some() {
        return Err(error(format!("kind `{id}` sets `confidence`"))
            .with_label("no longer a kind key")
            .with_help(CONFIDENCE_REMOVED));
    }
    Ok(Kind::new(name, version, disposition))
}

fn read(path: &Path) -> Result<String, PackError> {
    fs::read_to_string(path).map_err(|source| PackError::Io { path: path.to_path_buf(), source })
}

/// Paths go into diagnostics, so they render with forward slashes on every
/// platform and a Windows test asserts the same string a Linux one does.
fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
