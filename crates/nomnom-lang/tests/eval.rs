//! What a rule means: one node's facts in, a yes or no and a sentence out.
//!
//! Every case here is a wrong answer the evaluator could give silently. A rule
//! that fires on the wrong node deletes the wrong directory, and a reason with
//! a hole in it is the sentence a human approves that deletion on.

use std::collections::HashMap;

use nomnom_lang::vocab::{Field, Predicate};
use nomnom_lang::{Expr, Facts, Literal, Source, Value, eval, parse, render_reason};

/// A node whose facts are whatever the test says they are.
#[derive(Default)]
struct Node {
    fields: HashMap<Field, Value>,
    /// The predicate calls that answer `true`; everything else is `false`.
    true_calls: Vec<(Predicate, Vec<Literal>)>,
}

impl Node {
    fn with(mut self, field: Field, value: Value) -> Self {
        self.fields.insert(field, value);
        self
    }

    fn calling(mut self, predicate: Predicate, args: Vec<Literal>) -> Self {
        self.true_calls.push((predicate, args));
        self
    }
}

impl Facts for Node {
    fn field(&self, field: Field) -> Value {
        self.fields.get(&field).cloned().unwrap_or(Value::Absent)
    }

    fn predicate(&self, predicate: Predicate, args: &[Literal]) -> bool {
        self.true_calls.iter().any(|(p, a)| *p == predicate && a.as_slice() == args)
    }
}

/// The `when` of a one-condition rule, parsed the way a pack author writes it.
fn when(text: &str) -> Expr {
    let source = Source::new(
        "t.nom",
        format!(
            "rule \"t\" {{ when {text} then label = cache disposition = review \
             confidence = 0.5 reason = \"evidence\" }}"
        ),
    );
    let mut rules = parse(&source).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"));
    rules.remove(0).when
}

fn holds(text: &str, node: &Node) -> bool {
    eval(&when(text), node)
}

/// The central absence rule. A file with no recorded atime must not be treated
/// as stale, and `not` over it must not be treated as stale either — which is
/// the same statement read from both sides, and getting the second half wrong
/// deletes every file the filesystem happens to be quiet about.
#[test]
fn an_absent_fact_makes_a_comparison_false_and_its_negation_true() {
    let node = Node::default();
    assert_eq!(node.field(Field::AccessedAge), Value::Absent);

    assert!(!holds("accessed_age > 90d", &node));
    assert!(holds("not accessed_age > 90d", &node));
    // Every operator, not just `>`: absence answers nothing at all.
    assert!(!holds("accessed_age < 90d", &node));
    assert!(!holds("accessed_age == 90d", &node));
    assert!(!holds("accessed_age != 90d", &node));
}

/// A bare absent bool is false, so `has_accessed` is the guard a rule uses to
/// demand the fact exist before trusting a comparison over it.
#[test]
fn a_bare_bool_field_is_false_when_absent() {
    let node = Node::default();
    assert!(!holds("has_accessed", &node));
    assert!(!holds("has_accessed and accessed_age > 90d", &node));

    let known = Node::default()
        .with(Field::HasAccessed, Value::Bool(true))
        .with(Field::AccessedAge, Value::Duration(120 * 86_400));
    assert!(holds("has_accessed and accessed_age > 90d", &known));
}

/// Catches a precedence bug at evaluation rather than at parse: `and` binds
/// tighter than `or`, so this must be `(dir and symlink) or file` — the other
/// grouping answers the opposite on exactly this node.
#[test]
fn and_binds_tighter_than_or_when_evaluated() {
    let node = Node::default()
        .with(Field::IsDir, Value::Bool(false))
        .with(Field::IsSymlink, Value::Bool(false))
        .with(Field::IsFile, Value::Bool(true));

    // `(dir and symlink) or file` holds here; `dir and (symlink or file)` does
    // not, so the two groupings cannot both pass this node.
    assert!(holds("is_dir and is_symlink or is_file", &node));
    assert!(!holds("is_dir and (is_symlink or is_file)", &node));
}

/// `not` binds tighter than `and`: `not a and b` is `(not a) and b`, and the
/// other reading flips the answer on this node.
#[test]
fn not_binds_tighter_than_and_when_evaluated() {
    let node = Node::default()
        .with(Field::IsDir, Value::Bool(false))
        .with(Field::IsFile, Value::Bool(false))
        .with(Field::IsSymlink, Value::Bool(true));

    // `(not dir) and file` is false here; `not (dir and file)` is true.
    assert!(!holds("not is_dir and is_file", &node));
    assert!(holds("not (is_dir and is_file)", &node));
    assert!(holds("is_dir or is_symlink", &node));
    assert!(!holds("is_dir or is_file", &node));
}

/// Catches a size threshold compared in the wrong unit. The lexer normalised
/// `10mib` to bytes, so the evaluator must compare bytes to bytes and never
/// rescale — a second rescale here is the 1024-vs-1000 bug with no error.
#[test]
fn size_comparisons_use_the_bytes_the_lexer_normalised() {
    let node = Node::default().with(Field::Size, Value::Size(10 * 1024 * 1024));

    assert!(!holds("size > 10mib", &node));
    assert!(holds("size >= 10mib", &node));
    assert!(holds("size == 10mib", &node));
    assert!(holds("size > 10mb", &node), "10 MiB exceeds 10 MB");
    assert!(holds("size < 1gb", &node));
}

