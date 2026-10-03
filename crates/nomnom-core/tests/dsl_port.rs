//! Every rule in the compiled-in built-in packs, on one fixture tree.
//!
//! The regression this catches is the one a rule pack has: a filter whose
//! ownership signature stopped guarding, a confidence that drifted, or a
//! description that reads right but is not the sentence the tool actually
//! prints. So it pins the whole verdict set on exactly the fields a user sees —
//! label, disposition, confidence and the reason itself — plus the pack the
//! verdict is credited to, rather than spot-checking a rule or two.
//!
//! The fixture also holds the directories no rule may judge: a `target/`,
//! `build/`, `dist/`, `bin/`, `obj/`, `cache/`, `node_modules/` and friends with
//! no evidence of the tool that would own them. Each built-in rule names one
//! tool and matches only on a signature that tool leaves, so a name alone must
//! produce no verdict at all.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{catalog_of, write};
use nomnom_core::action::candidates;
use nomnom_core::verdict::{Disposition, TrustedPack, assess, judge};
use tempfile::TempDir;

/// What a user actually sees about one path.
#[derive(Debug, PartialEq)]
struct Row {
    pack: String,
    label: String,
    disposition: Disposition,
    confidence: f32,
    reason: String,
}

/// One file per entry, each placed so a rule's signature is present: the
/// path's directories are the target and its parents, the file name is the
/// evidence the owning tool writes.
const OWNED: &[&str] = &[
    "cargo/proj/target/.rustc_info.json",
    "cargo/proj/target/CACHEDIR.TAG",
    "cargo/home/registry/CACHEDIR.TAG",
    "cargo/home/registry/index/x",
    "cargo/home/registry/cache/x.crate",
    "cargo/home/registry/src/x.rs",
    "cargo/home/git/CACHEDIR.TAG",
    "cargo/home/git/db/x",
    "cargo/home/git/checkouts/x",
    "npm/proj/node_modules/.package-lock.json",
    "npm/npm-cache/_cacache/content-v2/x",
    "npm/npm-cache/_cacache/index-v5/x",
    "npm/npm-cache/_npx/x",
    "pnpm/proj/node_modules/.modules.yaml",
    // pnpm trees missing `.modules.yaml` (an interrupted or copied install).
    "pnpm/proj3/node_modules/.pnpm-workspace-state-v1.json",
    "pnpm/proj4/node_modules/.pnpm/lock.yaml",
    "pnpm/pnpm/store/v10/x",
    "pnpm/proj2/.pnpm-store/v3/x",
    "pnpm/pnpm-cache/metadata/x",
    "yarn/classic/node_modules/.yarn-integrity",
    "yarn/berry/node_modules/.yarn-state.yml",
    "bun/.bun/install/cache/x.npm",
    "next/proj/.next/BUILD_ID",
    "venv/p1/.venv/pyvenv.cfg",
    "venv/p2/venv/pyvenv.cfg",
    "cpython/__pycache__/m.cpython-312.pyc",
    "pip/pip/cache/http-v2/x",
    "pip/pip/cache/wheels/x",
    "pip/xdg/pip/http/x",
    "pip/xdg/pip/wheels/x",
    "uv/cache/CACHEDIR.TAG",
    "uv/cache/archive-v0/x",
    "python/.tox/CACHEDIR.TAG",
    "python/.mypy_cache/CACHEDIR.TAG",
    "python/.pytest_cache/CACHEDIR.TAG",
    "python/.ruff_cache/CACHEDIR.TAG",
    "dotnet/proj/App.csproj",
    "dotnet/proj/obj/project.assets.json",
    "dotnet/proj/bin/App.dll",
    "nuget/NuGet/v3-cache/x",
    "nuget/NuGet/plugins-cache/x",
    "nuget/.nuget/packages/x",
    "gradle/proj/.gradle/buildOutputCleanup/x",
    "gradle/home/.gradle/caches/modules-2/x",
    "gradle/home/.gradle/wrapper/dists/x",
    "maven/.m2/repository/x",
    "go/go-build/trim.txt",
    "go/go-build/README",
    "go/pkg/mod/cache/download/x",
    "cmake/proj/out/CMakeCache.txt",
    "cmake/proj/out/CMakeFiles/x",
    "cocoapods/Pods/Manifest.lock",
    "dart/.dart_tool/package_config.json",
    "chrome/Google/Chrome/User Data/Default/Cache/Cache_Data/index",
    "chrome/Google/Chrome/User Data/Default/Code Cache/js/x",
    "edge/Microsoft/Edge/User Data/Default/Cache/Cache_Data/index",
    "edge/Microsoft/Edge/User Data/Default/Code Cache/js/x",
    "firefox/Firefox/Profiles/p.default/cache2/entries/x",
    "vscode/Code/User/settings.json",
    "vscode/Code/CachedData/x",
    "vscode/Code/CachedExtensionVSIXs/x",
    "vscode/Code/Cache/x",
    "windows/SoftwareDistribution/DataStore/x",
    "windows/SoftwareDistribution/Download/x",
    "after-effects/Adobe After Effects 2024/Support Files/AfterFX.exe",
    "after-effects/Roaming/After Effects/24.5/Adobe After Effects 24.5 Prefs.txt",
    "after-effects/Roaming/Common/Media Cache Files/clip.cfa",
    "ableton/Live 12 Suite/Program/Ableton Live 12 Suite.exe",
    "ableton/Live 12 Suite/Program/Ableton Live Engine.dll",
    "ableton/Live 12 Suite/Resources/GUI.alp",
    "ableton/Live 12 Suite/Resources/Core Library/x",
    "ableton/Roaming/Live 12.4.6/Preferences/Preferences.cfg",
    "ableton/Roaming/Live 12.4.6/Preferences/Library.cfg",
    "ableton/Local/Ableton/Live Database/Live-files-12300.db",
    "ableton/Local/Ableton/Cache/Cache/Decoding/x.wav",
    "ableton/Documents/Ableton/User Library/Presets/x",
    "ableton/Documents/Ableton/Live Recordings/Temp Project/Ableton Project Info",
    // A Cargo target nested inside an npm tree: the outer verdict has to
    // swallow it rather than report it a second time.
    "npm/proj/node_modules/crate/target/.rustc_info.json",
    "npm/proj/node_modules/crate/target/CACHEDIR.TAG",
];

