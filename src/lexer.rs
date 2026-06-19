//! Tokenizer for the p2w Python subset.
//!
//! Design follows Ruff's Python lexer: tokens carry source position, newlines
//! are significant (statement terminators) and suppressed inside brackets, and
//! indentation is turned into INDENT/DEDENT tokens via an indentation stack
//! (the CPython approach). Blank and comment-only lines don't affect
//! indentation.

use crate::ast::BinOp;
use crate::error::CompileError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    /// An f-string, split into parts: (is_expression, text, format_spec).
    /// Literal parts are ready text; expression parts are raw source the parser
    /// re-parses, with an optional format spec (the part after `:`, empty if
    /// none).
    FStr(Vec<(bool, String, String)>),
    Name(String),

    // Arithmetic
    Plus,
    Minus,
    Star,
    DoubleStar,
    Slash,
    SlashSlash,
    Percent,

    // Comparison
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    BangEq,

    // Punctuation
    Eq,
    /// Augmented assignment: `+=`, `-=`, `*=`, `/=`, `//=`, `%=`.
    AugAssign(BinOp),
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Dot,
    Comma,

    // Layout
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, CompileError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut paren_depth: i32 = 0;
    let mut indent_stack: Vec<usize> = vec![0];
    let mut at_line_start = true;

    while i < chars.len() {
        // --- Indentation handling at the start of a logical line ---
        if at_line_start && paren_depth == 0 {
            let mut col = 0usize;
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                col += 1;
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            // Blank line — no indent effect.
            if chars[i] == '\n' {
                i += 1;
                line += 1;
                continue;
            }
            if chars[i] == '\r' {
                i += 1;
                continue;
            }
            // Comment-only line — no indent effect.
            if chars[i] == '#' {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // Real content: reconcile against the indentation stack.
            let top = *indent_stack.last().unwrap();
            if col > top {
                indent_stack.push(col);
                out.push(Token {
                    tok: Tok::Indent,
                    line,
                });
            } else if col < top {
                while col < *indent_stack.last().unwrap() {
                    indent_stack.pop();
                    out.push(Token {
                        tok: Tok::Dedent,
                        line,
                    });
                }
                if col != *indent_stack.last().unwrap() {
                    return Err(CompileError::at(line, "inconsistent indentation"));
                }
            }
            at_line_start = false;
        }

        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                i += 1;
            }
            // Line continuation: `\` before a line ending (LF or CRLF).
            '\\' if chars.get(i + 1) == Some(&'\n')
                || (chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n')) =>
            {
                i += if chars[i + 1] == '\r' { 3 } else { 2 };
                line += 1;
            }
            '\n' => {
                i += 1;
                if paren_depth == 0 {
                    // Emit NEWLINE only after content on the line.
                    if !matches!(out.last().map(|t| &t.tok), None | Some(Tok::Newline)) {
                        out.push(Token {
                            tok: Tok::Newline,
                            line,
                        });
                    }
                    at_line_start = true;
                }
                line += 1;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '+' => {
                let (tok, w) = aug_or(&chars, i, Tok::Plus, BinOp::Add);
                out.push(Token { tok, line });
                i += w;
            }
            '-' => {
                let (tok, w) = aug_or(&chars, i, Tok::Minus, BinOp::Sub);
                out.push(Token { tok, line });
                i += w;
            }
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // `**=` (3 chars) before `**`.
                    if i + 2 < chars.len() && chars[i + 2] == '=' {
                        out.push(Token {
                            tok: Tok::AugAssign(BinOp::Pow),
                            line,
                        });
                        i += 3;
                    } else {
                        out.push(Token {
                            tok: Tok::DoubleStar,
                            line,
                        });
                        i += 2;
                    }
                } else {
                    let (tok, w) = aug_or(&chars, i, Tok::Star, BinOp::Mul);
                    out.push(Token { tok, line });
                    i += w;
                }
            }
            '/' => {
                if i + 1 < chars.len() && chars[i + 1] == '/' {
                    // `//=` (3 chars) before `//`.
                    if i + 2 < chars.len() && chars[i + 2] == '=' {
                        out.push(Token {
                            tok: Tok::AugAssign(BinOp::FloorDiv),
                            line,
                        });
                        i += 3;
                    } else {
                        out.push(Token {
                            tok: Tok::SlashSlash,
                            line,
                        });
                        i += 2;
                    }
                } else {
                    let (tok, w) = aug_or(&chars, i, Tok::Slash, BinOp::Div);
                    out.push(Token { tok, line });
                    i += w;
                }
            }
            '%' => {
                let (tok, w) = aug_or(&chars, i, Tok::Percent, BinOp::Mod);
                out.push(Token { tok, line });
                i += w;
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Token { tok: Tok::Le, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Lt, line });
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Token { tok: Tok::Ge, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Gt, line });
                    i += 1;
                }
            }
            '=' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Token {
                        tok: Tok::EqEq,
                        line,
                    });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Eq, line });
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Token {
                        tok: Tok::BangEq,
                        line,
                    });
                    i += 2;
                } else {
                    return Err(CompileError::at(line, "use 'not' instead of '!'"));
                }
            }
            ':' => {
                out.push(Token {
                    tok: Tok::Colon,
                    line,
                });
                i += 1;
            }
            '(' => {
                paren_depth += 1;
                out.push(Token {
                    tok: Tok::LParen,
                    line,
                });
                i += 1;
            }
            ')' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(CompileError::at(line, "unmatched ')'"));
                }
                out.push(Token {
                    tok: Tok::RParen,
                    line,
                });
                i += 1;
            }
            // Brackets suppress newlines exactly like parens (Python allows
            // multi-line list literals).
            '[' => {
                paren_depth += 1;
                out.push(Token {
                    tok: Tok::LBracket,
                    line,
                });
                i += 1;
            }
            ']' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(CompileError::at(line, "unmatched ']'"));
                }
                out.push(Token {
                    tok: Tok::RBracket,
                    line,
                });
                i += 1;
            }
            '.' => {
                out.push(Token {
                    tok: Tok::Dot,
                    line,
                });
                i += 1;
            }
            '{' => {
                paren_depth += 1;
                out.push(Token {
                    tok: Tok::LBrace,
                    line,
                });
                i += 1;
            }
            '}' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(CompileError::at(line, "unmatched '}'"));
                }
                out.push(Token {
                    tok: Tok::RBrace,
                    line,
                });
                i += 1;
            }
            ',' => {
                out.push(Token {
                    tok: Tok::Comma,
                    line,
                });
                i += 1;
            }
            '"' | '\'' => {
                let (s, ni) = lex_string(&chars, i, line)?;
                out.push(Token {
                    tok: Tok::Str(s),
                    line,
                });
                i = ni;
            }
            c if c.is_ascii_digit() => {
                let (tok, ni) = lex_number(&chars, i, line)?;
                out.push(Token { tok, line });
                i = ni;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                // f"..." / f'...' is an f-string, not the name `f`.
                if (name == "f" || name == "F")
                    && i < chars.len()
                    && (chars[i] == '"' || chars[i] == '\'')
                {
                    let (parts, ni) = lex_fstring(&chars, i, line)?;
                    out.push(Token {
                        tok: Tok::FStr(parts),
                        line,
                    });
                    i = ni;
                    continue;
                }
                out.push(Token {
                    tok: Tok::Name(name),
                    line,
                });
            }
            other => {
                return Err(CompileError::at(
                    line,
                    format!("unexpected character '{other}'"),
                ));
            }
        }
    }

    if paren_depth != 0 {
        return Err(CompileError::at(line, "unclosed '('"));
    }

    // Terminate the final line, then unwind any open indentation.
    if !matches!(out.last().map(|t| &t.tok), None | Some(Tok::Newline)) {
        out.push(Token {
            tok: Tok::Newline,
            line,
        });
    }
    while *indent_stack.last().unwrap() > 0 {
        indent_stack.pop();
        out.push(Token {
            tok: Tok::Dedent,
            line,
        });
    }
    out.push(Token {
        tok: Tok::Eof,
        line,
    });
    Ok(out)
}

