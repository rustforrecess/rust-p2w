//! Recursive-descent parser with Pratt (precedence-climbing) expression
//! parsing — the structure Ruff's Python parser uses. Compound statements
//! (`if`, `for`, `while`) consume indented blocks delimited by INDENT/DEDENT;
//! simple statements consume their trailing NEWLINE.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::error::CompileError;
use crate::lexer::{Tok, Token};

type Result<T> = std::result::Result<T, CompileError>;

pub fn parse(tokens: &[Token]) -> Result<Vec<Stmt>> {
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

    fn expect(&mut self, want: &Tok, what: &str) -> Result<()> {
        if self.peek() == want {
            self.advance();
            Ok(())
        } else {
            Err(CompileError::at(
                self.line(),
                format!("expected {what}, found {:?}", self.peek()),
            ))
        }
    }

    fn is_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == kw)
    }

    fn eat_keyword(&mut self, kw: &str) -> Result<()> {
        if self.is_keyword(kw) {
            self.advance();
            Ok(())
        } else {
            Err(CompileError::at(self.line(), format!("expected '{kw}'")))
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>> {
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
    fn block(&mut self) -> Result<Vec<Stmt>> {
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
            return Err(CompileError::at(self.line(), "this block is empty"));
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt> {
        let line = self.line();
        if self.is_keyword("if") {
            return self.if_stmt();
        }
        if self.is_keyword("for") {
            return self.for_stmt();
        }
        if self.is_keyword("while") {
            return self.while_stmt();
        }
        if self.is_keyword("def") {
            return self.def_stmt();
        }
        if self.is_keyword("return") {
            self.advance();
            let value = if matches!(self.peek(), Tok::Newline) {
                None
            } else {
                Some(self.expr(0)?)
            };
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt {
                kind: StmtKind::Return(value),
                line,
            });
        }
        if self.is_keyword("break") {
            self.advance();
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt {
                kind: StmtKind::Break,
                line,
            });
        }
        if self.is_keyword("continue") {
            self.advance();
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt {
                kind: StmtKind::Continue,
                line,
            });
        }
        // Assignment or expression statement: parse the expression first,
        // then decide based on what follows.
        let e = self.expr(0)?;
        if matches!(self.peek(), Tok::Eq) {
            self.advance();
            let value = self.expr(0)?;
            self.expect(&Tok::Newline, "a new line")?;
            return match e.kind {
                ExprKind::Name(name) => Ok(Stmt {
                    kind: StmtKind::Assign(name, value),
                    line,
                }),
                ExprKind::Index(target, index) => Ok(Stmt {
                    kind: StmtKind::SetIndex {
                        target: *target,
                        index: *index,
                        value,
                    },
                    line,
                }),
                _ => Err(CompileError::at(
                    line,
                    "can only assign to a variable or an index like xs[i]",
                )),
            };
        }
        self.expect(&Tok::Newline, "a new line")?;
        Ok(Stmt {
            kind: StmtKind::Expr(e),
            line,
        })
    }

    fn if_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
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
        Ok(Stmt {
            kind: StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            },
            line,
        })
    }

    fn def_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        self.eat_keyword("def")?;
        let name = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(CompileError::at(
                    self.line(),
                    format!("expected a function name after 'def', found {other:?}"),
                ))
            }
        };
        self.expect(&Tok::LParen, "'(' after the function name")?;
        let mut params = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                match self.peek().clone() {
                    Tok::Name(p) => {
                        self.advance();
                        if params.contains(&p) {
                            return Err(CompileError::at(
                                self.line(),
                                format!("duplicate parameter name '{p}'"),
                            ));
                        }
                        params.push(p);
                    }
                    other => {
                        return Err(CompileError::at(
                            self.line(),
                            format!("expected a parameter name, found {other:?}"),
                        ))
                    }
                }
                if matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    if matches!(self.peek(), Tok::RParen) {
                        break; // trailing comma
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen, "')'")?;
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt {
            kind: StmtKind::Def { name, params, body },
            line,
        })
    }

    fn while_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        self.eat_keyword("while")?;
        let cond = self.expr(0)?;
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt {
            kind: StmtKind::While { cond, body },
            line,
        })
    }

    fn for_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        self.eat_keyword("for")?;
        let var = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(CompileError::at(
                    self.line(),
                    format!("expected a loop variable, found {other:?}"),
                ))
            }
        };
        self.eat_keyword("in")?;
        // `range(...)` is the counted fast path; any other expression
        // iterates a sequence (list or string).
        if self.is_keyword("range") && *self.peek2() == Tok::LParen {
            self.advance(); // range
            self.advance(); // (
            let args = self.call_args()?;
            let int = |n: i64| Expr {
                kind: ExprKind::Int(n),
                line,
            };
            let (start, end, step) = match args.len() {
                1 => (int(0), args[0].clone(), int(1)),
                2 => (args[0].clone(), args[1].clone(), int(1)),
                3 => (args[0].clone(), args[1].clone(), args[2].clone()),
                n => {
                    return Err(CompileError::at(
                        self.line(),
                        format!("range() takes 1 to 3 arguments, got {n}"),
                    ))
                }
            };
            self.expect(&Tok::Colon, "':'")?;
            self.expect(&Tok::Newline, "a new line")?;
            let body = self.block()?;
            return Ok(Stmt {
                kind: StmtKind::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                },
                line,
            });
        }
        let iterable = self.expr(0)?;
        self.expect(&Tok::Colon, "':'")?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt {
            kind: StmtKind::ForEach {
                var,
                iterable,
                body,
            },
            line,
        })
    }

    /// Pratt expression parser.
    fn expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.prefix()?;
        loop {
            let Some((op, l_bp, r_bp)) = self.peek_infix() else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            let op_line = self.line();
            self.advance();
            if op == BinOp::NotIn {
                self.advance(); // the `in` of `not in`
            }
            let rhs = self.expr(r_bp)?;
            if is_comparison(op) {
                lhs = self.comparison_chain(lhs, op, rhs, op_line)?;
            } else {
                lhs = Expr {
                    kind: ExprKind::Bin(op, Box::new(lhs), Box::new(rhs)),
                    line: op_line,
                };
            }
        }
        Ok(lhs)
    }

    /// Python chains comparisons: `a < b < c` means `a < b and b < c` (each
    /// middle operand evaluated once — safe to duplicate here because the
    /// supported expressions have no side effects).
    fn comparison_chain(&mut self, lhs: Expr, op: BinOp, rhs: Expr, line: usize) -> Result<Expr> {
        let mut chain = Expr {
            kind: ExprKind::Bin(op, Box::new(lhs), Box::new(rhs.clone())),
            line,
        };
        let mut prev = rhs;
        while let Some((next_op, _, r_bp)) = self.peek_infix() {
            if !is_comparison(next_op) {
                break;
            }
            let op_line = self.line();
            // The middle operand is cloned into both pairwise comparisons; a
            // function call there would run twice, so refuse it.
            if contains_call(&prev) {
                return Err(CompileError::at(
                    op_line,
                    "chained comparisons around a function call aren't supported — \
                     store the call's result in a variable first",
                ));
            }
            self.advance();
            let next = self.expr(r_bp)?;
            let pair = Expr {
                kind: ExprKind::Bin(next_op, Box::new(prev), Box::new(next.clone())),
                line: op_line,
            };
            chain = Expr {
                kind: ExprKind::Bin(BinOp::And, Box::new(chain), Box::new(pair)),
                line: op_line,
            };
            prev = next;
        }
        Ok(chain)
    }

    fn prefix(&mut self) -> Result<Expr> {
        let line = self.line();
        if matches!(self.peek(), Tok::Minus) {
            self.advance();
            let operand = self.expr(PREFIX_BP)?;
            return Ok(Expr {
                kind: ExprKind::Unary(UnOp::Neg, Box::new(operand)),
                line,
            });
        }
        if self.is_keyword("not") {
            self.advance();
            // `not` binds looser than comparisons but tighter than and/or.
            let operand = self.expr(7)?;
            return Ok(Expr {
                kind: ExprKind::Unary(UnOp::Not, Box::new(operand)),
                line,
            });
        }
        let atom = self.primary()?;
        self.postfix(atom)
    }

    /// Postfix operators: `xs[i]` subscripts and `xs.method(args)` calls,
    /// chaining left to right (`grid[0][1]`, `xs.append(v)`).
    fn postfix(&mut self, mut e: Expr) -> Result<Expr> {
        loop {
            match self.peek() {
                Tok::LBracket => {
                    let line = self.line();
                    self.advance();
                    let index = self.expr(0)?;
                    self.expect(&Tok::RBracket, "']'")?;
                    e = Expr {
                        kind: ExprKind::Index(Box::new(e), Box::new(index)),
                        line,
                    };
                }
                Tok::Dot => {
                    let line = self.line();
                    self.advance();
                    let method = match self.peek().clone() {
                        Tok::Name(m) => {
                            self.advance();
                            m
                        }
                        other => {
                            return Err(CompileError::at(
                                self.line(),
                                format!("expected a method name after '.', found {other:?}"),
                            ))
                        }
                    };
                    self.expect(
                        &Tok::LParen,
                        "'(' (only method calls are supported after '.')",
                    )?;
                    let args = self.call_args()?;
                    e = Expr {
                        kind: ExprKind::MethodCall(Box::new(e), method, args),
                        line,
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let line = self.line();
        let expr = |kind| Expr { kind, line };
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(expr(ExprKind::Int(n)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(expr(ExprKind::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(expr(ExprKind::Str(s)))
            }
            Tok::LParen => {
                self.advance();
                let e = self.expr(0)?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::LBracket => {
                self.advance();
                let mut elements = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        elements.push(self.expr(0)?);
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBracket) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBracket, "']'")?;
                Ok(expr(ExprKind::List(elements)))
            }
            Tok::LBrace => {
                self.advance();
                let mut entries = Vec::new();
                if !matches!(self.peek(), Tok::RBrace) {
                    loop {
                        let key = self.expr(0)?;
                        self.expect(&Tok::Colon, "':' between a dict key and its value")?;
                        let value = self.expr(0)?;
                        entries.push((key, value));
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBrace, "'}'")?;
                Ok(expr(ExprKind::Dict(entries)))
            }
            Tok::Name(name) => {
                self.advance();
                match name.as_str() {
                    "True" => Ok(expr(ExprKind::Bool(true))),
                    "False" => Ok(expr(ExprKind::Bool(false))),
                    "None" => Ok(expr(ExprKind::NoneLit)),
                    _ if matches!(self.peek(), Tok::LParen) => {
                        self.advance();
                        let args = self.call_args()?;
                        Ok(expr(ExprKind::Call(name, args)))
                    }
                    _ => Ok(expr(ExprKind::Name(name))),
                }
            }
            other => Err(CompileError::at(
                line,
                format!("expected a value, found {other:?}"),
            )),
        }
    }

    fn call_args(&mut self) -> Result<Vec<Expr>> {
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
                    return Err(CompileError::at(
                        self.line(),
                        format!("expected ',' or ')' in call, found {other:?}"),
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
            Tok::Name(n) if n == "in" => (BinOp::In, 7),
            // `not` in infix position can only start `not in` (two tokens;
            // expr() eats the second).
            Tok::Name(n) if n == "not" && matches!(self.peek2(), Tok::Name(m) if m == "in") => {
                (BinOp::NotIn, 7)
            }
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

/// Whether the expression contains a function call (side effects possible).
fn contains_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call(..) | ExprKind::MethodCall(..) => true,
        ExprKind::Unary(_, inner) => contains_call(inner),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => contains_call(a) || contains_call(b),
        ExprKind::List(elems) => elems.iter().any(contains_call),
        ExprKind::Dict(entries) => entries
            .iter()
            .any(|(k, v)| contains_call(k) || contains_call(v)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> Result<Vec<Stmt>> {
        parse(&lex(src).unwrap())
    }

    /// Build an expectation node; line is irrelevant (PartialEq ignores it).
    fn e(kind: ExprKind) -> Expr {
        Expr { kind, line: 0 }
    }

    fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
        e(ExprKind::Bin(op, Box::new(a), Box::new(b)))
    }

    fn int(n: i64) -> Expr {
        e(ExprKind::Int(n))
    }

    fn name(n: &str) -> Expr {
        e(ExprKind::Name(n.into()))
    }

    fn one_expr(src: &str) -> Expr {
        match parse_src(src).unwrap().pop().unwrap().kind {
            StmtKind::Expr(e) => e,
            other => panic!("expected expression statement, got {other:?}"),
        }
    }

    #[test]
    fn precedence_mul_over_add() {
        let got = one_expr("2 + 3 * 4");
        assert_eq!(
            got,
            bin(BinOp::Add, int(2), bin(BinOp::Mul, int(3), int(4)))
        );
    }

    #[test]
    fn comparison_binds_looser_than_arithmetic() {
        // x + 1 < 10  ==>  Lt(Add(x,1), 10)
        let got = one_expr("x + 1 < 10");
        assert_eq!(
            got,
            bin(BinOp::Lt, bin(BinOp::Add, name("x"), int(1)), int(10))
        );
    }

    #[test]
    fn and_binds_looser_than_comparison() {
        // a < b and c  ==> And(Lt(a,b), c)
        let got = one_expr("a < b and c");
        assert_eq!(
            got,
            bin(BinOp::And, bin(BinOp::Lt, name("a"), name("b")), name("c"))
        );
    }

    #[test]
    fn chained_comparison_desugars_to_and() {
        // 1 < 2 < 3  ==>  And(Lt(1,2), Lt(2,3)), like Python — NOT Lt(Lt(1,2), 3)
        let got = one_expr("1 < 2 < 3");
        assert_eq!(
            got,
            bin(
                BinOp::And,
                bin(BinOp::Lt, int(1), int(2)),
                bin(BinOp::Lt, int(2), int(3))
            )
        );
    }

    #[test]
    fn parses_assignment() {
        let s = parse_src("x = 2 + 3").unwrap().pop().unwrap();
        assert_eq!(
            s.kind,
            StmtKind::Assign("x".into(), bin(BinOp::Add, int(2), int(3)))
        );
    }

    #[test]
    fn parses_if_else() {
        let src = "if x:\n    print(1)\nelse:\n    print(2)\n";
        let s = parse_src(src).unwrap().pop().unwrap();
        match s.kind {
            StmtKind::If {
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
        match s.kind {
            StmtKind::For {
                var, start, end, ..
            } => {
                assert_eq!(var, "i");
                assert_eq!(start, int(0));
                assert_eq!(end, int(3));
            }
            other => panic!("expected for, got {other:?}"),
        }
    }

    #[test]
    fn nodes_carry_source_lines() {
        let stmts = parse_src("x = 1\ny = 2\nif x:\n    z = 3\n").unwrap();
        assert_eq!(stmts[0].line, 1);
        assert_eq!(stmts[1].line, 2);
        assert_eq!(stmts[2].line, 3);
        if let StmtKind::If { body, .. } = &stmts[2].kind {
            assert_eq!(body[0].line, 4);
        } else {
            panic!("expected if");
        }
    }

    #[test]
    fn errors_carry_lines() {
        let err = parse_src("x = 1\ny = +\n").unwrap_err();
        assert_eq!(err.line, Some(2));
    }
}
