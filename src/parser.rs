//! Recursive-descent parser with Pratt (precedence-climbing) expression
//! parsing — the structure Ruff's Python parser uses. Compound statements
//! (`if`, `for`, `while`) consume indented blocks delimited by INDENT/DEDENT;
//! simple statements consume their trailing NEWLINE.

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Method, Stmt, StmtKind, UnOp};
use crate::error::CompileError;
use crate::lexer::{Tok, Token};

type Result<T> = std::result::Result<T, CompileError>;

pub fn parse(tokens: &[Token]) -> Result<Vec<Stmt>> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        next_tmp: 0,
        recovering: false,
        errors: Vec::new(),
    };
    p.program()
}

/// Parse as much as possible, never bailing on the first error. Returns the
/// statements that did parse (a partial AST) plus every error encountered. Used
/// by the IDE's text->blocks path so one typo doesn't blank the whole canvas —
/// the valid statements still become blocks, and each mistake is reported.
///
/// Recovery is at the top-level statement boundary: a broken statement (and, if
/// it was a block header, its orphaned indented body) is skipped, then parsing
/// resumes with the next statement.
pub fn parse_recovering(tokens: &[Token]) -> (Vec<Stmt>, Vec<CompileError>) {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        next_tmp: 0,
        recovering: true,
        errors: Vec::new(),
    };
    p.program_recovering()
}

