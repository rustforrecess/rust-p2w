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
    List(Vec<Value>),
    /// An immutable tuple `(a, b)`. A distinct type from `List`: it has no item
    /// assignment and isn't equal to a list with the same elements.
    Tuple(Vec<Value>),
    /// Insertion-ordered unique elements (matches the compiled backends; sets are
    /// conceptually unordered, so equality below ignores order).
    Set(Vec<Value>),
    /// Insertion-ordered key/value pairs, like Python dicts.
    Dict(Vec<(Value, Value)>),
    None,
}

impl Value {
    /// Python truthiness.
    fn truthy(&self) -> bool {
        match self {
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Bool(b) => *b,
            Value::Str(s) => !s.is_empty(),
            Value::List(v) => !v.is_empty(),
            Value::Tuple(v) => !v.is_empty(),
            Value::Set(v) => !v.is_empty(),
            Value::Dict(d) => !d.is_empty(),
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
            Value::Int(n) => n.to_string(),
            // Rust's float Debug is shortest-round-trip like Python's repr, and
            // keeps the trailing `.0` Python shows (e.g. 5.0 -> "5.0").
            Value::Float(f) => format!("{f:?}"),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Value::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Value::None => "None".to_string(),
            Value::List(v) => {
                let items: Vec<String> = v.iter().map(Value::py_repr).collect();
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
            Value::Set(v) if v.is_empty() => "set()".to_string(),
            Value::Set(v) => {
                // Canonical *sorted* display when elements are homogeneously
                // orderable; mixed-type sets fall back to insertion order. Display
                // only — storage/iteration keep insertion order. Matches the WASM
                // backend's `$print_set`.
                let items: Vec<String> = sorted_set_view(v).iter().map(|x| x.py_repr()).collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Dict(d) => {
                let items: Vec<String> = d
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
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        // Sets are unordered: equal iff same size and every element is in both.
        (Value::Set(x), Value::Set(y)) => {
            x.len() == y.len() && x.iter().all(|p| y.iter().any(|q| py_eq(p, q)))
        }
        (Value::Dict(x), Value::Dict(y)) => {
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

    fn clone_scope(&self) -> HashMap<String, Value> {
        self.scope.clone()
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
            StmtKind::ClassDef { .. }
            | StmtKind::SetAttr { .. }
            | StmtKind::UnpackAssign { .. }
            | StmtKind::Import(_) => Err(format!(
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
        assign_index_in(&mut self.scope, target, index, value)
    }

    fn eval_int(&mut self, e: &Expr, what: &str) -> Result<i64, String> {
        let v = self.eval(e)?;
        as_int(&v).ok_or_else(|| format!("{what} must be an integer"))
    }

    fn iterable_items(&mut self, e: &Expr) -> Result<Vec<Value>, String> {
        match self.eval(e)? {
            Value::List(v) | Value::Set(v) | Value::Tuple(v) => Ok(v),
            Value::Str(s) => Ok(s.chars().map(|c| Value::Str(c.to_string())).collect()),
            Value::Dict(d) => Ok(d.into_iter().map(|(k, _)| k).collect()),
            other => Err(format!("can't loop over {}", type_name(&other))),
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
                        _ => Err(format!("can't negate {}", type_name(&v))),
                    },
                }
            }
            ExprKind::Bin(op, a, b) => Self::eval_bin(funcs, scope, out, *op, a, b),
            ExprKind::List(items) => {
                let mut out_items = Vec::with_capacity(items.len());
                for it in items {
                    out_items.push(Self::eval_in(funcs, scope, out, it)?);
                }
                Ok(Value::List(out_items))
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
                Ok(Value::Dict(out_pairs))
            }
            ExprKind::Index(obj, idx) => {
                let target = Self::eval_in(funcs, scope, out, obj)?;
                let index = Self::eval_in(funcs, scope, out, idx)?;
                index_get(&target, &index)
            }
            ExprKind::Call(name, args) => Self::eval_call(funcs, scope, out, name, args),
            ExprKind::MethodCall(obj, method, args) => {
                Self::eval_method(funcs, scope, out, obj, method, args)
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
            BinOp::In => Ok(Value::Bool(contains(&r, &l)?)),
            BinOp::NotIn => Ok(Value::Bool(!contains(&r, &l)?)),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, &l, &r),
            BinOp::Add => add(&l, &r),
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
                arith(op, &l, &r)
            }
            BinOp::And | BinOp::Or => unreachable!("handled above"),
            BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => bitwise_or_set(op, &l, &r),
        }
    }

    fn eval_call(
        funcs: &HashMap<String, FuncDef>,
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        name: &str,
        args: &[Expr],
    ) -> Result<Value, String> {
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
                Value::List(l) | Value::Set(l) | Value::Tuple(l) => Ok(Value::Int(l.len() as i64)),
                Value::Dict(d) => Ok(Value::Int(d.len() as i64)),
                _ => Err(format!("object of type {} has no len()", type_name(v))),
            },
            ("abs", [v]) => match v {
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                _ => Err(format!("bad operand type for abs(): {}", type_name(v))),
            },
            ("str", [v]) => Ok(Value::Str(v.py_str())),
            ("int", [v]) => match v {
                Value::Int(n) => Ok(Value::Int(*n)),
                Value::Float(f) => Ok(Value::Int(*f as i64)),
                Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                Value::Str(s) => s
                    .trim()
                    .parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| format!("invalid literal for int(): '{s}'")),
                _ => Err(format!("int() argument can't be {}", type_name(v))),
            },
            ("float", [v]) => as_num(v)
                .map(Value::Float)
                .or_else(|| match v {
                    Value::Str(s) => s.trim().parse::<f64>().ok().map(Value::Float),
                    _ => None,
                })
                .ok_or_else(|| format!("can't convert {} to float", type_name(v))),
            ("bool", [v]) => Ok(Value::Bool(v.truthy())),
            ("set", []) => Ok(Value::Set(Vec::new())),
            ("set", [v]) => make_set(v),
            ("print", _) => {
                // print used as a value: write, return None.
                if let Some(sink) = out {
                    let parts: Vec<String> = vals.iter().map(Value::py_str).collect();
                    sink.push_str(&parts.join(" "));
                    sink.push('\n');
                }
                Ok(Value::None)
            }
            _ => Err(format!(
                "calling {name}() isn't in the step debugger yet — use Run for that"
            )),
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
        // Mutating list/set methods need the variable itself, so require a Name.
        if let ExprKind::Name(name) = &obj.kind {
            match scope.get_mut(name) {
                Some(Value::List(items)) => match (method, vals.as_slice()) {
                    ("append", [v]) => {
                        items.push(v.clone());
                        return Ok(Value::None);
                    }
                    ("pop", []) => {
                        return items.pop().ok_or_else(|| "pop from empty list".to_string());
                    }
                    ("pop", [i]) => {
                        let i = as_int(i).ok_or("pop index must be an integer")?;
                        let n = items.len() as i64;
                        let real = if i < 0 { i + n } else { i };
                        if real < 0 || real >= n {
                            return Err("pop index out of range".to_string());
                        }
                        return Ok(items.remove(real as usize));
                    }
                    _ => {}
                },
                Some(Value::Set(items)) => {
                    if let Some(res) = set_method_mut(items, method, &vals) {
                        return res;
                    }
                }
                _ => {}
            }
        }
        // Non-mutating set methods (union/intersection/issubset/copy/…).
        let recv = Self::eval_in(funcs, scope, out, obj)?;
        if let Some(res) = set_method_val(&recv, method, &vals) {
            return res;
        }
        Err(format!(
            ".{method}() isn't in the step debugger yet — use Run for that"
        ))
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
                let idx = Self::eval_in(funcs, scope, out, index)?;
                let val = Self::eval_in(funcs, scope, out, value)?;
                assign_index_in(scope, target, idx, val)?;
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
                    Value::List(v) => v,
                    Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                    Value::Dict(d) => d.into_iter().map(|(k, _)| k).collect(),
                    other => return Err(format!("can't loop over {}", type_name(&other))),
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
            StmtKind::Return(opt) => {
                let v = match opt {
                    Some(e) => Self::eval_in(funcs, scope, out, e)?,
                    None => Value::None,
                };
                Ok(Flow::Return(v))
            }
            StmtKind::Def { .. } => Err(
                "a nested function definition isn't in the step debugger yet — use Run".to_string(),
            ),
            StmtKind::ClassDef { .. }
            | StmtKind::SetAttr { .. }
            | StmtKind::UnpackAssign { .. }
            | StmtKind::Import(_) => Err(format!(
                "{} isn't in the step debugger yet — use Run for that",
                describe_stmt(&s.kind)
            )),
        }
    }
}

/// Assign into `target[index]` in `scope` — a list (int index, negative ok) or a
/// dict (any key). MVP: `target` must be a simple variable.
fn assign_index_in(
    scope: &mut HashMap<String, Value>,
    target: &Expr,
    index: Value,
    value: Value,
) -> Result<(), String> {
    let ExprKind::Name(name) = &target.kind else {
        return Err(
            "item assignment is only supported on a simple variable in the step debugger yet"
                .to_string(),
        );
    };
    let slot = scope
        .get_mut(name)
        .ok_or_else(|| format!("name '{name}' is not defined"))?;
    match slot {
        Value::List(items) => {
            let i = as_int(&index).ok_or("list indices must be integers")?;
            let n = items.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err("list assignment index out of range".to_string());
            }
            items[real as usize] = value;
            Ok(())
        }
        Value::Dict(pairs) => {
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| py_eq(k, &index)) {
                slot.1 = value;
            } else {
                pairs.push((index, value));
            }
            Ok(())
        }
        Value::Tuple(_) => {
            Err("a tuple is immutable — you can't change its items (use a list)".to_string())
        }
        _ => Err("only lists and dicts support item assignment".to_string()),
    }
}

