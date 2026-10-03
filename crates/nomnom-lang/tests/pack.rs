//! Loading a pack directory off disk.

use std::fs;
use std::path::Path;

use nomnom_lang::pack::{PackError, load};
use nomnom_lang::{Disposition, Kinds, Source, parse};
use tempfile::TempDir;

fn rule_text(title: &str, kind: &str) -> String {
    format!(
        "[[rule]]\ntitle = \"{title}\"\ndescription = \"a `marker` file sits inside it\"\n\
         kind = \"{kind}\"\nfilter = '''\n$d has marker\nthen $d/\n'''\n"
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

const SQUARE_SVG: &str =
    r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><rect width="1" height="1"/></svg>"#;

fn icon_pack(icon: &str) -> TempDir {
    let manifest = format!("name = \"iconed\"\nversion = \"0.1.0\"\nicon = {icon:?}\n");
    let dir = pack_dir(&manifest, &[("main.toml", rule_text("only", "cache/v1"))]);
    fs::write(dir.path().join("icon.svg"), SQUARE_SVG).expect("write icon");
    fs::write(dir.path().join("broken.svg"), "<svg").expect("write broken icon");
    dir
}

// Catches an untrusted pack's `icon` reading outside its own directory, or a
// bad icon failing the whole pack instead of costing it only its picture.
#[test]
fn a_pack_icon_loads_from_its_own_directory_and_a_bad_one_is_only_a_warning() {
    let dir = icon_pack("icon.svg");
    let pack = load(dir.path()).expect("loads");
    let icon = pack.icon.expect("an icon");
    assert_eq!(icon.svg.as_deref(), Ok(SQUARE_SVG.as_bytes()));
    let listed = nomnom_lang::pack::load_icon(dir.path()).expect("an icon");
    assert_eq!(listed.svg.as_deref(), Ok(SQUARE_SVG.as_bytes()), "a listing reads the same icon");

    for (named, why) in [
        ("../icon.svg", "directly in the pack directory"),
        ("sub/icon.svg", "directly in the pack directory"),
        ("sub\\icon.svg", "directly in the pack directory"),
        ("/etc/icon.svg", "directly in the pack directory"),
        ("pack.toml", "must name an .svg file"),
        ("missing.svg", "cannot read missing.svg"),
        ("broken.svg", "not a usable SVG"),
    ] {
        let dir = icon_pack(named);
        let pack =
            load(dir.path()).unwrap_or_else(|error| panic!("`{named}` failed the pack: {error}"));
        let error = pack.icon.expect("an icon").svg.expect_err(named);
        assert!(error.contains(why), "`{named}`: {error}");
    }
}

const RUST_MANIFEST: &str ="name = \"rust\"\nversion = \"0.2.0\"\n\n\
    [kinds.\"toolchain-cache/v1\"]\ndisposition = \"review\"\n";

/// The whole-directory happy path: manifest fields and declared kinds land,
/// both `.toml` rule files are read, and rules come back in a deterministic order.
/// Rule order is the tie-break within a pack in `docs/lang.md`, so a
/// readdir-order-dependent load would make verdicts differ between machines.
#[test]
fn a_pack_directory_loads_every_rule_file_in_a_stable_order() {
    let dir = pack_dir(
        RUST_MANIFEST,
        &[
            ("b-second.toml", rule_text("second", "toolchain-cache/v1")),
            ("a-first.toml", rule_text("first", "build-output/v1")),
        ],
    );

    let pack = load(dir.path()).expect("a valid pack");
    assert_eq!(pack.name, "rust");
    assert_eq!(pack.version, "0.2.0");
    assert_eq!(pack.kinds.len(), 1);
    assert_eq!(pack.kinds[0].to_string(), "toolchain-cache/v1");

    let titles: Vec<&str> = pack.rules.iter().map(|r| r.rule.title.value.as_str()).collect();
    assert_eq!(titles, ["first", "second"], "sorted by file name, not by readdir order");
    assert_eq!(pack.rules[1].rule.disposition, Disposition::Review, "the declared kind's default");
    assert!(pack.rules[0].file.ends_with("a-first.toml"), "provenance points at the source file");
}

/// Catches a pack using a kind it never declared: nothing downstream would
/// know its defaults, so it must fail at load rather than guess.
#[test]
fn a_rule_using_an_undeclared_kind_refuses_the_pack() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("one.toml", rule_text("only", "toolchain-cache/v1"))],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("undeclared kind"));
    assert!(rendered.contains("unknown kind `toolchain-cache/v1`"), "{rendered}");
}

