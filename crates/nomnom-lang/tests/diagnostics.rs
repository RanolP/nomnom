//! Validation happens at parse time, and the error is readable.
//!
//! Every case here would otherwise surface during evaluation, where a bad rule
//! shows up as a wrong deletion instead of a startup error.

use nomnom_lang::{Diagnostic, Kinds, Source, parse};

fn fail(text: &str) -> Diagnostic {
    let source = Source::new("t.nom", text);
    match parse(&source, &Kinds::builtin()) {
        Ok(rules) => panic!("expected a diagnostic, parsed {} rule(s)", rules.len()),
        Err(diagnostic) => diagnostic,
    }
}

/// The offending text a diagnostic points at, so a test can assert the span
/// without hard-coding byte offsets that move when the fixture is reindented.
fn pointed_at<'a>(text: &'a str, diagnostic: &Diagnostic) -> &'a str {
    &text[diagnostic.span.start..diagnostic.span.end]
}

/// A valid rule with one key line and one filter line swapped in.
fn rule(keys: &str, line: &str) -> String {
    format!("[t]\ndescription = evidence\n{keys}\nfilter {{\n  {line}\n  then $f\n}}\n")
}

const KIND: &str = "kind = cache/v1";

/// Catches a misspelt key (`confidense = 0.2`) being ignored, which would
/// leave the kind's default confidence in force with no warning.
#[test]
fn an_unknown_key_is_rejected_instead_of_silently_ignored() {
    let text = rule(&format!("{KIND}\nconfidense = 0.2"), "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown key `confidense`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "confidense");
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `confidence`?"));
}

/// Catches a pack written against a kind (or a kind version) this build does
/// not have being loaded under some other meaning instead of refused.
#[test]
fn an_unknown_kind_or_kind_version_refuses_the_rule() {
    let text = rule("kind = scratch/v1", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown kind `scratch/v1`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "scratch/v1");

    let text = rule("kind = cache/v2", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("kind `cache` has no version 2"), "{}", diagnostic.message);
    assert!(diagnostic.label.as_deref().is_some_and(|l| l.contains("`cache/v1`")), "{:?}", diagnostic.label);
}

/// Catches a filter with no `then` being accepted, which would leave the rule
/// with no node to put its verdict on.
#[test]
fn a_filter_without_then_is_rejected_since_it_names_no_target() {
    let text = format!("[t]\ndescription = evidence\n{KIND}\nfilter {{\n  $f.is_file\n}}\n");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("has no `then`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "filter {");
}

/// Catches a `{$var}` hole the filter never binds, which would print
/// `unknown` into the sentence a human approves a deletion on.
#[test]
fn an_unbound_description_variable_is_rejected_before_it_could_print_unknown() {
    let text = format!(
        "[t]\ndescription = `{{$marker}}` sits beside it\n{KIND}\nfilter {{\n  \
         $d has Cargo.toml\n  then $d/target/\n}}\n"
    );
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("`$marker` is not bound"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "{$marker}");
}

/// Catches a downloaded pack upgrading `stale-download/v1` (review) to
/// `reclaimable`, which would turn "maybe look at this" into a deletion
/// proposal without declaring a kind that says so.
#[test]
fn a_disposition_stronger_than_the_kinds_is_rejected_so_a_rule_cannot_upgrade() {
    let text = rule("kind = stale-download/v1\ndisposition = reclaimable", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("stronger than kind"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "reclaimable");
}

/// Catches a rule with no description: the description is what a human
/// approves a deletion on, so a rule without one can never legitimately run.
#[test]
fn a_rule_without_a_description_is_rejected_at_its_title() {
    let text = format!("[t]\n{KIND}\nfilter {{\n  then $f\n}}\n");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("missing `description`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "t");
}

/// Confidence is the first conflict tie-break, so a rule scoring 1.5 would
/// outrank every honest rule in every pack.
#[test]
fn confidence_outside_the_unit_interval_is_rejected() {
    let text = rule(&format!("{KIND}\nconfidence = 1.5"), "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("out of range"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "1.5");
}

/// Catches `delete` (or any other word) being read as some disposition.
#[test]
fn an_unknown_disposition_is_rejected() {
    let diagnostic = fail(&rule(&format!("{KIND}\ndisposition = delete"), "$f.is_file"));
    assert!(diagnostic.message.contains("unknown disposition `delete`"), "{}", diagnostic.message);
}

/// Catches a field typo becoming a test that is never true.
#[test]
fn an_unknown_field_is_rejected_and_suggests_the_real_one() {
    let text = rule(KIND, "$f.modfied_age >= 90d");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown field `modfied_age`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "modfied_age");
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `modified_age`?"));
}

/// Catches `size > 100` meaning 100 bytes: a size needs its unit.
#[test]
fn a_unitless_number_is_not_a_size() {
    let diagnostic = fail(&rule(KIND, "$f.size > 100"));
    assert!(diagnostic.message.contains("compared to a number"), "{}", diagnostic.message);
}

/// Catches a constraint written below `then` being silently dropped or
/// silently applied — either way the author's intent is unclear.
#[test]
fn a_constraint_after_then_is_rejected() {
    let text = format!("[t]\ndescription = e\n{KIND}\nfilter {{\n  then $f\n  $f.is_file\n}}\n");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("`then` ends the filter"), "{}", diagnostic.message);
}

/// Catches a constraint on a variable `then` does not start from, which the
/// engine has no node for.
#[test]
fn a_constraint_on_another_variable_is_rejected() {
    let text = rule(KIND, "$g.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("`$g` is not bound"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "$g");
}

/// Catches unreadable errors: a diagnostic that is correct in a debug print
/// and garbage on screen is still a broken error. Asserts the whole rendering,
/// caret column included.
#[test]
fn a_diagnostic_renders_with_the_caret_under_the_offending_token() {
    let source = Source::new(
        "rules/downloads.nom",
        "[stale]\ndescription = old\nkind = stale-download/v1\nfilter {\n  $f under Downloads/\n  $f.size > \"big\"\n  then $f\n}\n",
    );
    let diagnostic = parse(&source, &Kinds::builtin()).expect_err("the comparison is ill-typed");

    assert_eq!(diagnostic.line(), 6);
    assert_eq!(diagnostic.column(), 13);
    assert_eq!(
        diagnostic.to_string(),
        concat!(
            "error: `size` is a size, but it is compared to a string\n",
            "  --> rules/downloads.nom:6:13\n",
            "   |\n",
            " 6 |   $f.size > \"big\"\n",
            "   |             ^^^^^ expected a size such as `100kb` or `10mib`\n",
            "   |\n",
            "   = help: own size in bytes\n",
        )
    );
}
