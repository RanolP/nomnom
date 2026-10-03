//! The pack list one run loads, in `docs/lang.md`'s resolution order.
//!
//! `nomnom-pack` resolves tiers 2 to 4 — user, project, `--pack` — and says so
//! in its own module docs: "The built-in packs are compiled into `nomnom-core`
//! and are that crate's to put in front." This is that crate doing it, in one
//! place, so `suggest` and `clean` cannot disagree about what a run loads.

use std::path::{Path, PathBuf};

use nomnom_pack::{Lock, PackSource, Resolver, Store, Tier, Trust};

use super::{TrustedPack, builtin_packs};

/// The built-in packs first, then every pack the lock and the tiers resolve to.
///
/// `project_root` is the scan root, which is where `.nomnom/packs.lock` and
/// `.nomnom/packs/` are looked for — not the shell's working directory, so the
/// same command loads the same rules wherever it was typed.
pub fn resolve_packs(
    project_root: &Path,
    explicit: &[PathBuf],
) -> Result<Vec<TrustedPack>, nomnom_pack::Error> {
    let mut packs = TrustedPack::builtins();

    let store = Store::open()?;
    let lock = Lock::load(project_root)?;
    let resolver = Resolver::new(store, project_root).with_explicit(explicit.iter().cloned());
    for source in resolver.resolve(&lock)? {
        packs.push(TrustedPack { pack: source.load()?, trust: source.trust });
    }

    debug_assert!(
        packs[..builtin_packs().len()].iter().all(|pack| matches!(pack.trust, Trust::Builtin))
    );
    Ok(packs)
}

/// Every pack tier 2 to 4 resolves to for this project, without loading any.
pub fn resolve_sources(
    project_root: &Path,
    explicit: &[PathBuf],
    lock: &Lock,
) -> Result<Vec<PackSource>, nomnom_pack::Error> {
    let store = Store::open()?;
    Resolver::new(store, project_root).with_explicit(explicit.iter().cloned()).resolve(lock)
}

/// One pack a run would load, as an inventory lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRow {
    pub name: String,
    /// `None` for a built-in pack, which belongs to no resolution tier.
    pub tier: Option<Tier>,
    pub trust: Trust,
    /// The pinned commit, or `None` for a built-in pack and for a pack that
    /// lives in a directory rather than in git.
    pub sha: Option<String>,
    pub url: Option<String>,
    /// `None` for a built-in pack, which is compiled in.
    pub dir: Option<PathBuf>,
}

/// The built-in packs, then every pack the project's lock and tiers resolve.
pub fn pack_inventory(
    project_root: &Path,
    explicit: &[PathBuf],
) -> Result<Vec<PackRow>, nomnom_pack::Error> {
    let lock = Lock::load(project_root)?;
    let mut rows: Vec<PackRow> = builtin_packs()
        .iter()
        .map(|pack| PackRow {
            name: pack.name.clone(),
            tier: None,
            trust: Trust::Builtin,
            sha: None,
            url: None,
            dir: None,
        })
        .collect();
    for source in resolve_sources(project_root, explicit, &lock)? {
        let locked = lock.get(&source.name);
        rows.push(PackRow {
            name: source.name,
            tier: Some(source.tier),
            trust: source.trust,
            sha: locked.and_then(|pack| pack.sha.clone()),
            url: locked.and_then(|pack| pack.url.clone()),
            dir: Some(source.dir),
        });
    }
    Ok(rows)
}

/// The pack a name refers to, as a human is shown it before a trust change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPack {
    pub name: String,
    pub url: Option<String>,
    pub sha: Option<String>,
    pub dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PackLookupError {
    #[error(transparent)]
    Pack(#[from] nomnom_pack::Error),
    #[error(
        "no pack named `{name}` resolves for this project\n  \
         resolved packs: {}\n  \
         `nomnom pack list` shows them with their tiers",
        name_list(.resolved)
    )]
    NotFound { name: String, resolved: Vec<String> },
}

fn name_list(names: &[String]) -> String {
    if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
}

/// The pack `name` refers to.
///
/// A pack can be known to the lock, or be a directory the lock has never
/// mentioned, or be neither — and the third case is a typo, which must not
/// silently create a trust row for a pack that does not exist.
pub fn find_pack(
    project_root: &Path,
    name: &str,
    explicit: &[PathBuf],
    lock: &Lock,
) -> Result<KnownPack, PackLookupError> {
    if let Some(locked) = lock.get(name)
        && locked.git().is_some()
    {
        let dir = nomnom_pack::materialize(&Store::open()?, locked)?;
        return Ok(KnownPack {
            name: name.to_string(),
            url: locked.url.clone(),
            sha: locked.sha.clone(),
            dir,
        });
    }
    let sources = resolve_sources(project_root, explicit, lock)?;
    match sources.iter().find(|source| source.name == name) {
        Some(source) => {
            Ok(KnownPack { name: name.to_string(), url: None, sha: None, dir: source.dir.clone() })
        }
        None => Err(PackLookupError::NotFound {
            name: name.to_string(),
            resolved: sources.into_iter().map(|s| s.name).collect(),
        }),
    }
}
