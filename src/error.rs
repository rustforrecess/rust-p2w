//! Compile errors: a friendly student-facing message plus the source line it
//! came from. Every stage (lexer, parser, codegen) produces these; the public
//! string API formats them as `line N: message`.

use std::fmt;

/// What KIND of mistake this is, in Python's own vocabulary (SyntaxError,
/// NameError, TypeError) so the label a student learns here transfers straight
/// to real CPython. Also the stable class an assessment stream can key on
/// (repeated-error-class metrics — Jadud's Error Quotient — need exactly this
/// tag). The surface message and the kind are deliberately separate axes: the
/// message says what to fix; the kind says what FAMILY of mistake it was.
/// Python's IndentationError is a SyntaxError subclass, so indentation
/// problems classify as `Syntax` here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorKind {
    /// The text doesn't parse (including indentation problems).
    #[default]
    Syntax,
    /// A name is used that isn't defined anywhere (including call-name typos).
    Name,
    /// An operation applied to a value of the wrong type.
    Type,
}

impl ErrorKind {
    /// The student-facing headline, matching Python's exception names.
    pub fn headline(self) -> &'static str {
        match self {
            ErrorKind::Syntax => "Syntax Error",
            ErrorKind::Name => "Name Error",
            ErrorKind::Type => "Type Error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// 1-based source line, when known.
    pub line: Option<usize>,
    /// Byte range `[start, end)` of the offending source text, when known — lets
    /// the editor underline the exact span (a squiggle) rather than the whole
    /// line. `None` falls back to line-level highlighting.
    pub span: Option<(usize, usize)>,
    pub message: String,
    /// The error's family (see [`ErrorKind`]); `Syntax` unless a site says
    /// otherwise via [`CompileError::with_kind`].
    pub kind: ErrorKind,
}

impl CompileError {
    pub fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: None,
            message: message.into(),
            kind: ErrorKind::Syntax,
        }
    }

    /// Like [`at`], but also carries the byte span of the offending text so the
    /// editor can underline exactly that range.
    pub fn at_span(line: usize, span: (usize, usize), message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: Some(span),
            message: message.into(),
            kind: ErrorKind::Syntax,
        }
    }

    /// An error with no specific location (rare — prefer `at`).
    pub fn general(message: impl Into<String>) -> Self {
        Self {
            line: None,
            span: None,
            message: message.into(),
            kind: ErrorKind::Syntax,
        }
    }

    /// Classify this error (chainable): `CompileError::at(...).with_kind(Name)`.
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for CompileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_headline_in_pythons_vocabulary() {
        // The label a student learns must transfer to real CPython.
        assert_eq!(ErrorKind::Syntax.headline(), "Syntax Error");
        assert_eq!(ErrorKind::Name.headline(), "Name Error");
        assert_eq!(ErrorKind::Type.headline(), "Type Error");
        // Unclassified errors default to Syntax; with_kind reclassifies.
        assert_eq!(CompileError::at(1, "x").kind, ErrorKind::Syntax);
        assert_eq!(
            CompileError::at(1, "x").with_kind(ErrorKind::Name).kind,
            ErrorKind::Name
        );
    }
}
