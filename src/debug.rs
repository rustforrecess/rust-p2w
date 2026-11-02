//! Step-through debugger engine — a resumable AST interpreter.
//!
//! This is the browser `InterpreterAdapter` from `DEBUGGER_ARCHITECTURE.md`: a
//! tree-walking interpreter that can **pause between statements** so the IDE can
//! single-step, run to breakpoints, inspect variables, and evaluate watch
//! expressions. The fast WASM-GC backend stays the "Run" path; this is the
//! "Debug" path.
//!
//! Because the IDE runs this compiled to WASM (single-threaded, no blocking),
//! the interpreter can't suspend a native call stack. Instead it keeps its own
//! explicit **control stack** (`Cont`) and a one-statement-ahead `pending` slot,
//! so each `step()` returns to the caller cleanly with all state preserved.
//!
//! Scope (MVP): the common teaching subset — assignment (incl. augmented, which
//! the parser desugars), `print`, `if`/`elif`/`else`, `while`, counted `for`,
//! for-each over lists/strings/dicts, `break`/`continue`, subscript get/set, list
//! `append`/`pop`, and the usual expressions (arithmetic with Python semantics,
//! comparisons, `and`/`or`/`not`, membership, lists, dicts, a few builtins).
//! Anything outside the subset (user functions, classes, ...) stops with a
//! friendly "not in the step debugger yet — use Run" message rather than
//! misbehaving. Semantics mirror the compiler/CPython; the same differential
//! corpus can later guard them.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::error::CompileError;

/// A runtime value in the debugger's own little interpreter. Distinct from the
/// compiler's WASM representation — this one only needs to be inspectable and to
/// print like Python.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// `Rc`-shared and `RefCell`-mutable, like `Object` below — Python lists
    /// are a reference type: binding one to a new name, or passing one as a
    /// function argument, must let both names see the same mutations. A
    /// site that means an actual copy (slicing, `list(x)`, `.copy()`,
    /// concatenation) builds a *fresh* `Rc` instead of cloning this one.
    List(Rc<std::cell::RefCell<Vec<Value>>>),
    /// An immutable tuple `(a, b)`. A distinct type from `List`: it has no item
    /// assignment and isn't equal to a list with the same elements. Tuples
    /// are immutable in Python, so (unlike `List`) a plain owned `Vec` is
    /// correct here — nothing can mutate it to make aliasing observable.
    Tuple(Vec<Value>),
    /// Insertion-ordered unique elements (matches the compiled backends; sets are
    /// conceptually unordered, so equality below ignores order). `Rc`-shared
    /// for the same reference-type reason as `List`.
    Set(Rc<std::cell::RefCell<Vec<Value>>>),
    /// Insertion-ordered key/value pairs, like Python dicts. `Rc`-shared for
    /// the same reference-type reason as `List`.
    Dict(Rc<std::cell::RefCell<Vec<(Value, Value)>>>),
    /// A class instance. `Rc`-shared so aliases see each other's attribute
    /// writes (true reference semantics, like the compiled backends).
    Object(Rc<VmObj>),
    /// An open file: an index into `Vm::open_files`, not the file's state
    /// itself — mutation (reading advances a cursor) happens on that side
    /// table, the same reason `Value::Object` is `Rc`-shared, just simpler
    /// since nothing else can alias a given open handle.
    File(usize),
    None,
}

/// One open file's state. Lines keep their trailing `\n` (matching real
/// Python's `readline()`/file iteration), except possibly the last one, the
/// same way a real text file may or may not end in a newline.
struct VmFile {
    name: String,
    lines: Vec<String>,
    cursor: usize,
    closed: bool,
    mode: FileMode,
    /// Accumulated `.write()` calls, in write mode — read back via
    /// `Vm::file_writes` after the run. The debugger has no real filesystem
    /// to commit this to; whatever embeds it (the IDE, a test) decides where
    /// it actually lands.
    writes: String,
}

#[derive(Clone, Copy, PartialEq)]
enum FileMode {
    Read,
    Write,
}

impl VmFile {
    fn new(name: String, content: &str, mode: FileMode) -> Self {
        let mut lines: Vec<String> = Vec::new();
        let mut rest = content;
        while let Some(i) = rest.find('\n') {
            lines.push(rest[..=i].to_string());
            rest = &rest[i + 1..];
        }
        if !rest.is_empty() {
            lines.push(rest.to_string());
        }
        VmFile {
            name,
            lines,
            cursor: 0,
            closed: false,
            mode,
            writes: String::new(),
        }
    }
}

/// A class instance's storage. Equality is IDENTITY (same allocation) — the
/// debugger dispatches `__eq__` at explicit `==`/`!=` sites, but `in` and
/// dict-key lookups fall back to identity for instances (a documented
/// debugger-only narrowing; the compiled backends dispatch there too).
#[derive(Debug)]
pub struct VmObj {
    pub class: String,
    pub attrs: std::cell::RefCell<HashMap<String, Value>>,
}

impl PartialEq for VmObj {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Value {
    /// Build a fresh, independently-owned list — use this (never
    /// `existing.clone()`) at every site that means a genuine copy (a
    /// literal, slicing, `list(x)`, `.copy()`, concatenation). Cloning an
    /// existing `Value::List` is correct only when the intent IS aliasing
    /// (binding a name, passing an argument) — `Value`'s derived `Clone`
    /// already does that right, since it clones the `Rc` (shares), not the
    /// `Vec` inside it.
    fn list(v: Vec<Value>) -> Value {
        Value::List(Rc::new(std::cell::RefCell::new(v)))
    }
    fn set(v: Vec<Value>) -> Value {
        Value::Set(Rc::new(std::cell::RefCell::new(v)))
    }
    fn dict(v: Vec<(Value, Value)>) -> Value {
        Value::Dict(Rc::new(std::cell::RefCell::new(v)))
    }

    /// Python truthiness.
    fn truthy(&self) -> bool {
        match self {
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Bool(b) => *b,
            Value::Str(s) => !s.is_empty(),
            Value::List(v) => !v.borrow().is_empty(),
            Value::Tuple(v) => !v.is_empty(),
            Value::Set(v) => !v.borrow().is_empty(),
            Value::Dict(d) => !d.borrow().is_empty(),
            Value::Object(_) => true, // instances are truthy (CPython default)
            Value::File(_) => true,
            Value::None => false,
        }
    }

    /// `str()` form — how `print` shows it (strings are bare, no quotes).
    pub fn py_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            other => other.py_repr(),
        }
    }

    /// `repr()` form — how a value shows *inside* a container (strings quoted).
    pub fn py_repr(&self) -> String {
        match self {
            // Default instance display; a __str__/__repr__ is dispatched at
            // top-level print/str sites (values nested in containers show
            // this default — a documented debugger-only narrowing).
            Value::Object(o) => format!("<{} object>", o.class),
            Value::File(_) => "<file object>".to_string(),
            Value::Int(n) => n.to_string(),
            // CPython-exact float repr (shortest round-trip + sci-notation
            // thresholds); Rust's own `{:?}` disagrees on 16-digit ties and
            // never uses scientific notation.
            Value::Float(f) => crate::floatfmt::py_float_repr(*f),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Value::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Value::None => "None".to_string(),
            Value::List(v) => {
                let items: Vec<String> = v.borrow().iter().map(Value::py_repr).collect();
                format!("[{}]", items.join(", "))
            }
            // A 1-tuple keeps its trailing comma: `(1,)`. Empty is `()`.
            Value::Tuple(v) => {
                let items: Vec<String> = v.iter().map(Value::py_repr).collect();
                if items.len() == 1 {
                    format!("({},)", items[0])
                } else {
                    format!("({})", items.join(", "))
                }
            }
            // Empty set prints `set()` (Python) — `{}` is an empty dict.
            Value::Set(v) if v.borrow().is_empty() => "set()".to_string(),
            Value::Set(v) => {
                // Canonical *sorted* display when elements are homogeneously
                // orderable; mixed-type sets fall back to insertion order. Display
                // only — storage/iteration keep insertion order. Matches the WASM
                // backend's `$print_set`.
                let borrowed = v.borrow();
                let items: Vec<String> = sorted_set_view(&borrowed)
                    .iter()
                    .map(|x| x.py_repr())
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Dict(d) => {
                let items: Vec<String> = d
                    .borrow()
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.py_repr(), v.py_repr()))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
        }
    }
}

