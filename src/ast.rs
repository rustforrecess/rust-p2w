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
    /// Byte range `[start, end)` of the statement in the source — full extent,
    /// i.e. header through the last line of the body for compound statements.
    /// `(0, 0)` when unset (e.g. a desugared statement with no single origin).
    pub span: Span,
}

impl PartialEq for Stmt {
    // Structure only — line and span are positional metadata, not meaning.
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
    /// Annotated assignment, e.g. `x: int = 5`. Behaves like `Assign` in every
    /// backend except the native one, which uses `ann` to pick an unboxed
    /// representation for the slot (see VALUE_MODEL.md).
    AnnAssign {
        name: String,
        ann: Expr,
        value: Expr,
    },
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
    /// `pass` — an explicit no-op statement (empty-body placeholder).
    Pass,
    /// `def name(params) [-> ret]: ...` (top level only). `defaults` holds the
    /// default expressions for the trailing parameters (so `params[params.len()
    /// - defaults.len() ..]` each have one).
    ///
    /// `param_types` runs parallel to `params` — each entry is that parameter's
    /// `: T` annotation, or `None` if unannotated — and `return_type` is the
    /// `-> T`. Annotations are *hints*: codegen ignores them, exactly as Python
    /// does at runtime. They're kept so block surfaces can show types (the
    /// roadmap's typed-ports layer) and as groundwork for Mojo.
    Def {
        name: String,
        params: Vec<String>,
        param_types: Vec<Option<Expr>>,
        defaults: Vec<Expr>,
        return_type: Option<Expr>,
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
    /// `class Name[(Base)]: ...` (top level only). Body splits into methods
    /// and class-level variable assignments.
    ClassDef {
        name: String,
        base: Option<String>,
        methods: Vec<Method>,
        class_vars: Vec<(String, Expr)>,
    },
    /// `obj.attr = value`
    SetAttr {
        obj: Expr,
        attr: String,
        value: Expr,
    },
    /// Tuple-unpacking assignment, e.g. `a, b = pair` or `a, b = b, a`. Each
    /// target is a `Name`, `Index`, or `Attr`; `value` is any iterable.
    UnpackAssign { targets: Vec<Expr>, value: Expr },
    /// `import name[, name]*` (only `math` is supported for now).
    Import(Vec<String>),
}

/// A method inside a class body. `params[0]` is conventionally `self`.
#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// Byte range `[start, end)` of a node in the source, when tracked. Currently
/// populated only for binary-operator nodes (the *operator token's* range), to
/// support IDE features like set-notation glyphing; `(0, 0)` everywhere else.
/// Populating it for more node kinds (e.g. span-level error underlining) is a
/// future extension.
pub type Span = (usize, usize);

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: usize,
    pub span: Span,
}

impl PartialEq for Expr {
    // Structure only — line and span are positional metadata, not meaning.
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
    /// Keyword arguments appear in the arg list as `Kwarg` entries.
    Call(String, Vec<Expr>),
    /// A keyword argument `name=value`, only valid inside a call's arg list.
    Kwarg(String, Box<Expr>),
    /// A list literal, e.g. `[1, 2, 3]`.
    List(Vec<Expr>),
    /// A tuple, e.g. `(1, 2)`, `(1,)`, `()`, or a bare `1, 2`. Immutable.
    Tuple(Vec<Expr>),
    /// A dict literal, e.g. `{"a": 1}` (insertion-ordered, like Python).
    Dict(Vec<(Expr, Expr)>),
    /// Subscript read, e.g. `xs[i]` (lists and strings).
    Index(Box<Expr>, Box<Expr>),
    /// Slice read, e.g. `xs[1:3]`, `s[::-1]` (lists and strings). Any of the
    /// three bounds may be omitted (`None`).
    Slice {
        obj: Box<Expr>,
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    /// A method call, e.g. `xs.append(v)`.
    MethodCall(Box<Expr>, String, Vec<Expr>),
    /// Attribute read, e.g. `obj.attr` (a `.name` not followed by `(`).
    Attr(Box<Expr>, String),
    /// `[element for x in it if cond ...]`
    ListComp {
        element: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `{key: value for x in it if cond ...}`
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// `{element for x in it if cond ...}` — a set comprehension (deduped).
    SetComp {
        element: Box<Expr>,
        clauses: Vec<CompClause>,
    },
    /// A conditional expression `then if cond else orelse` (Python's ternary).
    /// Only the taken branch is evaluated.
    IfExp {
        cond: Box<Expr>,
        then: Box<Expr>,
        orelse: Box<Expr>,
    },
}

/// One clause of a comprehension: a `for` binding or an `if` filter, in source
/// order (a comprehension is `element` followed by one or more of these).
#[derive(Debug, Clone, PartialEq)]
pub enum CompClause {
    /// `for v in iter` or `for a, b in iter` (one or more name targets).
    For { vars: Vec<String>, iter: Expr },
    /// `if cond`
    If(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~x` — bitwise inversion (`-x - 1`), integers only. Matches CPython and
    /// MicroPython/viper (machine-word invert on the native path).
    Invert,
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
    Pow,
    // Bitwise on ints (or / and / xor) *or* set operations (union /
    // intersection / symmetric difference) — dispatched by operand type, the
    // same way `+` chooses int-add vs str/list-concat. This is CPython-exact
    // and matches viper's integer bit manipulation.
    BitOr,
    BitAnd,
    BitXor,
    // Bit shifts on integers: `<<`, `>>` (machine-word on the native/viper
    // path; arithmetic right shift, like CPython on non-negative ints).
    Shl,
    Shr,
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
    // Membership: `x in seq` / `x not in seq` (lists, dict keys, substrings)
    In,
    NotIn,
}
