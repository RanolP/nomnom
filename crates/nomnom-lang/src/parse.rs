//! Tokens to a validated [`Rule`] list.
//!
//! Hand-written recursive descent, because the grammar is ten productions and
//! the error messages are the product. Everything that can be checked without
//! a filesystem is checked here — arity, types, required conclusion fields,
//! the confidence range — so that a bad rule is a startup error rather than a
//! wrong deletion later.
//!
//! Precedence, tightest first: `not`, then `and`, then `or`. So
//! `a and b or c` parses as `(a and b) or c` and `not a and b` as
//! `(not a) and b`.

use crate::ast::{CmpOp, Conclusion, Disposition, Expr, Literal, Rule, Spanned};
use crate::diagnostic::{Diagnostic, Result, Source, Span};
use crate::eval::TemplateError;
use crate::lex::{Tok, Token, lex};
use crate::vocab::{FIELDS, Field, PREDICATES, Predicate, Ty, nearest};

/// Parse a whole rule file.
pub fn parse(source: &Source) -> Result<Vec<Rule>> {
    let tokens = lex(source)?;
    let mut parser = Parser { source, tokens, pos: 0 };
    let mut rules = Vec::new();
    while !parser.at_eof() {
        rules.push(parser.rule()?);
    }
    Ok(rules)
}

