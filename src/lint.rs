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
//! - `unused_assignment_warnings` — a function-local set but never read ("you
//!   set `result` but never used it"), scoped to the mainstream-safe subset;
//! - `mutable_default_warnings` — a list/dict/set as a function default arg (the
//!   shared-and-mutated footgun), suggesting the `=None` fix;
//! - `unreachable_code_warnings` — a statement that can never run after a
//!   `return`/`break`/`continue` in the same block;
//! - `shadowed_builtin_warnings` — a variable named after a built-in type/
//!   function (`list = …`), curated to the never-a-good-variable-name set;
//! - `self_comparison_warnings` — comparing/assigning something to itself
//!   (`x == x`, `x = x`), pure operands only;
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
    // Nested scopes are independent: descend only NEW-scope child blocks with
    // a fresh `seen` (same-scope blocks were handled inside churn_walk).
    for s in stmts {
        for_each_child_block(s, |b, scope| {
            if scope == BlockScope::New {
                churn_scope(b, out);
            }
        });
    }
}

/// Walk the assignments of one scope: same-scope child blocks (if/for/while
/// bodies rebind names in THIS scope) via the shared walker; `def`/`class`
/// bodies are separate scopes, handled by [`churn_scope`].
fn churn_walk(
    stmts: &[Stmt],
    seen: &mut HashMap<String, (LintTy, bool)>,
    out: &mut Vec<(usize, String)>,
) {
    for s in stmts {
        if let StmtKind::Assign(name, value) | StmtKind::AnnAssign { name, value, .. } = &s.kind
            && let Some(ty) = lint_ty(value)
        {
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
        for_each_child_block(s, |b, scope| {
            if scope == BlockScope::Same {
                churn_walk(b, seen, out);
            }
        });
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
            StmtKind::Def { name, .. } | StmtKind::ClassDef { name, .. } => {
                defs.push(name.clone());
            }
            _ => {}
        }
        for_each_child_block(s, |b, _| collect_defs(b, defs));
    }
}

fn walk_stmts(stmts: &[Stmt], known: &[&str], out: &mut Vec<CompileError>) {
    for s in stmts {
        stmt_exprs(s, &mut |e| walk_expr(e, known, out));
        for_each_child_block(s, |b, _| walk_stmts(b, known, out));
    }
}

