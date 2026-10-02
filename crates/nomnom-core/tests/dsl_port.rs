//! Every rule in the compiled-in built-in pack, on one fixture tree.
//!
//! The regression this catches is the one a rule pack has: a `when` whose
//! corroboration guard stopped guarding, a confidence that drifted, or a
//! `reason` that reads right but is not the sentence the tool actually prints.
//! So it pins the whole verdict set on exactly the fields a user sees — label,
//! disposition, confidence and the reason itself — rather than spot-checking a
//! rule or two.
//!
//! This table was first proven equal, path for path, to the hand-written Rust
//! judge the pack replaced. Two wordings were allowed to differ, both because
//! the language has a literal where the Rust had a match:
//!
//! 1. `bin`/`obj` corroborate with `sibling_matches("*.csproj")`, and a reason
//!    can interpolate a field but not the name of whatever the glob matched, so
//!    the sentence names the pattern where the Rust named the file.
//! 2. The `cache` reason quoted the matched constant; one rule per cache name
//!    puts the same literal in the sentence, so this one turned out to be no
//!    difference at all.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{catalog_of, write};
use nomnom_core::verdict::{Disposition, DslJudge, Label, assess_all};
use tempfile::TempDir;

/// What a user actually sees about one path.
#[derive(Debug, PartialEq)]
struct Row {
    label: String,
    disposition: Disposition,
    confidence: f32,
    reason: String,
}

/// Every rule in the built-in pack, on one tree.
fn fixture() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();

    // Unambiguous build-output names, one each.
    for name in [
        "node_modules",
        ".venv",
        "venv",
        "__pycache__",
        ".next",
        ".gradle",
        ".tox",
        ".mypy_cache",
        ".pytest_cache",
    ] {
        write(root.join("unambiguous").join(name).join("payload.bin"), b"x");
    }

    // Generic names, corroborated: one directory per (name, sibling) pair, each
    // in its own parent so the corroborating files cannot cross over.
    let corroborated: &[(&str, &str)] = &[
        ("target", "Cargo.toml"),
        ("build", "package.json"),
        ("build", "pyproject.toml"),
        ("build", "CMakeLists.txt"),
        ("dist", "package.json"),
        ("dist", "pyproject.toml"),
        ("dist", "CMakeLists.txt"),
        ("bin", "App.csproj"),
        ("bin", "App.sln"),
        ("obj", "App.csproj"),
        ("obj", "App.sln"),
    ];
    for (index, (dir, sibling)) in corroborated.iter().enumerate() {
        let parent = root.join("corroborated").join(format!("p{index}"));
        write(parent.join(sibling), b"{}");
        write(parent.join(dir).join("out.bin"), b"built");
    }

    // Generic names with nothing beside them.
    for (index, dir) in ["target", "build", "dist", "bin", "obj"].iter().enumerate() {
        let parent = root.join("bare").join(format!("p{index}"));
        write(parent.join("notes.txt"), b"my data");
        write(parent.join(dir).join("data.csv"), b"1,2,3");
    }

    // Cache directories, each with a different file count so the rendered
    // counts in the reason are distinguishable. The container is not itself
    // named like a cache, or it would swallow the three as one unit.
    for (index, name) in [".cache", "cache", "caches"].iter().enumerate() {
        let dir = root.join("cache-fixtures").join(format!("p{index}")).join(name);
        for file in 0..=index {
            write(dir.join(format!("blob{file}.bin")), b"cached bytes");
        }
    }

    // A stale download, both timestamps pushed back past the 90-day gate. Only
    // the by-access rule can fire here: the by-mtime rule wants a file the
    // platform reports no atime for, which no fixture can manufacture on a
    // filesystem that records one.
    let download = root.join("Downloads").join("installer.iso");
    write(&download, b"a downloaded thing");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(200 * 24 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&download)
        .expect("reopen download")
        .set_times(std::fs::FileTimes::new().set_accessed(old).set_modified(old))
        .expect("set timestamps");

    tmp
}

fn rows(tmp: &TempDir) -> BTreeMap<PathBuf, Row> {
    let catalog = catalog_of(tmp.path());
    let judge = DslJudge::new(&catalog);
    assess_all(&judge, &catalog)
        .into_iter()
        // Duplicates are still decided in Rust, not by a rule, and this fixture
        // has no duplicate pair to decide about.
        .filter(|(_, verdict)| verdict.label != Label::DUPLICATE)
        .map(|(id, verdict)| {
            (
                catalog.path(id).strip_prefix(tmp.path()).expect("under root").to_path_buf(),
                Row {
                    label: verdict.label.as_str().to_owned(),
                    disposition: verdict.disposition,
                    confidence: verdict.confidence,
                    reason: verdict.reason,
                },
            )
        })
        .collect()
}