/// Parse a single expression (no trailing statement/newline) — used by the
/// step debugger to evaluate watch expressions in the current scope.
pub fn parse_expression(tokens: &[Token]) -> Result<Expr> {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        next_tmp: 0,
        recovering: false,
        errors: Vec::new(),
    };
    let e = p.expr(0)?;
    // Allow a trailing newline/EOF (the lexer appends one), but nothing else —
    // a watch is one expression, not a statement or a sequence.
    match p.peek() {
        Tok::Newline | Tok::Eof => Ok(e),
        other => Err(CompileError::at(
            p.line(),
            format!("a watch must be a single expression; found {other:?}"),
        )),
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    /// Counter for compiler-introduced temporaries (e.g. desugaring a
    /// tuple-target `for` loop). The `.u` prefix can't collide with a Python
    /// name.
    next_tmp: usize,
    /// When true, a failed statement is recorded and skipped (at any nesting)
    /// instead of aborting the whole parse — see [`take_statement`].
    recovering: bool,
    /// Errors gathered while `recovering`.
    errors: Vec<CompileError>,
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

    /// Byte offset of the current token's first character (for AST node spans).
    fn byte(&self) -> usize {
        self.toks[self.pos].start
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
                format!("expected {what}, but found {}", describe(self.peek())),
            ))
        }
    }

    /// Like `expect(Colon)`, but speaks the student's language: a header line
    /// (`if`/`for`/`def`/…) that runs straight into the end of the line almost
    /// always means a forgotten colon.
    fn expect_colon(&mut self) -> Result<()> {
        match self.peek() {
            Tok::Colon => {
                self.advance();
                Ok(())
            }
            Tok::Newline | Tok::Eof => Err(CompileError::at(
                self.line(),
                "did you forget a colon ':' at the end of this line?",
            )),
            other => Err(CompileError::at(
                self.line(),
                format!("expected a ':' here, but found {}", describe(other)),
            )),
        }
    }

    /// If a statement failed to parse and it began with a word that looks like a
    /// misspelled keyword — immediately followed by another value, e.g. `fro i`,
    /// which is never valid Python — replace the error with a focused "did you
    /// mean" suggestion. The "two values in a row" guard keeps this from
    /// second-guessing valid code (`total = 0` never trips it). `start` is the
    /// token index where the statement began.
    fn suggest_keyword(&self, err: CompileError, start: usize) -> CompileError {
        let Some(Tok::Name(n)) = self.toks.get(start).map(|t| &t.tok) else {
            return err;
        };
        let next_is_value = matches!(
            self.toks.get(start + 1).map(|t| &t.tok),
            Some(Tok::Name(_) | Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::FStr(_))
        );
        if !next_is_value {
            return err;
        }
        let line = self.toks[start].line;
        if n == "print" {
            return CompileError::at(line, "`print` needs parentheses, like `print(x)`");
        }
        match did_you_mean(n, STMT_KEYWORDS) {
            Some(kw) => CompileError::at(
                line,
                format!("`{n}` isn't a keyword — did you mean `{kw}`?"),
            ),
            None => err,
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
            self.take_statement(&mut stmts)?;
        }
        Ok(stmts)
    }

    /// Parse one statement into `stmts`. In recovering mode a failure is
    /// recorded and the parser resynchronizes (returning Ok so the loop
    /// continues); in strict mode the error propagates. Shared by `program` and
    /// `block`, so recovery works at every nesting level.
    fn take_statement(&mut self, stmts: &mut Vec<Stmt>) -> Result<()> {
        let start = self.pos;
        match self.statement() {
            Ok(s) => {
                stmts.push(s);
                Ok(())
            }
            Err(e) => {
                let e = self.suggest_keyword(e, start);
                if !self.recovering {
                    return Err(e);
                }
                self.errors.push(e);
                let before = self.pos;
                self.synchronize();
                // Guarantee forward progress so we can't loop forever.
                if self.pos == before {
                    self.advance();
                }
                Ok(())
            }
        }
    }

    /// Like [`program`], but recovers after a bad statement and keeps going,
    /// accumulating every error instead of returning the first. (Recovery
    /// happens at every nesting level — see [`take_statement`] and [`block`].)
    fn program_recovering(&mut self) -> (Vec<Stmt>, Vec<CompileError>) {
        // `program` never returns Err while `recovering` (take_statement
        // swallows statement errors).
        let stmts = self.program().unwrap_or_default();
        (stmts, std::mem::take(&mut self.errors))
    }

    /// Skip past a broken statement to the next safe restart point: the rest of
    /// the current line, plus — if the broken statement was a block header — its
    /// orphaned indented body (a balanced INDENT..DEDENT), so the token stream
    /// stays in sync.
    fn synchronize(&mut self) {
        while !matches!(self.peek(), Tok::Newline | Tok::Dedent | Tok::Eof) {
            self.advance();
        }
        if matches!(self.peek(), Tok::Newline) {
            self.advance();
        }
        if matches!(self.peek(), Tok::Indent) {
            let mut depth = 0usize;
            loop {
                match self.peek() {
                    Tok::Indent => {
                        depth += 1;
                        self.advance();
                    }
                    Tok::Dedent => {
                        self.advance();
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::Eof => break,
                    _ => {
                        self.advance();
                    }
                }
            }
        }
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
            self.take_statement(&mut stmts)?;
        }
        self.expect(&Tok::Dedent, "the end of the indented block")?;
        // While recovering, every statement in the body may have been skipped;
        // an empty body is fine (the compound still renders). Strict mode still
        // rejects a genuinely empty block.
        if stmts.is_empty() && !self.recovering {
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
        if self.is_keyword("class") {
            return self.class_stmt();
        }
        if self.is_keyword("import") {
            self.advance();
            let mut names = Vec::new();
            loop {
                match self.peek().clone() {
                    Tok::Name(m) => {
                        self.advance();
                        names.push(m);
                    }
                    other => {
                        return Err(CompileError::at(
                            self.line(),
                            format!("expected a module name after 'import', found {other:?}"),
                        ));
                    }
                }
                if matches!(self.peek(), Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&Tok::Newline, "a new line")?;
            return Ok(Stmt {
                kind: StmtKind::Import(names),
                line,
            });
        }
        if self.is_keyword("return") {
            self.advance();
            let value = if matches!(self.peek(), Tok::Newline) {
                None
            } else {
                Some(self.expr_list()?)
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
        // then decide based on what follows. `expr_list` makes a bare comma
        // list (`a, b` / `1, 2`) into a tuple.
        let e = self.expr_list()?;
        // Augmented assignment `target OP= rhs` desugars to `target = target OP
        // rhs` (the target is read once textually; a side-effecting index is
        // re-evaluated — a documented v1 simplification).
        if let Tok::AugAssign(op) = self.peek().clone() {
            self.advance();
            let rhs = self.expr(0)?;
            self.expect(&Tok::Newline, "a new line")?;
            let combined = |read: Expr, rhs: Expr| Expr {
                kind: ExprKind::Bin(op, Box::new(read), Box::new(rhs)),
                line,
                span: (0, 0),
            };
            return match e.kind {
                ExprKind::Name(name) => {
                    let read = Expr {
                        kind: ExprKind::Name(name.clone()),
                        line,
                        span: (0, 0),
                    };
                    Ok(Stmt {
                        kind: StmtKind::Assign(name, combined(read, rhs)),
                        line,
                    })
                }
                ExprKind::Index(target, index) => {
                    let read = Expr {
                        kind: ExprKind::Index(target.clone(), index.clone()),
                        line,
                        span: (0, 0),
                    };
                    Ok(Stmt {
                        kind: StmtKind::SetIndex {
                            target: *target,
                            index: *index,
                            value: combined(read, rhs),
                        },
                        line,
                    })
                }
                ExprKind::Attr(obj, attr) => {
                    let read = Expr {
                        kind: ExprKind::Attr(obj.clone(), attr.clone()),
                        line,
                        span: (0, 0),
                    };
                    Ok(Stmt {
                        kind: StmtKind::SetAttr {
                            obj: *obj,
                            attr,
                            value: combined(read, rhs),
                        },
                        line,
                    })
                }
                _ => Err(CompileError::at(
                    line,
                    "can only use += on a variable, an index like xs[i], or an attribute",
                )),
            };
        }
        // Annotated assignment `name: T = value`. The native backend uses `T` to
        // pick an unboxed representation; other backends treat it as a plain
        // assignment. Only a bare name may be annotated.
        if matches!(self.peek(), Tok::Colon) {
            if let ExprKind::Name(name) = e.kind {
                self.advance(); // ':'
                let ann = self.type_expr()?;
                self.expect(&Tok::Eq, "'=' after a type annotation")?;
                let value = self.expr_list()?;
                self.expect(&Tok::Newline, "a new line")?;
                return Ok(Stmt {
                    kind: StmtKind::AnnAssign { name, ann, value },
                    line,
                });
            }
            return Err(CompileError::at(
                line,
                "a type annotation ':' can only follow a variable name",
            ));
        }
        if matches!(self.peek(), Tok::Eq) {
            self.advance();
            let value = self.expr_list()?;
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
                ExprKind::Attr(obj, attr) => Ok(Stmt {
                    kind: StmtKind::SetAttr {
                        obj: *obj,
                        attr,
                        value,
                    },
                    line,
                }),
                // `a, b = ...` unpacks; each target must be assignable.
                ExprKind::Tuple(targets) => {
                    for t in &targets {
                        if !matches!(
                            t.kind,
                            ExprKind::Name(_) | ExprKind::Index(..) | ExprKind::Attr(..)
                        ) {
                            return Err(CompileError::at(
                                line,
                                "unpacking targets must be variables, indices, or attributes",
                            ));
                        }
                    }
                    Ok(Stmt {
                        kind: StmtKind::UnpackAssign { targets, value },
                        line,
                    })
                }
                ExprKind::Slice { .. } => Err(CompileError::at(
                    line,
                    "slice assignment (xs[a:b] = ...) isn't supported yet",
                )),
                _ => Err(CompileError::at(
                    line,
                    "can only assign to a variable, an index like xs[i], or an attribute like obj.x",
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
        self.expect_colon()?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;

        let mut elifs = Vec::new();
        let mut else_body = None;
        loop {
            if self.is_keyword("elif") {
                self.advance();
                let c = self.expr(0)?;
                self.expect_colon()?;
                self.expect(&Tok::Newline, "a new line")?;
                let b = self.block()?;
                elifs.push((c, b));
            } else if self.is_keyword("else") {
                self.advance();
                self.expect_colon()?;
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
                ));
            }
        };
        self.expect(&Tok::LParen, "'(' after the function name")?;
        let mut params = Vec::new();
        let mut param_types: Vec<Option<Expr>> = Vec::new();
        let mut defaults: Vec<Expr> = Vec::new();
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
                        // Optional type annotation (`: T`). A hint only — see
                        // StmtKind::Def — but parsed so the signature is faithful.
                        if matches!(self.peek(), Tok::Colon) {
                            self.advance();
                            param_types.push(Some(self.type_expr()?));
                        } else {
                            param_types.push(None);
                        }
                        // Optional default value (`= expr`).
                        if matches!(self.peek(), Tok::Eq) {
                            self.advance();
                            defaults.push(self.expr(0)?);
                        } else if !defaults.is_empty() {
                            return Err(CompileError::at(
                                self.line(),
                                "a parameter without a default can't follow one with a default",
                            ));
                        }
                    }
                    other => {
                        return Err(CompileError::at(
                            self.line(),
                            format!("expected a parameter name, found {other:?}"),
                        ));
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
        // Optional return annotation (`-> T`).
        let return_type = if matches!(self.peek(), Tok::Arrow) {
            self.advance();
            Some(self.type_expr()?)
        } else {
            None
        };
        self.expect_colon()?;
        self.expect(&Tok::Newline, "a new line")?;
        let body = self.block()?;
        Ok(Stmt {
            kind: StmtKind::Def {
                name,
                params,
                param_types,
                defaults,
                return_type,
                body,
            },
            line,
        })
    }

    /// Parse a type annotation (the `T` in `x: T` or `-> T`). A type is just an
    /// expression here — `int`, `str`, `list[int]`, `dict[str, int]` — so we
    /// reuse the expression parser; it stops cleanly at `,`, `=`, `)`, or `:`.
    fn type_expr(&mut self) -> Result<Expr> {
        self.expr(0)
    }

    fn class_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        self.eat_keyword("class")?;
        let name = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(CompileError::at(
                    self.line(),
                    format!("expected a class name after 'class', found {other:?}"),
                ));
            }
        };
        // Optional single base class: `class Name(Base):`
        let mut base = None;
        if matches!(self.peek(), Tok::LParen) {
            self.advance();
            if !matches!(self.peek(), Tok::RParen) {
                match self.peek().clone() {
                    Tok::Name(b) => {
                        self.advance();
                        base = Some(b);
                    }
                    other => {
                        return Err(CompileError::at(
                            self.line(),
                            format!("expected a base class name, found {other:?}"),
                        ));
                    }
                }
                if matches!(self.peek(), Tok::Comma) {
                    return Err(CompileError::at(
                        self.line(),
                        "multiple inheritance isn't supported — one base class only",
                    ));
                }
            }
            self.expect(&Tok::RParen, "')'")?;
        }
        self.expect_colon()?;
        self.expect(&Tok::Newline, "a new line")?;
        // Reuse the normal block parser, then split the body into methods and
        // class-level variable assignments.
        let body = self.block()?;
        let mut methods = Vec::new();
        let mut class_vars = Vec::new();
        for stmt in body {
            match stmt.kind {
                StmtKind::Def {
                    name,
                    params,
                    defaults,
                    body,
                    // Method annotations are accepted but, like Python at
                    // runtime, ignored (the Method struct stays untyped).
                    ..
                } => {
                    if !defaults.is_empty() {
                        return Err(CompileError::at(
                            stmt.line,
                            "default arguments aren't supported in methods yet",
                        ));
                    }
                    methods.push(Method { name, params, body })
                }
                StmtKind::Assign(n, v) => class_vars.push((n, v)),
                _ => {
                    return Err(CompileError::at(
                        stmt.line,
                        "a class body can only contain methods (def) and variable assignments",
                    ));
                }
            }
        }
        Ok(Stmt {
            kind: StmtKind::ClassDef {
                name,
                base,
                methods,
                class_vars,
            },
            line,
        })
    }

    fn while_stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        self.eat_keyword("while")?;
        let cond = self.expr(0)?;
        self.expect_colon()?;
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
        // One or more comma-separated loop variables (`for k, v in ...`).
        let mut targets = vec![self.for_target_name()?];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            targets.push(self.for_target_name()?);
        }
        let var = targets[0].clone();
        self.eat_keyword("in")?;
        // `range(...)` is the counted fast path; any other expression
        // iterates a sequence (list or string). A tuple target never uses the
        // range path (you can't unpack an int).
        if targets.len() == 1 && self.is_keyword("range") && *self.peek2() == Tok::LParen {
            self.advance(); // range
            self.advance(); // (
            let args = self.call_args()?;
            let int = |n: i64| Expr {
                kind: ExprKind::Int(n),
                line,
                span: (0, 0),
            };
            let (start, end, step) = match args.len() {
                1 => (int(0), args[0].clone(), int(1)),
                2 => (args[0].clone(), args[1].clone(), int(1)),
                3 => (args[0].clone(), args[1].clone(), args[2].clone()),
                n => {
                    return Err(CompileError::at(
                        self.line(),
                        format!("range() takes 1 to 3 arguments, got {n}"),
                    ));
                }
            };
            self.expect_colon()?;
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
        self.expect_colon()?;
        self.expect(&Tok::Newline, "a new line")?;
        let mut body = self.block()?;
        // A tuple target desugars to a single hidden loop variable plus an
        // unpacking assignment at the top of the body: `for k, v in it:` ->
        // `for .u in it: (k, v) = .u; <body>`.
        if targets.len() > 1 {
            let tmp = self.fresh_tmp();
            let unpack = Stmt {
                kind: StmtKind::UnpackAssign {
                    targets: targets
                        .into_iter()
                        .map(|n| Expr {
                            kind: ExprKind::Name(n),
                            line,
                            span: (0, 0),
                        })
                        .collect(),
                    value: Expr {
                        kind: ExprKind::Name(tmp.clone()),
                        line,
                        span: (0, 0),
                    },
                },
                line,
            };
            body.insert(0, unpack);
            return Ok(Stmt {
                kind: StmtKind::ForEach {
                    var: tmp,
                    iterable,
                    body,
                },
                line,
            });
        }
        Ok(Stmt {
            kind: StmtKind::ForEach {
                var,
                iterable,
                body,
            },
            line,
        })
    }

    /// A single `for` loop variable name (rejecting keywords like `in`).
    fn for_target_name(&mut self) -> Result<String> {
        match self.peek().clone() {
            Tok::Name(n) if !matches!(n.as_str(), "in" | "True" | "False" | "None") => {
                self.advance();
                Ok(n)
            }
            other => Err(CompileError::at(
                self.line(),
                format!("expected a loop variable, found {other:?}"),
            )),
        }
    }

    fn fresh_tmp(&mut self) -> String {
        let n = self.next_tmp;
        self.next_tmp += 1;
        format!(".u{n}")
    }

    /// Parse one expression, or a comma-separated list as a `Tuple` (a bare
    /// `a, b` / `1, 2,`). A single expression with no comma passes through
    /// unwrapped. Stops at tokens that can't start an expression (so a trailing
    /// comma before `=`, newline, `:`, or `)` is handled).
    fn expr_list(&mut self) -> Result<Expr> {
        let line = self.line();
        let first = self.expr(0)?;
        if !matches!(self.peek(), Tok::Comma) {
            return Ok(first);
        }
        let mut items = vec![first];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            if matches!(
                self.peek(),
                Tok::Newline | Tok::Eq | Tok::Colon | Tok::RParen | Tok::Eof
            ) {
                break; // trailing comma
            }
            items.push(self.expr(0)?);
        }
        Ok(Expr {
            kind: ExprKind::Tuple(items),
            line,
            span: (0, 0),
        })
    }

    /// Pratt expression parser.
    fn expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.prefix()?;
        while let Some((op, l_bp, r_bp)) = self.peek_infix() {
            if l_bp < min_bp {
                break;
            }
            let op_line = self.line();
            // Span of the operator token itself (single-char for the set ops
            // `& | - ^`), so the IDE can locate exactly which character to glyph.
            let op_byte = self.byte();
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
                    span: (op_byte, op_byte + 1),
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
            span: (0, 0),
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
                span: (0, 0),
            };
            chain = Expr {
                kind: ExprKind::Bin(BinOp::And, Box::new(chain), Box::new(pair)),
                line: op_line,
                span: (0, 0),
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
                span: (0, 0),
            });
        }
        if self.is_keyword("not") {
            self.advance();
            // `not` binds looser than comparisons but tighter than and/or.
            let operand = self.expr(7)?;
            return Ok(Expr {
                kind: ExprKind::Unary(UnOp::Not, Box::new(operand)),
                line,
                span: (0, 0),
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
                    // A leading `:` or a missing first operand means a slice;
                    // otherwise parse the first expression and decide on `:`.
                    let first = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr(0)?))
                    };
                    if matches!(self.peek(), Tok::Colon) {
                        self.advance(); // first ':'
                        let stop = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.expr(0)?))
                        };
                        let step = if matches!(self.peek(), Tok::Colon) {
                            self.advance(); // second ':'
                            if matches!(self.peek(), Tok::RBracket) {
                                None
                            } else {
                                Some(Box::new(self.expr(0)?))
                            }
                        } else {
                            None
                        };
                        self.expect(&Tok::RBracket, "']'")?;
                        e = Expr {
                            kind: ExprKind::Slice {
                                obj: Box::new(e),
                                start: first,
                                stop,
                                step,
                            },
                            line,
                            span: (0, 0),
                        };
                    } else {
                        let index = first.ok_or_else(|| {
                            CompileError::at(
                                line,
                                "empty subscript: write xs[i] or a slice xs[a:b]",
                            )
                        })?;
                        self.expect(&Tok::RBracket, "']'")?;
                        e = Expr {
                            kind: ExprKind::Index(Box::new(e), index),
                            line,
                            span: (0, 0),
                        };
                    }
                }
                Tok::Dot => {
                    let line = self.line();
                    self.advance();
                    let name = match self.peek().clone() {
                        Tok::Name(m) => {
                            self.advance();
                            m
                        }
                        other => {
                            return Err(CompileError::at(
                                self.line(),
                                format!("expected a name after '.', found {other:?}"),
                            ));
                        }
                    };
                    // `.name(...)` is a method call; bare `.name` is an
                    // attribute read.
                    if matches!(self.peek(), Tok::LParen) {
                        self.advance();
                        let args = self.call_args()?;
                        e = Expr {
                            kind: ExprKind::MethodCall(Box::new(e), name, args),
                            line,
                            span: (0, 0),
                        };
                    } else {
                        e = Expr {
                            kind: ExprKind::Attr(Box::new(e), name),
                            line,
                            span: (0, 0),
                        };
                    }
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let line = self.line();
        let expr = |kind| Expr {
            kind,
            line,
            span: (0, 0),
        };
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
            Tok::FStr(parts) => {
                self.advance();
                // Desugar to a str(...) concatenation chain: each literal
                // part is a Str, each {expr} part re-parses as a real
                // expression wrapped in str().
                let mut acc: Option<Expr> = None;
                for (is_expr, text, spec) in parts {
                    let piece = if is_expr {
                        let inner = parse_fragment(&text, line)?;
                        if spec.is_empty() {
                            expr(ExprKind::Call("str".into(), vec![inner]))
                        } else {
                            // `{x:spec}` -> format(x, "spec")
                            expr(ExprKind::Call(
                                "format".into(),
                                vec![
                                    inner,
                                    Expr {
                                        kind: ExprKind::Str(spec),
                                        line,
                                        span: (0, 0),
                                    },
                                ],
                            ))
                        }
                    } else {
                        expr(ExprKind::Str(text))
                    };
                    acc = Some(match acc {
                        None => piece,
                        Some(prev) => {
                            expr(ExprKind::Bin(BinOp::Add, Box::new(prev), Box::new(piece)))
                        }
                    });
                }
                Ok(acc.unwrap_or_else(|| expr(ExprKind::Str(String::new()))))
            }
            Tok::LParen => {
                self.advance();
                // `()` is the empty tuple.
                if matches!(self.peek(), Tok::RParen) {
                    self.advance();
                    return Ok(expr(ExprKind::Tuple(Vec::new())));
                }
                let first = self.expr(0)?;
                // `(x for x in xs)` is a (parenthesized) generator expression,
                // treated as an eager list here.
                if self.is_keyword("for") {
                    let clauses = self.comp_clauses()?;
                    self.expect(&Tok::RParen, "')'")?;
                    return Ok(expr(ExprKind::ListComp {
                        element: Box::new(first),
                        clauses,
                    }));
                }
                // A comma makes it a tuple; otherwise it's a grouping paren.
                if matches!(self.peek(), Tok::Comma) {
                    let mut items = vec![first];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break; // trailing comma (incl. the `(x,)` singleton)
                        }
                        items.push(self.expr(0)?);
                    }
                    self.expect(&Tok::RParen, "')'")?;
                    return Ok(expr(ExprKind::Tuple(items)));
                }
                self.expect(&Tok::RParen, "')'")?;
                Ok(first)
            }
            Tok::LBracket => {
                self.advance();
                if matches!(self.peek(), Tok::RBracket) {
                    self.advance();
                    return Ok(expr(ExprKind::List(Vec::new())));
                }
                let first = self.expr(0)?;
                // `[elem for ...]` is a comprehension; otherwise a list literal.
                if self.is_keyword("for") {
                    let clauses = self.comp_clauses()?;
                    self.expect(&Tok::RBracket, "']'")?;
                    return Ok(expr(ExprKind::ListComp {
                        element: Box::new(first),
                        clauses,
                    }));
                }
                let mut elements = vec![first];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    if matches!(self.peek(), Tok::RBracket) {
                        break; // trailing comma
                    }
                    elements.push(self.expr(0)?);
                }
                self.expect(&Tok::RBracket, "']'")?;
                Ok(expr(ExprKind::List(elements)))
            }
            Tok::LBrace => {
                self.advance();
                // `{}` is an empty dict (an empty set is `set()`).
                if matches!(self.peek(), Tok::RBrace) {
                    self.advance();
                    return Ok(expr(ExprKind::Dict(Vec::new())));
                }
                let first = self.expr(0)?;
                // A `:` after the first element means a dict; otherwise a set.
                if matches!(self.peek(), Tok::Colon) {
                    self.advance();
                    let value = self.expr(0)?;
                    if self.is_keyword("for") {
                        let clauses = self.comp_clauses()?;
                        self.expect(&Tok::RBrace, "'}'")?;
                        return Ok(expr(ExprKind::DictComp {
                            key: Box::new(first),
                            value: Box::new(value),
                            clauses,
                        }));
                    }
                    let mut entries = vec![(first, value)];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RBrace) {
                            break; // trailing comma
                        }
                        let k = self.expr(0)?;
                        self.expect(&Tok::Colon, "':' between a dict key and its value")?;
                        let v = self.expr(0)?;
                        entries.push((k, v));
                    }
                    self.expect(&Tok::RBrace, "'}'")?;
                    return Ok(expr(ExprKind::Dict(entries)));
                }
                // Set literal / comprehension — desugared to `set([...])` /
                // `set(<listcomp>)` so codegen reuses the set() builtin.
                let set_of = |arg: Expr| Expr {
                    kind: ExprKind::Call("set".into(), vec![arg]),
                    line,
                    span: (0, 0),
                };
                if self.is_keyword("for") {
                    let clauses = self.comp_clauses()?;
                    self.expect(&Tok::RBrace, "'}'")?;
                    return Ok(set_of(Expr {
                        kind: ExprKind::ListComp {
                            element: Box::new(first),
                            clauses,
                        },
                        line,
                        span: (0, 0),
                    }));
                }
                let mut elements = vec![first];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    if matches!(self.peek(), Tok::RBrace) {
                        break; // trailing comma
                    }
                    elements.push(self.expr(0)?);
                }
                self.expect(&Tok::RBrace, "'}'")?;
                Ok(set_of(Expr {
                    kind: ExprKind::List(elements),
                    line,
                    span: (0, 0),
                }))
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
        let mut seen_kwarg = false;
        loop {
            let line = self.line();
            // `name=value` is a keyword argument (but `name==value` is not).
            if let (Tok::Name(k), Tok::Eq) = (self.peek().clone(), self.peek2().clone()) {
                self.advance(); // name
                self.advance(); // =
                let value = self.expr(0)?;
                args.push(Expr {
                    kind: ExprKind::Kwarg(k, Box::new(value)),
                    line,
                    span: (0, 0),
                });
                seen_kwarg = true;
                match self.peek() {
                    Tok::Comma => {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break;
                        }
                        continue;
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
            if seen_kwarg {
                return Err(CompileError::at(
                    self.line(),
                    "positional argument can't follow a keyword argument",
                ));
            }
            let e = self.expr(0)?;
            // A bare generator expression as the sole argument, e.g.
            // `sum(x * x for x in xs)`, is treated as an (eager) list.
            if args.is_empty() && self.is_keyword("for") {
                let clauses = self.comp_clauses()?;
                self.expect(&Tok::RParen, "')'")?;
                return Ok(vec![Expr {
                    kind: ExprKind::ListComp {
                        element: Box::new(e),
                        clauses,
                    },
                    line,
                    span: (0, 0),
                }]);
            }
            args.push(e);
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

    /// The `for x in iter` / `if cond` clauses of a comprehension. The cursor
    /// is on the first `for`; parsing stops at the closing bracket/brace.
    fn comp_clauses(&mut self) -> Result<Vec<CompClause>> {
        let mut clauses = Vec::new();
        loop {
            if self.is_keyword("for") {
                self.advance();
                let mut vars = vec![self.for_target_name()?];
                while matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    vars.push(self.for_target_name()?);
                }
                self.eat_keyword("in")?;
                let iter = self.expr(0)?;
                clauses.push(CompClause::For { vars, iter });
            } else if self.is_keyword("if") {
                self.advance();
                clauses.push(CompClause::If(self.expr(0)?));
            } else {
                break;
            }
        }
        Ok(clauses)
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
            // Set operators sit between comparisons and `+`/`-` (Python's
            // order: `|` looser than `^` looser than `&`).
            Tok::Pipe => (BinOp::BitOr, 8),
            Tok::Caret => (BinOp::BitXor, 9),
            Tok::Amp => (BinOp::BitAnd, 10),
            Tok::Plus => (BinOp::Add, 12),
            Tok::Minus => (BinOp::Sub, 12),
            Tok::Star => (BinOp::Mul, 20),
            Tok::Slash => (BinOp::Div, 20),
            Tok::SlashSlash => (BinOp::FloorDiv, 20),
            Tok::Percent => (BinOp::Mod, 20),
            // `**` is right-associative and binds tighter than unary minus, so
            // `2 ** 3 ** 2` is `2 ** (3 ** 2)` and `-2 ** 2` is `-(2 ** 2)`.
            Tok::DoubleStar => return Some((BinOp::Pow, 30, 29)),
            _ => return None,
        };
        Some((op, bp, bp + 1)) // left-associative
    }
}

