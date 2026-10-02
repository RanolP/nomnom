//! The shape a valid rule file has after parsing.
//!
//! Everything in here is already checked: a [`Conclusion`] that exists has a
//! non-empty reason, a confidence inside `0.0..=1.0` and a known disposition,
//! and every [`Expr`] is well-typed against [`crate::vocab`]. An evaluator
//! walking this tree has no validation left to do and no error case to
//! invent — the only thing it can fail at is reading the filesystem.
//!
//! These types deliberately do not reference `nomnom-core`. The mapping from
//! [`Conclusion`] to a `Verdict` belongs to whoever owns both.

use crate::diagnostic::Span;
use crate::vocab::{Field, Predicate, Ty};

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

/// One `rule "name" { when ... then ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: Spanned<String>,
    pub when: Expr,
    pub then: Conclusion,
    /// From the `rule` keyword to the closing brace.
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

    /// `==` and `!=` work for every type; the rest need an ordering.
    pub fn needs_order(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }
}

/// The `when` predicate over one node. Total by construction: no calls a user
/// can define, no loops, no recursion.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Bool(Spanned<bool>),
    /// A bool-typed field used on its own, as in `is_dir and not is_symlink`.
    Field(Spanned<Field>),
    Compare {
        lhs: Spanned<Field>,
        op: Spanned<CmpOp>,
        rhs: Spanned<Literal>,
    },
    Call {
        predicate: Spanned<Predicate>,
        args: Vec<Spanned<Literal>>,
        span: Span,
    },
    Not {
        operand: Box<Expr>,
        span: Span,
    },
    And {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Or {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Bool(it) => it.span,
            Expr::Field(it) => it.span,
            Expr::Compare { lhs, rhs, .. } => lhs.span.to(rhs.span),
            Expr::Call { span, .. } => *span,
            Expr::Not { span, .. } => *span,
            Expr::And { lhs, rhs } | Expr::Or { lhs, rhs } => lhs.span().to(rhs.span()),
        }
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
}

/// What to conclude when the `when` expression holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Conclusion {
    /// What the path is. An identifier; a pack may introduce its own, which is
    /// why this is a `String` and not an enum.
    pub label: Spanned<String>,
    pub disposition: Spanned<Disposition>,
    /// Never empty: checked at parse time, because a reason is what a human
    /// approves a deletion on.
    pub reason: Spanned<String>,
    /// Inside `0.0..=1.0`, checked at parse time.
    pub confidence: Spanned<f32>,
    /// Whether this verdict speaks for the whole subtree. Absent means
    /// `false`, and then there is no span to point at.
    pub unit: Spanned<bool>,
    /// The span of `unit` when it was written, `None` when defaulted.
    pub unit_written: Option<Span>,
    pub span: Span,
}