/// A set's elements in display order: sorted when all are numbers or all are
/// strings (the canonical written form), else original (insertion) order — so a
/// mixed-type set never tries to order incomparable values.
fn sorted_set_view(items: &[Value]) -> Vec<&Value> {
    let mut view: Vec<&Value> = items.iter().collect();
    if items.iter().all(|x| as_num(x).is_some()) {
        view.sort_by(|a, b| {
            as_num(a)
                .unwrap()
                .partial_cmp(&as_num(b).unwrap())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else if items.iter().all(|x| matches!(x, Value::Str(_))) {
        view.sort_by(|a, b| match (a, b) {
            (Value::Str(x), Value::Str(y)) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        });
    }
    view
}

/// Python equality across the numeric tower (int == float, bool == int), plus
/// structural equality for strings/lists/dicts.
fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        // Instances compare by IDENTITY here (`in`, dict keys, nested
        // containers); explicit ==/!= sites dispatch __eq__ before reaching
        // this (see the Bin task).
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::List(x), Value::List(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| py_eq(p, q))
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        // Sets are unordered: equal iff same size and every element is in both.
        (Value::Set(x), Value::Set(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().all(|p| y.iter().any(|q| py_eq(p, q)))
        }
        (Value::Dict(x), Value::Dict(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| py_eq(k, k2) && py_eq(v, v2)))
        }
        _ => match (as_num(a), as_num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// A true deep copy — unlike `Value`'s derived `Clone`, which shares
/// `List`/`Set`/`Dict`'s `Rc` on purpose (ordinary aliasing). Needed
/// wherever an independent snapshot matters, e.g. `Stepper::clone_scope`'s
/// side-effect-free watch evaluation. `Object` is left sharing its `Rc` —
/// Python instances are reference types even under `copy.copy`-free code
/// like this debugger's subset, so there is no "deep copy an object"
/// operation to mirror here.
fn deep_clone(v: &Value) -> Value {
    match v {
        Value::List(items) => Value::list(items.borrow().iter().map(deep_clone).collect()),
        Value::Set(items) => Value::set(items.borrow().iter().map(deep_clone).collect()),
        Value::Dict(pairs) => Value::dict(
            pairs
                .borrow()
                .iter()
                .map(|(k, v)| (deep_clone(k), deep_clone(v)))
                .collect(),
        ),
        Value::Tuple(items) => Value::Tuple(items.iter().map(deep_clone).collect()),
        other => other.clone(),
    }
}

/// Numeric value of int/float/bool, or None for non-numbers (bool is 1/0 like
/// Python).
fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Int value of int/bool (for `range`, indexing), or None.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

/// True if both operands are integral (int/bool), so int op int stays int.
fn both_int(a: &Value, b: &Value) -> bool {
    as_int(a).is_some() && as_int(b).is_some()
}

/// One entry on the explicit control stack. The stack *is* the resumable
/// program counter: the topmost entry says what to do next.
enum Cont {
    /// Execute `block[ip..]` in order.
    Seq { block: Rc<Vec<Stmt>>, ip: usize },
    /// A `while`: re-check `cond`; if truthy, run `body` once more.
    While { cond: Expr, body: Rc<Vec<Stmt>> },
    /// A counted `for`: yield `next`, then advance by `step` until past `stop`.
    ForRange {
        var: String,
        next: i64,
        stop: i64,
        step: i64,
        body: Rc<Vec<Stmt>>,
    },
    /// A for-each over a fixed sequence captured at loop entry.
    ForEach {
        var: String,
        items: Rc<Vec<Value>>,
        idx: usize,
        body: Rc<Vec<Stmt>>,
    },
}

impl Cont {
    fn is_loop(&self) -> bool {
        matches!(
            self,
            Cont::While { .. } | Cont::ForRange { .. } | Cont::ForEach { .. }
        )
    }
}

/// What `step`/`run` left the program in.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Paused, ready to run more. `line` is the statement about to execute.
    Paused { line: usize },
    /// Reached the end of the program.
    Finished,
    /// Stopped on an error (a runtime error, or a construct the debugger doesn't
    /// support yet). `line` is where it happened, when known.
    Error {
        line: Option<usize>,
        message: String,
    },
}

/// A resumable interpreter over the AST. Build with [`Stepper::new`], then drive
/// with [`Stepper::step`] / [`Stepper::run`], reading [`Stepper::status`],
/// [`Stepper::variables`], [`Stepper::output`], and [`Stepper::eval_watch`].
pub struct Stepper {
    stack: Vec<Cont>,
    scope: HashMap<String, Value>,
    output: String,
    /// The next statement to execute, already resolved from the control stack,
    /// so `status` can report the line *about to* run (and a breakpoint can stop
    /// before it executes).
    pending: Option<Stmt>,
    status: Status,
    /// User functions registered as their `def`s are stepped over.
    funcs: HashMap<String, FuncDef>,
    /// Data watchpoints: expressions whose value, when it changes, pauses a
    /// running program. Each remembers its last observed repr (None = it errored
    /// / was undefined last time).
    watchpoints: Vec<Watchpoint>,
    /// Set when `run` stopped because a watchpoint changed: (expr, old, new).
    watch_hit: Option<(String, String, String)>,
}

/// One break-on-change watchpoint.
struct Watchpoint {
    src: String,
    expr: Expr,
    last: Option<String>,
}

/// A user-defined function, captured when its `def` is stepped over. Calls to it
/// run atomically (step-over) — the body executes to completion in one step.
/// (True step-into / a call stack is the planned CPS rewrite; see
/// DEBUGGER_ARCHITECTURE.md.)
#[derive(Clone)]
struct FuncDef {
    params: Vec<String>,
    defaults: Vec<Expr>,
    body: Rc<Vec<Stmt>>,
}

/// How a statement finished inside the atomic function executor.
enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

impl Stepper {
    /// Parse `source` and prepare to run its first statement. Returns a compile
    /// error if it doesn't lex/parse (the same front-end as the real compiler).
    pub fn new(source: &str) -> Result<Stepper, CompileError> {
        let tokens = crate::lexer::lex(source)?;
        let program = crate::parser::parse(&tokens)?;
        // Same closure conversion the compiler runs, so nested functions (and
        // their captures) behave identically here and under Run. The pass keeps
        // statement order, so stepping is unchanged for everything else.
        let mut program = crate::hoist::hoist_nested_functions(program)?;
        crate::fnval::resolve(&mut program)?; // same first-class-function desugar Run gets
        crate::check::gate(&program)?; // Step refuses what Run refuses
        let mut s = Stepper {
            stack: vec![Cont::Seq {
                block: Rc::new(program),
                ip: 0,
            }],
            scope: HashMap::new(),
            output: String::new(),
            pending: None,
            status: Status::Finished, // replaced by prime() below
            funcs: HashMap::new(),
            watchpoints: Vec::new(),
            watch_hit: None,
        };
        s.prime();
        Ok(s)
    }

    /// Set the break-on-change watchpoints (expression sources). Unparseable
    /// entries are skipped; each one's "last seen" value is seeded from the
    /// current scope so it only fires on a *future* change.
    pub fn set_watchpoints(&mut self, srcs: &[String]) {
        self.watch_hit = None;
        self.watchpoints = srcs
            .iter()
            .filter_map(|s| {
                let toks = crate::lexer::lex(s).ok()?;
                let expr = crate::parser::parse_expression(&toks).ok()?;
                let last = self.eval_repr(&expr);
                Some(Watchpoint {
                    src: s.clone(),
                    expr,
                    last,
                })
            })
            .collect();
    }

    /// If `run` last stopped on a watchpoint, the `(expression, old, new)` that
    /// changed.
    pub fn watch_hit(&self) -> Option<(String, String, String)> {
        self.watch_hit.clone()
    }

    /// Evaluate an expression against a scratch copy of the current scope,
    /// returning its repr, or None if it errors (e.g. an undefined name).
    fn eval_repr(&self, expr: &Expr) -> Option<String> {
        let mut scratch = self.scope.clone();
        Self::eval_in(&self.funcs, &mut scratch, &mut None, expr)
            .ok()
            .map(|v| v.py_repr())
    }

    /// Re-evaluate every watchpoint; if any changed since last time, record the
    /// first as `watch_hit`, resync all of them, and return true.
    fn check_watchpoints(&mut self) -> bool {
        if self.watchpoints.is_empty() {
            return false;
        }
        let curs: Vec<Option<String>> = self
            .watchpoints
            .iter()
            .map(|w| self.eval_repr(&w.expr))
            .collect();
        let mut hit = None;
        for (w, cur) in self.watchpoints.iter().zip(&curs) {
            if *cur != w.last {
                let shown = |o: &Option<String>| o.clone().unwrap_or_else(|| "(undefined)".into());
                hit = Some((w.src.clone(), shown(&w.last), shown(cur)));
                break;
            }
        }
        for (w, cur) in self.watchpoints.iter_mut().zip(curs) {
            w.last = cur;
        }
        if let Some(h) = hit {
            self.watch_hit = Some(h);
            true
        } else {
            false
        }
    }

    /// Everything written by `print` so far.
    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.status, Status::Paused { .. })
    }

    /// The line about to execute (when paused).
    pub fn current_line(&self) -> Option<usize> {
        match self.status {
            Status::Paused { line } => Some(line),
            _ => None,
        }
    }

    /// User variables in scope, as `(name, repr)` pairs sorted by name. Skips
    /// compiler-introduced temporaries (names containing `.`).
    pub fn variables(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .scope
            .iter()
            .filter(|(n, _)| !n.contains('.'))
            .map(|(n, v)| (n.clone(), v.py_repr()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Evaluate a watch expression against the current scope, returning its
    /// `repr`. Read-only by construction (watches are expressions) — but note a
    /// watch that *calls* a mutating method would still mutate, like any
    /// debugger; the IDE restricts the UI accordingly.
    pub fn eval_watch(&self, expr_src: &str) -> Result<String, String> {
        let tokens = crate::lexer::lex(expr_src).map_err(|e| e.to_string())?;
        let expr = crate::parser::parse_expression(&tokens).map_err(|e| e.to_string())?;
        // Watches must not mutate, so evaluate against a scratch copy of scope.
        let mut scratch = self.clone_scope();
        Self::eval_in(&self.funcs, &mut scratch, &mut None, &expr).map(|v| v.py_repr())
    }

    /// A scratch copy of `scope` for `eval_watch` to evaluate against —
    /// **must** be a real deep copy, not `scope.clone()`. `Value`'s derived
    /// `Clone` shares `List`/`Set`/`Dict`'s `Rc` (that's the point, for
    /// ordinary aliasing); a watch evaluating a mutating method call (e.g.
    /// `xs.append(99)`) needs an independent copy instead, so the real
    /// program state stays untouched.
    fn clone_scope(&self) -> HashMap<String, Value> {
        self.scope
            .iter()
            .map(|(k, v)| (k.clone(), deep_clone(v)))
            .collect()
    }

    /// Execute exactly one statement, then re-prime to the next. No-op once the
    /// program has finished or errored.
    pub fn step(&mut self) {
        if !self.is_paused() {
            return;
        }
        self.watch_hit = None;
        let Some(stmt) = self.pending.take() else {
            self.status = Status::Finished;
            return;
        };
        if let Err(message) = self.exec(&stmt) {
            self.status = Status::Error {
                line: Some(stmt.line),
                message,
            };
            return;
        }
        self.prime();
    }

    /// Run until the program finishes, errors, or is about to execute a line in
    /// `breakpoints` (after taking at least one step, so "Continue" from a line
    /// that is itself a breakpoint doesn't stall there).
    pub fn run(&mut self, breakpoints: &[usize]) {
        self.watch_hit = None;
        let mut took_step = false;
        while self.is_paused() {
            if took_step
                && let Status::Paused { line } = self.status
                && breakpoints.contains(&line)
            {
                return;
            }
            self.step();
            took_step = true;
            // A watchpoint changing pauses the run right after the step that
            // changed it.
            if self.check_watchpoints() {
                return;
            }
        }
    }

    /// Resolve the control stack down to the next concrete statement, storing it
    /// in `pending` and reporting its line as the paused position. Resolving may
    /// evaluate loop conditions (a side-effect-free operation for normal code);
    /// an error there moves to `Error`.
    fn prime(&mut self) {
        match self.resolve_next() {
            Ok(Some(stmt)) => {
                self.status = Status::Paused { line: stmt.line };
                self.pending = Some(stmt);
            }
            Ok(None) => {
                self.status = Status::Finished;
                self.pending = None;
            }
            Err((line, message)) => {
                self.status = Status::Error { line, message };
                self.pending = None;
            }
        }
    }

    /// Pop/advance the control stack until the next concrete statement surfaces.
    /// Loop entries push their body (and, for `while`/`for`, stay underneath to
    /// re-check) using the pop-and-rebuild pattern, which keeps the borrow
    /// checker happy and the counter advance explicit.
    #[allow(clippy::type_complexity)]
    fn resolve_next(&mut self) -> Result<Option<Stmt>, (Option<usize>, String)> {
        loop {
            let Some(top) = self.stack.last_mut() else {
                return Ok(None);
            };
            match top {
                Cont::Seq { block, ip } => {
                    if *ip >= block.len() {
                        self.stack.pop();
                        continue;
                    }
                    let stmt = block[*ip].clone();
                    *ip += 1;
                    return Ok(Some(stmt));
                }
                Cont::While { .. } => {
                    let Some(Cont::While { cond, body }) = self.stack.pop() else {
                        unreachable!()
                    };
                    let truthy = Self::eval_in(
                        &self.funcs,
                        &mut self.scope,
                        &mut Some(&mut self.output),
                        &cond,
                    )
                    .map_err(|m| (None, m))?
                    .truthy();
                    if truthy {
                        self.stack.push(Cont::While {
                            cond,
                            body: body.clone(),
                        });
                        self.stack.push(Cont::Seq { block: body, ip: 0 });
                    }
                    continue;
                }
                Cont::ForRange { .. } => {
                    let Some(Cont::ForRange {
                        var,
                        next,
                        stop,
                        step,
                        body,
                    }) = self.stack.pop()
                    else {
                        unreachable!()
                    };
                    let go = (step > 0 && next < stop) || (step < 0 && next > stop);
                    if go {
                        self.scope.insert(var.clone(), Value::Int(next));
                        self.stack.push(Cont::ForRange {
                            var,
                            next: next + step,
                            stop,
                            step,
                            body: body.clone(),
                        });
                        self.stack.push(Cont::Seq { block: body, ip: 0 });
                    }
                    continue;
                }
                Cont::ForEach { .. } => {
                    let Some(Cont::ForEach {
                        var,
                        items,
                        idx,
                        body,
                    }) = self.stack.pop()
                    else {
                        unreachable!()
                    };
                    if idx < items.len() {
                        self.scope.insert(var.clone(), items[idx].clone());
                        self.stack.push(Cont::ForEach {
                            var,
                            items: items.clone(),
                            idx: idx + 1,
                            body: body.clone(),
                        });
                        self.stack.push(Cont::Seq { block: body, ip: 0 });
                    }
                    continue;
                }
            }
        }
    }

    /// Execute one statement's effect (and, for compound statements, push the
    /// chosen branch/loop onto the control stack).
    fn exec(&mut self, s: &Stmt) -> Result<(), String> {
        match &s.kind {
            StmtKind::Assign(name, e) | StmtKind::AnnAssign { name, value: e, .. } => {
                let v = self.eval(e)?;
                self.scope.insert(name.clone(), v);
                Ok(())
            }
            StmtKind::Expr(e) => {
                // `print(...)` writes output; any other expression statement
                // (e.g. `xs.append(1)`) runs for its side effects.
                if let ExprKind::Call(name, args) = &e.kind
                    && name == "print"
                {
                    let parts: Result<Vec<String>, String> =
                        args.iter().map(|a| Ok(self.eval(a)?.py_str())).collect();
                    self.output.push_str(&parts?.join(" "));
                    self.output.push('\n');
                    return Ok(());
                }
                self.eval(e)?;
                Ok(())
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                let idx = self.eval(index)?;
                let val = self.eval(value)?;
                self.assign_index(target, idx, val)
            }
            // `a, b = value` / `for k, v in …` — bind an iterable's elements to
            // the targets, one each (length must match, like CPython).
            StmtKind::UnpackAssign { targets, value } => {
                let v = self.eval(value)?;
                let items = to_elements(&v)?;
                if items.len() != targets.len() {
                    return Err(crate::messages::VALUE_UNPACK_COUNT.text.into());
                }
                for (target, item) in targets.iter().zip(items) {
                    match &target.kind {
                        ExprKind::Name(n) => {
                            self.scope.insert(n.clone(), item);
                        }
                        ExprKind::Index(obj, idx) => {
                            let index = self.eval(idx)?;
                            self.assign_index(obj, index, item)?;
                        }
                        _ => {
                            return Err("an unpacking target must be a variable or an index here"
                                .to_string());
                        }
                    }
                }
                Ok(())
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                if self.eval(cond)?.truthy() {
                    self.push_block(body);
                } else if let Some(branch) = self.pick_elif(elifs)? {
                    self.push_block(branch);
                } else if let Some(eb) = else_body {
                    self.push_block(eb);
                }
                Ok(())
            }
            StmtKind::While { cond, body } => {
                self.stack.push(Cont::While {
                    cond: cond.clone(),
                    body: Rc::new(body.clone()),
                });
                Ok(())
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let next = self.eval_int(start, "range() start")?;
                let stop = self.eval_int(end, "range() stop")?;
                let step = self.eval_int(step, "range() step")?;
                if step == 0 {
                    return Err("range() step must not be zero".to_string());
                }
                self.stack.push(Cont::ForRange {
                    var: var.clone(),
                    next,
                    stop,
                    step,
                    body: Rc::new(body.clone()),
                });
                Ok(())
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                let items = self.iterable_items(iterable)?;
                self.stack.push(Cont::ForEach {
                    var: var.clone(),
                    items: Rc::new(items),
                    idx: 0,
                    body: Rc::new(body.clone()),
                });
                Ok(())
            }
            StmtKind::Break => {
                // Unwind through any nested blocks up to and including the
                // enclosing loop.
                while let Some(c) = self.stack.pop() {
                    if c.is_loop() {
                        break;
                    }
                }
                Ok(())
            }
            StmtKind::Continue => {
                // Unwind nested blocks until the enclosing loop is back on top,
                // so the next prime re-checks / advances it.
                while let Some(c) = self.stack.last() {
                    if c.is_loop() {
                        break;
                    }
                    self.stack.pop();
                }
                Ok(())
            }
            StmtKind::Pass => Ok(()),
            // Stepping over a `def` registers the function; calls to it run
            // atomically (see eval_call). The body isn't entered here.
            StmtKind::Def {
                name,
                params,
                defaults,
                body,
                ..
            } => {
                self.funcs.insert(
                    name.clone(),
                    FuncDef {
                        params: params.clone(),
                        defaults: defaults.clone(),
                        body: Rc::new(body.clone()),
                    },
                );
                Ok(())
            }
            StmtKind::Return(_) => {
                Err("`return` outside a function — did you mean to indent it?".to_string())
            }
            StmtKind::Import(names) => check_import(names),
            StmtKind::ClassDef { .. } | StmtKind::SetAttr { .. } => Err(format!(
                "{} isn't in the step debugger yet — use Run for that",
                describe_stmt(&s.kind)
            )),
        }
    }

    fn push_block(&mut self, body: &[Stmt]) {
        if !body.is_empty() {
            self.stack.push(Cont::Seq {
                block: Rc::new(body.to_vec()),
                ip: 0,
            });
        }
    }

    fn pick_elif<'a>(
        &mut self,
        elifs: &'a [(Expr, Vec<Stmt>)],
    ) -> Result<Option<&'a [Stmt]>, String> {
        for (cond, body) in elifs {
            if self.eval(cond)?.truthy() {
                return Ok(Some(body));
            }
        }
        Ok(None)
    }

    /// Assign into `target[index]` against the step machine's scope.
    fn assign_index(&mut self, target: &Expr, index: Value, value: Value) -> Result<(), String> {
        let t = self.eval(target)?;
        assign_index_value(&t, index, value)
    }

    fn eval_int(&mut self, e: &Expr, what: &str) -> Result<i64, String> {
        let v = self.eval(e)?;
        as_int(&v).ok_or_else(|| format!("{what} must be an integer"))
    }

    fn iterable_items(&mut self, e: &Expr) -> Result<Vec<Value>, String> {
        match self.eval(e)? {
            Value::List(v) | Value::Set(v) => Ok(v.borrow().clone()),
            Value::Tuple(v) => Ok(v),
            Value::Str(s) => Ok(s.chars().map(|c| Value::Str(c.to_string())).collect()),
            Value::Dict(d) => Ok(d.borrow().iter().map(|(k, _)| k.clone()).collect()),
            other => Err(fill_msg(
                &crate::messages::TYPE_FOR_NOT_ITERABLE,
                &[("type", &type_label(&other))],
            )),
        }
    }

    fn eval(&mut self, e: &Expr) -> Result<Value, String> {
        Self::eval_in(&self.funcs, &mut self.scope, &mut Some(&mut self.output), e)
    }

    /// The expression evaluator. Takes the scope (and optional output sink, for
    /// the rare expression-with-output) explicitly so watches can run it against
    /// a scratch scope with no output.
    fn eval_in(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        e: &Expr,
    ) -> Result<Value, String> {
        match &e.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(s) => Ok(Value::Str(s.clone())),
            ExprKind::NoneLit => Ok(Value::None),
            ExprKind::Name(n) => scope
                .get(n)
                .cloned()
                .ok_or_else(|| format!("name '{n}' is not defined")),
            ExprKind::Unary(op, inner) => {
                let v = Self::eval_in(funcs, scope, out, inner)?;
                match op {
                    UnOp::Not => Ok(Value::Bool(!v.truthy())),
                    UnOp::Neg => match v {
                        Value::Int(n) => Ok(Value::Int(-n)),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        Value::Bool(b) => Ok(Value::Int(if b { -1 } else { 0 })),
                        _ => Err(expected_number(&v)),
                    },
                    UnOp::Invert => match v {
                        Value::Int(n) => Ok(Value::Int(!n)),
                        Value::Bool(b) => Ok(Value::Int(!(b as i64))),
                        _ => Err(format!("bad operand type for unary ~: '{}'", type_name(&v))),
                    },
                }
            }
            ExprKind::Bin(op, a, b) => Self::eval_bin(funcs, scope, out, *op, a, b),
            ExprKind::List(items) => {
                let mut out_items = Vec::with_capacity(items.len());
                for it in items {
                    out_items.push(Self::eval_in(funcs, scope, out, it)?);
                }
                Ok(Value::list(out_items))
            }
            ExprKind::Tuple(items) => {
                let mut out_items = Vec::with_capacity(items.len());
                for it in items {
                    out_items.push(Self::eval_in(funcs, scope, out, it)?);
                }
                Ok(Value::Tuple(out_items))
            }
            ExprKind::Dict(pairs) => {
                let mut out_pairs = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    out_pairs.push((
                        Self::eval_in(funcs, scope, out, k)?,
                        Self::eval_in(funcs, scope, out, v)?,
                    ));
                }
                Ok(Value::dict(out_pairs))
            }
            ExprKind::Index(obj, idx) => {
                let target = Self::eval_in(funcs, scope, out, obj)?;
                let index = Self::eval_in(funcs, scope, out, idx)?;
                index_get(&target, &index)
            }
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                let target = Self::eval_in(funcs, scope, out, obj)?;
                let mut bound =
                    |b: &Option<Box<Expr>>, what: &str| -> Result<Option<i64>, String> {
                        match b {
                            Some(e) => Ok(Some(
                                as_int(&Self::eval_in(funcs, scope, out, e)?)
                                    .ok_or_else(|| format!("slice {what} must be an integer"))?,
                            )),
                            None => Ok(None),
                        }
                    };
                let s = bound(start, "start")?;
                let t = bound(stop, "stop")?;
                let p = bound(step, "step")?;
                slice_value(&target, s, t, p)
            }
            ExprKind::Attr(obj, attr) => {
                // `math.pi` / `math.e` / `math.tau` (the stepper has no user
                // objects, so a module constant is the only attribute here).
                if let ExprKind::Name(m) = &obj.kind
                    && m == "math"
                    && !scope.contains_key(m)
                {
                    return math_const(attr)
                        .ok_or_else(|| format!("math has no attribute '{attr}'"));
                }
                Err(
                    "attribute access isn't in the step debugger yet — use Run for that"
                        .to_string(),
                )
            }
            ExprKind::Call(name, args) => Self::eval_call(funcs, scope, out, name, args),
            ExprKind::MethodCall(obj, method, args) => {
                Self::eval_method(funcs, scope, out, obj, method, args)
            }
            ExprKind::IfExp { cond, then, orelse } => {
                let taken = if Self::eval_in(funcs, scope, out, cond)?.truthy() {
                    then
                } else {
                    orelse
                };
                Self::eval_in(funcs, scope, out, taken)
            }
            _ => Err(format!(
                "{} isn't in the step debugger yet — use Run for that",
                describe_expr(&e.kind)
            )),
        }
    }

    fn eval_bin(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        op: BinOp,
        a: &Expr,
        b: &Expr,
    ) -> Result<Value, String> {
        // `and`/`or` short-circuit and return the deciding operand (Python).
        if op == BinOp::And {
            let l = Self::eval_in(funcs, scope, out, a)?;
            return if l.truthy() {
                Self::eval_in(funcs, scope, out, b)
            } else {
                Ok(l)
            };
        }
        if op == BinOp::Or {
            let l = Self::eval_in(funcs, scope, out, a)?;
            return if l.truthy() {
                Ok(l)
            } else {
                Self::eval_in(funcs, scope, out, b)
            };
        }

        let l = Self::eval_in(funcs, scope, out, a)?;
        let r = Self::eval_in(funcs, scope, out, b)?;
        match op {
            BinOp::Eq => Ok(Value::Bool(py_eq(&l, &r))),
            BinOp::Ne => Ok(Value::Bool(!py_eq(&l, &r))),
            BinOp::Is => Ok(Value::Bool(
                matches!(l, Value::None) && matches!(r, Value::None),
            )),
            BinOp::IsNot => Ok(Value::Bool(
                !(matches!(l, Value::None) && matches!(r, Value::None)),
            )),
            BinOp::In => Ok(Value::Bool(contains(&r, &l)?)),
            BinOp::NotIn => Ok(Value::Bool(!contains(&r, &l)?)),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, &l, &r),
            BinOp::Add => add(&l, &r),
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
                arith(op, &l, &r)
            }
            BinOp::And | BinOp::Or => unreachable!("handled above"),
            BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => bitwise_or_set(op, &l, &r),
            BinOp::Shl | BinOp::Shr => int_shift(op, &l, &r),
        }
    }

    fn eval_call(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        name: &str,
        args: &[Expr],
    ) -> Result<Value, String> {
        // `sorted(seq, reverse=<bool>)` — the one builtin that takes a keyword.
        if let Some((seq_e, rev_e)) = sorted_reverse_args(name, args) {
            let seq = Self::eval_in(funcs, scope, out, seq_e)?;
            let rev = Self::eval_in(funcs, scope, out, rev_e)?.truthy();
            return sorted_values(&seq, rev);
        }
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(Self::eval_in(funcs, scope, out, a)?);
        }
        // A call to a user-defined function runs its body atomically (step-over).
        if let Some(fdef) = funcs.get(name) {
            return Self::call_user(funcs, out, name, fdef, vals);
        }
        match (name, vals.as_slice()) {
            ("len", [v]) => match v {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::List(l) | Value::Set(l) => Ok(Value::Int(l.borrow().len() as i64)),
                Value::Tuple(l) => Ok(Value::Int(l.len() as i64)),
                Value::Dict(d) => Ok(Value::Int(d.borrow().len() as i64)),
                _ => Err(fill_msg(
                    &crate::messages::TYPE_NO_LEN,
                    &[("type", &type_label(v))],
                )),
            },
            ("abs", [v]) => match v {
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                _ => Err(expected_number(v)),
            },
            ("str", [v]) => Ok(Value::Str(v.py_str())),
            ("int", [v]) => match v {
                Value::Int(n) => Ok(Value::Int(*n)),
                Value::Float(f) => Ok(Value::Int(*f as i64)),
                Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                Value::Str(s) => parse_int_str(s),
                _ => Err(expected_number(v)),
            },
            ("float", [v]) => match v {
                Value::Str(s) => s
                    .trim()
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| fill_msg(&crate::messages::VALUE_FLOAT_PARSE, &[("text", s)])),
                _ => as_num(v)
                    .map(Value::Float)
                    .ok_or_else(|| expected_number(v)),
            },
            ("bool", [v]) => Ok(Value::Bool(v.truthy())),
            ("set", []) => Ok(Value::set(Vec::new())),
            ("set", [v]) => make_set(v),
            ("list", []) => Ok(Value::list(Vec::new())),
            ("list", [v]) => Ok(Value::list(to_elements(v)?)),
            ("tuple", []) => Ok(Value::Tuple(Vec::new())),
            ("tuple", [v]) => Ok(Value::Tuple(to_elements(v)?)),
            ("dict", []) => Ok(Value::dict(Vec::new())),
            ("print", _) => {
                // print used as a value: write, return None.
                if let Some(sink) = out {
                    let parts: Vec<String> = vals.iter().map(Value::py_str).collect();
                    sink.push_str(&parts.join(" "));
                    sink.push('\n');
                }
                Ok(Value::None)
            }
            // Designer-authored layout call (see the Vm's twin arm): trace as a
            // no-op — the real move happens under Run.
            ("set_position", [_sel, _x, _y]) => Ok(Value::None),
            ("add_element", [_parent, _tag, _id]) => Ok(Value::None),
            ("pointer_x", []) | ("pointer_y", []) => Ok(Value::Int(0)),
            ("on_frame", [_h]) => Ok(Value::None),
            _ => match builtin_extra(name, vals.as_slice())? {
                Some(v) => Ok(v),
                None => Err(format!(
                    "calling {name}() isn't in the step debugger yet — use Run for that"
                )),
            },
        }
    }

    fn eval_method(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        obj: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Value, String> {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(Self::eval_in(funcs, scope, out, a)?);
        }
        // `math.sqrt(x)` etc. — a module function (unless `math` is a variable).
        if let ExprKind::Name(m) = &obj.kind
            && m == "math"
            && !scope.contains_key(m)
        {
            return math_call(method, vals.as_slice());
        }
        // Mutating list/set/dict/string methods need the variable itself (for
        // the mutating ones to write back), so require a Name — delegates to
        // the same dispatcher the step machine's Task::MethodOnName uses, so
        // a method behaves identically whether it's stepped or run inside a
        // called function's atomic body.
        if let ExprKind::Name(name) = &obj.kind
            && let Some(v) = scope.get_mut(name)
            && matches!(
                v,
                Value::List(_) | Value::Tuple(_) | Value::Set(_) | Value::Dict(_) | Value::Str(_)
            )
        {
            return container_method(v, method, vals);
        }
        // General receiver (a literal, or a non-container/non-Name
        // expression): evaluate it and dispatch by value. Mutation is
        // harmless since the value is a temporary.
        let mut recv = Self::eval_in(funcs, scope, out, obj)?;
        container_method(&mut recv, method, vals)
    }

    /// Run a user function to completion (step-over): bind args (and defaults)
    /// into a fresh local scope, execute the body atomically, and return its
    /// value (None if it fell off the end). Recurses for nested/recursive calls.
    fn call_user(
        funcs: &HashMap<String, FuncDef>,
        out: &mut Option<&mut String>,
        name: &str,
        fdef: &FuncDef,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let nparams = fdef.params.len();
        let nrequired = nparams - fdef.defaults.len();
        if args.len() < nrequired || args.len() > nparams {
            return Err(format!(
                "{name}() takes {nparams} argument(s) but {} were given",
                args.len()
            ));
        }
        let mut local = HashMap::new();
        for (i, p) in fdef.params.iter().enumerate() {
            let v = if i < args.len() {
                args[i].clone()
            } else {
                // Trailing parameter with a default; evaluated in an empty scope
                // (good enough for the literal defaults beginners use).
                let mut empty = HashMap::new();
                Self::eval_in(funcs, &mut empty, out, &fdef.defaults[i - nrequired])?
            };
            local.insert(p.clone(), v);
        }
        match Self::exec_block(funcs, &mut local, out, &fdef.body)? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::None), // ran off the end -> None
        }
    }

    /// Execute a block of statements atomically (for step-over function bodies),
    /// propagating break/continue/return as [`Flow`].
    fn exec_block(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        stmts: &[Stmt],
    ) -> Result<Flow, String> {
        for s in stmts {
            match Self::exec_atomic(funcs, scope, out, s)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    /// One statement, executed atomically (no pausing) — the executor used
    /// inside called functions. Mirrors the step machine's `exec`, but runs
    /// compounds to completion and returns control flow as [`Flow`].
    fn exec_atomic(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        s: &Stmt,
    ) -> Result<Flow, String> {
        match &s.kind {
            StmtKind::Assign(name, e) | StmtKind::AnnAssign { name, value: e, .. } => {
                let v = Self::eval_in(funcs, scope, out, e)?;
                scope.insert(name.clone(), v);
                Ok(Flow::Normal)
            }
            StmtKind::Expr(e) => {
                if let ExprKind::Call(name, args) = &e.kind
                    && name == "print"
                {
                    let mut parts = Vec::with_capacity(args.len());
                    for a in args {
                        parts.push(Self::eval_in(funcs, scope, out, a)?.py_str());
                    }
                    if let Some(sink) = out {
                        sink.push_str(&parts.join(" "));
                        sink.push('\n');
                    }
                    return Ok(Flow::Normal);
                }
                Self::eval_in(funcs, scope, out, e)?;
                Ok(Flow::Normal)
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                let t = Self::eval_in(funcs, scope, out, target)?;
                let idx = Self::eval_in(funcs, scope, out, index)?;
                let val = Self::eval_in(funcs, scope, out, value)?;
                assign_index_value(&t, idx, val)?;
                Ok(Flow::Normal)
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                if Self::eval_in(funcs, scope, out, cond)?.truthy() {
                    return Self::exec_block(funcs, scope, out, body);
                }
                for (c, b) in elifs {
                    if Self::eval_in(funcs, scope, out, c)?.truthy() {
                        return Self::exec_block(funcs, scope, out, b);
                    }
                }
                match else_body {
                    Some(eb) => Self::exec_block(funcs, scope, out, eb),
                    None => Ok(Flow::Normal),
                }
            }
            StmtKind::While { cond, body } => {
                while Self::eval_in(funcs, scope, out, cond)?.truthy() {
                    match Self::exec_block(funcs, scope, out, body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal | Flow::Continue => {}
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let int =
                    |v: Value, what: &str| as_int(&v).ok_or(format!("{what} must be an integer"));
                let mut next = int(Self::eval_in(funcs, scope, out, start)?, "range() start")?;
                let stop = int(Self::eval_in(funcs, scope, out, end)?, "range() stop")?;
                let step = int(Self::eval_in(funcs, scope, out, step)?, "range() step")?;
                if step == 0 {
                    return Err("range() step must not be zero".to_string());
                }
                while (step > 0 && next < stop) || (step < 0 && next > stop) {
                    scope.insert(var.clone(), Value::Int(next));
                    match Self::exec_block(funcs, scope, out, body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal | Flow::Continue => {}
                    }
                    next += step;
                }
                Ok(Flow::Normal)
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                let items = match Self::eval_in(funcs, scope, out, iterable)? {
                    Value::List(v) => v.borrow().clone(),
                    Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                    Value::Dict(d) => d.borrow().iter().map(|(k, _)| k.clone()).collect(),
                    other => {
                        return Err(fill_msg(
                            &crate::messages::TYPE_FOR_NOT_ITERABLE,
                            &[("type", &type_label(&other))],
                        ));
                    }
                };
                for it in items {
                    scope.insert(var.clone(), it);
                    match Self::exec_block(funcs, scope, out, body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal | Flow::Continue => {}
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::Break => Ok(Flow::Break),
            StmtKind::Continue => Ok(Flow::Continue),
            StmtKind::Pass => Ok(Flow::Normal),
            StmtKind::Return(opt) => {
                let v = match opt {
                    Some(e) => Self::eval_in(funcs, scope, out, e)?,
                    None => Value::None,
                };
                Ok(Flow::Return(v))
            }
            StmtKind::UnpackAssign { targets, value } => {
                let v = Self::eval_in(funcs, scope, out, value)?;
                let items = to_elements(&v)?;
                if items.len() != targets.len() {
                    return Err(crate::messages::VALUE_UNPACK_COUNT.text.into());
                }
                for (target, item) in targets.iter().zip(items) {
                    match &target.kind {
                        ExprKind::Name(n) => {
                            scope.insert(n.clone(), item);
                        }
                        ExprKind::Index(obj, idx) => {
                            let t = Self::eval_in(funcs, scope, out, obj)?;
                            let index = Self::eval_in(funcs, scope, out, idx)?;
                            assign_index_value(&t, index, item)?;
                        }
                        _ => {
                            return Err("an unpacking target must be a variable or an index here"
                                .to_string());
                        }
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::Def { .. } => Err(
                "a nested function definition isn't in the step debugger yet — use Run".to_string(),
            ),
            StmtKind::Import(names) => check_import(names).map(|_| Flow::Normal),
            StmtKind::ClassDef { .. } | StmtKind::SetAttr { .. } => Err(format!(
                "{} isn't in the step debugger yet — use Run for that",
                describe_stmt(&s.kind)
            )),
        }
    }
}

/// Assign into `target[index] = value`, where `target` is an already-
/// evaluated value (a list or a dict — int index, negative ok, for a list;
/// any key for a dict). Works for ANY target expression, not just a bare
/// variable: `target` was evaluated like any other expression before this
/// runs, so `obj.attr[i] = v` arrives here holding the attribute's own
/// `Rc`-shared container — mutating it through `.borrow_mut()` here is
/// already mutating the real object's attribute, no write-back needed.
fn assign_index_value(target: &Value, index: Value, value: Value) -> Result<(), String> {
    match target {
        Value::List(items) => {
            let mut items = items.borrow_mut();
            let i = as_int(&index).ok_or_else(|| expected_number(&index))?;
            let n = items.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err(fill_msg(
                    &crate::messages::INDEX_OUT_OF_RANGE,
                    &[("seq", "list")],
                ));
            }
            items[real as usize] = value;
            Ok(())
        }
        Value::Dict(pairs) => {
            let mut pairs = pairs.borrow_mut();
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| py_eq(k, &index)) {
                slot.1 = value;
            } else {
                pairs.push((index, value));
            }
            Ok(())
        }
        Value::Tuple(_) => Err(fill_msg(
            &crate::messages::TYPE_NO_ITEM_ASSIGN,
            &[("type", "tuple")],
        )),
        other => Err(fill_msg(
            &crate::messages::TYPE_NO_ITEM_ASSIGN,
            &[("type", &type_label(other))],
        )),
    }
}

/// `target[index]` read for lists (int, negative ok), strings (int -> 1-char
/// string), and dicts (any key).
fn index_get(target: &Value, index: &Value) -> Result<Value, String> {
    match target {
        Value::List(items) => {
            let items = items.borrow();
            let i = as_int(index).ok_or_else(|| expected_number(index))?;
            let n = items.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err(fill_msg(
                    &crate::messages::INDEX_OUT_OF_RANGE,
                    &[("seq", "list")],
                ));
            }
            Ok(items[real as usize].clone())
        }
        Value::Tuple(items) => {
            let i = as_int(index).ok_or_else(|| expected_number(index))?;
            let n = items.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err(fill_msg(
                    &crate::messages::INDEX_OUT_OF_RANGE,
                    &[("seq", "list")],
                ));
            }
            Ok(items[real as usize].clone())
        }
        Value::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let i = as_int(index).ok_or_else(|| expected_number(index))?;
            let n = chars.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err(fill_msg(
                    &crate::messages::INDEX_OUT_OF_RANGE,
                    &[("seq", "string")],
                ));
            }
            Ok(Value::Str(chars[real as usize].to_string()))
        }
        Value::Dict(pairs) => pairs
            .borrow()
            .iter()
            .find(|(k, _)| py_eq(k, index))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| fill_msg(&crate::messages::KEY_MISSING, &[("key", &index.py_repr())])),
        _ => Err(fill_msg(
            &crate::messages::TYPE_NOT_SUBSCRIPTABLE,
            &[("type", &type_label(target))],
        )),
    }
}

/// Membership test `needle in haystack` for strings (substring), lists, and dict
/// keys.
fn contains(haystack: &Value, needle: &Value) -> Result<bool, String> {
    match haystack {
        Value::List(items) | Value::Set(items) => {
            Ok(items.borrow().iter().any(|v| py_eq(v, needle)))
        }
        Value::Tuple(items) => Ok(items.iter().any(|v| py_eq(v, needle))),
        Value::Dict(pairs) => Ok(pairs.borrow().iter().any(|(k, _)| py_eq(k, needle))),
        Value::Str(s) => match needle {
            Value::Str(sub) => Ok(s.contains(sub.as_str())),
            _ => Err(fill_msg(
                &crate::messages::TYPE_IN_STRING_LEFT,
                &[("type", &type_label(needle))],
            )),
        },
        _ => Err(fill_msg(
            &crate::messages::TYPE_ARG_NOT_ITERABLE,
            &[("type", &type_label(haystack))],
        )),
    }
}

fn compare(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    let ord = match (l, r) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => {
            let (x, y) = (as_num(l), as_num(r));
            match (x, y) {
                (Some(x), Some(y)) => x
                    .partial_cmp(&y)
                    .ok_or("can't compare these values (NaN?)")?,
                _ => return Err(expected_number_bin(op, l, r)),
            }
        }
    };
    use std::cmp::Ordering::*;
    let res = match op {
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(Value::Bool(res))
}

/// `+` — numeric add, string concat, or list concat.
fn add(l: &Value, r: &Value) -> Result<Value, String> {
    match (l, r) {
        (Value::Str(x), Value::Str(y)) => Ok(Value::Str(format!("{x}{y}"))),
        (Value::List(x), Value::List(y)) => {
            let mut v = x.borrow().clone();
            v.extend(y.borrow().iter().cloned());
            Ok(Value::list(v))
        }
        _ => arith(BinOp::Add, l, r),
    }
}

/// Numeric arithmetic with Python semantics: `/` is always float, `//` floors,
/// `%` follows the divisor's sign, mixed int/float promotes to float.
fn arith(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    // `set - set` is set difference, not arithmetic.
    if op == BinOp::Sub && matches!((l, r), (Value::Set(_), Value::Set(_))) {
        return set_binop(BinOp::Sub, l, r);
    }
    // `seq * int` / `int * seq` — sequence repetition (str or list, either side).
    if op == BinOp::Mul {
        let rep = |seq: &Value, n: i64| {
            let n = n.max(0) as usize;
            match seq {
                Value::Str(s) => Value::Str(s.repeat(n)),
                Value::List(xs) => {
                    let xs = xs.borrow();
                    Value::list(xs.iter().cloned().cycle().take(xs.len() * n).collect())
                }
                _ => unreachable!("guarded to str/list"),
            }
        };
        match (l, r) {
            (Value::Str(_) | Value::List(_), _) if as_int(r).is_some() => {
                return Ok(rep(l, as_int(r).unwrap()));
            }
            (_, Value::Str(_) | Value::List(_)) if as_int(l).is_some() => {
                return Ok(rep(r, as_int(l).unwrap()));
            }
            _ => {}
        }
    }
    let (x, y) = match (as_num(l), as_num(r)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(expected_number_bin(op, l, r)),
    };
    let ints = both_int(l, r);
    match op {
        // The int cases go through `int_result`, which enforces the same 32-bit
        // range the compiled backends do. The interpreter computes in f64 and
        // would otherwise happily return 1000000000000 for a program that traps
        // when compiled — the Stepper must not show an answer the run cannot
        // produce.
        BinOp::Add => int_result(x + y, ints),
        BinOp::Sub => int_result(x - y, ints),
        BinOp::Mul => int_result(x * y, ints),
        BinOp::Div => {
            if y == 0.0 {
                return Err(crate::messages::ZERO_DIVISION.text.into());
            }
            Ok(Value::Float(x / y)) // true division is always float
        }
        BinOp::FloorDiv => {
            if y == 0.0 {
                return Err(crate::messages::ZERO_DIVISION.text.into());
            }
            Ok(num_result((x / y).floor(), ints))
        }
        BinOp::Mod => {
            if y == 0.0 {
                return Err(crate::messages::ZERO_DIVISION.text.into());
            }
            // Python modulo: result takes the divisor's sign.
            Ok(num_result(x - (x / y).floor() * y, ints))
        }
        BinOp::Pow => {
            let p = x.powf(y);
            // int ** non-negative int stays int.
            if ints && y >= 0.0 {
                Ok(Value::Int(p as i64))
            } else {
                Ok(Value::Float(p))
            }
        }
        _ => unreachable!(),
    }
}

/// Wrap a numeric result as Int when both operands were integral, else Float.
fn num_result(v: f64, ints: bool) -> Value {
    if ints {
        Value::Int(v as i64)
    } else {
        Value::Float(v)
    }
}

/// Whole numbers are 32-bit on the compiled backends, so the interpreter has to
/// refuse the same values they refuse. Message matches `$raise_overflow` in
/// codegen — one behaviour, three surfaces.
fn int_result(v: f64, ints: bool) -> Result<Value, String> {
    if ints && (v > i32::MAX as f64 || v < i32::MIN as f64) {
        return Err(
            "this calculation went outside the range of whole numbers we can store \
             (-2147483648 to 2147483647)"
                .to_string(),
        );
    }
    Ok(num_result(v, ints))
}

/// `int(<str>)` with the value model's range: a number outside i32 is an
/// OverflowError (matching both compiled backends), junk is a ValueError.
fn parse_int_str(s: &str) -> Result<Value, String> {
    match s.trim().parse::<i64>() {
        Ok(n) if (i32::MIN as i64..=i32::MAX as i64).contains(&n) => Ok(Value::Int(n)),
        Ok(_) => Err(crate::messages::INT_OVERFLOW.text.into()),
        Err(_) => {
            // An all-digit string that overflows even i64 is still an
            // overflow, not junk.
            let t = s.trim();
            let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                Err(crate::messages::INT_OVERFLOW.text.into())
            } else {
                Err(fill_msg(&crate::messages::VALUE_INT_PARSE, &[("text", s)]))
            }
        }
    }
}

/// Format-fill a table message (`crate::messages`): each `{slot}` replaced.
fn fill_msg(m: &crate::messages::Msg, vals: &[(&str, &str)]) -> String {
    let mut out = m.text.to_string();
    for (k, v) in vals {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// The type label the COMPILED backends print: instances report their class
/// name, everything else its type name.
fn type_label(v: &Value) -> String {
    match v {
        Value::Object(o) => o.class.clone(),
        _ => type_name(v).to_string(),
    }
}

/// The compiled backends' wrong-type-in-numeric-context error.
fn expected_number(v: &Value) -> String {
    fill_msg(
        &crate::messages::TYPE_EXPECTED_NUMBER,
        &[("type", &type_label(v))],
    )
}

/// Same, for a binary op — mirroring the compiled blame order: `+` blames the
/// string operand (the string is the odd one out in a numeric context);
/// everything else blames left-to-right.
fn expected_number_bin(op: BinOp, l: &Value, r: &Value) -> String {
    let (lstr, rstr) = (matches!(l, Value::Str(_)), matches!(r, Value::Str(_)));
    let blame = if matches!(op, BinOp::Add) && lstr != rstr {
        if lstr { l } else { r }
    } else if as_num(l).is_none() {
        l
    } else {
        r
    };
    expected_number(blame)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "object",
        Value::File(_) => "file",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Tuple(_) => "tuple",
        Value::Set(_) => "set",
        Value::Dict(_) => "dict",
        Value::None => "NoneType",
    }
}

fn describe_stmt(k: &StmtKind) -> &'static str {
    match k {
        StmtKind::Def { .. } => "defining a function",
        StmtKind::Return(_) => "`return`",
        StmtKind::ClassDef { .. } => "classes",
        StmtKind::SetAttr { .. } => "attribute assignment",
        StmtKind::UnpackAssign { .. } => "tuple unpacking",
        StmtKind::Import(_) => "`import`",
        _ => "this statement",
    }
}

fn describe_expr(k: &ExprKind) -> &'static str {
    match k {
        ExprKind::Tuple(_) => "tuples",
        ExprKind::Slice { .. } => "slicing",
        ExprKind::Attr(..) => "attribute access",
        ExprKind::ListComp { .. } | ExprKind::DictComp { .. } | ExprKind::SetComp { .. } => {
            "comprehensions"
        }
        _ => "this expression",
    }
}

