//! The nomnom rule language: text in, validated AST out.
//!
//! A rule says what a path **is**. The language is total — no assignment, no
//! loops, no user-defined functions — so a pack downloaded from a stranger can
//! misclassify but cannot compute, and its cost is bounded before it runs. See
//! `docs/lang.md` for the specification this implements.
//!
//! The phases, each its own module:
//! - [`diagnostic`] — spans, and the rendered error a human fixes a rule from
//! - [`lex`] — one filter line to words, with sizes normalised to bytes and
//!   durations to seconds
//! - [`vocab`] — the one table of fields, read by the parser and by any
//!   evaluator
//! - [`kind`] — what a rule concludes a path is, and that kind's defaults
//! - [`ast`] — the validated shape
//! - [`parse`] — the line-oriented parser, doing every check that needs no
//!   filesystem
//! - [`eval`] — a field test and one node's facts to a yes or no, plus the
//!   `description` rendering that turns a verdict into a sentence
//! - [`pack`] — a directory on disk to a validated [`pack::Pack`]
//!
//! [`eval`] defines what a field test *means* without knowing what a node
//! *is*: every fact reaches it through the [`eval::Facts`] trait, which the
//! caller implements over whatever it already has. So there is no dependency
//! on `nomnom-core` anywhere in this crate, and a rule stays parseable and
//! checkable with nothing but a string.
//!
//! ```
//! use nomnom_lang::{Kinds, Source, parse};
//!
//! let source = Source::new("example.nom", "\
//! [Cargo target/]
//! description = Cargo build output, rebuilt by `cargo build`
//! kind = build-output/v1
//! filter {
//!   $dir has Cargo.toml
//!   then $dir/target/
//! }
//! ");
//! let rules = parse(&source, &Kinds::builtin()).expect("valid rule");
//! assert_eq!(rules[0].title.value, "Cargo target/");
//! assert_eq!(rules[0].confidence, 0.9, "the kind's default");
//! ```

pub mod ast;
pub mod diagnostic;
pub mod eval;
pub mod kind;
pub mod lex;
pub mod pack;
pub mod parse;
pub mod vocab;

pub use ast::{
    ChildTest, CmpOp, Constraint, Disposition, FieldTest, Filter, Literal, NamePattern, Rule,
    Spanned, Target,
};
pub use diagnostic::{Diagnostic, Source, Span};
pub use eval::{Facts, Value, check, render_reason};
pub use kind::{Kind, Kinds};
pub use pack::{Pack, PackError, load};
pub use parse::parse;
pub use vocab::{Field, Ty};
