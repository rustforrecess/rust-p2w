//! WAT code generation — boxed WASM-GC value model.
//!
//! Every runtime value is a `(ref null eq)` (the universal type), following
//! the reference p2w compiler's boxed design:
//!
//! - small ints are `i31ref`; ints outside the 31-bit range spill to an
//!   `$INT` struct (the `$box`/`$unbox` helpers pick at runtime)
//! - `True`/`False` are the `$TRUE`/`$FALSE` singleton structs, so they
//!   print as `True`/`False` while still counting as 1/0 in arithmetic
//! - `print` dispatches on the runtime type via `$print_value`
//!
//! Compiler-internal loop counters and bound snapshots stay raw `i32`
//! locals — the dynamic model applies to *Python* values, not bookkeeping.
//! Conditions compile through `$truthy` (or a direct i32 comparison when the
//! expression is statically boolean-shaped).
//!
//! Output conventions mirror p2w's runnable module shape so the same browser
//! harness can execute it: the body is an exported `_start` returning an i32
//! exit code (0), and output goes through host imports `env.write_char(i32)`
//! / `env.write_i32(i32)`.
//!
//! Structure: `Gen` holds module-wide state (which runtime helpers are used);
//! `FuncCx` holds per-function state (locals, labels, loop stack) — `_start`
//! is the only function today, but `def` lands as one `FuncCx` per function.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::emit::{Body, Func, Module};
use crate::error::CompileError;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, CompileError>;

/// The universal boxed value type.
const VAL: &str = "(ref null eq)";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    /// A runtime value (int or bool today; everything dynamic eventually).
    Value,
    /// A string literal — compile-time only until strings become values.
    Str,
}

type Vars = HashMap<String, Ty>;

/// Largest/smallest ints that fit an i31ref.
const I31_MAX: i64 = (1 << 30) - 1;
const I31_MIN: i64 = -(1 << 30);

