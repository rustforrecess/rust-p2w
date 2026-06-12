//! Recursive-descent parser with Pratt (precedence-climbing) expression
//! parsing — the structure Ruff's Python parser uses. Compound statements
//! (`if`, `for`) consume indented blocks delimited by INDENT/DEDENT; simple
//! statements consume their trailing NEWLINE.

use crate::ast::{BinOp, Expr, Stmt, UnOp};
use crate::lexer::{Tok, Token};

pub fn parse(tokens: &[Token]) -> Result<Vec<Stmt>, String> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
    };
    p.program()
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek2(&self) -> &Tok {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> usize {
        self.toks[self.pos].line
    }

    fn advance(&mut self) -> &Tok {
        let t = &self.toks[self.pos].tok;
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Result<(), String> {
        if self.peek() == want {
            self.advance();
            Ok(())
        } else {
            Err(format!(
                "line {}: expected {what}, found {:?}",
                self.line(),
                self.peek()
            ))
        }
    }

    fn is_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == kw)
    }

    fn eat_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.is_keyword(kw) {
            self.advance();
            Ok(())
        } else {
            Err(format!("line {}: expected '{kw}'", self.line()))
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        loop {
            while matches!(self.peek(), Tok::Newline) {
                self.advance();
            }
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            stmts.push(self.statement()?);
        }
        Ok(stmts)
    }

    /// An indented block: INDENT, one or more statements, DEDENT.
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&Tok::Indent, "an indented block")?;
        let mut stmts = Vec::new();
        loop {
            while matches!(self.peek(), Tok::Newline) {
                self.advance();
            }
            if matches!(self.peek(), Tok::Dedent | Tok::Eof) {
                break;
            }
            stmts.push(self.statement()?);
        }
        self.expect(&Tok::Dedent, "the end of the indented block")?;
        if stmts.is_empty() {
            return Err(format!("line {}: this block is empty", self.line()));
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if self.is_keyword("if") {
            return self.if_stmt();
        }
        if self.is_keyword("for") {
            return self.for_stmt();
        }
        if self.is_keyword("while") {
            return self.while_stmt();
        }
        if self.is_keyword("break") {
            self.advance();
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt::Break);
        }
        if self.is_keyword("continue") {
            self.advance();
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt::Continue);
        }
        // Assignment: `name = expr`.
        if let Tok::Name(name) = self.peek().clone() {
            if *self.peek2() == Tok::Eq {
                self.advance(); // name
                self.advance(); // '='
                let value = self.expr(0)?;
                self.expect(&Tok::Newline, "a new line")?;
                return Ok(Stmt::Assign(name, value));
            }
        }
        // Expression statement.
        let e = self.expr(0)?;
        self.expect(&Tok::Newline, "a new line")?;
        Ok(Stmt::Expr(e))
    }

    fn if_stmt(&mut self) -> Result<Stmt, String> {
        self.eat_keyword("if")?;
        let cond = self.expr(0)?;
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;

        let mut elifs = Vec::new();
        let mut else_body = None;
        loop {
            if self.is_keyword("elif") {
                self.advance();
                let c = self.expr(0)?;
                self.expect(&Tok::Colon, "':'")?;
                self.expect(&Tok::Newline, "a new line")?;
                let b = self.block()?;
                elifs.push((c, b));
            } else if self.is_keyword("else") {
                self.advance();
                self.expect(&Tok::Colon, "':'")?;
                self.expect(&Tok::Newline, "a new line")?;
                else_body = Some(self.block()?);
                break;
            } else {
                break;
            }
        }
        Ok(Stmt::If {
            cond,
            body,
            elifs,
            else_body,
        })
    }

    fn while_stmt(&mut self) -> Result<Stmt, String> {
        self.eat_keyword("while")?;
        let cond = self.expr(0)?;
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt::While { cond, body })
    }

    fn for_stmt(&mut self) -> Result<Stmt, String> {
        self.eat_keyword("for")?;
        let var = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(format!(
                    "line {}: expected a loop variable, found {:?}",
                    self.line(),
                    other
                ))
            }
        };
        self.eat_keyword("in")?;
        self.eat_keyword("range")?;
        self.expect(&Tok::LParen, "'(' after range")?;
        let args = self.call_args()?;
        let (start, end, step) = match args.len() {
            1 => (Expr::Int(0), args[0].clone(), Expr::Int(1)),
            2 => (args[0].clone(), args[1].clone(), Expr::Int(1)),
            3 => (args[0].clone(), args[1].clone(), args[2].clone()),
            n => {
                return Err(format!(
                    "line {}: range() takes 1 to 3 arguments, got {n}",
                    self.line()
                ))
            }
        };
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt::For {
            var,
            start,
            end,
            step,
            body,
        })
    }

    /// Pratt expression parser.
    fn expr(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.prefix()?;
        loop {
            let Some((op, l_bp, r_bp)) = self.peek_infix() else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.expr(r_bp)?;
            if is_comparison(op) {
                lhs = self.comparison_chain(lhs, op, rhs)?;
            } else {
                lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
            }
        }
        Ok(lhs)
    }

    /// Python chains comparisons: `a < b < c` means `a < b and b < c` (each
    /// middle operand evaluated once — safe to duplicate here because the
    /// supported expressions have no side effects).
    fn comparison_chain(&mut self, lhs: Expr, op: BinOp, rhs: Expr) -> Result<Expr, String> {
        let mut chain = Expr::Bin(op, Box::new(lhs), Box::new(rhs.clone()));
        let mut prev = rhs;
        while let Some((next_op, _, r_bp)) = self.peek_infix() {
            if !is_comparison(next_op) {
                break;
            }
            self.advance();
            let next = self.expr(r_bp)?;
            let pair = Expr::Bin(next_op, Box::new(prev), Box::new(next.clone()));
            chain = Expr::Bin(BinOp::And, Box::new(chain), Box::new(pair));
            prev = next;
        }
        Ok(chain)
    }

    fn prefix(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Tok::Minus) {
            self.advance();
            let operand = self.expr(PREFIX_BP)?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(operand)));
        }
        if self.is_keyword("not") {
            self.advance();
            // `not` binds looser than comparisons but tighter than and/or.
            let operand = self.expr(7)?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(operand)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Int(n))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::LParen => {
                self.advance();
                let e = self.expr(0)?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Name(name) => {
                self.advance();
                match name.as_str() {
                    "True" => Ok(Expr::Int(1)),
                    "False" => Ok(Expr::Int(0)),
                    _ if matches!(self.peek(), Tok::LParen) => {
                        self.advance();
                        let args = self.call_args()?;
                        Ok(Expr::Call(name, args))
                    }
                    _ => Ok(Expr::Name(name)),
                }
            }
            other => Err(format!(
                "line {}: expected a value, found {:?}",
                self.line(),
                other
            )),
        }
    }

    fn call_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if matches!(self.peek(), Tok::RParen) {
            self.advance();
            return Ok(args);
        }
        loop {
            args.push(self.expr(0)?);
            match self.peek() {
                Tok::Comma => {
                    self.advance();
                    if matches!(self.peek(), Tok::RParen) {
                        break;
                    }
                }
                Tok::RParen => break,
                other => {
                    return Err(format!(
                        "line {}: expected ',' or ')' in call, found {:?}",
                        self.line(),
                        other
                    ));
                }
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(args)
    }

    /// Infix operator at the cursor, as `(op, left_bp, right_bp)`. Handles the
    /// `and` / `or` keyword operators alongside symbolic ones.
    fn peek_infix(&self) -> Option<(BinOp, u8, u8)> {
        let (op, bp) = match self.peek() {
            Tok::Name(n) if n == "or" => (BinOp::Or, 3),
            Tok::Name(n) if n == "and" => (BinOp::And, 4),
            Tok::Lt => (BinOp::Lt, 7),
            Tok::Le => (BinOp::Le, 7),
            Tok::Gt => (BinOp::Gt, 7),
            Tok::Ge => (BinOp::Ge, 7),
            Tok::EqEq => (BinOp::Eq, 7),
            Tok::BangEq => (BinOp::Ne, 7),
            Tok::Plus => (BinOp::Add, 10),
            Tok::Minus => (BinOp::Sub, 10),
            Tok::Star => (BinOp::Mul, 20),
            Tok::Slash => (BinOp::Div, 20),
            Tok::SlashSlash => (BinOp::FloorDiv, 20),
            Tok::Percent => (BinOp::Mod, 20),
            _ => return None,
        };
        Some((op, bp, bp + 1)) // left-associative
    }
}

