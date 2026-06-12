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
//! deciding operand, like Python), `if`/`elif`/`else`, and
//! `for i in range(...)` with a constant step (negative steps count down).
//! Integers (and booleans, as 0/1) are i32. Variables and loop counters
//! compile to WASM locals (Python has no block scope, so all locals live at
//! function level). Compiler-internal locals are prefixed `.` (valid in WAT
//! identifiers, impossible in Python ones, so they can't collide).

use crate::ast::{BinOp, Expr, Stmt, UnOp};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Str,
}

type Vars = HashMap<String, Ty>;

pub fn generate(stmts: &[Stmt]) -> Result<String, String> {
    let mut g = Gen::default();
    let body = g.stmts(stmts)?;

    let mut locals_decl = String::new();
    for name in &g.locals {
        locals_decl.push_str(&format!("    (local ${name} i32)\n"));
    }

    let mut helpers = String::new();
    if g.uses_floordiv {
        helpers.push_str(FLOORDIV_HELPER);
    }
    if g.uses_floormod {
        helpers.push_str(FLOORMOD_HELPER);
    }

    Ok(format!(
        "(module\n  \
         (import \"env\" \"write_char\" (func $write_char (param i32)))\n  \
         (import \"env\" \"write_i32\" (func $write_i32 (param i32)))\n  \
         (func $_start (export \"_start\") (result i32)\n{locals_decl}{body}    (i32.const 0)\n  )\n{helpers})\n"
    ))
}

/// Python floor division: truncating `i32.div_s` adjusted by -1 when the
/// signs differ and the division isn't exact (`-7 // 2` is -4, not -3).
const FLOORDIV_HELPER: &str = "  (func $i32_floordiv (param $a i32) (param $b i32) (result i32)\n    \
     (local $q i32)\n    \
     (local.set $q (i32.div_s (local.get $a) (local.get $b)))\n    \
     (if (i32.and\n          \
          (i32.ne (i32.rem_s (local.get $a) (local.get $b)) (i32.const 0))\n          \
          (i32.ne (i32.lt_s (local.get $a) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))\n      \
       (then (local.set $q (i32.sub (local.get $q) (i32.const 1)))))\n    \
     (local.get $q)\n  )\n";

/// Python modulo: the result takes the sign of the divisor (`-7 % 2` is 1).
const FLOORMOD_HELPER: &str = "  (func $i32_floormod (param $a i32) (param $b i32) (result i32)\n    \
     (local $r i32)\n    \
     (local.set $r (i32.rem_s (local.get $a) (local.get $b)))\n    \
     (if (i32.and\n          \
          (i32.ne (local.get $r) (i32.const 0))\n          \
          (i32.ne (i32.lt_s (local.get $r) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))\n      \
       (then (local.set $r (i32.add (local.get $r) (local.get $b)))))\n    \
     (local.get $r)\n  )\n";

#[derive(Default)]
struct Gen {
    vars: Vars,
    locals: Vec<String>,
    label: usize,
    scratch: usize,
    uses_floordiv: bool,
    uses_floormod: bool,
    /// Enclosing loops as `(break_label, continue_label)`, innermost last.
    /// In a `for`, continue targets the inner `$c` block so the counter
    /// increment still runs; in a `while`, it targets the loop head (re-test).
    loops: Vec<(String, String)>,
}

impl Gen {
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

    fn stmts(&mut self, stmts: &[Stmt]) -> Result<String, String> {
        let mut out = String::new();
        for s in stmts {
            out.push_str(&self.stmt(s)?);
        }
        Ok(out)
    }

