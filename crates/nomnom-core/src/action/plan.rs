//! What we intend to do, and the guards that decide whether we are allowed to.
//!
//! A [`Plan`] is the reviewable artifact: `nomnom clean` prints it and stops,
//! because dry-run is the default mode. Nothing here touches the filesystem
//! beyond reading path metadata.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::ActionError;

/// One reversible operation on one path.
///
/// There is no hard delete. [`Action::Trash`] goes to the OS recycle bin; the
/// other two are renames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Trash { path: PathBuf },
    Archive { path: PathBuf, to: PathBuf },
    Move { path: PathBuf, to: PathBuf },
}

impl Action {
    /// The path the action consumes.
    pub fn path(&self) -> &Path {
        match self {
            Action::Trash { path } | Action::Archive { path, .. } | Action::Move { path, .. } => {
                path
            }
        }
    }

    /// Where it goes, for the two actions that name a destination.
    pub fn destination(&self) -> Option<&Path> {
        match self {
            Action::Trash { .. } => None,
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
    /// For a likely copy, the original it must equal byte for byte before it
    /// may be trashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<PathBuf>,
    /// Set only by [`Plan::verify_copies`], never by deserialization, so a
    /// plan read back from disk has to be verified again before its copies
    /// can go.
    #[serde(skip)]
    confirmed: Option<Confirmed>,
}

impl PlanEntry {
    /// Whether this is a copy [`Plan::verify_copies`] found byte-identical to
    /// its original.
    pub fn is_confirmed_copy(&self) -> bool {
        self.copy_of.is_some() && self.confirmed.is_some()
    }

    /// Why apply must not trash this entry now, for a copy: never verified,
    /// or the copy or its original changed since.
    pub(super) fn copy_refusal(&self) -> Option<String> {
        let original = self.copy_of.as_ref()?;
        let copy = self.action.path();
        let Some(confirmed) = &self.confirmed else {
            return Some(format!(
                "refusing to trash {}: it was never verified byte-identical to {}",
                copy.display(),
                original.display()
            ));
        };
        if stamp(copy).ok() != Some(confirmed.copy)
            || stamp(original).ok() != Some(confirmed.original)
        {
            return Some(format!(
                "refusing to trash {}: it or its original {} changed since they were verified",
                copy.display(),
                original.display()
            ));
        }
        None
    }
}

/// Size and modification time of a copy and its original at the moment their
/// full hashes matched. Apply compares them again just before trashing, so a
/// file rewritten after verification is not trashed on a stale comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Confirmed {
    copy: Stamp,
    original: Stamp,
}

type Stamp = (u64, Option<SystemTime>);

fn stamp(path: &Path) -> std::io::Result<Stamp> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.len(), meta.modified().ok()))
}

fn full_hash(path: &Path) -> std::io::Result<(Stamp, [u8; 32])> {
    let before = stamp(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    let after = stamp(path)?;
    if before != after {
        return Err(std::io::Error::other("it changed while being read"));
    }
    Ok((after, *hasher.finalize().as_bytes()))
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
        self.push_entry(action, bytes, justification.into(), None)
    }

    /// Add the trashing of a likely copy of `original`. Apply refuses it
    /// until [`Plan::verify_copies`] has found the two byte-identical.
    pub fn push_copy(
        &mut self,
        copy: PathBuf,
        original: PathBuf,
        bytes: u64,
        justification: impl Into<Justification>,
    ) -> Result<(), ActionError> {
        let original = original.canonicalize().unwrap_or(original);
        self.push_entry(Action::Trash { path: copy }, bytes, justification.into(), Some(original))
    }

    fn push_entry(
        &mut self,
        action: Action,
        bytes: u64,
        justification: Justification,
        copy_of: Option<PathBuf>,
    ) -> Result<(), ActionError> {
        let action = self.guard(&action)?;
        let Justification { reason, pack, rule } = justification;
        self.entries.push(PlanEntry {
            action,
            bytes,
            reason,
            pack,
            rule,
            copy_of,
            confirmed: None,
        });
        Ok(())
    }

    /// Compare every copy with its original by a full blake3 hash, and drop
    /// each one that is not proven identical, with the reason.
    ///
    /// This is where the duplicate pass's sample becomes certainty, paid only
    /// for the copies a user approved rather than for every same-size file on
    /// the drive. Dropped: a copy whose bytes differ, one that or whose
    /// original cannot be read, and one whose original is itself planned to
    /// go, since trashing both would keep no copy at all.
    pub fn verify_copies(&mut self) -> Vec<(PathBuf, String)> {
        let planned: Vec<PathBuf> =
            self.entries.iter().map(|entry| entry.action.path().to_path_buf()).collect();
        let pending: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].copy_of.is_some() && self.entries[i].confirmed.is_none())
            .collect();
        let originals: BTreeSet<&Path> =
            pending.iter().filter_map(|&i| self.entries[i].copy_of.as_deref()).collect();
        let originals: HashMap<&Path, Result<(Stamp, [u8; 32]), String>> = originals
            .into_par_iter()
            .map(|path| (path, full_hash(path).map_err(|e| e.to_string())))
            .collect();
        let outcomes: Vec<(usize, Result<Confirmed, String>)> = pending
            .par_iter()
            .map(|&i| {
                let entry = &self.entries[i];
                let copy = entry.action.path();
                let original = entry.copy_of.as_deref().expect("pending entries are copies");
                let outcome = if planned.iter().any(|path| original.starts_with(path)) {
                    Err(format!(
                        "its original {} is planned to go too, which would keep no copy",
                        original.display()
                    ))
                } else {
                    match (&originals[original], full_hash(copy)) {
                        (Err(e), _) => Err(format!(
                            "its original {} could not be read to compare: {e}",
                            original.display()
                        )),
                        (_, Err(e)) => Err(format!("it could not be read to compare: {e}")),
                        (Ok((original_stamp, a)), Ok((copy_stamp, b))) if a == &b => {
                            Ok(Confirmed { copy: copy_stamp, original: *original_stamp })
                        }
                        _ => Err(format!(
                            "its contents differ from {}, despite the same size and sampled \
                             head, middle and tail",
                            original.display()
                        )),
                    }
                };
                (i, outcome)
            })
            .collect();
        let mut dropped = Vec::new();
        let mut drop: BTreeSet<usize> = BTreeSet::new();
        for (i, outcome) in outcomes {
            match outcome {
                Ok(confirmed) => self.entries[i].confirmed = Some(confirmed),
                Err(reason) => {
                    dropped.push((self.entries[i].action.path().to_path_buf(), reason));
                    drop.insert(i);
                }
            }
        }
        let mut index = 0;
        self.entries.retain(|_| {
            let keep = !drop.contains(&index);
            index += 1;
            keep
        });
        dropped
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
            Action::Trash { .. } => Action::Trash { path: source },
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
