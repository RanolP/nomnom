//! Validation happens at parse time, and the error is readable.
//!
//! Every case here would otherwise surface during evaluation, where a bad rule
//! shows up as a wrong deletion instead of a startup error.

use nomnom_lang::{Diagnostic, Kinds, Source, parse};

fn fail(text: &str) -> Diagnostic {
    let source = Source::new("t.toml", text);
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

/// A rule titled `t` with `keys` and a filter of `body`.
fn rule_with(keys: &str, body: &str) -> String {
    format!("[[rule]]\ntitle = \"t\"\n{keys}\nfilter = '''\n{body}\n'''\n")
}

/// A valid rule with its key lines and one filter line swapped in.
fn rule(keys: &str, line: &str) -> String {
    rule_with(&format!("description = \"evidence\"\n{keys}"), &format!("  {line}\n  then $f"))
}

const KIND: &str = "kind = \"cache/v1\"";

/// Catches a misspelt key (`dispositon = "keep"`) being ignored, which would
/// leave the kind's default disposition in force with no warning.
#[test]
fn an_unknown_key_is_rejected_instead_of_silently_ignored() {
    let text = rule(&format!("{KIND}\ndispositon = \"keep\""), "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown key `dispositon`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "dispositon");
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `disposition`?"));
}

/// Catches a key given twice silently keeping one of the two values.
#[test]
fn a_key_set_twice_is_rejected_at_the_second() {
    let text = rule(&format!("{KIND}\nkind = \"build-output/v1\""), "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("`kind` is set twice"), "{}", diagnostic.message);
    assert_eq!(diagnostic.line(), 5, "the second `kind`");
}

/// Catches rules keyed by title (`[rule."Title"]`) being accepted: a TOML table
/// has no order, and within a pack the earlier rule wins.
#[test]
fn rules_written_as_a_table_by_title_are_refused_since_they_lose_their_order() {
    let text = "[rule.\"t\"]\ndescription = \"e\"\nkind = \"cache/v1\"\nfilter = 'then $f'\n";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("array of tables"), "{}", diagnostic.message);
    assert!(diagnostic.help.as_deref().is_some_and(|h| h.contains("no order")), "{:?}", diagnostic.help);
}

/// Catches a basic-string filter being accepted, where TOML's escapes would
/// rewrite a glob and shift every reported column after them.
#[test]
fn a_filter_in_a_basic_string_is_refused_with_the_literal_form_as_help() {
    for filter in ["\"then $f\"", "\"\"\"\nthen $f\n\"\"\""] {
        let text = format!("[[rule]]\ntitle = \"t\"\ndescription = \"e\"\n{KIND}\nfilter = {filter}\n");
        let diagnostic = fail(&text);
        assert!(diagnostic.message.contains("literal string"), "{}", diagnostic.message);
        assert_eq!(pointed_at(&text, &diagnostic), filter);
        let help = diagnostic.help.as_deref().unwrap_or_default();
        assert!(help.contains("'''"), "{help}");
    }
}

/// Catches a filter written as a single-quoted literal on one line being
/// refused: it is a literal string too.
#[test]
fn a_one_line_literal_filter_is_accepted() {
    let text = format!("[[rule]]\ntitle = \"t\"\ndescription = \"e\"\n{KIND}\nfilter = 'then $f'\n");
    parse(&Source::new("t.toml", text), &Kinds::builtin()).expect("a literal string");
}

/// Catches a pack written against a kind (or a kind version) this build does
/// not have being loaded under some other meaning instead of refused.
#[test]
fn an_unknown_kind_or_kind_version_refuses_the_rule() {
    let text = rule("kind = \"scratch/v1\"", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown kind `scratch/v1`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "scratch/v1");

    let text = rule("kind = \"cache/v2\"", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("kind `cache` has no version 2"), "{}", diagnostic.message);
    assert!(diagnostic.label.as_deref().is_some_and(|l| l.contains("`cache/v1`")), "{:?}", diagnostic.label);
}

/// Catches a filter with no `then` being accepted, which would leave the rule
/// with no node to put its verdict on.
#[test]
fn a_filter_without_then_is_rejected_since_it_names_no_target() {
    let text = rule_with(&format!("description = \"e\"\n{KIND}"), "  $f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("has no `then`"), "{}", diagnostic.message);
    assert!(pointed_at(&text, &diagnostic).starts_with("'''"));
}

/// Catches a `{$var}` hole the filter never binds, which would print
/// `unknown` into the sentence a human approves a deletion on.
#[test]
fn an_unbound_description_variable_is_rejected_before_it_could_print_unknown() {
    let text = rule_with(
        &format!("description = \"`{{$marker}}` sits beside it\"\n{KIND}"),
        "  $d has Cargo.toml\n  then $d/target/",
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
    let text = rule("kind = \"stale-download/v1\"\ndisposition = \"reclaimable\"", "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("stronger than kind"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "reclaimable");
}

/// Catches a rule with no description: the description is what a human
/// approves a deletion on, so a rule without one can never legitimately run.
#[test]
fn a_rule_without_a_description_is_rejected_at_its_title() {
    let text = rule_with(KIND, "then $f");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("missing `description`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "t");
}

/// Catches a `[[rule]]` with no title being loaded under an empty name that no
/// verdict could cite.
#[test]
fn a_rule_without_a_title_is_rejected() {
    let text = format!("[[rule]]\ndescription = \"e\"\n{KIND}\nfilter = 'then $f'\n");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("missing `title`"), "{}", diagnostic.message);
}

/// Catches a rule still writing the removed `confidence` key being loaded
/// with it silently ignored, or refused without saying the key is gone.
#[test]
fn a_rule_still_writing_confidence_is_told_it_was_removed() {
    let text = rule(&format!("{KIND}\nconfidence = 0.9"), "$f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("unknown key `confidence`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(&text, &diagnostic), "confidence");
    let help = diagnostic.help.as_deref().unwrap_or_default();
    assert!(help.contains("`confidence` was removed"), "{help}");
}

/// Catches `delete` (or any other word) being read as some disposition.
#[test]
fn an_unknown_disposition_is_rejected() {
    let diagnostic = fail(&rule(&format!("{KIND}\ndisposition = \"delete\""), "$f.is_file"));
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
    let text = rule_with(&format!("description = \"e\"\n{KIND}"), "  then $f\n  $f.is_file");
    let diagnostic = fail(&text);
    assert!(diagnostic.message.contains("`then` ends the filter"), "{}", diagnostic.message);
    assert_eq!(diagnostic.help.as_deref(), Some("move this above the `then` on line 6"));
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

/// Catches a filter error reported relative to the filter string instead of
/// the file — off by the lines above it, or by the `'` of a one-line literal.
#[test]
fn a_filter_error_in_a_one_line_literal_points_at_its_file_column() {
    let text = format!("[[rule]]\ntitle = \"t\"\ndescription = \"e\"\n{KIND}\nfilter = '$f.siz > 1kb'\n");
    let diagnostic = fail(&text);
    assert_eq!((diagnostic.line(), diagnostic.column()), (5, 14));
    assert_eq!(pointed_at(&text, &diagnostic), "siz");
}

/// Catches unreadable errors, and a filter error whose column is off by the
/// `'''` and the newline TOML drops after it: a diagnostic that is correct in
/// a debug print and garbage on screen is still a broken error. Asserts the
/// whole rendering, caret column included.
#[test]
fn a_diagnostic_renders_with_the_caret_under_the_offending_token() {
    let source = Source::new(
        "rules/downloads.toml",
        "[[rule]]\ntitle = \"stale\"\ndescription = \"old\"\nkind = \"stale-download/v1\"\n\
         filter = '''\n  $f under Downloads/\n  $f.size > \"big\"\n  then $f\n'''\n",
    );
    let diagnostic = parse(&source, &Kinds::builtin()).expect_err("the comparison is ill-typed");

    assert_eq!(diagnostic.line(), 7);
    assert_eq!(diagnostic.column(), 13);
    assert_eq!(
        diagnostic.to_string(),
        concat!(
            "error: `size` is a size, but it is compared to a string\n",
            "  --> rules/downloads.toml:7:13\n",
            "   |\n",
            " 7 |   $f.size > \"big\"\n",
            "   |             ^^^^^ expected a size such as `100kb` or `10mib`\n",
            "   |\n",
            "   = help: own size in bytes\n",
        )
    );
}
