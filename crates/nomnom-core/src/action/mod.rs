//! Plan and apply — the only code in nomnom that changes a filesystem.
//!
//! Everything else in this crate observes and judges; this domain acts. It is
//! built on two commitments:
//!
//! - **Nothing moves without an explicit pick.** [`Action::Delete`] is
//!   permanent (no recycle bin), so a plan holds only what the user approved,
//!   minus exclusions. The other two actions are renames.
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
mod exclusions;
mod plan;

use std::path::PathBuf;

pub use apply::{ActionKind, ApplyRecord, ApplyReport, RecordStatus, apply, apply_with};
pub use candidates::{
    Approval, RuleGroup, RuleLookupError, approved, by_rule, candidates, find_rule, plan_from,
};
pub use display::plain;
pub use exclusions::{ExclusionError, Exclusions};
pub use plan::{Action, Justification, Plan, PlanEntry};

/// Why an action was refused.
///
/// Per-action execution failures are deliberately absent: those never abort a
/// run and are recorded in the [`ApplyReport`] instead.
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
    #[error(transparent)]
    Io(std::io::Error),
}