/// Catches a pack redefining `build-output` with its own defaults, which would
/// change what every other pack's `build-output/v1` rule means.
#[test]
fn a_pack_cannot_redefine_a_built_in_kind() {
    let dir = pack_dir(
        "name = \"x\"\nversion = \"0.1.0\"\n[kinds.\"build-output/v2\"]\n\
         disposition = \"reclaimable\"\n",
        &[],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("reserved kind"));
    assert!(rendered.contains("kind `build-output` is built in"), "{rendered}");
}

/// Catches a `pack.toml` still declaring a kind's `confidence` loading with
/// the key silently ignored, or failing with a bare serde "unknown field" that
/// does not say the key was removed.
#[test]
fn a_kind_still_declaring_confidence_is_told_it_was_removed() {
    let dir = pack_dir(
        "name = \"x\"\nversion = \"0.1.0\"\n[kinds.\"toolchain-cache/v1\"]\n\
         disposition = \"review\"\nconfidence = 0.4\n",
        &[],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("confidence was removed"));
    assert!(rendered.contains("`confidence` was removed"), "{rendered}");
}

/// Catches a manifest from the previous format (`labels = [...]`) loading
/// with its declarations silently ignored.
#[test]
fn an_unknown_manifest_key_is_rejected() {
    let dir = pack_dir("name = \"x\"\nversion = \"0.1.0\"\nlabels = []\n", &[]);
    let rendered = diagnostic_of(load(dir.path()).expect_err("unknown key"));
    assert!(rendered.contains("unknown field `labels`"), "{rendered}");
}

/// A verdict cites its rule by title, so two rules sharing one makes "why does
/// nomnom want to delete this" unanswerable.
#[test]
fn duplicate_rule_titles_across_files_are_rejected() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("a.toml", rule_text("same", "cache/v1")), ("b.toml", rule_text("same", "build-output/v1"))],
    );

    let rendered = diagnostic_of(load(dir.path()).expect_err("duplicate rule title"));
    assert!(rendered.contains("duplicate rule title `same`"), "{rendered}");
    assert!(rendered.contains("a.toml"), "the message names the first definition: {rendered}");
}

/// A parse error inside a pack must keep its file name, or the author cannot
/// tell which of ten rule files to open.
#[test]
fn a_broken_rule_file_reports_its_own_path() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("broken.toml", "[[rule]]\ntitle = \"t\"\nkind = \"cache/v1\"\n".into())],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("incomplete rule"));
    assert!(rendered.contains("broken.toml:2:"), "{rendered}");
}

/// Catches a pack still holding pre-TOML `rules/*.nom` files silently loading zero rules.
#[test]
fn an_old_nom_rule_file_refuses_the_pack_and_names_the_file() {
    let dir = pack_dir(
        "name = \"rust\"\nversion = \"0.1.0\"\n",
        &[("cargo.nom", "[Cargo target]\nkind = cache/v1\n".into())],
    );
    let rendered = diagnostic_of(load(dir.path()).expect_err("an old rule file"));
    assert!(rendered.contains("cargo.nom:1:"), "{rendered}");
    assert!(rendered.contains("rule files are TOML now: rename to .toml"), "{rendered}");
}

/// A missing `pack.toml` is an io failure, not a diagnostic: there is no
/// source text to point a caret at.
#[test]
fn a_directory_without_a_manifest_is_an_io_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(matches!(load(dir.path()), Err(PackError::Io { .. })));
}

/// Keeps the spec honest: the first example rule in `docs/lang.md` must still
/// parse with the vocabulary this crate ships.
#[test]
fn the_documented_example_rule_still_parses() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/lang.md");
    let text = fs::read_to_string(&spec).expect("docs/lang.md is next to the crate");
    let start =
        text.find("[[rule]]\ntitle = \"Yarn node_modules/\"").expect("the example is in the spec");
    let end = text[start..].find("\n```").expect("the example is fenced") + start;
    let source = Source::new("docs/lang.md", &text[start..end]);
    assert_eq!(parse(&source, &Kinds::builtin()).expect("the spec example parses").len(), 2);
}
