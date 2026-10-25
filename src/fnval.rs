//! First-class functions, Milestone 1 (the monomorphic case).
//!
//! A function value may only ever come from directly naming a known
//! top-level `def` — never computed (`SUBSET_POLICY.md`'s bright line
//! already forbids that) — so the set of concrete functions any binding can
//! ever hold is statically enumerable. This pass computes that set for
//! every module-level variable and top-level function parameter, and where
//! it resolves to exactly one function, REWRITES the program in place so
//! every call through it becomes a direct call to that function. Nothing
//! downstream (check.rs, codegen.rs, llvm.rs, debug.rs) needs to know this
//! feature exists: by the time they run, the AST contains only ordinary
//! direct calls, exactly as if the student had written the resolved name
//! themselves.
//!
//! Run AFTER `hoist::hoist_nested_functions` (so every function is flat,
//! top-level, and uniquely named) and BEFORE `check::gate` (so this is a
//! shared-front-end pass both backends inherit, per the project's rule that
//! a new check belongs in the shared front end, not a backend).
//!
//! Scope, deliberately narrow for this milestone:
//! - Module level and plain top-level `def` bodies only — not class methods
//!   (first-class *bound* methods are a separately deferred, harder
//!   feature; see `CLASSES_DESIGN.md`) and not inside a function that is
//!   itself still nested at this point (none should be, post-hoist).
//! - A binding must resolve to EXACTLY one function everywhere it is ever
//!   assigned or passed — including "sometimes a function, sometimes
//!   something else entirely" (not just "more than one function"), because
//!   either way a single static target would silently diverge from
//!   CPython's per-call-site duck typing at the branch this analysis can't
//!   see. Both are the same clean error, not a guess.
//! - Returning a function value, and storing one in a container, remain
//!   clean errors (unaffected — nothing here changes how those are
//!   evaluated, so they keep failing exactly as they did before this pass
//!   existed).
//!
//! Multi-candidate closed sets (the same binding provably holding one of
//! several functions) are an explicitly sequenced follow-up — tag + switch
//! dispatch, mirroring the native backend's existing `dyn_<name>_<n>`
//! class-dispatch pattern — not built here.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::error::{CompileError, ErrorKind};
use crate::hoist::stmt_exprs_mut;
use crate::lint::{BlockScope, each_child_expr, for_each_child_block, stmt_exprs};
use std::collections::{BTreeSet, HashMap, HashSet};

/// `None` = module level; `Some(name)` = inside that top-level `def`.
type Scope = Option<String>;
type Key = (Scope, String);