    fn stmt(&mut self, s: &Stmt) -> Result<String, String> {
        match s {
            Stmt::Assign(name, expr) => {
                let t = type_of(expr, &self.vars)?;
                if t != Ty::Int {
                    return Err(format!(
                        "variable '{name}' must be a number for now (string variables aren't supported yet)"
                    ));
                }
                self.ensure_local(name, Ty::Int);
                let value = self.int_expr(expr)?;
                Ok(format!("    (local.set ${name} {value})\n"))
            }
            Stmt::Expr(Expr::Call(name, args)) if name == "print" => self.gen_print(args),
            Stmt::Expr(Expr::Call(name, _)) => Err(format!(
                "only print(...) is supported so far — '{name}(...)' isn't implemented yet"
            )),
            Stmt::Expr(_) => {
                Err("a bare value on its own line has no effect; did you mean print(...)?".into())
            }
            Stmt::If {
                cond,
                body,
                elifs,
                else_body,
            } => self.gen_if(cond, body, elifs, else_body),
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => self.gen_for(var, start, end, step, body),
            Stmt::While { cond, body } => self.gen_while(cond, body),
            Stmt::Break => match self.loops.last() {
                Some((brk, _)) => Ok(format!("    (br {brk})\n")),
                None => Err("'break' can only be used inside a loop".into()),
            },
            Stmt::Continue => match self.loops.last() {
                Some((_, cont)) => Ok(format!("    (br {cont})\n")),
                None => Err("'continue' can only be used inside a loop".into()),
            },
        }
    }

    fn gen_while(&mut self, cond: &Expr, body: &[Stmt]) -> Result<String, String> {
        require_int(cond, &self.vars, "a while-condition")?;
        let c = self.int_expr(cond)?;
        let n = self.fresh();
        self.loops.push((format!("$b{n}"), format!("$l{n}")));
        let body_wat = self.stmts(body);
        self.loops.pop();
        let body_wat = body_wat?;
        Ok(format!(
            "    (block $b{n}\n\
             \x20     (loop $l{n}\n\
             \x20       (br_if $b{n} (i32.eqz {c}))\n\
             {body_wat}\
             \x20       (br $l{n})))\n"
        ))
    }

    fn gen_print(&mut self, args: &[Expr]) -> Result<String, String> {
        let mut out = String::new();
        for (idx, arg) in args.iter().enumerate() {
            if idx > 0 {
                emit_char(&mut out, b' ');
            }
            match type_of(arg, &self.vars)? {
                Ty::Int => {
                    let expr_wat = self.int_expr(arg)?;
                    out.push_str(&format!("    (call $write_i32 {expr_wat})\n"));
                }
                Ty::Str => {
                    if let Expr::Str(s) = arg {
                        for byte in s.bytes() {
                            emit_char(&mut out, byte);
                        }
                    } else {
                        return Err("only string literals can be printed so far".into());
                    }
                }
            }
        }
        emit_char(&mut out, b'\n');
        Ok(out)
    }