/// Ordinary names with no tool's signature in or beside them, each next to a
/// manifest that used to vouch for it or that a guess would lean on. None of
/// these may be judged.
const UNOWNED: &[&str] = &[
    "bare/target/data.csv",
    "bare/build/data.csv",
    "bare/dist/data.csv",
    "bare/bin/data.csv",
    "bare/obj/data.csv",
    "bare/cache/data.csv",
    "bare/.cache/data.csv",
    "bare/caches/data.csv",
    "bare/node_modules/data.csv",
    "bare/.venv/data.csv",
    "bare/venv/data.csv",
    "bare/__pycache__/notes.txt",
    "bare/.next/data.csv",
    "bare/.tox/data.csv",
    "bare/.mypy_cache/data.csv",
    "bare/Pods/data.csv",
    "manifests/Cargo.toml",
    "manifests/package.json",
    "manifests/pyproject.toml",
    "manifests/CMakeLists.txt",
    "manifests/App.sln",
    "manifests/target/data.csv",
    "manifests/build/data.csv",
    "manifests/dist/data.csv",
    "manifests/obj/data.csv",
    // An in-source CMake build: the cache sits in the source tree itself.
    "cmake-in-source/CMakeLists.txt",
    "cmake-in-source/CMakeCache.txt",
    "cmake-in-source/CMakeFiles/x",
    "cmake-in-source/main.c",
    // A pyvenv.cfg rooting a tool's environment under a name no rule takes.
    "pipx/venvs/black/pyvenv.cfg",
    // Yarn 1's cache version directory: no official doc states this path or
    // its `v6` name, and the dynamically-named package subfolders beneath it
    // leave no fixed child a `has` filter could pin, so the rule was dropped.
    "yarn/Yarn/Cache/v6/x",
    // An app's embedded WebView2 profile under a folder that happens to be
    // named `Edge`: Chromium's layout, but not Edge's to clear.
    "FL Studio/Settings/Edge/EBWebView/Default/Code Cache/js/x",
    "FL Studio/Settings/Edge/EBWebView/Default/Cache/Cache_Data/index",
    // Folders named like After Effects' and Live's, with none of the files
    // those apps write.
    "lookalike/After Effects 2024/Support Files/readme.txt",
    "lookalike/After Effects/24.5/Prefs.txt",
    "lookalike/Media Cache Files/clip.mov",
    "lookalike/Live 12 Suite/Program/Ableton Live Engine.dll",
    "lookalike/Live 12 Suite/Resources/Core Library/x",
    "lookalike/Live 12.4.6/Preferences/Preferences.cfg",
    "lookalike/Cache/Cache/Decoding/x.wav",
    "lookalike/User Library/Presets/x",
];

