//! Integration tests for the `action` domain — the only code in nomnom that
//! changes a filesystem. Every test here names the regression it catches.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use nomnom_core::action::{
    Action, ActionError, ApplyOptions, Journal, Plan, RecordStatus, TrashPolicy, apply, undo,
};
use tempfile::TempDir;

/// root/
///   keep.txt
///   cache/one.bin, cache/two.bin
///   logs/nested/deep.log
fn fixture() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("cache")).unwrap();
    fs::create_dir_all(root.join("logs").join("nested")).unwrap();
    fs::write(root.join("keep.txt"), b"keep me").unwrap();
    fs::write(root.join("cache").join("one.bin"), b"cache one payload").unwrap();
    fs::write(root.join("cache").join("two.bin"), vec![0xABu8; 4096]).unwrap();
    fs::write(root.join("logs").join("nested").join("deep.log"), b"deep log line\n").unwrap();
    (tmp, root)
}

/// Relative path -> file contents, or `None` for a directory. Compared whole,
/// so a restored-but-truncated file fails just as loudly as a missing one.
fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        let key = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            out.insert(key, None);
            walk(root, &path, out);
        } else {
            out.insert(key, Some(fs::read(&path).unwrap()));
        }
    }
}

fn opts(journal: &Path) -> ApplyOptions {
    ApplyOptions { journal_path: Some(journal.to_path_buf()), ..Default::default() }
}

#[test]
fn undo_restores_the_tree_byte_for_byte() {
    // Catches an undo that puts files back in the right places with the wrong
    // bytes — truncated, empty, or swapped between destinations.
    let (tmp, root) = fixture();
    let before = snapshot(&root);
    let archive = tmp.path().join("archive");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(
        Action::Move { path: root.join("cache").join("one.bin"), to: archive.join("one.bin") },
        17,
        "cache artifact",
    )
    .unwrap();
    plan.push(
        Action::Archive { path: root.join("logs"), to: archive.join("logs") },
        14,
        "old logs",
    )
    .unwrap();
    plan.push(
        Action::Move {
            path: root.join("cache").join("two.bin"),
            to: archive.join("deeper").join("two.bin"),
        },
        4096,
        "larger cache artifact",
    )
    .unwrap();
    assert_eq!(plan.len(), 3);
    assert_eq!(plan.total_bytes(), 17 + 14 + 4096);

    let journal = apply(&plan, &opts(&journal_path)).unwrap();
    assert!(journal.failures().next().is_none(), "unexpected failures: {:?}", journal.records());
    assert_eq!(journal.bytes_reclaimed(), 17 + 14 + 4096);
    assert_ne!(snapshot(&root), before, "apply moved nothing");

    let report = undo(&journal_path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.restored.len(), 3);
    assert_eq!(snapshot(&root), before, "the tree came back different from how it left");
}

#[test]
fn the_journal_on_disk_already_describes_a_run_killed_after_one_action() {
    // Catches a journal written after the fact, which would make every crash
    // unrecoverable while every happy-path test still passed.
    let (tmp, root) = fixture();
    let before = snapshot(&root);
    let archive = tmp.path().join("archive");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(
        Action::Move { path: root.join("cache").join("one.bin"), to: archive.join("one.bin") },
        17,
        "cache artifact",
    )
    .unwrap();
    plan.push(
        Action::Move { path: root.join("keep.txt"), to: archive.join("keep.txt") },
        7,
        "second action, never runs",
    )
    .unwrap();

    // The seam: stop as if the process had been killed right here.
    apply(&plan, &ApplyOptions { stop_after: Some(1), ..opts(&journal_path) }).unwrap();

    let on_disk = Journal::read(&journal_path).unwrap();
    assert_eq!(
        on_disk.records().len(),
        2,
        "the journal must describe the whole plan, not just what finished"
    );
    assert_eq!(on_disk.records()[0].status, RecordStatus::Succeeded);
    // The guard stores canonical paths, so compare the tail rather than the
    // verbatim-prefixed whole.
    assert!(
        on_disk.records()[0].destination.as_ref().is_some_and(|d| d.ends_with("archive/one.bin")),
        "{:?}",
        on_disk.records()[0].destination
    );
    assert_eq!(on_disk.records()[1].status, RecordStatus::Planned, "the second action never ran");
    assert!(root.join("keep.txt").exists(), "the second action must not have happened");

    let report = undo(&journal_path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(snapshot(&root), before, "undo on a crash journal must fully recover the tree");
}

#[test]
fn undo_reports_a_conflict_instead_of_overwriting_the_restore_destination() {
    // Catches an undo that eats newer work: something was recreated at the old
    // path while the file sat in the archive.
    let (tmp, root) = fixture();
    let archive = tmp.path().join("archive");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(
        Action::Move { path: root.join("keep.txt"), to: archive.join("keep.txt") },
        7,
        "archived, then recreated underneath us",
    )
    .unwrap();
    apply(&plan, &opts(&journal_path)).unwrap();

    fs::write(root.join("keep.txt"), b"newer work, do not clobber").unwrap();

    let report = undo(&journal_path).unwrap();
    assert!(!report.is_clean());
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert!(report.restored.is_empty());
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"newer work, do not clobber");
    assert!(archive.join("keep.txt").exists(), "the archived copy must stay put after a conflict");
}