/// Same class for durations, which the lexer normalised to seconds: `90d` is
/// 7_776_000 s and nothing downstream may multiply by 86_400 again.
#[test]
fn duration_comparisons_use_the_seconds_the_lexer_normalised() {
    let node = Node::default().with(Field::ModifiedAge, Value::Duration(120 * 86_400));

    assert!(holds("modified_age > 90d", &node));
    assert!(!holds("modified_age > 6mo", &node));
    assert!(holds("modified_age > 1w", &node));
    assert!(holds(
        "max_descendant_age > 1d",
        &Node::default().with(Field::MaxDescendantAge, Value::Duration(2 * 86_400))
    ));
}

/// A predicate's arguments have to arrive intact and in order; a call that
/// reaches `Facts` with the wrong literal silently asks a different question.
#[test]
fn predicate_arguments_reach_facts_verbatim() {
    let node = Node::default()
        .calling(Predicate::Sibling, vec![Literal::Str("Cargo.toml".into())])
        .calling(Predicate::SiblingMatches, vec![Literal::Str("*.csproj".into())]);

    assert!(holds("sibling(\"Cargo.toml\")", &node));
    assert!(!holds("sibling(\"package.json\")", &node));
    assert!(holds("sibling_matches(\"*.csproj\")", &node));
    assert!(!holds("sibling_matches(\"*.sln\")", &node));
}

/// Name comparison must follow the platform, because `README.md` and
/// `readme.md` are one file on Windows and two on Linux, and a rule written
/// against one spelling would otherwise miss on the machine where it matters.
#[test]
fn string_equality_follows_the_platform_rule() {
    let node = Node::default().with(Field::Name, Value::Str("Target".into()));
    assert!(holds("name == \"Target\"", &node));
    assert_eq!(holds("name == \"target\"", &node), cfg!(windows));
    assert_eq!(holds("name != \"target\"", &node), !cfg!(windows));
}

// -- reason templates ------------------------------------------------------

/// Every `Value` variant has a rendering the rule text is written against.
/// A `Size` printed as `10 MB` inside "… is {size} bytes" is a wrong sentence,
/// and a wrong sentence is what the human approves the deletion on.
#[test]
fn render_reason_renders_each_value_variant_as_the_rule_text_expects() {
    let node = Node::default()
        .with(Field::Name, Value::Str("target".into()))
        .with(Field::FileCount, Value::Num(12.0))
        .with(Field::Depth, Value::Num(2.5))
        .with(Field::Size, Value::Size(1_048_576))
        .with(Field::IsDir, Value::Bool(true));

    assert_eq!(render_reason("{name}", &node), "target");
    assert_eq!(render_reason("{file_count} files", &node), "12 files");
    assert_eq!(render_reason("{depth}", &node), "2.5");
    assert_eq!(render_reason("{size} bytes", &node), "1048576 bytes");
    assert_eq!(render_reason("{is_dir}", &node), "true");
    // An absent fact is named, not elided: a sentence with a silent gap reads
    // as a fact rather than as a missing one.
    assert_eq!(render_reason("{accessed_age}", &node), "unknown");
}

/// A duration renders as whole days because the rule text supplies the word
/// "days"; leaking seconds here turns "untouched for 120 days" into
/// "untouched for 10368000 days".
#[test]
fn render_reason_renders_a_duration_as_whole_days() {
    let node = Node::default()
        .with(Field::ModifiedAge, Value::Duration(120 * 86_400 + 3_600))
        .with(Field::AccessedAge, Value::Duration(3_600));

    assert_eq!(render_reason("untouched for {modified_age} days", &node), "untouched for 120 days");
    // Under a day truncates to 0 rather than rounding up to a day that has not
    // passed.
    assert_eq!(render_reason("{accessed_age}", &node), "0");
}

/// `{{` is the only way to write a literal brace, so a template that loses the
/// escape prints a rule's own syntax into the user's sentence.
#[test]
fn render_reason_unescapes_doubled_braces_and_leaves_plain_text_alone() {
    let node = Node::default().with(Field::Name, Value::Str("target".into()));

    assert_eq!(render_reason("{{name}}", &node), "{name}");
    assert_eq!(render_reason("{{{name}}}", &node), "{target}");
    assert_eq!(render_reason("no holes here", &node), "no holes here");
    assert_eq!(render_reason("", &node), "");
    assert_eq!(render_reason("a {name} b {name} c", &node), "a target b target c");
}

/// The reason a rule actually ships with, end to end: parse it, then render it
/// against a node. Catches the template surviving the parser but not the
/// renderer, or vice versa.
#[test]
fn a_parsed_rules_reason_renders_against_a_node() {
    let source = Source::new(
        "t.nom",
        "rule \"stale\" { when modified_age > 90d then label = stale-download \
         disposition = review confidence = 0.5 \
         reason = \"{name} is {size} bytes and untouched for {modified_age} days\" }",
    );
    let rules = parse(&source).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"));
    let node = Node::default()
        .with(Field::Name, Value::Str("big.iso".into()))
        .with(Field::Size, Value::Size(4_294_967_296))
        .with(Field::ModifiedAge, Value::Duration(200 * 86_400));

    assert!(eval(&rules[0].when, &node));
    assert_eq!(
        render_reason(&rules[0].then.reason.value, &node),
        "big.iso is 4294967296 bytes and untouched for 200 days"
    );
}
