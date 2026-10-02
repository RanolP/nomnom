//! The pack list one run loads, in `docs/lang.md`'s resolution order.
//!
//! `nomnom-pack` resolves tiers 2 to 4 — user, project, `--pack` — and says so
//! in its own module docs: "The built-in pack is compiled into `nomnom-core`
//! and is that crate's to put in front." This is that crate doing it, in one
//! place, so `suggest` and `clean` cannot disagree about what a run loads.

use std::path::{Path, PathBuf};

use nomnom_pack::{Lock, Resolver, Store, Trust};

use super::{TrustedPack, builtin_pack};

/// Built-in first, then every pack the lock and the tiers resolve to.
///
/// `project_root` is the scan root, which is where `.nomnom/packs.lock` and
/// `.nomnom/packs/` are looked for — not the shell's working directory, so the
/// same command loads the same rules wherever it was typed.
pub fn resolve_packs(
    project_root: &Path,
    explicit: &[PathBuf],
) -> Result<Vec<TrustedPack>, nomnom_pack::Error> {
    let mut packs = vec![TrustedPack::builtin(builtin_pack().clone())];

    let store = Store::open()?;
    let lock = Lock::load(project_root)?;
    let resolver = Resolver::new(store, project_root).with_explicit(explicit.iter().cloned());
    for source in resolver.resolve(&lock)? {
        packs.push(TrustedPack { pack: source.load()?, trust: source.trust });
    }

    debug_assert!(matches!(packs[0].trust, Trust::Builtin));
    Ok(packs)
}