#[test]
fn guards_reject_paths_outside_the_root_drive_roots_and_parent_traversal() {
    // Catches the guards being dropped — the one thing standing between a rule
    // bug and someone's C:\.
    let (tmp, root) = fixture();
    let outside = tmp.path().join("outside.txt");
    fs::write(&outside, b"not yours").unwrap();
    let mut plan = Plan::new(&root).unwrap();

    assert!(matches!(
        plan.push(Action::Trash { path: outside.clone() }, 0, "outside the fence"),
        Err(ActionError::OutsideRoot { .. })
    ));

    let drive_root = if cfg!(windows) { PathBuf::from("C:\\") } else { PathBuf::from("/") };
    assert!(matches!(
        plan.push(Action::Trash { path: drive_root }, 0, "a whole drive"),
        Err(ActionError::FilesystemRoot(_))
    ));

    assert!(matches!(
        plan.push(
            Action::Trash { path: root.join("cache").join("..").join("..").join("outside.txt") },
            0,
            "parent traversal"
        ),
        Err(ActionError::ParentTraversal(_))
    ));

    assert!(matches!(
        plan.push(Action::Trash { path: root.clone() }, 0, "the clean root itself"),
        Err(ActionError::IsCleanRoot(_))
    ));

    assert!(
        matches!(
            plan.push(
                Action::Move { path: root.join("cache"), to: root.join("cache").join("inner") },
                0,
                "into itself"
            ),
            Err(ActionError::DestinationInsideSource { .. })
        ),
        "moving a directory inside itself must be refused"
    );

    assert!(plan.is_empty(), "a refused action must not land in the plan");

    // A plan can also arrive by deserialization, bypassing `push` entirely.
    // `apply` has to run the same guards or the JSON path is an open door.
    let smuggled = format!(
        r#"{{"root":{},"entries":[{{"action":{{"action":"trash","path":{}}},"bytes":0,"reason":"smuggled"}}]}}"#,
        serde_json::to_string(&root).unwrap(),
        serde_json::to_string(&outside).unwrap()
    );
    let smuggled: Plan = serde_json::from_str(&smuggled).unwrap();
    let journal_path = tmp.path().join("journal.json");
    assert!(matches!(apply(&smuggled, &opts(&journal_path)), Err(ActionError::OutsideRoot { .. })));
    assert!(outside.exists());
    assert!(!journal_path.exists(), "a rejected plan must not even open a journal");
}

#[test]
fn undo_twice_moves_nothing_the_second_time() {
    // Catches a double undo relocating already-restored files — the second run
    // must see "already undone", not "destination missing, try again".
    let (tmp, root) = fixture();
    let before = snapshot(&root);
    let archive = tmp.path().join("archive");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(
        Action::Move { path: root.join("cache").join("one.bin"), to: archive.join("one.bin") },
        17,
        "cache artifact",
    )
    .unwrap();
    plan.push(
        Action::Archive { path: root.join("logs"), to: archive.join("logs") },
        14,
        "old logs",
    )
    .unwrap();
    apply(&plan, &opts(&journal_path)).unwrap();

    let first = undo(&journal_path).unwrap();
    assert_eq!(first.restored.len(), 2);
    let after_first = snapshot(&root);
    assert_eq!(after_first, before);

    let second = undo(&journal_path).unwrap();
    assert!(second.is_clean(), "{second:?}");
    assert!(second.restored.is_empty(), "the second undo must move nothing");
    assert_eq!(second.skipped.len(), 2);
    assert_eq!(second.bytes_restored, 0);
    assert_eq!(snapshot(&root), after_first, "the second undo changed the tree");
}

