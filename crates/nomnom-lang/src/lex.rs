//! One `filter` line to words, and a word to a literal.
//!
//! A rule's `filter` is line-oriented, one constraint per line, so there is no
//! token stream across lines — the parser hands each filter line here and gets
//! back its words. A word is a run of non-blank characters, a `"quoted string"`
//! (for a name with a space in it), or a lone `|`.
//!
//! Units are resolved here, never in the evaluator: a [`Literal::Size`] already
//! holds bytes and a [`Literal::Duration`] already holds seconds, so nothing
//! downstream ever multiplies by 1024 again.

use crate::ast::Literal;
use crate::diagnostic::{Diagnostic, Result, Source, Span};

/// One word of a filter line, with its span in the whole file.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    /// Whether it was written in quotes, which makes it a name and never a
    /// keyword, variable or operator.
    pub quoted: bool,
    pub span: Span,
}

impl Word {
    pub fn is(&self, keyword: &str) -> bool {
        !self.quoted && self.text == keyword
    }
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

/// The words of `source.text[start..end]`, one line of a filter block.
pub fn words(source: &Source, start: usize, end: usize) -> Result<Vec<Word>> {
    let text = &source.text[..end];
    let bytes = text.as_bytes();
    let mut pos = start;
    let mut out = Vec::new();
    loop {
        while pos < end && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= end || bytes[pos] == b'#' {
            return Ok(out);
        }
        let word_start = pos;
        if bytes[pos] == b'|' {
            pos += 1;
            out.push(Word { text: "|".into(), quoted: false, span: Span::new(word_start, pos) });
            continue;
        }
        if bytes[pos] == b'"' {
            let (value, after) = string(source, word_start, end)?;
            pos = after;
            out.push(Word { text: value, quoted: true, span: Span::new(word_start, pos) });
            continue;
        }
        while pos < end && !bytes[pos].is_ascii_whitespace() && bytes[pos] != b'|' {
            pos += 1;
        }
        out.push(Word {
            text: text[word_start..pos].to_owned(),
            quoted: false,
            span: Span::new(word_start, pos),
        });
    }
}

/// A quoted string starting at `start`; answers the unescaped value and the
/// offset just past the closing quote.
fn string(source: &Source, start: usize, end: usize) -> Result<(String, usize)> {
    let text = &source.text;
    let bytes = text.as_bytes();
    let mut pos = start + 1;
    let mut out = String::new();
    loop {
        if pos >= end {
            return Err(Diagnostic::new(source, Span::new(start, end), "unterminated string")
                .with_label("this string has no closing `\"`"));
        }
        match bytes[pos] {
            b'"' => return Ok((out, pos + 1)),
            b'\\' => {
                let escaped = match bytes.get(pos + 1) {
                    Some(b'"') => '"',
                    Some(b'\\') => '\\',
                    _ => {
                        return Err(Diagnostic::new(
                            source,
                            Span::new(pos, (pos + 2).min(end)),
                            "unknown escape",
                        )
                        .with_label("expected `\\\"` or `\\\\`"));
                    }
                };
                out.push(escaped);
                pos += 2;
            }
            _ => {
                let ch = text[pos..].chars().next().unwrap_or('\u{fffd}');
                out.push(ch);
                pos += ch.len_utf8();
            }
        }
    }
}

/// A word read as a literal value, or `None` when it is not shaped like one.
/// A word that starts like a number but carries an unknown unit is an error
/// rather than `None`, because `90days` is a typo, not a name.
pub fn literal(source: &Source, word: &Word) -> Result<Option<Literal>> {
    if word.quoted {
        return Ok(Some(Literal::Str(word.text.clone())));
    }
    match word.text.as_str() {
        "true" => return Ok(Some(Literal::Bool(true))),
        "false" => return Ok(Some(Literal::Bool(false))),
        _ => {}
    }
    let text = &word.text;
    let digits_end =
        text.bytes().position(|b| !(b.is_ascii_digit() || b == b'.')).unwrap_or(text.len());
    if digits_end == 0 || !text.as_bytes()[0].is_ascii_digit() {
        return Ok(None);
    }
    let digits = &text[..digits_end];
    let value: f64 = digits.parse().map_err(|_| {
        Diagnostic::new(source, word.span, format!("`{digits}` is not a valid number"))
    })?;
    let unit = text[digits_end..].to_ascii_lowercase();
    if unit.is_empty() {
        return Ok(Some(Literal::Num(value)));
    }
    if let Some((_, factor)) = SIZE_UNITS.iter().find(|(name, _)| *name == unit) {
        return Ok(Some(Literal::Size(scale(value, *factor))));
    }
    if let Some((_, factor)) = DURATION_UNITS.iter().find(|(name, _)| *name == unit) {
        return Ok(Some(Literal::Duration(scale(value, *factor))));
    }
    Err(Diagnostic::new(source, word.span, format!("unknown unit `{unit}`"))
        .with_label("not a size or duration unit")
        .with_help(
            "sizes: b kb mb gb tb (×1000) and kib mib gib tib (×1024); \
             durations: s min h d w mo y",
        ))
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
