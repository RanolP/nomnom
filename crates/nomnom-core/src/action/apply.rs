//! Carrying a plan out, and taking it back.
//!
//! Two rules shape everything here. A failing action never aborts the run: it
//! is recorded and the next one is attempted, because a cleanup that stops
//! halfway on one locked file and says nothing is worse than one that finishes
//! and reports what it could not do. And nothing happens before the journal
//! describing it is on disk and fsynced.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ActionError;
use super::journal::{
    ActionKind, Journal, JournalRecord, RecordStatus, TrashHandle, default_journal_path, now_unix,
};
use super::plan::{Action, Plan};

/// How [`Action::Trash`] is carried out.
///
/// `Recycle` is undoable wherever `trash::os_limited::restore_all` exists —
/// Windows and Freedesktop systems — and is *not* on macOS, where the `trash`
/// crate compiles that module out entirely. `Stage` is undoable everywhere,
/// because it is just a rename, and its destination is known before the move
/// rather than discovered after it, so a process killed mid-action still leaves
/// a complete record. A front-end that wants undo to be real on every platform
/// should default to `Stage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum TrashPolicy {
    #[default]
    Recycle,
    Stage {
        dir: PathBuf,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Where to write the journal. `None` uses
    /// [`super::journal::default_journal_path`].
    pub journal_path: Option<PathBuf>,
    pub trash_policy: TrashPolicy,
    /// Stop the run after this many actions, as if the process had been killed
    /// there. The journal on disk is left exactly as a kill would leave it,
    /// which is what makes crash recovery testable.
    pub stop_after: Option<usize>,
}

/// Execute `plan`, journalling as it goes.
///
/// Returns `Err` only when the plan itself is unacceptable or the journal
/// cannot be written — the two cases where proceeding would be unsafe. Every
/// per-action failure lands in the returned [`Journal`] instead.
pub fn apply(plan: &Plan, opts: &ApplyOptions) -> Result<Journal, ActionError> {
    plan.validate()?;

    let path = opts.journal_path.clone().unwrap_or_else(default_journal_path);
    let mut journal = Journal::new(plan.root().to_path_buf(), path);
    for entry in plan.actions() {
        journal.records.push(JournalRecord {
            kind: ActionKind::from(&entry.action),
            source: entry.action.path().to_path_buf(),
            destination: entry.action.destination().map(Path::to_path_buf),
            trash: None,
            bytes: entry.bytes,
            reason: entry.reason.clone(),
            pack: entry.pack.clone(),
            rule: entry.rule.clone(),
            at: now_unix(),
            status: RecordStatus::Planned,
        });
    }
    // The whole plan is on disk, fsynced, before the first byte moves.
    journal.flush()?;

    for (index, entry) in plan.actions().iter().enumerate() {
        let outcome = perform(&entry.action, plan.root(), &opts.trash_policy);
        let record = &mut journal.records[index];
        record.at = now_unix();
        match outcome {
            Ok(done) => {
                if done.destination.is_some() {
                    record.destination = done.destination;
                }
                record.trash = done.trash;
                record.status = RecordStatus::Succeeded;
            }
            Err(message) => record.status = RecordStatus::Failed { message },
        }
        journal.flush()?;
        if opts.stop_after == Some(index + 1) {
            return Ok(journal);
        }
    }
    Ok(journal)
}

struct Performed {
    destination: Option<PathBuf>,
    trash: Option<TrashHandle>,
}

fn perform(action: &Action, root: &Path, policy: &TrashPolicy) -> Result<Performed, String> {
    match action {
        Action::Trash { path } => match policy {
            TrashPolicy::Recycle => {
                let trash = recycle::trash_and_identify(path)?;
                Ok(Performed { destination: None, trash })
            }
            TrashPolicy::Stage { dir } => {
                let destination = staging_destination(path, root, dir);
                rename(path, &destination)?;
                Ok(Performed { destination: Some(destination), trash: None })
            }
        },
        Action::Archive { path, to } | Action::Move { path, to } => {
            rename(path, to)?;
            Ok(Performed { destination: Some(to.clone()), trash: None })
        }
    }
}