fn walk_expr(e: &Expr, known: &[&str], out: &mut Vec<CompileError>) {
    if let ExprKind::Call(name, _) = &e.kind
        && !known.contains(&name.as_str())
        && let Some(sugg) = did_you_mean(name, known)
    {
        let message = format!("`{name}` isn't defined — did you mean `{sugg}`?");
        // The parser records the callee name's span on Call nodes, so the
        // editor can squiggle exactly the misspelled name. `(0, 0)` means
        // unset (e.g. a desugared call) — fall back to line-only.
        out.push(
            match e.span {
                (0, 0) => CompileError::at(e.line, message),
                span => CompileError::at_span(e.line, span, message),
            }
            .with_kind(crate::error::ErrorKind::Name),
        );
    }
    each_child_expr(e, &mut |c| walk_expr(c, known, out));
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

/// Names that hold strings, by the same fixed-point scheme as
/// [`infer_set_names`]: seeded from string literals and string-returning
/// builtins, then propagated through assignment and `+` (concatenation) until
/// nothing new is learned. Deliberately an UNDER-approximation — a miss only
/// means the blocks decompiler renders a `+` as the numeric block instead of
/// "join text", never wrong code.
pub(crate) fn str_typed_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut assigns: Vec<(String, &Expr)> = Vec::new();
    let mut seen = HashSet::new(); // unused set-seed slot; reuse the collector
    collect_assigns(stmts, &mut assigns, &mut seen);
    let mut strs: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for (name, rhs) in &assigns {
            if !strs.contains(name) && is_str_expr(rhs, &strs) {
                strs.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    strs
}

/// Whether `e` is string-valued, given the names already known to be strings.
/// Conservative: only shapes that are certainly strings count.
pub(crate) fn is_str_expr(e: &Expr, strs: &HashSet<String>) -> bool {
    match &e.kind {
        ExprKind::Str(_) => true,
        ExprKind::Name(n) => strs.contains(n),
        // Concatenation: `+` with a known-string side is string-valued (with a
        // non-string other side it would be a runtime error, not a number).
        ExprKind::Bin(BinOp::Add, a, b) => is_str_expr(a, strs) || is_str_expr(b, strs),
        // Builtins that return strings.
        ExprKind::Call(name, _) => {
            matches!(
                name.as_str(),
                "str" | "repr" | "input" | "chr" | "get_value" | "get_field"
            )
        }
        // String methods that return strings, on a string receiver.
        ExprKind::MethodCall(obj, method, _) => {
            is_str_expr(obj, strs)
                && matches!(
                    method.as_str(),
                    "upper"
                        | "lower"
                        | "strip"
                        | "lstrip"
                        | "rstrip"
                        | "replace"
                        | "title"
                        | "capitalize"
                        | "swapcase"
                        | "zfill"
                        | "center"
                        | "ljust"
                        | "rjust"
                        | "format"
                        | "join"
                )
        }
        _ => false,
    }
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
        stmt_exprs(s, &mut |e| spans_in_expr(e, sets, out));
        for_each_child_block(s, |b, _| spans_in_stmts(b, sets, out));
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
            _ => {}
        }
        for_each_child_block(s, |b, _| collect_assigns(b, assigns, sets));
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

/// Unused-local warnings: a variable assigned inside a function/method but
/// never read anywhere in that function — the "you set `result` but never
/// used it" smell (usually a forgotten `return`/`print`, or a leftover line).
/// `(line, message)`, kid-facing.
///
/// Scoped to the **unimpeachably safe** subset every mainstream linter uses:
/// - only plain `x = …` / `x: T = …` assignments (NOT loop vars, unpack
///   targets, or parameters — an unused loop counter or unpacked half is
///   idiomatic, and a param is part of a signature);
/// - **module/class level is never flagged** (top-level names are, by
///   convention, treated as usable elsewhere — and it's where an intentional
///   discard lives);
/// - "read" is **over-approximated** — reads inside nested `def`/`class` bodies
///   (closures) count, so a used name is never flagged; the worst case is a
///   *missed* warning, never a false alarm;
/// - a `_`-prefixed name is a deliberate throwaway and is skipped;
/// - a name that reads itself (`x = x + 1`) counts as read, so accumulators
///   never fire.
pub fn unused_assignment_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    // Module top level is not a flagging scope; only recurse into functions.
    for s in stmts {
        collect_unused_in_defs(s, &mut out);
    }
    out.sort_by_key(|(line, _)| *line);
    out
}

/// Find function/method scopes under `s` and analyze each for unused locals.
fn collect_unused_in_defs(s: &Stmt, out: &mut Vec<(usize, String)>) {
    match &s.kind {
        StmtKind::Def { params, body, .. } => analyze_fn_unused(body, params, out),
        StmtKind::ClassDef { methods, .. } => {
            for m in methods {
                analyze_fn_unused(&m.body, &m.params, out);
            }
        }
        // A def can be nested inside control flow at module level; keep looking.
        StmtKind::If {
            body,
            elifs,
            else_body,
            ..
        } => {
            for st in body {
                collect_unused_in_defs(st, out);
            }
            for (_, b) in elifs {
                for st in b {
                    collect_unused_in_defs(st, out);
                }
            }
            if let Some(b) = else_body {
                for st in b {
                    collect_unused_in_defs(st, out);
                }
            }
        }
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::While { body, .. } => {
            for st in body {
                collect_unused_in_defs(st, out);
            }
        }
        _ => {}
    }
}

/// Analyze one function/method body: flag its own plain assignments to names
/// never read anywhere in the body (nested scopes included). Then recurse into
/// nested functions/methods for their own analysis.
fn analyze_fn_unused(body: &[Stmt], params: &[String], out: &mut Vec<(usize, String)>) {
    let mut reads: HashSet<String> = HashSet::new();
    collect_all_reads(body, &mut reads);
    flag_unused_assigns(body, params, &reads, out);
    // Nested scopes analyze independently.
    for s in body {
        collect_unused_in_defs(s, out);
    }
}

/// Flag plain assignments in this scope's statements (through control flow, NOT
/// into nested def/class bodies) whose target is never read.
fn flag_unused_assigns(
    stmts: &[Stmt],
    params: &[String],
    reads: &HashSet<String>,
    out: &mut Vec<(usize, String)>,
) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, _) | StmtKind::AnnAssign { name, .. } => {
                if !reads.contains(name)
                    && !name.starts_with('_')
                    && !params.iter().any(|p| p == name)
                {
                    out.push((
                        s.line,
                        format!(
                            "`{name}` is set but never used — did you forget to use it (e.g. \
                             `return {name}` / `print({name})`), or is it a leftover line?"
                        ),
                    ));
                }
            }
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                flag_unused_assigns(body, params, reads, out);
                for (_, b) in elifs {
                    flag_unused_assigns(b, params, reads, out);
                }
                if let Some(b) = else_body {
                    flag_unused_assigns(b, params, reads, out);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => flag_unused_assigns(body, params, reads, out),
            _ => {}
        }
    }
}

