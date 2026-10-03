//! The ordered list of pack directories a run loads, with each one's trust state.
//!
//! `docs/lang.md` fixes the order, later overriding earlier:
//!
//! 1. built-in, the `builtin.<domain>` packs compiled into the binary
//! 2. user — `%LOCALAPPDATA%\nomnom\packs\`
//! 3. project — `./.nomnom/packs/`
//! 4. `--pack <dir>`, explicit
//!
//! This resolver returns tiers 2 to 4. The built-in packs are compiled into
//! `nomnom-core` and are that crate's to put in front.
//!
//! Within the user tier, git packs from the lock come first and hand-placed
//! directories after them, so a directory a user dropped in themselves
//! overrides a pack they fetched — the same "later wins" rule the tiers
//! follow, applied to the one case `docs/lang.md` does not mention.

use std::path::{Path, PathBuf};

use nomnom_lang::pack::Pack;
use serde::Deserialize;

use crate::cache::Store;
use crate::error::{Error, Result};
use crate::lock::Lock;
use crate::trust::Trust;

/// Which of the three tiers a pack directory came from. Carried so the CLI can
/// say where a rule came from without re-deriving it from the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    User,
    Project,
    Explicit,
}

/// One pack directory, ready to hand to [`nomnom_lang::pack::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackSource {
    pub name: String,
    pub dir: PathBuf,
    pub tier: Tier,
    pub trust: Trust,
}

impl PackSource {
    pub fn load(&self) -> Result<Pack> {
        Ok(nomnom_lang::pack::load(&self.dir)?)
    }
}

pub struct Resolver {
    store: Store,
    project_root: PathBuf,
    explicit: Vec<PathBuf>,
}

impl Resolver {
    pub fn new(store: Store, project_root: impl Into<PathBuf>) -> Resolver {
        Resolver { store, project_root: project_root.into(), explicit: Vec::new() }
    }

    /// The `--pack <dir>` arguments, in the order they were given.
    pub fn with_explicit(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Resolver {
        self.explicit.extend(dirs);
        self
    }

    pub fn project_dir(&self) -> PathBuf {
        self.project_root.join(".nomnom").join("packs")
    }

    pub fn resolve(&self, lock: &Lock) -> Result<Vec<PackSource>> {
        let mut sources = Vec::new();

        for locked in lock.packs() {
            if locked.git().is_none() {
                continue;
            }
            let dir = crate::materialize(&self.store, locked)?;
            sources.push(PackSource {
                name: locked.name.clone(),
                dir,
                tier: Tier::User,
                trust: locked.trust(),
            });
        }
        sources.extend(self.scan(self.store.root(), Tier::User, lock)?);
        sources.extend(self.scan(&self.project_dir(), Tier::Project, lock)?);

        for dir in &self.explicit {
            let name = manifest_name(dir)?;
            let trust = lock.trust_of(&name);
            sources.push(PackSource { name, dir: dir.clone(), tier: Tier::Explicit, trust });
        }
        Ok(sources)
    }

    /// Direct children of `dir` that hold a `pack.toml`. The fetch cache lives
    /// under the same root as the user tier but is always three levels down
    /// under a host name, so it never matches here.
    fn scan(&self, dir: &Path, tier: Tier, lock: &Lock) -> Result<Vec<PackSource>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::io(dir, source)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::io(dir, source))?;
            let path = entry.path();
            if !path.join("pack.toml").is_file() {
                continue;
            }
            let name = manifest_name(&path)?;
            let trust = lock.trust_of(&name);
            found.push(PackSource { name, dir: path, tier, trust });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }
}

#[derive(Deserialize)]
struct NameOnly {
    name: String,
}

/// The `name` out of `dir/pack.toml`, without parsing the rules. Trust is
/// keyed by name, so the name has to be known before deciding whether the
/// pack is even worth loading.
pub fn manifest_name(dir: &Path) -> Result<String> {
    let path = dir.join("pack.toml");
    let text = std::fs::read_to_string(&path).map_err(|source| Error::io(&path, source))?;
    let manifest: NameOnly =
        toml::from_str(&text).map_err(|error| Error::Lock { path, message: error.to_string() })?;
    Ok(manifest.name)
}
