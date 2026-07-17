//! Lift nested `def`s to module level, converting closures by lambda lifting.
//!
//! Functions aren't first-class values here (`return inner` / `g = f` / `f(g)`
//! are all errors) and there's no `nonlocal`, so a nested function can never
//! escape its enclosing call and can never *rebind* an enclosing local — it can
//! only read one. That makes **lambda lifting** exactly equivalent to real
//! closures for this subset: a captured variable becomes an extra parameter,
//! and each call site passes its current value. Python's closures capture the
//! variable (cell), not the value, but passing the value *at the call* observes
//! the same thing — `x = 1; def s(): return x; x = 2; s()` still sees 2 — and a
//! captured container is passed by reference, so mutation through it still
//! shows. No closure objects, no environment allocation, no GC/RC involvement:
//! it's a pure AST transform, so both backends and the debugger get it free.
//!
//! Captured params are **prepended** (defaults align to the trailing params, so
//! appending would break `def f(a, b=1)`). Function names must be unique across
//! the program (no mangling), so a call resolves to exactly one function.

use crate::ast::{CompClause, Expr, ExprKind, Stmt, StmtKind};
use crate::error::CompileError;
use crate::lint::{BlockScope, each_child_expr, for_each_child_block, stmt_exprs};
use crate::reuse::vars_read;
use std::collections::{BTreeMap, BTreeSet};

/// Names ASSIGNED at this function level (params are added by the caller):
/// `Assign` / `AnnAssign` / `for` var / unpack targets, recursing into
/// `if`/`for`/`while` but NOT into nested `def`/`class` bodies (their own scope).
fn assigned_here(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(n, _) | StmtKind::AnnAssign { name: n, .. } => {
                out.insert(n.clone());
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
        for_each_child_block(s, |body, scope| {
            if scope == BlockScope::Same {
                assigned_here(body, out);
            }
        });
    }
}

/// Names READ at this function level (recursing into `if`/`for`/`while` but not
/// nested `def`/`class` bodies). `vars_read` skips call *callees* and literals,
/// so plain function calls aren't mistaken for captured variables.
fn reads_here(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        stmt_exprs(s, &mut |e| vars_read(e, out));
        for_each_child_block(s, |body, scope| {
            if scope == BlockScope::Same {
                reads_here(body, out);
            }
        });
    }
}

/// A function's local names: params plus everything it assigns (this scope).
fn function_locals(params: &[String], body: &[Stmt]) -> BTreeSet<String> {
    let mut set: BTreeSet<String> = params.iter().cloned().collect();
    assigned_here(body, &mut set);
    set
}

/// The `def`s directly in this body (the only ones we lift; a `def` tucked
/// inside an `if`/`for` stays put and is rejected downstream).
fn nested_defs(body: &[Stmt]) -> Vec<&Stmt> {
    body.iter()
        .filter(|s| matches!(s.kind, StmtKind::Def { .. }))
        .collect()
}

/// Names this function's whole subtree needs from OUTSIDE it: what it reads at
/// its own level, plus what its nested functions need, minus its own locals.
/// (So a doubly-nested function's capture is threaded through the middle one.)
fn free_vars_of(params: &[String], body: &[Stmt]) -> BTreeSet<String> {
    let locals = function_locals(params, body);
    let mut free = BTreeSet::new();
    reads_here(body, &mut free); // includes nested defs' *default* exprs
    for g in nested_defs(body) {
        if let StmtKind::Def {
            params: gp, body: gb, ..
        } = &g.kind
        {
            free.extend(free_vars_of(gp, gb));
        }
    }
    free.retain(|v| !locals.contains(v));
    free
}