/// Every bare-name read anywhere under `stmts`, **including nested def/class
/// bodies** (a closure reading an outer local must suppress the warning). Over-
/// approximate on purpose: a nested scope's own same-named local also counts,
/// which only ever *misses* a warning — never invents one.
fn collect_all_reads(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(_, value) | StmtKind::AnnAssign { value, .. } => reads_of(value, out),
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                reads_of(start, out);
                reads_of(end, out);
                reads_of(step, out);
                collect_all_reads(body, out);
            }
            StmtKind::ForEach { iterable, body, .. } => {
                reads_of(iterable, out);
                collect_all_reads(body, out);
            }
            StmtKind::While { cond, body } => {
                reads_of(cond, out);
                collect_all_reads(body, out);
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                reads_of(cond, out);
                collect_all_reads(body, out);
                for (c, b) in elifs {
                    reads_of(c, out);
                    collect_all_reads(b, out);
                }
                if let Some(b) = else_body {
                    collect_all_reads(b, out);
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Expr(e) => reads_of(e, out),
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                reads_of(target, out);
                reads_of(index, out);
                reads_of(value, out);
            }
            StmtKind::SetAttr { obj, value, .. } => {
                reads_of(obj, out);
                reads_of(value, out);
            }
            StmtKind::UnpackAssign { targets, value } => {
                reads_of(value, out);
                for t in targets {
                    if !matches!(t.kind, ExprKind::Name(_)) {
                        reads_of(t, out);
                    }
                }
            }
            // Descend into nested scopes so a closure read counts.
            StmtKind::Def { body, defaults, .. } => {
                for e in defaults {
                    reads_of(e, out);
                }
                collect_all_reads(body, out);
            }
            StmtKind::ClassDef {
                methods,
                class_vars,
                ..
            } => {
                for (_, e) in class_vars {
                    reads_of(e, out);
                }
                for m in methods {
                    collect_all_reads(&m.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect the bare-name reads of one expression (callee names excluded — they
/// aren't variable reads in this subset).
fn reads_of(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Name(n) => {
            out.insert(n.clone());
        }
        ExprKind::Unary(_, x) => reads_of(x, out),
        ExprKind::Bin(_, a, b) => {
            reads_of(a, out);
            reads_of(b, out);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                reads_of(a, out);
            }
        }
        ExprKind::Kwarg(_, v) => reads_of(v, out),
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            for it in items {
                reads_of(it, out);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                reads_of(k, out);
                reads_of(v, out);
            }
        }
        ExprKind::Index(o, i) => {
            reads_of(o, out);
            reads_of(i, out);
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            reads_of(obj, out);
            for b in [start, stop, step].into_iter().flatten() {
                reads_of(b, out);
            }
        }
        ExprKind::MethodCall(obj, _, args) => {
            reads_of(obj, out);
            for a in args {
                reads_of(a, out);
            }
        }
        ExprKind::Attr(obj, _) => reads_of(obj, out),
        ExprKind::ListComp { element, clauses } => {
            reads_of(element, out);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => reads_of(iter, out),
                    CompClause::If(cond) => reads_of(cond, out),
                }
            }
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            reads_of(key, out);
            reads_of(value, out);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => reads_of(iter, out),
                    CompClause::If(cond) => reads_of(cond, out),
                }
            }
        }
        _ => {}
    }
}

// --- shared AST visitors (for the teaching lints below) --------------------

/// Apply `f` to each nested statement block of `s` — every control-flow body and
/// every function/method/class body. Lets the lints below recurse without each
/// re-spelling the whole `StmtKind` match.
/// Whether a child block runs in the SAME variable scope as its parent
/// (if/for/while bodies) or opens a NEW one (def/method bodies). Scope-aware
/// lints (type churn, shadowing, unused locals) filter on this; purely
/// structural walks ignore it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockScope {
    Same,
    New,
}

