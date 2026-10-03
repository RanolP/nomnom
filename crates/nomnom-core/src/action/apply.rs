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
    Trash,
    Archive,
    Move,
}

impl From<&Action> for ActionKind {
    fn from(action: &Action) -> Self {
        match action {
            Action::Trash { .. } => ActionKind::Trash,
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

/// Execute `plan`: trashed paths go to the OS recycle bin, archives and moves
/// are renames.
///
/// Returns `Err` only when the plan itself is unacceptable, before anything
/// moves. Every per-action failure lands in the returned [`ApplyReport`].
pub fn apply(plan: &Plan) -> Result<ApplyReport, ActionError> {
    plan.validate()?;
    let records = plan
        .actions()
        .iter()
        .map(|entry| ApplyRecord {
            kind: ActionKind::from(&entry.action),
            source: entry.action.path().to_path_buf(),
            destination: entry.action.destination().map(Path::to_path_buf),
            bytes: entry.bytes,
            reason: entry.reason.clone(),
            pack: entry.pack.clone(),
            rule: entry.rule.clone(),
            // Checked here, at the last moment, rather than trusted from the
            // plan: a copy that was not proven identical is never trashed.
            status: match entry.copy_refusal().map_or_else(|| perform(&entry.action), Err) {
                Ok(()) => RecordStatus::Succeeded,
                Err(message) => RecordStatus::Failed { message },
            },
        })
        .collect();
    Ok(ApplyReport { root: plan.root().to_path_buf(), records })
}

fn perform(action: &Action) -> Result<(), String> {
    match action {
        Action::Trash { path } => {
            trash::delete(path).map_err(|e| format!("cannot trash {}: {e}", path.display()))
        }
        Action::Archive { path, to } | Action::Move { path, to } => rename(path, to),
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
