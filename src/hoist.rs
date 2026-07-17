//! Lift non-capturing nested `def`s to module level.
//!
//! Functions aren't first-class values in the subset and there are no closures,
//! so a nested function that only reads globals / other functions / its own
//! locals is really just a module-level function written inside another — we
//! hoist it out so both compiled backends (and the debugger) can run it. A
//! nested function that reads a variable *local to an enclosing function* would
//! need a closure; that's a clean, specific error. Hoisted names must be unique
//! across the program (no mangling) so a call like `inner()` resolves to exactly
//! one function.

use crate::ast::{ExprKind, Stmt, StmtKind};
use crate::error::CompileError;
use crate::lint::{BlockScope, for_each_child_block, stmt_exprs};
use crate::reuse::vars_read;
use std::collections::BTreeSet;

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

/// Lift non-capturing nested functions to module level. On success every
/// remaining function body is free of nested `def`s. Errors on a closure
/// (captured enclosing local) or a duplicate function name.
pub fn hoist_nested_functions(program: Vec<Stmt>) -> Result<Vec<Stmt>, CompileError> {
    // Every function name in the program (top level + to-be-hoisted), for
    // duplicate detection. Seed with the top-level names.
    let mut names: BTreeSet<String> = program
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Def { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();

    // Split top-level statements: functions go through the worklist (which may
    // grow as nested defs are lifted out); everything else keeps its order.
    let mut worklist: Vec<Stmt> = Vec::new();
    let mut other: Vec<Stmt> = Vec::new();
    for s in program {
        if matches!(s.kind, StmtKind::Def { .. }) {
            worklist.push(s);
        } else {
            other.push(s);
        }
    }

    let mut functions: Vec<Stmt> = Vec::new();
    while let Some(mut def_stmt) = worklist.pop() {
        let StmtKind::Def { params, body, .. } = &mut def_stmt.kind else {
            unreachable!("worklist holds only defs");
        };
        let outer_locals = function_locals(params, body);

        // Pull any nested defs out of this body's top level.
        let mut kept = Vec::with_capacity(body.len());
        for stmt in std::mem::take(body) {
            let StmtKind::Def {
                name: inner_name,
                params: inner_params,
                body: inner_body,
                ..
            } = &stmt.kind
            else {
                kept.push(stmt);
                continue;
            };

            // Closure check: the nested function may not read a variable that is
            // local to this (enclosing) function.
            let inner_locals = function_locals(inner_params, inner_body);
            let mut reads = BTreeSet::new();
            reads_here(inner_body, &mut reads);
            if let Some(captured) = reads
                .iter()
                .find(|r| !inner_locals.contains(*r) && outer_locals.contains(*r))
            {
                return Err(CompileError::at(
                    stmt.line,
                    format!(
                        "nested function '{inner_name}' uses '{captured}' from the \
                         enclosing function — closures aren't supported yet (move it \
                         out, or pass '{captured}' in as an argument)"
                    ),
                ));
            }

            // Uniqueness: `names` already contains every top-level name and each
            // nested name discovered so far. A fresh discovery must insert; a
            // clash means two functions share a name.
            if !names.insert(inner_name.clone()) {
                return Err(CompileError::at(
                    stmt.line,
                    format!(
                        "a function named '{inner_name}' is already defined — nested \
                         function names must be unique (no shadowing yet)"
                    ),
                ));
            }
            // The lifted function itself may contain further nested defs.
            worklist.push(stmt);
        }
        *body = kept;
        functions.push(def_stmt);
    }

    // Functions first (the front-end pre-collects them regardless of position),
    // then the module-level code in its original order.
    functions.extend(other);
    Ok(functions)
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

    /// Count top-level function defs and confirm none are left nested.
    fn top_level_defs(stmts: &[Stmt]) -> usize {
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
            .filter(|s| matches!(s.kind, StmtKind::Def { .. }))
            .count()
    }

    #[test]
    fn lifts_a_non_capturing_nested_def() {
        let out = hoist("def outer():\n    def inner():\n        return 1\n    return inner()\n").unwrap();
        // outer + the lifted inner are both top-level now.
        assert_eq!(top_level_defs(&out), 2);
    }

    #[test]
    fn triple_nesting_all_lift() {
        let out = hoist(
            "def a():\n    def b():\n        def c():\n            return 3\n        return c()\n    return b()\n",
        )
        .unwrap();
        assert_eq!(top_level_defs(&out), 3);
    }

    #[test]
    fn passing_the_param_in_is_not_capture() {
        let out = hoist("def outer(x):\n    def dbl(n):\n        return n * 2\n    return dbl(x)\n");
        assert!(out.is_ok());
    }

    #[test]
    fn reading_a_global_or_calling_a_function_is_not_capture() {
        assert!(hoist("G = 1\ndef o():\n    def h():\n        return G\n    return h()\n").is_ok());
        assert!(
            hoist("def t():\n    return 1\ndef o():\n    def h():\n        return t()\n    return h()\n")
                .is_ok()
        );
    }

    #[test]
    fn capturing_an_enclosing_local_errors() {
        let err = hoist("def o():\n    x = 5\n    def h():\n        return x\n    return h()\n")
            .unwrap_err();
        assert!(err.contains("closures aren't supported"), "{err}");
        assert!(err.contains("'x'"), "{err}");
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
    fn a_program_without_nested_defs_is_unchanged() {
        let out = hoist("def f():\n    return 1\nx = f()\nprint(x)\n").unwrap();
        assert_eq!(top_level_defs(&out), 1);
    }
}