/// Mirror the path's position under the clean root inside the staging
/// directory, so a staged tree is readable and two files with the same name
/// from different directories do not collide.
fn staging_destination(path: &Path, root: &Path, dir: &Path) -> PathBuf {
    let relative = path
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new(path.file_name().unwrap_or(path.as_os_str())));
    let candidate = dir.join(relative);
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 1u32;
    loop {
        let mut name = candidate.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{n}"));
        let next = candidate.with_file_name(name);
        if !next.exists() {
            return next;
        }
        n += 1;
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

// ---------------------------------------------------------------- undo

/// A path put back, and the reason the apply recorded for removing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredRecord {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRecord {
    pub path: PathBuf,
    pub reason: String,
}

/// Something already occupies the place a record wants to restore to. Reported,
/// never resolved: the file that is there now may be newer work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoConflict {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoReport {
    pub journal_path: PathBuf,
    pub restored: Vec<RestoredRecord>,
    pub skipped: Vec<SkippedRecord>,
    pub conflicts: Vec<UndoConflict>,
    pub failures: Vec<UndoFailure>,
    pub bytes_restored: u64,
}

impl UndoReport {
    /// Everything that could be put back was put back.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.failures.is_empty()
    }
}

enum Outcome {
    Restored,
    Skipped(String),
    Conflict(String),
    Failed(String),
}

/// Reverse a journal, newest action first.
///
/// Safe to re-run: a record already marked undone is a no-op, and a record
/// whose action never happened is skipped. The journal is rewritten after every
/// reversal, so an interrupted undo resumes rather than repeats.
pub fn undo(journal_path: &Path) -> Result<UndoReport, ActionError> {
    let mut journal = Journal::read(journal_path)?;
    let mut report = UndoReport {
        journal_path: journal_path.to_path_buf(),
        restored: Vec::new(),
        skipped: Vec::new(),
        conflicts: Vec::new(),
        failures: Vec::new(),
        bytes_restored: 0,
    };

    for index in (0..journal.records.len()).rev() {
        let outcome = undo_record(&journal.records[index]);
        let record = &mut journal.records[index];
        let source = record.source.clone();
        match outcome {
            Outcome::Restored => {
                record.status = RecordStatus::Undone;
                record.at = now_unix();
                report.bytes_restored += record.bytes;
                report
                    .restored
                    .push(RestoredRecord { path: source, reason: record.reason.clone() });
                journal.flush()?;
            }
            Outcome::Skipped(reason) => report.skipped.push(SkippedRecord { path: source, reason }),
            Outcome::Conflict(message) => {
                report.conflicts.push(UndoConflict { path: source, message })
            }
            Outcome::Failed(message) => report.failures.push(UndoFailure { path: source, message }),
        }
    }
    Ok(report)
}

fn undo_record(record: &JournalRecord) -> Outcome {
    match &record.status {
        RecordStatus::Undone => return Outcome::Skipped("already undone".into()),
        RecordStatus::Failed { .. } => {
            return Outcome::Skipped("action failed, nothing to undo".into());
        }
        RecordStatus::Planned => {
            // The kill window. The only evidence is the filesystem itself.
            match &record.destination {
                Some(destination) if destination.exists() && !record.source.exists() => {}
                Some(_) => return Outcome::Skipped("action was never performed".into()),
                None => {
                    return if record.source.exists() {
                        Outcome::Skipped("action was never performed".into())
                    } else {
                        Outcome::Failed(
                            "interrupted before the recycle-bin identity was recorded; restore it from the recycle bin by hand".into(),
                        )
                    };
                }
            }
        }
        RecordStatus::Succeeded => {}
    }

    if record.source.exists() {
        return Outcome::Conflict(format!(
            "{} exists again; refusing to overwrite it",
            record.source.display()
        ));
    }

    match &record.destination {
        Some(destination) => {
            if !destination.exists() {
                return Outcome::Failed(format!(
                    "{} is gone; cannot move it back",
                    destination.display()
                ));
            }
            match rename(destination, &record.source) {
                Ok(()) => Outcome::Restored,
                Err(message) => Outcome::Failed(message),
            }
        }
        None => match &record.trash {
            None => Outcome::Failed(format!(
                "no recycle-bin identity was recorded for {}; restore it by hand",
                record.source.display()
            )),
            Some(handle) => match recycle::restore(handle) {
                Ok(()) => Outcome::Restored,
                Err(recycle::RestoreError::Collision(path)) => Outcome::Conflict(format!(
                    "{} exists again; refusing to overwrite it",
                    path.display()
                )),
                Err(recycle::RestoreError::Other(message)) => Outcome::Failed(message),
            },
        },
    }
}

