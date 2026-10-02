//! Plan, apply, undo — the only code in nomnom that changes a filesystem.
//!
//! Everything else in this crate observes and judges; this domain acts. It is
//! built on three commitments:
//!
//! - **Nothing is hard-deleted.** [`Action::Trash`] goes to the OS recycle bin
//!   or to a staging directory, and the other two actions are renames.
//! - **The journal precedes the act.** [`apply`] writes and fsyncs a complete
//!   [`Journal`] before the first filesystem operation and rewrites it after
//!   every one, so a process killed at any instant leaves a file that still
//!   describes what may have happened. [`undo`] reads only that file.
//! - **The guards are not optional.** A [`Plan`] refuses any path that is a
//!   filesystem or drive root, contains `..`, or falls outside the declared
//!   clean root — at planning time and again at apply time, because a plan can
//!   also arrive by deserialization.
//!
//! A plan holds paths, not verdicts: the `verdict` domain decides what should
//! go, and [`plan_from`] is the one place the two are joined, so the plan and
//! its guards stay testable without a judge.

mod apply;
mod candidates;
mod display;
mod journal;
mod plan;

use std::path::PathBuf;

pub use apply::{
    ApplyOptions, RestoredRecord, SkippedRecord, TrashPolicy, UndoConflict, UndoFailure,
    UndoReport, apply, default_stage_dir, trash_policy, undo,
};
pub use candidates::plan_from;
pub use display::plain;
pub use journal::{
    ActionKind, FORMAT_VERSION, Journal, JournalEntry, JournalRecord, JournalSummary, RecordStatus,
    TrashHandle, default_journal_dir, default_journal_path, list_journals, list_journals_in,
};
pub use plan::{Action, Justification, Plan, PlanEntry};

/// Why an action was refused, or why the journal could not be trusted.
///
/// Per-action execution failures are deliberately absent: those never abort a
/// run and are recorded in the [`Journal`] instead.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("clean root {0} does not exist or is not readable")]
    RootUnreadable(PathBuf),
    #[error("refusing to touch {0}: that is a filesystem or drive root")]
    FilesystemRoot(PathBuf),
    #[error("refusing to touch {0}: that is the clean root itself")]
    IsCleanRoot(PathBuf),
    #[error("refusing to touch {path}: it lies outside the clean root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
    #[error("refusing to touch {0}: it contains a `..` traversal")]
    ParentTraversal(PathBuf),
    #[error("refusing to touch {0}: paths must be absolute")]
    RelativePath(PathBuf),
    #[error("destination {0} already exists")]
    DestinationExists(PathBuf),
    #[error("destination {destination} is inside the path {path} being moved")]
    DestinationInsideSource { path: PathBuf, destination: PathBuf },
    #[error("journal {path} is unreadable: {source}")]
    JournalUnreadable { path: PathBuf, source: std::io::Error },
    #[error("journal format version {found} is not the supported {expected}")]
    JournalVersion { found: u32, expected: u32 },
    #[error(transparent)]
    Serde(serde_json::Error),
    #[error(transparent)]
    Io(std::io::Error),
}
