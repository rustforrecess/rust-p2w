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

/// All names *mentioned* — read OR assigned — anywhere within `s`, recursing
/// through control-flow bodies (`if`/`for`/`while` and their conditions) but NOT
/// into `def`/`class` bodies: names read inside those go to `pinned` instead,
/// because the function/method can execute at *any* later point, so anything it
/// reads must never be treated as dead. Over-approximate on purpose (e.g. type
/// annotations and comprehension binders count as mentions): extra mentions only
/// keep values alive longer — never unsound.
fn stmt_mentions(s: &Stmt, out: &mut BTreeSet<String>, pinned: &mut BTreeSet<String>) {
    vars_assigned(s, out);
    match &s.kind {
        StmtKind::Expr(e) | StmtKind::Assign(_, e) => vars_read(e, out),
        StmtKind::AnnAssign { ann, value, .. } => {
            vars_read(ann, out);
            vars_read(value, out);
        }
        StmtKind::If {
            cond,
            body,
            elifs,
            else_body,
        } => {
            vars_read(cond, out);
            for st in body {
                stmt_mentions(st, out, pinned);
            }
            for (c, b) in elifs {
                vars_read(c, out);
                for st in b {
                    stmt_mentions(st, out, pinned);
                }
            }
            if let Some(b) = else_body {
                for st in b {
                    stmt_mentions(st, out, pinned);
                }
            }
        }
        StmtKind::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            vars_read(start, out);
            vars_read(end, out);
            vars_read(step, out);
            for st in body {
                stmt_mentions(st, out, pinned);
            }
        }
        StmtKind::ForEach { iterable, body, .. } => {
            vars_read(iterable, out);
            for st in body {
                stmt_mentions(st, out, pinned);
            }
        }
        StmtKind::While { cond, body } => {
            vars_read(cond, out);
            for st in body {
                stmt_mentions(st, out, pinned);
            }
        }
        StmtKind::Def { defaults, body, .. } => {
            // Defaults evaluate at call sites in this subset — pin their reads
            // too, alongside everything the body could ever read.
            for d in defaults {
                vars_read(d, pinned);
            }
            let mut inner = BTreeSet::new();
            for st in body {
                stmt_mentions(st, &mut inner, pinned);
            }
            pinned.extend(inner);
        }
        StmtKind::ClassDef {
            methods,
            class_vars,
            ..
        } => {
            // Class-var initializers run at class-creation time (a mention now);
            // method bodies run whenever — pin their reads.
            for (_, v) in class_vars {
                vars_read(v, out);
            }
            let mut inner = BTreeSet::new();
            for m in methods {
                for st in &m.body {
                    stmt_mentions(st, &mut inner, pinned);
                }
            }
            pinned.extend(inner);
        }
        StmtKind::Return(v) => {
            if let Some(e) = v {
                vars_read(e, out);
            }
        }
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            vars_read(target, out);
            vars_read(index, out);
            vars_read(value, out);
        }
        StmtKind::SetAttr { obj, value, .. } => {
            vars_read(obj, out);
            vars_read(value, out);
        }
        StmtKind::UnpackAssign { targets, value } => {
            // Name targets were collected by vars_assigned; Index/Attr targets
            // read their bases.
            for t in targets {
                if !matches!(t.kind, ExprKind::Name(_)) {
                    vars_read(t, out);
                }
            }
            vars_read(value, out);
        }
        StmtKind::Break | StmtKind::Continue | StmtKind::Import(_) => {}
    }
}

/// Last-use information for one body (a function's statement list, or the module
/// top level), at **statement granularity** via **last-mention analysis**: a
/// binding is dead after statement `idx` iff it was assigned at the top level of
/// this body at or before `idx`, `idx` is its last mention (read OR assignment,
/// anywhere within the statement, loops included as opaque units), nothing after
/// `idx` mentions it, and it isn't pinned by a `def`/`class` body.
///
/// Deliberately coarser than full backward liveness, for soundness:
///
/// - Counting *assignments* as mentions means an early release can never be
///   followed by a reassignment's release-the-old-value — no double release.
/// - Statement granularity means a value used in only one branch of an `if` is
///   released *after* the whole statement — no branch balancing.
/// - Loop bodies are opaque units, so back edges can't resurrect a "dead" name.
/// - Per-binding, so aliasing is safe under per-slot ownership (`y = x` gives
///   each slot its own +1; releasing `y`'s never touches `x`'s).
///
/// The emitter still applies its own slot policy on top (skip borrowed params,
/// raw/unboxed slots): the analysis reports *names*, ownership stays the
/// emitter's call. Upgrading to full liveness (early release before a
/// reassignment) is the follow-on — see `docs/REUSE_PLAN.md`.
#[derive(Debug, Default)]
pub struct Liveness {
    /// Per-statement-index: bindings whose last mention is that statement (so
    /// the emitter may release them right after it and drop them from the
    /// scope-exit release set).
    dead_after: Vec<Vec<String>>,
}