/// THE single encoding of "which child blocks does a statement have".
/// Exhaustive on purpose — no `_` arm — so adding an AST statement variant
/// fails compilation HERE (one audit point) instead of being silently skipped
/// by half a dozen hand-rolled walkers (an invisible lint false negative).
///
/// When you add an arm here, also audit the walkers that keep custom recursion
/// because their semantics are interwoven with scope/fold logic: the
/// undefined-name family (`scope_bindings`/`check_reads`/`read_expr`), the
/// unused-local family (`collect_all_reads`/`reads_of`), and the cycle
/// analysis (`stmt_may_cycle` — conservative-true, so a miss there is sound
/// but imprecise). Everything else traverses through this function,
/// [`stmt_exprs`], and [`each_child_expr`].
fn for_each_child_block<'a>(s: &'a Stmt, mut f: impl FnMut(&'a [Stmt], BlockScope)) {
    match &s.kind {
        StmtKind::If {
            body,
            elifs,
            else_body,
            ..
        } => {
            f(body, BlockScope::Same);
            for (_, b) in elifs {
                f(b, BlockScope::Same);
            }
            if let Some(b) = else_body {
                f(b, BlockScope::Same);
            }
        }
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::While { body, .. } => f(body, BlockScope::Same),
        StmtKind::Def { body, .. } => f(body, BlockScope::New),
        StmtKind::ClassDef { methods, .. } => {
            for m in methods {
                f(&m.body, BlockScope::New);
            }
        }
        // No child blocks. Listed explicitly (not `_`) — see the doc comment.
        StmtKind::Expr(_)
        | StmtKind::Assign(..)
        | StmtKind::AnnAssign { .. }
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Return(_)
        | StmtKind::SetIndex { .. }
        | StmtKind::SetAttr { .. }
        | StmtKind::UnpackAssign { .. }
        | StmtKind::Import(_) => {}
    }
}

/// Call `f` on each expression a statement evaluates *in place* (NOT descending
/// into nested blocks — those are walked via [`for_each_child_block`]).
fn stmt_exprs(s: &Stmt, f: &mut impl FnMut(&Expr)) {
    match &s.kind {
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
        // No in-place expressions. Explicit (not `_`) so a new statement
        // variant must be classified here — see for_each_child_block.
        StmtKind::Return(None)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_) => {}
    }
}

/// Call `f` on each direct child expression of `e` (one level; recurse by
/// calling this again inside `f`).
fn each_child_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    match &e.kind {
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
        ExprKind::ListComp { element, clauses } => {
            f(element);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => f(iter),
                    CompClause::If(x) => f(x),
                }
            }
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            f(key);
            f(value);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => f(iter),
                    CompClause::If(x) => f(x),
                }
            }
        }
        // Leaves. Explicit (not `_`) so a new expression variant must be
        // classified here — see for_each_child_block.
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Str(_)
        | ExprKind::Name(_) => {}
    }
}

// --- mutable default arguments ---------------------------------------------

/// `def f(x, acc=[])`: a list/dict/set default is created ONCE (when the `def`
/// runs) and shared by every call, so mutations silently leak between calls —
/// the classic Python surprise. Flags the `def` line and points at the standard
/// `=None` + make-a-fresh-one-inside fix. Only *mutable* defaults fire; a number,
/// string, `None`, or tuple default is fine and never flagged.
pub fn mutable_default_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    walk_mutable_defaults(stmts, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

/// The kid-facing noun if `e` is a *mutable* default value, else `None`. Covers
/// literals (`[]`, `{}`, `{1: 2}`) and the constructor calls (`list()`, `dict()`,
/// `set()`, and `{1, 2}` which the parser desugars to `set(...)`).
fn mutable_default_noun(e: &Expr) -> Option<&'static str> {
    match &e.kind {
        ExprKind::List(_) => Some("a list"),
        ExprKind::Dict(_) => Some("a dictionary"),
        ExprKind::Call(name, _) => match name.as_str() {
            "list" => Some("a list"),
            "dict" => Some("a dictionary"),
            "set" => Some("a set"),
            _ => None,
        },
        _ => None,
    }
}

fn walk_mutable_defaults(stmts: &[Stmt], out: &mut Vec<(usize, String)>) {
    for s in stmts {
        if let StmtKind::Def {
            params, defaults, ..
        } = &s.kind
        {
            // defaults align to the trailing params (Python rule).
            let first = params.len().saturating_sub(defaults.len());
            for (i, d) in defaults.iter().enumerate() {
                if let Some(noun) = mutable_default_noun(d) {
                    let p = params.get(first + i).map(String::as_str).unwrap_or("it");
                    out.push((
                        s.line,
                        format!(
                            "`{p}` uses {noun} as its default — that default is created once and \
                             SHARED by every call, so changes to it leak between calls. Use \
                             `{p}=None` and make a fresh one inside the function instead."
                        ),
                    ));
                }
            }
        }
        for_each_child_block(s, |b, _| walk_mutable_defaults(b, out));
    }
}

