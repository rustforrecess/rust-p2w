//! Lightweight semantic lints over the parsed AST.
//!
//! All deliberately conservative — a lint that cries wolf is worse than no lint
//! in a K-12 tool. Currently:
//! - `typo_diagnostics` — "did you mean…?" for a call to a function that doesn't
//!   exist but near-misses a known name (`pint(i)` → `print`);
//! - `undefined_name_warnings` — a bare-name read of a variable bound nowhere in
//!   scope (the variable-name complement of the above), over-approximating
//!   "bound" so only genuinely-undefined names flag;
//! - `type_churn_warnings` — a name reused for a different *kind* of value
//!   (`x = 1` … `x = "hi"`), the instrument for the reject-vs-demote decision;
//! - `may_form_cycle`, `set_typed_names`, `set_operator_spans` — analysis seams
//!   the backends / IDE consume.

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::error::CompileError;
use crate::parser::did_you_mean;
use std::collections::HashMap;
use std::collections::HashSet;

/// A coarse *category* of value for the type-churn lint — NOT a real type, just
/// "what kind of thing" a name currently holds, to spot a name being reused for
/// a genuinely different kind of value (`x = 1` then `x = "hi"`). `Number`
/// deliberately merges int/float/bool: `avg = 0` then `avg = total / n` is a
/// natural numeric progression, not confusing churn, so it must NOT fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LintTy {
    Number,
    Text,
    List,
    Dict,
    Set,
    Tuple,
}

impl LintTy {
    /// The kid-facing noun for a message ("a number", "a list", …).
    fn noun(self) -> &'static str {
        match self {
            LintTy::Number => "a number",
            LintTy::Text => "text (a string)",
            LintTy::List => "a list",
            LintTy::Dict => "a dictionary",
            LintTy::Set => "a set",
            LintTy::Tuple => "a tuple",
        }
    }
}

/// The category of an expression, when it is *knowable* from syntax alone;
/// `None` means "don't know" (a call to a user function, a name, an index —
/// anything dynamic), and an unknown never triggers churn (we only warn when
/// two *known, different* categories collide). Conservative on purpose: a
/// false "unknown" just misses a warning; it never invents one.
fn lint_ty(e: &Expr) -> Option<LintTy> {
    match &e.kind {
        // (f-strings desugar in the parser to str concatenation / `str(...)`
        // calls, so they classify as Text through those arms — no FString
        // node here. Set literals parse as `Call("set", ...)`, handled below.)
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) => Some(LintTy::Number),
        ExprKind::Str(_) => Some(LintTy::Text),
        ExprKind::List(_) | ExprKind::ListComp { .. } => Some(LintTy::List),
        ExprKind::Dict(_) | ExprKind::DictComp { .. } => Some(LintTy::Dict),
        ExprKind::Tuple(_) => Some(LintTy::Tuple),
        ExprKind::Unary(UnOp::Neg, inner) => match lint_ty(inner) {
            Some(LintTy::Number) => Some(LintTy::Number),
            _ => None,
        },
        // Arithmetic/comparison over knowable operands stays a number; `+` is
        // overloaded (str/list concat too), so it only stays a number when
        // BOTH sides are known-numbers.
        ExprKind::Bin(op, a, b) => match op {
            BinOp::Add => match (lint_ty(a), lint_ty(b)) {
                (Some(LintTy::Number), Some(LintTy::Number)) => Some(LintTy::Number),
                (Some(LintTy::Text), Some(LintTy::Text)) => Some(LintTy::Text),
                (Some(LintTy::List), Some(LintTy::List)) => Some(LintTy::List),
                _ => None,
            },
            BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::FloorDiv
            | BinOp::Mod
            | BinOp::Pow
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Eq
            | BinOp::Ne => Some(LintTy::Number),
            _ => None,
        },
        // The common conversion/measure builtins have a knowable result kind —
        // this is what lets `age = input()` (text) then `age = int(age)`
        // (number) surface, the canonical case the reject-vs-demote decision
        // wants data on (see docs/COMPILER_FRONTIER.md task 3).
        ExprKind::Call(name, _) => match name.as_str() {
            "int" | "float" | "len" | "abs" | "round" | "ord" => Some(LintTy::Number),
            "str" | "input" | "chr" => Some(LintTy::Text),
            "list" | "sorted" => Some(LintTy::List),
            "dict" => Some(LintTy::Dict),
            "set" => Some(LintTy::Set),
            "tuple" => Some(LintTy::Tuple),
            _ => None,
        },
        _ => None,
    }
}