impl Liveness {
    /// Analyze a body (last-mention, statement granularity — see type docs).
    pub fn analyze(body: &[Stmt]) -> Liveness {
        // Per-statement mention sets + the pinned set (def/class-body reads).
        let mut pinned = BTreeSet::new();
        let mut mentions: Vec<BTreeSet<String>> = Vec::with_capacity(body.len());
        for s in body {
            let mut m = BTreeSet::new();
            stmt_mentions(s, &mut m, &mut pinned);
            mentions.push(m);
        }
        // bound_by[idx]: names assigned at the top level of this body at <= idx
        // (params and globals are never in here, so they can't be reported).
        let mut bound = BTreeSet::new();
        let mut bound_by: Vec<BTreeSet<String>> = Vec::with_capacity(body.len());
        for s in body {
            vars_assigned(s, &mut bound);
            bound_by.push(bound.clone());
        }
        // Backward: `after` = everything mentioned strictly later. A name whose
        // last mention is idx (and is bound, and not pinned) dies after idx.
        let mut after: BTreeSet<String> = BTreeSet::new();
        let mut dead_after = vec![Vec::new(); body.len()];
        for idx in (0..body.len()).rev() {
            for name in &mentions[idx] {
                if bound_by[idx].contains(name) && !after.contains(name) && !pinned.contains(name) {
                    dead_after[idx].push(name.clone());
                }
            }
            after.extend(mentions[idx].iter().cloned());
        }
        Liveness { dead_after }
    }

    /// Bindings whose last mention is statement `idx` — safe to release
    /// immediately after emitting it (and to drop from the scope-exit release
    /// set), subject to the emitter's own slot policy (borrowed/raw slots).
    pub fn dead_after(&self, idx: usize) -> &[String] {
        self.dead_after.get(idx).map_or(&[], Vec::as_slice)
    }

    /// True when no binding is reported dead anywhere — i.e. this body offers
    /// no early releases (everything lives to scope end).
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

    fn dead(live: &Liveness, idx: usize) -> Vec<&str> {
        live.dead_after(idx).iter().map(String::as_str).collect()
    }

    #[test]
    fn chain_dies_stage_by_stage() {
        // The wl_chain shape: each stage's input dies at the stage that consumes
        // it — the drop the reuse tier needs.
        let body = parse("a = [1, 2]\nb = [x + 1 for x in a]\nc = [y * 2 for y in b]\nprint(c)\n");
        let live = Liveness::analyze(&body);
        assert_eq!(dead(&live, 0), Vec::<&str>::new());
        assert_eq!(dead(&live, 1), vec!["a"]);
        assert_eq!(dead(&live, 2), vec!["b"]);
        assert_eq!(dead(&live, 3), vec!["c"]);
        assert!(!live.is_conservative());
        assert!(live.dead_after(999).is_empty(), "out-of-range is empty");
    }

    #[test]
    fn reassignment_counts_as_a_mention() {
        // `a` is reassigned at stmt 2, so it must NOT be dead after stmt 1 —
        // this is the no-double-release property (the assign site releases the
        // old value; an early release before it would free twice).
        let body = parse("a = [1]\nprint(a)\na = [2]\nprint(a)\n");
        let live = Liveness::analyze(&body);
        assert_eq!(dead(&live, 1), Vec::<&str>::new());
        assert_eq!(dead(&live, 3), vec!["a"]);
    }

    #[test]
    fn loops_are_opaque_units() {
        // xs is read inside the while body: it dies after the whole loop
        // statement, never inside it (back edges can't resurrect it).
        let body =
            parse("xs = [1]\ni = 0\nwhile i < 3:\n    print(xs)\n    i = i + 1\nprint(\"end\")\n");
        let live = Liveness::analyze(&body);
        assert_eq!(dead(&live, 0), Vec::<&str>::new());
        assert_eq!(dead(&live, 1), Vec::<&str>::new());
        assert_eq!(dead(&live, 2), vec!["i", "xs"]);
    }

    #[test]
    fn def_bodies_pin_their_reads_forever() {
        // f() can be called at any later point, so x must never be dead.
        let body = parse("x = [1]\ndef f():\n    return x\nprint(\"hi\")\n");
        let live = Liveness::analyze(&body);
        for i in 0..body.len() {
            assert!(!dead(&live, i).contains(&"x"), "x pinned by def body");
        }
    }

    #[test]
    fn class_method_bodies_pin_their_reads() {
        let body = parse("y = [1]\nclass C:\n    def m(self):\n        return y\nprint(\"k\")\n");
        let live = Liveness::analyze(&body);
        for i in 0..body.len() {
            assert!(!dead(&live, i).contains(&"y"), "y pinned by method body");
        }
    }

    #[test]
    fn only_body_bound_names_are_reported() {
        // q is never assigned here (a param or global): never reported, even
        // though its last mention is stmt 0. Ditto the comprehension binder.
        let body = parse("print(q)\nb = [x * x for x in q]\nprint(b)\n");
        let live = Liveness::analyze(&body);
        for i in 0..body.len() {
            assert!(!dead(&live, i).contains(&"q"));
            assert!(
                !dead(&live, i).contains(&"x"),
                "comp binder isn't a body var"
            );
        }
        assert_eq!(dead(&live, 2), vec!["b"]);
    }

    #[test]
    fn unused_binding_dies_at_its_own_statement() {
        let body = parse("tmp = [1]\nprint(\"x\")\n");
        let live = Liveness::analyze(&body);
        assert_eq!(dead(&live, 0), vec!["tmp"]);
    }

    #[test]
    fn branch_use_dies_after_the_whole_if() {
        // a's last mention is inside one branch: it dies after the whole if
        // statement (statement granularity — no branch balancing needed).
        let body = parse("a = [1]\nflag = 1\nif flag:\n    print(a)\nprint(\"done\")\n");
        let live = Liveness::analyze(&body);
        assert_eq!(dead(&live, 0), Vec::<&str>::new());
        assert_eq!(dead(&live, 2), vec!["a", "flag"]);
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
