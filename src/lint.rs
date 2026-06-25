//! Lightweight semantic lints over the parsed AST.
//!
//! Currently: a "did you mean…?" for a call to a function that doesn't exist
//! but closely matches a known name (e.g. `pint(i)` → `print`). Deliberately
//! conservative — only a near-miss of a known builtin or a function the program
//! itself defines is flagged, so a planned-but-undefined helper (`my_helper()`)
//! or an unfamiliar name isn't second-guessed.

use crate::ast::{CompClause, Expr, ExprKind, Stmt, StmtKind};
use crate::error::CompileError;
use crate::parser::did_you_mean;

/// Builtins the compiler knows (plus type constructors). A call to a near-miss
/// of one of these — that isn't itself a known/defined name — is almost
/// certainly a typo.
const BUILTINS: &[&str] = &[
    "print",
    "range",
    "len",
    "int",
    "str",
    "float",
    "bool",
    "abs",
    "min",
    "max",
    "sum",
    "sorted",
    "input",
    "round",
    "any",
    "all",
    "enumerate",
    "zip",
    "repr",
    "list",
    "dict",
    "set",
    "tuple",
    "type",
];

/// "Did you mean…?" diagnostics for calls to unknown, near-miss function names.
pub fn typo_diagnostics(stmts: &[Stmt]) -> Vec<CompileError> {
    let mut defs: Vec<String> = Vec::new();
    collect_defs(stmts, &mut defs);
    let mut known: Vec<&str> = BUILTINS.to_vec();
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
                out.push(CompileError::at(
                    e.line,
                    format!("`{name}` isn't defined — did you mean `{sugg}`?"),
                ));
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
}
