//! Integration tests for the `action` domain — the only code in nomnom that
//! changes a filesystem. Every test here names the regression it catches.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{catalog_of, write};
use nomnom_core::action::{Action, ActionError, Approval, Exclusions, Plan, apply, plan_from};
use nomnom_core::catalog::DuplicateProgress;
use nomnom_core::verdict::{
    TrustedPack, assess, builtin_pack, duplicate_copy_rule, find_duplicates,
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

#[test]
fn a_drive_root_anchors_a_plan_but_is_never_a_target() {
    // Catches Clean planning nothing on every drive: scans are drive-only, so
    // the plan's fence is always a drive root.
    let drive_root = if cfg!(windows) { PathBuf::from("C:\\") } else { PathBuf::from("/") };
    let mut plan = Plan::new(&drive_root).unwrap();
    assert!(matches!(
        plan.push(Action::Trash { path: drive_root }, 0, "the whole drive"),
        Err(ActionError::FilesystemRoot(_))
    ));
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

/// Three same-size files whose sampled head, middle and tail agree, of which
/// `different.bin` differs a quarter of the way in, where no sample reads.
/// Oldest is `original.bin`, so it is the kept copy. Returns the approved
/// duplicate plan before verification.
fn likely_copies() -> (TempDir, PathBuf, Plan) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    let len = 2 * 1024 * 1024;
    let original = vec![b'd'; len];
    let mut different = original.clone();
    different[len / 4] = b'X';
    let now = SystemTime::now();
    for (age, name, contents) in [
        (3, "original.bin", &original),
        (2, "same.bin", &original),
        (1, "different.bin", &different),
    ] {
        let path = root.join(name);
        write(&path, contents);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now - Duration::from_secs(86_400 * age))
            .unwrap();
    }
    let catalog = catalog_of(&root);
    let rules = assess(&catalog, vec![TrustedPack::builtin(builtin_pack().clone())]);
    let duplicates =
        find_duplicates(&catalog, &rules, &DuplicateProgress::default()).expect("not cancelled");
    let assessment = rules.with_duplicates(&catalog, &duplicates);
    assert_eq!(assessment.duplicate_copies().count(), 2, "the sample must let both copies through");
    let approval = Approval { rules: [duplicate_copy_rule()].into(), paths: Default::default() };
    let (plan, refused) = plan_from(&assessment, &approval, &Exclusions::default(), false).unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(plan.len(), 2);
    (tmp, root, plan)
}

/// The regression: a sampled match trusted as identity, so a file that only
/// shares its size and sampled ends with the kept copy is trashed, and its
/// unique contents with it.
#[test]
fn a_copy_with_the_same_ends_but_a_different_middle_is_never_deleted() {
    let (_tmp, root, mut plan) = likely_copies();

    let dropped = plan.verify_copies();
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert!(dropped[0].0.ends_with("different.bin"));
    assert!(dropped[0].1.contains("differ"), "the reason says why: {}", dropped[0].1);

    let report = apply(&plan).unwrap();
    assert!(report.failures().next().is_none(), "{:?}", report.records());
    assert!(root.join("different.bin").exists(), "a copy that differs was trashed");
    assert!(root.join("original.bin").exists(), "the kept copy was trashed");
    assert!(!root.join("same.bin").exists(), "the verified copy should go");
}

/// The regression: apply trusting the plan, so a plan that skipped
/// verification, or came back from JSON where the verification does not
/// travel, trashes sampled matches unchecked.
#[test]
fn apply_refuses_every_copy_that_was_not_verified() {
    let (_tmp, root, plan) = likely_copies();
    let plan: Plan = serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();

    let report = apply(&plan).unwrap();
    assert_eq!(report.failures().count(), 2, "{:?}", report.records());
    assert!(root.join("same.bin").exists());
    assert!(root.join("different.bin").exists());
}

/// The regression: a copy verified, then rewritten before apply, trashed on
/// the strength of a comparison that no longer holds.
#[test]
fn apply_refuses_a_copy_that_changed_after_it_was_verified() {
    let (_tmp, root, mut plan) = likely_copies();
    plan.verify_copies();
    fs::write(root.join("same.bin"), b"rewritten").unwrap();

    let report = apply(&plan).unwrap();
    assert_eq!(report.failures().count(), 1, "{:?}", report.records());
    assert!(root.join("same.bin").exists());
}
