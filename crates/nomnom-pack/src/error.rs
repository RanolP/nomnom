//! Every way acquiring a pack can fail.
//!
//! Two of these carry more than a status code on purpose. A `git` invocation
//! that failed on authentication and one that failed on a bad ref exit with
//! the same code and are completely different problems, so [`Error::Git`]
//! carries the command and its stderr. And a checksum mismatch is not a
//! version skew to be papered over — `docs/lang.md` calls it a supply-chain
//! event, so [`Error::Drift`] says that in the message a user actually reads.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`{url}` is not a pack URL: {reason}")]
    Url { url: String, reason: String },

    #[error(
        "`{program}` is not on PATH, so {attempting} could not run\n  \
         nomnom shells out to the installed git binary to fetch packs; install git, or put it on PATH"
    )]
    GitMissing { program: String, attempting: String },

    #[error("{attempting} failed ({status})\n  command: {command}\n  stderr: {stderr}")]
    Git { attempting: String, command: String, status: String, stderr: String },

    #[error(
        "{url} has no ref named `{reference}`\n  a pack is pinned to a commit, so the ref has to resolve to one"
    )]
    RefNotFound { url: String, reference: String },

    #[error("{url} resolved `{reference}` to `{got}`, which is not a 40-character commit SHA")]
    NotACommit { url: String, reference: String, got: String },

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {message}")]
    Lock { path: PathBuf, message: String },

    #[error("no pack named `{name}` in .nomnom/packs.lock — run `nomnom pack add <url>` first")]
    NotLocked { name: String },

    #[error("pack `{name}` is a local directory entry with no URL, so there is nothing to fetch")]
    NotAGitPack { name: String },

    #[error(transparent)]
    Drift(#[from] Box<Drift>),

    #[error("could not find a home directory to cache packs in ({tried})")]
    NoCacheRoot { tried: String },

    #[error(transparent)]
    Pack(#[from] nomnom_lang::pack::PackError),
}

/// A pack's files no longer hash to what the lock recorded for the commit it
/// is pinned to. Boxed away from [`Error`] so the common error stays small.
#[derive(Debug, thiserror::Error)]
#[error(
    "pack `{name}` changed under the commit it is pinned to ({sha})\n  \
     expected checksum {expected}\n  \
     actual checksum   {actual}\n  \
     in {dir}\n  \
     A pack whose files differ from what the lock recorded for a fixed commit is a \
     supply-chain event, not an upgrade, so nomnom refuses to load it rather than \
     re-fetching. Inspect that directory, then re-add the pack deliberately if the \
     change is one you intend."
)]
pub struct Drift {
    pub name: String,
    pub sha: String,
    pub expected: String,
    pub actual: String,
    pub dir: PathBuf,
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
        Error::Io { path: path.into(), source }
    }
}
