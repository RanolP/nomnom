//! nomnom core: file-tree analysis and cleanup, with zero UI.
//!
//! Four domains, each owning one question:
//! - [`scan`] — what is on disk (facts only)
//! - [`catalog`] — the tree model with rolled-up aggregates
//! - [`verdict`] — what a path IS and whether it should go
//! - [`action`] — plan and apply

pub mod action;
pub mod catalog;
pub mod scan;
pub mod timings;
pub mod verdict;

/// Every user-facing capability nomnom has. The CLI and the GUI each match
/// on this exhaustively, one subcommand or entry point per variant, so a
/// variant added here fails to compile in whichever front-end has not grown it
/// yet: the two cannot drift apart in what they can do.
///
/// Presentation that only one medium has (hover, a treemap, revealing a path
/// in Explorer) is not a feature here; a capability is something a user can
/// get done, which both front-ends must offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// The fixed drives, with label, filesystem and capacity.
    Drives,
    /// A drive's tree with rolled-up sizes, biggest first.
    Tree,
    /// What each path is, and whether it can go.
    Suggest,
    /// A dry-run cleanup plan, applied on request to the recycle bin.
    Clean,
    /// The rule packs a drive's runs load, and their trust.
    Packs,
}

impl Feature {
    /// Every variant. Parity tests iterate this, so a new variant belongs here
    /// as well as in each front-end's match.
    pub const ALL: [Feature; 5] =
        [Feature::Drives, Feature::Tree, Feature::Suggest, Feature::Clean, Feature::Packs];
}
