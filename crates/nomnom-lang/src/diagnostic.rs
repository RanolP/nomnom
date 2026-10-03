//! What the user sees when a rule is wrong.
//!
//! A rule file is authored by hand, often by someone who is not us, and the
//! only feedback channel is this struct. So a [`Diagnostic`] carries the whole
//! context needed to print a rustc-shaped message on its own — file name,
//! source text and byte span — rather than expecting the caller to reunite
//! them later. [`Diagnostic`]'s `Display` is the product surface; treat a
//! change to it as a change to the UI.

use std::fmt;
use std::sync::Arc;

/// A half-open byte range into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The span covering both ends, for reporting about a whole construct.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }
}

/// The text a diagnostic is about, kept alive so rendering needs no lookup.
///
/// Cloning a `Source` is a refcount bump, so every diagnostic can own one.
#[derive(Debug, Clone)]
pub struct Source {
    pub name: Arc<str>,
    pub text: Arc<str>,
}

impl Source {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Source { name: Arc::from(name.into()), text: Arc::from(text.into()) }
    }

    /// 1-based line and column (in characters) of a byte offset, plus the
    /// line's own byte range.
    fn locate(&self, offset: usize) -> Location {
        let offset = offset.min(self.text.len());
        let before = &self.text[..offset];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let line_end =
            self.text[line_start..].find('\n').map_or(self.text.len(), |i| line_start + i);
        let column = self.text[line_start..offset].chars().count() + 1;
        Location { line, column, line_start, line_end }
    }
}

struct Location {
    line: usize,
    column: usize,
    line_start: usize,
    line_end: usize,
}

/// One parse or validation failure, renderable on its own.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
    /// The short note printed under the caret, saying what was expected here.
    pub label: Option<String>,
    /// A longer suggestion printed after the snippet.
    pub help: Option<String>,
    pub source: Source,
}

impl Diagnostic {
    pub fn new(source: &Source, span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            message: message.into(),
            span,
            label: None,
            help: None,
            source: source.clone(),
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// 1-based line number of the span's start.
    pub fn line(&self) -> usize {
        self.source.locate(self.span.start).line
    }

    /// 1-based column, in characters, of the span's start.
    pub fn column(&self) -> usize {
        self.source.locate(self.span.start).column
    }
}

impl fmt::Display for Diagnostic {
    /// ```text
    /// error: unknown disposition `nuke`
    ///   --> rules/bad.toml:4:16
    ///    |
    ///  4 | disposition = "nuke"
    ///    |                ^^^^ expected `keep`, `reclaimable` or `review`
    ///    |
    ///    = help: ...
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = self.source.locate(self.span.start);
        let line_text = &self.source.text[at.line_start..at.line_end];
        let gutter = " ".repeat(at.line.to_string().len() + 1);

        writeln!(f, "error: {}", self.message)?;
        writeln!(f, "{gutter}--> {}:{}:{}", self.source.name, at.line, at.column)?;
        writeln!(f, "{gutter} |")?;
        writeln!(f, " {} | {}", at.line, line_text)?;

        // The caret sits under the span. Columns are counted in characters so a
        // non-ASCII name upstream of the error does not push the caret off.
        let pad = " ".repeat(at.column - 1);
        let end = self.span.end.clamp(self.span.start, at.line_end);
        let width = self.source.text[self.span.start..end].chars().count().max(1);
        let caret = "^".repeat(width);
        match &self.label {
            Some(label) => writeln!(f, "{gutter} | {pad}{caret} {label}")?,
            None => writeln!(f, "{gutter} | {pad}{caret}")?,
        }

        if let Some(help) = &self.help {
            writeln!(f, "{gutter} |")?;
            writeln!(f, "{gutter} = help: {help}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

pub type Result<T> = std::result::Result<T, Diagnostic>;
