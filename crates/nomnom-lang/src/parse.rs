//! A rule file to a validated [`Rule`] list.
//!
//! The format is line-oriented, so the parser is too: a `[Title]` line opens a
//! rule, `key = value` lines set its keys, and a `filter { ... }` block holds
//! one constraint per line, ending with exactly one `then`. Everything that can
//! be checked without a filesystem is checked here — known keys, a known kind,
//! a disposition no stronger than the kind's, bound variables, field types —
//! so that a bad rule is a startup error rather than a wrong deletion later.

use crate::ast::{
    ChildTest, CmpOp, Constraint, Disposition, FieldTest, Filter, Literal, NamePattern, Rule,
    Spanned, Target,
};
use crate::diagnostic::{Diagnostic, Result, Source, Span};
use crate::eval::{Piece, TemplateError};
use crate::kind::{Kind, Kinds, split_id};
use crate::lex::{self, Word};
use crate::vocab::{FIELDS, Field, Ty, nearest};

const KEYS: [&str; 3] = ["description", "kind", "disposition"];

/// Why a rule or a kind that still writes `confidence` is refused, shared with
/// `pack.toml` so both say the same thing.
pub(crate) const CONFIDENCE_REMOVED: &str = "`confidence` was removed: two packs claiming one \
     node now leave it unjudged, and within a pack the earlier rule wins; delete the line";

/// `$v` alone, or `$v.field`.
type Subject = (Spanned<String>, Option<Spanned<Field>>);

/// Parse a whole rule file, resolving every `kind` against `kinds`.
pub fn parse(source: &Source, kinds: &Kinds) -> Result<Vec<Rule>> {
    let mut parser = Parser { source, kinds, lines: lines(&source.text), next: 0 };
    let mut rules = Vec::new();
    let mut draft: Option<Draft> = None;

    while let Some(span) = parser.line() {
        let line = &source.text[span.start..span.end];
        if line.starts_with('[') {
            if let Some(done) = draft.take() {
                rules.push(parser.finish(done)?);
            }
            draft = Some(parser.header(line, span)?);
            continue;
        }
        let Some(rule) = draft.as_mut() else {
            return Err(parser
                .error(span, "expected a `[Title]` line")
                .with_label("every rule starts with its title in brackets")
                .with_help("as in `[Cargo target/]`"));
        };
        if let Some((_, filter_span)) = &rule.filter {
            return Err(parser
                .error(span, "nothing may follow a rule's `filter` block")
                .with_label("expected the next `[Title]`")
                .with_help(format!(
                    "keys go above `filter`; this rule's filter ends at line {}",
                    parser.line_of(filter_span.end)
                )));
        }
        if is_filter_opener(line) {
            rule.filter = Some(parser.filter(span)?);
        } else {
            parser.key(rule, line, span)?;
        }
    }
    if let Some(done) = draft {
        rules.push(parser.finish(done)?);
    }
    Ok(rules)
}

/// Every line's span, without its line terminator.
fn lines(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut start = 0;
    for (at, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            let end = if at > start && text.as_bytes()[at - 1] == b'\r' { at - 1 } else { at };
            out.push(Span::new(start, end));
            start = at + 1;
        }
    }
    if start < text.len() {
        out.push(Span::new(start, text.len()));
    }
    out
}

fn is_filter_opener(line: &str) -> bool {
    line.strip_prefix("filter").is_some_and(|rest| rest.trim() == "{")
}

/// A rule whose keys and filter are still being read.
struct Draft {
    title: Spanned<String>,
    description: Option<Spanned<String>>,
    kind: Option<Spanned<String>>,
    disposition: Option<Spanned<Disposition>>,
    filter: Option<(Filter, Span)>,
}

/// A filter whose lines are still being read.
#[derive(Default)]
struct FilterDraft {
    constraints: Vec<Constraint>,
    /// Every `$v` a constraint tests, checked against `then` once it is known.
    subjects: Vec<Spanned<String>>,
    captures: Vec<Spanned<String>>,
    then: Option<(Spanned<String>, Target)>,
}