/// A single-char operator at `i`: `AugAssign(op)` (width 2) if followed by
/// `=`, otherwise `plain` (width 1).
fn aug_or(chars: &[char], i: usize, plain: Tok, op: BinOp) -> (Tok, usize) {
    if i + 1 < chars.len() && chars[i + 1] == '=' {
        (Tok::AugAssign(op), 2)
    } else {
        (plain, 1)
    }
}

fn lex_number(chars: &[char], start: usize, line: usize) -> Result<(Tok, usize), CompileError> {
    let mut i = start;
    let mut digits = String::new();
    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
        if chars[i] != '_' {
            digits.push(chars[i]);
        }
        i += 1;
    }
    // A '.' makes it a float literal: `3.14`, and Python's `3.` too.
    if i < chars.len() && chars[i] == '.' {
        digits.push('.');
        i += 1;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
            if chars[i] != '_' {
                digits.push(chars[i]);
            }
            i += 1;
        }
        let f: f64 = digits
            .parse()
            .map_err(|_| CompileError::at(line, format!("invalid number '{digits}'")))?;
        return Ok((Tok::Float(f), i));
    }
    let n: i64 = digits
        .parse()
        .map_err(|_| CompileError::at(line, format!("invalid integer '{digits}'")))?;
    Ok((Tok::Int(n), i))
}

