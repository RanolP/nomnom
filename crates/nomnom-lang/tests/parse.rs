//! Parsing: the specification's own example, operator grouping, and units.

use nomnom_lang::ast::{CmpOp, Expr, Literal};
use nomnom_lang::vocab::{Field, Predicate};
use nomnom_lang::{Disposition, Source, parse};

fn one(text: &str) -> nomnom_lang::Rule {
    let source = Source::new("t.nom", text);
    let mut rules = parse(&source).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"));
    assert_eq!(rules.len(), 1, "fixture defines exactly one rule");
    rules.remove(0)
}

/// The one expression the `when` of a single-condition fixture reduces to.
fn when(text: &str) -> Expr {
    one(&format!(
        "rule \"t\" {{ when {text} then label = cache disposition = review \
         confidence = 0.5 reason = \"evidence\" }}"
    ))
    .when
}

/// Catches the specification and the implementation drifting apart: this is
/// the `cargo-target` rule copied verbatim out of `docs/lang.md`, and every
/// field is asserted, so a grammar or default change that the document does
/// not also make fails here.
#[test]
fn spec_cargo_target_rule_parses_with_every_field_intact() {
    let rule = one(r#"rule "cargo-target" {
  when  dir.name == "target"
        and sibling("Cargo.toml")
  then  label       = build-output
        disposition = reclaimable
        unit        = true
        confidence  = 0.95
        reason      = "regenerable: Cargo build output, rebuilt by `cargo build` — `Cargo.toml` sits beside it"
}"#);

    assert_eq!(rule.name.value, "cargo-target");

    let Expr::And { lhs, rhs } = &rule.when else {
        panic!("top level is `and`, got {:?}", rule.when)
    };
    let Expr::Compare { lhs: field, op, rhs: value } = lhs.as_ref() else {
        panic!("left side is a comparison, got {lhs:?}")
    };
    assert_eq!(field.value, Field::DirName);
    assert_eq!(op.value, CmpOp::Eq);
    assert_eq!(value.value, Literal::Str("target".into()));

    let Expr::Call { predicate, args, .. } = rhs.as_ref() else {
        panic!("right side is a call, got {rhs:?}")
    };
    assert_eq!(predicate.value, Predicate::Sibling);
    assert_eq!(args.len(), 1);
    assert_eq!(args[0].value, Literal::Str("Cargo.toml".into()));

    let then = &rule.then;
    assert_eq!(then.label.value, "build-output");
    assert_eq!(then.disposition.value, Disposition::Reclaimable);
    assert!(then.unit.value);
    assert!(then.unit_written.is_some(), "`unit` was written, not defaulted");
    assert_eq!(then.confidence.value, 0.95);
    assert_eq!(
        then.reason.value,
        "regenerable: Cargo build output, rebuilt by `cargo build` — `Cargo.toml` sits beside it"
    );
}

/// Catches a `unit` default flip. The spec says absent means `false`, and
/// getting this wrong counts a subtree's bytes once instead of per file — a
/// wrong total rather than a visible error.
#[test]
fn unit_defaults_to_false_when_absent() {
    let rule = one(r#"rule "t" { when is_file then label = cache disposition = review
           confidence = 0.1 reason = "evidence" }"#);
    assert!(!rule.then.unit.value);
    assert!(rule.then.unit_written.is_none());
}

/// Catches a precedence bug, which silently changes which paths a rule matches
/// instead of failing: `and` must bind tighter than `or`.
#[test]
fn and_binds_tighter_than_or() {
    let Expr::Or { lhs, rhs } = when("is_dir and is_symlink or is_file") else {
        panic!("`a and b or c` must be an `or` at the top")
    };
    assert!(matches!(*lhs, Expr::And { .. }), "left of `or` is the `and`, got {lhs:?}");
    assert!(matches!(*rhs, Expr::Field(_)), "right of `or` is the bare field, got {rhs:?}");
}

/// Same class: `not` must bind tighter than `and`, so `not a and b` is
/// `(not a) and b` and never `not (a and b)`.
#[test]
fn not_binds_tighter_than_and() {
    let Expr::And { lhs, rhs } = when("not is_dir and is_file") else {
        panic!("`not a and b` must be an `and` at the top")
    };
    assert!(matches!(*lhs, Expr::Not { .. }), "left of `and` is the `not`, got {lhs:?}");
    assert!(matches!(*rhs, Expr::Field(_)), "right of `and` is the bare field, got {rhs:?}");
}

/// Catches parentheses being dropped, which would make the grouping the author
/// wrote unreachable.
#[test]
fn parentheses_override_precedence() {
    let Expr::And { lhs, .. } = when("is_dir and (is_symlink or is_file)") else {
        panic!("`a and (b or c)` must be an `and` at the top")
    };
    assert!(matches!(*lhs, Expr::Field(_)));
}

fn size_of(text: &str) -> u64 {
    let Expr::Compare { rhs, .. } = when(&format!("size > {text}")) else { panic!("comparison") };
    let Literal::Size(bytes) = rhs.value else { panic!("a size literal, got {:?}", rhs.value) };
    bytes
}

fn duration_of(text: &str) -> u64 {
    let Expr::Call { args, .. } = when(&format!("modified_before({text})")) else { panic!("call") };
    let Literal::Duration(seconds) = args[0].value else { panic!("a duration literal") };
    seconds
}

/// Catches an off-by-1024 in a size threshold: `kb` is decimal and `kib` is
/// binary, and confusing them makes every size rule wrong by 2.4% and every
/// gigabyte rule wrong by 7%.
#[test]
fn size_literals_keep_decimal_and_binary_apart() {
    assert_eq!(size_of("1b"), 1);
    assert_eq!(size_of("1kb"), 1_000);
    assert_eq!(size_of("1kib"), 1_024);
    assert_eq!(size_of("100kb"), 100_000);
    assert_eq!(size_of("10mib"), 10 * 1024 * 1024);
    assert_eq!(size_of("2gb"), 2_000_000_000);
    assert_eq!(size_of("1gib"), 1_073_741_824);
    // Fractional sizes round rather than truncate.
    assert_eq!(size_of("1.5kb"), 1_500);
    // Case is not significant in a unit.
    assert_eq!(size_of("1KiB"), 1_024);
}

/// Catches a duration unit drifting: `mo` and `y` are defined as the mean
/// Gregorian year (365.2425 d) and a twelfth of it, and a rule file compares
/// against those numbers whether or not anyone wrote them down.
#[test]
fn duration_literals_normalise_to_seconds() {
    assert_eq!(duration_of("1s"), 1);
    assert_eq!(duration_of("12h"), 43_200);
    assert_eq!(duration_of("30d"), 2_592_000);
    assert_eq!(duration_of("1w"), 604_800);
    assert_eq!(duration_of("6mo"), 6 * 2_629_746);
    assert_eq!(duration_of("1y"), 31_556_952);
}

/// Catches `#` comments being lexed as content, which would break every rule
/// file that disables a rule by commenting it out.
#[test]
fn line_comments_are_trivia() {
    let rule = one(r#"# a leading comment
           rule "t" { # trailing
             when is_file   # about the condition
             then label = cache disposition = review confidence = 0.1 reason = "evidence"
           }"#);
    assert_eq!(rule.name.value, "t");
}