struct Parser<'a> {
    source: &'a Source,
    kinds: &'a Kinds,
    lines: Vec<Span>,
    next: usize,
}

impl Parser<'_> {
    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.source, span, message)
    }

    fn line_of(&self, offset: usize) -> usize {
        self.source.text[..offset].matches('\n').count() + 1
    }

    /// The span of the next line that is not blank or a full-line comment,
    /// trimmed.
    fn line(&mut self) -> Option<Span> {
        while let Some(&span) = self.lines.get(self.next) {
            self.next += 1;
            let raw = &self.source.text[span.start..span.end];
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let start = span.start + (raw.len() - raw.trim_start().len());
            return Some(Span::new(start, start + trimmed.len()));
        }
        None
    }

    // -- the rule header ---------------------------------------------------

    fn header(&self, line: &str, span: Span) -> Result<Draft> {
        let Some(inner) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) else {
            return Err(self
                .error(span, "unclosed rule title")
                .with_label("expected `]` at the end of this line"));
        };
        let title = inner.trim();
        if title.is_empty() {
            return Err(self
                .error(span, "empty rule title")
                .with_label("a verdict cites its rule by this title")
                .with_help("name what the rule finds, as in `[Cargo target/]`"));
        }
        let start = span.start + 1 + (inner.len() - inner.trim_start().len());
        Ok(Draft {
            title: Spanned::new(title.to_owned(), Span::new(start, start + title.len())),
            description: None,
            kind: None,
            disposition: None,
            filter: None,
        })
    }

    fn key(&self, rule: &mut Draft, line: &str, span: Span) -> Result<()> {
        let Some(eq) = line.find('=') else {
            return Err(self
                .error(span, "expected `key = value` or `filter {`")
                .with_label("not a key or a filter")
                .with_help(format!("the keys are {}", KEYS.join(", "))));
        };
        let key = line[..eq].trim_end();
        let key_span = Span::new(span.start, span.start + key.len());
        let raw_value = &line[eq + 1..];
        let value = raw_value.trim();
        let value_start = span.start + eq + 1 + (raw_value.len() - raw_value.trim_start().len());
        let value_span = Span::new(value_start, value_start + value.len());
        if value.is_empty() {
            return Err(self
                .error(Span::new(span.start + eq, span.end), format!("`{key}` has no value"))
                .with_label("expected a value after `=`"));
        }

        let twice = |set: bool| -> Result<()> {
            if set {
                Err(self
                    .error(key_span, format!("`{key}` is set twice"))
                    .with_label("already given above in this rule"))
            } else {
                Ok(())
            }
        };
        match key {
            "description" => {
                twice(rule.description.is_some())?;
                rule.description = Some(Spanned::new(value.to_owned(), value_span));
            }
            "kind" => {
                twice(rule.kind.is_some())?;
                rule.kind = Some(Spanned::new(value.to_owned(), value_span));
            }
            "confidence" => {
                return Err(self
                    .error(key_span, "unknown key `confidence`")
                    .with_label("no longer a rule key")
                    .with_help(CONFIDENCE_REMOVED));
            }
            "disposition" => {
                twice(rule.disposition.is_some())?;
                let Some(parsed) = Disposition::lookup(value) else {
                    return Err(self
                        .error(value_span, format!("unknown disposition `{value}`"))
                        .with_label("expected `keep`, `reclaimable` or `review`"));
                };
                rule.disposition = Some(Spanned::new(parsed, value_span));
            }
            other => {
                let diagnostic = self
                    .error(key_span, format!("unknown key `{other}`"))
                    .with_label("not a rule key");
                return Err(match nearest(other, KEYS.into_iter()) {
                    Some(hint) => diagnostic.with_help(format!("did you mean `{hint}`?")),
                    None => diagnostic.with_help(format!("the keys are {}", KEYS.join(", "))),
                });
            }
        }
        Ok(())
    }

    /// Resolve a finished draft: required keys, the kind, the disposition
    /// against the kind, and the description against the filter's bindings.
    fn finish(&self, draft: Draft) -> Result<Rule> {
        let title = &draft.title;
        let missing = |key: &str, meaning: &str| {
            self.error(title.span, format!("rule `{}` is missing `{key}`", title.value))
                .with_label(format!("`{key}` is required"))
                .with_help(format!("`{key}` is {meaning}"))
        };
        let Some(description) = draft.description else {
            return Err(missing(
                "description",
                "the sentence a human reads before approving a deletion",
            ));
        };
        let Some(kind) = draft.kind else {
            return Err(missing("kind", "what the path is, as in `kind = cache/v1`"));
        };
        let Some((filter, filter_span)) = draft.filter else {
            return Err(missing("filter", "the block saying which paths the rule matches"));
        };
        let kind = self.kind(&kind)?;

        let disposition = match draft.disposition {
            Some(written) if written.value.strength() > kind.value.disposition.strength() => {
                return Err(self
                    .error(
                        written.span,
                        format!(
                            "`{}` is stronger than kind `{}` allows",
                            written.value.name(),
                            kind.value
                        ),
                    )
                    .with_label(format!(
                        "`{}` defaults to `{}`, and a rule may only downgrade it",
                        kind.value,
                        kind.value.disposition.name()
                    ))
                    .with_help(
                        "the order is reclaimable, then review, then keep; a stronger claim \
                         needs its own kind, declared in pack.toml",
                    ));
            }
            Some(written) => written.value,
            None => kind.value.disposition,
        };

        let mut bound = vec![filter.var.value.as_str()];
        bound.extend(filter.constraints.iter().filter_map(|constraint| match constraint {
            Constraint::Children(ChildTest { capture: Some(capture), .. }) => {
                Some(capture.value.as_str())
            }
            _ => None,
        }));
        self.check_template(&description, &bound)?;

        Ok(Rule {
            span: draft.title.span.to(filter_span),
            title: draft.title,
            description,
            kind,
            disposition,
            filter,
        })
    }

    fn kind(&self, written: &Spanned<String>) -> Result<Spanned<Kind>> {
        let Some((name, version)) = split_id(&written.value) else {
            return Err(self
                .error(written.span, format!("`{}` is not a kind", written.value))
                .with_label("expected `name/vN`")
                .with_help("a kind names what the path is and the version of that meaning, as in `cache/v1`"));
        };
        if let Some(kind) = self.kinds.lookup(name, version) {
            return Ok(Spanned::new(kind.clone(), written.span));
        }
        let versions = self.kinds.versions(name);
        if !versions.is_empty() {
            let known = versions.iter().map(|v| format!("`{name}/v{v}`")).collect::<Vec<_>>();
            return Err(self
                .error(written.span, format!("kind `{name}` has no version {version}"))
                .with_label(format!("known: {}", known.join(", ")))
                .with_help(
                    "a version is a fixed meaning; a rule written against a version that is not \
                     here would be read under a meaning it was not written for",
                ));
        }
        let names: Vec<String> = self.kinds.iter().map(|kind| kind.to_string()).collect();
        let diagnostic = self
            .error(written.span, format!("unknown kind `{}`", written.value))
            .with_label("not a built-in kind or one this pack declares");
        Err(match nearest(&written.value, names.iter().map(String::as_str)) {
            Some(hint) => diagnostic.with_help(format!("did you mean `{hint}`?")),
            None => diagnostic.with_help(format!(
                "declare it in pack.toml under `[kinds.\"{}\"]`, or use one of {}",
                written.value,
                names.join(", ")
            )),
        })
    }

    /// A description's `{field}` holes are vocabulary names and its `{$var}`
    /// holes are filter bindings, and both are checked here: an unchecked hole
    /// is a typo that survives until the sentence a human approves a deletion
    /// on is printed with a hole in it.
    fn check_template(&self, description: &Spanned<String>, bound: &[&str]) -> Result<()> {
        let text = &description.value;
        let at = |offset: usize, len: usize| {
            let start = description.span.start + offset;
            Span::new(start, (start + len).min(description.span.end))
        };
        let pieces = match crate::eval::template_pieces(text) {
            Ok(pieces) => pieces,
            Err(TemplateError::UnknownField { name, at: offset, len }) => {
                let diagnostic = self
                    .error(at(offset, len), format!("unknown field `{name}` in `description`"))
                    .with_label("not in the rule vocabulary");
                return Err(match nearest(name, FIELDS.iter().map(|d| d.name)) {
                    Some(suggestion) => {
                        diagnostic.with_help(format!("did you mean `{suggestion}`?"))
                    }
                    None => diagnostic.with_help(
                        "`{field}` interpolates a field and `{$var}` a filter variable; \
                         write `{{` for a literal brace",
                    ),
                });
            }
            Err(TemplateError::Unclosed { at: offset }) => {
                return Err(self
                    .error(at(offset, text.len() - offset), "unclosed `{` in `description`")
                    .with_label("expected a name and a closing `}`")
                    .with_help(
                        "`{field}` interpolates a field and `{$var}` a filter variable; \
                         write `{{` for a literal brace",
                    ));
            }
        };
        for piece in pieces {
            let Piece::Var(name) = piece else { continue };
            if bound.contains(&name) {
                continue;
            }
            let hole = format!("{{${name}}}");
            let offset = text.find(&hole).unwrap_or(0);
            let listed = bound.iter().map(|b| format!("`${b}`")).collect::<Vec<_>>();
            return Err(self
                .error(at(offset, hole.len()), format!("`${name}` is not bound by this filter"))
                .with_label("nothing in the filter names this variable")
                .with_help(format!(
                    "the filter binds {}; `as $name` after a `has` binds the matched name",
                    listed.join(" and ")
                )));
        }
        Ok(())
    }

    // -- the filter --------------------------------------------------------

    /// Everything up to the closing `}`. `opener` spans the `filter {` line.
    fn filter(&mut self, opener: Span) -> Result<(Filter, Span)> {
        let mut draft = FilterDraft::default();
        let close = loop {
            let Some(span) = self.line() else {
                return Err(self
                    .error(opener, "unclosed `filter {`")
                    .with_label("this block has no closing `}`"));
            };
            if &self.source.text[span.start..span.end] == "}" {
                break span;
            }
            let words = lex::words(self.source, span.start, span.end)?;
            if words.is_empty() {
                continue;
            }
            self.constraint(&mut draft, &words, span)?;
        };

        let Some((var, then)) = draft.then else {
            return Err(self
                .error(opener, "this `filter` has no `then`")
                .with_label("expected a `then` line before the closing `}`")
                .with_help("end the filter with the path the verdict lands on, as in `then $dir/target/`"));
        };
        for capture in &draft.captures {
            if capture.value == var.value {
                return Err(self
                    .error(capture.span, format!("`${}` is already the node `then` starts from", var.value))
                    .with_label("a capture needs a name of its own"));
            }
            if draft.captures.iter().filter(|other| other.value == capture.value).count() > 1 {
                return Err(self
                    .error(capture.span, format!("`${}` is captured twice", capture.value))
                    .with_label("each capture needs a name of its own"));
            }
        }
        for subject in &draft.subjects {
            if subject.value == var.value {
                continue;
            }
            if draft.captures.iter().any(|capture| capture.value == subject.value) {
                return Err(self
                    .error(subject.span, format!("`${}` is a captured name, not a node", subject.value))
                    .with_label("a capture holds the name a `has` matched, for the description")
                    .with_help(format!("constraints test `${}`, the node `then` starts from", var.value)));
            }
            return Err(self
                .error(subject.span, format!("`${}` is not bound", subject.value))
                .with_label(format!("this filter's `then` starts from `${}`", var.value))
                .with_help("a filter talks about one node, the one `then` starts from"));
        }

        let filter = Filter { var, constraints: draft.constraints, then };
        Ok((filter, opener.to(close)))
    }

    fn constraint(&self, draft: &mut FilterDraft, words: &[Word], line: Span) -> Result<()> {
        let first = &words[0];
        if let Some((_, then)) = &draft.then {
            return Err(self
                .error(line, "`then` ends the filter")
                .with_label("nothing may follow `then`")
                .with_help(format!(
                    "move this above the `then` on line {}",
                    self.line_of(then.span.start)
                )));
        }
        if first.is("then") {
            draft.then = Some(self.then(words, line)?);
            return Ok(());
        }
        if first.is("not") {
            let subject = words.get(1).and_then(|word| self.subject(word).transpose());
            let Some(subject) = subject.transpose()? else {
                return Err(self
                    .error(line, "`not` needs a field test after it")
                    .with_label("expected `not $v.field`")
                    .with_help("write `lacks` rather than `not ... has`"));
            };
            let (var, Some(field)) = subject else {
                return Err(self
                    .error(words[1].span, "`not` applies to a field test only")
                    .with_label("expected `$v.field`")
                    .with_help("write `$v lacks name` rather than `not $v has name`"));
            };
            draft.subjects.push(var);
            let test = self.field_test(field, &words[2..], true, line)?;
            draft.constraints.push(Constraint::Field(test));
            return Ok(());
        }
        let Some((var, field)) = self.subject(first)? else {
            return Err(self
                .error(first.span, format!("expected a constraint, found `{}`", first.text))
                .with_label("a filter line starts with `$var`, `not`, or `then`"));
        };
        draft.subjects.push(var);
        if let Some(field) = field {
            let test = self.field_test(field, &words[1..], false, line)?;
            draft.constraints.push(Constraint::Field(test));
            return Ok(());
        }
        let Some(verb) = words.get(1) else {
            return Err(self
                .error(line, "a variable alone is not a constraint")
                .with_label("expected `has`, `lacks`, `under` or `.field` after it"));
        };
        if verb.is("has") || verb.is("lacks") {
            let test = self.children(verb, &words[2..], line)?;
            if let Some(capture) = &test.capture {
                draft.captures.push(capture.clone());
            }
            draft.constraints.push(Constraint::Children(test));
        } else if verb.is("under") {
            draft.constraints.push(Constraint::Under(self.under(verb, &words[2..])?));
        } else {
            return Err(self
                .error(verb.span, format!("unknown constraint `{}`", verb.text))
                .with_label("expected `has`, `lacks` or `under`"));
        }
        Ok(())
    }

    /// `$v` or `$v.field`, or `None` when the word is not a variable at all.
    fn subject(&self, word: &Word) -> Result<Option<Subject>> {
        if word.quoted || !word.text.starts_with('$') {
            return Ok(None);
        }
        let body = &word.text[1..];
        let (name, field) = match body.split_once('.') {
            Some((name, field)) => (name, Some(field)),
            None => (body, None),
        };
        let name_span = Span::new(word.span.start, word.span.start + 1 + name.len());
        let var = Spanned::new(self.var_name(name, name_span)?, name_span);
        let Some(field_name) = field else { return Ok(Some((var, None))) };
        let field_span = Span::new(name_span.end + 1, word.span.end);
        let Some(field) = Field::lookup(field_name) else {
            let diagnostic = self
                .error(field_span, format!("unknown field `{field_name}`"))
                .with_label("not in the rule vocabulary");
            return Err(match nearest(field_name, FIELDS.iter().map(|d| d.name)) {
                Some(hint) => diagnostic.with_help(format!("did you mean `{hint}`?")),
                None => diagnostic,
            });
        };
        Ok(Some((var, Some(Spanned::new(field, field_span)))))
    }

    fn var_name(&self, name: &str, span: Span) -> Result<String> {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(self
                .error(span, format!("`${name}` is not a variable name"))
                .with_label("expected letters, digits and `_` after `$`"));
        }
        Ok(name.to_owned())
    }

    fn field_test(
        &self,
        field: Spanned<Field>,
        rest: &[Word],
        negated: bool,
        line: Span,
    ) -> Result<FieldTest> {
        let ty = field.value.ty();
        let Some(op_word) = rest.first() else {
            if ty != Ty::Bool {
                return Err(self
                    .error(field.span, format!("`{}` is a {ty}, not a condition", field.value.name()))
                    .with_label("expected a comparison here")
                    .with_help(format!(
                        "compare it, as in `$f.{} >= {}`",
                        field.value.name(),
                        match ty {
                            Ty::Str => "\"something\"",
                            Ty::Size => "100mb",
                            Ty::Duration => "90d",
                            _ => "3",
                        }
                    )));
            }
            return Ok(FieldTest { field, compare: None, negated, span: line });
        };
        if op_word.is("=") {
            return Err(self
                .error(op_word.span, "`=` sets a key; a filter compares with `==`")
                .with_label("expected `==`"));
        }
        let Some(op) = CmpOp::lookup(&op_word.text).filter(|_| !op_word.quoted) else {
            return Err(self
                .error(op_word.span, format!("expected a comparison, found `{}`", op_word.text))
                .with_label("expected one of == != < > <= >="));
        };
        let Some(value_word) = rest.get(1) else {
            return Err(self
                .error(op_word.span, format!("`{}` has nothing to compare against", op.symbol()))
                .with_label(format!("expected {}", ty.example())));
        };
        if let Some(extra) = rest.get(2) {
            return Err(self
                .error(extra.span, "a field test is one comparison")
                .with_label("unexpected after the value")
                .with_help("put each constraint on its own line; lines are ANDed"));
        }
        let literal = match lex::literal(self.source, value_word)? {
            Some(literal) => literal,
            // A bare word is a string where a string is expected: `$f.ext == zip`.
            None if ty == Ty::Str => Literal::Str(value_word.text.clone()),
            None => {
                return Err(self
                    .error(value_word.span, format!("expected {}", ty.example()))
                    .with_label("not a literal value"));
            }
        };
        if literal.ty() != ty {
            return Err(self
                .error(
                    value_word.span,
                    format!(
                        "`{}` is a {ty}, but it is compared to a {}",
                        field.value.name(),
                        literal.ty()
                    ),
                )
                .with_label(format!("expected {}", ty.example()))
                .with_help(field.value.def().doc.to_owned()));
        }
        if op.needs_order() && !ty.is_ordered() {
            return Err(self
                .error(op_word.span, format!("`{ty}` cannot be ordered"))
                .with_label(format!("`{}` is not defined for {ty}s", op.symbol()))
                .with_help("only `==` and `!=` apply here"));
        }
        Ok(FieldTest {
            field,
            compare: Some((
                Spanned::new(op, op_word.span),
                Spanned::new(literal, value_word.span),
            )),
            negated,
            span: line,
        })
    }

    /// `has a | b as $m` or `lacks a | b`, after the verb.
    fn children(&self, verb: &Word, rest: &[Word], line: Span) -> Result<ChildTest> {
        let negated = verb.is("lacks");
        let mut names = Vec::new();
        let mut capture = None;
        let mut at = 0;
        loop {
            let Some(word) = rest.get(at) else {
                return Err(self
                    .error(rest.last().map_or(verb.span, |w| w.span), "expected a name")
                    .with_label(format!("`{}` needs a file name after it", verb.text)));
            };
            names.push(self.child_name(word)?);
            at += 1;
            match rest.get(at) {
                None => break,
                Some(next) if next.is("|") => at += 1,
                Some(next) if next.is("as") => {
                    if negated {
                        return Err(self
                            .error(next.span, "`lacks` cannot capture")
                            .with_label("there is no matched name to bind"));
                    }
                    let Some(target) = rest.get(at + 1).filter(|w| !w.quoted && w.text.starts_with('$'))
                    else {
                        return Err(self
                            .error(next.span, "`as` needs a variable after it")
                            .with_label("expected `as $name`"));
                    };
                    let name = self.var_name(&target.text[1..], target.span)?;
                    capture = Some(Spanned::new(name, target.span));
                    if let Some(extra) = rest.get(at + 2) {
                        return Err(self
                            .error(extra.span, "`as $name` ends the constraint")
                            .with_label("unexpected after the capture"));
                    }
                    break;
                }
                Some(next) => {
                    return Err(self
                        .error(next.span, format!("unexpected `{}`", next.text))
                        .with_label("expected `|` before another name")
                        .with_help("alternatives are written `a | b`"));
                }
            }
        }
        Ok(ChildTest { negated, names, capture, span: line })
    }

    fn child_name(&self, word: &Word) -> Result<Spanned<NamePattern>> {
        if !word.quoted && (word.text == "|" || word.text == "as" || word.text.starts_with('$')) {
            return Err(self
                .error(word.span, format!("expected a name, found `{}`", word.text))
                .with_label("expected a file name or glob"));
        }
        if word.text.contains('/') || word.text.contains('\\') {
            return Err(self
                .error(word.span, "a child name is one path component")
                .with_label("no `/` here")
                .with_help("`has` and `lacks` look at direct children only"));
        }
        Ok(Spanned::new(NamePattern::of(&word.text), word.span))
    }

    /// `under Name/`, after the verb.
    fn under(&self, verb: &Word, rest: &[Word]) -> Result<Spanned<String>> {
        let Some(word) = rest.first() else {
            return Err(self
                .error(verb.span, "`under` needs a directory name after it")
                .with_label("expected `under Name/`"));
        };
        if let Some(extra) = rest.get(1) {
            return Err(self
                .error(extra.span, "`under` takes one name")
                .with_help("write one `under` line per ancestor; lines are ANDed"));
        }
        let name = word.text.strip_suffix('/').unwrap_or(&word.text);
        if name.is_empty() || name.contains(['/', '\\']) || NamePattern::of(name).literal().is_none()
        {
            return Err(self
                .error(word.span, format!("`{}` is not a directory name", word.text))
                .with_label("expected one literal name, as in `Downloads/`"));
        }
        Ok(Spanned::new(name.to_owned(), word.span))
    }

    /// `then $v`, `then $v/`, `then $v/a/b/`.
    fn then(&self, words: &[Word], line: Span) -> Result<(Spanned<String>, Target)> {
        let Some(path) = words.get(1).filter(|w| !w.quoted && w.text.starts_with('$')) else {
            return Err(self
                .error(words.get(1).map_or(line, |w| w.span), "`then` needs a path")
                .with_label("expected `$var` or `$var/name/`"));
        };
        if let Some(extra) = words.get(2) {
            return Err(self
                .error(extra.span, "`then` names one path")
                .with_label("unexpected after the path"));
        }
        let text = &path.text[1..];
        let dir = text.ends_with('/');
        let mut parts = text.trim_end_matches('/').split('/');
        let name = parts.next().unwrap_or_default();
        let name_span = Span::new(path.span.start, path.span.start + 1 + name.len());
        let var = Spanned::new(self.var_name(name, name_span)?, name_span);

        let mut segments = Vec::new();
        let mut offset = name_span.end + 1;
        for part in parts {
            let span = Span::new(offset, offset + part.len());
            if part.is_empty() || part == "." || part == ".." {
                return Err(self
                    .error(span, format!("`{part}` is not a path segment"))
                    .with_label("expected a file name or glob")
                    .with_help("a `then` path only goes down from its variable"));
            }
            segments.push(Spanned::new(NamePattern::of(part), span));
            offset = span.end + 1;
        }
        Ok((var, Target { segments, dir, span: path.span }))
    }
}
