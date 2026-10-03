//! The built-in packs, compiled into the binary.
//!
//! The rules live in `packs/builtin.<domain>/` as ordinary `.toml` rule files next to
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
//! than an error. Adding a rule file without adding it here fails the build.

use std::path::PathBuf;
use std::sync::OnceLock;

use nomnom_lang::diagnostic::Source;
use nomnom_lang::pack::{Pack, from_sources};

/// One compiled-in pack: its directory name, its manifest, and every rule file
/// in load order.
struct Embedded {
    dir: &'static str,
    manifest: &'static str,
    /// `icon.svg`, which every built-in pack has and names in its manifest.
    icon: &'static [u8],
    files: &'static [(&'static str, &'static str)],
}

macro_rules! embedded {
    ($dir:literal, [$($file:literal),+ $(,)?]) => {
        Embedded {
            dir: $dir,
            manifest: include_str!(concat!("../../packs/", $dir, "/pack.toml")),
            icon: include_bytes!(concat!("../../packs/", $dir, "/icon.svg")),
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
    embedded!("builtin.ableton", ["ableton.toml"]),
    embedded!("builtin.after-effects", ["after-effects.toml"]),
    embedded!("builtin.bun", ["bun.toml"]),
    embedded!("builtin.cargo", ["cargo.toml"]),
    embedded!("builtin.chrome", ["chrome.toml"]),
    embedded!("builtin.cmake", ["cmake.toml"]),
    embedded!("builtin.cocoapods", ["cocoapods.toml"]),
    embedded!("builtin.cpython", ["cpython.toml"]),
    embedded!("builtin.dart", ["dart.toml"]),
    embedded!("builtin.dotnet", ["dotnet.toml"]),
    embedded!("builtin.downloads", ["stale-download.toml"]),
    embedded!("builtin.edge", ["edge.toml"]),
    embedded!("builtin.firefox", ["firefox.toml"]),
    embedded!("builtin.go", ["go.toml"]),
    embedded!("builtin.gradle", ["gradle.toml"]),
    embedded!("builtin.maven", ["maven.toml"]),
    embedded!("builtin.mypy", ["mypy.toml"]),
    embedded!("builtin.next", ["next.toml"]),
    embedded!("builtin.npm", ["npm.toml"]),
    embedded!("builtin.nuget", ["nuget.toml"]),
    embedded!("builtin.pip", ["pip.toml"]),
    embedded!("builtin.pnpm", ["pnpm.toml"]),
    embedded!("builtin.pytest", ["pytest.toml"]),
    embedded!("builtin.ruff", ["ruff.toml"]),
    embedded!("builtin.tox", ["tox.toml"]),
    embedded!("builtin.uv", ["uv.toml"]),
    embedded!("builtin.venv", ["venv.toml"]),
    embedded!("builtin.vscode", ["vscode.toml"]),
    embedded!("builtin.windows-update", ["windows-update.toml"]),
    embedded!("builtin.yarn", ["yarn.toml"]),
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
        |file| match file {
            "icon.svg" => Ok(embedded.icon.to_vec()),
            other => Err(format!("{other} is not compiled in; a built-in pack's icon is icon.svg")),
        },
    )
    .unwrap_or_else(|error| {
        panic!("the built-in pack `{}` is compiled in and valid:\n{error}", embedded.dir)
    });
    // The directory is where a reader looks for the rules a provenance names.
    assert_eq!(pack.name, embedded.dir, "a built-in pack's `name` is its directory name");
    pack
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches a built-in pack shipping without an icon, or with one that
    // no longer parses, which would otherwise only show as a fallback glyph.
    #[test]
    fn every_builtin_pack_has_an_icon_that_parses() {
        for pack in builtin_packs() {
            let icon = pack.icon.as_ref().unwrap_or_else(|| panic!("`{}` names no icon", pack.name));
            if let Err(why) = &icon.svg {
                panic!("`{}`'s icon does not load: {why}", pack.name);
            }
        }
    }

    // Catches an icon added without recording where it came from and under
    // which license, which a later licensing review could not reconstruct.
    #[test]
    fn every_builtin_icon_records_its_source_and_license() {
        for embedded in PACKS {
            let line = |key: &str| {
                embedded.manifest.lines().find_map(|line| line.strip_prefix(key)).map(str::trim)
            };
            let source = line("# icon source:")
                .unwrap_or_else(|| panic!("`{}` has no `# icon source:` line", embedded.dir));
            assert!(source.contains("https://"), "`{}`'s icon source has no URL", embedded.dir);
            let license = line("# icon license:")
                .unwrap_or_else(|| panic!("`{}` has no `# icon license:` line", embedded.dir));
            assert!(
                ["CC0-1.0", "ISC", "MIT", "Apache-2.0"].contains(&license),
                "`{}`'s icon license `{license}` is not one known to be permissive",
                embedded.dir
            );
        }
    }
}