fn fixture() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    for file in OWNED.iter().chain(UNOWNED) {
        write(root.join(file), b"x");
    }
    tmp
}

fn rows(tmp: &TempDir) -> BTreeMap<String, Row> {
    let catalog = catalog_of(tmp.path());
    judge(&catalog, &TrustedPack::builtins())
        .into_iter()
        .map(|(id, verdict)| {
            let path = catalog.path(id);
            let relative = path.strip_prefix(tmp.path()).expect("under root");
            (
                relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
                Row {
                    pack: verdict.provenance.pack.clone(),
                    label: verdict.label.as_str().to_owned(),
                    disposition: verdict.disposition,
                    confidence: verdict.confidence,
                    reason: verdict.reason,
                },
            )
        })
        .collect()
}

use Disposition::{Keep, Reclaimable, Review};

/// Path, pack, label, disposition, confidence, reason — in path order.
#[rustfmt::skip]
const EXPECTED: &[(&str, &str, &str, Disposition, f32, &str)] = &[
    ("ableton/Documents/Ableton/Live Recordings/Temp Project", "builtin.ableton", "user-content", Keep, 0.91, "an Ableton Live Project — it holds Live's `Ableton Project Info`"),
    ("ableton/Documents/Ableton/User Library", "builtin.ableton", "user-content", Keep, 0.98, "your Ableton User Library: your own presets, samples, clips and templates"),
    ("ableton/Live 12 Suite/Program", "builtin.ableton", "application", Keep, 0.96, "the Ableton Live application — it holds `Ableton Live 12 Suite.exe`; uninstall it from Windows Settings"),
    ("ableton/Live 12 Suite/Resources", "builtin.ableton", "application", Keep, 0.94, "Ableton Live's bundled resources and Core Library — it holds Live's `GUI.alp`"),
    ("ableton/Local/Ableton/Cache/Cache/Decoding", "builtin.ableton", "cache", Review, 0.65, "Ableton Live's decoding cache; Live decodes compressed samples again when a Set loads them (close Live first)"),
    ("ableton/Local/Ableton/Live Database", "builtin.ableton", "settings", Keep, 0.88, "Ableton Live's browser database — it holds `Live-files-12300.db`"),
    ("ableton/Roaming/Live 12.4.6/Preferences", "builtin.ableton", "settings", Keep, 0.92, "Ableton Live's preferences for one version — it holds Live's `Preferences.cfg` and `Library.cfg`"),
    ("after-effects/Adobe After Effects 2024/Support Files", "builtin.after-effects", "application", Keep, 0.97, "the After Effects application — it holds `AfterFX.exe`; uninstall it through Creative Cloud"),
    ("after-effects/Roaming/After Effects/24.5", "builtin.after-effects", "settings", Keep, 0.93, "After Effects preferences, presets and scripts for one version — it holds `Adobe After Effects 24.5 Prefs.txt`"),
    ("after-effects/Roaming/Common/Media Cache Files", "builtin.after-effects", "cache", Review, 0.55, "Adobe's conformed-media cache, shared by After Effects, Premiere Pro and Media Encoder — it holds `clip.cfa`; they conform the media again on the next import (Clean Database & Cache in their preferences does this safely)"),
    ("bun/.bun/install/cache", "builtin.bun", "cache", Reclaimable, 0.7, "Bun's package install cache; Bun re-downloads packages on the next install"),
    ("cargo/home/git/checkouts", "builtin.cargo", "cache", Reclaimable, 0.8, "Cargo's working copies of git dependencies; Cargo checks them out again from `git/db` on the next build"),
    ("cargo/home/registry/cache", "builtin.cargo", "cache", Reclaimable, 0.8, "Cargo's downloaded `.crate` archives; Cargo re-downloads them on the next build that needs them"),
    ("cargo/home/registry/src", "builtin.cargo", "cache", Reclaimable, 0.8, "Cargo's unpacked crate sources; Cargo re-extracts them from its download cache on the next build"),
    ("cargo/proj/target", "builtin.cargo", "build-output", Reclaimable, 0.95, "regenerable: Cargo build output, rebuilt by `cargo build` — it holds Cargo's `.rustc_info.json` and `CACHEDIR.TAG`"),
    ("chrome/Google/Chrome/User Data/Default/Cache/Cache_Data", "builtin.chrome", "cache", Reclaimable, 0.8, "Chrome's HTTP cache; Chrome downloads pages again as you browse"),
    ("chrome/Google/Chrome/User Data/Default/Code Cache", "builtin.chrome", "cache", Reclaimable, 0.8, "Chrome's compiled-script cache; Chrome recompiles scripts as you browse"),
    ("cmake/proj/out", "builtin.cmake", "build-output", Reclaimable, 0.9, "regenerable: CMake build directory, rebuilt by configuring and building again — it holds CMake's `CMakeCache.txt`"),
    ("cocoapods/Pods", "builtin.cocoapods", "build-output", Reclaimable, 0.9, "regenerable: CocoaPods dependencies, rebuilt by `pod install` — it holds CocoaPods' `Manifest.lock`"),
    ("cpython/__pycache__", "builtin.cpython", "build-output", Reclaimable, 0.95, "regenerable: Python bytecode cache, rewritten on the next import — it holds `m.cpython-312.pyc`"),
    ("dart/.dart_tool", "builtin.dart", "build-output", Reclaimable, 0.9, "regenerable: Dart tool state, rebuilt by `dart pub get` — it holds pub's `package_config.json`"),
    ("dotnet/proj/bin", "builtin.dotnet", "build-output", Reclaimable, 0.85, "regenerable: .NET build output, rebuilt by `dotnet build` — `App.csproj` sits beside it"),
    ("dotnet/proj/obj", "builtin.dotnet", "build-output", Reclaimable, 0.9, "regenerable: .NET intermediate build output, rebuilt by `dotnet build` — it holds the SDK's `project.assets.json`"),
    ("edge/Microsoft/Edge/User Data/Default/Cache/Cache_Data", "builtin.edge", "cache", Reclaimable, 0.8, "Edge's HTTP cache; Edge downloads pages again as you browse"),
    ("edge/Microsoft/Edge/User Data/Default/Code Cache", "builtin.edge", "cache", Reclaimable, 0.8, "Edge's compiled-script cache; Edge recompiles scripts as you browse"),
    ("firefox/Firefox/Profiles/p.default/cache2", "builtin.firefox", "cache", Reclaimable, 0.8, "Firefox's HTTP cache; Firefox downloads pages again as you browse"),
    ("go/go-build", "builtin.go", "cache", Reclaimable, 0.85, "Go's build cache; `go build` recompiles on demand (`go clean -cache` empties it the same way)"),
    ("go/pkg/mod/cache/download", "builtin.go", "cache", Reclaimable, 0.7, "Go's module download cache; Go re-downloads modules from the module proxy on demand"),
    ("gradle/home/.gradle/caches", "builtin.gradle", "cache", Reclaimable, 0.8, "Gradle's dependency and transform caches; Gradle re-downloads and recomputes on the next build"),
    ("gradle/home/.gradle/wrapper/dists", "builtin.gradle", "cache", Review, 0.7, "Gradle distributions downloaded by the Gradle wrapper; the wrapper downloads its version again on the next build"),
    ("gradle/proj/.gradle", "builtin.gradle", "build-output", Reclaimable, 0.95, "regenerable: Gradle project cache, rebuilt on the next Gradle run — it holds Gradle's `buildOutputCleanup`"),
    ("maven/.m2/repository", "builtin.maven", "cache", Review, 0.6, "Maven's local repository; Maven re-downloads what a build needs, but locally installed artifacts exist nowhere else"),
    ("next/proj/.next", "builtin.next", "build-output", Reclaimable, 0.95, "regenerable: Next.js build output, rebuilt by `next build` — it holds Next.js's `BUILD_ID`"),
    ("npm/npm-cache/_cacache", "builtin.npm", "cache", Reclaimable, 0.8, "npm's download cache (`_cacache`); npm re-downloads packages on demand"),
    ("npm/npm-cache/_npx", "builtin.npm", "cache", Reclaimable, 0.8, "packages `npx` installed to run once; npx installs them again on the next run"),
    ("npm/proj/node_modules", "builtin.npm", "build-output", Reclaimable, 0.95, "regenerable: npm dependency tree, rebuilt by `npm install` — it holds npm's hidden lockfile `.package-lock.json`"),
    ("nuget/.nuget/packages", "builtin.nuget", "cache", Review, 0.7, "NuGet's global packages folder; `dotnet restore` re-downloads what is missing, but a package from a feed you no longer reach cannot be fetched again"),
    ("nuget/NuGet/plugins-cache", "builtin.nuget", "cache", Reclaimable, 0.8, "NuGet's credential-plugin cache; NuGet rebuilds it on the next restore"),
    ("nuget/NuGet/v3-cache", "builtin.nuget", "cache", Reclaimable, 0.8, "NuGet's HTTP cache (`v3-cache`); NuGet downloads again on the next restore"),
    ("pip/pip/cache", "builtin.pip", "cache", Reclaimable, 0.8, "pip's download and wheel cache; pip downloads or rebuilds on the next install"),
    ("pip/xdg/pip", "builtin.pip", "cache", Reclaimable, 0.8, "pip's download and wheel cache; pip downloads or rebuilds on the next install"),
    ("pnpm/pnpm-cache", "builtin.pnpm", "cache", Reclaimable, 0.8, "pnpm's registry metadata cache; pnpm fetches the metadata again on the next install"),
    ("pnpm/pnpm/store", "builtin.pnpm", "cache", Review, 0.7, "pnpm's content-addressable package store; the next `pnpm install` re-downloads what is missing, but a package unpublished from the registry cannot be fetched again"),
    ("pnpm/proj/node_modules", "builtin.pnpm", "build-output", Reclaimable, 0.95, "regenerable: pnpm dependency tree, rebuilt by `pnpm install` — it holds pnpm's `.modules.yaml`"),
    ("pnpm/proj3/node_modules", "builtin.pnpm", "build-output", Reclaimable, 0.95, "regenerable: pnpm dependency tree, rebuilt by `pnpm install` — it holds pnpm's `.pnpm-workspace-state-v1.json`"),
    ("pnpm/proj4/node_modules", "builtin.pnpm", "build-output", Reclaimable, 0.95, "regenerable: pnpm dependency tree, rebuilt by `pnpm install` — it holds pnpm's `.pnpm`"),
    ("pnpm/proj2/.pnpm-store", "builtin.pnpm", "cache", Review, 0.7, "a pnpm package store kept beside a project; the next `pnpm install` re-downloads what is missing, but a package unpublished from the registry cannot be fetched again"),
    ("python/.mypy_cache", "builtin.mypy", "build-output", Reclaimable, 0.95, "regenerable: mypy incremental cache, rebuilt on the next type-check — it holds mypy's `CACHEDIR.TAG`"),
    ("python/.pytest_cache", "builtin.pytest", "build-output", Reclaimable, 0.95, "regenerable: pytest run cache, rewritten on the next test run — it holds pytest's `CACHEDIR.TAG`"),
    ("python/.ruff_cache", "builtin.ruff", "build-output", Reclaimable, 0.95, "regenerable: Ruff lint cache, rewritten on the next `ruff check` — it holds Ruff's `CACHEDIR.TAG`"),
    ("python/.tox", "builtin.tox", "build-output", Reclaimable, 0.95, "regenerable: tox environments, rebuilt by `tox` — it holds tox's `CACHEDIR.TAG`"),
    ("uv/cache", "builtin.uv", "cache", Reclaimable, 0.8, "uv's package cache; uv re-downloads and re-unpacks on the next sync"),
    ("venv/p1/.venv", "builtin.venv", "build-output", Reclaimable, 0.9, "regenerable: Python virtualenv, rebuilt by `python -m venv` plus a reinstall — it holds `pyvenv.cfg`"),
    ("venv/p2/venv", "builtin.venv", "build-output", Reclaimable, 0.9, "regenerable: Python virtualenv, rebuilt by `python -m venv` plus a reinstall — it holds `pyvenv.cfg`"),
    ("vscode/Code/Cache", "builtin.vscode", "cache", Reclaimable, 0.8, "VS Code's HTTP cache; VS Code downloads again on demand"),
    ("vscode/Code/CachedData", "builtin.vscode", "cache", Reclaimable, 0.8, "VS Code's V8 code cache, one directory per VS Code build; VS Code rebuilds it on start"),
    ("vscode/Code/CachedExtensionVSIXs", "builtin.vscode", "cache", Reclaimable, 0.8, "extension packages VS Code downloaded; VS Code downloads them again when an extension is installed or updated"),
    ("windows/SoftwareDistribution/Download", "builtin.windows-update", "cache", Review, 0.6, "Windows Update's download cache; Windows Update downloads again what it still needs (stop the Windows Update service first)"),
    ("yarn/berry/node_modules", "builtin.yarn", "build-output", Reclaimable, 0.95, "regenerable: Yarn dependency tree, rebuilt by `yarn install` — it holds Yarn's `.yarn-state.yml`"),
    ("yarn/classic/node_modules", "builtin.yarn", "build-output", Reclaimable, 0.95, "regenerable: Yarn dependency tree, rebuilt by `yarn install` — it holds Yarn's `.yarn-integrity`"),
];

