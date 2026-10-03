//! Carrying a plan out.
//!
//! A failing action never aborts the run: it is recorded and the next one is
//! attempted, because a cleanup that stops halfway on one locked file and says
//! nothing is worse than one that finishes and reports what it could not do.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::ActionError;
use super::plan::{Action, Plan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Delete,
    Archive,
    Move,
}

impl From<&Action> for ActionKind {
    fn from(action: &Action) -> Self {
        match action {
            Action::Delete { .. } => ActionKind::Delete,
            Action::Archive { .. } => ActionKind::Archive,
            Action::Move { .. } => ActionKind::Move,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordStatus {
    Succeeded,
    Failed { message: String },
}

/// One plan entry, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyRecord {
    pub kind: ActionKind,
    pub source: PathBuf,
    /// Where the bytes went, for `Move` and `Archive`.
    pub destination: Option<PathBuf>,
    pub bytes: u64,
    /// The sentence that justified this action, carried over from the plan.
    pub reason: String,
    /// The pack that produced the reason, empty when no rule did.
    pub pack: String,
    /// The rule within that pack, empty on the same terms.
    pub rule: String,
    pub status: RecordStatus,
}

impl ApplyRecord {
    pub fn succeeded(&self) -> bool {
        matches!(self.status, RecordStatus::Succeeded)
    }
}

/// What one [`apply`] did, entry by entry, in plan order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyReport {
    /// The clean root this apply was fenced to.
    pub root: PathBuf,
    pub records: Vec<ApplyRecord>,
}

impl ApplyReport {
    pub fn records(&self) -> &[ApplyRecord] {
        &self.records
    }

    /// Bytes the actions that actually succeeded reclaimed.
    pub fn bytes_reclaimed(&self) -> u64 {
        self.records.iter().filter(|r| r.succeeded()).map(|r| r.bytes).sum()
    }

    pub fn failures(&self) -> impl Iterator<Item = &ApplyRecord> {
        self.records.iter().filter(|r| matches!(r.status, RecordStatus::Failed { .. }))
    }
}

/// Execute `plan`: deleted paths are removed permanently, archives and moves
/// are renames.
///
/// Returns `Err` only when the plan itself is unacceptable, before anything
/// moves. Every per-action failure lands in the returned [`ApplyReport`].
pub fn apply(plan: &Plan) -> Result<ApplyReport, ActionError> {
    apply_with(plan, |_| {})
}

/// [`apply`], calling `on_record` with each entry's record the moment that
/// entry is done, in plan order, so a front-end can log and meter the run as
/// it goes. The plan's `len` and `total_bytes` are the meter's totals.
pub fn apply_with(
    plan: &Plan,
    mut on_record: impl FnMut(&ApplyRecord),
) -> Result<ApplyReport, ActionError> {
    plan.validate()?;
    let records = plan
        .actions()
        .iter()
        .map(|entry| {
            let record = ApplyRecord {
                kind: ActionKind::from(&entry.action),
                source: entry.action.path().to_path_buf(),
                destination: entry.action.destination().map(Path::to_path_buf),
                bytes: entry.bytes,
                reason: entry.reason.clone(),
                pack: entry.pack.clone(),
                rule: entry.rule.clone(),
                status: match perform(&entry.action) {
                    Ok(()) => RecordStatus::Succeeded,
                    Err(message) => RecordStatus::Failed { message },
                },
            };
            on_record(&record);
            record
        })
        .collect();
    Ok(ApplyReport { root: plan.root().to_path_buf(), records })
}

fn perform(action: &Action) -> Result<(), String> {
    match action {
        Action::Delete { path } => {
            delete(path).map_err(|e| format!("cannot delete {}: {e}", path.display()))
        }
        Action::Archive { path, to } | Action::Move { path, to } => rename(path, to),
    }
}

/// Permanent removal of `path` and, for a directory, everything under it. A
/// symlink or junction is removed itself, never followed out of the plan's
/// fence. Windows refuses to remove a read-only file, so a refusal there
/// clears the attribute under `path` and tries once more.
fn delete(path: &Path) -> std::io::Result<()> {
    match remove(path) {
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(path);
            remove(path)
        }
        result => result,
    }
}

fn remove(path: &Path) -> std::io::Result<()> {
    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_symlink() {
        // A directory symlink or junction on Windows takes `remove_dir`.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if kind.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(windows)]
fn clear_readonly(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            clear_readonly(&entry.path());
        }
    }
    let mut permissions = meta.permissions();
    if permissions.readonly() {
        // On Windows this clears FILE_ATTRIBUTE_READONLY; no Unix mode bits.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

/// Renames only. A cross-volume move would need a recursive copy, and a
/// half-finished copy is precisely the state this module exists to avoid, so a
/// cross-device destination is reported as a failure instead.
fn rename(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::rename(from, to)
        .map_err(|e| format!("cannot move {} to {}: {e}", from.display(), to.display()))
}
