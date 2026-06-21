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

/// Python equality across the numeric tower (int == float, bool == int), plus
/// structural equality for strings/lists/dicts.
fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
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
        matches!(self, Cont::While { .. } | Cont::ForRange { .. } | Cont::ForEach { .. })
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
    Error { line: Option<usize>, message: String },
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
        Self::eval_in(&mut scratch, &mut None, expr)
            .ok()
            .map(|v| v.py_repr())
    }

    /// Re-evaluate every watchpoint; if any changed since last time, record the
    /// first as `watch_hit`, resync all of them, and return true.
    fn check_watchpoints(&mut self) -> bool {
        if self.watchpoints.is_empty() {
            return false;
        }
        let curs: Vec<Option<String>> =
            self.watchpoints.iter().map(|w| self.eval_repr(&w.expr)).collect();
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
        Self::eval_in(&mut scratch, &mut None, &expr).map(|v| v.py_repr())
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
                    let truthy = Self::eval_in(&mut self.scope, &mut Some(&mut self.output), &cond)
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
            StmtKind::Assign(name, e) => {
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
            StmtKind::Def { .. }
            | StmtKind::Return(_)
            | StmtKind::ClassDef { .. }
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

    /// Assign into `target[index]` — supports a list (int index, negative ok) or
    /// a dict (any key). MVP: `target` must be a simple variable.
    fn assign_index(&mut self, target: &Expr, index: Value, value: Value) -> Result<(), String> {
        let ExprKind::Name(name) = &target.kind else {
            return Err(
                "item assignment is only supported on a simple variable in the step debugger yet"
                    .to_string(),
            );
        };
        let slot = self
            .scope
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
            _ => Err("only lists and dicts support item assignment".to_string()),
        }
    }

    fn eval_int(&mut self, e: &Expr, what: &str) -> Result<i64, String> {
        let v = self.eval(e)?;
        as_int(&v).ok_or_else(|| format!("{what} must be an integer"))
    }

    fn iterable_items(&mut self, e: &Expr) -> Result<Vec<Value>, String> {
        match self.eval(e)? {
            Value::List(v) => Ok(v),
            Value::Str(s) => Ok(s.chars().map(|c| Value::Str(c.to_string())).collect()),
            Value::Dict(d) => Ok(d.into_iter().map(|(k, _)| k).collect()),
            other => Err(format!("can't loop over {}", type_name(&other))),
        }
    }

    fn eval(&mut self, e: &Expr) -> Result<Value, String> {
        Self::eval_in(&mut self.scope, &mut Some(&mut self.output), e)
    }

    /// The expression evaluator. Takes the scope (and optional output sink, for
    /// the rare expression-with-output) explicitly so watches can run it against
    /// a scratch scope with no output.
    fn eval_in(
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
                let v = Self::eval_in(scope, out, inner)?;
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
            ExprKind::Bin(op, a, b) => Self::eval_bin(scope, out, *op, a, b),
            ExprKind::List(items) => {
                let mut out_items = Vec::with_capacity(items.len());
                for it in items {
                    out_items.push(Self::eval_in(scope, out, it)?);
                }
                Ok(Value::List(out_items))
            }
            ExprKind::Dict(pairs) => {
                let mut out_pairs = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    out_pairs.push((Self::eval_in(scope, out, k)?, Self::eval_in(scope, out, v)?));
                }
                Ok(Value::Dict(out_pairs))
            }
            ExprKind::Index(obj, idx) => {
                let target = Self::eval_in(scope, out, obj)?;
                let index = Self::eval_in(scope, out, idx)?;
                index_get(&target, &index)
            }
            ExprKind::Call(name, args) => Self::eval_call(scope, out, name, args),
            ExprKind::MethodCall(obj, method, args) => {
                Self::eval_method(scope, out, obj, method, args)
            }
            _ => Err(format!(
                "{} isn't in the step debugger yet — use Run for that",
                describe_expr(&e.kind)
            )),
        }
    }

    fn eval_bin(
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        op: BinOp,
        a: &Expr,
        b: &Expr,
    ) -> Result<Value, String> {
        // `and`/`or` short-circuit and return the deciding operand (Python).
        if op == BinOp::And {
            let l = Self::eval_in(scope, out, a)?;
            return if l.truthy() {
                Self::eval_in(scope, out, b)
            } else {
                Ok(l)
            };
        }
        if op == BinOp::Or {
            let l = Self::eval_in(scope, out, a)?;
            return if l.truthy() {
                Ok(l)
            } else {
                Self::eval_in(scope, out, b)
            };
        }

        let l = Self::eval_in(scope, out, a)?;
        let r = Self::eval_in(scope, out, b)?;
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
            BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => {
                Err("set operators aren't in the step debugger yet — use Run".to_string())
            }
        }
    }

    fn eval_call(
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        name: &str,
        args: &[Expr],
    ) -> Result<Value, String> {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(Self::eval_in(scope, out, a)?);
        }
        match (name, vals.as_slice()) {
            ("len", [v]) => match v {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::List(l) => Ok(Value::Int(l.len() as i64)),
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
        scope: &mut HashMap<String, Value>,
        out: &mut Option<&mut String>,
        obj: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Value, String> {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(Self::eval_in(scope, out, a)?);
        }
        // Mutating list methods need the variable itself, so require a Name.
        if let ExprKind::Name(name) = &obj.kind
            && let Some(Value::List(items)) = scope.get_mut(name)
        {
            match (method, vals.as_slice()) {
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
            }
        }
        Err(format!(
            ".{method}() isn't in the step debugger yet — use Run for that"
        ))
    }
}

/// `target[index]` read for lists (int, negative ok), strings (int -> 1-char
/// string), and dicts (any key).
fn index_get(target: &Value, index: &Value) -> Result<Value, String> {
    match target {
        Value::List(items) => {
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
        Value::List(items) => Ok(items.iter().any(|v| py_eq(v, needle))),
        Value::Dict(pairs) => Ok(pairs.iter().any(|(k, _)| py_eq(k, needle))),
        Value::Str(s) => match needle {
            Value::Str(sub) => Ok(s.contains(sub.as_str())),
            _ => Err("'in <string>' requires a string".to_string()),
        },
        _ => Err(format!("argument of type {} is not iterable", type_name(haystack))),
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
        assert_eq!(run_to_end("xs = [1, 2, 3]\nxs[0] = 9\nprint(xs[0])\n"), "9\n");
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
        assert_eq!(vars, vec![("x".into(), "10".into()), ("y".into(), "20".into())]);
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
        // A user function call isn't in the stepper yet — friendly stop, not a panic.
        let mut s = Stepper::new("def f():\n    return 1\nf()\n").unwrap();
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
    fn nested_loops_produce_correct_output() {
        let src = "for i in range(2):\n    for j in range(2):\n        print(i * 10 + j)\n";
        assert_eq!(run_to_end(src), "0\n1\n10\n11\n");
    }
}
