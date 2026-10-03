//! The cap that keeps a stranger's repository from choosing what to delete.
//!
//! `docs/lang.md`: a rule from any pack other than the built-in ones is capped
//! at `disposition = review` until the user runs `nomnom pack trust <name>`.
//!
//! The cap is a **value the judge applies**, never an edit to a loaded
//! [`nomnom_lang::pack::Pack`]. Downgrading in place would make the verdict
//! indistinguishable from one the rule wrote as `review` itself, and then the
//! CLI could not answer "why is this only a review?" — which is the whole
//! point of having the cap. So [`Trust::cap`] returns a [`Capped`] that
//! remembers what the rule asked for.

use nomnom_lang::ast::Disposition;

/// Whether a pack's verdicts may propose a deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trust {
    /// A pack compiled into the binary. Never capped: it is us.
    Builtin,
    /// The user ran `nomnom pack trust <name>`.
    Trusted,
    /// Everything else, including a pack that has only just been added.
    Untrusted,
}

impl Trust {
    /// True when this pack's `reclaimable` verdicts get downgraded. The
    /// predicate the judge consults before bothering with [`Trust::cap`].
    pub fn caps(self) -> bool {
        self == Trust::Untrusted
    }

    /// Applies the cap to one rule's disposition.
    ///
    /// Only `reclaimable` moves. `keep` is the safe direction already and
    /// `review` is where the cap lands, so both pass through unchanged and
    /// report no downgrade.
    pub fn cap(self, disposition: Disposition) -> Capped {
        if self.caps() && disposition == Disposition::Reclaimable {
            Capped { disposition: Disposition::Review, downgraded_from: Some(disposition) }
        } else {
            Capped { disposition, downgraded_from: None }
        }
    }
}

/// A disposition after the trust cap, carrying what it was before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capped {
    /// What the verdict uses.
    pub disposition: Disposition,
    /// What the rule asked for, when that differs. `None` means the rule got
    /// what it wrote.
    pub downgraded_from: Option<Disposition>,
}

impl Capped {
    pub fn was_downgraded(self) -> bool {
        self.downgraded_from.is_some()
    }

    /// The sentence the CLI prints beside a downgraded verdict. `None` when
    /// nothing was downgraded, so a caller can `if let` its way to the line
    /// without deciding when to show it.
    pub fn explanation(self, pack: &str) -> Option<String> {
        let from = self.downgraded_from?;
        Some(format!(
            "capped at review: pack `{pack}` is not trusted, so its `{}` became `{}` — \
             run `nomnom pack trust {pack}` to accept it",
            from.name(),
            self.disposition.name()
        ))
    }
}
