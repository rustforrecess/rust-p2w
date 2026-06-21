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
            StmtKind::Expr(e) | StmtKind::Assign(_, e) => walk_expr(e, known, out),
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
            if !known.contains(&name.as_str()) {
                if let Some(sugg) = did_you_mean(name, known) {
                    out.push(CompileError::at(
                        e.line,
                        format!("`{name}` isn't defined — did you mean `{sugg}`?"),
                    ));
                }
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
