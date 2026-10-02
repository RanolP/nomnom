//! Validation happens at parse time, and the error is readable.
//!
//! Every case here would otherwise surface during evaluation, where a bad rule
//! shows up as a wrong deletion instead of a startup error.

use nomnom_lang::{Diagnostic, Source, parse};

fn fail(text: &str) -> Diagnostic {
    let source = Source::new("t.nom", text);
    match parse(&source) {
        Ok(rules) => panic!("expected a diagnostic, parsed {} rule(s)", rules.len()),
        Err(diagnostic) => diagnostic,
    }
}

/// The offending text a diagnostic points at, so a test can assert the span
/// without hard-coding byte offsets that move when the fixture is reindented.
fn pointed_at<'a>(text: &'a str, diagnostic: &Diagnostic) -> &'a str {
    &text[diagnostic.span.start..diagnostic.span.end]
}

/// A `then` with no `reason` must fail here, not at evaluation: the reason is
/// what a human approves a deletion on, so a rule without one can never
/// legitimately run.
#[test]
fn missing_reason_is_a_parse_error_pointing_at_then() {
    let text =
        "rule \"t\" { when is_file\n  then label = cache disposition = review confidence = 0.5 }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("missing `reason`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "then");
}

/// A present-but-blank reason is the same failure as an absent one; catching
/// only the absent case would let `reason = "  "` through.
#[test]
fn whitespace_only_reason_is_rejected_at_its_own_span() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 0.5 reason = \"   \" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("`reason` is empty"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "\"   \"");
}

/// Confidence is the first conflict tie-break, so a rule scoring 1.5 would
/// outrank every honest rule in every pack.
#[test]
fn confidence_outside_the_unit_interval_is_rejected() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 1.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("out of range"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "1.5");
}

/// An unknown disposition must not fall back to anything. `delete` silently
/// read as `reclaimable` is the worst available failure.
#[test]
fn unknown_disposition_is_rejected_with_the_three_legal_values() {
    let text = "rule \"t\" { when is_file then label = cache disposition = delete \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("unknown disposition `delete`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "delete");
    let rendered = diagnostic.to_string();
    assert!(rendered.contains("`keep`, `reclaimable` or `review`"), "{rendered}");
}

/// A misspelt field would otherwise evaluate to "no opinion", making the rule
/// quietly dead rather than broken.
#[test]
fn unknown_field_name_is_rejected_and_suggests_the_real_one() {
    let text = "rule \"t\" { when filename == \"x\" then label = cache disposition = review \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("unknown field or predicate"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "filename");
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `file.name`?"));
}

/// Arity is fixed by the vocabulary table; an extra argument means the author
/// expected a behaviour the predicate does not have.
#[test]
fn predicate_arity_mismatch_is_rejected() {
    let text = "rule \"t\" { when sibling(\"a\", \"b\") then label = cache disposition = review \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("takes 1 argument"), "{}", diagnostic.message);
    assert!(diagnostic.message.contains("2 were given"), "{}", diagnostic.message);
}

/// `sibling(3)` has no meaning; without this check it would have to be given
/// one at evaluation time, and any choice there is a guess.
#[test]
fn predicate_argument_type_mismatch_is_rejected() {
    let text = "rule \"t\" { when sibling(3) then label = cache disposition = review \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("expects a string here"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "3");
}

/// Comparing a size to a string cannot be true or false, so it must not reach
/// the evaluator at all.
#[test]
fn comparing_a_size_field_to_a_string_is_rejected() {
    let text = "rule \"t\" { when size > \"big\" then label = cache disposition = review \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("compared to a string"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "\"big\"");
}

/// A bare number where a size belongs would be read as bytes, making
/// `size > 100` look like a 100 MB threshold to a reader and be a 100 byte one
/// in fact. The unit is mandatory.
#[test]
fn a_unitless_number_is_not_a_size() {
    let text = "rule \"t\" { when size > 100 then label = cache disposition = review \
                confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("compared to a number"), "{}", diagnostic.message);
}

/// Setting a conclusion field twice means one of the two values is being
/// silently discarded.
#[test]
fn a_repeated_then_field_is_rejected() {
    let text = "rule \"t\" { when is_file then label = cache label = duplicate \
                disposition = review confidence = 0.5 reason = \"evidence\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("set twice"), "{}", diagnostic.message);
}

/// A `reason` is a template, and a misspelt hole would otherwise print
/// `{siez}` verbatim into the sentence a human approves a deletion on.
#[test]
fn an_unknown_reason_template_field_is_rejected_and_suggests_the_real_one() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 0.5 reason = \"it is {siez} bytes\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("unknown field `siez`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "{siez}");
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `size`?"));
}

/// An unclosed `{` is the same class caught one step earlier: without it the
/// brace and everything after it is printed as if it were prose.
#[test]
fn an_unclosed_brace_in_a_reason_is_rejected() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 0.5 reason = \"it is {size bytes\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("unclosed `{`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "{size bytes");
}

/// The lexer resolved the escapes, so an offset into the unescaped reason no
/// longer indexes the source. Pointing at the whole literal is the honest
/// answer; pointing at a computed offset would put the caret on the wrong
/// characters.
#[test]
fn a_template_error_after_an_escape_points_at_the_whole_literal() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 0.5 reason = \"a \\\"quoted\\\" {filesize}\" }";
    let diagnostic = fail(text);
    assert!(diagnostic.message.contains("unknown field `filesize`"), "{}", diagnostic.message);
    assert_eq!(pointed_at(text, &diagnostic), "\"a \\\"quoted\\\" {filesize}\"");
}

/// `{{` is an escape, not a hole: treating it as one would reject every reason
/// that wants a literal brace in it.
#[test]
fn doubled_braces_in_a_reason_are_not_field_holes() {
    let text = "rule \"t\" { when is_file then label = cache disposition = review \
                confidence = 0.5 reason = \"literally {{size}}\" }";
    let source = Source::new("t.nom", text);
    let rules = parse(&source).unwrap_or_else(|d| panic!("expected a parse, got:\n{d}"));
    assert_eq!(rules[0].then.reason.value, "literally {{size}}");
}

/// Catches unreadable errors: a diagnostic that is correct in a debug print
/// and garbage on screen is still a broken error. Asserts the whole rendering,
/// caret column included.
#[test]
fn a_diagnostic_renders_with_the_caret_under_the_offending_token() {
    let source = Source::new(
        "rules/downloads.nom",
        "rule \"stale\" {\n  when  ancestor(\"Downloads\")\n        and size > \"big\"\n  then  label = stale-download\n}\n",
    );
    let diagnostic = parse(&source).expect_err("the comparison is ill-typed");

    assert_eq!(diagnostic.line(), 3);
    assert_eq!(diagnostic.column(), 20);
    assert_eq!(
        diagnostic.to_string(),
        concat!(
            "error: `size` is a size, but it is compared to a string\n",
            "  --> rules/downloads.nom:3:20\n",
            "   |\n",
            " 3 |         and size > \"big\"\n",
            "   |                    ^^^^^ expected a size such as `100kb` or `10mib`\n",
            "   |\n",
            "   = help: own size in bytes\n",
        )
    );
}
