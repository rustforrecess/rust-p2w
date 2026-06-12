//! Typed AST for the p2w Python subset.
//!
//! Deliberately small: it contains only the nodes the codegen handles, so the
//! parser and codegen grow together. Every node carries the 1-based source
//! line it started on, so any later stage can report a located error.
//!
//! `PartialEq` compares structure only (lines are ignored): two programs are
//! "equal" when they mean the same thing, which is also what tests want.

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: usize,
}

impl PartialEq for Stmt {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
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
    /// `for var in range(start, end, step): ...` — the counted fast path.
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Expr,
        body: Vec<Stmt>,
    },
    /// `for var in iterable: ...` over a sequence (list or string).
    ForEach {
        var: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    /// `while cond: ...`
    While { cond: Expr, body: Vec<Stmt> },
    /// `break` (inside a loop)
    Break,
    /// `continue` (inside a loop)
    Continue,
    /// `def name(params): ...` (top level only)
    Def {
        name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    /// `return [expr]` (inside a function; bare return yields None)
    Return(Option<Expr>),
    /// `target[index] = value`
    SetIndex {
        target: Expr,
        index: Expr,
        value: Expr,
    },
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: usize,
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    /// `True` / `False` — a distinct runtime type (prints as True/False),
    /// numerically equal to 1/0 like Python.
    Bool(bool),
    /// `None` — the singleton a function returns when it doesn't `return`.
    NoneLit,
    Str(String),
    Name(String),
    Unary(UnOp, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// A call by name, e.g. `print(...)`. Callee is a bare name for now.
    Call(String, Vec<Expr>),
    /// A list literal, e.g. `[1, 2, 3]`.
    List(Vec<Expr>),
    /// A dict literal, e.g. `{"a": 1}` (insertion-ordered, like Python).
    Dict(Vec<(Expr, Expr)>),
    /// Subscript read, e.g. `xs[i]` (lists and strings).
    Index(Box<Expr>, Box<Expr>),
    /// A method call, e.g. `xs.append(v)`.
    MethodCall(Box<Expr>, String, Vec<Expr>),
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