// ======================================================================
// CPS VM — an explicit-stack interpreter that can step *into* functions
// and expose a live call stack (the destination from DEBUGGER_ARCHITECTURE.md).
//
// Unlike `Stepper` (a tree-walker that runs calls atomically), this keeps an
// explicit work stack of `Task`s plus an operand stack per call frame, so
// execution can suspend ANYWHERE — including mid-expression at a call — and
// resume. That's what makes true step-into and a call stack possible.
//
// Phase 1 scope: control flow (if/elif/else, while, counted for, break/continue,
// return), user functions (step-into, recursion, default args), arithmetic /
// comparison / and-or / unary, names with a global read-fallback, print, and a
// few builtins. Lists/dicts/indexing/methods/for-each come in Phase 2; they stop
// with a friendly message for now. The atomic `Stepper` stays the IDE engine
// until this reaches parity.
// ======================================================================

/// A captured user function for the VM.
#[derive(Clone)]
struct VmFunc {
    params: Vec<String>,
    defaults: Vec<Expr>,
    body: Rc<Vec<Stmt>>,
}

/// Normalized `if`/`elif`/`else`: ordered branches, each `(condition, body)`
/// with `None` condition meaning the `else`.
type Branches = Rc<Vec<(Option<Expr>, Rc<Vec<Stmt>>)>>;

/// One unit of pending work on a frame's continuation stack. Popped from the
/// back (top). Expression tasks push their result onto the operand stack.
enum Task {
    /// Statement iterator over a block; `idx < len` is a step boundary.
    Next(Rc<Vec<Stmt>>, usize),
    Eval(Rc<Expr>),
    Bin(BinOp),
    Unary(UnOp),
    /// Short-circuit `and`/`or`: inspect the operand on top, maybe eval the rhs.
    AndThen(Rc<Expr>),
    OrElse(Rc<Expr>),
    /// Conditional expression: pop the condition, eval `then` or `orelse`.
    Ternary(Rc<Expr>, Rc<Expr>),
    /// Pop a value and bind it to a name in the current scope.
    Store(String),
    /// Push an already-known value — for stitching a pre-resolved value
    /// into an operand-stack sequence built from other, still-pending
    /// `Eval` tasks (see `Task::Unpack`'s index-target case).
    Push(Value),
    /// Pop an iterable value, then assign its elements to these targets
    /// (one each). A `Name` target binds directly; anything else (e.g. an
    /// `Index` target, for `a[i], a[j] = a[j], a[i]`) schedules its own
    /// `obj`/`index` evaluation followed by `StoreIndex`, exactly like a
    /// single `target[index] = value` statement would.
    Unpack(Vec<Expr>),
    /// Pop the reverse flag then the sequence; push `sorted(seq, reverse=…)`.
    SortedKw,
    /// Pop one argument; push `math.<func>(arg)`.
    MathCall(String, usize),
    /// `random.<fn>` with N evaluated args (shuffle-on-a-variable is handled
    /// at dispatch, where the scope is reachable).
    RandomCall(String, usize),
    /// Pop `n` values and print them (space-joined + newline).
    Print(usize),
    /// Discard the top operand (an expression statement's result).
    Pop,
    /// Pop `argc` args and call `name` (user function -> new frame; else builtin).
    Call(String, usize),
    /// `if`/`elif`/`else` as a normalized branch list; decides which to run.
    IfChain(Branches, usize),
    /// Pop a bool: branch `idx` taken if true, else fall through to the next.
    IfTest(Branches, usize),
    /// `while`: (re)evaluate the condition, then decide.
    WhileHead(Rc<Expr>, Rc<Vec<Stmt>>),
    WhileTest(Rc<Expr>, Rc<Vec<Stmt>>),
    /// Pop start/end/step and start a counted loop.
    ForSetup(String, Rc<Vec<Stmt>>),
    /// A live counted loop (also the break/continue marker).
    ForHead {
        var: String,
        next: i64,
        stop: i64,
        step: i64,
        body: Rc<Vec<Stmt>>,
    },
    Break,
    Continue,
    Return,
    /// Pop `n` values and build a list.
    BuildList(usize),
    /// Pop `n` values and build a tuple.
    BuildTuple(usize),
    /// Pop `2n` values (key/value interleaved) and build a dict.
    BuildDict(usize),
    /// Pop index then target; push `target[index]`.
    IndexGet,
    /// Pop the present bounds (step, stop, start — only those flagged true) then
    /// the target; push `target[start:stop:step]`.
    Slice(bool, bool, bool),
    /// Pop `argc` args and call `name.method(...)` on a list variable.
    MethodOnName(String, String, usize),
    /// Pop the iterable value and start a for-each loop.
    ForEachSetup(String, Rc<Vec<Stmt>>),
    /// A live for-each loop (also a break/continue marker).
    ForEachHead {
        var: String,
        items: Rc<Vec<Value>>,
        idx: usize,
        body: Rc<Vec<Stmt>>,
    },
    /// Pop value, then index, then target; assign `target[index] = value`.
    /// `target` is evaluated like any other expression (so `obj.attr[i] = v`
    /// mutates through the SAME `Rc`-shared container the attribute holds,
    /// not a scope slot) — no restriction to a bare name.
    StoreIndex,
    /// Pop the (discarded) __init__ return and push the constructed instance.
    ReplaceTop(Value),
    /// Pop an instance; push its attribute's value.
    AttrGet(String),
    /// Pop value then instance; store the attribute.
    StoreAttr(String),
    /// Pop a value; an instance becomes its __str__/__repr__ (frame call) or
    /// default display string — everything else passes through untouched.
    ToDisplay,
    /// Pop `argc` args then the receiver VALUE; dispatch a method on an
    /// instance (non-Name receivers — Name receivers use MethodOnName).
    CallMethodValue(String, usize),
    /// Pop `argc` args; call `owner.method` with the current frame's `self`
    /// (the compile-time-resolved `super().method(...)`).
    CallSuper(String, String, usize),
    /// Pop `argc` args then the base instance; call `method` on its ATTRIBUTE
    /// (`self.tricks.append(..)`): an instance attribute dispatches a frame,
    /// a container attribute mutates in place inside the attrs cell.
    CallMethodOnAttr(String, String, usize),
    /// Pop a value; store it as class `0`'s class variable `1`
    /// (`Counter.count = v` — resolved to the owning class at lowering).
    StoreClassVar(String, String),
}

impl Task {
    /// Loop markers that `break`/`continue` unwind to.
    fn is_loop(&self) -> bool {
        matches!(
            self,
            Task::WhileHead(..) | Task::ForHead { .. } | Task::ForEachHead { .. }
        )
    }
}

/// A class as the debugger sees it: its base and directly-defined methods.
struct VmClass {
    base: Option<String>,
    methods: HashMap<String, VmFunc>,
    /// Class variables (evaluated at the class statement, like function
    /// defaults — simple constants). Instance attrs shadow these.
    class_vars: HashMap<String, Value>,
}

/// One call frame: its own continuation + operand stacks and local scope.
struct VmFrame {
    /// Display name for the call stack ("<module>" for the top level).
    func: String,
    work: Vec<Task>,
    operands: Vec<Value>,
    scope: HashMap<String, Value>,
    line: usize,
    /// The class whose method body this frame runs (None elsewhere) —
    /// `super()` resolves from this class's base at the call site.
    class_ctx: Option<String>,
}

/// The CPS virtual machine. Same public surface as [`Stepper`], plus
/// [`Vm::call_stack`].
pub struct Vm {
    frames: Vec<VmFrame>,
    funcs: HashMap<String, VmFunc>,
    /// Classes registered by executed `class` statements: name -> (base,
    /// directly-defined methods). Lookup walks the base chain (hop-capped).
    classes: HashMap<String, VmClass>,
    output: String,
    status: Status,
    watchpoints: Vec<Watchpoint>,
    watch_hit: Option<(String, String, String)>,
    /// The value most recently returned to a caller during the last step
    /// (cleared at the start of each step) — a teaching cue for "what came back".
    last_return: Option<Value>,
    /// Pre-supplied input lines served to `input()` during stepping (the VM has
    /// no real stdin; the IDE fills this from its input box via `set_stdin`).
    stdin: std::collections::VecDeque<String>,
    /// Pre-staged file content, keyed by the name a program's `open(name)`
    /// call uses — the debugger has no real filesystem underneath any more
    /// than it has real stdin; the embedder supplies what's answerable via
    /// `set_files` (a lesson's bundled example files, say) before running.
    file_sources: HashMap<String, String>,
    /// Handles created by `open()` so far, indexed by `Value::File`.
    open_files: Vec<VmFile>,
    /// xorshift64* state — the bit-exact mirror of the compiled backend's
    /// $rnd helpers. Seeded 42 like the test hosts' env.seed, so the Vm and
    /// the compiled module produce the SAME sequence under test.
    rnd: u64,
}

impl Vm {
    pub fn new(source: &str) -> Result<Vm, CompileError> {
        let tokens = crate::lexer::lex(source)?;
        let program = crate::parser::parse(&tokens)?;
        // Closure-convert like the compiler, so a nested function that captures
        // steps the same way it Runs (and a bad capture errors identically).
        let mut program = crate::hoist::hoist_nested_functions(program)?;
        crate::fnval::resolve(&mut program)?; // same first-class-function desugar Run gets
        crate::check::gate(&program)?; // Step refuses what Run refuses
        let module = VmFrame {
            func: "<module>".to_string(),
            work: vec![Task::Next(Rc::new(program), 0)],
            operands: Vec::new(),
            scope: HashMap::new(),
            line: 0,
            class_ctx: None,
        };
        let mut vm = Vm {
            frames: vec![module],
            funcs: HashMap::new(),
            classes: HashMap::new(),
            output: String::new(),
            status: Status::Finished,
            watchpoints: Vec::new(),
            watch_hit: None,
            last_return: None,
            stdin: std::collections::VecDeque::new(),
            file_sources: HashMap::new(),
            open_files: Vec::new(),
            rnd: rnd_seed(42),
        };
        vm.settle();
        Ok(vm)
    }

    /// Provide input lines for `input()` during debugging (one per line), so a
    /// student can step through an activity that reads input. The IDE calls this
    /// with its input box.
    pub fn set_stdin(&mut self, s: &str) {
        self.stdin = s.lines().map(|l| l.to_string()).collect();
    }

    /// Provide file content a program's `open(name)` may read, keyed by the
    /// name it opens — a lesson's bundled example files, say. Files not
    /// listed here are not openable (same shape as `set_stdin`: the debugger
    /// has no real filesystem, only what's staged before running).
    pub fn set_files(&mut self, files: HashMap<String, String>) {
        self.file_sources = files;
    }

    pub fn output(&self) -> &str {
        &self.output
    }
    pub fn status(&self) -> &Status {
        &self.status
    }
    pub fn is_paused(&self) -> bool {
        matches!(self.status, Status::Paused { .. })
    }
    pub fn current_line(&self) -> Option<usize> {
        match self.status {
            Status::Paused { line } => Some(line),
            _ => None,
        }
    }

    /// The call stack, innermost first: `(function name, current line)`.
    pub fn call_stack(&self) -> Vec<(String, usize)> {
        self.frames
            .iter()
            .rev()
            .map(|f| (f.func.clone(), f.line))
            .collect()
    }