#[test]
fn the_built_in_packs_judge_the_fixture_exactly_as_written() {
    let tmp = fixture();
    let mut actual = rows(&tmp);

    for (path, pack, label, disposition, confidence, reason) in EXPECTED {
        let want = Row {
            pack: (*pack).into(),
            label: (*label).into(),
            disposition: *disposition,
            confidence: *confidence,
            reason: (*reason).into(),
        };
        assert_eq!(actual.remove(*path).as_ref(), Some(&want), "{path}");
    }

    assert!(actual.is_empty(), "the packs judged paths the fixture does not expect: {actual:#?}");
}

/// The regression: an edit to `builtin.ableton` or `builtin.after-effects`
/// (a kind swapped, a disposition dropped) that makes a Live Project, the User
/// Library, an installed app or its preferences offerable for deletion. Only
/// the two documented caches may ever be offered, and only as review.
#[test]
fn recognized_app_files_and_user_content_are_never_offered_for_removal() {
    let tmp = fixture();
    let assessment = assess(&catalog_of(tmp.path()), TrustedPack::builtins());
    let mut offerable: Vec<String> = candidates(&assessment, true)
        .into_iter()
        .filter(|entry| {
            ["builtin.ableton", "builtin.after-effects"].contains(&entry.verdict.provenance.pack.as_str())
        })
        .map(|entry| {
            let path = std::path::Path::new(&entry.path);
            let relative = path.strip_prefix(tmp.path()).expect("under root");
            relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect();
    offerable.sort();
    assert_eq!(
        offerable,
        ["ableton/Local/Ableton/Cache/Cache/Decoding", "after-effects/Roaming/Common/Media Cache Files"],
    );
}

/// The regression: a directory judged on its name alone. Every `UNOWNED`
/// entry is somebody's data as far as any tool's evidence goes, so nothing at
/// or above it inside the fixture may carry a verdict.
#[test]
fn a_name_without_its_tool_s_signature_is_never_judged() {
    let tmp = fixture();
    let judged = rows(&tmp);
    for file in UNOWNED {
        let mut prefix = String::new();
        for part in file.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            assert!(!judged.contains_key(&prefix), "`{prefix}` was judged with no tool evidence");
        }
    }
}

/// "Later pack wins" on an equal confidence, so a built-in rule that overlaps
/// another built-in pack's rule makes verdicts depend on the order of the
/// `PACKS` list. The regression: a new rule that claims a target another
/// pack's rule already claims — say a `node_modules` holding both npm's and
/// pnpm's marker — with the winner decided by list position rather than by
/// anyone's intent. The crowded tree puts every signature inside and beside
/// every name a rule targets, which is where such an overlap would surface.
#[test]
fn the_built_in_packs_judge_the_same_in_either_order() {
    let tmp = fixture();
    let crowded = tmp.path().join("crowded");
    let signatures: BTreeSet<&str> =
        OWNED.iter().filter_map(|file| file.rsplit('/').next()).collect();
    let names: BTreeSet<&str> =
        OWNED.iter().flat_map(|file| file.split('/').rev().skip(1)).collect();
    for (index, name) in names.iter().enumerate() {
        let parent = crowded.join(format!("p{index}"));
        // A file may not share a name with the directory beside it.
        for signature in signatures.iter().filter(|signature| *signature != name) {
            write(parent.join(signature), b"x");
            write(parent.join(name).join(signature), b"x");
        }
    }
    let catalog = catalog_of(tmp.path());

    let forward = TrustedPack::builtins();
    let mut reversed = forward.clone();
    reversed.reverse();

    assert_eq!(judge(&catalog, &forward), judge(&catalog, &reversed));
}
