//! Compile errors: a friendly student-facing message plus the source line it
//! came from. Every stage (lexer, parser, codegen) produces these; the public
//! string API formats them as `line N: message`.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// 1-based source line, when known.
    pub line: Option<usize>,
    /// Byte range `[start, end)` of the offending source text, when known — lets
    /// the editor underline the exact span (a squiggle) rather than the whole
    /// line. `None` falls back to line-level highlighting.
    pub span: Option<(usize, usize)>,
    pub message: String,
}

impl CompileError {
    pub fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: None,
            message: message.into(),
        }
    }

    /// Like [`at`], but also carries the byte span of the offending text so the
    /// editor can underline exactly that range.
    pub fn at_span(line: usize, span: (usize, usize), message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: Some(span),
            message: message.into(),
        }
    }

    /// An error with no specific location (rare — prefer `at`).
    pub fn general(message: impl Into<String>) -> Self {
        Self {
            line: None,
            span: None,
            message: message.into(),
        }
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
