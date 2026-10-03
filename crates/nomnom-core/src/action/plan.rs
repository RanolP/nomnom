//! What we intend to do, and the guards that decide whether we are allowed to.
//!
//! A [`Plan`] is the reviewable artifact: `nomnom clean` prints it and stops,
//! because dry-run is the default mode. Nothing here touches the filesystem
//! beyond reading path metadata.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ActionError;

/// One operation on one path.
///
/// [`Action::Delete`] is permanent: no recycle bin, because a recycled path
/// frees no space until the bin is emptied. The other two are renames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Delete { path: PathBuf },
    Archive { path: PathBuf, to: PathBuf },
    Move { path: PathBuf, to: PathBuf },
}

impl Action {
    /// The path the action consumes.
    pub fn path(&self) -> &Path {
        match self {
            Action::Delete { path } | Action::Archive { path, .. } | Action::Move { path, .. } => {
                path
            }
        }
    }

    /// Where it goes, for the two actions that name a destination.
    pub fn destination(&self) -> Option<&Path> {
        match self {
            Action::Delete { .. } => None,
            Action::Archive { to, .. } | Action::Move { to, .. } => Some(to),
        }
    }
}

/// Why an action is proposed, and who proposed it.
///
/// Both halves travel together because they answer one question between them:
/// the sentence says what the evidence is, and the pack and rule say who is
/// making the claim. With packs coming from the network the second half is not
/// optional — `docs/lang.md` requires "why does nomnom want to delete this" to
/// be answerable down to the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Justification {
    pub reason: String,
    /// The pack that produced the reason, empty when a caller wrote the
    /// sentence by hand rather than getting it from a rule.
    pub pack: String,
    /// The rule within that pack, empty on the same terms.
    pub rule: String,
}

impl Justification {
    pub fn new(
        reason: impl Into<String>,
        pack: impl Into<String>,
        rule: impl Into<String>,
    ) -> Self {
        Self { reason: reason.into(), pack: pack.into(), rule: rule.into() }
    }
}

/// A bare sentence with no rule behind it — a guard message, a test fixture.
impl From<&str> for Justification {
    fn from(reason: &str) -> Self {
        Justification::new(reason, "", "")
    }
}

impl From<String> for Justification {
    fn from(reason: String) -> Self {
        Justification::new(reason, "", "")
    }
}

/// One action, the bytes it reclaims, and the justification behind it.
///
/// `reason` is required rather than optional: it is what a human approves on,
/// and it travels *inside* the entry so no front-end can pair it to the wrong
/// path by holding it in a parallel index-aligned list. `pack` and `rule` ride
/// along for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub action: Action,
    pub bytes: u64,
    pub reason: String,
    #[serde(default)]
    pub pack: String,
    #[serde(default)]
    pub rule: String,
}

/// An ordered, reviewable set of actions rooted at one directory.
///
/// Every path that enters through [`Plan::push`] has already passed the guards;
/// a `Plan` that arrives by deserialization has not, which is why
/// [`super::apply`] calls [`Plan::validate`] again before it moves anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    root: PathBuf,
    entries: Vec<PlanEntry>,
}

