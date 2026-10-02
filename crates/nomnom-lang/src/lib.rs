//! The nomnom rule language: text in, validated AST out.
//!
//! A rule says what a path **is**. The language is total — no assignment, no
//! loops, no user-defined functions — so a pack downloaded from a stranger can
//! misclassify but cannot compute, and its cost is bounded before it runs. See
//! `docs/lang.md` for the specification this implements.
//!
//! The phases, each its own module:
//! - [`diagnostic`] — spans, and the rendered error a human fixes a rule from
//! - [`lex`] — text to tokens, with sizes normalised to bytes and durations to
//!   seconds
//! - [`vocab`] — the one table of fields and predicates, read by the parser
//!   and by any evaluator
//! - [`ast`] — the validated shape
//! - [`parse`] — recursive descent, doing every check that needs no filesystem
//! - [`eval`] — a validated [`Expr`] and one node's facts to a yes or no, plus
//!   the `reason` template rendering that turns a verdict into a sentence
//! - [`pack`] — a directory on disk to a validated [`pack::Pack`]
//!
//! [`eval`] defines what a rule *means* without knowing what a node *is*:
//! every fact reaches it through the [`eval::Facts`] trait, which the caller
//! implements over whatever it already has. So there is still no dependency on
//! `nomnom-core` anywhere in this crate, and a rule stays exercisable with
//! nothing but a string and a hand-written `Facts`.
//!
//! ```
//! use nomnom_lang::{Source, parse};
//!
//! let source = Source::new("example.nom", r#"
//!     rule "cargo-target" {
//!       when  dir.name == "target" and sibling("Cargo.toml")
//!       then  label       = build-output
//!             disposition = reclaimable
//!             unit        = true
//!             confidence  = 0.95
//!             reason      = "`Cargo.toml` sits beside it"
//!     }
//! "#);
//! let rules = parse(&source).expect("valid rule");
//! assert_eq!(rules[0].name.value, "cargo-target");
//! ```

pub mod ast;
pub mod diagnostic;
pub mod eval;
pub mod lex;
pub mod pack;
pub mod parse;
pub mod vocab;

pub use ast::{CmpOp, Conclusion, Disposition, Expr, Literal, Rule, Spanned};
pub use diagnostic::{Diagnostic, Source, Span};
pub use eval::{Facts, Value, eval, render_reason};
pub use pack::{Pack, PackError, load};
pub use parse::parse;
pub use vocab::{Field, Predicate, Ty};