/// Unary prefix binding power — tighter than `*`/`/`/`%` but looser than `**`
/// (so `-2 ** 2` is `-(2 ** 2)`, matching Python).
const PREFIX_BP: u8 = 25;

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
    )
}

/// Parse one f-string `{...}` fragment as an expression. Errors are
/// reported at the f-string's line.
fn parse_fragment(src: &str, line: usize) -> Result<Expr> {
    let tokens = crate::lexer::lex(src).map_err(|e| CompileError::at(line, e.message))?;
    let mut p = Parser {
        toks: &tokens,
        pos: 0,
        next_tmp: 0,
        recovering: false,
        errors: Vec::new(),
    };
    let mut e = p.expr(0).map_err(|e| CompileError::at(line, e.message))?;
    if !matches!(p.peek(), Tok::Newline | Tok::Eof) {
        return Err(CompileError::at(
            line,
            "couldn't parse the expression inside the f-string braces",
        ));
    }
    // The fragment lexed as its own line 1 — re-line every node onto the
    // f-string's line so codegen errors point at the right place.
    set_lines(&mut e, line);
    Ok(e)
}

fn set_lines(e: &mut Expr, line: usize) {
    e.line = line;
    match &mut e.kind {
        ExprKind::Unary(_, inner) => set_lines(inner, line),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            set_lines(a, line);
            set_lines(b, line);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                set_lines(a, line);
            }
        }
        ExprKind::Kwarg(_, v) => set_lines(v, line),
        ExprKind::MethodCall(recv, _, args) => {
            set_lines(recv, line);
            for a in args {
                set_lines(a, line);
            }
        }
        ExprKind::List(elems) | ExprKind::Tuple(elems) => {
            for el in elems {
                set_lines(el, line);
            }
        }
        ExprKind::Dict(entries) => {
            for (k, v) in entries {
                set_lines(k, line);
                set_lines(v, line);
            }
        }
        _ => {}
    }
}