/// Type-churn warnings: a name reused for a genuinely different *kind* of value
/// within one scope (`x = 1` … `x = "hi"`). Each entry is `(line, message)`,
/// kid-facing. This is a **gentle lint only** — the compiler still runs the
/// program (an unannotated name that churns simply stays on the dynamic path,
/// output identical to CPython). It is the teaching surface *and* the
/// data-collection instrument for the deferred "demote vs. lint vs. reject"
/// policy in `docs/COMPILER_FRONTIER.md` task 3: measuring how often real
/// student code trips it (including the legit `age = int(age)` pattern) is
/// exactly the evidence that decision waits on.
///
/// Each scope (module top level, and every function/method body) is analyzed
/// independently — reusing `x` as a number in one function and text in another
/// is not churn. Within a scope, reassignments inside `if`/`for`/`while` bodies
/// count (they rebind the same name), but `def`/`class` bodies are separate
/// scopes, recursed on their own.
pub fn type_churn_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    churn_scope(stmts, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

/// Analyze one scope for churn, then recurse into nested `def`/`class` scopes.
fn churn_scope(stmts: &[Stmt], out: &mut Vec<(usize, String)>) {
    // name -> (established category, whether we've already warned about it).
    let mut seen: HashMap<String, (LintTy, bool)> = HashMap::new();
    churn_walk(stmts, &mut seen, out);
    // Nested scopes are independent.
    for s in stmts {
        match &s.kind {
            StmtKind::Def { body, .. } => churn_scope(body, out),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    churn_scope(&m.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Walk the assignments of one scope (descending through control-flow bodies,
/// which rebind names in the *same* scope, but not `def`/`class`).
fn churn_walk(
    stmts: &[Stmt],
    seen: &mut HashMap<String, (LintTy, bool)>,
    out: &mut Vec<(usize, String)>,
) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, value) | StmtKind::AnnAssign { name, value, .. } => {
                if let Some(ty) = lint_ty(value) {
                    match seen.get_mut(name) {
                        None => {
                            seen.insert(name.clone(), (ty, false));
                        }
                        Some((first, warned)) if *first != ty && !*warned => {
                            out.push((
                                s.line,
                                format!(
                                    "'{name}' held {} earlier but is {} here — reusing one \
                                     name for a different kind of value is a common source of \
                                     confusion; a new name is usually clearer",
                                    first.noun(),
                                    ty.noun()
                                ),
                            ));
                            *warned = true;
                        }
                        _ => {}
                    }
                }
            }
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                churn_walk(body, seen, out);
                for (_, b) in elifs {
                    churn_walk(b, seen, out);
                }
                if let Some(b) = else_body {
                    churn_walk(b, seen, out);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => churn_walk(body, seen, out),
            _ => {}
        }
    }
}

/// "Did you mean…?" diagnostics for calls to unknown, near-miss function names.
/// The known-builtins set comes from the central registry ([`crate::builtins`]),
/// so it can't drift from what codegen/blocks support.
pub fn typo_diagnostics(stmts: &[Stmt]) -> Vec<CompileError> {
    let mut defs: Vec<String> = Vec::new();
    collect_defs(stmts, &mut defs);
    let mut known: Vec<&str> = crate::builtins::names().collect();
    known.extend(defs.iter().map(String::as_str));

    let mut out = Vec::new();
    walk_stmts(stmts, &known, &mut out);
    out
}

/// Names the program defines and can call by bare name: `def`s and classes
/// (constructors), at any nesting.
fn collect_defs(stmts: &[Stmt], defs: &mut Vec<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Def { name, body, .. } => {
                defs.push(name.clone());
                collect_defs(body, defs);
            }
            StmtKind::ClassDef { name, methods, .. } => {
                defs.push(name.clone());
                for m in methods {
                    collect_defs(&m.body, defs);
                }
            }
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                collect_defs(body, defs);
                for (_, b) in elifs {
                    collect_defs(b, defs);
                }
                if let Some(b) = else_body {
                    collect_defs(b, defs);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => collect_defs(body, defs),
            _ => {}
        }
    }
}

fn walk_stmts(stmts: &[Stmt], known: &[&str], out: &mut Vec<CompileError>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Expr(e) | StmtKind::Assign(_, e) | StmtKind::AnnAssign { value: e, .. } => {
                walk_expr(e, known, out)
            }
            StmtKind::Return(Some(e)) => walk_expr(e, known, out),
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                walk_expr(cond, known, out);
                walk_stmts(body, known, out);
                for (c, b) in elifs {
                    walk_expr(c, known, out);
                    walk_stmts(b, known, out);
                }
                if let Some(b) = else_body {
                    walk_stmts(b, known, out);
                }
            }
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                walk_expr(start, known, out);
                walk_expr(end, known, out);
                walk_expr(step, known, out);
                walk_stmts(body, known, out);
            }
            StmtKind::ForEach { iterable, body, .. } => {
                walk_expr(iterable, known, out);
                walk_stmts(body, known, out);
            }
            StmtKind::While { cond, body } => {
                walk_expr(cond, known, out);
                walk_stmts(body, known, out);
            }
            StmtKind::Def { defaults, body, .. } => {
                for d in defaults {
                    walk_expr(d, known, out);
                }
                walk_stmts(body, known, out);
            }
            StmtKind::ClassDef {
                methods,
                class_vars,
                ..
            } => {
                for (_, e) in class_vars {
                    walk_expr(e, known, out);
                }
                for m in methods {
                    walk_stmts(&m.body, known, out);
                }
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                walk_expr(target, known, out);
                walk_expr(index, known, out);
                walk_expr(value, known, out);
            }
            StmtKind::SetAttr { obj, value, .. } => {
                walk_expr(obj, known, out);
                walk_expr(value, known, out);
            }
            StmtKind::UnpackAssign { targets, value } => {
                for t in targets {
                    walk_expr(t, known, out);
                }
                walk_expr(value, known, out);
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Import(_) => {
            }
        }
    }
}