// ---------------------------------------------------------------- recycle bin

/// The recycle bin, behind the one question that matters: can we get it back?
///
/// `trash::os_limited` — which holds `list` and `restore_all` — is compiled
/// only for Windows and Freedesktop platforms (see the `cfg` on the module in
/// `trash-5.2.9/src/lib.rs`). Everywhere else, deleting to the bin is a
/// one-way trip and this module says so instead of pretending.
mod recycle {
    use std::path::{Path, PathBuf};

    use super::TrashHandle;

    pub enum RestoreError {
        Collision(PathBuf),
        Other(String),
    }

    #[cfg(any(
        target_os = "windows",
        all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))
    ))]
    mod imp {
        use super::{RestoreError, TrashHandle};
        use std::path::Path;

        pub fn trash_and_identify(path: &Path) -> Result<Option<TrashHandle>, String> {
            trash::delete(path).map_err(|e| format!("cannot trash {}: {e}", path.display()))?;
            Ok(identify(path))
        }

        /// The bin does not tell us which item it just created, so find it: the
        /// most recently deleted item whose original path is ours.
        fn identify(path: &Path) -> Option<TrashHandle> {
            let items = trash::os_limited::list().ok()?;
            let item = items
                .into_iter()
                .filter(|item| super::same_path(&item.original_path(), path))
                .max_by_key(|item| item.time_deleted)?;
            Some(TrashHandle {
                id: item.id.into_string().ok()?,
                name: item.name.into_string().ok()?,
                original_parent: item.original_parent,
                time_deleted: item.time_deleted,
            })
        }

        pub fn restore(handle: &TrashHandle) -> Result<(), RestoreError> {
            let item = trash::TrashItem {
                id: handle.id.clone().into(),
                name: handle.name.clone().into(),
                original_parent: handle.original_parent.clone(),
                time_deleted: handle.time_deleted,
            };
            trash::os_limited::restore_all([item]).map_err(|e| match e {
                trash::Error::RestoreCollision { path, .. } => RestoreError::Collision(path),
                other => RestoreError::Other(format!("restore failed: {other}")),
            })
        }
    }

    #[cfg(not(any(
        target_os = "windows",
        all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))
    )))]
    mod imp {
        use super::{RestoreError, TrashHandle};
        use std::path::Path;

        pub fn trash_and_identify(path: &Path) -> Result<Option<TrashHandle>, String> {
            trash::delete(path).map_err(|e| format!("cannot trash {}: {e}", path.display()))?;
            Ok(None)
        }

        pub fn restore(_handle: &TrashHandle) -> Result<(), RestoreError> {
            Err(RestoreError::Other(
                "this platform offers no programmatic restore from the trash; use TrashPolicy::Stage for undoable cleanups"
                    .into(),
            ))
        }
    }

    pub use imp::{restore, trash_and_identify};

    /// The bin reports plain Windows paths while we hold canonical verbatim
    /// ones, and Windows path comparison is case-insensitive.
    #[allow(dead_code)]
    fn same_path(a: &Path, b: &Path) -> bool {
        normalize(a) == normalize(b)
    }

    #[allow(dead_code)]
    fn normalize(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if cfg!(windows) { text.to_lowercase() } else { text.to_string() }
    }
}