/// Path, label, disposition, confidence, reason — in path order.
const EXPECTED: &[(&str, &str, Disposition, f32, &str)] = &[
    (
        "Downloads/installer.iso",
        "stale-download",
        Disposition::Review,
        0.5,
        "in Downloads, last opened 200 days ago, 18 bytes; may still be the only copy",
    ),
    (
        "bare/p0/target",
        "build-output",
        Disposition::Review,
        0.35,
        "named `target`, but no Cargo.toml beside it — `target` is also an ordinary directory name, so this may be your data rather than build output",
    ),
    (
        "bare/p1/build",
        "build-output",
        Disposition::Review,
        0.35,
        "named `build`, but no package.json or pyproject.toml or CMakeLists.txt beside it — `build` is also an ordinary directory name, so this may be your data rather than build output",
    ),
    (
        "bare/p2/dist",
        "build-output",
        Disposition::Review,
        0.35,
        "named `dist`, but no package.json or pyproject.toml or CMakeLists.txt beside it — `dist` is also an ordinary directory name, so this may be your data rather than build output",
    ),
    (
        "bare/p3/bin",
        "build-output",
        Disposition::Review,
        0.35,
        "named `bin`, but no *.csproj or *.sln beside it — `bin` is also an ordinary directory name, so this may be your data rather than build output",
    ),
    (
        "bare/p4/obj",
        "build-output",
        Disposition::Review,
        0.35,
        "named `obj`, but no *.csproj or *.sln beside it — `obj` is also an ordinary directory name, so this may be your data rather than build output",
    ),
    (
        "cache-fixtures/p0/.cache",
        "cache",
        Disposition::Reclaimable,
        0.6,
        "cache directory `.cache`: 12 bytes across 1 files, refilled on next use",
    ),
    (
        "cache-fixtures/p1/cache",
        "cache",
        Disposition::Reclaimable,
        0.6,
        "cache directory `cache`: 24 bytes across 2 files, refilled on next use",
    ),
    (
        "cache-fixtures/p2/caches",
        "cache",
        Disposition::Reclaimable,
        0.6,
        "cache directory `caches`: 36 bytes across 3 files, refilled on next use",
    ),
    (
        "corroborated/p0/target",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: Cargo build output, rebuilt by `cargo build` — `Cargo.toml` sits beside it",
    ),
    (
        "corroborated/p1/build",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `package.json` sits beside it",
    ),
    (
        "corroborated/p10/obj",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: .NET build output, rebuilt by `dotnet build` — `*.sln` sits beside it",
    ),
    (
        "corroborated/p2/build",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `pyproject.toml` sits beside it",
    ),
    (
        "corroborated/p3/build",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `CMakeLists.txt` sits beside it",
    ),
    (
        "corroborated/p4/dist",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `package.json` sits beside it",
    ),
    (
        "corroborated/p5/dist",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `pyproject.toml` sits beside it",
    ),
    (
        "corroborated/p6/dist",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: build output, rebuilt by the project's build command — `CMakeLists.txt` sits beside it",
    ),
    (
        "corroborated/p7/bin",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: .NET build output, rebuilt by `dotnet build` — `*.csproj` sits beside it",
    ),
    (
        "corroborated/p8/bin",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: .NET build output, rebuilt by `dotnet build` — `*.sln` sits beside it",
    ),
    (
        "corroborated/p9/obj",
        "build-output",
        Disposition::Reclaimable,
        0.9,
        "regenerable: .NET build output, rebuilt by `dotnet build` — `*.csproj` sits beside it",
    ),
    (
        "unambiguous/.gradle",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: Gradle project cache, rebuilt on the next Gradle run",
    ),
    (
        "unambiguous/.mypy_cache",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: mypy incremental cache, rebuilt on the next type-check",
    ),
    (
        "unambiguous/.next",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: Next.js build output, rebuilt by `next build`",
    ),
    (
        "unambiguous/.pytest_cache",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: pytest run cache, rewritten on the next test run",
    ),
    (
        "unambiguous/.tox",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: tox environments, rebuilt by `tox`",
    ),
    (
        "unambiguous/.venv",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: Python virtualenv, rebuilt by `python -m venv` plus a reinstall",
    ),
    (
        "unambiguous/__pycache__",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: Python bytecode cache, rewritten on the next import",
    ),
    (
        "unambiguous/node_modules",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: npm dependency tree, rebuilt by `npm install`",
    ),
    (
        "unambiguous/venv",
        "build-output",
        Disposition::Reclaimable,
        0.95,
        "regenerable: Python virtualenv, rebuilt by `python -m venv` plus a reinstall",
    ),
];

#[test]
fn the_built_in_pack_judges_the_fixture_exactly_as_written() {
    let tmp = fixture();
    let mut actual = rows(&tmp);

    for (path, label, disposition, confidence, reason) in EXPECTED {
        let want = Row {
            label: (*label).into(),
            disposition: *disposition,
            confidence: *confidence,
            reason: (*reason).into(),
        };
        let got = actual.remove(&PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR)));
        assert_eq!(got.as_ref(), Some(&want), "{path}");
    }

    assert!(actual.is_empty(), "the pack judged paths the fixture does not expect: {actual:?}");
}
