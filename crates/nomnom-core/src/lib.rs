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
