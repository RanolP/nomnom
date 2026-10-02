//! Text to tokens.
//!
//! Units are resolved here, not in the parser and never in the evaluator: a
//! `Tok::Size` already holds bytes and a `Tok::Duration` already holds seconds,
//! so nothing downstream ever multiplies by 1024 again.

use crate::diagnostic::{Diagnostic, Result, Source, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// A bare word. `-` and `.` are word characters, so `build-output` and
    /// `dir.name` are each one token; the grammar has no arithmetic, so the
    /// minus sign is never an operator and this costs nothing.
    Ident(String),
    Str(String),
    Num(f64),
    /// Already normalised to bytes.
    Size(u64),
    /// Already normalised to seconds.
    Duration(u64),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Assign,
    EqEq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Eof,
}

impl Tok {
    /// How the token is named in an error message.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(name) => format!("`{name}`"),
            Tok::Str(_) => "a string".into(),
            Tok::Num(_) => "a number".into(),
            Tok::Size(_) => "a size".into(),
            Tok::Duration(_) => "a duration".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Assign => "`=`".into(),
            Tok::EqEq => "`==`".into(),
            Tok::Ne => "`!=`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Le => "`<=`".into(),
            Tok::Ge => "`>=`".into(),
            Tok::Eof => "end of file".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// Size units. Decimal suffixes are powers of 1000; the `i` suffixes are
/// powers of 1024. `1kb` is 1000 bytes and `1kib` is 1024 — the IEC reading,
/// which is what `docs/lang.md` means by "binary and decimal both understood".
const SIZE_UNITS: &[(&str, u64)] = &[
    ("b", 1),
    ("kb", 1000),
    ("mb", 1000u64.pow(2)),
    ("gb", 1000u64.pow(3)),
    ("tb", 1000u64.pow(4)),
    ("kib", 1024),
    ("mib", 1024u64.pow(2)),
    ("gib", 1024u64.pow(3)),
    ("tib", 1024u64.pow(4)),
];

/// Duration units, in seconds.
///
/// `mo` and `y` use the mean Gregorian year of 365.2425 days: `y` is
/// 31_556_952 s and `mo` is that over twelve, 2_629_746 s (30.436875 days). A
/// calendar month has no fixed length, but "not modified in 6 months" does not
/// need calendar precision — it needs a threshold that is stable across
/// machines and never silently off by a factor. The mean year gives both, and
/// the worst case error against a real calendar is under three days.
const DURATION_UNITS: &[(&str, u64)] = &[
    ("s", 1),
    ("min", 60),
    ("h", 3600),
    ("d", 86_400),
    ("w", 604_800),
    ("mo", 2_629_746),
    ("y", 31_556_952),
];

pub fn lex(source: &Source) -> Result<Vec<Token>> {
    Lexer { source, bytes: source.text.as_bytes(), pos: 0 }.run()
}

struct Lexer<'a> {
    source: &'a Source,
    bytes: &'a [u8],
    pos: usize,
}