fn walk_expr(e: &Expr, known: &[&str], out: &mut Vec<CompileError>) {
    match &e.kind {
        ExprKind::Call(name, args) => {
            if !known.contains(&name.as_str())
                && let Some(sugg) = did_you_mean(name, known)
            {
                let message = format!("`{name}` isn't defined — did you mean `{sugg}`?");
                // The parser records the callee name's span on Call nodes, so the
                // editor can squiggle exactly the misspelled name. `(0, 0)` means
                // unset (e.g. a desugared call) — fall back to line-only.
                out.push(match e.span {
                    (0, 0) => CompileError::at(e.line, message),
                    span => CompileError::at_span(e.line, span, message),
                });
            }
            for a in args {
                walk_expr(a, known, out);
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            walk_expr(recv, known, out);
            for a in args {
                walk_expr(a, known, out);
            }
        }
        ExprKind::Unary(_, x) | ExprKind::Attr(x, _) | ExprKind::Kwarg(_, x) => {
            walk_expr(x, known, out)
        }
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            walk_expr(a, known, out);
            walk_expr(b, known, out);
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) => {
            for x in xs {
                walk_expr(x, known, out);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                walk_expr(k, known, out);
                walk_expr(v, known, out);
            }
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            walk_expr(obj, known, out);
            for o in [start, stop, step].into_iter().flatten() {
                walk_expr(o, known, out);
            }
        }
        ExprKind::ListComp { element, clauses } => {
            walk_expr(element, known, out);
            walk_clauses(clauses, known, out);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            walk_expr(key, known, out);
            walk_expr(value, known, out);
            walk_clauses(clauses, known, out);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_)
        | ExprKind::Name(_) => {}
    }
}

fn walk_clauses(clauses: &[CompClause], known: &[&str], out: &mut Vec<CompileError>) {
    for c in clauses {
        match c {
            CompClause::For { iter, .. } => walk_expr(iter, known, out),
            CompClause::If(e) => walk_expr(e, known, out),
        }
    }
}

// --- cycle-freedom analysis ------------------------------------------------
//
// Plain reference counting frees everything *except* reference cycles. A cycle
// needs an already-built heap container to be mutated so it (transitively) holds
// a reference reaching back to itself — pure construction (literals,
// comprehensions) can't do it, and storing a scalar (number/bool/string, none of
// which hold references) can't either. So we soundly over-approximate: the
// program "may form a cycle" if it ever mutates a container (`append`/`insert`/
// `extend`, subscript-set, attribute-set) with a value that isn't provably
// cycle-safe. A `false` result is a guarantee — plain RC is leak-complete — and
// is the seam for a `--no-mutation` fast path / cycle collector (see
// MEMORY_MANAGEMENT.md).

/// Whether the program could create a reference cycle (and thus leak under RC).
/// Sound and conservative; `false` means provably cycle-free.
pub fn may_form_cycle(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_may_cycle)
}

/// A value that can't (transitively) hold a reference: numbers, bools, strings,
/// None, and arithmetic over them. A `Name` is unknown (could be a container), so
/// it's conservatively unsafe.
fn expr_is_cycle_safe(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::NoneLit => true,
        ExprKind::Unary(_, x) => expr_is_cycle_safe(x),
        ExprKind::Bin(_, a, b) => expr_is_cycle_safe(a) && expr_is_cycle_safe(b),
        _ => false,
    }
}

fn stmt_may_cycle(s: &Stmt) -> bool {
    let block = |b: &[Stmt]| b.iter().any(stmt_may_cycle);
    match &s.kind {
        StmtKind::Expr(e) => expr_may_cycle(e),
        StmtKind::Assign(_, e)
        | StmtKind::AnnAssign { value: e, .. }
        | StmtKind::Return(Some(e)) => expr_may_cycle(e),
        // Storing a non-scalar into a container/attribute is the cycle source.
        StmtKind::SetIndex { value, .. } | StmtKind::SetAttr { value, .. } => {
            !expr_is_cycle_safe(value)
        }
        StmtKind::If {
            cond,
            body,
            elifs,
            else_body,
        } => {
            expr_may_cycle(cond)
                || block(body)
                || elifs.iter().any(|(c, b)| expr_may_cycle(c) || block(b))
                || else_body.as_deref().is_some_and(block)
        }
        StmtKind::While { cond, body } => expr_may_cycle(cond) || block(body),
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::Def { body, .. } => block(body),
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => false,
        // ClassDef (mutable attributes), tuple-unpacking, and anything else: be
        // conservative and assume a cycle is possible.
        _ => true,
    }
}

fn expr_may_cycle(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::MethodCall(obj, m, args) => {
            (matches!(m.as_str(), "append" | "insert" | "extend")
                && args.iter().any(|a| !expr_is_cycle_safe(a)))
                || expr_may_cycle(obj)
                || args.iter().any(expr_may_cycle)
        }
        ExprKind::Bin(_, a, b) => expr_may_cycle(a) || expr_may_cycle(b),
        ExprKind::Unary(_, x) => expr_may_cycle(x),
        ExprKind::Index(o, i) => expr_may_cycle(o) || expr_may_cycle(i),
        ExprKind::Call(_, args) => args.iter().any(expr_may_cycle),
        ExprKind::List(items) => items.iter().any(expr_may_cycle),
        ExprKind::Dict(pairs) => pairs
            .iter()
            .any(|(k, v)| expr_may_cycle(k) || expr_may_cycle(v)),
        ExprKind::ListComp { element, clauses } => {
            expr_may_cycle(element) || clauses_may_cycle(clauses)
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => expr_may_cycle(key) || expr_may_cycle(value) || clauses_may_cycle(clauses),
        _ => false,
    }
}

fn clauses_may_cycle(clauses: &[CompClause]) -> bool {
    clauses.iter().any(|c| match c {
        CompClause::For { iter, .. } => expr_may_cycle(iter),
        CompClause::If(e) => expr_may_cycle(e),
    })
}