impl Plan {
    /// Anchor a plan at `root`. The root must exist: it is the fence every
    /// action is checked against, and a fence that cannot be resolved is no
    /// fence at all. A drive root is a valid fence, since nomnom scans whole
    /// drives; [`guard_source`] still refuses the root itself as a target.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ActionError> {
        let root = root.as_ref();
        reject_parent_dir(root)?;
        let root =
            root.canonicalize().map_err(|_| ActionError::RootUnreadable(root.to_path_buf()))?;
        Ok(Self { root, entries: Vec::new() })
    }

    /// Add an action, running every guard first. The stored action carries
    /// canonical paths, so the apply report later records unambiguous ones.
    pub fn push(
        &mut self,
        action: Action,
        bytes: u64,
        justification: impl Into<Justification>,
    ) -> Result<(), ActionError> {
        let action = self.guard(&action)?;
        let Justification { reason, pack, rule } = justification.into();
        self.entries.push(PlanEntry { action, bytes, reason, pack, rule });
        Ok(())
    }

    /// Re-run every guard. Cheap, and the only thing standing between a
    /// deserialized plan and someone's drive root.
    pub fn validate(&self) -> Result<(), ActionError> {
        for entry in &self.entries {
            self.guard(&entry.action)?;
        }
        Ok(())
    }

    fn guard(&self, action: &Action) -> Result<Action, ActionError> {
        let source = guard_source(action.path(), &self.root)?;
        Ok(match action {
            Action::Delete { .. } => Action::Delete { path: source },
            Action::Archive { to, .. } => {
                let to = guard_destination(to, &source)?;
                Action::Archive { path: source, to }
            }
            Action::Move { to, .. } => {
                let to = guard_destination(to, &source)?;
                Action::Move { path: source, to }
            }
        })
    }

    /// The directory every action is fenced inside.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn actions(&self) -> &[PlanEntry] {
        &self.entries
    }

    /// Bytes the whole plan reclaims.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A source path is acceptable when it is absolute, contains no `..`, is not a
/// filesystem or drive root, is not the clean root itself, and lies inside it.
fn guard_source(path: &Path, root: &Path) -> Result<PathBuf, ActionError> {
    reject_parent_dir(path)?;
    if !path.is_absolute() {
        return Err(ActionError::RelativePath(path.to_path_buf()));
    }
    // A path that no longer exists cannot be canonicalized, and cannot be
    // harmed either: fall back to the lexical form so the containment guard
    // still runs and let the action fail at execution time instead.
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    reject_parent_dir(&resolved)?;
    if resolved.parent().is_none() {
        return Err(ActionError::FilesystemRoot(resolved));
    }
    if resolved == root {
        return Err(ActionError::IsCleanRoot(resolved));
    }
    if !resolved.starts_with(root) {
        return Err(ActionError::OutsideRoot { path: resolved, root: root.to_path_buf() });
    }
    Ok(resolved)
}

/// A destination is acceptable when it is absolute, contains no `..`, is not a
/// drive root, does not already exist, and is not inside the thing being moved.
///
/// It is deliberately *not* fenced to the clean root: an archive staging
/// directory normally lives outside the tree being cleaned.
fn guard_destination(to: &Path, source: &Path) -> Result<PathBuf, ActionError> {
    reject_parent_dir(to)?;
    if !to.is_absolute() {
        return Err(ActionError::RelativePath(to.to_path_buf()));
    }
    let resolved = absolutize(to)?;
    reject_parent_dir(&resolved)?;
    if resolved.parent().is_none() {
        return Err(ActionError::FilesystemRoot(resolved));
    }
    if resolved.exists() {
        return Err(ActionError::DestinationExists(resolved));
    }
    if resolved.starts_with(source) {
        return Err(ActionError::DestinationInsideSource {
            path: source.to_path_buf(),
            destination: resolved,
        });
    }
    Ok(resolved)
}

/// Canonicalize as much of `path` as exists, then re-append the rest.
///
/// `Path::canonicalize` needs the whole path to exist, but an archive
/// destination is exactly the path that does not exist yet.
fn absolutize(path: &Path) -> Result<PathBuf, ActionError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(OsString::from) else {
            return Err(ActionError::FilesystemRoot(existing));
        };
        let Some(parent) = existing.parent().map(Path::to_path_buf) else {
            return Err(ActionError::FilesystemRoot(existing));
        };
        tail.push(name);
        existing = parent;
    }
    let mut out = existing.canonicalize().map_err(ActionError::Io)?;
    for name in tail.iter().rev() {
        out.push(name);
    }
    Ok(out)
}

/// `..` never survives this module. Canonicalization would silently resolve it,
/// so the rejection has to happen on the raw components.
fn reject_parent_dir(path: &Path) -> Result<(), ActionError> {
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(ActionError::ParentTraversal(path.to_path_buf()));
    }
    Ok(())
}