/// Whether the expression contains a function call (side effects possible).
fn contains_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call(..) | ExprKind::MethodCall(..) => true,
        ExprKind::Unary(_, inner) => contains_call(inner),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => contains_call(a) || contains_call(b),
        ExprKind::List(elems) | ExprKind::Tuple(elems) => elems.iter().any(contains_call),
        ExprKind::Dict(entries) => entries
            .iter()
            .any(|(k, v)| contains_call(k) || contains_call(v)),
        ExprKind::Attr(inner, _) => contains_call(inner),
        ExprKind::Kwarg(_, v) => contains_call(v),
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            contains_call(obj)
                || [start, stop, step]
                    .into_iter()
                    .flatten()
                    .any(|b| contains_call(b))
        }
        // Comprehensions evaluate a loop; treat as potentially side-effecting.
        ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => true,
        _ => false,
    }
}

/// Statement-starting keywords we offer "did you mean" suggestions for.
const STMT_KEYWORDS: &[&str] = &[
    "if", "elif", "else", "for", "while", "def", "class", "return", "import", "pass", "break",
    "continue",
];

/// Closest candidate within an edit-distance threshold that scales with word
/// length (short words must match almost exactly, to avoid wild guesses).
/// Exact matches are excluded — those aren't typos.
pub(crate) fn did_you_mean<'a>(word: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let threshold = if word.chars().count() <= 4 { 1 } else { 2 };
    candidates
        .iter()
        .map(|c| (*c, edit_distance(word, c)))
        .filter(|(_, d)| *d >= 1 && *d <= threshold)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Optimal string alignment distance — Levenshtein plus *adjacent