/// Resolve and desugar first-class function calls in place.
pub fn resolve(program: &mut Vec<Stmt>) -> Result<(), CompileError> {
    let params_of: HashMap<String, Vec<String>> = program
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Def { name, params, .. } => Some((name.clone(), params.clone())),
            _ => None,
        })
        .collect();
    let def_names: HashSet<String> = params_of.keys().cloned().collect();

    // candidates[(scope, name)] = every function `name` might hold there.
    // impure[(scope, name)] = `name` was ALSO, somewhere, fed a value that
    // doesn't resolve to a function at all — fatal if it's ever called, same
    // as holding more than one function (both are "can't pick just one").
    let mut candidates: HashMap<Key, BTreeSet<String>> = HashMap::new();
    let mut impure: HashSet<Key> = HashSet::new();
    loop {
        let mut changed = false;
        for_each_scope(program, |scope, stmts| {
            seed_assignments(
                stmts,
                scope,
                &def_names,
                &mut candidates,
                &mut impure,
                &mut changed,
            );
        });
        for_each_scope(program, |scope, stmts| {
            seed_call_args(
                stmts,
                scope,
                &def_names,
                &params_of,
                &mut candidates,
                &mut impure,
                &mut changed,
            );
        });
        if !changed {
            break;
        }
    }

    let mut err: Option<CompileError> = None;
    for_each_scope_mut(program, |scope, stmts| {
        if err.is_none() {
            rewrite_scope(
                stmts,
                &scope,
                &def_names,
                &params_of,
                &candidates,
                &impure,
                &mut err,
            );
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Call `f` once per scope: the module level (all top-level statements, the
/// `Def`s among them included verbatim), then once more per top-level
/// `def`'s body. `f` is responsible for recursing into same-scope child
/// blocks (`if`/`for`/`while`) itself, via `for_each_child_block`, so a
/// scope's statements are always a flat list handed to it at once.
fn for_each_scope(program: &[Stmt], mut f: impl FnMut(&Scope, &[Stmt])) {
    f(&None, program);
    for s in program {
        if let StmtKind::Def { name, body, .. } = &s.kind {
            f(&Some(name.clone()), body);
        }
    }
}

fn for_each_scope_mut(program: &mut Vec<Stmt>, mut f: impl FnMut(Scope, &mut Vec<Stmt>)) {
    f(None, program);
    for s in program.iter_mut() {
        if let StmtKind::Def { name, body, .. } = &mut s.kind {
            f(Some(name.clone()), body);
        }
    }
}

/// If `value` is a bare `Name` that already resolves to a function (either
/// it names a top-level `def` directly, or it's a local in `scope` already
/// known to resolve), return that resolved set. `None` means "not a
/// function-value expression at all" — not an error, just nothing to seed.
fn resolved_set(
    value: &Expr,
    scope: &Scope,
    def_names: &HashSet<String>,
    candidates: &HashMap<Key, BTreeSet<String>>,
) -> Option<BTreeSet<String>> {
    let ExprKind::Name(n) = &value.kind else {
        return None;
    };
    if def_names.contains(n) {
        return Some(std::iter::once(n.clone()).collect());
    }
    candidates.get(&(scope.clone(), n.clone())).cloned()
}

fn merge(
    candidates: &mut HashMap<Key, BTreeSet<String>>,
    key: Key,
    src: BTreeSet<String>,
    changed: &mut bool,
) {
    let entry = candidates.entry(key).or_default();
    let before = entry.len();
    entry.extend(src);
    if entry.len() != before {
        *changed = true;
    }
}

fn mark_impure(impure: &mut HashSet<Key>, key: Key, changed: &mut bool) {
    if impure.insert(key) {
        *changed = true;
    }
}

/// Seed from direct assignment: `name = <bare Name>`.
fn seed_assignments(
    stmts: &[Stmt],
    scope: &Scope,
    def_names: &HashSet<String>,
    candidates: &mut HashMap<Key, BTreeSet<String>>,
    impure: &mut HashSet<Key>,
    changed: &mut bool,
) {
    for s in stmts {
        if let StmtKind::Assign(name, value) = &s.kind {
            match resolved_set(value, scope, def_names, candidates) {
                Some(src) => merge(candidates, (scope.clone(), name.clone()), src, changed),
                // Any other RHS shape: if `name` is ever also fed a real
                // function value elsewhere, this assignment makes it
                // impure (sometimes a function, sometimes not) — tracked
                // unconditionally; validation only acts on it if `name`
                // also has nonempty candidates by the time the fixpoint
                // settles.
                None => mark_impure(impure, (scope.clone(), name.clone()), changed),
            }
        }
        for_each_child_block(s, |body, bs| {
            if bs == BlockScope::Same {
                seed_assignments(body, scope, def_names, candidates, impure, changed);
            }
        });
    }
}

/// Seed from call-argument flow into a known top-level function's own
/// parameters: `apply(double, 5)` feeds `apply`'s first parameter.
fn seed_call_args(
    stmts: &[Stmt],
    scope: &Scope,
    def_names: &HashSet<String>,
    params_of: &HashMap<String, Vec<String>>,
    candidates: &mut HashMap<Key, BTreeSet<String>>,
    impure: &mut HashSet<Key>,
    changed: &mut bool,
) {
    for s in stmts {
        stmt_exprs(s, &mut |e| {
            walk_for_call_args(e, scope, def_names, params_of, candidates, impure, changed)
        });
        for_each_child_block(s, |body, bs| {
            if bs == BlockScope::Same {
                seed_call_args(
                    body, scope, def_names, params_of, candidates, impure, changed,
                );
            }
        });
    }
}

fn walk_for_call_args(
    e: &Expr,
    scope: &Scope,
    def_names: &HashSet<String>,
    params_of: &HashMap<String, Vec<String>>,
    candidates: &mut HashMap<Key, BTreeSet<String>>,
    impure: &mut HashSet<Key>,
    changed: &mut bool,
) {
    if let ExprKind::Call(callee, args) = &e.kind
        && let Some(params) = params_of.get(callee)
    {
        let mut positional = 0usize;
        for a in args {
            let (pname, inner) = match &a.kind {
                ExprKind::Kwarg(k, v) => (Some(k.clone()), v.as_ref()),
                _ => {
                    let p = params.get(positional).cloned();
                    positional += 1;
                    (p, a)
                }
            };
            let Some(pname) = pname else { continue };
            let key = (Some(callee.clone()), pname);
            match resolved_set(inner, scope, def_names, candidates) {
                Some(src) => merge(candidates, key, src, changed),
                // Not every argument here is a function value (e.g. the
                // `5` in `apply(double, 5)`) — only mark impure when the
                // PARAMETER on the receiving end is otherwise known to be
                // used as a function value somewhere, which the full
                // program may not have told us yet. Conservatively mark
                // impure whenever the expression isn't itself a bare Name
                // at all AND isn't a literal/constant shape that's clearly
                // not meant as a callback (so passing `5` for an ordinary
                // int parameter never gets flagged). A bare Name that
                // simply doesn't resolve (e.g. an unrelated local) is the
                // ambiguous case we must catch, so it DOES mark impure.
                None => {
                    if matches!(inner.kind, ExprKind::Name(_)) {
                        mark_impure(impure, key, changed);
                    }
                }
            }
        }
    }
    each_child_expr(e, &mut |c| {
        walk_for_call_args(c, scope, def_names, params_of, candidates, impure, changed)
    });
}

fn ambiguous_error(line: usize, name: &str) -> CompileError {
    CompileError::at(
        line,
        format!(
            "can't tell which function '{name}' refers to here — assign it directly \
             from a function's name (f = somefunction), or pass it in directly; this \
             version doesn't support a function value that could be more than one \
             function (or something else) yet"
        ),
    )
    .with_kind(ErrorKind::Name)
}

/// Resolve `name` in place (a `Call`'s callee, or a bare `Name` value) or
/// set `*err`. Does nothing if `name` was never seeded as a function value
/// at all (today's existing error paths handle that case unchanged).
fn try_resolve_in_place(
    name: &mut String,
    line: usize,
    scope: &Scope,
    def_names: &HashSet<String>,
    candidates: &HashMap<Key, BTreeSet<String>>,
    impure: &HashSet<Key>,
    err: &mut Option<CompileError>,
) {
    if def_names.contains(name.as_str()) {
        return;
    }
    let key = (scope.clone(), name.clone());
    let Some(set) = candidates.get(&key) else {
        return;
    };
    if impure.contains(&key) || set.len() != 1 {
        *err = Some(ambiguous_error(line, name));
        return;
    }
    *name = set.iter().next().unwrap().clone();
}

/// One scope's statements: delete now-dead function-alias assignments,
/// rewrite calls/names, recurse into same-scope child blocks.
fn rewrite_scope(
    stmts: &mut Vec<Stmt>,
    scope: &Scope,
    def_names: &HashSet<String>,
    params_of: &HashMap<String, Vec<String>>,
    candidates: &HashMap<Key, BTreeSet<String>>,
    impure: &HashSet<Key>,
    err: &mut Option<CompileError>,
) {
    let mut i = 0;
    while i < stmts.len() {
        if err.is_some() {
            return;
        }
        if let StmtKind::Assign(name, value) = &stmts[i].kind
            && matches!(value.kind, ExprKind::Name(_))
        {
            let key = (scope.clone(), name.clone());
            if candidates.get(&key).is_some_and(|s| s.len() == 1) && !impure.contains(&key) {
                // Fully resolved function alias — every call through it was
                // (or will be) rewritten to a direct call below, so the
                // binding itself is now dead. Drop it rather than leave a
                // store nothing downstream knows how to evaluate (a bare
                // top-level function name is not a value expression).
                stmts.remove(i);
                continue;
            }
        }
        stmt_exprs_mut(&mut stmts[i], &mut |e| {
            rewrite_expr(e, scope, def_names, params_of, candidates, impure, err)
        });
        // Same-scope child blocks only (`if`/`for`/`while`) — a nested
        // `Def`'s body is handled by `for_each_scope_mut`'s own sweep, with
        // the correct `Some(that def's name)` scope; recursing into it here
        // too, under this call's scope, would attribute it wrongly.
        same_scope_blocks_mut(&mut stmts[i], &mut |body| {
            rewrite_scope(body, scope, def_names, params_of, candidates, impure, err)
        });
        i += 1;
    }
}

fn rewrite_expr(
    e: &mut Expr,
    scope: &Scope,
    def_names: &HashSet<String>,
    params_of: &HashMap<String, Vec<String>>,
    candidates: &HashMap<Key, BTreeSet<String>>,
    impure: &HashSet<Key>,
    err: &mut Option<CompileError>,
) {
    if err.is_some() {
        return;
    }
    let line = e.line;
    match &mut e.kind {
        ExprKind::Name(n) => {
            try_resolve_in_place(n, line, scope, def_names, candidates, impure, err);
            return; // leaf
        }
        ExprKind::Call(name, args) => {
            try_resolve_in_place(name, line, scope, def_names, candidates, impure, err);
            if err.is_some() {
                return;
            }
            // Arguments feeding a known function's OWN function-valued
            // parameters are now dead at runtime (every call through that
            // parameter, inside that function, was rewritten to a direct
            // call) — substitute a cheap placeholder rather than asking
            // codegen to evaluate a bare top-level function name as a
            // value, which nothing downstream supports.
            if let Some(params) = params_of.get(name.as_str()) {
                let mut positional = 0usize;
                for a in args.iter_mut() {
                    let pname = match &a.kind {
                        ExprKind::Kwarg(k, _) => Some(k.clone()),
                        _ => {
                            let p = params.get(positional).cloned();
                            positional += 1;
                            p
                        }
                    };
                    let Some(pname) = pname else { continue };
                    let key = (Some(name.clone()), pname);
                    let dead = candidates.get(&key).is_some_and(|s| s.len() == 1)
                        && !impure.contains(&key);
                    if dead {
                        let target = match &mut a.kind {
                            ExprKind::Kwarg(_, v) => v.as_mut(),
                            _ => a,
                        };
                        if matches!(target.kind, ExprKind::Name(_)) {
                            target.kind = ExprKind::NoneLit;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    each_child_expr_mut(e, &mut |c| {
        rewrite_expr(c, scope, def_names, params_of, candidates, impure, err)
    });
}

/// Mutable twin of `lint::each_child_expr` — every direct child expression,
/// exhaustive on purpose (see that function's own doc comment for why).
fn each_child_expr_mut(e: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    match &mut e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_) => {}
        ExprKind::Name(_) => {}
        ExprKind::Unary(_, x) | ExprKind::Attr(x, _) | ExprKind::Kwarg(_, x) => f(x),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            f(a);
            f(b);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                f(a);
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            f(recv);
            for a in args {
                f(a);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) => {
            for x in xs {
                f(x);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                f(k);
                f(v);
            }
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            f(obj);
            for o in [start, stop, step].into_iter().flatten() {
                f(o);
            }
        }
        ExprKind::ListComp { element, clauses } | ExprKind::SetComp { element, clauses } => {
            f(element);
            each_clause_mut(clauses, f);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            f(key);
            f(value);
            each_clause_mut(clauses, f);
        }
        ExprKind::IfExp { cond, then, orelse } => {
            f(cond);
            f(then);
            f(orelse);
        }
    }
}

fn each_clause_mut(clauses: &mut [crate::ast::CompClause], f: &mut impl FnMut(&mut Expr)) {
    for c in clauses {
        match c {
            crate::ast::CompClause::For { iter, .. } => f(iter),
            crate::ast::CompClause::If(e) => f(e),
        }
    }
}

/// Mutable, scope-aware twin of `lint::for_each_child_block`'s
/// `BlockScope::Same` arms only (`if`/`for`/`while`) — deliberately omits
/// `Def`/`ClassDef`, unlike `hoist::child_blocks_mut`, so a caller walking
/// one function's body never silently wanders into another scope.
fn same_scope_blocks_mut(s: &mut Stmt, f: &mut impl FnMut(&mut Vec<Stmt>)) {
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
        | StmtKind::While { body, .. } => f(body),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn run(src: &str) -> Result<Vec<Stmt>, String> {
        let toks = lexer::lex(src).unwrap();
        let stmts = parser::parse(&toks).unwrap();
        let mut stmts = crate::hoist::hoist_nested_functions(stmts).map_err(|e| e.to_string())?;
        resolve(&mut stmts).map_err(|e| e.to_string())?;
        Ok(stmts)
    }

    fn has_call_to(stmts: &[Stmt], target: &str) -> bool {
        fn in_expr(e: &Expr, target: &str) -> bool {
            let mut found = matches!(&e.kind, ExprKind::Call(n, _) if n == target);
            each_child_expr(e, &mut |c| found |= in_expr(c, target));
            found
        }
        stmts.iter().any(|s| {
            let mut found = false;
            stmt_exprs(s, &mut |e| found |= in_expr(e, target));
            for_each_child_block(s, |body, _| found |= has_call_to(body, target));
            found
        })
    }

    #[test]
    fn a_plain_alias_call_resolves_to_a_direct_call() {
        let out = run("def double(x):\n    return x * 2\nf = double\nprint(f(5))\n").unwrap();
        assert!(has_call_to(&out, "double"));
        assert!(!has_call_to(&out, "f"));
    }

    #[test]
    fn the_alias_assignment_itself_is_dropped() {
        let out = run("def double(x):\n    return x * 2\nf = double\nprint(f(5))\n").unwrap();
        assert!(
            !out.iter()
                .any(|s| matches!(&s.kind, StmtKind::Assign(n, _) if n == "f"))
        );
    }

    #[test]
    fn a_callback_parameter_resolves_to_a_direct_call() {
        let out = run(
            "def double(x):\n    return x * 2\ndef apply(f, x):\n    return f(x)\nprint(apply(double, 5))\n",
        )
        .unwrap();
        let apply = out
            .iter()
            .find_map(|s| match &s.kind {
                StmtKind::Def { name, body, .. } if name == "apply" => Some(body.clone()),
                _ => None,
            })
            .unwrap();
        assert!(has_call_to(&apply, "double"));
        assert!(!has_call_to(&apply, "f"));
    }

    #[test]
    fn two_different_functions_through_the_same_name_is_a_clean_error() {
        let err = run(
            "def double(x):\n    return x * 2\ndef triple(x):\n    return x * 3\ndef pick(c):\n    if c:\n        f = double\n    else:\n        f = triple\n    return f(1)\n",
        )
        .unwrap_err();
        assert!(err.contains("can't tell which function"), "{err}");
    }

    #[test]
    fn a_name_sometimes_a_function_and_sometimes_not_is_a_clean_error() {
        let err =
            run("def double(x):\n    return x * 2\nf = double\nf = 5\nprint(f(1))\n").unwrap_err();
        assert!(err.contains("can't tell which function"), "{err}");
    }

    #[test]
    fn an_ordinary_program_with_no_function_values_is_untouched() {
        let out = run("def double(x):\n    return x * 2\nprint(double(5))\n").unwrap();
        assert!(has_call_to(&out, "double"));
    }

    #[test]
    fn a_plain_non_function_call_target_is_left_for_existing_errors() {
        // `x` is never fed a function anywhere — not this pass's business.
        let out = run("x = 5\nprint(x)\n").unwrap();
        assert_eq!(out.len(), 2);
    }
}
