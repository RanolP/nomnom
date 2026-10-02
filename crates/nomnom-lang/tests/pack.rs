//! Loading a pack directory off disk.

use std::fs;
use std::path::Path;

use nomnom_lang::pack::{PackError, load};
use nomnom_lang::{Disposition, Source, parse};
use tempfile::TempDir;

fn rule_text(name: &str, label: &str) -> String {
    format!(
        "rule \"{name}\" {{\n  when is_dir and child(\"marker\")\n  \
         then label       = {label}\n       disposition = review\n       \
         confidence  = 0.5\n       reason      = \"a `marker` file sits inside it\"\n}}\n"
    )
}

fn pack_dir(manifest: &str, files: &[(&str, String)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("pack.toml"), manifest).expect("write pack.toml");
    fs::create_dir_all(dir.path().join("rules")).expect("rules dir");
    for (name, text) in files {
        fs::write(dir.path().join("rules").join(name), text).expect("write rule file");
    }
    dir
}

fn diagnostic_of(error: PackError) -> String {
    match error {
        PackError::Source(diagnostic) => diagnostic.to_string(),
        other => panic!("expected a source diagnostic, got {other}"),
    }
}

/// The whole-directory happy path: manifest fields land, both `.nom` files are
/// read, and rules come back in a deterministic order. Rule order is the last
/// conflict tie-break in `docs/lang.md`, so a readdir-order-dependent load
/// would make verdicts differ between machines.
#[test]
fn a_pack_directory_loads_every_rule_file_in_a_stable_order() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.2.0\"\nlabels = [\"toolchain-cache\"]\n",
        &[
            ("b-second.nom", rule_text("second", "toolchain-cache")),
            ("a-first.nom", rule_text("first", "build-output")),
        ],
    );

    let pack = load(dir.path()).expect("a valid pack");
    assert_eq!(pack.name, "rust");
    assert_eq!(pack.version, "0.2.0");
    assert_eq!(pack.labels, ["toolchain-cache"]);

    let names: Vec<&str> = pack.rules.iter().map(|r| r.rule.name.value.as_str()).collect();
    assert_eq!(names, ["first", "second"], "sorted by file name, not by readdir order");
    assert_eq!(pack.rules[0].rule.then.disposition.value, Disposition::Review);
    assert!(pack.rules[0].file.ends_with("a-first.nom"), "provenance points at the source file");
}

/// Catches a pack silently introducing a label the rest of the system has
/// never heard of: nothing downstream could render or group it, so the failure
/// would surface as a missing row rather than an error.
#[test]
fn a_rule_using_an_undeclared_label_fails_and_names_the_label() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("one.nom", rule_text("only", "mystery-label"))],
    );

    let rendered = diagnostic_of(load(dir.path()).expect_err("undeclared label"));
    assert!(rendered.contains("undeclared label `mystery-label`"), "{rendered}");
    // Thirteen carets, one per character of `mystery-label`.
    assert!(rendered.contains("^^^^^^^^^^^^^ `mystery-label` is not declared"), "{rendered}");
    assert!(rendered.contains("add it to `labels` in pack.toml"), "{rendered}");
}

/// The four built-in labels need no declaration; requiring them would make
/// every pack repeat the core vocabulary.
#[test]
fn built_in_labels_need_no_declaration() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("one.nom", rule_text("only", "stale-download"))],
    );
    assert_eq!(load(dir.path()).expect("built-ins are always available").rules.len(), 1);
}

/// A verdict cites its rule by name, so two rules sharing one makes "why does
/// nomnom want to delete this" unanswerable.
#[test]
fn duplicate_rule_names_across_files_are_rejected() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("a.nom", rule_text("same", "cache")), ("b.nom", rule_text("same", "build-output"))],
    );

    let rendered = diagnostic_of(load(dir.path()).expect_err("duplicate rule name"));
    assert!(rendered.contains("duplicate rule name `same`"), "{rendered}");
    assert!(rendered.contains("already defined in"), "{rendered}");
    assert!(rendered.contains("a.nom"), "the message names the first definition: {rendered}");
}

/// A parse error inside a pack must keep its file name, or the author cannot
/// tell which of ten rule files to open.
#[test]
fn a_broken_rule_file_reports_its_own_path() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("broken.nom", "rule \"t\" { when is_file then label = cache }".into())],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("incomplete `then`"));
    assert!(rendered.contains("broken.nom:1:"), "{rendered}");
}

/// A missing `pack.toml` is an io failure, not a diagnostic: there is no
/// source text to point a caret at.
#[test]
fn a_directory_without_a_manifest_is_an_io_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(matches!(load(dir.path()), Err(PackError::Io { .. })));
}

/// Keeps the crate's own doc example honest: the `cargo-target` rule in
/// `docs/lang.md` must still parse with the vocabulary this crate ships.
#[test]
fn the_documented_example_rule_still_parses() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/lang.md");
    let text = fs::read_to_string(&spec).expect("docs/lang.md is next to the crate");
    let start = text.find("rule \"cargo-target\"").expect("the example is in the spec");
    let end = text[start..].find("\n```").expect("the example is fenced") + start;
    let source = Source::new("docs/lang.md", &text[start..end]);
    assert_eq!(parse(&source).expect("the spec example parses").len(), 1);
}