// --- unreachable code ------------------------------------------------------

/// A statement that unconditionally follows a `return`/`break`/`continue` in the
/// SAME block can never run. Only same-level exits count — a `return` inside an
/// `if` doesn't kill the code after the `if` — so every hit is a true finding.
/// Reports the first dead line per block (the rest below it is dead too).
pub fn unreachable_code_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    walk_unreachable(stmts, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

/// The keyword if `s` is an unconditional block-exit, else `None`.
fn terminator_kw(s: &Stmt) -> Option<&'static str> {
    match &s.kind {
        StmtKind::Return(_) => Some("return"),
        StmtKind::Break => Some("break"),
        StmtKind::Continue => Some("continue"),
        _ => None,
    }
}

fn walk_unreachable(stmts: &[Stmt], out: &mut Vec<(usize, String)>) {
    let mut flagged = false;
    for (i, s) in stmts.iter().enumerate() {
        if !flagged
            && i > 0
            && let Some(kw) = terminator_kw(&stmts[i - 1])
        {
            out.push((
                s.line,
                format!(
                    "this line can't run — the `{kw}` just above always exits first. Move it above \
                     the `{kw}`, or remove it."
                ),
            ));
            flagged = true;
        }
        for_each_child_block(s, |b, _| walk_unreachable(b, out));
    }
}

// --- shadowed built-ins ----------------------------------------------------

/// Built-ins that are essentially never a good variable name, so binding one is
/// almost always an accidental shadow (`list = [...]` then `list(...)` breaks).
/// Deliberately EXCLUDES built-ins that ARE common, sensible variable names
/// (`sum`, `min`, `max`, `type`, `id`, `input`, `abs`, `round`, `open`,
/// `format`) so the lint never cries wolf over `sum = 0`.
const SHADOW_DANGEROUS: &[&str] = &[
    "list",
    "dict",
    "set",
    "str",
    "int",
    "float",
    "tuple",
    "bool",
    "range",
    "len",
    "print",
    "sorted",
    "reversed",
    "enumerate",
    "zip",
    "map",
    "filter",
];

/// Binding a name that shadows a common built-in type/function. Deduped to one
/// warning per shadowed name across the program, to stay calm.
pub fn shadowed_builtin_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    walk_shadow(stmts, &mut seen, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

fn shadow_check(
    name: &str,
    line: usize,
    seen: &mut HashSet<String>,
    out: &mut Vec<(usize, String)>,
) {
    if SHADOW_DANGEROUS.contains(&name) && seen.insert(name.to_string()) {
        out.push((
            line,
            format!(
                "`{name}` is the name of a built-in — using it as a variable means you can't call \
                 `{name}(...)` afterwards. A name like `items` or `values` avoids the clash."
            ),
        ));
    }
}

fn walk_shadow(stmts: &[Stmt], seen: &mut HashSet<String>, out: &mut Vec<(usize, String)>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, _) | StmtKind::AnnAssign { name, .. } => {
                shadow_check(name, s.line, seen, out)
            }
            StmtKind::For { var, .. } | StmtKind::ForEach { var, .. } => {
                shadow_check(var, s.line, seen, out)
            }
            _ => {}
        }
        for_each_child_block(s, |b, _| walk_shadow(b, seen, out));
    }
}

// --- self-comparison / no-op self-assignment -------------------------------

/// Comparing something to itself (`if x == x:` — always the same answer) or
/// assigning a variable to itself (`x = x` — does nothing). Only fires on *pure*
/// operands that contain a variable, so `random() == random()` (two calls, not
/// self-comparison) and constant folds like `1 == 1` are left alone.
pub fn self_comparison_warnings(stmts: &[Stmt]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    walk_self_cmp(stmts, &mut out);
    out.sort_by_key(|(line, _)| *line);
    out
}

/// No calls anywhere inside — so evaluating `e` twice yields the same value with
/// no side effects (the precondition for calling `a <op> a` redundant).
fn is_pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::NoneLit
        | ExprKind::Name(_) => true,
        ExprKind::Unary(_, x) | ExprKind::Attr(x, _) => is_pure(x),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => is_pure(a) && is_pure(b),
        _ => false,
    }
}

/// Whether `e` references at least one variable (so `x == x` flags but `1 == 1`,
/// a constant comparison, does not — the "compare to itself" wording fits only
/// the variable case).
fn expr_has_name(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Name(_) => true,
        ExprKind::Unary(_, x) | ExprKind::Attr(x, _) => expr_has_name(x),
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => expr_has_name(a) || expr_has_name(b),
        _ => false,
    }
}