impl Lexer<'_> {
    fn run(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.pos;
            let Some(byte) = self.peek() else {
                out.push(Token { tok: Tok::Eof, span: Span::new(start, start) });
                return Ok(out);
            };
            let tok = match byte {
                b'(' => self.single(Tok::LParen),
                b')' => self.single(Tok::RParen),
                b'{' => self.single(Tok::LBrace),
                b'}' => self.single(Tok::RBrace),
                b',' => self.single(Tok::Comma),
                b'=' => {
                    self.pos += 1;
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        Tok::EqEq
                    } else {
                        Tok::Assign
                    }
                }
                b'!' => {
                    self.pos += 1;
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        Tok::Ne
                    } else {
                        return Err(self
                            .error(Span::new(start, self.pos), "stray `!`")
                            .with_label("expected `!=`")
                            .with_help("negation is spelled `not`, as in `not is_dir`"));
                    }
                }
                b'<' => self.two_way(Tok::Le, Tok::Lt),
                b'>' => self.two_way(Tok::Ge, Tok::Gt),
                b'"' => self.string()?,
                b'0'..=b'9' => self.number()?,
                b if is_ident_start(b) => {
                    while self.peek().is_some_and(is_ident_continue) {
                        self.pos += 1;
                    }
                    Tok::Ident(self.slice(start).to_owned())
                }
                other => {
                    self.pos += 1;
                    let shown = char_at(&self.source.text, start);
                    return Err(self
                        .error(
                            Span::new(start, self.pos),
                            format!("unexpected character `{shown}`"),
                        )
                        .with_label(format!("byte 0x{other:02x} is not part of the language")));
                }
            };
            out.push(Token { tok, span: Span::new(start, self.pos) });
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn slice(&self, start: usize) -> &str {
        &self.source.text[start..self.pos]
    }

    fn single(&mut self, tok: Tok) -> Tok {
        self.pos += 1;
        tok
    }

    fn two_way(&mut self, with_eq: Tok, without: Tok) -> Tok {
        self.pos += 1;
        if self.peek() == Some(b'=') {
            self.pos += 1;
            with_eq
        } else {
            without
        }
    }

    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.source, span, message)
    }

    /// Whitespace and `#` line comments. `docs/lang.md` does not mention
    /// comments; a rule file's `reason` is its documentation, but a pack author
    /// still needs to disable a rule, so `#` (TOML's, matching `pack.toml`)
    /// is accepted.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => self.pos += 1,
                Some(b'#') => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn string(&mut self) -> Result<Tok> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n') => {
                    return Err(self
                        .error(Span::new(start, self.pos), "unterminated string")
                        .with_label("this string has no closing `\"`"));
                }
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(Tok::Str(out));
                }
                Some(b'\\') => {
                    let esc_start = self.pos;
                    self.pos += 1;
                    let escaped = match self.peek() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'n') => '\n',
                        Some(b't') => '\t',
                        _ => {
                            self.pos = (self.pos + 1).min(self.bytes.len());
                            return Err(self
                                .error(Span::new(esc_start, self.pos), "unknown escape")
                                .with_label("expected one of `\\\"`, `\\\\`, `\\n`, `\\t`"));
                        }
                    };
                    self.pos += 1;
                    out.push(escaped);
                }
                Some(_) => {
                    let ch = char_at(&self.source.text, self.pos);
                    self.pos += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    fn number(&mut self) -> Result<Tok> {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            self.pos += 1;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let digits = self.slice(start);
        let value: f64 = digits.parse().map_err(|_| {
            self.error(Span::new(start, self.pos), format!("`{digits}` is not a valid number"))
        })?;

        let unit_start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if unit_start == self.pos {
            return Ok(Tok::Num(value));
        }
        let unit = self.source.text[unit_start..self.pos].to_ascii_lowercase();
        let span = Span::new(start, self.pos);
        if let Some((_, factor)) = SIZE_UNITS.iter().find(|(name, _)| *name == unit) {
            return Ok(Tok::Size(scale(value, *factor)));
        }
        if let Some((_, factor)) = DURATION_UNITS.iter().find(|(name, _)| *name == unit) {
            return Ok(Tok::Duration(scale(value, *factor)));
        }
        Err(self
            .error(span, format!("unknown unit `{unit}`"))
            .with_label("not a size or duration unit")
            .with_help(
                "sizes: b kb mb gb tb (×1000) and kib mib gib tib (×1024); \
                 durations: s min h d w mo y",
            ))
    }
}

/// `1.5gb` is legal, so the product is rounded rather than truncated, and
/// saturated rather than wrapped.
fn scale(value: f64, factor: u64) -> u64 {
    let product = (value * factor as f64).round();
    if product < 0.0 {
        0
    } else if product >= u64::MAX as f64 {
        u64::MAX
    } else {
        product as u64
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

fn char_at(text: &str, offset: usize) -> char {
    text[offset..].chars().next().unwrap_or('\u{fffd}')
}