    fn gen_if(
        &mut self,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<String, String> {
        require_int(cond, &self.vars, "an if-condition")?;
        let c = self.int_expr(cond)?;
        let then_body = self.stmts(body)?;
        let else_chain = self.gen_else_chain(elifs, else_body)?;
        Ok(format!(
            "    (if {c} (then\n{then_body}    ){else_chain})\n"
        ))
    }

    fn gen_else_chain(
        &mut self,
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<String, String> {
        if let Some(((cond, body), rest)) = elifs.split_first() {
            require_int(cond, &self.vars, "an elif-condition")?;
            let c = self.int_expr(cond)?;
            let then_body = self.stmts(body)?;
            let inner = self.gen_else_chain(rest, else_body)?;
            Ok(format!(
                " (else\n    (if {c} (then\n{then_body}    ){inner})\n    )"
            ))
        } else if let Some(body) = else_body {
            let b = self.stmts(body)?;
            Ok(format!(" (else\n{b}    )"))
        } else {
            Ok(String::new())
        }
    }

    fn gen_for(
        &mut self,
        var: &str,
        start: &Expr,
        end: &Expr,
        step: &Expr,
        body: &[Stmt],
    ) -> Result<String, String> {
        require_int(start, &self.vars, "a range start")?;
        require_int(end, &self.vars, "a range end")?;

        // A runtime step would need a sign-aware termination check; until that
        // lands, only constant steps are accepted (so the direction is known).
        let step_v = match const_int(step) {
            Some(0) => return Err("range() step can't be zero".into()),
            Some(v) => i32::try_from(v).map_err(|_| "range() step is too big".to_string())?,
            None => return Err("the range() step must be a plain number for now".into()),
        };
        let done_cmp = if step_v > 0 { "i32.ge_s" } else { "i32.le_s" };

        let start_wat = self.int_expr(start)?;
        // Python evaluates range() bounds once, before the loop — snapshot a
        // non-constant end so the body mutating its variables can't change
        // the iteration count.
        let end_wat = self.int_expr(end)?;
        let mut pre = String::new();
        let end_operand = if const_int(end).is_some() {
            end_wat
        } else {
            let snap = self.scratch_local();
            pre.push_str(&format!("    (local.set ${snap} {end_wat})\n"));
            format!("(local.get ${snap})")
        };

        // Iterate a hidden counter and assign it to the loop variable at the
        // top of each pass, so reassigning the variable in the body doesn't
        // change the iteration count (matching Python). The variable is a
        // function-level local, visible after the loop.
        let n = self.fresh();
        let ctr = format!(".f{n}");
        self.locals.push(ctr.clone());
        self.ensure_local(var, Ty::Int);
        self.loops.push((format!("$b{n}"), format!("$c{n}")));
        let body_wat = self.stmts(body);
        self.loops.pop();
        let body_wat = body_wat?;

        Ok(format!(
            "{pre}    (local.set ${ctr} {start_wat})\n\
                 (block $b{n}\n\
             \x20     (loop $l{n}\n\
             \x20       (br_if $b{n} ({done_cmp} (local.get ${ctr}) {end_operand}))\n\
             \x20       (local.set ${var} (local.get ${ctr}))\n\
             \x20       (block $c{n}\n\
             {body_wat}\
             \x20       )\n\
             \x20       (local.set ${ctr} (i32.add (local.get ${ctr}) (i32.const {step_v})))\n\
             \x20       (br $l{n})))\n"
        ))
    }

    /// Generate WAT that pushes the i32 value of an integer (or boolean)
    /// expression.
    fn int_expr(&mut self, e: &Expr) -> Result<String, String> {
        // Fold constants first — this is also where literals are range-checked
        // instead of silently wrapping (3000000000 must not become a negative).
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v) => Ok(format!("(i32.const {v})")),
                Err(_) => Err(format!(
                    "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                )),
            };
        }
        match e {
            // All literals (and negated literals) were handled by const_int.
            Expr::Int(_) => unreachable!("integer literals are folded above"),
            Expr::Name(n) => match self.vars.get(n) {
                Some(Ty::Int) => Ok(format!("(local.get ${n})")),
                Some(Ty::Str) => Err(format!("'{n}' is text, not a number")),
                None => Err(format!("unknown name '{n}'")),
            },
            Expr::Unary(UnOp::Neg, inner) => {
                let v = self.int_expr(inner)?;
                Ok(format!("(i32.sub (i32.const 0) {v})"))
            }
            Expr::Unary(UnOp::Not, inner) => {
                let v = self.int_expr(inner)?;
                Ok(format!("(i32.eqz {v})"))
            }
            Expr::Bin(BinOp::And, a, b) => {
                // Python value semantics with short-circuit: `a and b` is `a`
                // if a is falsy, else `b` (b unevaluated when a is falsy).
                let lhs = self.int_expr(a)?;
                let rhs = self.int_expr(b)?;
                let t = self.scratch_local();
                Ok(format!(
                    "(if (result i32) (local.tee ${t} {lhs}) (then {rhs}) (else (local.get ${t})))"
                ))
            }
            Expr::Bin(BinOp::Or, a, b) => {
                let lhs = self.int_expr(a)?;
                let rhs = self.int_expr(b)?;
                let t = self.scratch_local();
                Ok(format!(
                    "(if (result i32) (local.tee ${t} {lhs}) (then (local.get ${t})) (else {rhs}))"
                ))
            }
            Expr::Bin(BinOp::Div, _, _) => Err(
                "'/' makes a decimal number in Python, and decimals aren't supported yet — \
                 use '//' for whole-number division"
                    .into(),
            ),
            Expr::Bin(BinOp::FloorDiv, a, b) => {
                self.uses_floordiv = true;
                let lhs = self.int_expr(a)?;
                let rhs = self.int_expr(b)?;
                Ok(format!("(call $i32_floordiv {lhs} {rhs})"))
            }
            Expr::Bin(BinOp::Mod, a, b) => {
                self.uses_floormod = true;
                let lhs = self.int_expr(a)?;
                let rhs = self.int_expr(b)?;
                Ok(format!("(call $i32_floormod {lhs} {rhs})"))
            }
            Expr::Bin(op, a, b) => {
                let lhs = self.int_expr(a)?;
                let rhs = self.int_expr(b)?;
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
            Expr::Str(_) => Err("expected a number, found a string".into()),
            Expr::Call(n, _) => Err(format!("can't use the result of '{n}(...)' yet")),
        }
    }
}

fn emit_char(out: &mut String, byte: u8) {
    out.push_str(&format!("    (call $write_char (i32.const {}))\n", byte));
}

/// Constant value of an integer literal (handling unary minus), if it is one.
fn const_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Int(n) => Some(*n),
        Expr::Unary(UnOp::Neg, inner) => const_int(inner).map(|v| -v),
        _ => None,
    }
}

