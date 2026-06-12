//! WAT code generation.
//!
//! Output conventions mirror p2w's runnable module shape so the same browser
//! harness can execute it: the body is an exported `_start` returning an i32
//! exit code (0), and output goes through host imports `env.write_char(i32)` /
//! `env.write_i32(i32)`.
//!
//! Supports: `print(...)`, integer arithmetic (`//` and `%` with Python floor
//! semantics), string literals, integer variables, comparisons (including
//! chains, via the parser) / booleans (`and`/`or` short-circuit and yield the
//! deciding operand, like Python), `if`/`elif`/`else`, `while`/`break`/
//! `continue`, and `for i in range(...)` with a constant step (negative steps
//! count down). Integers (and booleans, as 0/1) are i32. Variables and loop
//! counters compile to WASM locals (Python has no block scope, so all locals
//! live at function level). Compiler-internal locals are prefixed `.` (valid
//! in WAT identifiers, impossible in Python ones, so they can't collide).
//!
//! Structure: `Gen` holds module-wide state (which runtime helpers are used);
//! `FuncCx` holds per-function state (locals, labels, loop stack) — `_start`
//! is the only function today, but `def` lands as one `FuncCx` per function.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::emit::{Body, Func, Module};
use crate::error::CompileError;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, CompileError>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Str,
}

type Vars = HashMap<String, Ty>;

