//! Tokenizer for the p2w Python subset.
//!
//! Design follows Ruff's Python lexer: tokens carry source position, newlines
//! are significant (statement terminators) and suppressed inside brackets, and
//! indentation is turned into INDENT/DEDENT tokens via an indentation stack
//! (the CPython approach). Blank and comment-only lines don't affect
//! indentation.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    Str(String),
    Name(String),

    // Arithmetic
    Plus,
    Minus,
    Star,
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
    Colon,
    LParen,
    RParen,
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

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
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
                out.push(Token { tok: Tok::Indent, line });
            } else if col < top {
                while col < *indent_stack.last().unwrap() {
                    indent_stack.pop();
                    out.push(Token { tok: Tok::Dedent, line });
                }
                if col != *indent_stack.last().unwrap() {
                    return Err(format!("line {line}: inconsistent indentation"));
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
                        out.push(Token { tok: Tok::Newline, line });
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
                out.push(Token { tok: Tok::Plus, line });
                i += 1;
            }
            '-' => {
                out.push(Token { tok: Tok::Minus, line });
                i += 1;
            }
            '*' => {
                out.push(Token { tok: Tok::Star, line });
                i += 1;
            }
            '/' => {
                if i + 1 < chars.len() && chars[i + 1] == '/' {
                    out.push(Token { tok: Tok::SlashSlash, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Slash, line });
                    i += 1;
                }
            }
            '%' => {
                out.push(Token { tok: Tok::Percent, line });
                i += 1;
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
                    out.push(Token { tok: Tok::EqEq, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Eq, line });
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    out.push(Token { tok: Tok::BangEq, line });
                    i += 2;
                } else {
                    return Err(format!("line {line}: use 'not' instead of '!'"));
                }
            }
            ':' => {
                out.push(Token { tok: Tok::Colon, line });
                i += 1;
            }
            '(' => {
                paren_depth += 1;
                out.push(Token { tok: Tok::LParen, line });
                i += 1;
            }
            ')' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(format!("line {line}: unmatched ')'"));
                }
                out.push(Token { tok: Tok::RParen, line });
                i += 1;
            }
            ',' => {
                out.push(Token { tok: Tok::Comma, line });
                i += 1;
            }
            '"' | '\'' => {
                let (s, ni) = lex_string(&chars, i, line)?;
                out.push(Token { tok: Tok::Str(s), line });
                i = ni;
            }
            c if c.is_ascii_digit() => {
                let (n, ni) = lex_number(&chars, i, line)?;
                out.push(Token { tok: Tok::Int(n), line });
                i = ni;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                out.push(Token { tok: Tok::Name(name), line });
            }
            other => {
                return Err(format!("line {line}: unexpected character '{other}'"));
            }
        }
    }

    if paren_depth != 0 {
        return Err(format!("line {line}: unclosed '('"));
    }

    // Terminate the final line, then unwind any open indentation.
    if !matches!(out.last().map(|t| &t.tok), None | Some(Tok::Newline)) {
        out.push(Token { tok: Tok::Newline, line });
    }
    while *indent_stack.last().unwrap() > 0 {
        indent_stack.pop();
        out.push(Token { tok: Tok::Dedent, line });
    }
    out.push(Token { tok: Tok::Eof, line });
    Ok(out)
}

fn lex_number(chars: &[char], start: usize, line: usize) -> Result<(i64, usize), String> {
    let mut i = start;
    let mut digits = String::new();
    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
        if chars[i] != '_' {
            digits.push(chars[i]);
        }
        i += 1;
    }
    if i < chars.len() && chars[i] == '.' {
        return Err(format!(
            "line {line}: floating-point numbers aren't supported yet"
        ));
    }
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("line {line}: invalid integer '{digits}'"))?;
    Ok((n, i))
}

fn lex_string(chars: &[char], start: usize, line: usize) -> Result<(String, usize), String> {
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
    Err(format!("line {line}: unterminated string literal"))
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
    fn float_is_rejected() {
        assert!(lex("3.14").is_err());
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(lex("\"oops").is_err());
    }
}
