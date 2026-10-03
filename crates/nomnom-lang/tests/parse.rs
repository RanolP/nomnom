//! What a valid rule file parses to.

use nomnom_lang::ast::{ChildTest, Constraint, Disposition, Literal, NamePattern};
use nomnom_lang::vocab::Field;
use nomnom_lang::{Kinds, Rule, Source, parse};

fn rules(text: &str) -> Vec<Rule> {
    let source = Source::new("t.nom", text);
    parse(&source, &Kinds::builtin()).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"))
}

fn one(text: &str) -> Rule {
    let mut parsed = rules(text);
    assert_eq!(parsed.len(), 1);
    parsed.remove(0)
}

/// A rule around one filter line, for tests about that line alone.
fn with_line(line: &str) -> Rule {
    one(&format!(
        "[t]\ndescription = evidence\nkind = cache/v1\nfilter {{\n  {line}\n  then $f\n}}\n"
    ))
}

const SPEC: &str = "\
[build/ beside a manifest]
description = build output, rebuilt by the project's build command — `{$marker}` sits beside it
kind = build-output/v1
confidence = 0.9
filter {
  $dir has package.json | pyproject.toml | CMakeLists.txt as $marker
  then $dir/build/
}
";

/// Catches a parser that drops or reorders part of the documented example:
/// every key, the alternation in written order, the capture and the target.
#[test]
fn the_spec_rule_parses_with_every_part_intact() {
    let rule = one(SPEC);
    assert_eq!(rule.title.value, "build/ beside a manifest");
    assert!(rule.description.value.ends_with("`{$marker}` sits beside it"));
    assert_eq!(rule.kind.value.to_string(), "build-output/v1");
    assert_eq!(rule.disposition, Disposition::Reclaimable);
    assert_eq!(rule.filter.var.value, "dir");

    let [Constraint::Children(ChildTest { negated, names, capture, .. })] =
        rule.filter.constraints.as_slice()
    else {
        panic!("one `has`, got {:?}", rule.filter.constraints);
    };
    assert!(!negated);
    let names: Vec<&str> = names.iter().map(|n| n.value.text()).collect();
    assert_eq!(names, ["package.json", "pyproject.toml", "CMakeLists.txt"]);
    assert_eq!(capture.as_ref().map(|c| c.value.as_str()), Some("marker"));

    let then = &rule.filter.then;
    assert_eq!(then.segments.len(), 1);
    assert_eq!(then.segments[0].value, NamePattern::Literal("build".into()));
    assert!(then.dir, "the trailing `/` requires a directory");
}

/// Catches a rule that loses its kind's defaults when it omits them: the kind,
/// not a hard-coded fallback, is where an unset disposition and confidence
/// come from.
#[test]
fn an_unset_confidence_and_disposition_come_from_the_kind() {
    let rule = one(
        "[s]\ndescription = old\nkind = stale-download/v1\nfilter {\n  $f.is_file\n  then $f\n}\n",
    );
    assert_eq!(rule.disposition, Disposition::Review);
    assert_eq!(rule.confidence, 0.5);
}

/// Catches a downgrade being refused along with an upgrade: moving down from
/// the kind's disposition is the whole point of the key.
#[test]
fn a_disposition_below_the_kind_is_accepted() {
    let rule = one(
        "[t]\ndescription = maybe\nkind = build-output/v1\ndisposition = review\n\
         confidence = 0.35\nfilter {\n  $d lacks Cargo.toml\n  then $d/target/\n}\n",
    );
    assert_eq!(rule.disposition, Disposition::Review);
    assert_eq!(rule.confidence, 0.35);
}

/// Catches a glob compared as a literal name (or the reverse), which would
/// make `*.csproj` match only a file literally named that.
#[test]
fn a_name_with_glob_characters_is_a_glob_and_any_other_is_literal() {
    let rule = with_line("$f has *.csproj | App.sln");
    let Constraint::Children(test) = &rule.filter.constraints[0] else { panic!() };
    assert_eq!(test.names[0].value, NamePattern::Glob("*.csproj".into()));
    assert_eq!(test.names[1].value, NamePattern::Literal("App.sln".into()));
}

