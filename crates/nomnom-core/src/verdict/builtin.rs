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

use std::path::PathBuf;
use std::sync::OnceLock;

use nomnom_lang::diagnostic::Source;
use nomnom_lang::pack::{Pack, from_sources};

const MANIFEST: &str = include_str!("../../packs/builtin/pack.toml");

/// Every rule file, in load order. `docs/lang.md` makes rule order the last
/// conflict tie-break, so the order is this list rather than a readdir.
const FILES: &[(&str, &str)] = &[
    ("build-output.nom", include_str!("../../packs/builtin/rules/build-output.nom")),
    ("cache.nom", include_str!("../../packs/builtin/rules/cache.nom")),
    ("stale-download.nom", include_str!("../../packs/builtin/rules/stale-download.nom")),
];

/// The built-in pack, parsed once, through the same checks a downloaded pack
/// passes.
///
/// Panics if it does not validate. That is not a runtime failure mode: the
/// files are compiled in, so a bad one is a bug that every test run and every
/// startup hits identically, and limping on with a silently empty rule set
/// would mean the tool quietly suggests nothing.
pub fn builtin_pack() -> &'static Pack {
    static PACK: OnceLock<Pack> = OnceLock::new();
    PACK.get_or_init(|| {
        let rules =
            FILES.iter().map(|(name, text)| (Source::new(*name, *text), PathBuf::from(*name)));
        from_sources(&Source::new("pack.toml", MANIFEST), rules, PathBuf::from("<built-in>"))
            .unwrap_or_else(|error| panic!("the built-in pack is compiled in and valid:\n{error}"))
    })
}
