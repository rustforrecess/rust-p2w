//! Compile errors: a friendly student-facing message plus the source line it
//! came from. Every stage (lexer, parser, codegen) produces these; the public
//! string API formats them as `line N: message`.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// 1-based source line, when known.
    pub line: Option<usize>,
    pub message: String,
}

impl CompileError {
    pub fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }

    /// An error with no specific location (rare — prefer `at`).
    pub fn general(message: impl Into<String>) -> Self {
        Self {
            line: None,
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
