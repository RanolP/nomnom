//! What a rule concludes a path *is*: `build-output/v1`, `cache/v1`, ….
//!
//! A kind carries the default disposition every rule of that kind starts
//! from, so a rule says what it found and the kind says what that finding is
//! worth. The version is part of the name because a
//! kind's meaning is a contract with whoever reads the verdict: a pack written
//! against `cache/v1` must be refused, not silently reinterpreted, once the
//! only `cache` that exists is `cache/v2`.
//!
//! The built-in kinds are below. A pack adds its own in `pack.toml`, under
//! `[kinds."name/vN"]`, and may not redefine a built-in one.

use std::fmt;

use crate::ast::Disposition;

#[derive(Debug, Clone, PartialEq)]
pub struct Kind {
    /// What a verdict shows as its label.
    pub name: String,
    pub version: u32,
    pub disposition: Disposition,
}

impl Kind {
    pub fn new(name: &str, version: u32, disposition: Disposition) -> Kind {
        Kind { name: name.to_owned(), version, disposition }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/v{}", self.name, self.version)
    }
}

/// `name/vN` split into its parts, or `None` when it is not shaped like one.
pub fn split_id(id: &str) -> Option<(&str, u32)> {
    let (name, version) = id.rsplit_once("/v")?;
    let valid_name = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let valid_version = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
    if !valid_name || !valid_version {
        return None;
    }
    Some((name, version.parse().ok()?))
}

/// Names no pack may declare: the built-in kinds.
pub const RESERVED: &[&str] = &["build-output", "cache", "stale-download"];

/// The kinds one pack's rules may use.
#[derive(Debug, Clone, PartialEq)]
pub struct Kinds {
    kinds: Vec<Kind>,
}

impl Kinds {
    pub fn builtin() -> Kinds {
        Kinds {
            kinds: vec![
                Kind::new("build-output", 1, Disposition::Reclaimable),
                Kind::new("cache", 1, Disposition::Reclaimable),
                Kind::new("stale-download", 1, Disposition::Review),
            ],
        }
    }

    /// The built-in kinds plus a pack's own. The caller has already refused a
    /// declaration of a [`RESERVED`] name.
    pub fn with(mut self, declared: impl IntoIterator<Item = Kind>) -> Kinds {
        self.kinds.extend(declared);
        self
    }

    pub fn lookup(&self, name: &str, version: u32) -> Option<&Kind> {
        self.kinds.iter().find(|kind| kind.name == name && kind.version == version)
    }

    /// Every version of `name` there is, for the message refusing a version
    /// that is not one of them.
    pub fn versions(&self, name: &str) -> Vec<u32> {
        self.kinds.iter().filter(|kind| kind.name == name).map(|kind| kind.version).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Kind> {
        self.kinds.iter()
    }
}
