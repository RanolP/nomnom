//! A validated [`Expr`] plus one node's facts to a yes or no.
//!
//! The evaluator learns nothing by itself. Every fact arrives through the
//! [`Facts`] trait, which the caller implements over whatever it already has —
//! a `nomnom-core` node, a test fixture, a row out of a cache. That is what
//! keeps this crate free of a dependency on `nomnom-core` while still being the
//! place the language's meaning is defined: the rule semantics live here, the
//! filesystem lives on the other side of the trait, and a rule can be exercised
//! with nothing but a string and a hand-written `Facts`.
//!
//! # Absence
//!
//! A node cannot always supply a fact: a file with no recorded access time has
//! no `accessed_age`, and [`Facts::field`] answers [`Value::Absent`]. An
//! `Absent` value makes **every** comparison false, and a bare `Absent` bool
//! field false.
//!
//! `not` therefore turns an unknown fact into `true`, and that is deliberate:
//! `accessed_before(90d)` asks "known to be stale", so `not
//! accessed_before(90d)` asks "not known to be stale", which is exactly the
//! conservative reading a deletion proposal wants. A rule that needs the fact
//! to exist says so — `has_accessed and not accessed_before(90d)`.
//!
//! # Types
//!
//! The parser type-checks every comparison against [`crate::vocab`], so a
//! [`Expr::Compare`] whose [`Value`] and [`Literal`] disagree is unreachable.
//! It answers `false` rather than panicking, because a panic in a rule engine
//! walking a million paths is a worse outcome than a rule that does not fire.
//! Ordering comparisons on strings and bools are likewise unparseable, so only
//! `==` and `!=` ever reach those arms.
//!
//! `Num` is `f64`; `Size` is bytes and `Duration` is seconds, both `u64`,
//! matching what the lexer already normalised the literals to.

use std::cmp::Ordering;

use crate::ast::{CmpOp, Expr, Literal};
use crate::vocab::{Field, Predicate};

/// A fact's value, or `Absent` when the node cannot supply it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
    Size(u64),
    Duration(u64),
    Bool(bool),
    Absent,
}

/// Everything the evaluator can learn about one node.
pub trait Facts {
    fn field(&self, field: Field) -> Value;
    fn predicate(&self, predicate: Predicate, args: &[Literal]) -> bool;
}

/// Does this rule's `when` hold for this node?
pub fn eval(expr: &Expr, facts: &dyn Facts) -> bool {
    match expr {
        Expr::Bool(value) => value.value,
        Expr::Field(field) => matches!(facts.field(field.value), Value::Bool(true)),
        Expr::Compare { lhs, op, rhs } => compare(&facts.field(lhs.value), op.value, &rhs.value),
        Expr::Call { predicate, args, .. } => {
            let args: Vec<Literal> = args.iter().map(|arg| arg.value.clone()).collect();
            facts.predicate(predicate.value, &args)
        }
        Expr::Not { operand, .. } => !eval(operand, facts),
        Expr::And { lhs, rhs } => eval(lhs, facts) && eval(rhs, facts),
        Expr::Or { lhs, rhs } => eval(lhs, facts) || eval(rhs, facts),
    }
}

fn compare(value: &Value, op: CmpOp, literal: &Literal) -> bool {
    match (value, literal) {
        (Value::Str(a), Literal::Str(b)) => match op {
            CmpOp::Eq => str_eq(a, b),
            CmpOp::Ne => !str_eq(a, b),
            // Unreachable: `<` and `>` on strings do not parse.
            _ => false,
        },
        (Value::Num(a), Literal::Num(b)) => holds(a.partial_cmp(b), op),
        (Value::Size(a), Literal::Size(b)) => holds(Some(a.cmp(b)), op),
        (Value::Duration(a), Literal::Duration(b)) => holds(Some(a.cmp(b)), op),
        (Value::Bool(a), Literal::Bool(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            // Unreachable: bools are not ordered.
            _ => false,
        },
        // An absent fact answers nothing, so no comparison about it is true.
        (Value::Absent, _) => false,
        // Unreachable: the parser rejects a comparison between two types.
        _ => false,
    }
}

/// `None` is a NaN comparison, which is false whichever operator asked.
fn holds(ordering: Option<Ordering>, op: CmpOp) -> bool {
    let Some(ordering) = ordering else { return false };
    match op {
        CmpOp::Eq => ordering.is_eq(),
        CmpOp::Ne => ordering.is_ne(),
        CmpOp::Lt => ordering.is_lt(),
        CmpOp::Gt => ordering.is_gt(),
        CmpOp::Le => ordering.is_le(),
        CmpOp::Ge => ordering.is_ge(),
    }
}

/// The platform's own rule for whether two names are the same name.
#[cfg(windows)]
fn str_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(not(windows))]
fn str_eq(a: &str, b: &str) -> bool {
    a == b
}