#[test]
fn a_staged_trash_is_a_plain_reversible_move() {
    // Catches `TrashPolicy::Stage` losing the destination, which is the whole
    // reason it exists: it is the undoable trash on platforms where the
    // recycle bin cannot be restored from.
    let (tmp, root) = fixture();
    let before = snapshot(&root);
    let staging = tmp.path().join("staging");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(Action::Trash { path: root.join("cache").join("two.bin") }, 4096, "staged trash")
        .unwrap();

    let journal = apply(
        &plan,
        &ApplyOptions {
            trash_policy: TrashPolicy::Stage { dir: staging.clone() },
            ..opts(&journal_path)
        },
    )
    .unwrap();
    assert!(
        journal.records()[0].destination.is_some(),
        "a staged trash must record where it put the bytes"
    );
    assert!(!root.join("cache").join("two.bin").exists());

    let report = undo(&journal_path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(snapshot(&root), before);
}

#[test]
fn reordering_a_plan_keeps_every_path_with_its_own_reason() {
    // Catches the reason drifting onto the wrong path — the failure mode a
    // front-end holding reasons in a parallel index-aligned list has, where a
    // user approves a deletion on a sentence that justified a different file.
    // The reorder here stands in for any future sort, filter or dedup inside
    // `Plan`.
    let (tmp, root) = fixture();
    let archive = tmp.path().join("archive");
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(
        Action::Move { path: root.join("cache").join("one.bin"), to: archive.join("one.bin") },
        17,
        "one.bin: rebuildable cache",
    )
    .unwrap();
    plan.push(
        Action::Move { path: root.join("cache").join("two.bin"), to: archive.join("two.bin") },
        4096,
        "two.bin: stale download",
    )
    .unwrap();
    plan.push(
        Action::Archive { path: root.join("logs"), to: archive.join("logs") },
        14,
        "logs: superseded build output",
    )
    .unwrap();

    // Reordering the entries is what a sort, a filter or a dedup inside `Plan`
    // would do. Go through serde so the reorder happens to the real entries.
    let mut wire: serde_json::Value = serde_json::to_value(&plan).unwrap();
    wire["entries"].as_array_mut().unwrap().reverse();
    let plan: Plan = serde_json::from_value(wire).unwrap();

    let expected = |path: &Path| -> String {
        let name = path.file_name().unwrap().to_string_lossy();
        match name.as_ref() {
            "one.bin" => "one.bin: rebuildable cache",
            "two.bin" => "two.bin: stale download",
            "logs" => "logs: superseded build output",
            other => panic!("unexpected path in the plan: {other}"),
        }
        .to_string()
    };

    for entry in plan.actions() {
        assert_eq!(entry.reason, expected(entry.action.path()), "plan entry lost its own reason");
    }

    let journal = apply(&plan, &opts(&journal_path)).unwrap();
    for record in journal.records() {
        assert_eq!(record.reason, expected(&record.source), "journal record lost its own reason");
    }

    let report = undo(&journal_path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.restored.len(), 3);
    for restored in &report.restored {
        assert_eq!(
            restored.reason,
            expected(&restored.path),
            "undo reported the wrong reason for a restored path"
        );
    }
}

/// Needs the real recycle bin, so it is off by default. Run it with:
/// `cargo test -p nomnom-core --test action -- --ignored --test-threads=1`
#[test]
#[ignore = "touches the real OS recycle bin"]
fn recycled_files_come_back_from_the_bin() {
    let (tmp, root) = fixture();
    let before = snapshot(&root);
    let journal_path = tmp.path().join("journal.json");

    let mut plan = Plan::new(&root).unwrap();
    plan.push(Action::Trash { path: root.join("cache").join("one.bin") }, 17, "recycled").unwrap();
    let journal = apply(&plan, &opts(&journal_path)).unwrap();
    assert!(
        journal.records()[0].trash.is_some(),
        "no recycle-bin identity was captured, so undo cannot work"
    );
    assert!(!root.join("cache").join("one.bin").exists());

    let report = undo(&journal_path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(snapshot(&root), before);
}