/// The constant result of a self-comparison `a <op> a`, for comparison ops only.
fn self_cmp_verdict(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::Eq | BinOp::Le | BinOp::Ge => Some("always True"),
        BinOp::Ne | BinOp::Lt | BinOp::Gt => Some("always False"),
        _ => None,
    }
}

fn walk_self_cmp(stmts: &[Stmt], out: &mut Vec<(usize, String)>) {
    for s in stmts {
        // No-op self-assignment: `x = x`.
        if let StmtKind::Assign(name, value) = &s.kind
            && matches!(&value.kind, ExprKind::Name(n) if n == name)
        {
            out.push((
                s.line,
                format!(
                    "`{name} = {name}` doesn't do anything — a variable assigned to itself is \
                     unchanged. Did you mean to change it, or is it a leftover line?"
                ),
            ));
        }
        // Self-comparisons anywhere in the statement's expressions.
        stmt_exprs(s, &mut |e| find_self_cmp(e, out));
        for_each_child_block(s, |b, _| walk_self_cmp(b, out));
    }
}

fn find_self_cmp(e: &Expr, out: &mut Vec<(usize, String)>) {
    if let ExprKind::Bin(op, a, b) = &e.kind
        && let Some(verdict) = self_cmp_verdict(*op)
        && is_pure(a)
        && expr_has_name(a)
        && a == b
    {
        out.push((
            e.line,
            format!(
                "this compares something to itself, so it's {verdict} — did you mean to compare \
                 two different things?"
            ),
        ));
    }
    each_child_expr(e, &mut |c| find_self_cmp(c, out));
}

// --- scaffolded fix ladders ------------------------------------------------
//
// Each concept-bearing lint carries a *fading hint ladder*: a guiding question,
// then a narrower hint, then the concrete fix. The student escalates only as far
// as they need, so they do the reasoning (self-explanation) without being
// stranded (completion-problem scaffolding). Two roles beyond teaching:
//   1. Assessment instrument — how far down the ladder a student goes is a
//      proficiency signal (Evidence-Centered Design / stealth assessment), the
//      kind of evidence the activity `report()` channel carries.
//   2. AI-tutor seam — the ladders are authored here as DATA (deterministic,
//      glass-box, offline). A future adaptive tutor can regenerate the rungs per
//      student; the "every diagnostic has a fading scaffold" contract is stable.
// Mechanical lints (a typo, a dead line) have no concept to teach, so they get
// no ladder (`scaffold` returns `None`) — a one-click fix, not a question.

/// Which diagnostic produced a lint — used to look up its fix scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintKind {
    Typo,
    UndefinedName,
    TypeChurn,
    UnusedLocal,
    MutableDefault,
    UnreachableCode,
    ShadowedBuiltin,
    SelfComparison,
}

/// A fading hint ladder for a concept-bearing lint. Rungs are least-help-first;
/// the student stops at whichever they can solve from.
#[derive(Debug, Clone, Copy)]
pub struct Scaffold {
    /// A guiding question that orients attention to the concept (no answer).
    pub question: &'static str,
    /// A narrower nudge for a student still stuck after the question.
    pub hint: &'static str,
    /// The concrete fix, shown only on request (the escape hatch).
    pub fix: &'static str,
}

