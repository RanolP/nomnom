//! Acquiring rule packs from git: fetch, pin, cache, lock, trust.
//!
//! `nomnom-lang` answers whether a directory is a usable pack. This crate
//! supplies the directory — and the three properties `docs/lang.md` asks of
//! the way it was obtained:
//!
//! - **pinned.** A pack is resolved to a full commit SHA and recorded by SHA.
//!   A branch name is a record of what was asked for, never what is loaded.
//! - **locked.** `.nomnom/packs.lock` is what a project loads. Content that
//!   drifts under a fixed commit is a supply-chain event and an error, not an
//!   upgrade.
//! - **capped.** A rule from any pack but the built-in one is capped at
//!   `disposition = review` until the pack is trusted, and the cap is a value
//!   the judge applies so the CLI can say why.
//!
//! ```no_run
//! # fn main() -> Result<(), nomnom_pack::Error> {
//! use nomnom_pack::{Lock, Resolver, Store};
//!
//! let store = Store::open()?;
//! let root = std::path::Path::new(".");
//! let mut lock = Lock::load(root)?;
//!
//! nomnom_pack::add(&store, &mut lock, "github.com/ranolp/nomnom-packs/rust")?;
//! lock.trust("rust");
//! lock.save(root)?;
//!
//! for source in Resolver::new(store, root).resolve(&lock)? {
//!     let pack = source.load()?;
//!     println!("{} ({:?}, {:?})", pack.name, source.tier, source.trust);
//! }
//! # Ok(()) }
//! ```

pub mod cache;
pub mod checksum;
pub mod error;
pub mod git;
pub mod lock;
pub mod resolve;
pub mod trust;
pub mod url;

use std::path::PathBuf;

pub use cache::Store;
pub use error::{Error, Result};
pub use git::Git;
pub use lock::{Lock, LockedPack};
pub use resolve::{PackSource, Resolver, Tier};
pub use trust::{Capped, Trust};
pub use url::PackUrl;

/// `nomnom pack add <url>`: resolve the ref to a commit, fetch it, validate
/// what came down, and write the row into the lock.
///
/// The pack's name comes out of its own `pack.toml` rather than out of the
/// URL, because the name is the key everything else uses — trust, provenance
/// in a verdict, `nomnom pack trust <name>` — and a URL's last path segment is
/// only a guess at it.
pub fn add(store: &Store, lock: &mut Lock, url_text: &str) -> Result<LockedPack> {
    let url = PackUrl::parse(url_text)?;
    let sha = store.git().resolve_sha(&url.git_url, url.reference.as_deref())?;
    let dir = store.pack_dir(&url, &sha)?;
    let pack = nomnom_lang::pack::load(&dir)?;
    let checksum = checksum::of_dir(&dir)?;

    let locked = LockedPack {
        name: pack.name,
        url: Some(url.raw.clone()),
        sha: Some(sha),
        subdir: url.subdir.clone(),
        checksum: Some(checksum),
        trusted: false,
    };
    lock.upsert(locked.clone());
    // `upsert` keeps trust already granted, so read the row back rather than
    // handing out the one we built.
    Ok(lock.get(&locked.name).cloned().unwrap_or(locked))
}

/// `nomnom pack update <name>`: the one operation allowed to move a pin. It
/// re-resolves the ref recorded in the lock, so a pack added at `@main` moves
/// to whatever `main` points at now — deliberately, on the user's word.
pub fn update(store: &Store, lock: &mut Lock, name: &str) -> Result<LockedPack> {
    let locked = lock.get(name).ok_or_else(|| Error::NotLocked { name: name.to_string() })?;
    let url = locked.url.clone().ok_or_else(|| Error::NotAGitPack { name: name.to_string() })?;
    add(store, lock, &url)
}

/// The pack directory for a locked row, fetching only if the cache does not
/// already have that commit, and refusing a pack whose content no longer
/// matches what the lock recorded.
pub fn materialize(store: &Store, locked: &LockedPack) -> Result<PathBuf> {
    let (url_text, sha) =
        locked.git().ok_or_else(|| Error::NotAGitPack { name: locked.name.clone() })?;
    let url = PackUrl::parse(url_text)?;
    let checkout = store.ensure(&url, sha)?;
    let dir = match &locked.subdir {
        Some(subdir) => checkout.join(subdir.replace('/', std::path::MAIN_SEPARATOR_STR)),
        None => checkout,
    };

    if let Some(expected) = &locked.checksum {
        let actual = checksum::of_dir(&dir)?;
        if &actual != expected {
            return Err(Box::new(error::Drift {
                name: locked.name.clone(),
                sha: sha.to_string(),
                expected: expected.clone(),
                actual,
                dir,
            })
            .into());
        }
    }
    Ok(dir)
}
