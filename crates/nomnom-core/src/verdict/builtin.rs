//! The built-in pack, compiled into the binary.
//!
//! The rules live in `packs/builtin/` as ordinary `.nom` text next to an
//! ordinary `pack.toml`, so they read, diff and review as source rather than as
//! a Rust string table — and so the same directory could be handed to
//! [`nomnom_lang::load`] unchanged.
//!
//! [`nomnom_lang::load`] reads a directory at runtime, which the built-in pack
//! must not: it has to be there when the binary is alone on a machine. The
//! files are therefore listed one by one in [`FILES`], because an `include_dir`
//! over the directory would silently ship a pack missing whatever file nobody
//! registered, and a missing rule is invisible — it produces no verdict rather
//! than an error. Adding a `.nom` file without adding it here fails the build.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use nomnom_lang::diagnostic::Source;
use nomnom_lang::pack::{BUILTIN_LABELS, LoadedRule, Pack, PackError};
use nomnom_lang::parse;
use serde::Deserialize;

const MANIFEST: &str = include_str!("../../packs/builtin/pack.toml");

/// Every rule file, in load order. `docs/lang.md` makes rule order the last
/// conflict tie-break, so the order is this list rather than a readdir.
const FILES: &[(&str, &str)] = &[
    ("build-output.nom", include_str!("../../packs/builtin/rules/build-output.nom")),
    ("cache.nom", include_str!("../../packs/builtin/rules/cache.nom")),
    ("stale-download.nom", include_str!("../../packs/builtin/rules/stale-download.nom")),
];

#[derive(Deserialize)]
struct Manifest {
    name: String,
    version: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// The built-in pack, parsed once.
///
/// Panics if it does not validate. That is not a runtime failure mode: the
/// files are compiled in, so a bad one is a bug that every test run and every
/// startup hits identically, and limping on with a silently empty rule set
/// would mean the tool quietly suggests nothing.
pub fn builtin_pack() -> &'static Pack {
    static PACK: OnceLock<Pack> = OnceLock::new();
    PACK.get_or_init(|| {
        let manifest: Manifest =
            toml::from_str(MANIFEST).expect("the built-in pack.toml is compiled in and valid");
        compose(&manifest.name, &manifest.version, manifest.labels, FILES)
            .expect("the built-in pack is compiled in and valid")
    })
}

/// [`nomnom_lang::load`], for rule text that is already in memory.
///
/// Runs the same checks `load` does — every label declared, every rule name
/// unique across the pack — because a pack that skipped them would be a pack
/// the rest of the system cannot reason about. The composition lives here
/// rather than in `nomnom-lang` so that crate keeps its one entry point on a
/// real directory.
fn compose(
    name: &str,
    version: &str,
    labels: Vec<String>,
    files: &[(&str, &str)],
) -> Result<Pack, PackError> {
    let declared: Vec<&str> =
        BUILTIN_LABELS.iter().copied().chain(labels.iter().map(String::as_str)).collect();

    let mut rules = Vec::new();
    // Rule name to the file that first used it, so the duplicate message can
    // say where.
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();

    for (file_name, text) in files {
        let source = Source::new(*file_name, (*text).to_owned());
        for rule in parse(&source)? {
            let label = &rule.then.label;
            if !declared.contains(&label.value.as_str()) {
                return Err(nomnom_lang::Diagnostic::new(
                    &source,
                    label.span,
                    format!("rule uses the undeclared label `{}`", label.value),
                )
                .with_label(format!("`{}` is not declared by pack `{name}`", label.value))
                .into());
            }
            if let Some(first) = seen.get(&rule.name.value) {
                return Err(nomnom_lang::Diagnostic::new(
                    &source,
                    rule.name.span,
                    format!("duplicate rule name `{}`", rule.name.value),
                )
                .with_label(format!("already defined in {first}"))
                .with_help("a verdict cites its rule by name, so names must be unique in a pack")
                .into());
            }
            seen.insert(rule.name.value.clone(), file_name);
            rules.push(LoadedRule { rule, file: PathBuf::from(*file_name) });
        }
    }

    Ok(Pack {
        name: name.to_owned(),
        version: version.to_owned(),
        labels,
        rules,
        dir: PathBuf::from("<built-in>"),
    })
}