    /// Variables in the current (innermost) frame, sorted, sans temporaries.
    pub fn variables(&self) -> Vec<(String, String)> {
        let Some(frame) = self.frames.last() else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = frame
            .scope
            .iter()
            .filter(|(n, _)| !n.contains('.'))
            .map(|(n, v)| (n.clone(), v.py_repr()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Evaluate a watch expression against the current frame (read-only; no calls
    /// in Phase 1). Returns its repr.
    pub fn eval_watch(&self, src: &str) -> Result<String, String> {
        let tokens = crate::lexer::lex(src).map_err(|e| e.to_string())?;
        let expr = crate::parser::parse_expression(&tokens).map_err(|e| e.to_string())?;
        self.watch_eval(&expr).map(|v| v.py_repr())
    }

    pub fn set_watchpoints(&mut self, srcs: &[String]) {
        self.watch_hit = None;
        self.watchpoints = srcs
            .iter()
            .filter_map(|s| {
                let toks = crate::lexer::lex(s).ok()?;
                let expr = crate::parser::parse_expression(&toks).ok()?;
                let last = self.watch_eval(&expr).ok().map(|v| v.py_repr());
                Some(Watchpoint {
                    src: s.clone(),
                    expr,
                    last,
                })
            })
            .collect();
    }

    pub fn watch_hit(&self) -> Option<(String, String, String)> {
        self.watch_hit.clone()
    }

    /// Execute one statement, stepping *into* any function it calls.
    pub fn step(&mut self) {
        if !self.is_paused() {
            return;
        }
        self.watch_hit = None;
        self.last_return = None;
        self.exec_current_statement();
        self.settle();
        self.check_watchpoints();
    }

    /// The value most recently returned to a caller during the last step, as its
    /// repr (e.g. after Step out / stepping past a `return`).
    pub fn last_return(&self) -> Option<String> {
        self.last_return.as_ref().map(Value::py_repr)
    }

    /// Step *over*: execute the current statement fully — running any functions
    /// it calls to completion — and stop at the next statement in this (or an
    /// outer) frame, rather than descending into the callee.
    pub fn step_over(&mut self) {
        if !self.is_paused() {
            return;
        }
        let depth = self.frames.len();
        self.step();
        while self.is_paused() && self.frames.len() > depth {
            self.step();
        }
    }

    /// Step *out*: run until the current function returns, pausing in the caller.
    /// At the top level this runs to the end.
    pub fn step_out(&mut self) {
        if !self.is_paused() {
            return;
        }
        let depth = self.frames.len();
        while self.is_paused() && self.frames.len() >= depth {
            self.step();
        }
    }

    /// Run until finished, errored, or about to execute a breakpoint line (after
    /// at least one step), or a watchpoint changes.
    pub fn run(&mut self, breakpoints: &[usize]) {
        self.watch_hit = None;
        let mut took = false;
        while self.is_paused() {
            if took
                && let Status::Paused { line } = self.status
                && breakpoints.contains(&line)
            {
                return;
            }
            self.step();
            took = true;
            if self.watch_hit.is_some() {
                return;
            }
        }
    }

    // --- internals --------------------------------------------------------

    fn top(&mut self) -> &mut VmFrame {
        self.frames.last_mut().expect("a frame")
    }

    fn push_op(&mut self, v: Value) {
        self.top().operands.push(v);
    }

    fn pop_op(&mut self) -> Result<Value, String> {
        self.top()
            .operands
            .pop()
            .ok_or_else(|| "internal: operand stack underflow".to_string())
    }

    fn push_task(&mut self, t: Task) {
        self.top().work.push(t);
    }

    /// Is the top frame poised at a statement to run? Returns its line.
    fn boundary_line(&self) -> Option<usize> {
        let f = self.frames.last()?;
        if let Some(Task::Next(stmts, idx)) = f.work.last()
            && *idx < stmts.len()
        {
            return Some(stmts[*idx].line);
        }
        None
    }

    /// Pump tasks until poised at the next statement, finished, or errored.
    fn settle(&mut self) {
        loop {
            if matches!(self.status, Status::Error { .. }) {
                return;
            }
            if let Some(line) = self.boundary_line() {
                // Keep the top frame's line current so the call stack is accurate
                // at the pause point (it otherwise only updates when a statement
                // is consumed).
                if let Some(f) = self.frames.last_mut() {
                    f.line = line;
                }
                self.status = Status::Paused { line };
                return;
            }
            if self.frames.is_empty() {
                self.status = Status::Finished;
                return;
            }
            if let Err(msg) = self.pump_one() {
                let line = self.frames.last().map(|f| f.line);
                self.status = Status::Error { line, message: msg };
                return;
            }
        }
    }

    /// Consume the boundary statement: set the line, schedule the continuation,
    /// and expand the statement into tasks.
    fn exec_current_statement(&mut self) {
        let frame = self.top();
        let Some(Task::Next(stmts, idx)) = frame.work.pop() else {
            return; // not at a boundary (shouldn't happen)
        };
        let stmt = stmts[idx].clone();
        frame.line = stmt.line;
        frame.work.push(Task::Next(stmts, idx + 1));
        if let Err(msg) = self.expand_stmt(&stmt) {
            self.status = Status::Error {
                line: Some(stmt.line),
                message: msg,
            };
        }
    }

    /// Push the tasks that carry out one statement.
    fn expand_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match &s.kind {
            StmtKind::Assign(name, e) | StmtKind::AnnAssign { name, value: e, .. } => {
                self.push_task(Task::Store(name.clone()));
                self.push_task(Task::Eval(Rc::new(e.clone())));
            }
            StmtKind::Expr(e) => {
                if let ExprKind::Call(name, args) = &e.kind
                    && name == "print"
                {
                    self.push_task(Task::Print(args.len()));
                    for a in args.iter().rev() {
                        // Instances display via __str__/__repr__ (a frame
                        // call), so convert each argument after evaluating it.
                        self.push_task(Task::ToDisplay);
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                } else {
                    self.push_task(Task::Pop);
                    self.push_task(Task::Eval(Rc::new(e.clone())));
                }
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                let mut branches: Vec<(Option<Expr>, Rc<Vec<Stmt>>)> =
                    vec![(Some(cond.clone()), Rc::new(body.clone()))];
                for (c, b) in elifs {
                    branches.push((Some(c.clone()), Rc::new(b.clone())));
                }
                if let Some(eb) = else_body {
                    branches.push((None, Rc::new(eb.clone())));
                }
                self.push_task(Task::IfChain(Rc::new(branches), 0));
            }
            StmtKind::While { cond, body } => {
                self.push_task(Task::WhileHead(
                    Rc::new(cond.clone()),
                    Rc::new(body.clone()),
                ));
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.push_task(Task::ForSetup(var.clone(), Rc::new(body.clone())));
                self.push_task(Task::Eval(Rc::new(step.clone())));
                self.push_task(Task::Eval(Rc::new(end.clone())));
                self.push_task(Task::Eval(Rc::new(start.clone())));
            }
            StmtKind::Break => self.push_task(Task::Break),
            StmtKind::Continue => self.push_task(Task::Continue),
            StmtKind::Pass => {}
            StmtKind::Return(opt) => {
                self.push_task(Task::Return);
                let e = match opt {
                    Some(e) => e.clone(),
                    None => Expr {
                        kind: ExprKind::NoneLit,
                        line: s.line,
                        span: (0, 0),
                    },
                };
                self.push_task(Task::Eval(Rc::new(e)));
            }
            StmtKind::Def {
                name,
                params,
                defaults,
                body,
                ..
            } => {
                self.funcs.insert(
                    name.clone(),
                    VmFunc {
                        params: params.clone(),
                        defaults: defaults.clone(),
                        body: Rc::new(body.clone()),
                    },
                );
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                self.push_task(Task::ForEachSetup(var.clone(), Rc::new(body.clone())));
                self.push_task(Task::Eval(Rc::new(iterable.clone())));
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                self.push_task(Task::StoreIndex);
                self.push_task(Task::Eval(Rc::new(value.clone())));
                self.push_task(Task::Eval(Rc::new(index.clone())));
                self.push_task(Task::Eval(Rc::new(target.clone())));
            }
            StmtKind::ClassDef {
                name,
                base,
                methods,
                class_vars,
            } => {
                let mut cvs = HashMap::new();
                for (vname, vexpr) in class_vars {
                    cvs.insert(vname.clone(), eval_const(vexpr)?);
                }
                let mut mm = HashMap::new();
                for m in methods {
                    let allowed = matches!(
                        m.name.as_str(),
                        "__init__"
                            | "__repr__"
                            | "__str__"
                            | "__add__"
                            | "__sub__"
                            | "__mul__"
                            | "__eq__"
                            | "__lt__"
                            | "__le__"
                            | "__gt__"
                            | "__ge__"
                            | "__len__"
                            | "__getitem__"
                            // Called via ordinary `.method()` syntax (the `with`
                            // statement desugars to plain MethodCall nodes), so
                            // no special operator-dunder wiring is needed here.
                            | "__enter__"
                            | "__exit__"
                    );
                    if m.name.starts_with("__") && m.name.ends_with("__") && !allowed {
                        return Err(format!(
                            "{} isn't dispatched in the step debugger yet — use Run",
                            m.name
                        ));
                    }
                    if m.params.first().map(String::as_str) != Some("self") {
                        return Err("a method's first parameter must be 'self'".to_string());
                    }
                    mm.insert(
                        m.name.clone(),
                        VmFunc {
                            params: m.params.clone(),
                            defaults: Vec::new(),
                            body: Rc::new(m.body.clone()),
                        },
                    );
                }
                if let Some(b) = base
                    && !self.classes.contains_key(b)
                {
                    return Err(format!("class '{name}' inherits from unknown '{b}'"));
                }
                self.classes.insert(
                    name.clone(),
                    VmClass {
                        base: base.clone(),
                        methods: mm,
                        class_vars: cvs,
                    },
                );
            }
            StmtKind::SetAttr { obj, attr, value } => {
                // Class-name base: write the class variable itself.
                if let ExprKind::Name(cn) = &obj.kind
                    && self.classes.contains_key(cn)
                    && self.lookup(cn).is_none()
                {
                    let mut owner = None;
                    let mut cur = Some(cn.clone());
                    let mut hops = 0;
                    while let Some(cname) = cur {
                        hops += 1;
                        if hops > self.classes.len() + 1 {
                            break;
                        }
                        let Some(c) = self.classes.get(&cname) else {
                            break;
                        };
                        if c.class_vars.contains_key(attr) {
                            owner = Some(cname);
                            break;
                        }
                        cur = c.base.clone();
                    }
                    let Some(owner) = owner else {
                        return Err(format!(
                            "'{attr}' isn't a class variable of '{cn}' — declare it in the class body first"
                        ));
                    };
                    self.push_task(Task::StoreClassVar(owner, attr.clone()));
                    self.push_task(Task::Eval(Rc::new(value.clone())));
                    return Ok(());
                }
                self.push_task(Task::StoreAttr(attr.clone()));
                self.push_task(Task::Eval(Rc::new(value.clone())));
                self.push_task(Task::Eval(Rc::new(obj.clone())));
            }
            StmtKind::UnpackAssign { targets, value } => {
                // `a, b = value` / `for k, v in …`, and also the swap idiom
                // `a[i], a[j] = a[j], a[i]` — see Task::Unpack for how a
                // non-Name target is handled.
                self.push_task(Task::Unpack(targets.clone()));
                self.push_task(Task::Eval(Rc::new(value.clone())));
            }
            StmtKind::Import(names) => {
                check_import(names)?;
            }
        }
        Ok(())
    }

    /// Execute one task.
    fn pump_one(&mut self) -> Result<(), String> {
        // An empty frame returns None to its caller (implicit fall-off-the-end).
        if self.top().work.is_empty() {
            self.return_value(Value::None);
            return Ok(());
        }
        let task = self.top().work.pop().unwrap();
        match task {
            Task::Next(stmts, idx) => {
                // Only reached for an exhausted block (idx >= len); a live one is
                // a boundary handled by step(). Exhausted -> just drop it.
                debug_assert!(idx >= stmts.len());
            }
            Task::Eval(e) => self.eval_task(&e)?,
            Task::Bin(op) => {
                let r = self.pop_op()?;
                let l = self.pop_op()?;
                if matches!(op, BinOp::Is | BinOp::IsNot) {
                    // Never dunder-dispatched (the parser only allows this
                    // against `None`, and identity against the singleton
                    // doesn't go through `__eq__`): a raw value-shape check.
                    let both_none = matches!(l, Value::None) && matches!(r, Value::None);
                    let is = if op == BinOp::Is {
                        both_none
                    } else {
                        !both_none
                    };
                    self.push_op(Value::Bool(is));
                } else if matches!(l, Value::Object(_)) || matches!(r, Value::Object(_)) {
                    self.bin_dunder(op, l, r)?;
                } else {
                    self.push_op(apply_bin(op, &l, &r)?);
                }
            }
            Task::Unary(op) => {
                let v = self.pop_op()?;
                self.push_op(apply_unary(op, v)?);
            }
            Task::AndThen(b) => {
                let keep = self.top().operands.last().cloned().unwrap_or(Value::None);
                if keep.truthy() {
                    self.pop_op()?;
                    self.push_task(Task::Eval(b));
                }
            }
            Task::OrElse(b) => {
                let keep = self.top().operands.last().cloned().unwrap_or(Value::None);
                if !keep.truthy() {
                    self.pop_op()?;
                    self.push_task(Task::Eval(b));
                }
            }
            Task::Ternary(then, orelse) => {
                let c = self.pop_op()?;
                self.push_task(Task::Eval(if c.truthy() { then } else { orelse }));
            }
            Task::Store(name) => {
                let v = self.pop_op()?;
                self.top().scope.insert(name, v);
            }
            Task::Push(v) => self.push_op(v),
            Task::Unpack(targets) => {
                let v = self.pop_op()?;
                let items = to_elements(&v)?;
                if items.len() != targets.len() {
                    return Err(crate::messages::VALUE_UNPACK_COUNT.text.into());
                }
                for (target, item) in targets.into_iter().zip(items) {
                    match target.kind {
                        ExprKind::Name(n) => {
                            self.top().scope.insert(n, item);
                        }
                        ExprKind::Index(obj, idx) => {
                            self.push_task(Task::StoreIndex);
                            self.push_task(Task::Push(item));
                            self.push_task(Task::Eval(Rc::new(*idx)));
                            self.push_task(Task::Eval(Rc::new(*obj)));
                        }
                        _ => {
                            return Err("an unpacking target must be a variable or an index here"
                                .to_string());
                        }
                    }
                }
            }
            Task::SortedKw => {
                let reverse = self.pop_op()?.truthy();
                let seq = self.pop_op()?;
                self.push_op(sorted_values(&seq, reverse)?);
            }
            Task::MathCall(func, n) => {
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                self.push_op(math_call(&func, &args)?);
            }
            Task::RandomCall(func, n) => {
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                let v = random_call(&mut self.rnd, &func, &mut args)?;
                self.push_op(v);
            }
            Task::Print(n) => {
                let mut vals = Vec::with_capacity(n);
                for _ in 0..n {
                    vals.push(self.pop_op()?);
                }
                vals.reverse();
                let parts: Vec<String> = vals.iter().map(Value::py_str).collect();
                self.output.push_str(&parts.join(" "));
                self.output.push('\n');
            }
            Task::Pop => {
                self.pop_op()?;
            }
            Task::Call(name, argc) => self.do_call(&name, argc)?,
            Task::IfChain(branches, idx) => {
                if idx < branches.len() {
                    match &branches[idx].0 {
                        None => {
                            let body = branches[idx].1.clone();
                            self.push_task(Task::Next(body, 0));
                        }
                        Some(cond) => {
                            self.push_task(Task::IfTest(branches.clone(), idx));
                            self.push_task(Task::Eval(Rc::new(cond.clone())));
                        }
                    }
                }
            }
            Task::IfTest(branches, idx) => {
                let v = self.pop_op()?;
                if v.truthy() {
                    let body = branches[idx].1.clone();
                    self.push_task(Task::Next(body, 0));
                } else {
                    self.push_task(Task::IfChain(branches, idx + 1));
                }
            }
            Task::WhileHead(cond, body) => {
                self.push_task(Task::WhileTest(cond.clone(), body));
                self.push_task(Task::Eval(cond));
            }
            Task::WhileTest(cond, body) => {
                let v = self.pop_op()?;
                if v.truthy() {
                    self.push_task(Task::WhileHead(cond, body.clone()));
                    self.push_task(Task::Next(body, 0));
                }
            }
            Task::ForSetup(var, body) => {
                let step = as_int(&self.pop_op()?).ok_or("range() step must be an integer")?;
                let stop = as_int(&self.pop_op()?).ok_or("range() stop must be an integer")?;
                let next = as_int(&self.pop_op()?).ok_or("range() start must be an integer")?;
                if step == 0 {
                    return Err("range() step must not be zero".to_string());
                }
                self.push_task(Task::ForHead {
                    var,
                    next,
                    stop,
                    step,
                    body,
                });
            }
            Task::ForHead {
                var,
                next,
                stop,
                step,
                body,
            } => {
                let go = (step > 0 && next < stop) || (step < 0 && next > stop);
                if go {
                    self.top().scope.insert(var.clone(), Value::Int(next));
                    self.push_task(Task::ForHead {
                        var,
                        next: next + step,
                        stop,
                        step,
                        body: body.clone(),
                    });
                    self.push_task(Task::Next(body, 0));
                }
            }
            Task::Break => {
                while let Some(t) = self.top().work.pop() {
                    if t.is_loop() {
                        break;
                    }
                }
            }
            Task::Continue => {
                while let Some(t) = self.top().work.last() {
                    if t.is_loop() {
                        break;
                    }
                    self.top().work.pop();
                }
            }
            Task::Return => {
                let v = self.pop_op().unwrap_or(Value::None);
                self.return_value(v);
            }
            Task::BuildList(n) => {
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.pop_op()?);
                }
                items.reverse();
                self.push_op(Value::list(items));
            }
            Task::BuildTuple(n) => {
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.pop_op()?);
                }
                items.reverse();
                self.push_op(Value::Tuple(items));
            }
            Task::BuildDict(n) => {
                let mut flat = Vec::with_capacity(2 * n);
                for _ in 0..(2 * n) {
                    flat.push(self.pop_op()?);
                }
                flat.reverse(); // now [k0, v0, k1, v1, ...]
                let mut pairs = Vec::with_capacity(n);
                let mut i = 0;
                while i + 1 < flat.len() {
                    pairs.push((flat[i].clone(), flat[i + 1].clone()));
                    i += 2;
                }
                self.push_op(Value::dict(pairs));
            }
            Task::IndexGet => {
                let idx = self.pop_op()?;
                let obj = self.pop_op()?;
                if let Value::Object(o) = &obj {
                    let (owner, _) = self
                        .resolve_method(&o.class, "__getitem__")
                        .ok_or("this object can't be indexed (no __getitem__)")?;
                    self.call_method_frame(&owner, "__getitem__", obj.clone(), vec![idx])?;
                } else {
                    self.push_op(index_get(&obj, &idx)?);
                }
            }
            Task::Slice(has_start, has_stop, has_step) => {
                // Pop top-first: step, stop, start (only the present ones), then
                // the target underneath.
                let mut int_bound = |present: bool, what: &str| -> Result<Option<i64>, String> {
                    if present {
                        Ok(Some(as_int(&self.pop_op()?).ok_or_else(|| {
                            format!("slice {what} must be an integer")
                        })?))
                    } else {
                        Ok(None)
                    }
                };
                let step = int_bound(has_step, "step")?;
                let stop = int_bound(has_stop, "stop")?;
                let start = int_bound(has_start, "start")?;
                let target = self.pop_op()?;
                self.push_op(slice_value(&target, start, stop, step)?);
            }
            Task::MethodOnName(name, method, argc) => {
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                // A file's mutable state (the read cursor) lives on the Vm's
                // own side table, not inside the `Value` — see `Value::File`.
                if let Some(Value::File(idx)) = self.lookup(&name) {
                    let result = self.file_method(idx, &method, &args)?;
                    self.push_op(result);
                } else if let Some(recv @ Value::Object(_)) = self.lookup(&name) {
                    let owner = match &recv {
                        Value::Object(o) => self
                            .resolve_method(&o.class, &method)
                            .map(|(w, _)| w)
                            .ok_or_else(|| {
                                format!("this object has no method '{method}' (or wrong arguments)")
                            })?,
                        _ => unreachable!(),
                    };
                    self.call_method_frame(&owner, &method, recv, args)?;
                } else {
                    let result = list_method(self.top(), &name, &method, args)?;
                    self.push_op(result);
                }
            }
            Task::ForEachSetup(var, body) => {
                let items = match self.pop_op()? {
                    Value::List(v) | Value::Set(v) => v.borrow().clone(),
                    Value::Tuple(v) => v,
                    Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                    Value::Dict(d) => d.borrow().iter().map(|(k, _)| k.clone()).collect(),
                    other => {
                        return Err(fill_msg(
                            &crate::messages::TYPE_FOR_NOT_ITERABLE,
                            &[("type", &type_label(&other))],
                        ));
                    }
                };
                self.push_task(Task::ForEachHead {
                    var,
                    items: Rc::new(items),
                    idx: 0,
                    body,
                });
            }
            Task::ForEachHead {
                var,
                items,
                idx,
                body,
            } => {
                if idx < items.len() {
                    self.top().scope.insert(var.clone(), items[idx].clone());
                    self.push_task(Task::ForEachHead {
                        var,
                        items: items.clone(),
                        idx: idx + 1,
                        body: body.clone(),
                    });
                    self.push_task(Task::Next(body, 0));
                }
            }
            Task::StoreIndex => {
                let value = self.pop_op()?;
                let index = self.pop_op()?;
                let target = self.pop_op()?;
                assign_index_value(&target, index, value)?;
            }
            Task::ReplaceTop(v) => {
                self.pop_op()?; // the discarded __init__ return value
                self.push_op(v);
            }
            Task::AttrGet(attr) => {
                let v = self.pop_op()?;
                let Value::Object(o) = &v else {
                    return Err(fill_msg(
                        &crate::messages::ATTR_MISSING,
                        &[("type", &type_label(&v)), ("name", &attr)],
                    ));
                };
                // Instance attrs shadow class variables; the fallback walks
                // the inheritance chain (nearest class first).
                if let Some(got) = o.attrs.borrow().get(&attr).cloned() {
                    self.push_op(got);
                    return Ok(());
                }
                let mut cur = Some(o.class.clone());
                let mut hops = 0;
                while let Some(cname) = cur {
                    hops += 1;
                    if hops > self.classes.len() + 1 {
                        break;
                    }
                    let Some(c) = self.classes.get(&cname) else {
                        break;
                    };
                    if let Some(cv) = c.class_vars.get(&attr) {
                        let cv = cv.clone();
                        self.push_op(cv);
                        return Ok(());
                    }
                    if c.methods.contains_key(&attr) {
                        return Err(fill_msg(
                            &crate::messages::TYPE_METHOD_AS_VALUE,
                            &[("name", &attr)],
                        ));
                    }
                    cur = c.base.clone();
                }
                return Err(fill_msg(
                    &crate::messages::ATTR_MISSING,
                    &[("type", &o.class), ("name", &attr)],
                ));
            }
            Task::StoreAttr(attr) => {
                let value = self.pop_op()?;
                let objv = self.pop_op()?;
                let Value::Object(o) = &objv else {
                    return Err(fill_msg(
                        &crate::messages::ATTR_MISSING,
                        &[("type", &type_label(&objv)), ("name", &attr)],
                    ));
                };
                o.attrs.borrow_mut().insert(attr, value);
            }
            Task::ToDisplay => {
                let v = self.pop_op()?;
                if let Value::Object(o) = &v {
                    let shown = self
                        .resolve_method(&o.class, "__str__")
                        .map(|(w, _)| (w, "__str__"))
                        .or_else(|| {
                            self.resolve_method(&o.class, "__repr__")
                                .map(|(w, _)| (w, "__repr__"))
                        });
                    if let Some((owner, m)) = shown {
                        self.call_method_frame(&owner, m, v.clone(), Vec::new())?;
                        return Ok(());
                    }
                }
                self.push_op(v);
            }
            Task::CallMethodValue(method, argc) => {
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                let mut recv = self.pop_op()?;
                let Value::Object(o) = &recv else {
                    // Not an instance: a literal/temporary receiver (e.g.
                    // `"abc".upper()`, `[3, 1].sort()`) — mutation is
                    // harmless since the value is discarded after.
                    let out = container_method(&mut recv, &method, args)?;
                    self.push_op(out);
                    return Ok(());
                };
                let (owner, _) = self.resolve_method(&o.class, &method).ok_or_else(|| {
                    format!("this object has no method '{method}' (or wrong arguments)")
                })?;
                self.call_method_frame(&owner, &method, recv.clone(), args)?;
            }
            Task::CallMethodOnAttr(attr, method, argc) => {
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                let basev = self.pop_op()?;
                let Value::Object(o) = &basev else {
                    return Err(format!(
                        "only objects have attributes, not {}",
                        type_name(&basev)
                    ));
                };
                // An instance attribute dispatches through its class; a
                // container attribute mutates in place inside the cell.
                let attr_is_obj = matches!(o.attrs.borrow().get(&attr), Some(Value::Object(_)));
                if attr_is_obj {
                    let recv = o.attrs.borrow().get(&attr).cloned().unwrap();
                    let Value::Object(inner) = &recv else {
                        unreachable!()
                    };
                    let (owner, _) =
                        self.resolve_method(&inner.class, &method).ok_or_else(|| {
                            fill_msg(
                                &crate::messages::ATTR_MISSING,
                                &[("type", &inner.class), ("name", &method)],
                            )
                        })?;
                    self.call_method_frame(&owner, &method, recv.clone(), args)?;
                } else {
                    let mut cell = o.attrs.borrow_mut();
                    let slot = cell.get_mut(&attr).ok_or_else(|| {
                        fill_msg(
                            &crate::messages::ATTR_MISSING,
                            &[("type", &o.class), ("name", &attr)],
                        )
                    })?;
                    let out = container_method(slot, &method, args)?;
                    drop(cell);
                    self.push_op(out);
                }
            }
            Task::StoreClassVar(owner, attr) => {
                let v = self.pop_op()?;
                if let Some(c) = self.classes.get_mut(&owner) {
                    c.class_vars.insert(attr, v);
                }
            }
            Task::CallSuper(owner, method, argc) => {
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                let selfv = self
                    .lookup("self")
                    .ok_or("super() only works inside a method")?;
                self.call_method_frame(&owner, &method, selfv, args)?;
            }
        }
        Ok(())
    }