pub fn generate(stmts: &[Stmt]) -> Result<String> {
    let mut g = Gen::default();
    let mut cx = FuncCx::default();
    let mut body = Body::new();
    g.stmts(&mut cx, stmts, &mut body)?;
    body.push("(i32.const 0)");

    let mut module = Module::default();
    module.types.push("(type $INT (struct (field i32)))".into());
    module.types.push("(type $BOOL (struct (field i8)))".into());
    module
        .imports
        .push(r#"(import "env" "write_char" (func $write_char (param i32)))"#.into());
    module
        .imports
        .push(r#"(import "env" "write_i32" (func $write_i32 (param i32)))"#.into());
    module
        .globals
        .push("(global $TRUE (ref $BOOL) (struct.new $BOOL (i32.const 1)))".into());
    module
        .globals
        .push("(global $FALSE (ref $BOOL) (struct.new $BOOL (i32.const 0)))".into());

    module.funcs.push(Func {
        signature: r#"(func $_start (export "_start") (result i32)"#.into(),
        locals: cx
            .locals
            .iter()
            .map(|(name, ty)| format!("(local ${name} {ty})"))
            .collect(),
        body,
    });
    for f in runtime_helpers() {
        module.funcs.push(f);
    }
    if g.uses_floordiv {
        module.funcs.push(floordiv_helper());
    }
    if g.uses_floormod {
        module.funcs.push(floormod_helper());
    }
    Ok(module.render())
}

/// The always-present boxed-value runtime: box/unbox/bool/truthy/print.
fn runtime_helpers() -> Vec<Func> {
    let mut fs = Vec::new();

    // $box: i32 -> value (i31 when it fits, $INT struct otherwise).
    let mut b = Body::new();
    b.push("(if (result (ref null eq))");
    b.push_in(
        2,
        "(i32.eq (i32.shr_s (i32.shl (local.get $v) (i32.const 1)) (i32.const 1)) (local.get $v))",
    );
    b.push_in(1, "(then (ref.i31 (local.get $v)))");
    b.push_in(1, "(else (struct.new $INT (local.get $v))))");
    fs.push(Func {
        signature: "(func $box (param $v i32) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $unbox: value -> i32 (i31, $BOOL as 0/1, or $INT; traps on null).
    let mut b = Body::new();
    b.push("(if (result i32) (ref.test (ref i31) (local.get $r))");
    b.push_in(1, "(then (i31.get_s (ref.cast (ref i31) (local.get $r))))");
    b.push_in(1, "(else");
    b.push_in(2, "(if (result i32) (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(
        3,
        "(then (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r))))",
    );
    b.push_in(
        3,
        "(else (struct.get $INT 0 (ref.cast (ref $INT) (local.get $r)))))))",
    );
    fs.push(Func {
        signature: "(func $unbox (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $bool: i32 (0/1) -> the singleton $TRUE/$FALSE.
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (local.get $v)");
    b.push_in(1, "(then (global.get $TRUE))");
    b.push_in(1, "(else (global.get $FALSE)))");
    fs.push(Func {
        signature: "(func $bool (param $v i32) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $truthy: value -> i32 0/1 (nonzero numeric value is true).
    let mut b = Body::new();
    b.push("(i32.ne (call $unbox (local.get $r)) (i32.const 0))");
    fs.push(Func {
        signature: "(func $truthy (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $print_value: runtime type dispatch — bools as True/False, ints digits.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(if (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r)))",
    );
    b.push_in(3, "(then");
    for c in "True".bytes() {
        b.push_in(4, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(3, ")");
    b.push_in(3, "(else");
    for c in "False".bytes() {
        b.push_in(4, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(3, ")))");
    b.push_in(1, "(else (call $write_i32 (call $unbox (local.get $r)))))");
    fs.push(Func {
        signature: "(func $print_value (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    fs
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
    /// `(name, wat_type)` — Python variables are boxed values; compiler
    /// bookkeeping (loop counters, bound snapshots) stays raw i32.
    locals: Vec<(String, String)>,
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
    fn scratch_local(&mut self, ty: &str) -> String {
        let name = format!(".t{}", self.scratch);
        self.scratch += 1;
        self.locals.push((name.clone(), ty.to_string()));
        name
    }

    fn ensure_local(&mut self, name: &str) {
        if !self.vars.contains_key(name) {
            self.vars.insert(name.to_string(), Ty::Value);
            self.locals.push((name.to_string(), VAL.to_string()));
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
                if t != Ty::Value {
                    return Err(CompileError::at(
                        s.line,
                        format!(
                            "variable '{name}' must be a number for now (string variables aren't supported yet)"
                        ),
                    ));
                }
                cx.ensure_local(name);
                let value = self.value_expr(cx, expr)?;
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
                Ty::Value => {
                    let v = self.value_expr(cx, arg)?;
                    out.push(format!("(call $print_value {v})"));
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
        require_value(cond, &cx.vars, "an if-condition")?;
        let c = self.cond_i32(cx, cond)?;
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
            require_value(cond, &cx.vars, "an elif-condition")?;
            let c = self.cond_i32(cx, cond)?;
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
        require_value(cond, &cx.vars, "a while-condition")?;
        let c = self.cond_i32(cx, cond)?;
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
        require_value(start, &cx.vars, "a range start")?;
        require_value(end, &cx.vars, "a range end")?;

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

        // Counter and bounds are compiler bookkeeping: raw i32, not boxed.
        let start_wat = self.i32_expr(cx, start)?;
        // Python evaluates range() bounds once, before the loop — snapshot a
        // non-constant end so the body mutating its variables can't change
        // the iteration count.
        let end_wat = self.i32_expr(cx, end)?;
        let end_operand = if const_int(end).is_some() {
            end_wat
        } else {
            let snap = cx.scratch_local("i32");
            out.push(format!("(local.set ${snap} {end_wat})"));
            format!("(local.get ${snap})")
        };

        // Iterate a hidden counter and assign it (boxed) to the loop variable
        // at the top of each pass, so reassigning the variable in the body
        // doesn't change the iteration count (matching Python). The variable
        // is a function-level local, visible after the loop.
        let n = cx.fresh();
        let ctr = format!(".f{n}");
        cx.locals.push((ctr.clone(), "i32".to_string()));
        cx.ensure_local(var);

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
        out.push_in(
            2,
            format!("(local.set ${var} (call $box (local.get ${ctr})))"),
        );
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

    /// Generate WAT producing the boxed `(ref null eq)` value of `e`.
    fn value_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        // Fold integer constants — this is also where literals are
        // range-checked instead of silently wrapping.
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v32) => {
                    if (I31_MIN..=I31_MAX).contains(&v) {
                        Ok(format!("(ref.i31 (i32.const {v32}))"))
                    } else {
                        Ok(format!("(struct.new $INT (i32.const {v32}))"))
                    }
                }
                Err(_) => Err(CompileError::at(
                    e.line,
                    format!(
                        "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                    ),
                )),
            };
        }
        match &e.kind {
            // All integer literals (and negated ones) were folded above.
            ExprKind::Int(_) => unreachable!("integer literals are folded above"),
            ExprKind::Bool(true) => Ok("(global.get $TRUE)".into()),
            ExprKind::Bool(false) => Ok("(global.get $FALSE)".into()),
            ExprKind::Name(n) => match cx.vars.get(n) {
                Some(Ty::Value) => Ok(format!("(local.get ${n})")),
                Some(Ty::Str) => Err(CompileError::at(
                    e.line,
                    format!("'{n}' is text, not a number"),
                )),
                None => Err(CompileError::at(e.line, format!("unknown name '{n}'"))),
            },
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.i32_expr(cx, inner)?;
                Ok(format!("(call $box (i32.sub (i32.const 0) {v}))"))
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let c = self.cond_i32(cx, inner)?;
                Ok(format!("(call $bool (i32.eqz {c}))"))
            }
            ExprKind::Bin(BinOp::And, a, b) => {
                // Python value semantics with short-circuit: `a and b` is `a`
                // if a is falsy, else `b` (b unevaluated when a is falsy).
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let t = cx.scratch_local(VAL);
                Ok(format!(
                    "(if (result (ref null eq)) (call $truthy (local.tee ${t} {lhs})) (then {rhs}) (else (local.get ${t})))"
                ))
            }
            ExprKind::Bin(BinOp::Or, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let t = cx.scratch_local(VAL);
                Ok(format!(
                    "(if (result (ref null eq)) (call $truthy (local.tee ${t} {lhs})) (then (local.get ${t})) (else {rhs}))"
                ))
            }
            ExprKind::Bin(BinOp::Div, _, _) => Err(CompileError::at(
                e.line,
                "'/' makes a decimal number in Python, and decimals aren't supported yet — \
                 use '//' for whole-number division",
            )),
            ExprKind::Bin(BinOp::FloorDiv, a, b) => {
                self.uses_floordiv = true;
                let lhs = self.i32_expr(cx, a)?;
                let rhs = self.i32_expr(cx, b)?;
                Ok(format!("(call $box (call $i32_floordiv {lhs} {rhs}))"))
            }
            ExprKind::Bin(BinOp::Mod, a, b) => {
                self.uses_floormod = true;
                let lhs = self.i32_expr(cx, a)?;
                let rhs = self.i32_expr(cx, b)?;
                Ok(format!("(call $box (call $i32_floormod {lhs} {rhs}))"))
            }
            ExprKind::Bin(op, a, b) => {
                let lhs = self.i32_expr(cx, a)?;
                let rhs = self.i32_expr(cx, b)?;
                let arith = |instr: &str| format!("(call $box ({instr} {lhs} {rhs}))");
                let cmp = |instr: &str| format!("(call $bool ({instr} {lhs} {rhs}))");
                Ok(match op {
                    BinOp::Add => arith("i32.add"),
                    BinOp::Sub => arith("i32.sub"),
                    BinOp::Mul => arith("i32.mul"),
                    BinOp::Lt => cmp("i32.lt_s"),
                    BinOp::Le => cmp("i32.le_s"),
                    BinOp::Gt => cmp("i32.gt_s"),
                    BinOp::Ge => cmp("i32.ge_s"),
                    BinOp::Eq => cmp("i32.eq"),
                    BinOp::Ne => cmp("i32.ne"),
                    BinOp::And | BinOp::Or | BinOp::Div | BinOp::FloorDiv | BinOp::Mod => {
                        unreachable!("handled above")
                    }
                })
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

    /// Generate WAT producing the raw i32 of `e` — a constant directly, a
    /// statically boolean expression as 0/1, anything else via `$unbox`.
    fn i32_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v32) => Ok(format!("(i32.const {v32})")),
                Err(_) => Err(CompileError::at(
                    e.line,
                    format!(
                        "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                    ),
                )),
            };
        }
        Ok(format!("(call $unbox {})", self.value_expr(cx, e)?))
    }

    /// Generate WAT producing an i32 condition (0 = false). Comparisons and
    /// `not` skip the boxed-bool round-trip.
    fn cond_i32(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        match &e.kind {
            ExprKind::Bool(v) => Ok(format!("(i32.const {})", *v as i32)),
            ExprKind::Unary(UnOp::Not, inner) => {
                let c = self.cond_i32(cx, inner)?;
                Ok(format!("(i32.eqz {c})"))
            }
            ExprKind::Bin(op, a, b) if cmp_instr(*op).is_some() => {
                let lhs = self.i32_expr(cx, a)?;
                let rhs = self.i32_expr(cx, b)?;
                Ok(format!("({} {lhs} {rhs})", cmp_instr(*op).unwrap()))
            }
            _ => Ok(format!("(call $truthy {})", self.value_expr(cx, e)?)),
        }
    }
}

fn cmp_instr(op: BinOp) -> Option<&'static str> {
    Some(match op {
        BinOp::Lt => "i32.lt_s",
        BinOp::Le => "i32.le_s",
        BinOp::Gt => "i32.gt_s",
        BinOp::Ge => "i32.ge_s",
        BinOp::Eq => "i32.eq",
        BinOp::Ne => "i32.ne",
        _ => return None,
    })
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

fn require_value(e: &Expr, vars: &Vars, what: &str) -> Result<()> {
    match type_of(e, vars)? {
        Ty::Value => Ok(()),
        Ty::Str => Err(CompileError::at(
            e.line,
            format!("{what} needs to be a number, not text"),
        )),
    }
}

/// Static type of an expression, given the variables in scope.
fn type_of(e: &Expr, vars: &Vars) -> Result<Ty> {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Bool(_) => Ok(Ty::Value),
        ExprKind::Str(_) => Ok(Ty::Str),
        ExprKind::Unary(_, inner) => match type_of(inner, vars)? {
            Ty::Value => Ok(Ty::Value),
            Ty::Str => Err(CompileError::at(
                e.line,
                "operator needs a number, not text",
            )),
        },
        ExprKind::Bin(_, a, b) => match (type_of(a, vars)?, type_of(b, vars)?) {
            (Ty::Value, Ty::Value) => Ok(Ty::Value),
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
        // Constant operands feed arithmetic raw; the product is boxed and
        // unboxed on the way into the addition.
        assert!(wat.contains("(i32.mul (i32.const 3) (i32.const 4))"));
        assert!(wat.contains("(i32.add (i32.const 2)"));
        assert!(wat.contains("(call $print_value"));
    }

    #[test]
    fn variable_then_print() {
        let wat = compile("x = 5\nprint(x)").unwrap();
        assert!(wat.contains("(local $x (ref null eq))"));
        assert!(wat.contains("(local.set $x (ref.i31 (i32.const 5)))"));
        assert!(wat.contains("(call $print_value (local.get $x))"));
    }

    #[test]
    fn booleans_are_singletons_not_ints() {
        let wat = compile("x = True\nprint(x, False)").unwrap();
        assert!(wat.contains("(local.set $x (global.get $TRUE))"));
        assert!(wat.contains("(call $print_value (global.get $FALSE))"));
        // The runtime knows how to spell them.
        assert!(wat.contains("(type $BOOL (struct (field i8)))"));
    }

    #[test]
    fn big_literals_spill_to_int_struct() {
        let wat = compile("print(2147483647)").unwrap();
        assert!(wat.contains("(struct.new $INT (i32.const 2147483647))"));
        let wat = compile("print(5)").unwrap();
        assert!(wat.contains("(ref.i31 (i32.const 5))"));
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
        // Comparison conditions skip the boxed-bool round-trip.
        assert!(wat.contains("(if (i32.lt_s (call $unbox (local.get $x)) (i32.const 5))"));
        assert!(wat.contains("(then"));
        assert!(wat.contains("(else"));
    }

    #[test]
    fn elif_chain_nests() {
        let src =
            "x = 2\nif x < 1:\n    print(1)\nelif x < 3:\n    print(2)\nelse:\n    print(3)\n";
        let wat = compile(src).unwrap();
        // Two conditions compile to two direct comparisons in _start.
        assert_eq!(wat.matches("(if (i32.lt_s").count(), 2);
    }

    #[test]
    fn for_loop_uses_raw_i32_counter() {
        let wat = compile("for i in range(3):\n    print(i)\n").unwrap();
        assert!(wat.contains("(local $i (ref null eq))"));
        assert!(wat.contains("(local $.f0 i32)"));
        assert!(wat.contains("(local.set $.f0 (i32.const 0))"));
        assert!(wat.contains("(br_if $b0 (i32.ge_s (local.get $.f0) (i32.const 3)))"));
        // The Python-visible loop variable gets the boxed counter.
        assert!(wat.contains("(local.set $i (call $box (local.get $.f0)))"));
        assert!(wat.contains("(local.set $.f0 (i32.add (local.get $.f0) (i32.const 1)))"));
    }

    #[test]
    fn for_loop_snapshots_nonconstant_end() {
        let wat = compile("n = 3\nfor i in range(0, n):\n    n = n + 1\n").unwrap();
        // The end bound is unboxed once into an i32 scratch local.
        assert!(wat.contains("(local $.t0 i32)"));
        assert!(wat.contains("(local.set $.t0 (call $unbox (local.get $n)))"));
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
        assert!(wat.contains(
            "(br_if $b0 (i32.eqz (i32.gt_s (call $unbox (local.get $i)) (i32.const 0))))"
        ));
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
            "(if (result (ref null eq)) (call $truthy (local.tee $.t0 (ref.i31 (i32.const 2)))) (then (ref.i31 (i32.const 1))) (else (local.get $.t0)))"
        ));
        let wat = compile("print(4 or 2)").unwrap();
        assert!(wat.contains("(then (local.get $.t0)) (else (ref.i31 (i32.const 2)))"));
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