/// Unary prefix binding power — higher than every binary operator.
const PREFIX_BP: u8 = 100;

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> Result<Vec<Stmt>, String> {
        parse(&lex(src).unwrap())
    }

    fn one_expr(src: &str) -> Expr {
        match parse_src(src).unwrap().pop().unwrap() {
            Stmt::Expr(e) => e,
            other => panic!("expected expression statement, got {other:?}"),
        }
    }

    #[test]
    fn precedence_mul_over_add() {
        let e = one_expr("2 + 3 * 4");
        assert_eq!(
            e,
            Expr::Bin(
                BinOp::Add,
                Box::new(Expr::Int(2)),
                Box::new(Expr::Bin(
                    BinOp::Mul,
                    Box::new(Expr::Int(3)),
                    Box::new(Expr::Int(4))
                ))
            )
        );
    }

    #[test]
    fn comparison_binds_looser_than_arithmetic() {
        // x + 1 < 10  ==>  Lt(Add(x,1), 10)
        let e = one_expr("x + 1 < 10");
        assert_eq!(
            e,
            Expr::Bin(
                BinOp::Lt,
                Box::new(Expr::Bin(
                    BinOp::Add,
                    Box::new(Expr::Name("x".into())),
                    Box::new(Expr::Int(1))
                )),
                Box::new(Expr::Int(10))
            )
        );
    }

    #[test]
    fn and_binds_looser_than_comparison() {
        // a < b and c  ==> And(Lt(a,b), c)
        let e = one_expr("a < b and c");
        assert_eq!(
            e,
            Expr::Bin(
                BinOp::And,
                Box::new(Expr::Bin(
                    BinOp::Lt,
                    Box::new(Expr::Name("a".into())),
                    Box::new(Expr::Name("b".into()))
                )),
                Box::new(Expr::Name("c".into()))
            )
        );
    }

    #[test]
    fn chained_comparison_desugars_to_and() {
        // 1 < 2 < 3  ==>  And(Lt(1,2), Lt(2,3)), like Python — NOT Lt(Lt(1,2), 3)
        let e = one_expr("1 < 2 < 3");
        assert_eq!(
            e,
            Expr::Bin(
                BinOp::And,
                Box::new(Expr::Bin(
                    BinOp::Lt,
                    Box::new(Expr::Int(1)),
                    Box::new(Expr::Int(2))
                )),
                Box::new(Expr::Bin(
                    BinOp::Lt,
                    Box::new(Expr::Int(2)),
                    Box::new(Expr::Int(3))
                ))
            )
        );
    }

    #[test]
    fn parses_assignment() {
        let s = parse_src("x = 2 + 3").unwrap().pop().unwrap();
        assert_eq!(
            s,
            Stmt::Assign(
                "x".into(),
                Expr::Bin(BinOp::Add, Box::new(Expr::Int(2)), Box::new(Expr::Int(3)))
            )
        );
    }

    #[test]
    fn parses_if_else() {
        let src = "if x:\n    print(1)\nelse:\n    print(2)\n";
        let s = parse_src(src).unwrap().pop().unwrap();
        match s {
            Stmt::If {
                body,
                else_body,
                elifs,
                ..
            } => {
                assert_eq!(body.len(), 1);
                assert!(elifs.is_empty());
                assert_eq!(else_body.unwrap().len(), 1);
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn parses_for_range() {
        let src = "for i in range(3):\n    print(i)\n";
        let s = parse_src(src).unwrap().pop().unwrap();
        match s {
            Stmt::For {
                var, start, end, ..
            } => {
                assert_eq!(var, "i");
                assert_eq!(start, Expr::Int(0));
                assert_eq!(end, Expr::Int(3));
            }
            other => panic!("expected for, got {other:?}"),
        }
    }
}