    /// Expand one expression into evaluation tasks (or push a literal directly).
    fn eval_task(&mut self, e: &Expr) -> Result<(), String> {
        match &e.kind {
            ExprKind::Int(n) => self.push_op(Value::Int(*n)),
            ExprKind::Float(f) => self.push_op(Value::Float(*f)),
            ExprKind::Bool(b) => self.push_op(Value::Bool(*b)),
            ExprKind::Str(s) => self.push_op(Value::Str(s.clone())),
            ExprKind::NoneLit => self.push_op(Value::None),
            ExprKind::Name(n) => {
                let v = self
                    .lookup(n)
                    .ok_or_else(|| format!("name '{n}' is not defined"))?;
                self.push_op(v);
            }
            ExprKind::Unary(op, inner) => {
                self.push_task(Task::Unary(*op));
                self.push_task(Task::Eval(Rc::new((**inner).clone())));
            }
            ExprKind::Bin(BinOp::And, a, b) => {
                self.push_task(Task::AndThen(Rc::new((**b).clone())));
                self.push_task(Task::Eval(Rc::new((**a).clone())));
            }
            ExprKind::Bin(BinOp::Or, a, b) => {
                self.push_task(Task::OrElse(Rc::new((**b).clone())));
                self.push_task(Task::Eval(Rc::new((**a).clone())));
            }
            ExprKind::IfExp { cond, then, orelse } => {
                self.push_task(Task::Ternary(
                    Rc::new((**then).clone()),
                    Rc::new((**orelse).clone()),
                ));
                self.push_task(Task::Eval(Rc::new((**cond).clone())));
            }
            ExprKind::Bin(op, a, b) => {
                self.push_task(Task::Bin(*op));
                self.push_task(Task::Eval(Rc::new((**b).clone())));
                self.push_task(Task::Eval(Rc::new((**a).clone())));
            }
            ExprKind::Call(name, args) => {
                // `sorted(seq, reverse=<expr>)`: evaluate seq (bottom) then the
                // reverse flag (top); SortedKw pops both.
                if let Some((seq_e, rev_e)) = sorted_reverse_args(name, args) {
                    self.push_task(Task::SortedKw);
                    self.push_task(Task::Eval(Rc::new(rev_e.clone())));
                    self.push_task(Task::Eval(Rc::new(seq_e.clone())));
                    return Ok(());
                }
                self.push_task(Task::Call(name.clone(), args.len()));
                for a in args.iter().rev() {
                    self.push_task(Task::Eval(Rc::new(a.clone())));
                }
            }
            ExprKind::List(items) => {
                self.push_task(Task::BuildList(items.len()));
                for it in items.iter().rev() {
                    self.push_task(Task::Eval(Rc::new(it.clone())));
                }
            }
            ExprKind::Tuple(items) => {
                self.push_task(Task::BuildTuple(items.len()));
                for it in items.iter().rev() {
                    self.push_task(Task::Eval(Rc::new(it.clone())));
                }
            }
            ExprKind::Dict(pairs) => {
                self.push_task(Task::BuildDict(pairs.len()));
                // Push so each pair evaluates key-then-value, in source order.
                for (k, v) in pairs.iter().rev() {
                    self.push_task(Task::Eval(Rc::new(v.clone())));
                    self.push_task(Task::Eval(Rc::new(k.clone())));
                }
            }
            ExprKind::Index(obj, idx) => {
                self.push_task(Task::IndexGet);
                self.push_task(Task::Eval(Rc::new((**idx).clone())));
                self.push_task(Task::Eval(Rc::new((**obj).clone())));
            }
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                // Evaluate target, then present bounds in source order (so the
                // op stack holds target, start, stop, step bottom-to-top); the
                // Slice task pops them back off top-first.
                self.push_task(Task::Slice(start.is_some(), stop.is_some(), step.is_some()));
                if let Some(e) = step {
                    self.push_task(Task::Eval(Rc::new((**e).clone())));
                }
                if let Some(e) = stop {
                    self.push_task(Task::Eval(Rc::new((**e).clone())));
                }
                if let Some(e) = start {
                    self.push_task(Task::Eval(Rc::new((**e).clone())));
                }
                self.push_task(Task::Eval(Rc::new((**obj).clone())));
            }
            ExprKind::Attr(obj, attr) => {
                // `math.pi` / `math.e` / `math.tau` — module constant, unless a
                // variable named `math` shadows the module.
                if let ExprKind::Name(m) = &obj.kind
                    && m == "math"
                    && self.lookup(m).is_none()
                {
                    let c = math_const(attr)
                        .ok_or_else(|| format!("math has no attribute '{attr}'"))?;
                    self.push_op(c);
                    return Ok(());
                }
                // `Counter.limit` — a class-name base resolves against the
                // registry (a same-named variable shadows, like CPython).
                if let ExprKind::Name(cn) = &obj.kind
                    && self.classes.contains_key(cn)
                    && self.lookup(cn).is_none()
                {
                    let mut cur = Some(cn.clone());
                    let mut hops = 0;
                    while let Some(cname) = cur {
                        hops += 1;
                        if hops > self.classes.len() + 1 {
                            break;
                        }
                        let Some(c) = self.classes.get(&cname) else {
                            break;
                        };
                        if let Some(v) = c.class_vars.get(attr) {
                            let v = v.clone();
                            self.push_op(v);
                            return Ok(());
                        }
                        if c.methods.contains_key(attr) {
                            return Err(
                                "a method isn't a value yet — call it: obj.method(...)".to_string()
                            );
                        }
                        cur = c.base.clone();
                    }
                    return Err(format!("class '{cn}' has no attribute '{attr}'"));
                }
                self.push_task(Task::AttrGet(attr.clone()));
                self.push_task(Task::Eval(Rc::new((**obj).clone())));
            }
            ExprKind::MethodCall(obj, method, args) => {
                // `super().m(...)` — resolved from the running method's class.
                if let ExprKind::Call(n, cargs) = &obj.kind
                    && n == "super"
                    && cargs.is_empty()
                {
                    let class = self
                        .frames
                        .last()
                        .and_then(|f| f.class_ctx.clone())
                        .ok_or("super() only works inside a method")?;
                    let base = self
                        .classes
                        .get(&class)
                        .and_then(|c| c.base.clone())
                        .ok_or_else(|| format!("'{class}' has no base class for super()"))?;
                    let (owner, _) = self
                        .resolve_method(&base, method)
                        .ok_or_else(|| format!("super() found no method '{method}'"))?;
                    self.push_task(Task::CallSuper(owner, method.clone(), args.len()));
                    for a in args.iter().rev() {
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                    return Ok(());
                }
                // `math.sqrt(x)` / `random.randint(...)` — a module function
                // (unless shadowed by a variable). Arity checked in the call.
                if let ExprKind::Name(m) = &obj.kind
                    && (m == "math" || m == "random")
                    && self.lookup(m).is_none()
                {
                    // shuffle on a named list mutates through the scope NOW
                    // (the append precedent) — everything else goes through a
                    // task over evaluated args.
                    if m == "random"
                        && method == "shuffle"
                        && let [arg] = args.as_slice()
                        && let ExprKind::Name(xs) = &arg.kind
                    {
                        let items = match self.frames.last_mut().and_then(|f| f.scope.get_mut(xs)) {
                            Some(Value::List(items)) => items.clone(),
                            _ => return Err("random.shuffle() needs a list variable".into()),
                        };
                        let mut items = items.borrow_mut();
                        let mut i = items.len().saturating_sub(1);
                        while i > 0 {
                            let j = (rnd_next(&mut self.rnd) % (i as u64 + 1)) as usize;
                            items.swap(i, j);
                            i -= 1;
                        }
                        drop(items);
                        self.push_op(Value::None);
                        return Ok(());
                    }
                    let task = if m == "random" {
                        Task::RandomCall(method.clone(), args.len())
                    } else {
                        Task::MathCall(method.clone(), args.len())
                    };
                    self.push_task(task);
                    for a in args.iter().rev() {
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                    return Ok(());
                }
                if let ExprKind::Name(recv) = &obj.kind {
                    self.push_task(Task::MethodOnName(recv.clone(), method.clone(), args.len()));
                    for a in args.iter().rev() {
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                } else if let ExprKind::Attr(base, attr) = &obj.kind {
                    // `self.tricks.append(t)` — the receiver lives in an
                    // instance's attrs cell, so mutations write back there.
                    self.push_task(Task::CallMethodOnAttr(
                        attr.clone(),
                        method.clone(),
                        args.len(),
                    ));
                    for a in args.iter().rev() {
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                    self.push_task(Task::Eval(Rc::new((**base).clone())));
                } else {
                    // General receiver: evaluate it, then dispatch by value —
                    // instances only (lists still need the name-based path).
                    self.push_task(Task::CallMethodValue(method.clone(), args.len()));
                    for a in args.iter().rev() {
                        self.push_task(Task::Eval(Rc::new(a.clone())));
                    }
                    self.push_task(Task::Eval(Rc::new((**obj).clone())));
                }
            }
            _ => {
                return Err(format!(
                    "{} isn't in the step debugger's call-stack mode yet — use Run",
                    describe_expr(&e.kind)
                ));
            }
        }
        Ok(())
    }

    /// Resolve a name: current frame, then module globals (read fallback).
    fn lookup(&self, name: &str) -> Option<Value> {
        let top = self.frames.last()?;
        if let Some(v) = top.scope.get(name) {
            return Some(v.clone());
        }
        if self.frames.len() > 1 {
            return self.frames[0].scope.get(name).cloned();
        }
        None
    }

    /// Perform a call: user function -> push a new frame (step-into); builtin ->
    /// compute and push the result.
    fn do_call(&mut self, name: &str, argc: usize) -> Result<(), String> {
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(self.pop_op()?);
        }
        args.reverse();

        // Construction: `Dog(args)` — build the instance, run __init__ (a
        // real frame, so kids step INTO their constructor), then deliver the
        // instance as the call's value (ReplaceTop discards __init__'s None).
        if self.classes.contains_key(name) {
            let obj = Value::Object(Rc::new(VmObj {
                class: name.to_string(),
                attrs: std::cell::RefCell::new(HashMap::new()),
            }));
            if let Some((owner, f)) = self.resolve_method(name, "__init__") {
                if args.len() != f.params.len() - 1 {
                    return Err(format!(
                        "{name}() takes {} argument(s) but {} were given",
                        f.params.len() - 1,
                        args.len()
                    ));
                }
                self.push_task(Task::ReplaceTop(obj.clone()));
                return self.call_method_frame(&owner, "__init__", obj, args);
            }
            if !args.is_empty() {
                return Err(format!(
                    "{name}() takes 0 arguments but {} were given",
                    args.len()
                ));
            }
            self.push_op(obj);
            return Ok(());
        }
        // len()/str() on an instance dispatch __len__ / __str__-then-__repr__.
        if name == "len"
            && args.len() == 1
            && let Value::Object(o) = &args[0]
        {
            let (owner, _) = self
                .resolve_method(&o.class, "__len__")
                .ok_or_else(|| fill_msg(&crate::messages::TYPE_NO_LEN, &[("type", &o.class)]))?;
            return self.call_method_frame(&owner, "__len__", args[0].clone(), Vec::new());
        }
        if name == "str"
            && args.len() == 1
            && let Value::Object(o) = &args[0]
        {
            let shown = self
                .resolve_method(&o.class, "__str__")
                .map(|(w, _)| (w, "__str__"))
                .or_else(|| {
                    self.resolve_method(&o.class, "__repr__")
                        .map(|(w, _)| (w, "__repr__"))
                });
            if let Some((owner, m)) = shown {
                return self.call_method_frame(&owner, m, args[0].clone(), Vec::new());
            }
            self.push_op(Value::Str(args[0].py_repr()));
            return Ok(());
        }
        if let Some(f) = self.funcs.get(name).cloned() {
            let nparams = f.params.len();
            let nrequired = nparams - f.defaults.len();
            if args.len() < nrequired || args.len() > nparams {
                return Err(format!(
                    "{name}() takes {nparams} argument(s) but {} were given",
                    args.len()
                ));
            }
            let mut scope = HashMap::new();
            for (i, p) in f.params.iter().enumerate() {
                let v = if i < args.len() {
                    args[i].clone()
                } else {
                    eval_const(&f.defaults[i - nrequired])?
                };
                scope.insert(p.clone(), v);
            }
            let line = f.body.first().map(|s| s.line).unwrap_or(0);
            self.frames.push(VmFrame {
                func: name.to_string(),
                work: vec![Task::Next(f.body, 0)],
                operands: Vec::new(),
                scope,
                line,
                class_ctx: None,
            });
            Ok(())
        } else if name == "input" {
            // Host INPUT capability in the debugger: print the optional prompt,
            // then serve a pre-supplied line (set_stdin), or "" when exhausted.
            if let [Value::Str(p)] = args.as_slice() {
                self.output.push_str(p);
            }
            let line = self.stdin.pop_front().unwrap_or_default();
            self.push_op(Value::Str(line));
            Ok(())
        } else if name == "open" {
            // `open(name[, mode])` — the debugger has no real filesystem, only
            // what `set_files` staged (same shape as `input`'s `set_stdin`)
            // plus whatever this run has itself written and closed. Read
            // mode ("r", the default) needs the name pre-staged OR already
            // written-and-closed earlier in this same run (see `close`'s
            // `file_sources` commit below); write mode ("w") always succeeds
            // and starts from an empty buffer. `Vm::file_writes` still
            // surfaces every write-mode file's final content after the run,
            // for the embedder, independent of this in-run commit.
            let filename = match args.first() {
                Some(Value::Str(s)) => s.clone(),
                _ => return Err("open() needs a text filename".to_string()),
            };
            let mode_str = match args.get(1) {
                Some(Value::Str(m)) => m.as_str(),
                None => "r",
                Some(_) => return Err("open()'s mode must be text".to_string()),
            };
            let (mode, content) = match mode_str {
                "r" => {
                    let Some(content) = self.file_sources.get(&filename) else {
                        return Err(format!(
                            "can't open '{filename}' — it isn't one of this program's \
                             available files"
                        ));
                    };
                    (FileMode::Read, content.clone())
                }
                "w" => (FileMode::Write, String::new()),
                other => {
                    return Err(format!(
                        "open() mode '{other}' isn't supported yet — use 'r' or 'w'"
                    ));
                }
            };
            self.open_files.push(VmFile::new(filename, &content, mode));
            self.push_op(Value::File(self.open_files.len() - 1));
            Ok(())
        } else {
            let v = call_builtin(name, &args)?;
            self.push_op(v);
            Ok(())
        }
    }

    /// `.readline()` / `.read()` / `.write()` / `.close()` / `__enter__` /
    /// `__exit__` on an open file (`Value::File` indexes here, not into the
    /// value itself — see its doc comment).
    fn file_method(&mut self, idx: usize, method: &str, args: &[Value]) -> Result<Value, String> {
        // `__enter__` returns the same handle — not touched through `f`.
        if method == "__enter__" && args.is_empty() {
            return Ok(Value::File(idx));
        }
        let f = self
            .open_files
            .get_mut(idx)
            .ok_or("this file handle doesn't exist")?;
        match (method, args) {
            ("readline", []) => {
                if f.closed {
                    return Err("this file is already closed".to_string());
                }
                if f.mode != FileMode::Read {
                    return Err("this file isn't open for reading".to_string());
                }
                if f.cursor < f.lines.len() {
                    let line = f.lines[f.cursor].clone();
                    f.cursor += 1;
                    Ok(Value::Str(line))
                } else {
                    Ok(Value::Str(String::new()))
                }
            }
            ("read", []) => {
                if f.closed {
                    return Err("this file is already closed".to_string());
                }
                if f.mode != FileMode::Read {
                    return Err("this file isn't open for reading".to_string());
                }
                let rest = f.lines[f.cursor..].concat();
                f.cursor = f.lines.len();
                Ok(Value::Str(rest))
            }
            ("write", [Value::Str(text)]) => {
                if f.closed {
                    return Err("this file is already closed".to_string());
                }
                if f.mode != FileMode::Write {
                    return Err("this file isn't open for writing".to_string());
                }
                f.writes.push_str(text);
                Ok(Value::Int(text.chars().count() as i64))
            }
            ("write", [_]) => Err("write() needs text".to_string()),
            ("close", []) => {
                f.closed = true;
                if f.mode == FileMode::Write {
                    let (name, writes) = (f.name.clone(), f.writes.clone());
                    self.file_sources.insert(name, writes);
                }
                Ok(Value::None)
            }
            ("__exit__", [_, _, _]) => {
                f.closed = true;
                if f.mode == FileMode::Write {
                    let (name, writes) = (f.name.clone(), f.writes.clone());
                    self.file_sources.insert(name, writes);
                }
                Ok(Value::None)
            }
            (m, _) => Err(format!("a file object has no method '{m}' yet")),
        }
    }

    /// What every file opened in write mode accumulated, keyed by name —
    /// read back after the run. The debugger never commits this anywhere
    /// real; whatever embeds it (the IDE, a test) decides where it lands.
    pub fn file_writes(&self) -> HashMap<String, String> {
        // Multiple opens of the same name in write mode overwrite — later
        // open wins, matching real Python's truncate-on-open "w" semantics.
        let mut out = HashMap::new();
        for f in &self.open_files {
            if f.mode == FileMode::Write {
                out.insert(f.name.clone(), f.writes.clone());
            }
        }
        out
    }

    /// Pop the current frame, delivering `v` to the caller (or finishing the
    /// program if the module frame returns).
    fn return_value(&mut self, v: Value) {
        self.frames.pop();
        if let Some(caller) = self.frames.last_mut() {
            caller.operands.push(v.clone());
            self.last_return = Some(v); // a real return to a caller
        }
        // else: module frame ended -> settle() will report Finished.
    }

    /// Where `class.method` resolves: the OWNING class and its function,
    /// walking the single-inheritance chain (hop-capped, so a registration
    /// cycle can't loop).
    fn resolve_method(&self, class: &str, method: &str) -> Option<(String, VmFunc)> {
        let mut cur = class.to_string();
        for _ in 0..=self.classes.len() {
            let c = self.classes.get(&cur)?;
            if let Some(f) = c.methods.get(method) {
                return Some((cur, f.clone()));
            }
            cur = c.base.clone()?;
        }
        None
    }

    /// Push a frame running `owner.method` with `self` + `args` bound —
    /// construction, dispatch, dunders, and super() all funnel through here,
    /// so every one of them is step-into-able like a plain function call.
    fn call_method_frame(
        &mut self,
        owner: &str,
        method: &str,
        selfv: Value,
        args: Vec<Value>,
    ) -> Result<(), String> {
        let f = self
            .classes
            .get(owner)
            .and_then(|c| c.methods.get(method))
            .cloned()
            .ok_or_else(|| {
                fill_msg(
                    &crate::messages::ATTR_MISSING,
                    &[("type", owner), ("name", method)],
                )
            })?;
        if args.len() != f.params.len() - 1 {
            return Err(format!(
                "{method}() takes {} argument(s) but {} were given",
                f.params.len() - 1,
                args.len()
            ));
        }
        let mut scope = HashMap::new();
        scope.insert(f.params[0].clone(), selfv);
        for (i, a) in args.into_iter().enumerate() {
            scope.insert(f.params[i + 1].clone(), a);
        }
        let line = f.body.first().map(|s| s.line).unwrap_or(0);
        self.frames.push(VmFrame {
            func: format!("{owner}.{method}"),
            work: vec![Task::Next(f.body, 0)],
            operands: Vec::new(),
            scope,
            line,
            class_ctx: Some(owner.to_string()),
        });
        Ok(())
    }

    /// A binary operator with an instance operand: dispatch the dunder.
    /// `==`/`!=` try direct, then reflected, then identity (`!=` negates via
    /// a pre-pushed `not` task, so the method's own result flows through it).
    fn bin_dunder(&mut self, op: BinOp, l: Value, r: Value) -> Result<(), String> {
        let dunder = match op {
            BinOp::Add => "__add__",
            BinOp::Sub => "__sub__",
            BinOp::Mul => "__mul__",
            BinOp::Lt => "__lt__",
            BinOp::Le => "__le__",
            BinOp::Gt => "__gt__",
            BinOp::Ge => "__ge__",
            BinOp::Eq | BinOp::Ne => "__eq__",
            _ => return Err(expected_number_bin(op, &l, &r)),
        };
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            if matches!(op, BinOp::Ne) {
                self.push_task(Task::Unary(UnOp::Not));
            }
            if let Value::Object(o) = &l
                && let Some((owner, _)) = self.resolve_method(&o.class, "__eq__")
            {
                return self.call_method_frame(&owner, "__eq__", l.clone(), vec![r]);
            }
            if let Value::Object(o) = &r
                && let Some((owner, _)) = self.resolve_method(&o.class, "__eq__")
            {
                return self.call_method_frame(&owner, "__eq__", r.clone(), vec![l]);
            }
            // Identity fallback (both instances: same allocation; mixed: False).
            let same = match (&l, &r) {
                (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
                _ => false,
            };
            self.push_op(Value::Bool(same));
            return Ok(());
        }
        let Value::Object(o) = &l else {
            return Err(expected_number_bin(op, &l, &r));
        };
        let (owner, _) = self
            .resolve_method(&o.class, dunder)
            .ok_or_else(|| expected_number_bin(op, &l, &r))?;
        self.call_method_frame(&owner, dunder, l.clone(), vec![r])
    }

    fn check_watchpoints(&mut self) {
        if self.watchpoints.is_empty() {
            return;
        }
        let curs: Vec<Option<String>> = self
            .watchpoints
            .iter()
            .map(|w| self.watch_eval(&w.expr).ok().map(|v| v.py_repr()))
            .collect();
        let mut hit = None;
        for (w, cur) in self.watchpoints.iter().zip(&curs) {
            if *cur != w.last {
                let shown = |o: &Option<String>| o.clone().unwrap_or_else(|| "(undefined)".into());
                hit = Some((w.src.clone(), shown(&w.last), shown(cur)));
                break;
            }
        }
        for (w, cur) in self.watchpoints.iter_mut().zip(curs) {
            w.last = cur;
        }
        self.watch_hit = hit;
    }

    /// Read-only watch evaluation against the current frame (+ globals). No
    /// calls in Phase 1.
    fn watch_eval(&self, e: &Expr) -> Result<Value, String> {
        match &e.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(s) => Ok(Value::Str(s.clone())),
            ExprKind::NoneLit => Ok(Value::None),
            ExprKind::Name(n) => self
                .lookup(n)
                .ok_or_else(|| format!("name '{n}' is not defined")),
            ExprKind::Unary(op, inner) => apply_unary(*op, self.watch_eval(inner)?),
            ExprKind::Bin(BinOp::And, a, b) => {
                let l = self.watch_eval(a)?;
                if l.truthy() {
                    self.watch_eval(b)
                } else {
                    Ok(l)
                }
            }
            ExprKind::Bin(BinOp::Or, a, b) => {
                let l = self.watch_eval(a)?;
                if l.truthy() {
                    Ok(l)
                } else {
                    self.watch_eval(b)
                }
            }
            ExprKind::Bin(op, a, b) => apply_bin(*op, &self.watch_eval(a)?, &self.watch_eval(b)?),
            ExprKind::Index(obj, idx) => index_get(&self.watch_eval(obj)?, &self.watch_eval(idx)?),
            ExprKind::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.watch_eval(it)?);
                }
                Ok(Value::list(out))
            }
            ExprKind::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.watch_eval(it)?);
                }
                Ok(Value::Tuple(out))
            }
            ExprKind::Dict(pairs) => {
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    out.push((self.watch_eval(k)?, self.watch_eval(v)?));
                }
                Ok(Value::dict(out))
            }
            _ => Err("that watch isn't supported in call-stack mode yet".to_string()),
        }
    }
}

/// Set members must be immutable (hashable). A list/dict/set can change, so it
/// can't be a member — a tuple can. Mirrors the compiled backends.
fn check_set_elem(v: &Value) -> Result<(), String> {
    match v {
        Value::List(_) | Value::Dict(_) | Value::Set(_) => Err(fill_msg(
            &crate::messages::TYPE_UNHASHABLE,
            &[("type", &type_label(v))],
        )),
        _ => Ok(()),
    }
}

/// Build a set from any iterable value, de-duplicating by Python equality and
/// keeping first-seen (insertion) order — matching the compiled backends.
/// The elements of any iterable value, in order (dicts yield keys) — shared by
/// `set()`, `list()`, and `tuple()`.
fn to_elements(v: &Value) -> Result<Vec<Value>, String> {
    match v {
        Value::List(x) | Value::Set(x) => Ok(x.borrow().clone()),
        Value::Tuple(x) => Ok(x.clone()),
        Value::Str(s) => Ok(s.chars().map(|c| Value::Str(c.to_string())).collect()),
        Value::Dict(d) => Ok(d.borrow().iter().map(|(k, _)| k.clone()).collect()),
        _ => Err(fill_msg(
            &crate::messages::TYPE_NO_LEN,
            &[("type", &type_label(v))],
        )),
    }
}

/// `target[start:stop:step]` with CPython slice semantics — shared by the step
/// debugger's two evaluators (matching the compiled backends). A list/tuple
/// slice returns the same kind; a string slice returns a string.
fn slice_value(
    target: &Value,
    start: Option<i64>,
    stop: Option<i64>,
    step: Option<i64>,
) -> Result<Value, String> {
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err("slice step cannot be zero".to_string());
    }
    // Clamp one bound the way CPython's PySlice_AdjustIndices does.
    let adjust = |idx: i64, n: i64| -> i64 {
        let (lower, upper) = if step < 0 { (-1, n - 1) } else { (0, n) };
        if idx < 0 {
            (idx + n).max(lower)
        } else {
            idx.min(upper)
        }
    };
    let bounds = |n: i64| -> (i64, i64) {
        let s = match start {
            Some(v) => adjust(v, n),
            None if step < 0 => n - 1,
            None => 0,
        };
        let e = match stop {
            Some(v) => adjust(v, n),
            None if step < 0 => -1,
            None => n,
        };
        (s, e)
    };
    let in_range = |i: i64, e: i64| (step > 0 && i < e) || (step < 0 && i > e);
    match target {
        Value::List(xs) => {
            let xs = xs.borrow();
            let (s, e) = bounds(xs.len() as i64);
            let mut out = Vec::new();
            let mut i = s;
            while in_range(i, e) {
                out.push(xs[i as usize].clone());
                i += step;
            }
            Ok(Value::list(out))
        }
        Value::Tuple(xs) => {
            let (s, e) = bounds(xs.len() as i64);
            let mut out = Vec::new();
            let mut i = s;
            while in_range(i, e) {
                out.push(xs[i as usize].clone());
                i += step;
            }
            Ok(Value::Tuple(out))
        }
        Value::Str(sv) => {
            let chars: Vec<char> = sv.chars().collect();
            let (s, e) = bounds(chars.len() as i64);
            let mut out = String::new();
            let mut i = s;
            while in_range(i, e) {
                out.push(chars[i as usize]);
                i += step;
            }
            Ok(Value::Str(out))
        }
        _ => Err(fill_msg(
            &crate::messages::TYPE_NOT_SUBSCRIPTABLE,
            &[("type", &type_label(target))],
        )),
    }
}

/// True-if-less-than for sorting/min/max (numeric or lexicographic strings) —
/// same ordering as the compiled backends.
fn py_lt(a: &Value, b: &Value) -> Result<bool, String> {
    Ok(matches!(compare(BinOp::Lt, a, b)?, Value::Bool(true)))
}

/// `math.<name>` module constant (`pi`/`e`/`tau`), matching the compiled
/// backends. `None` if `name` isn't a known constant.
fn math_const(name: &str) -> Option<Value> {
    match name {
        "pi" => Some(Value::Float(std::f64::consts::PI)),
        "e" => Some(Value::Float(std::f64::consts::E)),
        "tau" => Some(Value::Float(std::f64::consts::TAU)),
        _ => None,
    }
}

/// `math.<func>(args)` — matching the compiled backends' `gen_math_call`:
/// sqrt/fabs/exp/log/log2/log10/pow return floats, floor/ceil/trunc ints.
///
/// Transcendentals use std's f64 methods. In the browser — where the
/// Stepper actually runs — wasm32 std routes them to the libm crate, the
/// same source the compiled module vendors, so stepping shows exactly the
/// bits the program computes. On a native HOST, std uses the platform
/// libm, which may differ from musl-libm in the last ULP on some inputs —
/// the same caveat CPython itself has across platforms.
fn math_call(func: &str, args: &[Value]) -> Result<Value, String> {
    // math.pow is the one two-argument function; always float (CPython).
    if func == "pow" {
        let [a, b] = args else {
            return Err("math.pow() takes two arguments".to_string());
        };
        let x = as_num(a).ok_or_else(|| "math.pow() needs numbers".to_string())?;
        let y = as_num(b).ok_or_else(|| "math.pow() needs numbers".to_string())?;
        return Ok(Value::Float(x.powf(y)));
    }
    let [arg] = args else {
        return Err(format!("math.{func}() takes one argument"));
    };
    let x = as_num(arg).ok_or_else(|| format!("math.{func}() needs a number"))?;
    Ok(match func {
        "sqrt" => Value::Float(x.sqrt()),
        "exp" => Value::Float(x.exp()),
        "log" => Value::Float(x.ln()),
        "log2" => Value::Float(x.log2()),
        "log10" => Value::Float(x.log10()),
        "fabs" => Value::Float(x.abs()),
        "floor" => Value::Int(x.floor() as i64),
        "ceil" => Value::Int(x.ceil() as i64),
        "trunc" => Value::Int(x.trunc() as i64),
        _ => return Err(format!("math has no function '{func}'")),
    })
}

/// `import name1, name2, …` — `math` and `random` exist in the subset
/// (from-imports arrive colon-encoded, e.g. "random:randint"). A no-op on
/// success (the compiled backends treat it the same); a bad module is an error.
fn check_import(names: &[String]) -> Result<(), String> {
    for m in names {
        let module = m.split_once(':').map_or(m.as_str(), |(md, _)| md);
        if module != "math" && module != "random" {
            return Err(format!(
                "module '{module}' isn't available (only 'math' and 'random' for now)"
            ));
        }
    }
    Ok(())
}

/// If this call is `sorted(seq, reverse=<expr>)`, return `(seq, reverse-expr)`
/// so both evaluators can handle the one keyword builtin. Any other keyword
/// falls through to the normal (unsupported-kwarg) path.
fn sorted_reverse_args<'a>(name: &str, args: &'a [Expr]) -> Option<(&'a Expr, &'a Expr)> {
    if name == "sorted"
        && args.len() == 2
        && let ExprKind::Kwarg(k, val) = &args[1].kind
        && k == "reverse"
    {
        return Some((&args[0], val));
    }
    None
}

/// Builtins beyond both debug evaluators' inline core set — numeric reductions,
/// sorting, and the range/enumerate/zip materializers. Returns `Ok(None)` if
/// `name` isn't one of these, so the caller falls through to its own error.
/// Seed exactly like the WAT `$rnd_init`: odd nonzero state, churned 3x.
fn rnd_seed(s: i32) -> u64 {
    let mut st = ((s as u32 as u64) << 1) | 1;
    for _ in 0..3 {
        rnd_next(&mut st);
    }
    st
}

/// xorshift64* (12/25/27, Vigna's multiplier) — must stay bit-identical to
/// the WAT `$rnd_next`; a divergence here is a language fork.
fn rnd_next(st: &mut u64) -> u64 {
    let mut x = *st;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *st = x;
    x.wrapping_mul(2685821657736338717)
}

/// `random.<fn>(args)` in the Vm — same arms as `gen_random_call`.
fn random_call(st: &mut u64, func: &str, args: &mut [Value]) -> Result<Value, String> {
    match (func, &mut *args) {
        ("random", []) => Ok(Value::Float(
            (rnd_next(st) >> 11) as f64 * 1.110_223_024_625_156_5e-16,
        )),
        ("randint", [Value::Int(a), Value::Int(b)]) => {
            let (a, b) = (*a, *b);
            if a > b {
                return Err(crate::messages::VALUE_RANDINT_ORDER.text.into());
            }
            let span = ((b as i128) - (a as i128) + 1) as u64;
            Ok(Value::Int(a + (rnd_next(st) % span) as i64))
        }
        ("choice", [Value::List(items)]) => {
            let items = items.borrow();
            if items.is_empty() {
                return Err(crate::messages::INDEX_CHOICE_EMPTY.text.into());
            }
            let i = (rnd_next(st) % items.len() as u64) as usize;
            Ok(items[i].clone())
        }
        ("shuffle", [Value::List(items)]) => {
            let mut items = items.borrow_mut();
            let mut i = items.len().saturating_sub(1);
            while i > 0 {
                let j = (rnd_next(st) % (i as u64 + 1)) as usize;
                items.swap(i, j);
                i -= 1;
            }
            Ok(Value::None)
        }
        ("seed", [Value::Int(n)]) => {
            *st = rnd_seed(*n as i32);
            Ok(Value::None)
        }
        _ => Err(format!(
            "random.{func}() got arguments it doesn't understand"
        )),
    }
}

