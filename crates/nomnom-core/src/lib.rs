//! nomnom core: file-tree analysis and cleanup, with zero UI.
//!
//! Four domains, each owning one question:
//! - [`scan`] — what is on disk (facts only)
//! - [`catalog`] — the tree model with rolled-up aggregates
//! - [`verdict`] — what a path IS and whether it should go
//! - [`action`] — plan, apply, undo, with a journal

pub mod action;
pub mod catalog;
pub mod scan;
pub mod verdict;

/// Every user-facing capability nomnom has. The CLI and the GUI each match
/// on this exhaustively, one subcommand or screen per variant, so a variant
/// added here fails to compile in whichever front-end has not grown it yet:
/// the two cannot drift apart in what they can do.
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
    /// A drive's bytes and file counts per extension.
    FileTypes,
    /// A drive's largest files.
    LargestFiles,
    /// What each path is, and whether it can go.
    Suggest,
    /// A dry-run cleanup plan, applied on request with a journal.
    Clean,
    /// The rule packs a drive's runs load, and their trust.
    Packs,
    /// Reversing an apply from its journal.
    Undo,
}

impl Feature {
    /// Every variant, in the GUI's sidebar order. Parity tests iterate this,
    /// so a new variant belongs here as well as in each front-end's match.
    pub const ALL: [Feature; 8] = [
        Feature::Drives,
        Feature::Tree,
        Feature::FileTypes,
        Feature::LargestFiles,
        Feature::Suggest,
        Feature::Clean,
        Feature::Packs,
        Feature::Undo,
    ];
}
