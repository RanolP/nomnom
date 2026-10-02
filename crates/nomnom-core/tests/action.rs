//! Integration tests for the `action` domain — the only code in nomnom that
//! changes a filesystem. Every test here names the regression it catches.

use std::fs;
use std::path::{Path, PathBuf};

use nomnom_core::action::{Action, ActionError, Plan, apply};
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
    assert!(matches!(apply(&smuggled), Err(ActionError::OutsideRoot { .. })));
    assert!(outside.exists());
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

    let report = apply(&plan).unwrap();
    assert!(report.failures().next().is_none(), "unexpected failures: {:?}", report.records());
    assert_eq!(report.bytes_reclaimed(), 17 + 4096 + 14);
    for record in report.records() {
        assert_eq!(record.reason, expected(&record.source), "apply record lost its own reason");
    }
    assert!(archive.join("logs").join("nested").join("deep.log").exists());
}