fn builtin_extra(name: &str, args: &[Value]) -> Result<Option<Value>, String> {
    // Bare math names bound by `from math import ...`. The COMPILER enforces
    // that the import is present (unknown function otherwise); the
    // interpreters trust programs that compile, so the name alone suffices.
    if matches!(
        name,
        "sqrt" | "fabs" | "floor" | "ceil" | "trunc" | "exp" | "log" | "log2" | "log10" | "pow"
    ) {
        return math_call(name, args).map(Some);
    }
    let int_arg =
        |v: &Value, what: &str| as_int(v).ok_or_else(|| format!("{what} must be an integer"));
    let v = match (name, args) {
        ("range", a) if (1..=3).contains(&a.len()) => {
            let (start, stop, step) = match a {
                [b] => (0, int_arg(b, "range() stop")?, 1),
                [a, b] => (int_arg(a, "range() start")?, int_arg(b, "range() stop")?, 1),
                [a, b, c] => (
                    int_arg(a, "range() start")?,
                    int_arg(b, "range() stop")?,
                    int_arg(c, "range() step")?,
                ),
                _ => unreachable!(),
            };
            if step == 0 {
                return Err("range() step must not be zero".to_string());
            }
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                out.push(Value::Int(i));
                i += step;
            }
            Value::list(out)
        }
        ("sum", [v]) => {
            let mut acc = Value::Int(0);
            for el in to_elements(v)? {
                acc = add(&acc, &el)?;
            }
            acc
        }
        ("min", a) if !a.is_empty() => min_max_values(a, true)?,
        ("max", a) if !a.is_empty() => min_max_values(a, false)?,
        ("sorted", [v]) => sorted_values(v, false)?,
        ("round", [v]) => round_value(v, None)?,
        ("round", [v, nd]) => round_value(v, Some(int_arg(nd, "round() ndigits")?))?,
        ("enumerate", [v]) => enumerate_values(v, 0)?,
        ("enumerate", [v, s]) => enumerate_values(v, int_arg(s, "enumerate() start")?)?,
        ("zip", [a, b]) => {
            let (ea, eb) = (to_elements(a)?, to_elements(b)?);
            let n = ea.len().min(eb.len());
            Value::list(
                (0..n)
                    .map(|i| Value::Tuple(vec![ea[i].clone(), eb[i].clone()]))
                    .collect(),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}

fn min_max_values(args: &[Value], want_min: bool) -> Result<Value, String> {
    // One iterable, or several positional args (like CPython).
    let items = if args.len() == 1 {
        to_elements(&args[0])?
    } else {
        args.to_vec()
    };
    let mut best: Option<Value> = None;
    for el in items {
        best = Some(match best {
            None => el,
            Some(b) => {
                let take = if want_min {
                    py_lt(&el, &b)?
                } else {
                    py_lt(&b, &el)?
                };
                if take { el } else { b }
            }
        });
    }
    best.ok_or_else(|| {
        format!(
            "{}() arg is an empty sequence",
            if want_min { "min" } else { "max" }
        )
    })
}

fn sorted_values(seq: &Value, reverse: bool) -> Result<Value, String> {
    let mut items = to_elements(seq)?;
    // Stable insertion sort — same shape as the compiled backends.
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 {
            let shift = if reverse {
                py_lt(&items[j - 1], &items[j])?
            } else {
                py_lt(&items[j], &items[j - 1])?
            };
            if !shift {
                break;
            }
            items.swap(j, j - 1);
            j -= 1;
        }
    }
    Ok(Value::list(items))
}

fn round_value(v: &Value, ndigits: Option<i64>) -> Result<Value, String> {
    let x = as_num(v).ok_or_else(|| format!("round() needs a number, not {}", type_name(v)))?;
    match ndigits {
        None => Ok(Value::Int(x.round_ties_even() as i64)),
        Some(nd) => {
            let mut scale = 1.0_f64;
            if nd >= 0 {
                for _ in 0..nd {
                    scale *= 10.0;
                }
            } else {
                for _ in 0..(-nd) {
                    scale *= 0.1;
                }
            }
            Ok(Value::Float((x * scale).round_ties_even() / scale))
        }
    }
}

fn enumerate_values(seq: &Value, start: i64) -> Result<Value, String> {
    Ok(Value::list(
        to_elements(seq)?
            .into_iter()
            .enumerate()
            .map(|(i, el)| Value::Tuple(vec![Value::Int(start + i as i64), el]))
            .collect(),
    ))
}

fn make_set(v: &Value) -> Result<Value, String> {
    let items = to_elements(v)?;
    let mut out: Vec<Value> = Vec::new();
    for it in items {
        check_set_elem(&it)?;
        if !out.iter().any(|y| py_eq(y, &it)) {
            out.push(it);
        }
    }
    Ok(Value::set(out))
}

/// A set-theory operation on two sets: `|` union, `&` intersection, `^`
/// symmetric difference, `-` difference. Insertion-order preserving.
fn set_binop(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    let (Value::Set(a), Value::Set(b)) = (l, r) else {
        return Err(expected_number_bin(op, l, r));
    };
    let (a, b) = (a.borrow(), b.borrow());
    let in_set = |set: &[Value], v: &Value| set.iter().any(|y| py_eq(y, v));
    let res: Vec<Value> = match op {
        BinOp::BitOr => {
            let mut v = a.clone();
            for x in b.iter() {
                if !in_set(&v, x) {
                    v.push(x.clone());
                }
            }
            v
        }
        BinOp::BitAnd => a.iter().filter(|x| in_set(&b, x)).cloned().collect(),
        BinOp::Sub => a.iter().filter(|x| !in_set(&b, x)).cloned().collect(),
        BinOp::BitXor => a
            .iter()
            .filter(|x| !in_set(&b, x))
            .chain(b.iter().filter(|x| !in_set(&a, x)))
            .cloned()
            .collect(),
        _ => unreachable!("non-set-op passed to set_binop"),
    };
    Ok(Value::set(res))
}

/// `&`/`|`/`^`: a set operation when both sides are sets, else integer bitwise
/// (matching Python and the native backend).
fn bitwise_or_set(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    if matches!((l, r), (Value::Set(_), Value::Set(_))) {
        return set_binop(op, l, r);
    }
    match (as_int(l), as_int(r)) {
        (Some(x), Some(y)) => {
            let z = match op {
                BinOp::BitOr => x | y,
                BinOp::BitAnd => x & y,
                BinOp::BitXor => x ^ y,
                _ => unreachable!(),
            };
            Ok(Value::Int(z))
        }
        _ => Err(expected_number_bin(op, l, r)),
    }
}

/// `<<` / `>>` — integer shifts (both operands integral, like Python and the
/// native/viper machine-word path). Negative shift counts are an error, as in
/// CPython; `>>` is arithmetic (sign-preserving).
fn int_shift(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    match (as_int(l), as_int(r)) {
        (Some(x), Some(y)) => {
            if y < 0 {
                return Err("negative shift count".to_string());
            }
            let sh = y as u32;
            let z = match op {
                BinOp::Shl => x.wrapping_shl(sh),
                BinOp::Shr => x.wrapping_shr(sh),
                _ => unreachable!(),
            };
            Ok(Value::Int(z))
        }
        _ => Err(expected_number_bin(op, l, r)),
    }
}

/// Whether every element of `a` is in `b` (both must be sets).
fn set_subset(a: &Value, b: &Value) -> Result<bool, String> {
    match (a, b) {
        (Value::Set(x), Value::Set(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            Ok(x.iter().all(|e| y.iter().any(|f| py_eq(f, e))))
        }
        _ => Err("issubset/issuperset need a set argument".to_string()),
    }
}

/// ASCII whitespace, matching the compiled backends' `$is_space` (not Rust's
/// fuller `is_ascii_whitespace`, which also treats form-feed/vertical-tab as
/// space).
fn is_ascii_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// Does `sub` occur in `s` at byte offset `pos`? Shared by find/count/replace/
/// starts/ends, mirroring the runtime's `str_match_at`.
fn str_match_at(s: &[u8], pos: usize, sub: &[u8]) -> bool {
    pos + sub.len() <= s.len() && s[pos..pos + sub.len()] == *sub
}

/// Byte index of the first occurrence of `sub` in `s`, or -1. An empty `sub`
/// matches at 0 (CPython's rule).
fn str_find_idx(s: &str, sub: &str) -> i64 {
    let (b, sb) = (s.as_bytes(), sub.as_bytes());
    let (n, sl) = (b.len(), sb.len());
    let mut i = 0;
    while i + sl <= n {
        if str_match_at(b, i, sb) {
            return i as i64;
        }
        i += 1;
    }
    -1
}

/// Non-overlapping occurrences of `sub` in `s`. An empty `sub` counts every
/// gap between bytes plus the two ends (CPython's `"abc".count("") == 4`).
/// A string method's argument, type-checked. `method` is only used for the
/// error text.
fn arg_str<'a>(v: &'a Value, method: &str) -> Result<&'a str, String> {
    match v {
        Value::Str(s) => Ok(s.as_str()),
        _ => Err(format!(
            ".{method}() needs a string argument, not {}",
            type_name(v)
        )),
    }
}

fn str_count_sub(s: &str, sub: &str) -> i64 {
    let (b, sb) = (s.as_bytes(), sub.as_bytes());
    let (n, sl) = (b.len(), sb.len());
    if sl == 0 {
        return (n + 1) as i64;
    }
    let mut i = 0;
    let mut cnt = 0i64;
    while i + sl <= n {
        if str_match_at(b, i, sb) {
            cnt += 1;
            i += sl;
        } else {
            i += 1;
        }
    }
    cnt
}

/// Non-mutating string methods — strings are immutable, so there is no
/// separate mutating variant. Transforms are byte-wise and ASCII-only
/// (upper/lower/strip/capitalize/title), matching the compiled backends'
/// deliberate simplification exactly: a UTF-8 continuation/lead byte is
/// always >= 0x80 and never equals an ASCII letter, so this generalizes
/// correctly to non-ASCII text too, same as `runtime/src/lib.rs`'s
/// `str_case_shift` et al. Returns `None` if `method` isn't a string method.
fn str_method_val(s: &str, method: &str, args: &[Value]) -> Option<Result<Value, String>> {
    let r: Result<Value, String> = match (method, args) {
        ("upper", []) | ("lower", []) => {
            let upper = method == "upper";
            let bytes: Vec<u8> = s
                .bytes()
                .map(|c| {
                    if upper && (97..=122).contains(&c) {
                        c - 32
                    } else if !upper && (65..=90).contains(&c) {
                        c + 32
                    } else {
                        c
                    }
                })
                .collect();
            Ok(Value::Str(String::from_utf8(bytes).unwrap()))
        }
        ("strip", []) | ("lstrip", []) | ("rstrip", []) => {
            let (left, right) = (method != "rstrip", method != "lstrip");
            let b = s.as_bytes();
            let (mut start, mut end) = (0usize, b.len());
            if left {
                while start < end && is_ascii_space(b[start]) {
                    start += 1;
                }
            }
            if right {
                while end > start && is_ascii_space(b[end - 1]) {
                    end -= 1;
                }
            }
            Ok(Value::Str(
                String::from_utf8(b[start..end].to_vec()).unwrap(),
            ))
        }
        ("capitalize", []) => {
            let bytes: Vec<u8> = s
                .bytes()
                .enumerate()
                .map(|(i, c)| {
                    if i == 0 {
                        if (97..=122).contains(&c) { c - 32 } else { c }
                    } else if (65..=90).contains(&c) {
                        c + 32
                    } else {
                        c
                    }
                })
                .collect();
            Ok(Value::Str(String::from_utf8(bytes).unwrap()))
        }
        ("title", []) => {
            let mut prev_alpha = false;
            let bytes: Vec<u8> = s
                .bytes()
                .map(|c| {
                    let is_alpha = (65..=90).contains(&c) || (97..=122).contains(&c);
                    let out = if !is_alpha {
                        c
                    } else if prev_alpha {
                        if (65..=90).contains(&c) { c + 32 } else { c }
                    } else if (97..=122).contains(&c) {
                        c - 32
                    } else {
                        c
                    };
                    prev_alpha = is_alpha;
                    out
                })
                .collect();
            Ok(Value::Str(String::from_utf8(bytes).unwrap()))
        }
        ("split", []) => Ok(Value::list(
            s.split_ascii_whitespace()
                .map(|w| Value::Str(w.to_string()))
                .collect(),
        )),
        ("split", [sepv]) => match arg_str(sepv, method) {
            Err(e) => Err(e),
            Ok("") => Ok(Value::list(vec![Value::Str(s.to_string())])),
            Ok(sep) => Ok(Value::list(
                s.split(sep).map(|w| Value::Str(w.to_string())).collect(),
            )),
        },
        ("join", [it]) => match to_elements(it) {
            Err(e) => Err(e),
            Ok(elems) => {
                let mut parts = Vec::with_capacity(elems.len());
                for e in &elems {
                    match e {
                        Value::Str(w) => parts.push(w.as_str()),
                        _ => return Some(Err("join() needs an iterable of strings".to_string())),
                    }
                }
                Ok(Value::Str(parts.join(s)))
            }
        },
        ("find", [subv]) => arg_str(subv, method).map(|sub| Value::Int(str_find_idx(s, sub))),
        ("count", [subv]) => arg_str(subv, method).map(|sub| Value::Int(str_count_sub(s, sub))),
        ("index", [subv]) => arg_str(subv, method).and_then(|sub| {
            let idx = str_find_idx(s, sub);
            if idx < 0 {
                Err(crate::messages::VALUE_NOT_IN_SEQUENCE.text.to_string())
            } else {
                Ok(Value::Int(idx))
            }
        }),
        ("startswith", [p]) => arg_str(p, method).map(|p| {
            Value::Bool(p.len() <= s.len() && str_match_at(s.as_bytes(), 0, p.as_bytes()))
        }),
        ("endswith", [p]) => arg_str(p, method).map(|p| {
            let (n, pl) = (s.len(), p.len());
            Value::Bool(pl <= n && str_match_at(s.as_bytes(), n - pl, p.as_bytes()))
        }),
        ("isdigit", []) => Ok(Value::Bool(
            !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()),
        )),
        ("isalpha", []) => Ok(Value::Bool(
            !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphabetic()),
        )),
        ("zfill", [w]) => match as_int(w) {
            None => Err(expected_number(w)),
            Some(w) => {
                let w = w.max(0) as usize;
                let n = s.len();
                if n >= w {
                    Ok(Value::Str(s.to_string()))
                } else {
                    let pad = w - n;
                    let b = s.as_bytes();
                    let first = if n > 0 { b[0] } else { 0 };
                    let has_sign = first == b'+' || first == b'-';
                    let mut out = Vec::with_capacity(w);
                    if has_sign {
                        out.push(first);
                        out.extend(std::iter::repeat_n(b'0', pad));
                        out.extend_from_slice(&b[1..]);
                    } else {
                        out.extend(std::iter::repeat_n(b'0', pad));
                        out.extend_from_slice(b);
                    }
                    Ok(Value::Str(String::from_utf8(out).unwrap()))
                }
            }
        },
        ("replace", [oldv, newv]) => match (arg_str(oldv, method), arg_str(newv, method)) {
            (Ok(""), Ok(_)) => Ok(Value::Str(s.to_string())),
            (Ok(old), Ok(new)) => {
                let (b, ob) = (s.as_bytes(), old.as_bytes());
                let (n, ol) = (b.len(), ob.len());
                let mut out = Vec::new();
                let mut i = 0;
                while i < n {
                    if i + ol <= n && str_match_at(b, i, ob) {
                        out.extend_from_slice(new.as_bytes());
                        i += ol;
                    } else {
                        out.push(b[i]);
                        i += 1;
                    }
                }
                Ok(Value::Str(String::from_utf8(out).unwrap()))
            }
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        _ => return None,
    };
    Some(r)
}

/// Mutating set methods on the set's elements (the receiver is a set variable).
/// Returns `None` if `method` isn't a mutating set method.
fn set_method_mut(
    items: &Rc<std::cell::RefCell<Vec<Value>>>,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let mut items = items.borrow_mut();
    let r = match (method, args) {
        ("add", [v]) => match check_set_elem(v) {
            Err(e) => Err(e),
            Ok(()) => {
                if !items.iter().any(|y| py_eq(y, v)) {
                    items.push(v.clone());
                }
                Ok(Value::None)
            }
        },
        ("discard", [v]) => {
            items.retain(|y| !py_eq(y, v));
            Ok(Value::None)
        }
        ("remove", [v]) => match items.iter().position(|y| py_eq(y, v)) {
            Some(i) => {
                items.remove(i);
                Ok(Value::None)
            }
            None => Err(format!("{} is not in the set", v.py_repr())),
        },
        // Our sets are insertion-ordered, so pop removes the last element.
        ("pop", []) => items
            .pop()
            .ok_or_else(|| "pop from an empty set".to_string()),
        ("clear", []) => {
            items.clear();
            Ok(Value::None)
        }
        _ => return None,
    };
    Some(r)
}

/// Non-mutating set methods on a set value. Returns `None` if `recv` isn't a set
/// or `method` isn't a non-mutating set method. Binary-op methods require a set
/// argument (matching the compiled backends).
fn set_method_val(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if !matches!(recv, Value::Set(_)) {
        return None;
    }
    let r = match (method, args) {
        ("union", [o]) => set_binop(BinOp::BitOr, recv, o),
        ("intersection", [o]) => set_binop(BinOp::BitAnd, recv, o),
        ("difference", [o]) => set_binop(BinOp::Sub, recv, o),
        ("symmetric_difference", [o]) => set_binop(BinOp::BitXor, recv, o),
        ("issubset", [o]) => set_subset(recv, o).map(Value::Bool),
        ("issuperset", [o]) => set_subset(o, recv).map(Value::Bool),
        ("copy", []) => match recv {
            Value::Set(items) => Ok(Value::set(items.borrow().clone())),
            _ => unreachable!("guarded by the matches! check above"),
        },
        _ => return None,
    };
    Some(r)
}

/// Apply a binary operator to two evaluated values (shared by the VM).
fn apply_bin(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    match op {
        BinOp::Eq => Ok(Value::Bool(py_eq(l, r))),
        BinOp::Ne => Ok(Value::Bool(!py_eq(l, r))),
        // Restricted to `None` by the parser; never dunder-dispatched, so
        // this is correct even when called directly (bypassing Task::Bin's
        // own identity shortcut, e.g. from `watch_eval`).
        BinOp::Is => Ok(Value::Bool(
            matches!(l, Value::None) && matches!(r, Value::None),
        )),
        BinOp::IsNot => Ok(Value::Bool(
            !(matches!(l, Value::None) && matches!(r, Value::None)),
        )),
        BinOp::In => Ok(Value::Bool(contains(r, l)?)),
        BinOp::NotIn => Ok(Value::Bool(!contains(r, l)?)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, l, r),
        BinOp::Add => add(l, r),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
            arith(op, l, r)
        }
        BinOp::And | BinOp::Or => unreachable!("short-circuited before apply"),
        BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => bitwise_or_set(op, l, r),
        BinOp::Shl | BinOp::Shr => int_shift(op, l, r),
    }
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, String> {
    match op {
        UnOp::Not => Ok(Value::Bool(!v.truthy())),
        UnOp::Neg => match v {
            Value::Int(n) => Ok(Value::Int(-n)),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Bool(b) => Ok(Value::Int(if b { -1 } else { 0 })),
            _ => Err(expected_number(&v)),
        },
        // `~x` == `-x - 1` (Rust's `!` on a signed int is bitwise NOT). Ints
        // (and bools, which act as 0/1) only — matches CPython and viper.
        UnOp::Invert => match v {
            Value::Int(n) => Ok(Value::Int(!n)),
            Value::Bool(b) => Ok(Value::Int(!(b as i64))),
            _ => Err(format!("bad operand type for unary ~: '{}'", type_name(&v))),
        },
    }
}

/// Evaluate a constant expression (for default arguments) — literals only.
fn eval_const(e: &Expr) -> Result<Value, String> {
    match &e.kind {
        ExprKind::Int(n) => Ok(Value::Int(*n)),
        ExprKind::Float(f) => Ok(Value::Float(*f)),
        ExprKind::Bool(b) => Ok(Value::Bool(*b)),
        ExprKind::Str(s) => Ok(Value::Str(s.clone())),
        ExprKind::NoneLit => Ok(Value::None),
        ExprKind::Unary(op, inner) => apply_unary(*op, eval_const(inner)?),
        _ => Err("only literal default arguments are supported in call-stack mode yet".to_string()),
    }
}

/// The VM's builtin functions (a curated K-12 subset).
fn call_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match (name, args) {
        ("len", [v]) => match v {
            Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
            Value::List(l) | Value::Set(l) => Ok(Value::Int(l.borrow().len() as i64)),
            Value::Tuple(l) => Ok(Value::Int(l.len() as i64)),
            Value::Dict(d) => Ok(Value::Int(d.borrow().len() as i64)),
            _ => Err(fill_msg(
                &crate::messages::TYPE_NO_LEN,
                &[("type", &type_label(v))],
            )),
        },
        ("abs", [v]) => match v {
            Value::Int(n) => Ok(Value::Int(n.abs())),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            _ => Err(expected_number(v)),
        },
        ("str", [v]) => Ok(Value::Str(v.py_str())),
        ("int", [v]) => match v {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(*f as i64)),
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            Value::Str(s) => parse_int_str(s),
            _ => Err(expected_number(v)),
        },
        ("float", [v]) => match v {
            Value::Str(s) => s
                .trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| fill_msg(&crate::messages::VALUE_FLOAT_PARSE, &[("text", s)])),
            _ => as_num(v)
                .map(Value::Float)
                .ok_or_else(|| expected_number(v)),
        },
        ("bool", [v]) => Ok(Value::Bool(v.truthy())),
        ("set", []) => Ok(Value::set(Vec::new())),
        ("set", [v]) => make_set(v),
        ("list", []) => Ok(Value::list(Vec::new())),
        ("list", [v]) => Ok(Value::list(to_elements(v)?)),
        ("tuple", []) => Ok(Value::Tuple(Vec::new())),
        ("tuple", [v]) => Ok(Value::Tuple(to_elements(v)?)),
        ("dict", []) => Ok(Value::dict(Vec::new())),
        // Host-capability builtins (seed/report lower to env.* at runtime). The
        // step debugger has no host, so they no-op with a placeholder — the
        // control flow still traces; the real effect happens under Run.
        ("seed", []) => Ok(Value::Int(0)),
        ("report", [_score, _trace]) => Ok(Value::None),
        ("evidence", [_key, _value]) => Ok(Value::None),
        ("emit_html", [_html]) => Ok(Value::None),
        ("show", [_value]) => Ok(Value::None),
        ("set_field", [_key, _value]) => Ok(Value::None),
        ("get_field", [_key]) => Ok(Value::Str(String::new())),
        // The designer's drag feature AUTHORS module-level set_position calls
        // (the IDE's pycode::upsert_position), so any drag-touched program hits
        // this on its first steps — it must trace as a no-op, not kill the
        // session (the feature's whole point is that layout is steppable).
        ("set_position", [_sel, _x, _y]) => Ok(Value::None),
        ("add_element", [_parent, _tag, _id]) => Ok(Value::None),
        ("pointer_x", []) | ("pointer_y", []) => Ok(Value::Int(0)),
        ("on_frame", [_h]) => Ok(Value::None),
        _ => match builtin_extra(name, args)? {
            Some(v) => Ok(v),
            None => Err(format!(
                "calling {name}() isn't in the step debugger's call-stack mode yet — use Run"
            )),
        },
    }
}

/// `name.method(args)` on a list variable in `frame`'s scope (append / pop).
fn list_method(
    frame: &mut VmFrame,
    name: &str,
    method: &str,
    args: Vec<Value>,
) -> Result<Value, String> {
    let recv = frame
        .scope
        .get_mut(name)
        .ok_or_else(|| format!("name '{name}' is not defined"))?;
    container_method(recv, method, args)
}