struct Parser<'a> {
    source: &'a Source,
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser<'_> {
    // -- cursor ------------------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn at_eof(&self) -> bool {
        self.peek().tok == Tok::Eof
    }

    fn bump(&mut self) -> Token {
        let token = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    /// True (and consumes) when the next token is the bare word `word`.
    fn eat_word(&mut self, word: &str) -> Option<Span> {
        match &self.peek().tok {
            Tok::Ident(name) if name == word => Some(self.bump().span),
            _ => None,
        }
    }

    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.source, span, message)
    }

    /// The standard "expected X, found Y" failure at the cursor.
    fn expected(&self, what: &str) -> Diagnostic {
        let token = self.peek();
        self.error(token.span, format!("expected {what}, found {}", token.tok.describe()))
            .with_label(format!("expected {what}"))
    }

    fn expect(&mut self, tok: Tok, what: &str) -> Result<Span> {
        if self.peek().tok == tok { Ok(self.bump().span) } else { Err(self.expected(what)) }
    }

    fn expect_word(&mut self, word: &str) -> Result<Span> {
        self.eat_word(word).ok_or_else(|| self.expected(&format!("`{word}`")))
    }

    /// A bare word used as a name: a label, a disposition, `true`.
    fn expect_name(&mut self, what: &str) -> Result<Spanned<String>> {
        match &self.peek().tok {
            Tok::Ident(name) => {
                let name = name.clone();
                let span = self.bump().span;
                Ok(Spanned::new(name, span))
            }
            _ => Err(self.expected(what)),
        }
    }

    // -- rules -------------------------------------------------------------

    fn rule(&mut self) -> Result<Rule> {
        let start = self.eat_word("rule").ok_or_else(|| {
            let token = self.peek();
            self.error(token.span, format!("expected `rule`, found {}", token.tok.describe()))
                .with_label("a rule file contains only `rule` blocks")
                .with_help("every rule starts `rule \"some-name\" { when ... then ... }`")
        })?;

        let name = match &self.peek().tok {
            Tok::Str(name) => {
                let name = name.clone();
                let span = self.bump().span;
                Spanned::new(name, span)
            }
            _ => {
                return Err(self
                    .expected("a quoted rule name")
                    .with_help("rule names are strings, as in `rule \"cargo-target\"`"));
            }
        };
        if name.value.trim().is_empty() {
            return Err(self
                .error(name.span, "rule name is empty")
                .with_label("a rule is cited by name in every verdict it produces"));
        }

        self.expect(Tok::LBrace, "`{`")?;
        self.expect_word("when")?;
        let when = self.expr()?;
        let then_span = self.expect_word("then")?;
        let then = self.conclusion(then_span)?;
        let end = self.expect(Tok::RBrace, "`}`")?;

        Ok(Rule { name, when, then, span: start.to(end) })
    }

    // -- expressions -------------------------------------------------------

    fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.and_expr()?;
        while self.eat_word("or").is_some() {
            let rhs = self.and_expr()?;
            lhs = Expr::Or { lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.unary()?;
        while self.eat_word("and").is_some() {
            let rhs = self.unary()?;
            lhs = Expr::And { lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr> {
        if let Some(start) = self.eat_word("not") {
            let operand = self.unary()?;
            let span = start.to(operand.span());
            return Ok(Expr::Not { operand: Box::new(operand), span });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.peek().tok == Tok::LParen {
            self.bump();
            let inner = self.expr()?;
            self.expect(Tok::RParen, "`)`")?;
            return Ok(inner);
        }

        let token = self.peek().clone();
        let Tok::Ident(name) = &token.tok else {
            return Err(self
                .expected("a field, a predicate or `(`")
                .with_help("a `when` clause is built from the vocabulary in `docs/lang.md`"));
        };
        let name = name.clone();
        self.bump();

        if name == "true" || name == "false" {
            return Ok(Expr::Bool(Spanned::new(name == "true", token.span)));
        }
        if let Some(predicate) = Predicate::lookup(&name) {
            return self.call(predicate, token.span);
        }
        if let Some(field) = Field::lookup(&name) {
            return self.field_expr(field, token.span);
        }
        Err(self.unknown_name(&name, token.span))
    }

    fn unknown_name(&self, name: &str, span: Span) -> Diagnostic {
        let known = FIELDS.iter().map(|d| d.name).chain(PREDICATES.iter().map(|d| d.name));
        let diagnostic = self
            .error(span, format!("unknown field or predicate `{name}`"))
            .with_label("not in the rule vocabulary");
        match nearest(name, known) {
            Some(suggestion) => diagnostic.with_help(format!("did you mean `{suggestion}`?")),
            None => diagnostic,
        }
    }

    fn call(&mut self, predicate: Predicate, name_span: Span) -> Result<Expr> {
        self.expect(Tok::LParen, "`(`")?;
        let mut args: Vec<Spanned<Literal>> = Vec::new();
        if self.peek().tok != Tok::RParen {
            loop {
                args.push(self.literal()?);
                if self.peek().tok == Tok::Comma {
                    self.bump();
                    continue;
                }
                break;
            }
        }
        let close = self.expect(Tok::RParen, "`)`")?;
        let span = name_span.to(close);

        let params = predicate.params();
        if args.len() != params.len() {
            let expected = params.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
            return Err(self
                .error(
                    span,
                    format!(
                        "`{}` takes {} argument{}, but {} {} given",
                        predicate.name(),
                        params.len(),
                        if params.len() == 1 { "" } else { "s" },
                        args.len(),
                        if args.len() == 1 { "was" } else { "were" }
                    ),
                )
                .with_label(format!("expected `{}({expected})`", predicate.name()))
                .with_help(predicate.def().doc.to_owned()));
        }

        for (arg, param) in args.iter().zip(params) {
            if arg.ty() != *param {
                return Err(self
                    .error(
                        arg.span,
                        format!(
                            "`{}` expects a {param} here, found a {}",
                            predicate.name(),
                            arg.ty()
                        ),
                    )
                    .with_label(format!("expected {}", param.example())));
            }
        }
        Ok(Expr::Call { predicate: Spanned::new(predicate, name_span), args, span })
    }

    fn field_expr(&mut self, field: Field, span: Span) -> Result<Expr> {
        let op = match self.peek().tok {
            Tok::EqEq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Gt => CmpOp::Gt,
            Tok::Le => CmpOp::Le,
            Tok::Ge => CmpOp::Ge,
            Tok::Assign => {
                return Err(self
                    .error(self.peek().span, "`=` is assignment, not comparison")
                    .with_label("expected `==`"));
            }
            // A bare field is an expression only when it is already a bool.
            _ => {
                return if field.ty() == Ty::Bool {
                    Ok(Expr::Field(Spanned::new(field, span)))
                } else {
                    Err(self
                        .error(
                            span,
                            format!("`{}` is a {}, not a condition", field.name(), field.ty()),
                        )
                        .with_label("expected a comparison here")
                        .with_help(format!(
                            "compare it, as in `{} == {}`",
                            field.name(),
                            match field.ty() {
                                Ty::Str => "\"something\"",
                                Ty::Size => "100mb",
                                _ => "3",
                            }
                        )))
                };
            }
        };
        let op_span = self.bump().span;
        let rhs = self.literal()?;

        if rhs.ty() != field.ty() {
            return Err(self
                .error(
                    rhs.span,
                    format!(
                        "`{}` is a {}, but it is compared to a {}",
                        field.name(),
                        field.ty(),
                        rhs.ty()
                    ),
                )
                .with_label(format!("expected {}", field.ty().example()))
                .with_help(field.def().doc.to_owned()));
        }
        if op.needs_order() && !field.ty().is_ordered() {
            return Err(self
                .error(op_span, format!("`{}` cannot be ordered", field.ty()))
                .with_label(format!("`{}` is not defined for {}s", op.symbol(), field.ty()))
                .with_help("only `==` and `!=` apply here"));
        }

        Ok(Expr::Compare { lhs: Spanned::new(field, span), op: Spanned::new(op, op_span), rhs })
    }

    fn literal(&mut self) -> Result<Spanned<Literal>> {
        let token = self.peek().clone();
        let value = match &token.tok {
            Tok::Str(s) => Literal::Str(s.clone()),
            Tok::Num(n) => Literal::Num(*n),
            Tok::Size(b) => Literal::Size(*b),
            Tok::Duration(s) => Literal::Duration(*s),
            Tok::Ident(name) if name == "true" => Literal::Bool(true),
            Tok::Ident(name) if name == "false" => Literal::Bool(false),
            _ => return Err(self.expected("a literal value")),
        };
        self.bump();
        Ok(Spanned::new(value, token.span))
    }

    // -- conclusions -------------------------------------------------------

    /// `then_span` points at the `then` keyword, which is where a
    /// missing-field error has to land: the mistake is the absence, and an
    /// absence has no span of its own.
    fn conclusion(&mut self, then_span: Span) -> Result<Conclusion> {
        let start = self.peek().span;
        let mut label: Option<Spanned<String>> = None;
        let mut disposition: Option<Spanned<Disposition>> = None;
        let mut reason: Option<Spanned<String>> = None;
        let mut confidence: Option<Spanned<f32>> = None;
        let mut unit: Option<Spanned<bool>> = None;
        let mut end = start;

        while self.peek().tok != Tok::RBrace && !self.at_eof() {
            let key = self.expect_name("a `then` field or `}`")?;
            self.expect(Tok::Assign, "`=`")?;
            match key.value.as_str() {
                "label" => {
                    let value = self.expect_name("a label name")?;
                    end = value.span;
                    self.set_once(&mut label, value, &key)?;
                }
                "disposition" => {
                    let name = self.expect_name("a disposition")?;
                    end = name.span;
                    let parsed = Disposition::lookup(&name.value).ok_or_else(|| {
                        self.error(name.span, format!("unknown disposition `{}`", name.value))
                            .with_label("expected `keep`, `reclaimable` or `review`")
                            .with_help(
                                "`reclaimable` proposes a deletion, `review` asks a human, \
                                 `keep` protects the path",
                            )
                    })?;
                    self.set_once(&mut disposition, Spanned::new(parsed, name.span), &key)?;
                }
                "reason" => {
                    let token = self.peek().clone();
                    let Tok::Str(text) = &token.tok else {
                        return Err(self.expected("a quoted reason").with_help(
                            "the reason names the evidence, as in \
                             \"`Cargo.toml` sits beside it\"",
                        ));
                    };
                    let text = text.clone();
                    self.bump();
                    end = token.span;
                    if text.trim().is_empty() {
                        return Err(self
                            .error(token.span, "`reason` is empty")
                            .with_label("a reason must say why")
                            .with_help(
                                "this is the sentence a human approves a deletion on; \
                                 name the evidence, not the label",
                            ));
                    }
                    self.check_template(&text, token.span)?;
                    self.set_once(&mut reason, Spanned::new(text, token.span), &key)?;
                }
                "confidence" => {
                    let token = self.peek().clone();
                    let Tok::Num(value) = token.tok else {
                        return Err(self.expected("a number between 0.0 and 1.0"));
                    };
                    self.bump();
                    end = token.span;
                    if !(0.0..=1.0).contains(&value) {
                        return Err(self
                            .error(token.span, format!("confidence {value} is out of range"))
                            .with_label("expected a number between 0.0 and 1.0")
                            .with_help(
                                "confidence is the first tie-break between competing rules, \
                                 so the scale has to mean the same thing in every pack",
                            ));
                    }
                    self.set_once(&mut confidence, Spanned::new(value as f32, token.span), &key)?;
                }
                "unit" => {
                    let name = self.expect_name("`true` or `false`")?;
                    end = name.span;
                    let value = match name.value.as_str() {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(self
                                .error(name.span, format!("`unit` is a bool, found `{other}`"))
                                .with_label("expected `true` or `false`"));
                        }
                    };
                    self.set_once(&mut unit, Spanned::new(value, name.span), &key)?;
                }
                other => {
                    let known = ["label", "disposition", "reason", "confidence", "unit"];
                    let diagnostic = self
                        .error(key.span, format!("unknown `then` field `{other}`"))
                        .with_label("not a conclusion field");
                    return Err(match nearest(other, known.into_iter()) {
                        Some(hint) => diagnostic.with_help(format!("did you mean `{hint}`?")),
                        None => {
                            diagnostic.with_help(format!("expected one of {}", known.join(", ")))
                        }
                    });
                }
            }
        }

        let span = start.to(end);
        let unit_written = unit.as_ref().map(|it| it.span);
        Ok(Conclusion {
            label: self.require(label, then_span, "label", "what the path is")?,
            disposition: self.require(
                disposition,
                then_span,
                "disposition",
                "`keep`, `reclaimable` or `review`",
            )?,
            reason: self.require(
                reason,
                then_span,
                "reason",
                "the sentence a human reads before approving a deletion",
            )?,
            confidence: self.require(
                confidence,
                then_span,
                "confidence",
                "a number between 0.0 and 1.0",
            )?,
            unit: unit.unwrap_or(Spanned::new(false, span)),
            unit_written,
            span,
        })
    }

    /// A `reason` is a template, so its `{field}` holes are vocabulary names
    /// and have to be checked here for the same reason a `when` field is: an
    /// unchecked hole is a typo that survives until the sentence a human
    /// approves a deletion on is printed with a hole in it.
    fn check_template(&self, text: &str, literal: Span) -> Result<()> {
        match crate::eval::template_pieces(text) {
            Ok(_) => Ok(()),
            Err(TemplateError::UnknownField { name, at, len }) => {
                let span = self.inside_literal(literal, at, len);
                let diagnostic = self
                    .error(span, format!("unknown field `{name}` in `reason`"))
                    .with_label("not in the rule vocabulary");
                Err(match nearest(name, FIELDS.iter().map(|d| d.name)) {
                    Some(suggestion) => {
                        diagnostic.with_help(format!("did you mean `{suggestion}`?"))
                    }
                    None => diagnostic.with_help(
                        "`{field}` interpolates a field; write `{{` for a literal brace",
                    ),
                })
            }
            Err(TemplateError::Unclosed { at }) => {
                let span = self.inside_literal(literal, at, text.len() - at);
                Err(self
                    .error(span, "unclosed `{` in `reason`")
                    .with_label("expected a field name and a closing `}`")
                    .with_help("`{field}` interpolates a field; write `{{` for a literal brace"))
            }
        }
    }

    /// An offset into the *unescaped* string mapped back onto the source.
    ///
    /// The lexer already resolved `\n` and friends, so an offset only maps
    /// faithfully when nothing was escaped away. When something was, the whole
    /// literal is pointed at: a caret in the wrong place is worse than a wide
    /// one in the right place.
    fn inside_literal(&self, literal: Span, at: usize, len: usize) -> Span {
        let raw = &self.source.text[literal.start..literal.end];
        if raw.contains('\\') {
            return literal;
        }
        let start = literal.start + 1 + at;
        Span::new(start.min(literal.end), (start + len).min(literal.end))
    }

    fn set_once<T>(
        &self,
        slot: &mut Option<Spanned<T>>,
        value: Spanned<T>,
        key: &Spanned<String>,
    ) -> Result<()> {
        if slot.is_some() {
            return Err(self
                .error(key.span, format!("`{}` is set twice", key.value))
                .with_label("already given above in this `then`"));
        }
        *slot = Some(value);
        Ok(())
    }

    fn require<T>(
        &self,
        slot: Option<Spanned<T>>,
        span: Span,
        name: &str,
        meaning: &str,
    ) -> Result<Spanned<T>> {
        slot.ok_or_else(|| {
            self.error(span, format!("`then` is missing `{name}`"))
                .with_label(format!("`{name}` is required"))
                .with_help(format!("`{name}` is {meaning}"))
        })
    }
}

impl Spanned<Literal> {
    fn ty(&self) -> crate::vocab::Ty {
        self.value.ty()
    }
}