/// Every function name called anywhere in these statements (descending into
/// nested defs too, so a sibling call from a deeper level still counts).
fn collect_calls(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    fn in_expr(e: &Expr, out: &mut BTreeSet<String>) {
        if let ExprKind::Call(n, _) = &e.kind {
            out.insert(n.clone());
        }
        each_child_expr(e, &mut |c| in_expr(c, out));
    }
    for s in stmts {
        stmt_exprs(s, &mut |e| in_expr(e, out));
        for_each_child_block(s, |body, _| collect_calls(body, out));
    }
}

/// Prepend each lifted function's captured arguments at its call sites.
fn rewrite_calls(stmts: &mut [Stmt], caps: &BTreeMap<String, Vec<String>>) {
    for s in stmts.iter_mut() {
        stmt_exprs_mut(s, &mut |e| rewrite_call_expr(e, caps));
        child_blocks_mut(s, &mut |body| rewrite_calls(body, caps));
    }
}

fn rewrite_call_expr(e: &mut Expr, caps: &BTreeMap<String, Vec<String>>) {
    let line = e.line;
    match &mut e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_)
        | ExprKind::Name(_) => {}
        ExprKind::Unary(_, x) | ExprKind::Kwarg(_, x) | ExprKind::Attr(x, _) => {
            rewrite_call_expr(x, caps)
        }
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            rewrite_call_expr(a, caps);
            rewrite_call_expr(b, caps);
        }
        ExprKind::List(v) | ExprKind::Tuple(v) => {
            for x in v {
                rewrite_call_expr(x, caps);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                rewrite_call_expr(k, caps);
                rewrite_call_expr(v, caps);
            }
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            rewrite_call_expr(obj, caps);
            for o in [start, stop, step] {
                if let Some(x) = o {
                    rewrite_call_expr(x, caps);
                }
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            rewrite_call_expr(recv, caps);
            for a in args {
                rewrite_call_expr(a, caps);
            }
        }
        ExprKind::ListComp { element, clauses } | ExprKind::SetComp { element, clauses } => {
            rewrite_call_expr(element, caps);
            rewrite_clauses(clauses, caps);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            rewrite_call_expr(key, caps);
            rewrite_call_expr(value, caps);
            rewrite_clauses(clauses, caps);
        }
        ExprKind::IfExp { cond, then, orelse } => {
            rewrite_call_expr(cond, caps);
            rewrite_call_expr(then, caps);
            rewrite_call_expr(orelse, caps);
        }
        ExprKind::Call(name, args) => {
            for a in args.iter_mut() {
                rewrite_call_expr(a, caps);
            }
            if let Some(cs) = caps.get(name.as_str())
                && !cs.is_empty()
            {
                let mut new_args: Vec<Expr> = cs
                    .iter()
                    .map(|c| Expr {
                        kind: ExprKind::Name(c.clone()),
                        line,
                        span: (0, 0),
                    })
                    .collect();
                new_args.append(args);
                *args = new_args;
            }
        }
    }
}

fn rewrite_clauses(clauses: &mut [CompClause], caps: &BTreeMap<String, Vec<String>>) {
    for c in clauses {
        match c {
            CompClause::For { iter, .. } => rewrite_call_expr(iter, caps),
            CompClause::If(e) => rewrite_call_expr(e, caps),
        }
    }
}

/// Mutable twin of `lint::stmt_exprs` (expressions a statement evaluates in
/// place, not descending into child blocks).
fn stmt_exprs_mut(s: &mut Stmt, f: &mut impl FnMut(&mut Expr)) {
    match &mut s.kind {
        StmtKind::Expr(e)
        | StmtKind::Assign(_, e)
        | StmtKind::AnnAssign { value: e, .. }
        | StmtKind::Return(Some(e)) => f(e),
        StmtKind::If { cond, elifs, .. } => {
            f(cond);
            for (c, _) in elifs {
                f(c);
            }
        }
        StmtKind::For {
            start, end, step, ..
        } => {
            f(start);
            f(end);
            f(step);
        }
        StmtKind::ForEach { iterable, .. } => f(iterable),
        StmtKind::While { cond, .. } => f(cond),
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            f(target);
            f(index);
            f(value);
        }
        StmtKind::SetAttr { obj, value, .. } => {
            f(obj);
            f(value);
        }
        StmtKind::UnpackAssign { targets, value } => {
            for t in targets {
                f(t);
            }
            f(value);
        }
        StmtKind::Def { defaults, .. } => {
            for d in defaults {
                f(d);
            }
        }
        StmtKind::ClassDef { class_vars, .. } => {
            for (_, e) in class_vars {
                f(e);
            }
        }
        StmtKind::Return(None)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_) => {}
    }
}

