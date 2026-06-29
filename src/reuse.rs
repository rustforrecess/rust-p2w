//! Perceus reuse tier — analysis scaffold (the compiler hire fills this in).
//!
//! The native backend's RC pass (`llvm.rs`) is correct but **naive**: it releases
//! heap values at scope end, not at last use, and has only one hand-written reuse
//! case (`try_inplace_map`). The precise drops + general drop-reuse that make it
//! true Perceus need a **last-use (backward liveness) analysis**. This module is
//! where that lives; see `docs/REUSE_PLAN.md` for the staging + acceptance
//! contract (every change keeps `tools/native_run.sh` green: matches CPython AND
//! `live == 0`).
//!
//! Shipped here as PREP:
//! - [`vars_read`] / [`vars_assigned`] — correct, tested AST primitives (the raw
//!   "which names appear" substrate any dataflow needs).
//! - [`Liveness`] — a CONSERVATIVE stub (nothing dies before scope end, i.e.
//!   today's emitter behavior). Replacing [`Liveness::analyze`]'s body with real
//!   backward liveness is the keystone task; the emitter consumes [`Liveness::dead_after`]
//!   and the `live == 0` oracle is the safety net.
//!
//! Native-only: the browser backend (WASM-GC) needs none of this.
#![allow(dead_code)]

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use std::collections::BTreeSet;

/// Collect the variable names *read* by an expression (every `Name` occurrence,
/// recursively). Syntactic, not scope-aware: a comprehension's bound variables
/// and function parameters appear here like any other name — resolving those
/// scopes is the liveness analysis's job, built on top of this. A call's callee
/// (`f` in `f(x)`) is a function name, not a variable read, so it is *not*
/// collected; its arguments are.
pub fn vars_read(e: &Expr, out: &mut BTreeSet<String>) {
    match &e.kind {
        ExprKind::Name(n) => {
            out.insert(n.clone());
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_) => {}
        ExprKind::Unary(_, inner) => vars_read(inner, out),
        ExprKind::Bin(_, a, b) => {
            vars_read(a, out);
            vars_read(b, out);
        }
        // The callee is a function name (separate namespace), not a var read.
        ExprKind::Call(_, args) => args.iter().for_each(|a| vars_read(a, out)),
        ExprKind::Kwarg(_, v) => vars_read(v, out),
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            items.iter().for_each(|i| vars_read(i, out))
        }
        ExprKind::Dict(pairs) => pairs.iter().for_each(|(k, v)| {
            vars_read(k, out);
            vars_read(v, out);
        }),
        ExprKind::Index(obj, idx) => {
            vars_read(obj, out);
            vars_read(idx, out);
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            vars_read(obj, out);
            for part in [start, stop, step].into_iter().flatten() {
                vars_read(part, out);
            }
        }
        // The method name is not a variable; the receiver and args are read.
        ExprKind::MethodCall(obj, _, args) => {
            vars_read(obj, out);
            args.iter().for_each(|a| vars_read(a, out));
        }
        ExprKind::Attr(obj, _) => vars_read(obj, out),
        ExprKind::ListComp { element, clauses } => {
            vars_read(element, out);
            clauses.iter().for_each(|c| comp_clause_reads(c, out));
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            vars_read(key, out);
            vars_read(value, out);
            clauses.iter().for_each(|c| comp_clause_reads(c, out));
        }
    }
}

fn comp_clause_reads(c: &crate::ast::CompClause, out: &mut BTreeSet<String>) {
    match c {
        // The `for v in it` binds `v` (local to the comp) and reads `it`. We
        // collect both here syntactically; scope-correct handling (subtracting
        // the bound vars) belongs to the liveness analysis.
        crate::ast::CompClause::For { vars, iter } => {
            vars_read(iter, out);
            out.extend(vars.iter().cloned());
        }
        crate::ast::CompClause::If(cond) => vars_read(cond, out),
    }
}

/// Collect the variable names a single statement *binds* (assignment/annotated
/// assignment targets, the loop variable of `for`/for-each, tuple-unpack targets).
/// Names introduced by nested bodies (inside `if`/`for`/`while`/`def`) are NOT
/// descended into — this is the names bound *directly* by `s`. (`def`/`class`
/// bind a function/class name, a separate namespace from variables, so they are
/// not collected here.)
pub fn vars_assigned(s: &Stmt, out: &mut BTreeSet<String>) {
    match &s.kind {
        StmtKind::Assign(name, _) | StmtKind::AnnAssign { name, .. } => {
            out.insert(name.clone());
        }
        StmtKind::For { var, .. } | StmtKind::ForEach { var, .. } => {
            out.insert(var.clone());
        }
        StmtKind::UnpackAssign { targets, .. } => {
            for t in targets {
                if let ExprKind::Name(n) = &t.kind {
                    out.insert(n.clone());
                }
            }
        }
        _ => {}
    }
}

