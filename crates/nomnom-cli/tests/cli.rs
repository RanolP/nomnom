//! End-to-end tests against the real binary. Each names the regression it
//! catches; there is nothing here that only restates the implementation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_nomnom");

/// A tree that trips exactly one rule: `node_modules` beside a `package.json`.
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(&root.join("package.json"), br#"{"name":"fixture"}"#);
    write(&root.join("src/index.js"), b"console.log('hi');\n");
    write(&root.join("node_modules/left-pad/index.js"), b"module.exports = 1;\n");
    write(&root.join("node_modules/left-pad/package.json"), br#"{"name":"left-pad"}"#);
    write(&root.join("node_modules/.bin/left-pad"), b"#!/bin/sh\n");
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// Every path under `root`, relative, with file contents. Directories map to
/// `None`. This is the thing a dry run must leave identical.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("relative").to_path_buf();
            if entry.file_type().expect("file_type").is_dir() {
                out.insert(relative, None);
                stack.push(path);
            } else {
                out.insert(relative, Some(std::fs::read(&path).expect("read")));
            }
        }
    }
    out
}

fn nomnom(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run nomnom")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A dry run that is not dry is the single worst bug this tool could ship:
/// `clean` without `--apply` must leave every path and every byte where it was.
#[test]
fn clean_without_apply_touches_nothing() {
    let dir = fixture();
    let before = snapshot(dir.path());

    let output = nomnom(&["clean", dir.path().to_str().unwrap()]);
    assert!(output.status.success(), "clean failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout(&output).contains("node_modules"), "plan did not name node_modules");

    assert_eq!(before, snapshot(dir.path()), "dry run modified the tree");
}

/// The journal path nomnom PRINTS is the one `undo` must accept, and the round
/// trip has to put the bytes back exactly. Parsing the path out of the command's
/// own output is what ties the two together.
#[test]
fn apply_then_undo_restores_the_tree() {
    let dir = fixture();
    let staging = tempfile::tempdir().expect("staging");
    let before = snapshot(dir.path());

    let applied = nomnom(&[
        "clean",
        dir.path().to_str().unwrap(),
        "--apply",
        "--stage",
        staging.path().join("staged").to_str().unwrap(),
    ]);
    assert!(applied.status.success(), "apply failed: {}", String::from_utf8_lossy(&applied.stderr));
    let applied_out = stdout(&applied);
    assert_ne!(before, snapshot(dir.path()), "apply changed nothing");

    let journal = applied_out
        .lines()
        .find_map(|line| line.strip_prefix("Journal: "))
        .expect("apply printed no journal path")
        .trim()
        .to_string();

    let undone = nomnom(&["undo", &journal]);
    assert!(undone.status.success(), "undo failed: {}", String::from_utf8_lossy(&undone.stderr));

    assert_eq!(before, snapshot(dir.path()), "undo did not restore the tree byte-for-byte");
}

/// The JSON surface must stay the core's serde types, and a verdict without a
/// reason must never reach a front-end: the reason is what a human approves on.
#[test]
fn suggest_json_carries_a_reason_for_every_verdict() {
    let dir = fixture();
    let output = nomnom(&["suggest", dir.path().to_str().unwrap(), "--json"]);
    assert!(output.status.success(), "suggest failed: {}", String::from_utf8_lossy(&output.stderr));

    let parsed: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    let groups = parsed["groups"].as_array().expect("groups array");
    assert!(!groups.is_empty(), "no verdicts on a fixture that has node_modules");

    let mut verdicts = 0;
    for group in groups {
        for entry in group["entries"].as_array().expect("entries array") {
            let reason = entry["verdict"]["reason"].as_str().expect("reason string");
            assert!(!reason.is_empty(), "empty reason for {}", entry["path"]);
            verdicts += 1;
        }
    }
    assert!(verdicts > 0, "no verdicts in the JSON");
}

/// The core judges `node_modules` beside a `package.json` correctly; this
/// catches the CLI failing to surface what it judged.
#[test]
fn suggest_names_node_modules() {
    let dir = fixture();
    let output = nomnom(&["suggest", dir.path().to_str().unwrap()]);
    assert!(output.status.success(), "suggest failed: {}", String::from_utf8_lossy(&output.stderr));

    let text = stdout(&output);
    assert!(text.contains("node_modules"), "suggest output did not name node_modules:\n{text}");
    assert!(text.contains("npm install"), "suggest output did not carry the reason:\n{text}");
}
