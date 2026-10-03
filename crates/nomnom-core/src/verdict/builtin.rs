//! The built-in packs, compiled into the binary.
//!
//! The rules live in `packs/builtin.<domain>/` as ordinary `.nom` text next to
//! an ordinary `pack.toml`, so they read, diff and review as source rather than
//! as a Rust string table — and so each directory could be handed to
//! [`nomnom_lang::load`] unchanged. There is one pack per tool that creates the
//! files, so a verdict's provenance names that tool.
//!
//! [`nomnom_lang::load`] reads a directory at runtime, which the built-in packs
//! must not: they have to be there when the binary is alone on a machine. The
//! files are therefore listed one by one in [`PACKS`], because an `include_dir`
//! over the directory would silently ship a pack missing whatever file nobody
//! registered, and a missing rule is invisible — it produces no verdict rather
//! than an error. Adding a `.nom` file without adding it here fails the build.

use std::path::PathBuf;
use std::sync::OnceLock;

use nomnom_lang::diagnostic::Source;
use nomnom_lang::pack::{Pack, from_sources};

/// One compiled-in pack: its directory name, its manifest, and every rule file
/// in load order.
struct Embedded {
    dir: &'static str,
    manifest: &'static str,
    files: &'static [(&'static str, &'static str)],
}

macro_rules! embedded {
    ($dir:literal, [$($file:literal),+ $(,)?]) => {
        Embedded {
            dir: $dir,
            manifest: include_str!(concat!("../../packs/", $dir, "/pack.toml")),
            files: &[$(($file, include_str!(concat!("../../packs/", $dir, "/rules/", $file)))),+],
        }
    };
}

/// Every built-in pack, in resolution order.
///
/// Every rule names the one tool that made its target and matches only on
/// evidence that tool left, so no two built-in rules select the same target —
/// `tests/dsl_port.rs` holds that by judging the packs in both orders — and
/// the order decides nothing today. It is this list rather than a readdir so
/// that it is the same on every machine should two rules ever overlap.
/// `docs/lang.md` makes rule order the last tie-break, so each pack's files
/// are listed in load order too.
const PACKS: &[Embedded] = &[
    embedded!("builtin.ableton", ["ableton.nom"]),
    embedded!("builtin.after-effects", ["after-effects.nom"]),
    embedded!("builtin.bun", ["bun.nom"]),
    embedded!("builtin.cargo", ["cargo.nom"]),
    embedded!("builtin.chrome", ["chrome.nom"]),
    embedded!("builtin.cmake", ["cmake.nom"]),
    embedded!("builtin.cocoapods", ["cocoapods.nom"]),
    embedded!("builtin.cpython", ["cpython.nom"]),
    embedded!("builtin.dart", ["dart.nom"]),
    embedded!("builtin.dotnet", ["dotnet.nom"]),
    embedded!("builtin.downloads", ["stale-download.nom"]),
    embedded!("builtin.edge", ["edge.nom"]),
    embedded!("builtin.firefox", ["firefox.nom"]),
    embedded!("builtin.go", ["go.nom"]),
    embedded!("builtin.gradle", ["gradle.nom"]),
    embedded!("builtin.maven", ["maven.nom"]),
    embedded!("builtin.mypy", ["mypy.nom"]),
    embedded!("builtin.next", ["next.nom"]),
    embedded!("builtin.npm", ["npm.nom"]),
    embedded!("builtin.nuget", ["nuget.nom"]),
    embedded!("builtin.pip", ["pip.nom"]),
    embedded!("builtin.pnpm", ["pnpm.nom"]),
    embedded!("builtin.pytest", ["pytest.nom"]),
    embedded!("builtin.ruff", ["ruff.nom"]),
    embedded!("builtin.tox", ["tox.nom"]),
    embedded!("builtin.uv", ["uv.nom"]),
    embedded!("builtin.venv", ["venv.nom"]),
    embedded!("builtin.vscode", ["vscode.nom"]),
    embedded!("builtin.windows-update", ["windows-update.nom"]),
    embedded!("builtin.yarn", ["yarn.nom"]),
];

/// The built-in packs in resolution order, parsed once, through the same
/// checks a downloaded pack passes.
///
/// Panics if one does not validate. That is not a runtime failure mode: the
/// files are compiled in, so a bad one is a bug that every test run and every
/// startup hits identically, and limping on with a silently empty rule set
/// would mean the tool quietly suggests nothing.
pub fn builtin_packs() -> &'static [Pack] {
    static PARSED: OnceLock<Vec<Pack>> = OnceLock::new();
    PARSED.get_or_init(|| PACKS.iter().map(parse).collect())
}

fn parse(embedded: &Embedded) -> Pack {
    let rules =
        embedded.files.iter().map(|(name, text)| (Source::new(*name, *text), PathBuf::from(*name)));
    let pack = from_sources(
        &Source::new("pack.toml", embedded.manifest),
        rules,
        PathBuf::from("<built-in>"),
    )
    .unwrap_or_else(|error| {
        panic!("the built-in pack `{}` is compiled in and valid:\n{error}", embedded.dir)
    });
    // The directory is where a reader looks for the rules a provenance names.
    assert_eq!(pack.name, embedded.dir, "a built-in pack's `name` is its directory name");
    pack
}