/// Lex `f"...{expr[:spec]}..."` into (is_expression, text, spec) parts.
/// `{{`/`}}` are literal braces. Bracket depth is tracked so a slice colon
/// (`{xs[1:3]}`) and nested braces (`{ {1:2} }`) aren't mistaken for a spec.
///
/// Each part is `(is_expression, text, format_spec)`.
type FStrParts = Vec<(bool, String, String)>;

fn lex_fstring(
    chars: &[char],
    start: usize,
    line: usize,
) -> Result<(FStrParts, usize), CompileError> {
    let quote = chars[start];
    let mut i = start + 1;
    let mut parts: Vec<(bool, String, String)> = Vec::new();
    let mut lit = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == quote {
            if !lit.is_empty() {
                parts.push((false, lit, String::new()));
            }
            return Ok((parts, i + 1));
        }
        if c == '\n' {
            break;
        }
        if c == '{' {
            if chars.get(i + 1) == Some(&'{') {
                lit.push('{');
                i += 2;
                continue;
            }
            if !lit.is_empty() {
                parts.push((false, std::mem::take(&mut lit), String::new()));
            }
            let expr_start = i + 1;
            let mut j = expr_start;
            let mut depth = 0i32;
            let mut colon: Option<usize> = None;
            while j < chars.len() && chars[j] != quote && chars[j] != '\n' {
                match chars[j] {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' => depth -= 1,
                    '}' if depth == 0 => break,
                    '}' => depth -= 1,
                    ':' if depth == 0 && colon.is_none() => colon = Some(j),
                    _ => {}
                }
                j += 1;
            }
            if j >= chars.len() || chars[j] != '}' {
                return Err(CompileError::at(line, "missing '}' in f-string"));
            }
            let end = colon.unwrap_or(j);
            let src: String = chars[expr_start..end].iter().collect();
            if src.trim().is_empty() {
                return Err(CompileError::at(line, "empty expression in f-string"));
            }
            let spec: String = match colon {
                Some(ci) => chars[ci + 1..j].iter().collect(),
                None => String::new(),
            };
            parts.push((true, src, spec));
            i = j + 1;
            continue;
        }
        if c == '}' {
            if chars.get(i + 1) == Some(&'}') {
                lit.push('}');
                i += 2;
                continue;
            }
            return Err(CompileError::at(line, "single '}' in f-string — use '}}'"));
        }
        if c == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            lit.push(match e {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            });
            i += 2;
            continue;
        }
        lit.push(c);
        i += 1;
    }
    Err(CompileError::at(line, "unterminated f-string"))
}

fn lex_string(chars: &[char], start: usize, line: usize) -> Result<(String, usize), CompileError> {
    let quote = chars[start];
    let mut i = start + 1;
    let mut s = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            s.push(match e {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            });
            i += 2;
            continue;
        }
        if c == quote {
            return Ok((s, i + 1));
        }
        if c == '\n' {
            break;
        }
        s.push(c);
        i += 1;
    }
    Err(CompileError::at(line, "unterminated string literal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn numbers_and_ops() {
        assert_eq!(
            toks("2 + 3 * 4"),
            vec![
                Tok::Int(2),
                Tok::Plus,
                Tok::Int(3),
                Tok::Star,
                Tok::Int(4),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn newline_suppressed_in_parens() {
        assert_eq!(
            toks("print(\n1\n)"),
            vec![
                Tok::Name("print".into()),
                Tok::LParen,
                Tok::Int(1),
                Tok::RParen,
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn indentation_tokens() {
        // a / indented b / back to c
        assert_eq!(
            toks("a\n    b\nc"),
            vec![
                Tok::Name("a".into()),
                Tok::Newline,
                Tok::Indent,
                Tok::Name("b".into()),
                Tok::Newline,
                Tok::Dedent,
                Tok::Name("c".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn blank_and_comment_lines_ignored_for_indent() {
        // Blank line and comment between two top-level statements must not
        // produce INDENT/DEDENT.
        let t = toks("a\n\n   # note\nb");
        assert_eq!(
            t,
            vec![
                Tok::Name("a".into()),
                Tok::Newline,
                Tok::Name("b".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(
            toks("a <= b == c != d >= e"),
            vec![
                Tok::Name("a".into()),
                Tok::Le,
                Tok::Name("b".into()),
                Tok::EqEq,
                Tok::Name("c".into()),
                Tok::BangEq,
                Tok::Name("d".into()),
                Tok::Ge,
                Tok::Name("e".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn float_literals_lex() {
        assert_eq!(
            toks("x = 2.75"),
            vec![
                Tok::Name("x".into()),
                Tok::Eq,
                Tok::Float(2.75),
                Tok::Newline,
                Tok::Eof
            ]
        );
        // Python's trailing-dot form.
        assert_eq!(toks("3.")[0], Tok::Float(3.0));
        assert_eq!(toks("1_000.5")[0], Tok::Float(1000.5));
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(lex("\"oops").is_err());
    }
}