// --- set-type inference (for IDE glyph display) ----------------------------
//
// `&`/`|`/`-`/`^` are *also* the int bitwise / subtraction operators, so the IDE
// may only render them as set-theory glyphs (∩ ∪ ∖ ∆) when both operands are
// sets. Deciding that needs to know which *names* hold sets — which this infers.
// Flow-insensitive: a name assigned a set anywhere counts as a set everywhere.
// That's deliberately loose (a display hint, never a correctness gate) and keeps
// the analysis a simple fixed point.

/// Names that are bound to a set somewhere in the program. See
/// `acornstem-ide/SET_NOTATION_SPEC.md` (Part 2).
pub fn set_typed_names(stmts: &[Stmt]) -> Vec<String> {
    let mut out: Vec<String> = infer_set_names(stmts).into_iter().collect();
    out.sort();
    out
}

/// The fixed-point set-name inference shared by `set_typed_names` and
/// `set_operator_spans`. `c = a & b` only resolves once a and b are known, so we
/// iterate until nothing new is learned (bounded by the number of names).
fn infer_set_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut assigns: Vec<(String, &Expr)> = Vec::new();
    let mut sets: HashSet<String> = HashSet::new();
    collect_assigns(stmts, &mut assigns, &mut sets);
    loop {
        let mut changed = false;
        for (name, rhs) in &assigns {
            if !sets.contains(name) && is_set_expr(rhs, &sets) {
                sets.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    sets
}

/// Byte spans of the `& | - ^` operators that are *set* operations (both
/// operands are sets), so the IDE can render exactly those as set-theory glyphs
/// (∩ ∪ ∖ ∆) — leaving int bitwise / subtraction alone. Because this works on
/// the real AST, it is precedence- and parenthesis-correct: `(A | B) & C`, a
/// `set(...)` result, or nested set ops all classify properly. See
/// `acornstem-ide/SET_NOTATION_SPEC.md` (Part 2). Spans are `[start, end)` byte
/// offsets into the source.
pub fn set_operator_spans(stmts: &[Stmt]) -> Vec<crate::ast::Span> {
    let sets = infer_set_names(stmts);
    let mut out: Vec<crate::ast::Span> = Vec::new();
    spans_in_stmts(stmts, &sets, &mut out);
    out.sort_unstable(); // source order (tree walk visits outer-then-inner)
    out
}

fn spans_in_stmts(stmts: &[Stmt], sets: &HashSet<String>, out: &mut Vec<crate::ast::Span>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Expr(e)
            | StmtKind::Assign(_, e)
            | StmtKind::AnnAssign { value: e, .. }
            | StmtKind::Return(Some(e)) => spans_in_expr(e, sets, out),
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                spans_in_expr(cond, sets, out);
                spans_in_stmts(body, sets, out);
                for (c, b) in elifs {
                    spans_in_expr(c, sets, out);
                    spans_in_stmts(b, sets, out);
                }
                if let Some(b) = else_body {
                    spans_in_stmts(b, sets, out);
                }
            }
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                spans_in_expr(start, sets, out);
                spans_in_expr(end, sets, out);
                spans_in_expr(step, sets, out);
                spans_in_stmts(body, sets, out);
            }
            StmtKind::ForEach { iterable, body, .. } => {
                spans_in_expr(iterable, sets, out);
                spans_in_stmts(body, sets, out);
            }
            StmtKind::While { cond, body } => {
                spans_in_expr(cond, sets, out);
                spans_in_stmts(body, sets, out);
            }
            StmtKind::Def { defaults, body, .. } => {
                for d in defaults {
                    spans_in_expr(d, sets, out);
                }
                spans_in_stmts(body, sets, out);
            }
            StmtKind::ClassDef {
                methods,
                class_vars,
                ..
            } => {
                for (_, e) in class_vars {
                    spans_in_expr(e, sets, out);
                }
                for m in methods {
                    spans_in_stmts(&m.body, sets, out);
                }
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                spans_in_expr(target, sets, out);
                spans_in_expr(index, sets, out);
                spans_in_expr(value, sets, out);
            }
            StmtKind::SetAttr { obj, value, .. } => {
                spans_in_expr(obj, sets, out);
                spans_in_expr(value, sets, out);
            }
            StmtKind::UnpackAssign { targets, value } => {
                for t in targets {
                    spans_in_expr(t, sets, out);
                }
                spans_in_expr(value, sets, out);
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Import(_) => {
            }
        }
    }
}

fn spans_in_expr(e: &Expr, sets: &HashSet<String>, out: &mut Vec<crate::ast::Span>) {
    if let ExprKind::Bin(op, a, b) = &e.kind
        && matches!(
            op,
            BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor | BinOp::Sub
        )
        && is_set_expr(a, sets)
        && is_set_expr(b, sets)
    {
        out.push(e.span);
    }
    // Recurse into every child expression.
    match &e.kind {
        ExprKind::Unary(_, x) | ExprKind::Attr(x, _) | ExprKind::Kwarg(_, x) => {
            spans_in_expr(x, sets, out)
        }
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            spans_in_expr(a, sets, out);
            spans_in_expr(b, sets, out);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                spans_in_expr(a, sets, out);
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            spans_in_expr(recv, sets, out);
            for a in args {
                spans_in_expr(a, sets, out);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) => {
            for x in xs {
                spans_in_expr(x, sets, out);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                spans_in_expr(k, sets, out);
                spans_in_expr(v, sets, out);
            }
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            spans_in_expr(obj, sets, out);
            for o in [start, stop, step].into_iter().flatten() {
                spans_in_expr(o, sets, out);
            }
        }
        ExprKind::ListComp { element, clauses } => {
            spans_in_expr(element, sets, out);
            spans_in_clauses(clauses, sets, out);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            spans_in_expr(key, sets, out);
            spans_in_expr(value, sets, out);
            spans_in_clauses(clauses, sets, out);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_)
        | ExprKind::Name(_) => {}
    }
}

