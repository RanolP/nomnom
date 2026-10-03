//! The shape a valid rule file has after parsing.
//!
//! Everything in here is already checked: a [`Rule`] that exists has a
//! non-empty description whose `{holes}` all resolve, a kind the pack knows, a
//! disposition no stronger than its kind's,
//! and a filter whose every variable is bound and whose every field test is
//! well-typed against [`crate::vocab`]. An evaluator walking this tree has no
//! validation left to do and no error case to invent.
//!
//! These types deliberately do not reference `nomnom-core`. The mapping from a
//! [`Rule`] to a `Verdict` belongs to whoever owns both.

use crate::diagnostic::Span;
use crate::kind::Kind;
use crate::vocab::{Field, Ty};

/// A value paired with where it was written, so a later error about it can
/// still point at the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: Span) -> Self {
        Spanned { value, span }
    }
}

/// One `[[rule]]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// What a verdict cites as the rule that produced it.
    pub title: Spanned<String>,
    /// The sentence template a human approves a deletion on.
    pub description: Spanned<String>,
    /// The kind this rule concludes, already resolved against the kinds the
    /// pack may use.
    pub kind: Spanned<Kind>,
    /// The kind's default unless the rule downgraded it.
    pub disposition: Disposition,
    pub filter: Filter,
    /// From the title's text to the `'''` closing the filter.
    pub span: Span,
}

/// `filter = '''...'''`: every constraint holds of [`Filter::var`], and `then`
/// names the node the verdict lands on, relative to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// The node variable `then` starts from, without its `$`.
    pub var: Spanned<String>,
    /// In written order. The evaluator reorders by cost; the meaning is the
    /// conjunction, so order carries none.
    pub constraints: Vec<Constraint>,
    pub then: Target,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    /// `$v has a | b as $m` and `$v lacks a | b`.
    Children(ChildTest),
    /// `$v under Name/`: some strict ancestor of `$v` has this name.
    Under(Spanned<String>),
    /// `$v.field`, `not $v.field`, `$v.field op literal`.
    Field(FieldTest),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildTest {
    /// `false` for `has`, `true` for `lacks`.
    pub negated: bool,
    /// The alternatives, in written order. The first one some child matches is
    /// the one a capture reports.
    pub names: Vec<Spanned<NamePattern>>,
    /// `as $m`, without the `$`. Only `has` can capture.
    pub capture: Option<Spanned<String>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldTest {
    pub field: Spanned<Field>,
    /// `None` for a bare bool field.
    pub compare: Option<(Spanned<CmpOp>, Spanned<Literal>)>,
    /// A leading `not`.
    pub negated: bool,
    pub span: Span,
}

/// One file-name component as written: compared by the platform's name
/// equality when literal, by a glob when it holds `*`, `?`, `[` or `{`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NamePattern {
    Literal(String),
    Glob(String),
}

impl NamePattern {
    pub fn of(text: &str) -> NamePattern {
        if text.contains(['*', '?', '[', '{']) {
            NamePattern::Glob(text.to_owned())
        } else {
            NamePattern::Literal(text.to_owned())
        }
    }

    pub fn text(&self) -> &str {
        match self {
            NamePattern::Literal(text) | NamePattern::Glob(text) => text,
        }
    }

    pub fn literal(&self) -> Option<&str> {
        match self {
            NamePattern::Literal(text) => Some(text),
            NamePattern::Glob(_) => None,
        }
    }
}

/// `then $v/a/b/`: the path from the variable down to the verdict's node.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Empty for `then $v` and `then $v/`, where the variable's own node is
    /// the target.
    pub segments: Vec<Spanned<NamePattern>>,
    /// A trailing `/`: the target must be a directory.
    pub dir: bool,
    pub span: Span,
}

/// A literal value. Sizes are bytes and durations are seconds — the lexer
/// already did the unit arithmetic.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Num(f64),
    Size(u64),
    Duration(u64),
    Bool(bool),
}

impl Literal {
    pub fn ty(&self) -> Ty {
        match self {
            Literal::Str(_) => Ty::Str,
            Literal::Num(_) => Ty::Num,
            Literal::Size(_) => Ty::Size,
            Literal::Duration(_) => Ty::Duration,
            Literal::Bool(_) => Ty::Bool,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    pub const ALL: [CmpOp; 6] = [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Gt, CmpOp::Le, CmpOp::Ge];

    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
        }
    }

    pub fn lookup(symbol: &str) -> Option<CmpOp> {
        CmpOp::ALL.into_iter().find(|op| op.symbol() == symbol)
    }

    /// `==` and `!=` work for every type; the rest need an ordering.
    pub fn needs_order(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }
}

/// `keep`, `reclaimable` or `review`.
///
/// Mirrors `nomnom_core::verdict::Disposition` without depending on it: the
/// language must be parseable and testable with no filesystem crate loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    Keep,
    Reclaimable,
    Review,
}

impl Disposition {
    pub const ALL: [Disposition; 3] =
        [Disposition::Keep, Disposition::Reclaimable, Disposition::Review];

    pub fn name(self) -> &'static str {
        match self {
            Disposition::Keep => "keep",
            Disposition::Reclaimable => "reclaimable",
            Disposition::Review => "review",
        }
    }

    pub fn lookup(name: &str) -> Option<Disposition> {
        Disposition::ALL.into_iter().find(|d| d.name() == name)
    }

    /// How close to a deletion this disposition is. A rule may move its kind's
    /// disposition down this scale and never up it.
    pub fn strength(self) -> u8 {
        match self {
            Disposition::Keep => 0,
            Disposition::Review => 1,
            Disposition::Reclaimable => 2,
        }
    }
}