/// Mutable twin of `lint::for_each_child_block` (every nested statement block).
fn child_blocks_mut(s: &mut Stmt, f: &mut impl FnMut(&mut Vec<Stmt>)) {
    match &mut s.kind {
        StmtKind::If {
            body,
            elifs,
            else_body,
            ..
        } => {
            f(body);
            for (_, b) in elifs {
                f(b);
            }
            if let Some(b) = else_body {
                f(b);
            }
        }
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::While { body, .. }
        | StmtKind::Def { body, .. } => f(body),
        StmtKind::ClassDef { methods, .. } => {
            for m in methods {
                f(&mut m.body);
            }
        }
        _ => {}
    }
}

/// Lift the nested functions out of one function, converting captures into
/// leading parameters. Lifted functions (deepest first) are pushed to `out`.
fn lift_in_function(
    def_stmt: &mut Stmt,
    names: &mut BTreeSet<String>,
    out: &mut Vec<Stmt>,
) -> Result<(), CompileError> {
    let StmtKind::Def { params, body, .. } = &mut def_stmt.kind else {
        return Ok(());
    };

    // Split this body into the statements we keep and the defs we lift.
    let mut nested: Vec<Stmt> = Vec::new();
    let mut kept: Vec<Stmt> = Vec::new();
    for s in std::mem::take(body) {
        if matches!(s.kind, StmtKind::Def { .. }) {
            nested.push(s);
        } else {
            kept.push(s);
        }
    }
    if nested.is_empty() {
        *body = kept;
        return Ok(());
    }
    let outer_locals = function_locals(params, &kept);
    let def_name = |s: &Stmt| match &s.kind {
        StmtKind::Def { name, .. } => name.clone(),
        _ => unreachable!(),
    };
    let siblings: BTreeSet<String> = nested.iter().map(def_name).collect();

    // What each nested function captures from THIS function's locals.
    let mut caps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for g in &nested {
        let StmtKind::Def {
            name,
            params: gp,
            body: gb,
            ..
        } = &g.kind
        else {
            unreachable!()
        };
        let free = free_vars_of(gp, gb);
        caps.insert(
            name.clone(),
            free.intersection(&outer_locals).cloned().collect(),
        );
    }

    // A nested function that CALLS a capturing sibling must also receive that
    // sibling's captures so it can pass them along. Iterate to a fixpoint.
    loop {
        let mut changed = false;
        for g in &nested {
            let name = def_name(g);
            let StmtKind::Def { body: gb, .. } = &g.kind else {
                unreachable!()
            };
            let mut called = BTreeSet::new();
            collect_calls(gb, &mut called);
            let mut add: BTreeSet<String> = BTreeSet::new();
            for h in called.intersection(&siblings) {
                if h != &name && let Some(hc) = caps.get(h) {
                    add.extend(hc.iter().cloned());
                }
            }
            let entry = caps.get_mut(&name).expect("seeded above");
            let before = entry.len();
            entry.extend(add);
            if entry.len() != before {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // A pass-through capture can't survive a same-named local in the middle
    // function — it would pass its own value instead of the enclosing one.
    for g in &nested {
        let StmtKind::Def {
            name,
            params: gp,
            body: gb,
            ..
        } = &g.kind
        else {
            unreachable!()
        };
        let own = function_locals(gp, gb);
        if let Some(c) = caps[name].iter().find(|c| own.contains(*c)) {
            return Err(CompileError::at(
                g.line,
                format!(
                    "nested function '{name}' has its own '{c}', which shadows the \
                     '{c}' a function it calls captures — rename one of them"
                ),
            ));
        }
    }

    let cap_args: BTreeMap<String, Vec<String>> = caps
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
        .collect();

    // Pass the captures at every call site (in this body and in the lifted
    // functions, including recursive and sibling calls).
    rewrite_calls(&mut kept, &cap_args);
    for g in &mut nested {
        if let StmtKind::Def { body: gb, .. } = &mut g.kind {
            rewrite_calls(gb, &cap_args);
        }
    }

    // Captures become leading parameters, then lift (recursing first so a
    // deeper function is emitted before the one that calls it).
    for mut g in nested {
        let name = def_name(&g);
        if let StmtKind::Def {
            params: gp,
            param_types,
            ..
        } = &mut g.kind
        {
            let cs = &cap_args[&name];
            let mut np = cs.clone();
            np.append(gp);
            *gp = np;
            let mut nt: Vec<Option<Expr>> = cs.iter().map(|_| None).collect();
            nt.append(param_types);
            *param_types = nt;
        }
        if !names.insert(name.clone()) {
            return Err(CompileError::at(
                g.line,
                format!(
                    "a function named '{name}' is already defined — nested \
                     function names must be unique (no shadowing yet)"
                ),
            ));
        }
        lift_in_function(&mut g, names, out)?;
        out.push(g);
    }
    *body = kept;
    Ok(())
}

/// Lift every nested function to module level (converting captures to leading
/// parameters). Statement order is preserved: each lifted function is inserted
/// just before the function it came out of, so the step debugger — which
/// registers a `def` when it executes it — still sees definitions before use.
pub fn hoist_nested_functions(program: Vec<Stmt>) -> Result<Vec<Stmt>, CompileError> {
    let mut names: BTreeSet<String> = program
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Def { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut out: Vec<Stmt> = Vec::new();
    for mut s in program {
        if matches!(s.kind, StmtKind::Def { .. }) {
            let mut lifted = Vec::new();
            lift_in_function(&mut s, &mut names, &mut lifted)?;
            out.extend(lifted);
        }
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn hoist(src: &str) -> Result<Vec<Stmt>, String> {
        let toks = lexer::lex(src).unwrap();
        let stmts = parser::parse(&toks).unwrap();
        hoist_nested_functions(stmts).map_err(|e| e.message)
    }

    /// Every top-level def, as (name, params) — and no nested defs survive.
    fn defs(stmts: &[Stmt]) -> Vec<(String, Vec<String>)> {
        for s in stmts {
            if let StmtKind::Def { body, .. } = &s.kind {
                assert!(
                    !body.iter().any(|b| matches!(b.kind, StmtKind::Def { .. })),
                    "a nested def survived hoisting"
                );
            }
        }
        stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Def { name, params, .. } => Some((name.clone(), params.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lifts_a_non_capturing_nested_def() {
        let out =
            hoist("def outer():\n    def inner():\n        return 1\n    return inner()\n").unwrap();
        assert_eq!(
            defs(&out),
            vec![("inner".into(), vec![]), ("outer".into(), vec![])]
        );
    }

    #[test]
    fn a_capture_becomes_a_leading_parameter() {
        let out =
            hoist("def outer():\n    x = 5\n    def inner():\n        return x + 1\n    return inner()\n")
                .unwrap();
        assert_eq!(
            defs(&out),
            vec![
                ("inner".into(), vec!["x".into()]),
                ("outer".into(), vec![])
            ]
        );
    }

    #[test]
    fn captures_prepend_before_existing_params_so_defaults_stay_trailing() {
        let out = hoist(
            "def outer(n):\n    def add(a, b=2):\n        return a + b + n\n    return add(1)\n",
        )
        .unwrap();
        assert_eq!(
            defs(&out),
            vec![
                ("add".into(), vec!["n".into(), "a".into(), "b".into()]),
                ("outer".into(), vec!["n".into()])
            ]
        );
    }

    #[test]
    fn multiple_captures_are_deterministic_and_sorted() {
        let out = hoist(
            "def outer():\n    b = 1\n    a = 2\n    def f():\n        return a + b\n    return f()\n",
        )
        .unwrap();
        assert_eq!(defs(&out)[0], ("f".into(), vec!["a".into(), "b".into()]));
    }

    #[test]
    fn a_deeper_function_threads_its_capture_through_the_middle() {
        // `deep` needs outer's `x`, so `mid` must receive it to pass along.
        let out = hoist(
            "def outer():\n    x = 7\n    def mid():\n        def deep():\n            return x\n        return deep()\n    return mid()\n",
        )
        .unwrap();
        let d = defs(&out);
        assert_eq!(d.iter().find(|(n, _)| n == "mid").unwrap().1, vec!["x"]);
        assert_eq!(d.iter().find(|(n, _)| n == "deep").unwrap().1, vec!["x"]);
    }

    #[test]
    fn calling_a_capturing_sibling_propagates_the_capture() {
        let out = hoist(
            "def outer():\n    x = 1\n    def a():\n        return b()\n    def b():\n        return x\n    return a()\n",
        )
        .unwrap();
        let d = defs(&out);
        // `a` doesn't read x itself, but must receive it to pass to `b`.
        assert_eq!(d.iter().find(|(n, _)| n == "a").unwrap().1, vec!["x"]);
        assert_eq!(d.iter().find(|(n, _)| n == "b").unwrap().1, vec!["x"]);
    }

    #[test]
    fn reading_a_global_or_calling_a_function_is_not_capture() {
        let hp = |src: &str| -> Vec<String> {
            let out = hoist(src).unwrap();
            defs(&out)
                .into_iter()
                .find(|(n, _)| n == "h")
                .expect("h was lifted")
                .1
        };
        // A module global isn't an enclosing local, so it isn't captured.
        assert!(hp("G = 1\ndef o():\n    def h():\n        return G\n    return h()\n").is_empty());
        // Nor is a call to another function.
        assert!(
            hp("def t():\n    return 1\ndef o():\n    def h():\n        return t()\n    return h()\n")
                .is_empty()
        );
    }

    #[test]
    fn a_shadowing_pass_through_is_a_clean_error() {
        // `a` must pass outer's y to `b`, but has its own y — ambiguous.
        let err = hoist(
            "def outer():\n    y = 1\n    def b():\n        return y\n    def a():\n        y = 99\n        return b()\n    return a()\n",
        )
        .unwrap_err();
        assert!(err.contains("shadows"), "{err}");
    }

    #[test]
    fn duplicate_nested_names_error() {
        let err = hoist(
            "def a():\n    def h():\n        return 1\n    return h()\ndef b():\n    def h():\n        return 2\n    return h()\n",
        )
        .unwrap_err();
        assert!(err.contains("already defined"), "{err}");
    }

    #[test]
    fn statement_order_is_preserved_with_the_lift_before_its_parent() {
        let out = hoist("x = 1\ndef o():\n    def i():\n        return 2\n    return i()\nprint(o())\n")
            .unwrap();
        // x = 1 stays first; `i` is inserted just before `o`; print stays last.
        assert!(matches!(out[0].kind, StmtKind::Assign(..)));
        assert_eq!(defs(&out), vec![("i".into(), vec![]), ("o".into(), vec![])]);
        assert!(matches!(out[3].kind, StmtKind::Expr(_)));
    }

    #[test]
    fn a_program_without_nested_defs_is_unchanged() {
        let out = hoist("def f():\n    return 1\nx = f()\nprint(x)\n").unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(defs(&out), vec![("f".into(), vec![])]);
    }
}