fn spans_in_clauses(
    clauses: &[CompClause],
    sets: &HashSet<String>,
    out: &mut Vec<crate::ast::Span>,
) {
    for c in clauses {
        match c {
            CompClause::For { iter, .. } => spans_in_expr(iter, sets, out),
            CompClause::If(e) => spans_in_expr(e, sets, out),
        }
    }
}

/// Gather every `name = rhs` assignment (for the fixed point) and seed `sets`
/// with any `name: set = …` annotation (a declared set, regardless of the rhs).
fn collect_assigns<'a>(
    stmts: &'a [Stmt],
    assigns: &mut Vec<(String, &'a Expr)>,
    sets: &mut HashSet<String>,
) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, value) => assigns.push((name.clone(), value)),
            StmtKind::AnnAssign { name, ann, value } => {
                if matches!(&ann.kind, ExprKind::Name(n) if n == "set") {
                    sets.insert(name.clone());
                }
                assigns.push((name.clone(), value));
            }
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                collect_assigns(body, assigns, sets);
                for (_, b) in elifs {
                    collect_assigns(b, assigns, sets);
                }
                if let Some(b) = else_body {
                    collect_assigns(b, assigns, sets);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. }
            | StmtKind::Def { body, .. } => collect_assigns(body, assigns, sets),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    collect_assigns(&m.body, assigns, sets);
                }
            }
            _ => {}
        }
    }
}

/// Whether `e` evaluates to a set, given the set-typed names known so far. Set
/// literals and comprehensions desugar to `set(...)` in the parser, so they're
/// covered by the `Call("set", …)` arm.
fn is_set_expr(e: &Expr, sets: &HashSet<String>) -> bool {
    match &e.kind {
        ExprKind::Call(name, _) => name == "set" || name == "frozenset",
        ExprKind::Name(n) => sets.contains(n),
        ExprKind::Bin(BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor | BinOp::Sub, a, b) => {
            is_set_expr(a, sets) && is_set_expr(b, sets)
        }
        ExprKind::MethodCall(recv, m, _) => {
            matches!(
                m.as_str(),
                "union" | "intersection" | "difference" | "symmetric_difference" | "copy"
            ) && is_set_expr(recv, sets)
        }
        _ => false,
    }
}

/// Undefined-variable warnings: a bare-name READ of a variable that is bound
/// NOWHERE in scope — the complement of the "did you mean…?" *call-name* lint
/// (`typo_diagnostics`), covering value names (a typo'd or never-set variable).
/// Each entry is `(line, message)`, kid-facing, with a "did you mean…?" when
/// the name near-misses one that IS in scope.
///
/// Sound-as-a-lint by **over-approximating "bound"**: a name counts as defined
/// if it is a builtin, a `def`/`class` name, an `import`, a parameter, `self`,
/// or assigned/bound ANYWHERE in the same-or-enclosing scope — including loop
/// vars, unpack targets, and comprehension targets (matching Python's
/// "assigned anywhere → local" rule). So only a name bound *nowhere* is
/// flagged: always a real error, never a false alarm. Deliberately NOT
/// flow-sensitive — a forward / use-before-assignment (`print(x)` then
/// `x = 1`) is not flagged (that harder check invites false positives); this
/// catches the common "never defined at all" case. Type/annotation positions
/// (`x: int`, `-> bool`) are skipped — they name types, not variables.
pub fn undefined_name_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let builtins: HashSet<String> = crate::builtins::names().map(String::from).collect();
    let mut out = Vec::new();
    check_scope_names(stmts, &builtins, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

/// Names BOUND at this scope level — through control flow, but NOT into nested
/// `def`/`class` bodies (separate scopes).
fn scope_bindings(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, _) | StmtKind::AnnAssign { name, .. } => {
                out.insert(name.clone());
            }
            StmtKind::For { var, body, .. } | StmtKind::ForEach { var, body, .. } => {
                out.insert(var.clone());
                scope_bindings(body, out);
            }
            StmtKind::While { body, .. } => scope_bindings(body, out),
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                scope_bindings(body, out);
                for (_, b) in elifs {
                    scope_bindings(b, out);
                }
                if let Some(b) = else_body {
                    scope_bindings(b, out);
                }
            }
            StmtKind::UnpackAssign { targets, .. } => {
                for t in targets {
                    if let ExprKind::Name(n) = &t.kind {
                        out.insert(n.clone());
                    }
                }
            }
            StmtKind::Def { name, .. } | StmtKind::ClassDef { name, .. } => {
                out.insert(name.clone());
            }
            StmtKind::Import(names) => {
                for n in names {
                    out.insert(n.clone());
                }
            }
            _ => {}
        }
    }
}