// -- reason templates ------------------------------------------------------

/// Fill a rule's `reason` with this node's facts.
///
/// `{field_name}` interpolates a vocabulary field and `{{` / `}}` are literal
/// braces; nothing else is substitutable, so a reason cannot compute. Values
/// render as the rule text expects to read them: a `Size` is a bare byte count
/// and a `Duration` a bare whole-day count, because the sentence around them
/// already supplies the words "bytes" and "days". An absent fact renders as
/// `unknown` rather than as nothing, so the gap is visible to the human
/// approving the deletion.
///
/// A malformed template cannot get here — [`crate::parse`] rejects one — so
/// this is total: a template that does not validate is returned verbatim
/// rather than panicking.
pub fn render_reason(template: &str, facts: &dyn Facts) -> String {
    let Ok(pieces) = template_pieces(template) else {
        return template.to_owned();
    };
    let mut out = String::with_capacity(template.len());
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Field(field) => out.push_str(&render_value(&facts.field(field))),
        }
    }
    out
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        // An integral f64 reads as a count, not as `3` spelled `3`.
        Value::Num(n) if n.is_finite() && n.fract() == 0.0 => format!("{n:.0}"),
        Value::Num(n) => n.to_string(),
        Value::Size(bytes) => bytes.to_string(),
        Value::Duration(seconds) => (seconds / 86_400).to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Absent => "unknown".to_owned(),
    }
}

/// One resolved span of a reason template.
pub(crate) enum Piece<'a> {
    Text(&'a str),
    Field(Field),
}

/// Why a reason template is not a template. Offsets are into the template.
pub(crate) enum TemplateError<'a> {
    /// `{` naming something that is not a field. `len` covers `{name}`.
    UnknownField { name: &'a str, at: usize, len: usize },
    /// `{` with no `}` after it.
    Unclosed { at: usize },
}

/// Split a reason template, shared by the parser's check and the renderer so
/// that what validates and what renders cannot drift apart.
pub(crate) fn template_pieces(template: &str) -> Result<Vec<Piece<'_>>, TemplateError<'_>> {
    let bytes = template.as_bytes();
    let mut pieces = Vec::new();
    let mut text_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'}' if bytes.get(i + 1) == Some(&bytes[i]) => {
                if text_start < i {
                    pieces.push(Piece::Text(&template[text_start..i]));
                }
                // Emit one of the two braces, borrowed rather than allocated.
                pieces.push(Piece::Text(&template[i..i + 1]));
                i += 2;
                text_start = i;
            }
            b'{' => {
                if text_start < i {
                    pieces.push(Piece::Text(&template[text_start..i]));
                }
                let Some(offset) = template[i + 1..].find('}') else {
                    return Err(TemplateError::Unclosed { at: i });
                };
                let name = &template[i + 1..i + 1 + offset];
                let Some(field) = Field::lookup(name) else {
                    return Err(TemplateError::UnknownField { name, at: i, len: offset + 2 });
                };
                pieces.push(Piece::Field(field));
                i += offset + 2;
                text_start = i;
            }
            _ => i += 1,
        }
    }
    if text_start < template.len() {
        pieces.push(Piece::Text(&template[text_start..]));
    }
    Ok(pieces)
}
