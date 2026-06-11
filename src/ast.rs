//! Typed AST for the p2w Python subset.
//!
//! Deliberately small: it contains only the nodes the codegen handles, so the
//! parser and codegen grow together.

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// A bare expression used as a statement, e.g. `print("hi")`.
    Expr(Expr),
    /// Assignment to a simple name, e.g. `x = 5`.
    Assign(String, Expr),
    /// `if cond: ... [elif cond: ...]* [else: ...]`
    If {
        cond: Expr,
        body: Vec<Stmt>,
        elifs: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
    },
    /// `for var in range(start, end, step): ...`
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Expr,
        body: Vec<Stmt>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Str(String),
    Name(String),
    Unary(UnOp, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// A call by name, e.g. `print(...)`. Callee is a bare name for now.
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    // Comparison (yield 0/1)
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    // Logical (Python value semantics: the result is the deciding operand,
    // e.g. `2 and 1` is 1, `4 or 2` is 4; the right side is short-circuited)
    And,
    Or,
}