/// Check one scope's reads against `outer` (enclosing-visible names + builtins)
/// plus this scope's own bindings, then recurse into nested `def`/`class`
/// scopes (each seeing the accumulated outer names — over-approximate, so a
/// closure-style read never false-flags even though we don't model closures).
fn check_scope_names(stmts: &[Stmt], outer: &HashSet<String>, out: &mut Vec<(usize, String)>) {
    let mut allowed = outer.clone();
    scope_bindings(stmts, &mut allowed);
    check_reads(stmts, &allowed, out);
    for s in stmts {
        match &s.kind {
            StmtKind::Def { params, body, .. } => {
                let mut inner = allowed.clone();
                inner.extend(params.iter().cloned());
                check_scope_names(body, &inner, out);
            }
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    let mut inner = allowed.clone();
                    inner.extend(m.params.iter().cloned());
                    check_scope_names(&m.body, &inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Walk one scope's *value-read* positions (through control flow, but not into
/// nested def/class bodies — those recurse as their own scope). Annotation and
/// default-argument positions belonging to nested scopes are skipped;
/// class-var and default expressions evaluate in THIS scope and are checked.
fn check_reads(stmts: &[Stmt], allowed: &HashSet<String>, out: &mut Vec<(usize, String)>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(_, value) | StmtKind::AnnAssign { value, .. } => {
                read_expr(value, allowed, out)
            }
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                read_expr(start, allowed, out);
                read_expr(end, allowed, out);
                read_expr(step, allowed, out);
                check_reads(body, allowed, out);
            }
            StmtKind::ForEach { iterable, body, .. } => {
                read_expr(iterable, allowed, out);
                check_reads(body, allowed, out);
            }
            StmtKind::While { cond, body } => {
                read_expr(cond, allowed, out);
                check_reads(body, allowed, out);
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                read_expr(cond, allowed, out);
                check_reads(body, allowed, out);
                for (c, b) in elifs {
                    read_expr(c, allowed, out);
                    check_reads(b, allowed, out);
                }
                if let Some(b) = else_body {
                    check_reads(b, allowed, out);
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Expr(e) => read_expr(e, allowed, out),
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                read_expr(target, allowed, out);
                read_expr(index, allowed, out);
                read_expr(value, allowed, out);
            }
            StmtKind::SetAttr { obj, value, .. } => {
                read_expr(obj, allowed, out);
                read_expr(value, allowed, out);
            }
            StmtKind::UnpackAssign { targets, value } => {
                read_expr(value, allowed, out);
                // A Name target BINDS; an Index/Attr target READS its container.
                for t in targets {
                    if !matches!(t.kind, ExprKind::Name(_)) {
                        read_expr(t, allowed, out);
                    }
                }
            }
            // Class-var values + default-arg exprs evaluate in THIS scope.
            StmtKind::ClassDef { class_vars, .. } => {
                for (_, e) in class_vars {
                    read_expr(e, allowed, out);
                }
            }
            StmtKind::Def { defaults, .. } => {
                for e in defaults {
                    read_expr(e, allowed, out);
                }
            }
            _ => {}
        }
    }
}

/// Check the bare-name reads inside one expression against `allowed`.
fn read_expr(e: &Expr, allowed: &HashSet<String>, out: &mut Vec<(usize, String)>) {
    match &e.kind {
        ExprKind::Name(n) => {
            if !allowed.contains(n) {
                let cands: Vec<&str> = allowed.iter().map(String::as_str).collect();
                let msg = match did_you_mean(n, &cands) {
                    Some(sugg) => format!("`{n}` isn't defined — did you mean `{sugg}`?"),
                    None => format!(
                        "`{n}` isn't defined yet — set it first (e.g. `{n} = ...`) or check the spelling"
                    ),
                };
                out.push((e.line, msg));
            }
        }
        ExprKind::Unary(_, x) => read_expr(x, allowed, out),
        ExprKind::Bin(_, a, b) => {
            read_expr(a, allowed, out);
            read_expr(b, allowed, out);
        }
        // The callee NAME is owned by the call-name typo lint; only args here.
        ExprKind::Call(_, args) => {
            for a in args {
                read_expr(a, allowed, out);
            }
        }
        ExprKind::Kwarg(_, v) => read_expr(v, allowed, out),
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            for it in items {
                read_expr(it, allowed, out);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                read_expr(k, allowed, out);
                read_expr(v, allowed, out);
            }
        }
        ExprKind::Index(o, i) => {
            read_expr(o, allowed, out);
            read_expr(i, allowed, out);
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            read_expr(obj, allowed, out);
            for b in [start, stop, step].into_iter().flatten() {
                read_expr(b, allowed, out);
            }
        }
        ExprKind::MethodCall(obj, _, args) => {
            read_expr(obj, allowed, out);
            for a in args {
                read_expr(a, allowed, out);
            }
        }
        ExprKind::Attr(obj, _) => read_expr(obj, allowed, out),
        ExprKind::ListComp { element, clauses } => {
            let inner = comp_allowed(allowed, clauses);
            read_expr(element, &inner, out);
            read_comp_clauses(clauses, &inner, out);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            let inner = comp_allowed(allowed, clauses);
            read_expr(key, &inner, out);
            read_expr(value, &inner, out);
            read_comp_clauses(clauses, &inner, out);
        }
        // Literals (Int/Float/Bool/Str/None) — nothing to read.
        _ => {}
    }
}

/// `allowed` plus every comprehension `for` target (over-approximate: even the
/// first clause's iter may reference a comp var, which only avoids false
/// positives).
fn comp_allowed(allowed: &HashSet<String>, clauses: &[CompClause]) -> HashSet<String> {
    let mut inner = allowed.clone();
    for c in clauses {
        if let CompClause::For { vars, .. } = c {
            inner.extend(vars.iter().cloned());
        }
    }
    inner
}

fn read_comp_clauses(
    clauses: &[CompClause],
    allowed: &HashSet<String>,
    out: &mut Vec<(usize, String)>,
) {
    for c in clauses {
        match c {
            CompClause::For { iter, .. } => read_expr(iter, allowed, out),
            CompClause::If(cond) => read_expr(cond, allowed, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(src: &str) -> Vec<String> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        typo_diagnostics(&stmts)
            .iter()
            .map(|e| e.to_string())
            .collect()
    }

    fn cycles(src: &str) -> bool {
        let toks = crate::lexer::lex(src).unwrap();
        may_form_cycle(&crate::parser::parse(&toks).unwrap())
    }

    #[test]
    fn cycle_free_programs_are_recognized() {
        // Construction + scalar mutation + comprehensions can't cycle.
        assert!(!cycles(
            "xs = [1, 2, 3]\nxs.append(4)\nys = [x * x for x in xs]\n"
        ));
        assert!(!cycles("xs = [1, 2]\nxs[0] = 99\n"));
        assert!(!cycles("d = {n: n * n for n in range(3)}\n"));
    }

    #[test]
    fn container_holding_a_reference_may_cycle() {
        // Appending/storing a (possibly heap) value is the cycle source.
        assert!(cycles("a = []\nb = a\na.append(b)\n")); // self-reference
        assert!(cycles("a = []\nd = {}\nd[0] = a\n")); // dict holds a list
        assert!(cycles("xs = []\nys = [1]\nxs.append(ys)\n")); // list of lists
    }

    #[test]
    fn suggests_print_for_pint() {
        let d = diags("pint(\"hi\")\n");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("did you mean `print`"), "{}", d[0]);
        assert!(d[0].contains("line 1"));
    }

    #[test]
    fn typo_diagnostic_spans_the_misspelled_name() {
        let src = "x = 1\nresult = pint(x)\n";
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        let d = typo_diagnostics(&stmts);
        assert_eq!(d.len(), 1, "{d:?}");
        let (s, e) = d[0].span.expect("typo should carry the name's span");
        assert_eq!(&src[s..e], "pint"); // squiggles exactly the bad name
    }

    #[test]
    fn nested_calls_are_checked() {
        let d = diags("for i in range(3):\n    pint(i)\n");
        assert!(
            d.iter().any(|m| m.contains("did you mean `print`")),
            "{d:?}"
        );
    }

    #[test]
    fn user_defined_functions_are_not_flagged() {
        // `greet` is defined, so calling it is fine; `print` is a real builtin.
        let d = diags("def greet():\n    print(\"hi\")\n\ngreet()\n");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn unknown_but_not_a_near_miss_is_left_alone() {
        // Not close to any known name — probably a function they'll define.
        let d = diags("frobnicate(3)\n");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn correct_builtins_are_not_flagged() {
        let d = diags("print(len(range(3)))\n");
        assert!(d.is_empty(), "{d:?}");
    }

    fn sets(src: &str) -> Vec<String> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        set_typed_names(&stmts)
    }

    #[test]
    fn set_literals_and_calls_are_sets() {
        assert_eq!(sets("a = {1, 2, 3}\n"), vec!["a"]);
        assert_eq!(sets("a = set([1, 2])\n"), vec!["a"]);
        assert_eq!(sets("a = set()\n"), vec!["a"]);
        // Set comprehension desugars to set(<listcomp>).
        assert_eq!(sets("a = {x for x in range(3)}\n"), vec!["a"]);
    }

    #[test]
    fn set_operators_propagate_through_the_fixed_point() {
        // c depends on a and b, which are sets — the fixed point must catch it
        // regardless of how the assignment chain is ordered.
        assert_eq!(
            sets("a = {1, 2}\nb = {2, 3}\nc = a & b\nd = c | a\n"),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn non_sets_are_not_flagged() {
        // Bitwise/arith over ints, lists, and dicts are NOT sets.
        let s = sets("flags = 6 & 3\nn = 10 - 1\nxs = [1, 2]\nd = {1: 2}\n");
        assert!(s.is_empty(), "{s:?}");
    }

    #[test]
    fn set_methods_and_annotation_count() {
        assert_eq!(
            sets("a = {1}\nb = a.union({2})\n"),
            vec!["a", "b"],
            "method result is a set"
        );
        // A `: set` annotation marks it even if the rhs is opaque (a param, say).
        assert_eq!(sets("s: set = make_it()\n"), vec!["s"]);
    }

    /// The operator chars at the returned spans, for readable assertions.
    fn op_chars(src: &str) -> Vec<char> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        set_operator_spans(&stmts)
            .into_iter()
            .map(|(s, _)| src[s..].chars().next().unwrap())
            .collect()
    }

    fn spans(src: &str) -> Vec<(usize, usize)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        set_operator_spans(&stmts)
    }

    #[test]
    fn operator_spans_point_at_set_operators_only() {
        // a=10, b={1,2}, c={3}; only the `&` is a set op (a&b would be int — but
        // a is not a set, so excluded). Here b & c both sets → the `&` spans.
        let src = "b = {1, 2}\nc = {3}\nx = b & c\n";
        assert_eq!(op_chars(src), vec!['&']);
        // The span actually lands on the `&` character.
        let (s, e) = spans(src)[0];
        assert_eq!(&src[s..e], "&");
    }

    #[test]
    fn operator_spans_are_precedence_and_paren_correct() {
        // (A | B) & C with all sets: both operators are set ops.
        let src = "A = {1}\nB = {2}\nC = {3}\nx = (A | B) & C\n";
        assert_eq!(op_chars(src), vec!['|', '&']);
        // Set literal operand and set(...) result, no set-typed names needed.
        assert_eq!(op_chars("x = {1, 2} - set([2])\n"), vec!['-']);
    }

    #[test]
    fn non_set_operators_have_no_spans() {
        // Int bitwise / subtraction, and a set mixed with a non-set, are excluded.
        assert!(op_chars("x = 6 & 3\n").is_empty());
        assert!(op_chars("n = 10 - 1\n").is_empty());
        assert!(op_chars("s = {1}\nx = s - 1\n").is_empty());
    }

    fn churn(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        type_churn_warnings(&stmts)
    }

    #[test]
    fn churn_flags_cross_category_reuse() {
        // number -> text: the classic confusing reuse.
        let w = churn("x = 1\nprint(x)\nx = \"hi\"\nprint(x)\n");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 3); // the line of the conflicting assignment
        assert!(w[0].1.contains("'x'") && w[0].1.contains("number") && w[0].1.contains("string"));
        // The canonical age pattern (text -> number via int()) surfaces — this
        // is the data the reject-vs-demote decision wants.
        assert_eq!(churn("age = input()\nage = int(age)\n").len(), 1);
        // list -> number is churn too.
        assert_eq!(churn("xs = [1, 2]\nxs = 5\n").len(), 1);
    }

    #[test]
    fn churn_does_not_cry_wolf() {
        // Numeric progression (int -> float) is NOT churn — both are numbers.
        assert!(churn("avg = 0\navg = total / n\n").is_empty());
        // Same-category reassignment (accumulator) — never fires.
        assert!(churn("total = 0\ntotal = total + 5\ntotal = total * 2\n").is_empty());
        // A dynamic source (unknown category) never triggers a warning.
        assert!(churn("x = something()\nx = other()\n").is_empty());
        assert!(churn("x = 1\nx = f(x)\n").is_empty());
        // One warning per name, not one per later assignment.
        assert_eq!(churn("x = 1\nx = \"a\"\nx = \"b\"\nx = \"c\"\n").len(), 1);
    }

    fn undef(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        undefined_name_warnings(&stmts)
    }

    #[test]
    fn undef_flags_never_bound_names() {
        // A read of a name set nowhere.
        let w = undef("print(total)\n");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 1);
        assert!(w[0].1.contains("`total`"));
        // A near-miss of an in-scope name suggests it.
        let w2 = undef("score = 10\nprint(scoer)\n");
        assert_eq!(w2.len(), 1);
        assert!(w2[0].1.contains("did you mean `score`"), "{}", w2[0].1);
        // Undefined inside a function body.
        assert_eq!(undef("def f(a):\n    return a + b\n").len(), 1);
    }

    #[test]
    fn undef_does_not_cry_wolf() {
        // Every legit binding form marks a name defined:
        assert!(undef("x = 1\nprint(x)\n").is_empty()); // assign
        assert!(undef("x: int = 1\nprint(x)\n").is_empty()); // annassign
        assert!(undef("for i in range(3):\n    print(i)\n").is_empty()); // loop var
        assert!(undef("for w in [1, 2]:\n    print(w)\n").is_empty()); // foreach var
        assert!(undef("a, b = 1, 2\nprint(a + b)\n").is_empty()); // unpack targets
        assert!(undef("def g(n):\n    return n * 2\nprint(g(3))\n").is_empty()); // param + def name
        assert!(undef("import math\nprint(math.pi)\n").is_empty()); // import name
        assert!(undef("ys = [x * 2 for x in [1, 2]]\nprint(ys)\n").is_empty()); // comp var
        assert!(undef("print(len([1, 2]))\n").is_empty()); // builtin call
        // Reassignment reading itself is fine (bound in scope).
        assert!(undef("t = 0\nt = t + 1\nprint(t)\n").is_empty());
        // A forward reference is deliberately NOT flagged (over-approximate).
        assert!(undef("print(y)\ny = 5\n").is_empty());
        // A function reads a module global.
        assert!(undef("g = 10\ndef f():\n    return g\n").is_empty());
        // self + attributes inside a method.
        assert!(
            undef("class C:\n    def __init__(self):\n        self.n = 0\n    def get(self):\n        return self.n\n")
                .is_empty()
        );
        // Handler-as-value: a def name passed as an argument.
        assert!(undef("def boom():\n    return 0\non_click(boom)\n").is_empty());
    }

    #[test]
    fn churn_is_scoped_per_function() {
        // `n` as a number in one function and text in another is NOT churn.
        let src = "def a(x):\n    n = 1\n    return n\ndef b(y):\n    n = \"hi\"\n    return n\n";
        assert!(churn(src).is_empty());
        // But churn INSIDE a single function is caught.
        let inside = "def f():\n    v = 1\n    v = \"x\"\n    return v\n";
        assert_eq!(churn(inside).len(), 1);
        // Reassignment inside a branch counts (same scope).
        let branch = "x = 1\nif x > 0:\n    x = \"pos\"\n";
        assert_eq!(churn(branch).len(), 1);
    }
}
