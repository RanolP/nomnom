//! What a field test and a description mean: one node's facts in, a yes or
//! no and a sentence out.
//!
//! Every case here is a wrong answer the evaluator could give silently. A rule
//! that fires on the wrong node deletes the wrong directory, and a description
//! with a hole in it is the sentence a human approves that deletion on.

use std::collections::HashMap;

use nomnom_lang::vocab::Field;
use nomnom_lang::{Constraint, Facts, FieldTest, Kinds, Source, Value, check, parse, render_reason};

/// A node whose facts are whatever the test says they are.
#[derive(Default)]
struct Node {
    fields: HashMap<Field, Value>,
}

impl Node {
    fn with(mut self, field: Field, value: Value) -> Self {
        self.fields.insert(field, value);
        self
    }
}

impl Facts for Node {
    fn field(&self, field: Field) -> Value {
        self.fields.get(&field).cloned().unwrap_or(Value::Absent)
    }
}

/// One filter line, parsed the way a pack author writes it.
fn test(line: &str) -> FieldTest {
    let source = Source::new(
        "t.nom",
        format!("[t]\ndescription = e\nkind = cache/v1\nfilter {{\n  {line}\n  then $f\n}}\n"),
    );
    let mut rules =
        parse(&source, &Kinds::builtin()).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"));
    match rules.remove(0).filter.constraints.remove(0) {
        Constraint::Field(test) => test,
        other => panic!("expected a field test, got {other:?}"),
    }
}

fn holds(line: &str, node: &Node) -> bool {
    check(&test(line), node)
}

/// The central absence rule. A file with no recorded atime must not be treated
/// as stale; `not` over it reads "not known to be stale", which is true.
#[test]
fn an_absent_fact_makes_a_comparison_false_and_its_negation_true() {
    let node = Node::default();
    assert!(!holds("$f.accessed_age >= 90d", &node));
    assert!(!holds("$f.accessed_age < 90d", &node));
    assert!(holds("not $f.accessed_age >= 90d", &node));
}

/// Catches an absent bool reading as true.
#[test]
fn a_bare_bool_field_is_false_when_absent() {
    assert!(!holds("$f.has_accessed", &Node::default()));
    assert!(holds("not $f.has_accessed", &Node::default()));
    assert!(holds("$f.has_accessed", &Node::default().with(Field::HasAccessed, Value::Bool(true))));
}

/// Catches a threshold compared in the wrong unit.
#[test]
fn size_and_duration_comparisons_use_the_normalised_values() {
    let node = Node::default()
        .with(Field::Size, Value::Size(1_500))
        .with(Field::ModifiedAge, Value::Duration(91 * 86_400));
    assert!(holds("$f.size > 1kb", &node));
    assert!(!holds("$f.size > 1.5kb", &node));
    assert!(holds("$f.modified_age >= 90d", &node));
    assert!(!holds("$f.modified_age >= 14w", &node));
}

/// Catches name comparisons ignoring the platform: NTFS names are
/// case-insensitive, so `TARGET` is `target` there and nowhere else.
#[test]
fn string_equality_follows_the_platform_rule() {
    let node = Node::default().with(Field::Name, Value::Str("TARGET".into()));
    assert_eq!(holds("$f.name == target", &node), cfg!(windows));
}

/// Catches a value rendered in the wrong shape: the sentence around a hole
/// already says "bytes" and "days".
#[test]
fn render_reason_renders_each_value_variant_as_the_description_expects() {
    let node = Node::default()
        .with(Field::Name, Value::Str("target".into()))
        .with(Field::FileCount, Value::Num(12.0))
        .with(Field::Size, Value::Size(1_048_576))
        .with(Field::ModifiedAge, Value::Duration(120 * 86_400 + 3_600));

    assert_eq!(render_reason("{name}", &node, &[]), "target");
    assert_eq!(render_reason("{file_count} files", &node, &[]), "12 files");
    assert_eq!(render_reason("{size} bytes", &node, &[]), "1048576 bytes");
    assert_eq!(render_reason("{modified_age} days", &node, &[]), "120 days");
    assert_eq!(render_reason("{accessed_age}", &node, &[]), "unknown");
}

/// Catches `{$var}` printing the variable name instead of the bound on-disk
/// name, or a literal brace being taken for a hole.
#[test]
fn render_reason_fills_variables_from_the_bindings_and_unescapes_braces() {
    let node = Node::default();
    let vars = [("dir", "app"), ("marker", "App.csproj")];
    assert_eq!(render_reason("`{$marker}` beside {$dir}", &node, &vars), "`App.csproj` beside app");
    assert_eq!(render_reason("{{$marker}}", &node, &vars), "{$marker}");
    assert_eq!(render_reason("{$gone}", &node, &vars), "unknown");
}