pub fn generate(stmts: &[Stmt]) -> Result<String> {
    let mut g = Gen::default();
    let mut cx = FuncCx::default();
    let mut body = Body::new();
    g.stmts(&mut cx, stmts, &mut body)?;
    body.push("(i32.const 0)");

    let mut module = Module::default();
    module
        .imports
        .push(r#"(import "env" "write_char" (func $write_char (param i32)))"#.into());
    module
        .imports
        .push(r#"(import "env" "write_i32" (func $write_i32 (param i32)))"#.into());
    module.funcs.push(Func {
        signature: r#"(func $_start (export "_start") (result i32)"#.into(),
        locals: cx
            .locals
            .iter()
            .map(|name| format!("(local ${name} i32)"))
            .collect(),
        body,
    });
    if g.uses_floordiv {
        module.funcs.push(floordiv_helper());
    }
    if g.uses_floormod {
        module.funcs.push(floormod_helper());
    }
    Ok(module.render())
}

/// Python floor division: truncating `i32.div_s` adjusted by -1 when the
/// signs differ and the division isn't exact (`-7 // 2` is -4, not -3).
fn floordiv_helper() -> Func {
    let mut b = Body::new();
    b.push("(local.set $q (i32.div_s (local.get $a) (local.get $b)))");
    b.push("(if (i32.and");
    b.push_in(
        2,
        "(i32.ne (i32.rem_s (local.get $a) (local.get $b)) (i32.const 0))",
    );
    b.push_in(
        2,
        "(i32.ne (i32.lt_s (local.get $a) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))",
    );
    b.push_in(
        1,
        "(then (local.set $q (i32.sub (local.get $q) (i32.const 1)))))",
    );
    b.push("(local.get $q)");
    Func {
        signature: "(func $i32_floordiv (param $a i32) (param $b i32) (result i32)".into(),
        locals: vec!["(local $q i32)".into()],
        body: b,
    }
}

/// Python modulo: the result takes the sign of the divisor (`-7 % 2` is 1).
fn floormod_helper() -> Func {
    let mut b = Body::new();
    b.push("(local.set $r (i32.rem_s (local.get $a) (local.get $b)))");
    b.push("(if (i32.and");
    b.push_in(2, "(i32.ne (local.get $r) (i32.const 0))");
    b.push_in(
        2,
        "(i32.ne (i32.lt_s (local.get $r) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))",
    );
    b.push_in(
        1,
        "(then (local.set $r (i32.add (local.get $r) (local.get $b)))))",
    );
    b.push("(local.get $r)");
    Func {
        signature: "(func $i32_floormod (param $a i32) (param $b i32) (result i32)".into(),
        locals: vec!["(local $r i32)".into()],
        body: b,
    }
}

/// Module-wide codegen state.
#[derive(Default)]
struct Gen {
    uses_floordiv: bool,
    uses_floormod: bool,
}

/// Per-function codegen state.
#[derive(Default)]
struct FuncCx {
    vars: Vars,
    locals: Vec<String>,
    label: usize,
    scratch: usize,
    /// Enclosing loops as `(break_label, continue_label)`, innermost last.
    /// In a `for`, continue targets the inner `$c` block so the counter
    /// increment still runs; in a `while`, it targets the loop head (re-test).
    loops: Vec<(String, String)>,
}

impl FuncCx {
    fn fresh(&mut self) -> usize {
        let n = self.label;
        self.label += 1;
        n
    }

    /// A fresh compiler-internal local (`.`-prefixed, so it can't collide
    /// with a Python variable name).
    fn scratch_local(&mut self) -> String {
        let name = format!(".t{}", self.scratch);
        self.scratch += 1;
        self.locals.push(name.clone());
        name
    }

    fn ensure_local(&mut self, name: &str, ty: Ty) {
        if !self.vars.contains_key(name) {
            self.vars.insert(name.to_string(), ty);
            self.locals.push(name.to_string());
        }
    }
}

impl Gen {
    fn stmts(&mut self, cx: &mut FuncCx, stmts: &[Stmt], out: &mut Body) -> Result<()> {
        for s in stmts {
            self.stmt(cx, s, out)?;
        }
        Ok(())
    }

    fn stmt(&mut self, cx: &mut FuncCx, s: &Stmt, out: &mut Body) -> Result<()> {
        match &s.kind {
            StmtKind::Assign(name, expr) => {
                let t = type_of(expr, &cx.vars)?;
                if t != Ty::Int {
                    return Err(CompileError::at(
                        s.line,
                        format!(
                            "variable '{name}' must be a number for now (string variables aren't supported yet)"
                        ),
                    ));
                }
                cx.ensure_local(name, Ty::Int);
                let value = self.int_expr(cx, expr)?;
                out.push(format!("(local.set ${name} {value})"));
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => self.gen_print(cx, args, out),
                ExprKind::Call(name, _) => Err(CompileError::at(
                    s.line,
                    format!(
                        "only print(...) is supported so far — '{name}(...)' isn't implemented yet"
                    ),
                )),
                _ => Err(CompileError::at(
                    s.line,
                    "a bare value on its own line has no effect; did you mean print(...)?",
                )),
            },
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => self.gen_if(cx, cond, body, elifs, else_body, out),
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => self.gen_for(cx, var, start, end, step, body, s.line, out),
            StmtKind::While { cond, body } => self.gen_while(cx, cond, body, out),
            StmtKind::Break => match cx.loops.last() {
                Some((brk, _)) => {
                    out.push(format!("(br {brk})"));
                    Ok(())
                }
                None => Err(CompileError::at(
                    s.line,
                    "'break' can only be used inside a loop",
                )),
            },
            StmtKind::Continue => match cx.loops.last() {
                Some((_, cont)) => {
                    out.push(format!("(br {cont})"));
                    Ok(())
                }
                None => Err(CompileError::at(
                    s.line,
                    "'continue' can only be used inside a loop",
                )),
            },
        }
    }

    fn gen_print(&mut self, cx: &mut FuncCx, args: &[Expr], out: &mut Body) -> Result<()> {
        for (idx, arg) in args.iter().enumerate() {
            if idx > 0 {
                emit_char(out, b' ');
            }
            match type_of(arg, &cx.vars)? {
                Ty::Int => {
                    let expr_wat = self.int_expr(cx, arg)?;
                    out.push(format!("(call $write_i32 {expr_wat})"));
                }
                Ty::Str => {
                    if let ExprKind::Str(s) = &arg.kind {
                        for byte in s.bytes() {
                            emit_char(out, byte);
                        }
                    } else {
                        return Err(CompileError::at(
                            arg.line,
                            "only string literals can be printed so far",
                        ));
                    }
                }
            }
        }
        emit_char(out, b'\n');
        Ok(())
    }

    fn gen_if(
        &mut self,
        cx: &mut FuncCx,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
        out: &mut Body,
    ) -> Result<()> {
        require_int(cond, &cx.vars, "an if-condition")?;
        let c = self.int_expr(cx, cond)?;
        let mut then_b = Body::new();
        self.stmts(cx, body, &mut then_b)?;
        let else_b = self.else_chain(cx, elifs, else_body)?;

        out.push(format!("(if {c}"));
        out.push_in(1, "(then");
        out.append(then_b, 2);
        out.push_in(1, ")");
        if let Some(e) = else_b {
            out.push_in(1, "(else");
            out.append(e, 2);
            out.push_in(1, ")");
        }
        out.push(")");
        Ok(())
    }

    /// The else-side of an if: an elif chain lowers to a nested if inside the
    /// else. Returns None when there is no else at all.
    fn else_chain(
        &mut self,
        cx: &mut FuncCx,
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<Option<Body>> {
        if let Some(((cond, body), rest)) = elifs.split_first() {
            require_int(cond, &cx.vars, "an elif-condition")?;
            let c = self.int_expr(cx, cond)?;
            let mut then_b = Body::new();
            self.stmts(cx, body, &mut then_b)?;
            let inner = self.else_chain(cx, rest, else_body)?;

            let mut b = Body::new();
            b.push(format!("(if {c}"));
            b.push_in(1, "(then");
            b.append(then_b, 2);
            b.push_in(1, ")");
            if let Some(e) = inner {
                b.push_in(1, "(else");
                b.append(e, 2);
                b.push_in(1, ")");
            }
            b.push(")");
            Ok(Some(b))
        } else if let Some(body) = else_body {
            let mut b = Body::new();
            self.stmts(cx, body, &mut b)?;
            Ok(Some(b))
        } else {
            Ok(None)
        }
    }

    fn gen_while(
        &mut self,
        cx: &mut FuncCx,
        cond: &Expr,
        body: &[Stmt],
        out: &mut Body,
    ) -> Result<()> {
        require_int(cond, &cx.vars, "a while-condition")?;
        let c = self.int_expr(cx, cond)?;
        let n = cx.fresh();

        cx.loops.push((format!("$b{n}"), format!("$l{n}")));
        let mut body_b = Body::new();
        let r = self.stmts(cx, body, &mut body_b);
        cx.loops.pop();
        r?;

        out.push(format!("(block $b{n}"));
        out.push_in(1, format!("(loop $l{n}"));
        out.push_in(2, format!("(br_if $b{n} (i32.eqz {c}))"));
        out.append(body_b, 2);
        out.push_in(2, format!("(br $l{n})"));
        out.push_in(1, ")");
        out.push(")");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_for(
        &mut self,
        cx: &mut FuncCx,
        var: &str,
        start: &Expr,
        end: &Expr,
        step: &Expr,
        body: &[Stmt],
        line: usize,
        out: &mut Body,
    ) -> Result<()> {
        require_int(start, &cx.vars, "a range start")?;
        require_int(end, &cx.vars, "a range end")?;

        // A runtime step would need a sign-aware termination check; until that
        // lands, only constant steps are accepted (so the direction is known).
        let step_v = match const_int(step) {
            Some(0) => return Err(CompileError::at(line, "range() step can't be zero")),
            Some(v) => {
                i32::try_from(v).map_err(|_| CompileError::at(line, "range() step is too big"))?
            }
            None => {
                return Err(CompileError::at(
                    line,
                    "the range() step must be a plain number for now",
                ))
            }
        };
        let done_cmp = if step_v > 0 { "i32.ge_s" } else { "i32.le_s" };

        let start_wat = self.int_expr(cx, start)?;
        // Python evaluates range() bounds once, before the loop — snapshot a
        // non-constant end so the body mutating its variables can't change
        // the iteration count.
        let end_wat = self.int_expr(cx, end)?;
        let end_operand = if const_int(end).is_some() {
            end_wat
        } else {
            let snap = cx.scratch_local();
            out.push(format!("(local.set ${snap} {end_wat})"));
            format!("(local.get ${snap})")
        };

        // Iterate a hidden counter and assign it to the loop variable at the
        // top of each pass, so reassigning the variable in the body doesn't
        // change the iteration count (matching Python). The variable is a
        // function-level local, visible after the loop.
        let n = cx.fresh();
        let ctr = format!(".f{n}");
        cx.locals.push(ctr.clone());
        cx.ensure_local(var, Ty::Int);

        cx.loops.push((format!("$b{n}"), format!("$c{n}")));
        let mut body_b = Body::new();
        let r = self.stmts(cx, body, &mut body_b);
        cx.loops.pop();
        r?;

        out.push(format!("(local.set ${ctr} {start_wat})"));
        out.push(format!("(block $b{n}"));
        out.push_in(1, format!("(loop $l{n}"));
        out.push_in(
            2,
            format!("(br_if $b{n} ({done_cmp} (local.get ${ctr}) {end_operand}))"),
        );
        out.push_in(2, format!("(local.set ${var} (local.get ${ctr}))"));
        out.push_in(2, format!("(block $c{n}"));
        out.append(body_b, 3);
        out.push_in(2, ")");
        out.push_in(
            2,
            format!("(local.set ${ctr} (i32.add (local.get ${ctr}) (i32.const {step_v})))"),
        );
        out.push_in(2, format!("(br $l{n})"));
        out.push_in(1, ")");
        out.push(")");
        Ok(())
    }

    /// Generate WAT that pushes the i32 value of an integer (or boolean)
    /// expression.
    fn int_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        // Fold constants first — this is also where literals are range-checked
        // instead of silently wrapping (3000000000 must not become a negative).
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v) => Ok(format!("(i32.const {v})")),
                Err(_) => Err(CompileError::at(
                    e.line,
                    format!(
                        "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                    ),
                )),
            };
        }
        match &e.kind {
            // All literals (and negated literals) were handled by const_int.
            ExprKind::Int(_) => unreachable!("integer literals are folded above"),
            ExprKind::Name(n) => match cx.vars.get(n) {
                Some(Ty::Int) => Ok(format!("(local.get ${n})")),
                Some(Ty::Str) => Err(CompileError::at(
                    e.line,
                    format!("'{n}' is text, not a number"),
                )),
                None => Err(CompileError::at(e.line, format!("unknown name '{n}'"))),
            },
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.int_expr(cx, inner)?;
                Ok(format!("(i32.sub (i32.const 0) {v})"))
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let v = self.int_expr(cx, inner)?;
                Ok(format!("(i32.eqz {v})"))
            }
            ExprKind::Bin(BinOp::And, a, b) => {
                // Python value semantics with short-circuit: `a and b` is `a`
                // if a is falsy, else `b` (b unevaluated when a is falsy).
                let lhs = self.int_expr(cx, a)?;
                let rhs = self.int_expr(cx, b)?;
                let t = cx.scratch_local();
                Ok(format!(
                    "(if (result i32) (local.tee ${t} {lhs}) (then {rhs}) (else (local.get ${t})))"
                ))
            }
            ExprKind::Bin(BinOp::Or, a, b) => {
                let lhs = self.int_expr(cx, a)?;
                let rhs = self.int_expr(cx, b)?;
                let t = cx.scratch_local();
                Ok(format!(
                    "(if (result i32) (local.tee ${t} {lhs}) (then (local.get ${t})) (else {rhs}))"
                ))
            }
            ExprKind::Bin(BinOp::Div, _, _) => Err(CompileError::at(
                e.line,
                "'/' makes a decimal number in Python, and decimals aren't supported yet — \
                 use '//' for whole-number division",
            )),
            ExprKind::Bin(BinOp::FloorDiv, a, b) => {
                self.uses_floordiv = true;
                let lhs = self.int_expr(cx, a)?;
                let rhs = self.int_expr(cx, b)?;
                Ok(format!("(call $i32_floordiv {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Mod, a, b) => {
                self.uses_floormod = true;
                let lhs = self.int_expr(cx, a)?;
                let rhs = self.int_expr(cx, b)?;
                Ok(format!("(call $i32_floormod {lhs} {rhs})"))
            }
            ExprKind::Bin(op, a, b) => {
                let lhs = self.int_expr(cx, a)?;
                let rhs = self.int_expr(cx, b)?;
                let instr = match op {
                    BinOp::Add => "i32.add",
                    BinOp::Sub => "i32.sub",
                    BinOp::Mul => "i32.mul",
                    BinOp::Lt => "i32.lt_s",
                    BinOp::Le => "i32.le_s",
                    BinOp::Gt => "i32.gt_s",
                    BinOp::Ge => "i32.ge_s",
                    BinOp::Eq => "i32.eq",
                    BinOp::Ne => "i32.ne",
                    BinOp::And | BinOp::Or | BinOp::Div | BinOp::FloorDiv | BinOp::Mod => {
                        unreachable!("handled above")
                    }
                };
                Ok(format!("({instr} {lhs} {rhs})"))
            }
            ExprKind::Str(_) => Err(CompileError::at(
                e.line,
                "expected a number, found a string",
            )),
            ExprKind::Call(n, _) => Err(CompileError::at(
                e.line,
                format!("can't use the result of '{n}(...)' yet"),
            )),
        }
    }
}

fn emit_char(out: &mut Body, byte: u8) {
    out.push(format!("(call $write_char (i32.const {byte}))"));
}

/// Constant value of an integer literal (handling unary minus), if it is one.
fn const_int(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Unary(UnOp::Neg, inner) => const_int(inner).map(|v| -v),
        _ => None,
    }
}

fn require_int(e: &Expr, vars: &Vars, what: &str) -> Result<()> {
    match type_of(e, vars)? {
        Ty::Int => Ok(()),
        Ty::Str => Err(CompileError::at(
            e.line,
            format!("{what} needs to be a number, not text"),
        )),
    }
}

/// Static type of an expression, given the variables in scope.
fn type_of(e: &Expr, vars: &Vars) -> Result<Ty> {
    match &e.kind {
        ExprKind::Int(_) => Ok(Ty::Int),
        ExprKind::Str(_) => Ok(Ty::Str),
        ExprKind::Unary(_, inner) => match type_of(inner, vars)? {
            Ty::Int => Ok(Ty::Int),
            Ty::Str => Err(CompileError::at(
                e.line,
                "operator needs a number, not text",
            )),
        },
        ExprKind::Bin(_, a, b) => match (type_of(a, vars)?, type_of(b, vars)?) {
            (Ty::Int, Ty::Int) => Ok(Ty::Int),
            _ => Err(CompileError::at(
                e.line,
                "this operator needs numbers on both sides",
            )),
        },
        ExprKind::Name(n) => vars.get(n).copied().ok_or_else(|| {
            CompileError::at(
                e.line,
                format!("unknown name '{n}' (define it with `{n} = ...` first)"),
            )
        }),
        ExprKind::Call(n, _) => Err(CompileError::at(
            e.line,
            format!("can't use the result of '{n}(...)' yet"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::lex, parser::parse};

    fn compile(src: &str) -> Result<String> {
        generate(&parse(&lex(src).unwrap()).unwrap())
    }

    #[test]
    fn print_int_arithmetic() {
        let wat = compile("print(2 + 3 * 4)").unwrap();
        assert!(wat.contains("(export \"_start\")"));
        assert!(wat.contains("(i32.add (i32.const 2) (i32.mul (i32.const 3) (i32.const 4)))"));
    }

    #[test]
    fn variable_then_print() {
        let wat = compile("x = 5\nprint(x)").unwrap();
        assert!(wat.contains("(local $x i32)"));
        assert!(wat.contains("(local.set $x (i32.const 5))"));
        assert!(wat.contains("(call $write_i32 (local.get $x))"));
    }

    #[test]
    fn start_returns_exit_code() {
        let wat = compile("print(1)").unwrap();
        assert!(wat.contains("(func $_start (export \"_start\") (result i32)"));
        assert!(wat.contains("(i32.const 0)\n  )"));
    }

    #[test]
    fn if_else_emits_branches() {
        let wat = compile("x = 3\nif x < 5:\n    print(1)\nelse:\n    print(2)\n").unwrap();
        assert!(wat.contains("(if (i32.lt_s (local.get $x) (i32.const 5))"));
        assert!(wat.contains("(then"));
        assert!(wat.contains("(else"));
    }

    #[test]
    fn elif_chain_nests() {
        let src =
            "x = 2\nif x < 1:\n    print(1)\nelif x < 3:\n    print(2)\nelse:\n    print(3)\n";
        let wat = compile(src).unwrap();
        // Two `if` constructs: the outer and the elif lowered to a nested if.
        assert_eq!(wat.matches("(if ").count(), 2);
    }

    #[test]
    fn for_loop_uses_hidden_counter() {
        let wat = compile("for i in range(3):\n    print(i)\n").unwrap();
        assert!(wat.contains("(local $i i32)"));
        assert!(wat.contains("(local $.f0 i32)"));
        assert!(wat.contains("(local.set $.f0 (i32.const 0))"));
        assert!(wat.contains("(br_if $b0 (i32.ge_s (local.get $.f0) (i32.const 3)))"));
        assert!(wat.contains("(local.set $i (local.get $.f0))"));
        assert!(wat.contains("(local.set $.f0 (i32.add (local.get $.f0) (i32.const 1)))"));
    }

    #[test]
    fn for_loop_snapshots_nonconstant_end() {
        let wat = compile("n = 3\nfor i in range(0, n):\n    n = n + 1\n").unwrap();
        // The end bound is copied to a scratch local before the loop, so the
        // br_if must not read $n directly.
        assert!(wat.contains("(local.set $.t0 (local.get $n))"));
        assert!(wat.contains("(br_if $b0 (i32.ge_s (local.get $.f0) (local.get $.t0)))"));
    }

    #[test]
    fn nested_loops_get_unique_labels() {
        let src = "for i in range(2):\n    for j in range(2):\n        print(j)\n";
        let wat = compile(src).unwrap();
        assert!(wat.contains("$l0"));
        assert!(wat.contains("$l1"));
    }

    #[test]
    fn use_before_assignment_errors() {
        assert!(compile("print(x)").is_err());
    }

    #[test]
    fn codegen_errors_carry_lines() {
        let err = compile("x = 1\nprint(y)\n").unwrap_err();
        assert_eq!(err.line, Some(2));
        let err = compile("x = 1\n\nbreak\n").unwrap_err();
        assert_eq!(err.line, Some(3));
    }

    #[test]
    fn negative_step_counts_down() {
        let wat = compile("for i in range(5, 0, -1):\n    print(i)\n").unwrap();
        assert!(wat.contains("(i32.le_s (local.get $.f0) (i32.const 0))"));
        assert!(wat.contains("(i32.const -1)"));
    }

    #[test]
    fn zero_step_is_rejected() {
        assert!(compile("for i in range(0, 5, 0):\n    print(i)\n").is_err());
    }

    #[test]
    fn non_constant_step_is_rejected() {
        let err = compile("s = 1\nfor i in range(0, 5, s):\n    print(i)\n").unwrap_err();
        assert!(err.message.contains("step"));
    }

    #[test]
    fn while_emits_loop_with_negated_test() {
        let wat = compile("i = 3\nwhile i > 0:\n    i = i - 1\n").unwrap();
        assert!(wat.contains("(br_if $b0 (i32.eqz (i32.gt_s (local.get $i) (i32.const 0))))"));
        assert!(wat.contains("(br $l0)"));
    }

    #[test]
    fn break_and_continue_target_the_right_labels() {
        // In a for-loop, continue must reach the increment (the $c block),
        // and break must exit the whole loop (the $b block).
        let wat =
            compile("for i in range(3):\n    if i == 1:\n        continue\n    break\n").unwrap();
        assert!(wat.contains("(br $c0)"));
        assert!(wat.contains("(br $b0)"));
        // In a while, continue re-tests the condition (the loop head).
        let wat = compile("i = 0\nwhile i < 3:\n    i = i + 1\n    continue\n").unwrap();
        assert!(wat.contains("(br $l0)"));
    }

    #[test]
    fn break_continue_outside_loop_are_rejected() {
        assert!(compile("break\n")
            .unwrap_err()
            .message
            .contains("inside a loop"));
        assert!(compile("continue\n")
            .unwrap_err()
            .message
            .contains("inside a loop"));
        // ...including in an if that isn't inside a loop.
        assert!(compile("if 1:\n    break\n").is_err());
    }

    #[test]
    fn and_or_short_circuit_shape() {
        let wat = compile("print(2 and 1)").unwrap();
        assert!(wat.contains(
            "(if (result i32) (local.tee $.t0 (i32.const 2)) (then (i32.const 1)) (else (local.get $.t0)))"
        ));
        let wat = compile("print(4 or 2)").unwrap();
        assert!(wat.contains("(then (local.get $.t0)) (else (i32.const 2))"));
    }

    #[test]
    fn floordiv_and_mod_call_helpers() {
        let wat = compile("print(-7 // 2)\nprint(-7 % 2)").unwrap();
        assert!(wat.contains("(call $i32_floordiv (i32.const -7) (i32.const 2))"));
        assert!(wat.contains("(call $i32_floormod (i32.const -7) (i32.const 2))"));
        assert!(wat.contains("(func $i32_floordiv"));
        assert!(wat.contains("(func $i32_floormod"));
    }

    #[test]
    fn helpers_omitted_when_unused() {
        let wat = compile("print(1 + 2)").unwrap();
        assert!(!wat.contains("$i32_floordiv"));
        assert!(!wat.contains("$i32_floormod"));
    }

    #[test]
    fn true_division_is_rejected() {
        let err = compile("print(7 / 2)").unwrap_err();
        assert!(err.message.contains("//"));
    }

    #[test]
    fn out_of_range_literal_is_rejected() {
        assert!(compile("print(3000000000)").is_err());
        assert!(compile("print(-2147483649)").is_err());
        // The i32 boundary values themselves are fine.
        assert!(compile("print(2147483647)").is_ok());
        assert!(compile("print(-2147483648)").is_ok());
    }
}