/// `target[index]` read for lists (int, negative ok), strings (int -> 1-char
/// string), and dicts (any key).
fn index_get(target: &Value, index: &Value) -> Result<Value, String> {
    match target {
        Value::List(items) | Value::Tuple(items) => {
            let i = as_int(index).ok_or("list indices must be integers")?;
            let n = items.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err("list index out of range".to_string());
            }
            Ok(items[real as usize].clone())
        }
        Value::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let i = as_int(index).ok_or("string indices must be integers")?;
            let n = chars.len() as i64;
            let real = if i < 0 { i + n } else { i };
            if real < 0 || real >= n {
                return Err("string index out of range".to_string());
            }
            Ok(Value::Str(chars[real as usize].to_string()))
        }
        Value::Dict(pairs) => pairs
            .iter()
            .find(|(k, _)| py_eq(k, index))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("key {} not found", index.py_repr())),
        _ => Err(format!("{} is not subscriptable", type_name(target))),
    }
}

/// Membership test `needle in haystack` for strings (substring), lists, and dict
/// keys.
fn contains(haystack: &Value, needle: &Value) -> Result<bool, String> {
    match haystack {
        Value::List(items) | Value::Set(items) | Value::Tuple(items) => {
            Ok(items.iter().any(|v| py_eq(v, needle)))
        }
        Value::Dict(pairs) => Ok(pairs.iter().any(|(k, _)| py_eq(k, needle))),
        Value::Str(s) => match needle {
            Value::Str(sub) => Ok(s.contains(sub.as_str())),
            _ => Err("'in <string>' requires a string".to_string()),
        },
        _ => Err(format!(
            "argument of type {} is not iterable",
            type_name(haystack)
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
                _ => {
                    return Err(format!(
                        "'{}' not supported between {} and {}",
                        op_symbol(op),
                        type_name(l),
                        type_name(r)
                    ));
                }
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
            let mut v = x.clone();
            v.extend(y.clone());
            Ok(Value::List(v))
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
    let (x, y) = match (as_num(l), as_num(r)) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            return Err(format!(
                "unsupported operand types for {}: {} and {}",
                op_symbol(op),
                type_name(l),
                type_name(r)
            ));
        }
    };
    let ints = both_int(l, r);
    match op {
        BinOp::Add => Ok(num_result(x + y, ints)),
        BinOp::Sub => Ok(num_result(x - y, ints)),
        BinOp::Mul => Ok(num_result(x * y, ints)),
        BinOp::Div => {
            if y == 0.0 {
                return Err("division by zero".to_string());
            }
            Ok(Value::Float(x / y)) // true division is always float
        }
        BinOp::FloorDiv => {
            if y == 0.0 {
                return Err("integer division or modulo by zero".to_string());
            }
            Ok(num_result((x / y).floor(), ints))
        }
        BinOp::Mod => {
            if y == 0.0 {
                return Err("integer division or modulo by zero".to_string());
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

fn type_name(v: &Value) -> &'static str {
    match v {
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

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::FloorDiv => "//",
        BinOp::Mod => "%",
        BinOp::Pow => "**",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        _ => "?",
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
        ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => "comprehensions",
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
    /// Pop a value and bind it to a name in the current scope.
    Store(String),
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
    /// Pop value then index; assign `name[index] = value`.
    StoreIndex(String),
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

/// One call frame: its own continuation + operand stacks and local scope.
struct VmFrame {
    /// Display name for the call stack ("<module>" for the top level).
    func: String,
    work: Vec<Task>,
    operands: Vec<Value>,
    scope: HashMap<String, Value>,
    line: usize,
}

/// The CPS virtual machine. Same public surface as [`Stepper`], plus
/// [`Vm::call_stack`].
pub struct Vm {
    frames: Vec<VmFrame>,
    funcs: HashMap<String, VmFunc>,
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
}

impl Vm {
    pub fn new(source: &str) -> Result<Vm, CompileError> {
        let tokens = crate::lexer::lex(source)?;
        let program = crate::parser::parse(&tokens)?;
        let module = VmFrame {
            func: "<module>".to_string(),
            work: vec![Task::Next(Rc::new(program), 0)],
            operands: Vec::new(),
            scope: HashMap::new(),
            line: 0,
        };
        let mut vm = Vm {
            frames: vec![module],
            funcs: HashMap::new(),
            output: String::new(),
            status: Status::Finished,
            watchpoints: Vec::new(),
            watch_hit: None,
            last_return: None,
            stdin: std::collections::VecDeque::new(),
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
                let ExprKind::Name(name) = &target.kind else {
                    return Err(
                        "item assignment needs a simple variable (e.g. xs[i] = ..) yet".to_string(),
                    );
                };
                self.push_task(Task::StoreIndex(name.clone()));
                self.push_task(Task::Eval(Rc::new(value.clone())));
                self.push_task(Task::Eval(Rc::new(index.clone())));
            }
            StmtKind::ClassDef { .. }
            | StmtKind::SetAttr { .. }
            | StmtKind::UnpackAssign { .. }
            | StmtKind::Import(_) => {
                return Err(format!(
                    "{} isn't in the step debugger yet — use Run for that",
                    describe_stmt(&s.kind)
                ));
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
                self.push_op(apply_bin(op, &l, &r)?);
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
            Task::Store(name) => {
                let v = self.pop_op()?;
                self.top().scope.insert(name, v);
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
                self.push_op(Value::List(items));
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
                self.push_op(Value::Dict(pairs));
            }
            Task::IndexGet => {
                let idx = self.pop_op()?;
                let obj = self.pop_op()?;
                self.push_op(index_get(&obj, &idx)?);
            }
            Task::MethodOnName(name, method, argc) => {
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(self.pop_op()?);
                }
                args.reverse();
                let result = list_method(self.top(), &name, &method, args)?;
                self.push_op(result);
            }
            Task::ForEachSetup(var, body) => {
                let items = match self.pop_op()? {
                    Value::List(v) | Value::Set(v) | Value::Tuple(v) => v,
                    Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                    Value::Dict(d) => d.into_iter().map(|(k, _)| k).collect(),
                    other => return Err(format!("can't loop over {}", type_name(&other))),
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
            Task::StoreIndex(name) => {
                let value = self.pop_op()?;
                let index = self.pop_op()?;
                let target = Expr {
                    kind: ExprKind::Name(name),
                    line: 0,
                    span: (0, 0),
                };
                assign_index_in(&mut self.top().scope, &target, index, value)?;
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
            ExprKind::Bin(op, a, b) => {
                self.push_task(Task::Bin(*op));
                self.push_task(Task::Eval(Rc::new((**b).clone())));
                self.push_task(Task::Eval(Rc::new((**a).clone())));
            }
            ExprKind::Call(name, args) => {
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
            ExprKind::MethodCall(obj, method, args) => {
                let ExprKind::Name(recv) = &obj.kind else {
                    return Err(
                        "method calls need a simple variable (e.g. xs.append(..)) in call-stack mode yet"
                            .to_string(),
                    );
                };
                self.push_task(Task::MethodOnName(recv.clone(), method.clone(), args.len()));
                for a in args.iter().rev() {
                    self.push_task(Task::Eval(Rc::new(a.clone())));
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
        } else {
            let v = call_builtin(name, &args)?;
            self.push_op(v);
            Ok(())
        }
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
                Ok(Value::List(out))
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
                Ok(Value::Dict(out))
            }
            _ => Err("that watch isn't supported in call-stack mode yet".to_string()),
        }
    }
}

/// Set members must be immutable (hashable). A list/dict/set can change, so it
/// can't be a member — a tuple can. Mirrors the compiled backends.
fn check_set_elem(v: &Value) -> Result<(), String> {
    match v {
        Value::List(_) | Value::Dict(_) | Value::Set(_) => Err(format!(
            "a set can't contain a {} — use a tuple",
            type_name(v)
        )),
        _ => Ok(()),
    }
}

/// Build a set from any iterable value, de-duplicating by Python equality and
/// keeping first-seen (insertion) order — matching the compiled backends.
fn make_set(v: &Value) -> Result<Value, String> {
    let items: Vec<Value> = match v {
        Value::List(x) | Value::Set(x) | Value::Tuple(x) => x.clone(),
        Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
        Value::Dict(d) => d.iter().map(|(k, _)| k.clone()).collect(),
        _ => return Err(format!("{} object is not iterable", type_name(v))),
    };
    let mut out: Vec<Value> = Vec::new();
    for it in items {
        check_set_elem(&it)?;
        if !out.iter().any(|y| py_eq(y, &it)) {
            out.push(it);
        }
    }
    Ok(Value::Set(out))
}

/// A set-theory operation on two sets: `|` union, `&` intersection, `^`
/// symmetric difference, `-` difference. Insertion-order preserving.
fn set_binop(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    let (Value::Set(a), Value::Set(b)) = (l, r) else {
        return Err(format!(
            "unsupported operand type for a set operation: {} and {}",
            type_name(l),
            type_name(r)
        ));
    };
    let in_set = |set: &[Value], v: &Value| set.iter().any(|y| py_eq(y, v));
    let res: Vec<Value> = match op {
        BinOp::BitOr => {
            let mut v = a.clone();
            for x in b {
                if !in_set(&v, x) {
                    v.push(x.clone());
                }
            }
            v
        }
        BinOp::BitAnd => a.iter().filter(|x| in_set(b, x)).cloned().collect(),
        BinOp::Sub => a.iter().filter(|x| !in_set(b, x)).cloned().collect(),
        BinOp::BitXor => a
            .iter()
            .filter(|x| !in_set(b, x))
            .chain(b.iter().filter(|x| !in_set(a, x)))
            .cloned()
            .collect(),
        _ => unreachable!("non-set-op passed to set_binop"),
    };
    Ok(Value::Set(res))
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
        _ => Err(format!(
            "unsupported operand type for {}: {} and {}",
            op_symbol(op),
            type_name(l),
            type_name(r)
        )),
    }
}

/// Whether every element of `a` is in `b` (both must be sets).
fn set_subset(a: &Value, b: &Value) -> Result<bool, String> {
    match (a, b) {
        (Value::Set(x), Value::Set(y)) => Ok(x.iter().all(|e| y.iter().any(|f| py_eq(f, e)))),
        _ => Err("issubset/issuperset need a set argument".to_string()),
    }
}

/// Mutating set methods on the set's elements (the receiver is a set variable).
/// Returns `None` if `method` isn't a mutating set method.
fn set_method_mut(
    items: &mut Vec<Value>,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
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
        ("copy", []) => Ok(recv.clone()),
        _ => return None,
    };
    Some(r)
}

/// Apply a binary operator to two evaluated values (shared by the VM).
fn apply_bin(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    match op {
        BinOp::Eq => Ok(Value::Bool(py_eq(l, r))),
        BinOp::Ne => Ok(Value::Bool(!py_eq(l, r))),
        BinOp::In => Ok(Value::Bool(contains(r, l)?)),
        BinOp::NotIn => Ok(Value::Bool(!contains(r, l)?)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, l, r),
        BinOp::Add => add(l, r),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
            arith(op, l, r)
        }
        BinOp::And | BinOp::Or => unreachable!("short-circuited before apply"),
        BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => bitwise_or_set(op, l, r),
    }
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, String> {
    match op {
        UnOp::Not => Ok(Value::Bool(!v.truthy())),
        UnOp::Neg => match v {
            Value::Int(n) => Ok(Value::Int(-n)),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Bool(b) => Ok(Value::Int(if b { -1 } else { 0 })),
            _ => Err(format!("can't negate {}", type_name(&v))),
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
            Value::List(l) | Value::Set(l) | Value::Tuple(l) => Ok(Value::Int(l.len() as i64)),
            Value::Dict(d) => Ok(Value::Int(d.len() as i64)),
            _ => Err(format!("object of type {} has no len()", type_name(v))),
        },
        ("abs", [v]) => match v {
            Value::Int(n) => Ok(Value::Int(n.abs())),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            _ => Err(format!("bad operand type for abs(): {}", type_name(v))),
        },
        ("str", [v]) => Ok(Value::Str(v.py_str())),
        ("int", [v]) => match v {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(*f as i64)),
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            Value::Str(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| format!("invalid literal for int(): '{s}'")),
            _ => Err(format!("int() argument can't be {}", type_name(v))),
        },
        ("float", [v]) => as_num(v)
            .map(Value::Float)
            .or_else(|| match v {
                Value::Str(s) => s.trim().parse::<f64>().ok().map(Value::Float),
                _ => None,
            })
            .ok_or_else(|| format!("can't convert {} to float", type_name(v))),
        ("bool", [v]) => Ok(Value::Bool(v.truthy())),
        ("set", []) => Ok(Value::Set(Vec::new())),
        ("set", [v]) => make_set(v),
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
        _ => Err(format!(
            "calling {name}() isn't in the step debugger's call-stack mode yet — use Run"
        )),
    }
}

/// `name.method(args)` on a list variable in `frame`'s scope (append / pop).
fn list_method(
    frame: &mut VmFrame,
    name: &str,
    method: &str,
    args: Vec<Value>,
) -> Result<Value, String> {
    let unsupported =
        || format!(".{method}() isn't in the step debugger's call-stack mode yet — use Run");
    match frame.scope.get_mut(name) {
        Some(Value::List(items)) => match (method, args.as_slice()) {
            ("append", [v]) => {
                items.push(v.clone());
                Ok(Value::None)
            }
            ("pop", []) => items.pop().ok_or_else(|| "pop from empty list".to_string()),
            ("pop", [i]) => {
                let i = as_int(i).ok_or("pop index must be an integer")?;
                let n = items.len() as i64;
                let real = if i < 0 { i + n } else { i };
                if real < 0 || real >= n {
                    return Err("pop index out of range".to_string());
                }
                Ok(items.remove(real as usize))
            }
            _ => Err(unsupported()),
        },
        Some(Value::Set(items)) => {
            if let Some(res) = set_method_mut(items, method, &args) {
                return res;
            }
            // Non-mutating methods need the value; clone the set and dispatch.
            let recv = Value::Set(items.clone());
            set_method_val(&recv, method, &args).unwrap_or_else(|| Err(unsupported()))
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
        // `import` isn't in the stepper yet — friendly stop, not a panic.
        let mut s = Stepper::new("import math\nprint(1)\n").unwrap();
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
        // Item assignment to a tuple is an error, not a silent mutation.
        let err = vm_err("t = (1, 2)\nt[0] = 9\n").expect("should error");
        assert!(err.contains("immutable"), "{err}");
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
}