/// A container method against a mutable VALUE — shared by variable receivers
/// (`xs.append(..)`, via the scope slot) and attribute receivers
/// (`self.tricks.append(..)`, via the instance's attrs cell).
fn container_method(recv: &mut Value, method: &str, args: Vec<Value>) -> Result<Value, String> {
    let unsupported =
        || format!(".{method}() isn't in the step debugger's call-stack mode yet — use Run");
    match recv {
        Value::List(items) => {
            let mut items = items.borrow_mut();
            match (method, args.as_slice()) {
                ("append", [v]) => {
                    items.push(v.clone());
                    Ok(Value::None)
                }
                ("pop", []) => items.pop().ok_or_else(|| {
                    fill_msg(&crate::messages::INDEX_OUT_OF_RANGE, &[("seq", "list")])
                }),
                ("pop", [i]) => {
                    let i = as_int(i).ok_or_else(|| expected_number(i))?;
                    let n = items.len() as i64;
                    let real = if i < 0 { i + n } else { i };
                    if real < 0 || real >= n {
                        return Err(fill_msg(
                            &crate::messages::INDEX_OUT_OF_RANGE,
                            &[("seq", "list")],
                        ));
                    }
                    Ok(items.remove(real as usize))
                }
                // Stable insertion sort via `py_lt` — same shape as `sorted()`.
                ("sort", []) => {
                    for i in 1..items.len() {
                        let mut j = i;
                        while j > 0 && py_lt(&items[j], &items[j - 1])? {
                            items.swap(j, j - 1);
                            j -= 1;
                        }
                    }
                    Ok(Value::None)
                }
                ("reverse", []) => {
                    items.reverse();
                    Ok(Value::None)
                }
                ("extend", [it]) => {
                    items.extend(to_elements(it)?);
                    Ok(Value::None)
                }
                ("insert", [idx, v]) => {
                    let i = as_int(idx).ok_or_else(|| expected_number(idx))?;
                    let n = items.len() as i64;
                    let real = i.clamp(0, n);
                    items.insert(real as usize, v.clone());
                    Ok(Value::None)
                }
                ("count", [v]) => Ok(Value::Int(
                    items.iter().filter(|x| py_eq(x, v)).count() as i64
                )),
                ("index", [v]) => items
                    .iter()
                    .position(|x| py_eq(x, v))
                    .map(|i| Value::Int(i as i64))
                    .ok_or_else(|| crate::messages::VALUE_NOT_IN_SEQUENCE.text.to_string()),
                _ => Err(unsupported()),
            }
        }
        // Tuples are immutable, so only the non-mutating element queries apply.
        Value::Tuple(items) => match (method, args.as_slice()) {
            ("count", [v]) => Ok(Value::Int(
                items.iter().filter(|x| py_eq(x, v)).count() as i64
            )),
            ("index", [v]) => items
                .iter()
                .position(|x| py_eq(x, v))
                .map(|i| Value::Int(i as i64))
                .ok_or_else(|| crate::messages::VALUE_NOT_IN_SEQUENCE.text.to_string()),
            _ => Err(unsupported()),
        },
        Value::Set(items) => {
            if let Some(res) = set_method_mut(items, method, &args) {
                return res;
            }
            // Non-mutating methods need the value; clone the set and dispatch.
            let recv = Value::Set(items.clone());
            set_method_val(&recv, method, &args).unwrap_or_else(|| Err(unsupported()))
        }
        Value::Dict(pairs) => {
            let mut pairs = pairs.borrow_mut();
            match (method, args.as_slice()) {
                ("pop", [k]) => match pairs.iter().position(|(dk, _)| py_eq(dk, k)) {
                    Some(i) => Ok(pairs.remove(i).1),
                    None => Err(fill_msg(
                        &crate::messages::KEY_MISSING,
                        &[("key", &k.py_repr())],
                    )),
                },
                ("update", [other]) => {
                    let Value::Dict(extra) = other else {
                        return Err(".update() needs a dict argument".to_string());
                    };
                    for (k, v) in extra.borrow().iter() {
                        match pairs.iter_mut().find(|(dk, _)| py_eq(dk, k)) {
                            Some(slot) => slot.1 = v.clone(),
                            None => pairs.push((k.clone(), v.clone())),
                        }
                    }
                    Ok(Value::None)
                }
                ("clear", []) => {
                    pairs.clear();
                    Ok(Value::None)
                }
                ("setdefault", [k, default]) => match pairs.iter().find(|(dk, _)| py_eq(dk, k)) {
                    Some((_, v)) => Ok(v.clone()),
                    None => {
                        pairs.push((k.clone(), default.clone()));
                        Ok(default.clone())
                    }
                },
                ("keys", []) => Ok(Value::list(pairs.iter().map(|(k, _)| k.clone()).collect())),
                ("values", []) => Ok(Value::list(pairs.iter().map(|(_, v)| v.clone()).collect())),
                ("items", []) => Ok(Value::list(
                    pairs
                        .iter()
                        .map(|(k, v)| Value::Tuple(vec![k.clone(), v.clone()]))
                        .collect(),
                )),
                _ => Err(unsupported()),
            }
        }
        Value::Str(s) => {
            str_method_val(s.as_str(), method, &args).unwrap_or_else(|| Err(unsupported()))
        }
        _ => Err(unsupported()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a program to completion (stepping one statement at a time) and return
    /// its output — asserting it actually finished.
    fn run_to_end(src: &str) -> String {
        let mut s = Stepper::new(src).expect("parse");
        let mut guard = 0;
        while s.is_paused() {
            s.step();
            guard += 1;
            assert!(guard < 100_000, "ran away");
        }
        assert_eq!(*s.status(), Status::Finished, "did not finish cleanly");
        s.output().to_string()
    }

    #[test]
    fn steps_assignment_and_print() {
        assert_eq!(run_to_end("x = 5\nprint(x)\n"), "5\n");
    }

    #[test]
    fn a_plain_function_alias_steps_like_calling_it_directly() {
        assert_eq!(
            run_to_end("def double(x):\n    return x * 2\ng = double\nprint(g(7))\n"),
            "14\n"
        );
    }

    #[test]
    fn a_callback_parameter_steps_like_calling_it_directly() {
        assert_eq!(
            run_to_end(
                "def double(x):\n    return x * 2\ndef apply(f, x):\n    return f(x)\nprint(apply(double, 5))\n"
            ),
            "10\n"
        );
    }

    #[test]
    fn list_tuple_dict_constructors_in_the_step_debugger() {
        assert_eq!(run_to_end("print(list(\"abc\"))\n"), "['a', 'b', 'c']\n");
        assert_eq!(run_to_end("print(tuple([1, 2]))\n"), "(1, 2)\n");
        assert_eq!(run_to_end("print(list())\n"), "[]\n");
        assert_eq!(
            run_to_end("d = dict()\nd[\"a\"] = 1\nprint(d)\n"),
            "{'a': 1}\n"
        );
    }

    #[test]
    fn sequence_repetition_in_the_step_debugger() {
        assert_eq!(run_to_end("print(\"=\" * 5)\n"), "=====\n");
        assert_eq!(run_to_end("print(3 * \"ab\")\n"), "ababab\n");
        assert_eq!(run_to_end("print([0] * 3)\n"), "[0, 0, 0]\n");
        assert_eq!(run_to_end("print(2 * [1, 2])\n"), "[1, 2, 1, 2]\n");
    }

    #[test]
    fn slicing_in_the_step_debugger() {
        // Lists, strings, negative bounds, and steps (incl. the reverse slice
        // that `reversed()` and starred unpack desugar to).
        assert_eq!(run_to_end("print([1, 2, 3, 4][1:3])\n"), "[2, 3]\n");
        assert_eq!(run_to_end("print([1, 2, 3, 4][:2])\n"), "[1, 2]\n");
        assert_eq!(run_to_end("print([1, 2, 3, 4][2:])\n"), "[3, 4]\n");
        assert_eq!(run_to_end("print([1, 2, 3, 4][-2:])\n"), "[3, 4]\n");
        assert_eq!(run_to_end("print([1, 2, 3][::-1])\n"), "[3, 2, 1]\n");
        assert_eq!(run_to_end("print(\"hello\"[1:4])\n"), "ell\n");
        assert_eq!(run_to_end("print(\"abc\"[::-1])\n"), "cba\n");
    }

    #[test]
    fn reversed_and_starred_unpack_now_run_in_the_stepper() {
        // Both desugar to slices, so both work now that slicing is in.
        assert_eq!(
            run_to_end("for x in reversed([1, 2, 3]):\n    print(x)\n"),
            "3\n2\n1\n"
        );
        assert_eq!(
            run_to_end("a, *rest = [1, 2, 3, 4]\nprint(a)\nprint(rest)\n"),
            "1\n[2, 3, 4]\n"
        );
    }

    #[test]
    fn closures_run_in_the_step_debugger() {
        // The debugger runs the same closure conversion as the compiler, so
        // captures behave identically here (values match CPython).
        assert_eq!(
            run_to_end(
                "def outer():\n    x = 5\n    def inner():\n        return x + 1\n    return inner()\nprint(outer())\n"
            ),
            "6\n"
        );
        assert_eq!(
            run_to_end(
                "def f(n):\n    def dbl():\n        return n * 2\n    return dbl()\nprint(f(21))\n"
            ),
            "42\n"
        );
        // A rebind before the call is seen, like Python's cell capture.
        assert_eq!(
            run_to_end(
                "def f():\n    x = 1\n    def s():\n        return x\n    x = 2\n    return s()\nprint(f())\n"
            ),
            "2\n"
        );
    }

    #[test]
    fn float_display_matches_cpython_in_the_debugger() {
        assert_eq!(run_to_end("print(1.5)\n"), "1.5\n");
        assert_eq!(run_to_end("print(3.0)\n"), "3.0\n");
        assert_eq!(run_to_end("print(1234567.891)\n"), "1234567.891\n"); // was ...061467
        assert_eq!(run_to_end("print(0.00001)\n"), "1e-05\n");
        assert_eq!(
            run_to_end("print([0.1, 2.0, 0.25])\n"),
            "[0.1, 2.0, 0.25]\n"
        );
        assert_eq!(run_to_end("print(10.0 / 4.0)\n"), "2.5\n");
    }

    #[test]
    fn import_math_in_the_step_debugger() {
        assert_eq!(run_to_end("import math\nprint(math.sqrt(9.0))\n"), "3.0\n");
        assert_eq!(run_to_end("import math\nprint(math.floor(3.7))\n"), "3\n");
        assert_eq!(run_to_end("import math\nprint(math.ceil(3.2))\n"), "4\n");
        // A constant, and a value named `math` shadows the module (like CPython).
        assert_eq!(
            run_to_end("import math\nx = math.pi\nprint(x > 3.14)\nprint(x < 3.15)\n"),
            "True\nTrue\n"
        );
        // math inside a function body (the atomic executor).
        assert_eq!(
            run_to_end("import math\ndef h(n):\n    return math.sqrt(n)\nprint(h(16.0))\n"),
            "4.0\n"
        );
        // An unavailable module is a clean error, not a crash.
        let mut s = Stepper::new("import numpy\n").unwrap();
        while s.is_paused() {
            s.step();
        }
        assert!(matches!(s.status(), Status::Error { message, .. } if message.contains("numpy")));
    }

    #[test]
    fn slicing_and_unpack_inside_a_called_function() {
        // Exercises the atomic step-over executor used for function bodies.
        let src = "def head(xs):\n    a, *rest = xs\n    return a\n\
                   def mid(s):\n    return s[1:-1]\n\
                   print(head([10, 20, 30]))\nprint(mid(\"hello\"))\n";
        assert_eq!(run_to_end(src), "10\nell\n");
    }

    #[test]
    fn range_and_reduction_builtins_in_the_step_debugger() {
        assert_eq!(run_to_end("print(list(range(4)))\n"), "[0, 1, 2, 3]\n");
        assert_eq!(
            run_to_end("print(sorted(list(range(5))))\n"),
            "[0, 1, 2, 3, 4]\n"
        );
        assert_eq!(run_to_end("print(sum([1, 2, 3]))\n"), "6\n");
        assert_eq!(run_to_end("print(min([3, 1, 2]))\n"), "1\n");
        assert_eq!(run_to_end("print(max(4, 9, 2))\n"), "9\n");
        assert_eq!(run_to_end("print(sorted([3, 1, 2]))\n"), "[1, 2, 3]\n");
        assert_eq!(
            run_to_end("print(sorted([3, 1, 2], reverse=True))\n"),
            "[3, 2, 1]\n"
        );
        assert_eq!(
            run_to_end("print(sorted([3, 1, 2], reverse=False))\n"),
            "[1, 2, 3]\n"
        );
        assert_eq!(run_to_end("print(round(3.7))\n"), "4\n");
        assert_eq!(run_to_end("print(round(2.5))\n"), "2\n");
        assert_eq!(
            run_to_end("for i, x in enumerate([\"a\", \"b\"]):\n    print(i, x)\n"),
            "0 a\n1 b\n"
        );
        assert_eq!(
            run_to_end("for a, b in zip([1, 2], [3, 4]):\n    print(a, b)\n"),
            "1 3\n2 4\n"
        );
    }

    #[test]
    fn del_removes_list_and_dict_items() {
        // `del` desugars to a discarded `.pop(...)`; the step debugger must
        // execute both the list-index and dict-key forms.
        let src = "xs = [1, 2, 3, 4]\ndel xs[1]\nprint(xs)\n\
                   d = {\"a\": 1, \"b\": 2}\ndel d[\"a\"]\nprint(len(d))\nprint(d[\"b\"])\n";
        assert_eq!(run_to_end(src), "[1, 3, 4]\n1\n2\n");
    }

    #[test]
    fn each_step_advances_one_statement() {
        let mut s = Stepper::new("a = 1\nb = 2\nprint(a + b)\n").unwrap();
        assert_eq!(s.current_line(), Some(1));
        s.step(); // a = 1
        assert_eq!(s.current_line(), Some(2));
        s.step(); // b = 2
        assert_eq!(s.current_line(), Some(3));
        s.step(); // print
        assert_eq!(*s.status(), Status::Finished);
        assert_eq!(s.output(), "3\n");
    }

    #[test]
    fn arithmetic_matches_python() {
        assert_eq!(run_to_end("print(7 // 2)\n"), "3\n");
        assert_eq!(run_to_end("print(7 / 2)\n"), "3.5\n"); // true division -> float
        assert_eq!(run_to_end("print(7 % 3)\n"), "1\n");
        assert_eq!(run_to_end("print(-7 % 3)\n"), "2\n"); // divisor's sign
        assert_eq!(run_to_end("print(2 ** 10)\n"), "1024\n");
        assert_eq!(run_to_end("print(5.0)\n"), "5.0\n"); // float keeps .0
    }

    #[test]
    fn and_or_return_operands() {
        assert_eq!(run_to_end("print(2 and 1)\n"), "1\n");
        assert_eq!(run_to_end("print(0 or 7)\n"), "7\n");
    }

    #[test]
    fn loops_and_break_continue() {
        // sum 1..5 -> 15
        let src = "total = 0\nfor i in range(1, 6):\n    total = total + i\nprint(total)\n";
        assert_eq!(run_to_end(src), "15\n");

        // continue skips evens, break stops at 7
        let src = "i = 0\nwhile i < 100:\n    i = i + 1\n    if i % 2 == 0:\n        continue\n    if i > 7:\n        break\n    print(i)\n";
        assert_eq!(run_to_end(src), "1\n3\n5\n7\n");
    }

    #[test]
    fn lists_dicts_indexing_and_methods() {
        assert_eq!(
            run_to_end("xs = [1, 2, 3]\nxs[0] = 9\nprint(xs[0])\n"),
            "9\n"
        );
        assert_eq!(
            run_to_end("xs = [1]\nxs.append(2)\nprint(xs)\n"),
            "[1, 2]\n"
        );
        assert_eq!(
            run_to_end("d = {\"a\": 1}\nd[\"b\"] = 2\nprint(d[\"b\"])\n"),
            "2\n"
        );
        assert_eq!(run_to_end("print(\"cat\" in \"category\")\n"), "True\n");
    }

    #[test]
    fn string_methods_in_the_step_debugger() {
        // Closes the step debugger's string-method gap (this session's
        // native closure for the same gap): every method here must agree
        // byte-for-byte with the compiled backends (see BACKEND_DIVERGENCE.md).
        assert_eq!(run_to_end("print('Hello'.upper())\n"), "HELLO\n");
        assert_eq!(run_to_end("print('Hello'.lower())\n"), "hello\n");
        assert_eq!(run_to_end("print('  hi  '.strip())\n"), "hi\n");
        assert_eq!(run_to_end("print('  hi  '.lstrip())\n"), "hi  \n");
        assert_eq!(run_to_end("print('  hi  '.rstrip())\n"), "  hi\n");
        assert_eq!(
            run_to_end("print('hello world'.capitalize())\n"),
            "Hello world\n"
        );
        assert_eq!(
            run_to_end("print('hello world'.title())\n"),
            "Hello World\n"
        );
        assert_eq!(run_to_end("print('a b  c'.split())\n"), "['a', 'b', 'c']\n");
        assert_eq!(
            run_to_end("print('a,b,,c'.split(','))\n"),
            "['a', 'b', '', 'c']\n"
        );
        assert_eq!(run_to_end("print(','.join(['a', 'b', 'c']))\n"), "a,b,c\n");
        assert_eq!(run_to_end("print('abcabc'.find('bc'))\n"), "1\n");
        assert_eq!(run_to_end("print('abcabc'.find('zz'))\n"), "-1\n");
        assert_eq!(run_to_end("print('abcabc'.count('bc'))\n"), "2\n");
        assert_eq!(run_to_end("print('abcabc'.index('bc'))\n"), "1\n");
        assert_eq!(run_to_end("print('abc'.startswith('ab'))\n"), "True\n");
        assert_eq!(run_to_end("print('abc'.endswith('bc'))\n"), "True\n");
        assert_eq!(run_to_end("print('123'.isdigit())\n"), "True\n");
        assert_eq!(run_to_end("print('abc'.isalpha())\n"), "True\n");
        assert_eq!(run_to_end("print('42'.zfill(5))\n"), "00042\n");
        assert_eq!(run_to_end("print('-42'.zfill(5))\n"), "-0042\n");
        assert_eq!(
            run_to_end("print('abcabc'.replace('a', 'X'))\n"),
            "XbcXbc\n"
        );
        // A literal receiver (not a variable) — exercises the general
        // (non-Name) dispatch path, not the name-bound one.
        assert_eq!(run_to_end("print('ABC'.lower())\n"), "abc\n");
    }

    #[test]
    fn string_index_of_a_missing_substring_errors_in_the_step_debugger() {
        let mut s = Stepper::new("'abc'.index('zz')\n").unwrap();
        while s.is_paused() {
            s.step();
        }
        assert!(matches!(s.status(), Status::Error { message, .. } if message.contains("not in")));
    }

    #[test]
    fn list_methods_in_the_step_debugger() {
        assert_eq!(
            run_to_end("xs = [3, 1, 2]\nxs.sort()\nprint(xs)\n"),
            "[1, 2, 3]\n"
        );
        assert_eq!(
            run_to_end("xs = [1, 2, 3]\nxs.reverse()\nprint(xs)\n"),
            "[3, 2, 1]\n"
        );
        assert_eq!(
            run_to_end("xs = [1, 2]\nxs.extend([3, 4])\nprint(xs)\n"),
            "[1, 2, 3, 4]\n"
        );
        assert_eq!(
            run_to_end("xs = [1, 2, 3]\nxs.insert(1, 9)\nprint(xs)\n"),
            "[1, 9, 2, 3]\n"
        );
        assert_eq!(run_to_end("xs = [1, 2, 1, 3]\nprint(xs.count(1))\n"), "2\n");
        assert_eq!(run_to_end("xs = [1, 2, 3]\nprint(xs.index(2))\n"), "1\n");
    }

    #[test]
    fn dict_methods_in_the_step_debugger() {
        assert_eq!(
            run_to_end("d = {\"a\": 1}\nd.update({\"b\": 2})\nprint(d)\n"),
            "{'a': 1, 'b': 2}\n"
        );
        assert_eq!(run_to_end("d = {\"a\": 1}\nd.clear()\nprint(d)\n"), "{}\n");
        assert_eq!(
            run_to_end(
                "d = {\"a\": 1}\nprint(d.setdefault(\"a\", 9))\nprint(d.setdefault(\"b\", 9))\nprint(d)\n"
            ),
            "1\n9\n{'a': 1, 'b': 9}\n"
        );
        assert_eq!(
            run_to_end("d = {\"a\": 1, \"b\": 2}\nprint(d.keys())\n"),
            "['a', 'b']\n"
        );
        assert_eq!(
            run_to_end("d = {\"a\": 1, \"b\": 2}\nprint(d.values())\n"),
            "[1, 2]\n"
        );
        assert_eq!(
            run_to_end("d = {\"a\": 1, \"b\": 2}\nprint(d.items())\n"),
            "[('a', 1), ('b', 2)]\n"
        );
    }

    #[test]
    fn container_methods_also_work_inside_a_called_function() {
        // The atomic step-over executor (`eval_method`) is a SEPARATE, smaller
        // interpreter from the step machine above — it must support the same
        // methods or a call inside a user function would silently diverge.
        let src = "def f(xs):\n    xs.sort()\n    return xs\nprint(f([3, 1, 2]))\n\
                   def g(s):\n    return s.upper()\nprint(g('ok'))\n";
        assert_eq!(run_to_end(src), "[1, 2, 3]\nOK\n");
    }

    #[test]
    fn variables_panel_reflects_scope() {
        let mut s = Stepper::new("x = 10\ny = x * 2\n").unwrap();
        s.step(); // x = 10
        s.step(); // y = 20
        let vars = s.variables();
        assert_eq!(
            vars,
            vec![("x".into(), "10".into()), ("y".into(), "20".into())]
        );
    }

    #[test]
    fn watch_evaluates_in_current_scope() {
        let mut s = Stepper::new("xs = [3, 1, 2]\ntotal = 6\n").unwrap();
        s.step(); // xs = ...
        s.step(); // total = 6
        assert_eq!(s.eval_watch("len(xs)").unwrap(), "3");
        assert_eq!(s.eval_watch("total > 5").unwrap(), "True");
        assert_eq!(s.eval_watch("xs[0] + total").unwrap(), "9");
        assert!(s.eval_watch("nope").is_err()); // undefined name
    }

    #[test]
    fn watch_does_not_mutate_program_state() {
        let mut s = Stepper::new("xs = [1, 2]\n").unwrap();
        s.step();
        // Evaluating a mutating method in a watch runs on a scratch copy, so the
        // real list is untouched.
        let _ = s.eval_watch("xs.append(99)");
        assert_eq!(s.eval_watch("len(xs)").unwrap(), "2");
    }

    #[test]
    fn watchpoint_breaks_on_change() {
        let mut s = Stepper::new("x = 0\nx = 1\nx = 9\nprint(x)\n").unwrap();
        s.set_watchpoints(&["x".to_string()]);

        s.run(&[]); // x: undefined -> 0
        let (src, old, new) = s.watch_hit().expect("watchpoint should fire");
        assert_eq!(src, "x");
        assert_eq!((old.as_str(), new.as_str()), ("(undefined)", "0"));

        s.run(&[]); // x: 0 -> 1
        assert_eq!(s.watch_hit().unwrap().2, "1");

        s.run(&[]); // x: 1 -> 9
        assert_eq!(s.watch_hit().unwrap().2, "9");

        s.run(&[]); // no more changes -> runs to the end
        assert_eq!(s.watch_hit(), None);
        assert_eq!(*s.status(), Status::Finished);
        assert_eq!(s.output(), "9\n");
    }

    #[test]
    fn watchpoint_on_loop_variable_fires_each_iteration() {
        // Classic "watch i change" — break each time the loop var advances.
        let mut s = Stepper::new("for i in range(3):\n    print(i)\n").unwrap();
        s.set_watchpoints(&["i".to_string()]);
        let mut seen = Vec::new();
        for _ in 0..3 {
            s.run(&[]);
            if let Some((_, _, new)) = s.watch_hit() {
                seen.push(new);
            }
        }
        assert_eq!(seen, vec!["0", "1", "2"]);
    }

    #[test]
    fn run_stops_at_breakpoint() {
        let src = "a = 1\nb = 2\nc = 3\nd = 4\n";
        let mut s = Stepper::new(src).unwrap();
        s.run(&[3]); // run until line 3 is about to execute
        assert_eq!(s.current_line(), Some(3));
        assert_eq!(s.eval_watch("b").unwrap(), "2"); // a,b ran; c didn't
        assert!(s.eval_watch("c").is_err());
        s.run(&[]); // finish
        assert_eq!(*s.status(), Status::Finished);
    }

    #[test]
    fn unsupported_construct_stops_friendly() {
        // A class def isn't in the stepper (step-over mode) — friendly stop,
        // not a panic. (The call-stack Vm does run classes.)
        let mut s =
            Stepper::new("class C:\n    def m(self):\n        return 1\nprint(1)\n").unwrap();
        for _ in 0..10 {
            if !s.is_paused() {
                break;
            }
            s.step();
        }
        match s.status() {
            Status::Error { message, .. } => assert!(message.contains("step debugger")),
            other => panic!("expected a friendly error, got {other:?}"),
        }
    }

    #[test]
    fn functions_run_over_with_return_values() {
        // Calls to user functions execute atomically (step-over) and yield their
        // return value.
        assert_eq!(
            run_to_end("def double(n):\n    return n * 2\nprint(double(21))\n"),
            "42\n"
        );
        // Void function with a side effect.
        assert_eq!(
            run_to_end("def greet(name):\n    print(\"Hi \" + name)\ngreet(\"Bo\")\n"),
            "Hi Bo\n"
        );
        // Falling off the end returns None.
        assert_eq!(
            run_to_end("def nothing():\n    x = 1\nprint(nothing())\n"),
            "None\n"
        );
    }

    #[test]
    fn functions_support_recursion_and_defaults() {
        assert_eq!(
            run_to_end(
                "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n"
            ),
            "120\n"
        );
        assert_eq!(
            run_to_end("def inc(x, by=1):\n    return x + by\nprint(inc(5))\nprint(inc(5, 10))\n"),
            "6\n15\n"
        );
    }

    #[test]
    fn a_call_is_a_single_step_over() {
        // def registers (step 1); the call line runs the whole function in one
        // step (step 2), leaving the result in scope.
        let mut s = Stepper::new("def double(n):\n    return n * 2\nx = double(21)\n").unwrap();
        assert_eq!(s.current_line(), Some(1)); // def
        s.step(); // register def
        assert_eq!(s.current_line(), Some(3)); // body line 2 is not stepped
        s.step(); // x = double(21), function runs atomically
        assert_eq!(*s.status(), Status::Finished);
        assert_eq!(s.eval_watch("x").unwrap(), "42");
    }

    #[test]
    fn nested_loops_produce_correct_output() {
        let src = "for i in range(2):\n    for j in range(2):\n        print(i * 10 + j)\n";
        assert_eq!(run_to_end(src), "0\n1\n10\n11\n");
    }

    // --- CPS VM (step-into + call stack) ---

    fn vm_run(src: &str) -> String {
        let mut vm = Vm::new(src).expect("parse");
        let mut guard = 0;
        while vm.is_paused() {
            vm.step();
            guard += 1;
            assert!(guard < 100_000, "ran away");
        }
        assert_eq!(
            *vm.status(),
            Status::Finished,
            "did not finish: {:?}",
            vm.status()
        );
        vm.output().to_string()
    }

    #[test]
    fn vm_runs_classes_end_to_end() {
        // The canonical curriculum program: inheritance, override,
        // super().__init__, attrs, methods, list-in-attr — same output as
        // the compiled backends.
        let out = vm_run(
            "class Animal:\n    def __init__(self, name):\n        self.name = name\n    def speak(self):\n        return self.name + \" makes a sound\"\nclass Dog(Animal):\n    def __init__(self, name):\n        super().__init__(name)\n        self.tricks = []\n    def speak(self):\n        return self.name + \" barks\"\n    def learn(self, t):\n        self.tricks.append(t)\nd = Dog(\"Rex\")\nd.learn(\"sit\")\nprint(d.speak())\nprint(d.tricks)\na = Animal(\"Cat\")\nprint(a.speak())\n",
        );
        assert_eq!(out, "Rex barks\n['sit']\nCat makes a sound\n");
    }

    #[test]
    fn vm_objects_have_reference_semantics_and_dunders() {
        // Aliasing: writes through one name are visible through the other.
        let out = vm_run(
            "class Box:\n    def __init__(self):\n        self.v = 0\nb = Box()\nc = b\nc.v = 9\nprint(b.v)\n",
        );
        assert_eq!(out, "9\n");
        // __add__ + __str__ (print dispatches __str__ via a frame call).
        let out2 = vm_run(
            "class Vec:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def __add__(self, o):\n        return Vec(self.x + o.x, self.y + o.y)\n    def __str__(self):\n        return \"Vec(\" + str(self.x) + \", \" + str(self.y) + \")\"\nv = Vec(1, 2) + Vec(3, 4)\nprint(v)\n",
        );
        assert_eq!(out2, "Vec(4, 6)\n");
        // __eq__ (direct + !=), and identity/int-mix for a class WITHOUT
        // __eq__ (CPython's default) — same shapes as the native oracle.
        let out3 = vm_run(
            "class Pt:\n    def __init__(self, x):\n        self.x = x\n    def __eq__(self, o):\n        return self.x == o.x\nprint(Pt(3) == Pt(3))\nprint(Pt(3) == Pt(4))\nprint(Pt(3) != Pt(4))\n",
        );
        assert_eq!(out3, "True\nFalse\nTrue\n");
        let out3b = vm_run(
            "class Tag:\n    def __init__(self):\n        self.z = 0\nt1 = Tag()\nt2 = Tag()\nprint(t1 == t1)\nprint(t1 == t2)\nprint(t1 == 5)\n",
        );
        assert_eq!(out3b, "True\nFalse\nFalse\n");
        let out4 = vm_run(
            "class Deck:\n    def __init__(self):\n        self.cards = [\"A\", \"K\"]\n    def __len__(self):\n        return len(self.cards)\n    def __getitem__(self, i):\n        return self.cards[i]\nd = Deck()\nprint(len(d))\nprint(d[1])\n",
        );
        assert_eq!(out4, "2\nK\n");
    }

    #[test]
    fn vm_class_variables_shadow_and_inherit() {
        // Instance attrs shadow; the class var stays untouched for others;
        // base-class vars are visible through the chain.
        let out = vm_run(
            "class Counter:\n    kind = \"counter\"\n    limit = 10\n    def __init__(self):\n        self.n = 0\nc = Counter()\nprint(c.kind)\nprint(c.limit)\nc.limit = 3\nprint(c.limit)\nd = Counter()\nprint(d.limit)\n",
        );
        assert_eq!(out, "counter\n10\n3\n10\n");
        let out2 = vm_run(
            "class Base:\n    tag = \"b\"\nclass Kid(Base):\n    extra = 1\n    def __init__(self):\n        self.z = 0\nk = Kid()\nprint(k.tag)\nprint(k.extra)\n",
        );
        assert_eq!(out2, "b\n1\n");
    }

    #[test]
    fn vm_class_name_access_reads_and_writes() {
        // The instance-counter idiom + chain read + shadow guard.
        let out = vm_run(
            "class Counter:\n    made = 0\n    def __init__(self):\n        Counter.made = Counter.made + 1\na = Counter()\nb = Counter()\nprint(Counter.made)\nprint(a.made)\n",
        );
        assert_eq!(out, "2\n2\n");
        let out2 = vm_run(
            "class Base:\n    tag = \"b\"\nclass Kid(Base):\n    def __init__(self):\n        self.z = 0\nprint(Kid.tag)\n",
        );
        assert_eq!(out2, "b\n");
        let out3 = vm_run("class K:\n    v = 1\nK = 5\nprint(K)\n");
        assert_eq!(out3, "5\n");
    }

    #[test]
    fn vm_class_errors_are_friendly() {
        // A dunder the debugger doesn't dispatch is a clean error at the
        // class statement (parity with the native backend's guard).
        let mut vm =
            Vm::new("class V:\n    def __setitem__(self, k, v):\n        return 0\n").unwrap();
        while vm.is_paused() {
            vm.step();
        }
        assert!(
            matches!(vm.status(), Status::Error { message, .. } if message.contains("__setitem__")),
            "{:?}",
            vm.status()
        );
    }

    #[test]
    fn vm_opens_a_staged_file_and_reads_it_line_by_line() {
        // readline() returns each line WITH its trailing newline (matching
        // real Python), then "" forever after EOF — never an error, the same
        // way a real file object behaves at end of file.
        let mut vm = Vm::new(
            "f = open(\"data.txt\")\n\
             print(f.readline())\n\
             print(f.readline())\n\
             print(f.readline())\n\
             f.close()\n",
        )
        .unwrap();
        vm.set_files(HashMap::from([(
            "data.txt".to_string(),
            "one\ntwo".to_string(),
        )]));
        let mut guard = 0;
        while vm.is_paused() {
            vm.step();
            guard += 1;
            assert!(guard < 100_000, "ran away");
        }
        assert_eq!(*vm.status(), Status::Finished, "{:?}", vm.status());
        // print() adds its own newline, so "one\n" shows as a blank line after it.
        assert_eq!(vm.output(), "one\n\ntwo\n\n");
    }

    #[test]
    fn vm_reads_the_whole_remaining_content_at_once() {
        let mut vm = Vm::new(
            "f = open(\"data.txt\")\n\
             print(f.readline())\n\
             print(f.read())\n\
             f.close()\n",
        )
        .unwrap();
        vm.set_files(HashMap::from([(
            "data.txt".to_string(),
            "one\ntwo\nthree".to_string(),
        )]));
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(*vm.status(), Status::Finished, "{:?}", vm.status());
        // readline() took "one\n"; read() takes everything left ("two\nthree").
        assert_eq!(vm.output(), "one\n\ntwo\nthree\n");
    }

    #[test]
    fn vm_writes_are_readable_back_both_after_the_run_and_within_it() {
        // write -> close -> reopen for read -> read, all in one run: close
        // commits into file_sources, so the reopen sees it immediately —
        // the single most common file-I/O exercise ("did I save it right?").
        let mut vm = Vm::new(
            "f = open(\"out.txt\", \"w\")\n\
             n = f.write(\"hi\")\n\
             print(n)\n\
             f.close()\n\
             g = open(\"out.txt\", \"r\")\n\
             print(g.read())\n\
             g.close()\n",
        )
        .unwrap();
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(*vm.status(), Status::Finished, "{:?}", vm.status());
        assert_eq!(vm.output(), "2\nhi\n"); // write() returns the character count
        // Still surfaced after the run too, unaffected by the in-run commit.
        assert_eq!(
            vm.file_writes().get("out.txt").map(String::as_str),
            Some("hi")
        );
        // A read-mode file's writes never appear (nothing was opened "w").
        assert!(!vm.file_writes().contains_key("missing.txt"));
    }

    #[test]
    fn vm_runs_with_statement_on_a_real_file_object() {
        // The with-statement test elsewhere uses a user-defined class; this
        // confirms the SAME desugaring also works against the compiler's own
        // File object, closing the loop `with open(...) as f:` promises.
        let mut vm =
            Vm::new("with open(\"data.txt\") as f:\n    print(f.readline())\nprint(\"done\")\n")
                .unwrap();
        vm.set_files(HashMap::from([(
            "data.txt".to_string(),
            "hello".to_string(),
        )]));
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(*vm.status(), Status::Finished, "{:?}", vm.status());
        assert_eq!(vm.output(), "hello\ndone\n");
    }

    #[test]
    fn vm_rejects_opening_a_file_that_was_never_staged() {
        let mut vm = Vm::new("f = open(\"missing.txt\")\n").unwrap();
        while vm.is_paused() {
            vm.step();
        }
        assert!(
            matches!(vm.status(), Status::Error { message, .. } if message.contains("missing.txt")),
            "{:?}",
            vm.status()
        );
    }

    #[test]
    fn vm_runs_with_statement_via_enter_exit() {
        // `with` desugars (parser.rs) to plain `.method()` calls, so any
        // class implementing the protocol works, not just a compiler-special
        // one — this is the debugger-engine half of that claim; codegen.rs
        // (WASM) and llvm.rs (native) are the other two.
        let out = vm_run(
            "class Guard:\n    def __init__(self, label):\n        self.label = label\n    \
             def __enter__(self):\n        print(\"enter \" + self.label)\n        return self\n    \
             def __exit__(self, a, b, c):\n        print(\"exit \" + self.label)\n    \
             def ping(self):\n        print(\"ping \" + self.label)\n\n\
             with Guard(\"outer\") as g:\n    g.ping()\n    print(\"inside\")\n\
             print(\"after\")\n",
        );
        assert_eq!(out, "enter outer\nping outer\ninside\nexit outer\nafter\n");
    }

    #[test]
    fn both_engines_support_sets() {
        // Literal dedup + len + membership + print (sorted display, like Run).
        let basics = "s = {3, 1, 2, 1}\nprint(s)\nprint(len(s))\nprint(2 in s)\nprint(9 in s)\n";
        let want = "{1, 2, 3}\n3\nTrue\nFalse\n";
        assert_eq!(run_to_end(basics), want);
        assert_eq!(vm_run(basics), want);
        // Strings sort lexicographically; mixed types keep insertion order.
        assert_eq!(
            vm_run("print({\"c\", \"a\", \"b\"})\n"),
            "{'a', 'b', 'c'}\n"
        );
        assert_eq!(vm_run("print({2, \"a\", 1})\n"), "{2, 'a', 1}\n");

        // Set theory operators.
        let ops = "a = {1, 2, 3}\nb = {2, 3, 4}\n\
                   print(len(a & b))\nprint(len(a | b))\nprint(len(a - b))\nprint(len(a ^ b))\n";
        assert_eq!(run_to_end(ops), "2\n4\n1\n2\n");
        assert_eq!(vm_run(ops), "2\n4\n1\n2\n");

        // Iteration, empty set, set()/set-from-list, and int bitwise (not a set op).
        assert_eq!(
            vm_run("s = {1, 2, 3}\nt = 0\nfor x in s:\n    t = t + x\nprint(t)\n"),
            "6\n"
        );
        assert_eq!(vm_run("print(set())\n"), "set()\n");
        assert_eq!(vm_run("print(len(set([1, 1, 2])))\n"), "2\n");
        assert_eq!(vm_run("print(6 & 3)\n"), "2\n");
    }

    /// Run to completion, returning the error message if it stopped on one.
    fn vm_err(src: &str) -> Option<String> {
        let mut vm = Vm::new(src).expect("parse");
        let mut guard = 0;
        while vm.is_paused() {
            vm.step();
            guard += 1;
            assert!(guard < 100_000, "ran away");
        }
        match vm.status() {
            Status::Error { message, .. } => Some(message.clone()),
            _ => None,
        }
    }

    #[test]
    fn both_engines_support_real_tuples() {
        // Build, print (parens), len, index-read, membership, iteration.
        let t = "t = (1, 2, 3)\nprint(t)\nprint(len(t))\nprint(t[0])\nprint(2 in t)\n";
        assert_eq!(run_to_end(t), "(1, 2, 3)\n3\n1\nTrue\n");
        assert_eq!(vm_run(t), "(1, 2, 3)\n3\n1\nTrue\n");
        // 1-tuple keeps its comma; empty is ().
        assert_eq!(vm_run("print((5,))\nprint(())\n"), "(5,)\n()\n");
        // A tuple is NOT equal to a list with the same elements.
        assert_eq!(vm_run("print((1, 2) == [1, 2])\n"), "False\n");
        // Sum a tuple via iteration.
        assert_eq!(
            vm_run("t = (1, 2, 3)\ns = 0\nfor x in t:\n    s = s + x\nprint(s)\n"),
            "6\n"
        );
    }

    #[test]
    fn sets_reject_mutable_elements() {
        // A list/dict/set can't be a set member; a tuple can.
        assert!(vm_err("s = {[1, 2]}\n").is_some_and(|e| e.contains("tuple")));
        assert!(vm_err("s = set()\ns.add([1])\n").is_some_and(|e| e.contains("tuple")));
        // Tuples are valid set elements.
        assert_eq!(vm_run("s = {(1, 2), (3, 4)}\nprint(len(s))\n"), "2\n");
    }

    #[test]
    fn tuples_are_immutable() {
        // Item assignment to a tuple is an error, not a silent mutation — and
        // the words are the compiled backends' words (the message table).
        let err = vm_err("t = (1, 2)\nt[0] = 9\n").expect("should error");
        assert!(
            err.contains("'tuple' object does not support item assignment"),
            "{err}"
        );
    }

    #[test]
    fn vm_error_messages_are_the_compiled_backends_messages() {
        // One row per converged family: the Stepper's words ARE the message
        // table's words, which are the compiled backends' words. Before the
        // table the Vm said e.g. "list index out of range" (no exception
        // name) and "unsupported operand types for -: int and str" where the
        // compiled program said "TypeError: expected a number, got 'str'".
        let cases = [
            (
                "xs = [1, 2]\nprint(xs[9])\n",
                "IndexError: list index out of range",
            ),
            (
                "s = 'abc'\nprint(s[9])\n",
                "IndexError: string index out of range",
            ),
            ("d = {'a': 1}\nprint(d['b'])\n", "KeyError: 'b'"),
            ("print(1 / 0)\n", "ZeroDivisionError: division by zero"),
            ("print(1 // 0)\n", "ZeroDivisionError: division by zero"),
            (
                "xs = [1, 'one']\nprint(xs[0] - xs[1])\n",
                "TypeError: expected a number, got 'str'",
            ),
            (
                "xs = [1, 'one']\nprint(xs[0] < xs[1])\n",
                "TypeError: expected a number, got 'str'",
            ),
            // Via a mixed list (Dyn to the checker) — the static shapes are
            // compile errors since phase C, rule 3.
            (
                "xs = [5, 'a']\nprint(len(xs[0]))\n",
                "TypeError: object of type 'int' has no len()",
            ),
            (
                "xs = [5, 'a']\nprint(xs[0][0])\n",
                "TypeError: 'int' object is not subscriptable",
            ),
            (
                "a, b = 1, 2, 3\nprint(a)\n",
                "ValueError: wrong number of values to unpack",
            ),
            (
                "print(int('abc'))\n",
                "ValueError: invalid literal for int() with base 10: 'abc'",
            ),
            (
                "print(int('99999999999'))\n",
                "OverflowError: this calculation went outside the range of whole numbers we can store (-2147483648 to 2147483647)",
            ),
            (
                "print(int('2147483648'))\n",
                "OverflowError: this calculation went outside the range of whole numbers we can store (-2147483648 to 2147483647)",
            ),
            (
                "print(float('abc'))\n",
                "ValueError: could not convert string to float: 'abc'",
            ),
            // Straggler copies in the shared value ops (verified against the
            // compiled backend by running it):
            (
                "n = 5\nprint(list(n))\n",
                "TypeError: object of type 'int' has no len()",
            ),
            (
                "xs = [1, 'a']\nprint(xs[0] & xs[1])\n",
                "TypeError: expected a number, got 'str'",
            ),
            ("xs = []\nxs.pop()\n", "IndexError: list index out of range"),
            ("d = {'a': 1}\nprint(d.pop('b'))\n", "KeyError: 'b'"),
        ];
        for (src, want) in cases {
            let err = vm_err(src).unwrap_or_else(|| panic!("no error for {src}"));
            assert_eq!(err, want, "for {src}");
        }
    }

    #[test]
    fn both_engines_support_set_methods() {
        // add / discard / remove / len, then membership.
        let m = "s = {1, 2}\ns.add(3)\ns.add(2)\ns.discard(1)\nprint(len(s))\nprint(2 in s)\nprint(1 in s)\n";
        assert_eq!(run_to_end(m), "2\nTrue\nFalse\n");
        assert_eq!(vm_run(m), "2\nTrue\nFalse\n");
        // union / intersection / issubset (non-mutating, set arg).
        let n = "a = {1, 2}\nb = {2, 3}\nprint(len(a.union(b)))\nprint(len(a.intersection(b)))\nprint(a.issubset({1, 2, 3}))\n";
        assert_eq!(run_to_end(n), "3\n1\nTrue\n");
        assert_eq!(vm_run(n), "3\n1\nTrue\n");
        // pop shrinks; clear empties.
        assert_eq!(
            vm_run("s = {7}\nx = s.pop()\nprint(x)\nprint(len(s))\n"),
            "7\n0\n"
        );
        assert_eq!(vm_run("s = {1, 2}\ns.clear()\nprint(len(s))\n"), "0\n");
    }

    #[test]
    fn vm_slicing_range_and_unpack() {
        // The same step-debugger fixes in call-stack (Vm) mode: slices, range
        // as a value, reductions, and tuple unpacking (enumerate/zip loops).
        assert_eq!(vm_run("print([1, 2, 3, 4][1:3])\n"), "[2, 3]\n");
        assert_eq!(vm_run("print([1, 2, 3][::-1])\n"), "[3, 2, 1]\n");
        assert_eq!(vm_run("print(\"hello\"[1:4])\n"), "ell\n");
        assert_eq!(vm_run("print(list(range(4)))\n"), "[0, 1, 2, 3]\n");
        assert_eq!(
            vm_run("print(sum([1, 2, 3]))\nprint(max([4, 9, 2]))\n"),
            "6\n9\n"
        );
        assert_eq!(
            vm_run("print(sorted([3, 1, 2], reverse=True))\n"),
            "[3, 2, 1]\n"
        );
        // import math, and classes + nested def already run in call-stack mode.
        assert_eq!(
            vm_run("import math\nprint(math.sqrt(16.0))\nprint(math.floor(2.9))\n"),
            "4.0\n2\n"
        );
        assert_eq!(
            vm_run(
                "class C:\n    def __init__(self, n):\n        self.n = n\n    def two(self):\n        return self.n * 2\nprint(C(5).two())\n"
            ),
            "10\n"
        );
        assert_eq!(
            vm_run(
                "def outer():\n    def inner():\n        return 7\n    return inner()\nprint(outer())\n"
            ),
            "7\n"
        );
        assert_eq!(vm_run("a, *rest = [1, 2, 3]\nprint(rest)\n"), "[2, 3]\n");
        assert_eq!(
            vm_run("for i, x in enumerate([\"a\", \"b\"]):\n    print(i, x)\n"),
            "0 a\n1 b\n"
        );
        assert_eq!(
            vm_run("for a, b in zip([1, 2], [3, 4]):\n    print(a, b)\n"),
            "1 3\n2 4\n"
        );
    }

    #[test]
    fn vm_runs_control_flow() {
        assert_eq!(vm_run("x = 5\nprint(x)\n"), "5\n");
        assert_eq!(vm_run("print(7 // 2)\nprint(7 / 2)\n"), "3\n3.5\n");
        assert_eq!(vm_run("print(2 and 0 or 9)\n"), "9\n");
        let src = "total = 0\nfor i in range(1, 6):\n    total = total + i\nprint(total)\n";
        assert_eq!(vm_run(src), "15\n");
        let src =
            "i = 0\nwhile i < 5:\n    i = i + 1\n    if i == 3:\n        continue\n    print(i)\n";
        assert_eq!(vm_run(src), "1\n2\n4\n5\n");
    }

    #[test]
    fn vm_runs_functions_with_recursion_and_defaults() {
        assert_eq!(
            vm_run("def double(n):\n    return n * 2\nprint(double(21))\n"),
            "42\n"
        );
        assert_eq!(
            vm_run(
                "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n"
            ),
            "120\n"
        );
        assert_eq!(
            vm_run("def inc(x, by=1):\n    return x + by\nprint(inc(5))\nprint(inc(5, 10))\n"),
            "6\n15\n"
        );
    }

    #[test]
    fn vm_evaluates_a_ternary() {
        // The CPS machine handles `then if cond else orelse` via Task::Ternary.
        let mut vm = Vm::new("x = 5\ny = \"big\" if x > 3 else \"small\"\nprint(y)\n").unwrap();
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(vm.output(), "big\n");
        // The other branch is never evaluated (would divide by zero here).
        let mut vm2 = Vm::new("x = 0\nprint(-1 if x == 0 else 7 // x)\n").unwrap();
        while vm2.is_paused() {
            vm2.step();
        }
        assert_eq!(vm2.output(), "-1\n");
    }

    #[test]
    fn vm_random_matches_the_compiled_backend_bit_for_bit() {
        // The Vm's xorshift mirror seeded 42 must reproduce EXACTLY the
        // digits the compiled module produces under the test hosts'
        // env.seed = 42 — pinned in tests/exec.rs
        // (random_is_deterministic_from_the_host_seed). A drift here is a
        // language fork between Run and Step.
        let src = "import random\nprint(random.randint(1, 100))\nprint(random.random())\nxs = [1, 2, 3, 4, 5]\nrandom.shuffle(xs)\nprint(xs)\nprint(random.choice(xs))\n";
        let mut vm = Vm::new(src).unwrap();
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(vm.output(), "70\n0.3458877553122256\n[1, 2, 3, 5, 4]\n1\n");
    }

    #[test]
    fn vm_steps_into_a_function_and_shows_the_call_stack() {
        let mut vm = Vm::new("def f(n):\n    return n + 1\nprint(f(10))\n").unwrap();
        assert_eq!(vm.current_line(), Some(1)); // def
        vm.step(); // register def -> next is line 3 (print)
        assert_eq!(vm.current_line(), Some(3));
        // At line 3 only the module frame exists.
        assert_eq!(vm.call_stack(), vec![("<module>".to_string(), 3)]);
        vm.step(); // step INTO f -> paused on line 2 (return), inside f
        assert_eq!(vm.current_line(), Some(2));
        let stack = vm.call_stack();
        assert_eq!(stack.len(), 2, "should be inside f: {stack:?}");
        assert_eq!(stack[0].0, "f");
        assert_eq!(stack[1].0, "<module>");
        // n is visible in f's frame.
        assert_eq!(vm.eval_watch("n").unwrap(), "10");
        // Finish.
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(vm.output(), "11\n");
    }

    #[test]
    fn vm_call_stack_deepens_with_recursion() {
        let mut vm =
            Vm::new("def down(n):\n    if n > 0:\n        down(n - 1)\ndown(3)\n").unwrap();
        let mut max_depth = 0;
        while vm.is_paused() {
            max_depth = max_depth.max(vm.call_stack().len());
            vm.step();
        }
        // module + down(3)+down(2)+down(1)+down(0) = 5 frames at the deepest.
        assert!(
            max_depth >= 5,
            "recursion should deepen the stack, got {max_depth}"
        );
    }

    #[test]
    fn vm_lists_dicts_indexing_methods_foreach() {
        assert_eq!(vm_run("xs = [1, 2, 3]\nxs[0] = 9\nprint(xs[0])\n"), "9\n");
        assert_eq!(vm_run("xs = [1]\nxs.append(2)\nprint(xs)\n"), "[1, 2]\n");
        assert_eq!(
            vm_run("d = {\"a\": 1}\nd[\"b\"] = 2\nprint(d[\"b\"])\n"),
            "2\n"
        );
        assert_eq!(
            vm_run("total = 0\nfor v in [10, 20, 30]:\n    total = total + v\nprint(total)\n"),
            "60\n"
        );
        // A function that builds and returns a list, stepped over by Continue.
        assert_eq!(
            vm_run(
                "def squares(n):\n    out = []\n    for i in range(n):\n        out.append(i * i)\n    return out\nprint(squares(4))\n"
            ),
            "[0, 1, 4, 9]\n"
        );
    }

    #[test]
    fn vm_watch_reads_index_into_a_list() {
        let mut vm = Vm::new("xs = [5, 6, 7]\nprint(xs)\n").unwrap();
        vm.step(); // xs = ...
        assert_eq!(vm.eval_watch("xs[1]").unwrap(), "6");
        assert!(vm.eval_watch("len(xs)").unwrap_err().contains("call-stack"));
    }

    #[test]
    fn vm_step_over_runs_a_call_without_descending() {
        let mut vm = Vm::new("def f(n):\n    return n + 1\nx = f(10)\nprint(x)\n").unwrap();
        vm.step(); // def -> line 3 (x = f(10))
        assert_eq!(vm.current_line(), Some(3));
        vm.step_over(); // run f(10) atomically -> line 4 (print), still top-level
        assert_eq!(vm.current_line(), Some(4));
        assert_eq!(
            vm.call_stack().len(),
            1,
            "step-over must not descend into f"
        );
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(vm.output(), "11\n");
    }

    #[test]
    fn vm_step_out_returns_to_the_caller() {
        let mut vm =
            Vm::new("def f(n):\n    a = n + 1\n    return a\nx = f(5)\nprint(x)\n").unwrap();
        vm.step(); // def -> line 4 (x = f(5))
        vm.step(); // step INTO f -> line 2 (a = n + 1)
        assert_eq!(vm.call_stack().len(), 2);
        assert_eq!(vm.current_line(), Some(2));
        vm.step_out(); // finish f, back in the caller -> line 5 (print)
        assert_eq!(
            vm.call_stack().len(),
            1,
            "step-out should return to <module>"
        );
        assert_eq!(vm.current_line(), Some(5));
        assert_eq!(vm.last_return().as_deref(), Some("6"), "f returned 6");
        while vm.is_paused() {
            vm.step();
        }
        assert_eq!(vm.output(), "6\n");
    }

    #[test]
    fn vm_watchpoint_breaks_on_change() {
        let mut vm = Vm::new("x = 0\nx = 1\nx = 9\nprint(x)\n").unwrap();
        vm.set_watchpoints(&["x".to_string()]);
        vm.run(&[]);
        assert_eq!(vm.watch_hit().unwrap().2, "0");
        vm.run(&[]);
        assert_eq!(vm.watch_hit().unwrap().2, "1");
    }

    fn run_vm_to_end(src: &str) -> (String, Status) {
        let mut v = Vm::new(src).expect("parse");
        let mut guard = 0;
        while v.is_paused() {
            v.step();
            guard += 1;
            assert!(guard < 100_000, "ran away");
        }
        (v.output().to_string(), v.status().clone())
    }

    /// **Fixed 2026-11-01**: a list passed into a function and mutated
    /// there (`xs.append(v)`) now mutates the caller's list, matching
    /// Python's shared-binding semantics — `Value::List` is `Rc`-shared
    /// (like `Value::Object` already was), so `args[i].clone()` at the
    /// call-binding site shares the list instead of deep-copying it.
    #[test]
    fn list_mutated_through_function_param_persists_to_the_caller() {
        let (out, status) =
            run_vm_to_end("def add(xs, v):\n    xs.append(v)\nh = []\nadd(h, 1)\nprint(h)\n");
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[1]\n");
    }

    /// **Known bug, not yet fixed**: assigning through a subscript whose
    /// base isn't a bare name (`obj.attr[i] = v`) is rejected outright,
    /// even though reading `obj.attr[i]` and calling `obj.attr.append(v)`
    /// both work. Once fixed, this should run cleanly instead of erroring.
    #[test]
    fn item_assignment_through_attribute_subscript_works() {
        let (out, status) = run_vm_to_end(
            "class Node:\n    def __init__(self):\n        self.children = [1, 2, 3]\nn = Node()\nn.children[0] = 9\nprint(n.children)\n",
        );
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[9, 2, 3]\n");
    }

    /// **Known bug, not yet fixed**: unpacking into subscript targets
    /// (`a[i], a[j] = a[j], a[i]`, the classic swap idiom) is rejected —
    /// at plain module level, not just inside a function call despite the
    /// error message's "call-stack mode" wording.
    #[test]
    fn tuple_unpack_into_subscript_targets_swaps_correctly() {
        let (out, status) = run_vm_to_end("a = [1, 2, 3]\na[0], a[1] = a[1], a[0]\nprint(a)\n");
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[2, 1, 3]\n");
    }

    /// The swap idiom still works from inside a function call (`call-stack
    /// mode`, the case the old rejection message specifically named).
    #[test]
    fn tuple_unpack_into_subscript_targets_swaps_correctly_inside_a_function() {
        let (out, status) = run_vm_to_end(
            "def swap(a):\n    a[0], a[1] = a[1], a[0]\na = [1, 2, 3]\nswap(a)\nprint(a)\n",
        );
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[2, 1, 3]\n");
    }

    /// Binding a list to a second name aliases it — mutating through
    /// either name is visible through the other, matching Python.
    #[test]
    fn two_names_bound_to_one_list_share_mutations_vm() {
        let (out, status) = run_vm_to_end("h = []\nk = h\nk.append(1)\nprint(h)\nprint(k)\n");
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[1]\n[1]\n");
    }

    /// Slicing, `list(x)`, and `+` concatenation must each produce an
    /// independent list — the representation change that made plain
    /// binding alias (above) must NOT make an actual copy operation
    /// alias too.
    #[test]
    fn slicing_and_list_and_concat_still_produce_independent_copies_vm() {
        let (out, status) = run_vm_to_end(
            "a = [1, 2, 3]\nb = a[0:2]\nb.append(99)\nprint(a)\nc = list(a)\nc.append(99)\nprint(a)\nd = a + []\nd.append(99)\nprint(a)\n",
        );
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[1, 2, 3]\n[1, 2, 3]\n[1, 2, 3]\n");
    }

    /// Same two properties for dicts and sets.
    #[test]
    fn dict_and_set_alias_on_bind_but_not_through_dict_and_set_constructors_vm() {
        let (out, status) = run_vm_to_end(
            "d1 = {\"x\": 1}\nd2 = d1\nd2[\"x\"] = 2\nprint(d1[\"x\"])\ns1 = {1, 2}\ns2 = s1\ns2.add(3)\nprint(len(s1))\n",
        );
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "2\n3\n");
    }

    /// The same generalized assignment-target path for a dict attribute,
    /// not just a list — `assign_index_value` doesn't special-case either
    /// container type by how its target expression was spelled.
    #[test]
    fn item_assignment_through_a_dict_attribute_works() {
        let (out, status) = run_vm_to_end(
            "class Node:\n    def __init__(self):\n        self.fields = {\"x\": 1}\nn = Node()\nn.fields[\"x\"] = 2\nprint(n.fields[\"x\"])\n",
        );
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "2\n");
    }

    /// A doubly-nested subscript target (`a[0][1] = v`) — `target` here is
    /// itself an `Index` expression, not a bare name or an attribute,
    /// confirming `assign_index_value` really does work for ANY expression
    /// that evaluates to a mutable container, not just the two shapes the
    /// fix specifically set out to unblock.
    #[test]
    fn nested_subscript_assignment_works_vm() {
        let (out, status) = run_vm_to_end("a = [[1, 2], [3, 4]]\na[0][1] = 99\nprint(a)\n");
        assert_eq!(status, Status::Finished);
        assert_eq!(out, "[[1, 99], [3, 4]]\n");
    }
}