fn require_int(e: &Expr, vars: &Vars, what: &str) -> Result<(), String> {
    match type_of(e, vars)? {
        Ty::Int => Ok(()),
        Ty::Str => Err(format!("{what} needs to be a number, not text")),
    }
}

/// Static type of an expression, given the variables in scope.
fn type_of(e: &Expr, vars: &Vars) -> Result<Ty, String> {
    match e {
        Expr::Int(_) => Ok(Ty::Int),
        Expr::Str(_) => Ok(Ty::Str),
        Expr::Unary(_, inner) => match type_of(inner, vars)? {
            Ty::Int => Ok(Ty::Int),
            Ty::Str => Err("operator needs a number, not text".into()),
        },
        Expr::Bin(_, a, b) => match (type_of(a, vars)?, type_of(b, vars)?) {
            (Ty::Int, Ty::Int) => Ok(Ty::Int),
            _ => Err("this operator needs numbers on both sides".into()),
        },
        Expr::Name(n) => vars
            .get(n)
            .copied()
            .ok_or_else(|| format!("unknown name '{n}' (define it with `{n} = ...` first)")),
        Expr::Call(n, _) => Err(format!("can't use the result of '{n}(...)' yet")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::lex, parser::parse};

    fn compile(src: &str) -> Result<String, String> {
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
        assert!(wat.contains("(if (i32.lt_s (local.get $x) (i32.const 5)) (then"));
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
        assert!(err.contains("step"));
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
        assert!(compile("break\n").unwrap_err().contains("inside a loop"));
        assert!(compile("continue\n").unwrap_err().contains("inside a loop"));
        // ...including in an if that isn't inside a loop.
        assert!(compile("if 1:\n    break\n").is_err());
    }

    #[test]
    fn and_or_short_circuit_shape() {
        let wat = compile("print(2 and 1)").unwrap();
        assert!(wat.contains("(if (result i32) (local.tee $.t0 (i32.const 2)) (then (i32.const 1)) (else (local.get $.t0)))"));
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
        assert!(err.contains("//"));
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