/// Last-use / liveness information for one function body (or the module top
/// level). **Conservative stub**: reports that nothing dies before scope end,
/// which reproduces the emitter's current naive scope-end release behavior
/// exactly. The compiler hire replaces [`analyze`](Liveness::analyze) with real
/// backward liveness; the emitter (which queries [`dead_after`](Liveness::dead_after))
/// and the `live == 0` oracle stay put. See `docs/REUSE_PLAN.md`.
#[derive(Debug, Default)]
pub struct Liveness {
    /// Per-statement-index: variables whose last use is that statement (so the
    /// emitter may release them right after it). Empty everywhere in the stub.
    dead_after: Vec<Vec<String>>,
}

impl Liveness {
    /// Analyze a body. STUB: conservative — no early deaths (everything lives to
    /// scope end). Replace the body with backward liveness; keep the contract.
    pub fn analyze(body: &[Stmt]) -> Liveness {
        Liveness {
            dead_after: vec![Vec::new(); body.len()],
        }
    }

    /// Bindings whose last use is statement `idx` — safe to release immediately
    /// after emitting it (and to drop from the scope-exit release set). Empty in
    /// the conservative stub, so the emitter behaves exactly as today.
    pub fn dead_after(&self, idx: usize) -> &[String] {
        self.dead_after.get(idx).map_or(&[], Vec::as_slice)
    }

    /// True if any binding is reported dead anywhere — i.e. the analysis is doing
    /// real work (false for the stub). Lets the emitter assert it's still in the
    /// safe naive mode until the analysis lands.
    pub fn is_conservative(&self) -> bool {
        self.dead_after.iter().all(Vec::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn parse(src: &str) -> Vec<Stmt> {
        parser::parse(&lexer::lex(src).unwrap()).unwrap()
    }

    fn reads(src_expr: &str) -> BTreeSet<String> {
        // Wrap as `_ = <expr>` so we can pull the expression back out.
        let stmts = parse(&format!("__probe__ = {src_expr}\n"));
        let mut out = BTreeSet::new();
        if let StmtKind::Assign(_, e) = &stmts[0].kind {
            vars_read(e, &mut out);
        }
        out
    }

    #[test]
    fn vars_read_collects_names_not_callees_or_literals() {
        assert_eq!(reads("a + b * 2"), set(["a", "b"]));
        assert_eq!(reads("3"), set([]));
        // Callee name is not a read; arguments are.
        assert_eq!(reads("f(x, y)"), set(["x", "y"]));
        // Method receiver + args, not the method name.
        assert_eq!(reads("xs.append(v)"), set(["xs", "v"]));
        assert_eq!(reads("obj.attr"), set(["obj"]));
        assert_eq!(reads("d[k]"), set(["d", "k"]));
        assert_eq!(reads("xs[a:b:c]"), set(["xs", "a", "b", "c"]));
        assert_eq!(reads("[p, q]"), set(["p", "q"]));
        assert_eq!(reads("{kk: vv}"), set(["kk", "vv"]));
    }

    #[test]
    fn vars_read_descends_comprehensions() {
        // `it` and the filtered/element vars all appear (scope-correct handling
        // is the analysis's job; this is the syntactic substrate).
        assert_eq!(reads("[x * y for x in it if x > 0]"), set(["x", "y", "it"]));
    }

    #[test]
    fn vars_assigned_covers_the_binding_forms() {
        assert_eq!(assigned("x = 1\n"), set(["x"]));
        assert_eq!(assigned("y: int = 2\n"), set(["y"]));
        assert_eq!(assigned("for i in range(3):\n    pass\n"), set(["i"]));
        assert_eq!(assigned("for w in ws:\n    pass\n"), set(["w"]));
        assert_eq!(assigned("a, b = pair\n"), set(["a", "b"]));
        // print(...) binds nothing.
        assert_eq!(assigned("print(x)\n"), set([]));
    }

    #[test]
    fn liveness_stub_is_conservative() {
        let body = parse("x = [1, 2]\nprint(x)\ny = x\n");
        let live = Liveness::analyze(&body);
        assert!(live.is_conservative(), "stub must report no early deaths");
        for i in 0..body.len() {
            assert!(live.dead_after(i).is_empty());
        }
        assert!(live.dead_after(999).is_empty(), "out-of-range is empty");
    }

    fn assigned(src: &str) -> BTreeSet<String> {
        let stmts = parse(src);
        let mut out = BTreeSet::new();
        vars_assigned(&stmts[0], &mut out);
        out
    }

    fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
}