/// transpositions* counted as one edit, so the most common typos (`fro`->`for`,
/// `improt`->`import`) score distance 1 instead of 2.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            // Adjacent transposition (e.g. ...ab... vs ...ba...).
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// A short, student-friendly name for a token, for error messages (so a kid
/// sees "the end of the line" instead of `Newline`, or "`for`" instead of
/// `Name("for")`).
fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Int(n) => format!("the number {n}"),
        Tok::Float(f) => format!("the number {f}"),
        Tok::Str(_) => "a piece of text (a string)".to_string(),
        Tok::FStr(_) => "an f-string".to_string(),
        Tok::Name(n) => format!("`{n}`"),
        Tok::Plus => "`+`".to_string(),
        Tok::Minus => "`-`".to_string(),
        Tok::Star => "`*`".to_string(),
        Tok::DoubleStar => "`**`".to_string(),
        Tok::Slash => "`/`".to_string(),
        Tok::SlashSlash => "`//`".to_string(),
        Tok::Percent => "`%`".to_string(),
        Tok::Pipe => "`|`".to_string(),
        Tok::Amp => "`&`".to_string(),
        Tok::Caret => "`^`".to_string(),
        Tok::Lt => "`<`".to_string(),
        Tok::Le => "`<=`".to_string(),
        Tok::Gt => "`>`".to_string(),
        Tok::Ge => "`>=`".to_string(),
        Tok::EqEq => "`==`".to_string(),
        Tok::BangEq => "`!=`".to_string(),
        Tok::Eq => "`=`".to_string(),
        Tok::AugAssign(_) => "an augmented assignment (like `+=`)".to_string(),
        Tok::Colon => "`:`".to_string(),
        Tok::Arrow => "`->`".to_string(),
        Tok::LParen => "`(`".to_string(),
        Tok::RParen => "`)`".to_string(),
        Tok::LBracket => "`[`".to_string(),
        Tok::RBracket => "`]`".to_string(),
        Tok::LBrace => "`{`".to_string(),
        Tok::RBrace => "`}`".to_string(),
        Tok::Dot => "`.`".to_string(),
        Tok::Comma => "`,`".to_string(),
        Tok::Newline => "the end of the line".to_string(),
        Tok::Indent => "an indented block".to_string(),
        Tok::Dedent => "less indentation (the end of a block)".to_string(),
        Tok::Eof => "the end of your program".to_string(),
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
        Expr {
            kind,
            line: 0,
            span: (0, 0),
        }
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
    fn def_parses_param_and_return_annotations() {
        // `: T` per param (parallel to params, None when absent) and `-> T`.
        let stmts = parse_src("def greet(name: str, loud: bool, times=2) -> str:\n    return name")
            .unwrap();
        let StmtKind::Def {
            params,
            param_types,
            defaults,
            return_type,
            ..
        } = &stmts[0].kind
        else {
            panic!("expected a def, got {:?}", stmts[0].kind);
        };
        assert_eq!(params, &["name", "loud", "times"]);
        assert_eq!(
            param_types,
            &[Some(name("str")), Some(name("bool")), None],
            "annotations run parallel to params, None where absent (the default param)"
        );
        assert_eq!(defaults, &[int(2)], "the one default is unaffected");
        assert_eq!(return_type, &Some(name("str")));
    }

    #[test]
    fn def_subscripted_type_annotation_parses() {
        // A generic type like `list[int]` is just an expression to the parser
        // (a subscript), so it's accepted and stored without special-casing.
        let stmts = parse_src("def f(xs: list[int]):\n    return xs").unwrap();
        let StmtKind::Def { param_types, .. } = &stmts[0].kind else {
            panic!("expected a def");
        };
        assert_eq!(
            param_types[0],
            Some(e(ExprKind::Index(
                Box::new(name("list")),
                Box::new(name("int"))
            )))
        );
    }

    #[test]
    fn def_without_annotations_still_parses() {
        // Regression: the untyped form must be unchanged (no annotations).
        let stmts = parse_src("def add(a, b):\n    return a + b").unwrap();
        let StmtKind::Def {
            param_types,
            return_type,
            ..
        } = &stmts[0].kind
        else {
            panic!("expected a def");
        };
        assert_eq!(param_types, &[None, None]);
        assert_eq!(return_type, &None);
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
    fn parses_annotated_assignment() {
        let s = parse_src("x: int = 5").unwrap().pop().unwrap();
        match s.kind {
            StmtKind::AnnAssign { name, ann, value } => {
                assert_eq!(name, "x");
                assert_eq!(ann.kind, ExprKind::Name("int".into()));
                assert_eq!(value.kind, ExprKind::Int(5));
            }
            other => panic!("expected AnnAssign, got {other:?}"),
        }
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

    #[test]
    fn missing_colon_is_coached() {
        // Every compound-statement header should suggest the forgotten colon.
        for src in [
            "for i in range(5)\n    print(i)\n",
            "if x\n    print(x)\n",
            "while x\n    print(x)\n",
            "def f()\n    return 1\n",
        ] {
            let err = parse_src(src).unwrap_err();
            assert!(
                err.message.contains("colon"),
                "expected colon coaching for {src:?}, got: {}",
                err.message
            );
            assert_eq!(err.line, Some(1), "colon error should point at the header");
        }
    }

    #[test]
    fn error_messages_name_tokens_in_plain_language() {
        // No raw `Newline`/`Tok::…` debug output leaks to the student.
        let err = parse_src("if x:\nprint(x)\n").unwrap_err();
        assert!(
            !err.message.contains("Tok") && !err.message.contains("Newline"),
            "message should be student-friendly: {}",
            err.message
        );
    }

    #[test]
    fn describe_is_friendly() {
        assert_eq!(describe(&Tok::Newline), "the end of the line");
        assert_eq!(describe(&Tok::Colon), "`:`");
        assert_eq!(describe(&Tok::Name("foo".into())), "`foo`");
        assert_eq!(describe(&Tok::Eof), "the end of your program");
    }

    #[test]
    fn misspelled_keyword_is_suggested() {
        let cases = [
            ("fro i in range(5):\n    print(i)\n", "for"),
            ("whil x:\n    print(x)\n", "while"),
            ("retrun x\n", "return"),
            ("improt math\n", "import"),
        ];
        for (src, want) in cases {
            let err = parse_src(src).unwrap_err();
            assert!(
                err.message.contains(&format!("`{want}`")) && err.message.contains("did you mean"),
                "for {src:?} expected a suggestion of `{want}`, got: {}",
                err.message
            );
        }
    }

    #[test]
    fn print_without_parens_is_coached() {
        let err = parse_src("print x\n").unwrap_err();
        assert!(err.message.contains("parentheses"), "got: {}", err.message);
    }

    #[test]
    fn near_keyword_names_are_not_second_guessed() {
        // These are all valid; the suggester must never fire on them.
        for src in [
            "fi = 5\n",          // `fi` is edit-distance 1 from `if`
            "whil = 3\n",        // `whil` is 1 from `while`
            "fro = fro + 1\n",   // `fro` is 1 from `for`
            "x = 5\nprint(x)\n", // ordinary program
        ] {
            assert!(
                parse_src(src).is_ok(),
                "suggester wrongly rejected valid code: {src:?}"
            );
        }
    }

    #[test]
    fn recovery_keeps_valid_statements_around_a_bad_one() {
        // Missing colon on the `for`: the loop (and its orphaned body) is
        // skipped, but the assignment before and the print after still parse.
        let src = "total = 0\nfor i in range(1, 6)\n    total = total + 1\nprint(total)\n";
        let (stmts, errors) = parse_recovering(&lex(src).unwrap());
        assert_eq!(errors.len(), 1, "expected exactly one error");
        assert!(errors[0].message.contains("colon"), "{}", errors[0].message);
        assert_eq!(stmts.len(), 2, "assignment + print should survive");
        assert!(matches!(stmts[0].kind, StmtKind::Assign(..)));
        assert!(matches!(stmts[1].kind, StmtKind::Expr(_))); // print(total)
    }

    #[test]
    fn recovery_collects_multiple_errors() {
        // Two lines that lex fine but don't parse (`1 2 3`, `4 5 6` — numbers
        // with no operators between them), between three clean assignments.
        let src = "x = 1\n1 2 3\ny = 2\n4 5 6\nz = 3\n";
        let (stmts, errors) = parse_recovering(&lex(src).unwrap());
        assert!(errors.len() >= 2, "expected several errors, got {errors:?}");
        assert_eq!(stmts.len(), 3, "the three clean assignments should survive");
    }

    #[test]
    fn recovery_inside_a_block_keeps_the_compound() {
        // A syntax error in the loop body drops only that line; the `for` and
        // its other statements survive (per-block recovery).
        let src = "for i in range(3):\n    print(i)\n    1 2 3\n    print(i)\n";
        let (stmts, errors) = parse_recovering(&lex(src).unwrap());
        assert_eq!(stmts.len(), 1, "the for loop should survive: {stmts:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
        match &stmts[0].kind {
            StmtKind::For { body, .. } => {
                assert_eq!(body.len(), 2, "two prints survive, bad line dropped");
            }
            other => panic!("expected a for loop, got {other:?}"),
        }
    }

    #[test]
    fn recovery_on_clean_program_has_no_errors() {
        let src = "x = 1\nfor i in range(3):\n    print(i)\n";
        let (stmts, errors) = parse_recovering(&lex(src).unwrap());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("for", "for"), 0);
        assert_eq!(edit_distance("fro", "for"), 1); // transposition = 1 (OSA)
        assert_eq!(edit_distance("retrun", "return"), 1); // transposition
        assert_eq!(edit_distance("whil", "while"), 1); // insertion
        assert_eq!(did_you_mean("fro", STMT_KEYWORDS), Some("for"));
        assert_eq!(did_you_mean("x", STMT_KEYWORDS), None); // too short/far
        assert_eq!(did_you_mean("for", STMT_KEYWORDS), None); // exact = not a typo
    }
}