/// The fix scaffold for a lint kind, or `None` for the mechanical / advisory
/// lints with no single concept to teach (a spelling typo, dead code after a
/// `return`, an undefined name, a type churn).
pub fn scaffold(kind: LintKind) -> Option<Scaffold> {
    Some(match kind {
        LintKind::MutableDefault => Scaffold {
            question: "This list (or dict) is made once and shared by EVERY call to the \
                       function. How could you give each call its own fresh one?",
            hint: "Start the parameter at `None` instead of `[]`, then create the list on the \
                   first line inside the function.",
            fix: "Change the default to `None` (e.g. `acc=None`), then make the first line \
                  `if acc is None:` and set `acc = []` inside it.",
        },
        LintKind::ShadowedBuiltin => Scaffold {
            question: "This variable has the same name as a built-in tool. If you renamed it, \
                       what name would still say what it holds?",
            hint: "Pick a name that describes the data — like `items`, `values`, or `numbers` \
                   — instead of the built-in's name.",
            fix: "Rename the variable everywhere it appears (e.g. `list` → `items`) so the \
                  built-in still works.",
        },
        LintKind::UnusedLocal => Scaffold {
            question: "You set this variable but nothing ever uses its value. What did you plan \
                       to do with it?",
            hint: "Did you mean to send it back with `return`, show it with `print(...)`, or \
                   use it on the next line?",
            fix: "Either use the value (e.g. `return <name>` / `print(<name>)`), or delete the \
                  line if it was left over.",
        },
        LintKind::SelfComparison => Scaffold {
            question: "This compares (or assigns) something to itself, so the result never \
                       changes. What did you actually mean to use on one side?",
            hint: "Look at both sides — one of them is probably meant to be a different \
                   variable or value.",
            fix: "Replace one side with what you meant to check against (e.g. `count == limit`, \
                  not `count == count`).",
        },
        LintKind::Typo
        | LintKind::UndefinedName
        | LintKind::UnreachableCode
        | LintKind::TypeChurn => return None,
    })
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

    fn unused(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        unused_assignment_warnings(&stmts)
    }

    #[test]
    fn unused_flags_dead_locals() {
        // A local set and never read → the forgotten-return smell.
        let w = unused("def f():\n    result = 1 + 2\n    return 5\n");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 2);
        assert!(w[0].1.contains("`result`"));
        // Inside a method too.
        assert_eq!(
            unused("class C:\n    def m(self):\n        tmp = 9\n        return 0\n").len(),
            1
        );
    }

    #[test]
    fn unused_does_not_cry_wolf() {
        // Used locals never fire.
        assert!(unused("def f():\n    x = 1\n    return x\n").is_empty());
        // Self-referential accumulator (reads itself) — not dead.
        assert!(unused("def f():\n    t = 0\n    t = t + 1\n    return t\n").is_empty());
        // Read in another branch counts.
        assert!(
            unused("def f(c):\n    x = 1\n    if c:\n        return x\n    return 0\n").is_empty()
        );
        // Closure read (nested function) suppresses it.
        assert!(
            unused(
                "def outer():\n    n = 5\n    def inner():\n        return n\n    return inner\n"
            )
            .is_empty()
        );
        // `_`-prefixed is a deliberate throwaway.
        assert!(unused("def f():\n    _tmp = compute()\n    return 0\n").is_empty());
        // Loop vars, unpack targets, and params are NOT flagged.
        assert!(unused("def f():\n    for i in range(3):\n        print(\"hi\")\n").is_empty());
        assert!(unused("def f(pair):\n    a, b = pair\n    return a\n").is_empty());
        assert!(unused("def f(unused_param):\n    return 1\n").is_empty());
        // Module/class level is never flagged (conservative by design).
        assert!(unused("answer = 42\n").is_empty());
        // Used via attribute/index still counts as a read.
        assert!(unused("def f():\n    xs = [1, 2]\n    return xs[0]\n").is_empty());
    }

    fn mut_def(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        mutable_default_warnings(&stmts)
    }

    #[test]
    fn mutable_default_flags_shared_containers() {
        // The classic footgun: a list default shared across calls.
        let w = mut_def("def add(x, acc=[]):\n    acc.append(x)\n    return acc\n");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 1);
        assert!(
            w[0].1.contains("`acc`") && w[0].1.contains("a list"),
            "{}",
            w[0].1
        );
        // Dict and set literals + constructor calls also fire, on the right param.
        assert_eq!(mut_def("def f(a, b={}):\n    return b\n").len(), 1);
        assert_eq!(mut_def("def f(s=set()):\n    return s\n").len(), 1);
        assert_eq!(mut_def("def f(d=dict()):\n    return d\n").len(), 1);
    }

    #[test]
    fn mutable_default_does_not_cry_wolf() {
        // Immutable defaults are all fine.
        assert!(
            mut_def("def f(n=0, name=\"x\", flag=None, pair=(1, 2)):\n    return n\n").is_empty()
        );
        // No defaults at all.
        assert!(mut_def("def f(a, b):\n    return a + b\n").is_empty());
    }

    fn unreach(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        unreachable_code_warnings(&stmts)
    }

    #[test]
    fn unreachable_flags_code_after_exit() {
        // After a return in the same block.
        let w = unreach("def f():\n    return 1\n    print(\"never\")\n");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 3);
        assert!(w[0].1.contains("return"));
        // break / continue in a loop too.
        assert_eq!(
            unreach("for i in range(3):\n    break\n    print(i)\n").len(),
            1
        );
        assert_eq!(
            unreach("for i in range(3):\n    continue\n    print(i)\n").len(),
            1
        );
    }

    #[test]
    fn unreachable_does_not_cry_wolf() {
        // A return as the last statement is fine.
        assert!(unreach("def f():\n    x = 1\n    return x\n").is_empty());
        // A return INSIDE an if does NOT kill code after the if (the else path
        // continues) — this is the precision that keeps it honest.
        assert!(unreach("def f(c):\n    if c:\n        return 1\n    return 2\n").is_empty());
        // Code before the return is reachable.
        assert!(unreach("def f():\n    print(\"hi\")\n    return 1\n").is_empty());
    }

    fn shadow(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        shadowed_builtin_warnings(&stmts)
    }

    #[test]
    fn shadow_flags_dangerous_builtins() {
        let w = shadow("list = [1, 2, 3]\n");
        assert_eq!(w.len(), 1);
        assert!(w[0].1.contains("`list`"));
        // A loop variable named after a builtin shadows it too.
        assert_eq!(
            shadow("for str in [\"a\", \"b\"]:\n    print(str)\n").len(),
            1
        );
        // Deduped: one warning per shadowed name.
        assert_eq!(shadow("dict = {}\ndict = {1: 2}\n").len(), 1);
    }

    #[test]
    fn shadow_does_not_cry_wolf() {
        // Builtins that ARE common variable names are deliberately allowed.
        assert!(shadow("sum = 0\nfor x in [1, 2]:\n    sum = sum + x\n").is_empty());
        assert!(shadow("max = 10\ntype = \"circle\"\ninput = get()\n").is_empty());
        // An ordinary name is fine.
        assert!(shadow("total = 0\nitems = []\n").is_empty());
    }

    fn selfcmp(src: &str) -> Vec<(usize, String)> {
        let toks = crate::lexer::lex(src).unwrap();
        let (stmts, _) = crate::parser::parse_recovering(&toks);
        self_comparison_warnings(&stmts)
    }

    #[test]
    fn self_comparison_flags_redundant_checks() {
        // x == x is always True.
        let w = selfcmp("x = 5\nif x == x:\n    print(\"yes\")\n");
        assert_eq!(w.len(), 1);
        assert!(w[0].1.contains("always True"), "{}", w[0].1);
        // != is always False; attribute/index self-compares count.
        assert!(
            selfcmp("if a != a:\n    pass\n")[0]
                .1
                .contains("always False")
        );
        assert_eq!(selfcmp("if obj.x == obj.x:\n    pass\n").len(), 1);
        // No-op self-assignment.
        let a = selfcmp("x = 1\nx = x\n");
        assert_eq!(a.len(), 1);
        assert!(a[0].1.contains("doesn't do anything"), "{}", a[0].1);
    }

    #[test]
    fn self_comparison_does_not_cry_wolf() {
        // Two calls are NOT self-comparison (could differ / have effects).
        assert!(selfcmp("if roll() == roll():\n    pass\n").is_empty());
        // Comparing two different things is fine.
        assert!(selfcmp("if a == b:\n    pass\n").is_empty());
        // Constant folds (no variable) are left alone.
        assert!(selfcmp("if 1 == 1:\n    pass\n").is_empty());
        // A real reassignment is not a no-op.
        assert!(selfcmp("x = 1\nx = x + 1\n").is_empty());
    }

    #[test]
    fn scaffolds_cover_concept_lints_only() {
        // Concept-bearing lints have a 3-rung ladder.
        for k in [
            LintKind::MutableDefault,
            LintKind::ShadowedBuiltin,
            LintKind::UnusedLocal,
            LintKind::SelfComparison,
        ] {
            let s = scaffold(k).expect("concept lint should have a scaffold");
            assert!(!s.question.is_empty() && !s.hint.is_empty() && !s.fix.is_empty());
        }
        // Mechanical / advisory lints have none (nothing to teach, or no single fix).
        for k in [
            LintKind::Typo,
            LintKind::UnreachableCode,
            LintKind::UndefinedName,
            LintKind::TypeChurn,
        ] {
            assert!(scaffold(k).is_none(), "{k:?} should have no scaffold");
        }
    }

    #[test]
    fn pass_is_never_flagged() {
        // `pass` is a statement, not a name — the undefined-variable lint (which
        // used to see `Name("pass")`) must not touch it, in a def or a loop.
        assert!(undef("def f():\n    pass\n").is_empty());
        assert!(undef("for i in range(3):\n    pass\n").is_empty());
        // And a `pass` before a `return` is reachable (no unreachable warning).
        assert!(unreach("def f():\n    pass\n    return 1\n").is_empty());
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