/// Catches a deep target losing a segment or its directory requirement.
#[test]
fn a_then_path_keeps_every_segment_and_only_a_trailing_slash_requires_a_directory() {
    let rule = one(
        "[n]\ndescription = x\nkind = cache/v1\nfilter {\n  $p has next.config.js\n  \
         then $p/.next/cache/\n}\n",
    );
    let segments: Vec<&str> = rule.filter.then.segments.iter().map(|s| s.value.text()).collect();
    assert_eq!(segments, [".next", "cache"]);
    assert!(rule.filter.then.dir);
    assert!(!with_line("$f.is_file").filter.then.dir, "`then $f` accepts a file");
}

/// Catches `not` being attached to the wrong test or dropped.
#[test]
fn not_negates_the_field_test_it_precedes() {
    let rule = with_line("not $f.has_accessed");
    let Constraint::Field(test) = &rule.filter.constraints[0] else { panic!() };
    assert!(test.negated);
    assert_eq!(test.field.value, Field::HasAccessed);
}

/// Catches `under Downloads/` keeping the slash, which would compare against
/// a name no directory has.
#[test]
fn under_takes_the_name_without_its_trailing_slash() {
    let rule = with_line("$f under Downloads/");
    assert!(matches!(&rule.filter.constraints[0], Constraint::Under(name) if name.value == "Downloads"));
}

fn literal_of(line: &str) -> Literal {
    let rule = with_line(line);
    let Constraint::Field(test) = &rule.filter.constraints[0] else { panic!() };
    test.compare.as_ref().expect("a comparison").1.value.clone()
}

/// Catches `1kb` being read as 1024 or `1kib` as 1000 — off by 2.4%, silently.
#[test]
fn size_literals_keep_decimal_and_binary_apart() {
    assert_eq!(literal_of("$f.size > 1kb"), Literal::Size(1000));
    assert_eq!(literal_of("$f.size > 1kib"), Literal::Size(1024));
    assert_eq!(literal_of("$f.size > 1.5gb"), Literal::Size(1_500_000_000));
}

/// Catches a duration unit with the wrong number of seconds behind it.
#[test]
fn duration_literals_normalise_to_seconds() {
    assert_eq!(literal_of("$f.modified_age >= 90d"), Literal::Duration(90 * 86_400));
    assert_eq!(literal_of("$f.modified_age >= 1y"), Literal::Duration(31_556_952));
}

/// Catches `$f.ext == zip` being refused for want of quotes: a bare word where
/// a string is expected is that string.
#[test]
fn a_bare_word_compared_to_a_string_field_is_a_string() {
    assert_eq!(literal_of("$f.ext == zip"), Literal::Str("zip".into()));
    assert_eq!(literal_of("$f.name == \"two words\""), Literal::Str("two words".into()));
}

/// Catches CRLF checkouts (the Windows default) breaking `}` and key parsing,
/// and comments being read as constraints.
#[test]
fn crlf_line_endings_and_comments_parse_like_lf() {
    let text = format!("# leading comment\r\n{}", SPEC.replace('\n', "\r\n"))
        .replace("  then", "  # a comment inside the filter\r\n  then");
    let rule = one(&text);
    assert_eq!(rule.title.value, "build/ beside a manifest");
    assert_eq!(rule.filter.constraints.len(), 1);
}

/// Catches rules after the first being lost or merged.
#[test]
fn several_rules_in_one_file_parse_in_order() {
    let text = format!("{SPEC}\n{}", SPEC.replace("[build/ beside", "[dist/ beside"));
    let titles: Vec<String> = rules(&text).into_iter().map(|r| r.title.value).collect();
    assert_eq!(titles, ["build/ beside a manifest", "dist/ beside a manifest"]);
}
