//! The on-disk record of what was done, written before it is done.
//!
//! The journal is the only reason `nomnom undo` can exist. It is written and
//! fsynced *before* the first filesystem operation and rewritten after every
//! one, so a process killed at any instant leaves a file on disk that still
//! describes everything that could possibly have happened. A journal written
//! after the run would describe nothing at exactly the moment it matters.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ActionError;
use super::plan::Action;

/// Bumped when the record shape changes. [`Journal::read`] refuses anything it
/// does not understand rather than guessing at a half-matching layout.
///
/// Version 2 added [`JournalRecord::reason`] and version 3 its provenance,
/// [`JournalRecord::pack`] and [`JournalRecord::rule`]. Neither is migrated
/// from an older journal: together they are the justification a human approved
/// the deletion on, and a journal that cannot say which rule proposed a path
/// cannot answer the question the journal exists to answer. Inventing the
/// missing fields would be worse than saying they are missing, and nothing has
/// shipped, so there is no migration path to maintain.
pub const FORMAT_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Where an action stood the last time the journal was flushed.
///
/// `Planned` is not a placeholder to be tidied away: it is what a record looks
/// like when the process died mid-action, and [`super::undo`] treats it as
/// "may or may not have happened, go and look".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordStatus {
    Planned,
    Succeeded,
    Failed { message: String },
    Undone,
}

/// The recycle-bin identity of a trashed item, as far as the platform gives one.
///
/// On Windows `id` is the shell parsing name of the item inside the bin; on
/// Freedesktop systems it is the path of the `.trashinfo` file. Both are what
/// `trash::os_limited::restore_all` needs to find the item again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashHandle {
    pub id: String,
    pub name: String,
    pub original_parent: PathBuf,
    /// Unix seconds, as reported by the platform's trash implementation.
    pub time_deleted: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub kind: ActionKind,
    pub source: PathBuf,
    /// Where the bytes went, for `Move`, `Archive`, and a staged `Trash`.
    pub destination: Option<PathBuf>,
    /// `None` when the platform offers no restorable identity, which is the
    /// signal that this particular record is not undoable.
    pub trash: Option<TrashHandle>,
    pub bytes: u64,
    /// The sentence that justified this action, carried over from the plan so a
    /// journal read months later says *why* the path was removed.
    pub reason: String,
    /// The pack that produced the reason, empty when no rule did.
    pub pack: String,
    /// The rule within that pack, empty on the same terms. Together with
    /// `pack` this is what makes "why does nomnom want to delete this"
    /// answerable down to the rule, months after the fact.
    pub rule: String,
    /// Unix seconds at the last status change.
    pub at: u64,
    pub status: RecordStatus,
}

impl JournalRecord {
    pub fn succeeded(&self) -> bool {
        matches!(self.status, RecordStatus::Succeeded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    /// The clean root this apply was fenced to.
    pub root: PathBuf,
    /// Unix seconds when the apply began — before any filesystem operation.
    pub started_at: u64,
    /// Where this journal lives, so a report can name it.
    pub path: PathBuf,
    pub records: Vec<JournalRecord>,
}

impl Journal {
    pub(super) fn new(root: PathBuf, path: PathBuf) -> Self {
        Self { version: FORMAT_VERSION, root, started_at: now_unix(), path, records: Vec::new() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[JournalRecord] {
        &self.records
    }

    /// Bytes the actions that actually succeeded reclaimed.
    pub fn bytes_reclaimed(&self) -> u64 {
        self.records.iter().filter(|r| r.succeeded()).map(|r| r.bytes).sum()
    }

    pub fn failures(&self) -> impl Iterator<Item = &JournalRecord> {
        self.records.iter().filter(|r| matches!(r.status, RecordStatus::Failed { .. }))
    }

    /// Replace the file on disk, durably.
    ///
    /// Write to a sibling temp file, fsync *that*, then rename over the target:
    /// a rename is atomic, so a reader never sees a half-written journal, and
    /// the fsync is what makes the content survive a power loss rather than
    /// merely a process kill.
    pub fn flush(&self) -> Result<(), ActionError> {
        let parent =
            self.path.parent().ok_or_else(|| ActionError::FilesystemRoot(self.path.clone()))?;
        fs::create_dir_all(parent).map_err(ActionError::Io)?;
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(ActionError::Serde)?;
        {
            let mut file = File::create(&tmp).map_err(ActionError::Io)?;
            file.write_all(&bytes).map_err(ActionError::Io)?;
            file.sync_all().map_err(ActionError::Io)?;
        }
        fs::rename(&tmp, &self.path).map_err(ActionError::Io)?;
        // Durability of the rename itself needs the directory fsynced. Windows
        // has no way to open a directory handle through `std::fs`, and its
        // rename is already ordered against the file data, so this is a no-op
        // there by necessity rather than by choice.
        #[cfg(unix)]
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Self, ActionError> {
        let bytes = fs::read(path)
            .map_err(|e| ActionError::JournalUnreadable { path: path.to_path_buf(), source: e })?;
        // Read the version before the whole record shape: an older journal is
        // missing fields this version requires, and serde would report that as
        // a missing-field error rather than as the version mismatch it is.
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let probe: Versioned = serde_json::from_slice(&bytes).map_err(ActionError::Serde)?;
        if probe.version != FORMAT_VERSION {
            return Err(ActionError::JournalVersion {
                found: probe.version,
                expected: FORMAT_VERSION,
            });
        }
        let mut journal: Journal = serde_json::from_slice(&bytes).map_err(ActionError::Serde)?;
        // Trust the file we were handed over the path recorded inside it: the
        // journal may have been copied or moved since it was written.
        journal.path = path.to_path_buf();
        Ok(journal)
    }
}

/// Where journals live when the caller does not say.
///
/// Per-user and durable: `%LOCALAPPDATA%\nomnom\journals` on Windows,
/// `~/Library/Application Support/nomnom/journals` on macOS, and
/// `$XDG_DATA_HOME` (or `~/.local/share`) `/nomnom/journals` elsewhere.
///
/// Explicitly *not* the temp directory, which is swept, and explicitly not
/// anywhere under the tree being cleaned — a journal deleted along with its
/// targets is the one failure mode that makes undo impossible.
pub fn default_journal_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        })
    };
    // A sweepable temp directory is a poor home for a journal, but losing the
    // ability to record one entirely is worse.
    base.unwrap_or_else(std::env::temp_dir).join("nomnom").join("journals")
}

/// A fresh journal file name. Second-resolution plus the pid keeps two applies
/// started in the same second from sharing a file.
pub fn default_journal_path() -> PathBuf {
    default_journal_dir().join(format!("apply-{}-{}.json", now_unix(), std::process::id()))
}

pub(super) fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
