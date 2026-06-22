//! AST -> textual LLVM IR — the native Pico 2 W backend emitter (see
//! `PICO_BACKEND.md`).
//!
//! Like `codegen.rs` hand-emits WAT, this hand-emits LLVM IR as text, so the
//! crate needs no LLVM build dependency; turning the `.ll` into an RP2350 binary
//! (`llc`/`lld`/`picotool`) is a later, toolchain-gated phase.
//!
//! **Value model (phase 3):** every Python value is a uniform tagged `i64`, and
//! this emitter is **representation-agnostic** — it never assumes a bit layout,
//! it only *calls* a small **runtime ABI** of `p2w_*` functions (declared at the
//! top of the module). The device runtime owns the actual rep + allocator. This
//! is the same "box values + call runtime ops" split the WASM backend uses, and
//! it's what lets strings (and later lists/dicts) drop in without touching the
//! control-flow machinery.
//!
//! Supported now: ints, bools, strings, `print`, arithmetic (`+ - * / // % **`),
//! comparisons, `not`, `if`/`elif`/`else`, `while`, counted `for` (literal step),
//! `break`/`continue`, and user functions (`def`/`return`/calls, incl. recursion).
//! Not yet (clean errors): `and`/`or`, lists/dicts/indexing/methods, for-each,
//! default args, floats-as-literals beyond what the runtime handles.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// The runtime ABI the emitted module depends on (implemented by the device
/// runtime). Declared at the top of every module.
const RUNTIME_DECLS: &str = "\
; runtime ABI — values are opaque tagged i64; the device runtime owns the rep.
declare i64 @p2w_int(i32)
declare i64 @p2w_bool(i1)
declare i64 @p2w_none()
declare i64 @p2w_str(ptr, i32)
declare i64 @p2w_add(i64, i64)
declare i64 @p2w_sub(i64, i64)
declare i64 @p2w_mul(i64, i64)
declare i64 @p2w_div(i64, i64)
declare i64 @p2w_floordiv(i64, i64)
declare i64 @p2w_mod(i64, i64)
declare i64 @p2w_pow(i64, i64)
declare i64 @p2w_neg(i64)
declare i64 @p2w_lt(i64, i64)
declare i64 @p2w_le(i64, i64)
declare i64 @p2w_gt(i64, i64)
declare i64 @p2w_ge(i64, i64)
declare i64 @p2w_eq(i64, i64)
declare i64 @p2w_ne(i64, i64)
declare i64 @p2w_not(i64)
declare i1 @p2w_truthy(i64)
declare void @p2w_print(i64)
";

/// Emit a textual LLVM IR module for the supported subset of `stmts`, or an
/// error naming the first unsupported construct.
pub fn emit_llvm_ir(stmts: &[Stmt]) -> Result<String, String> {
    let mut funcs = HashSet::new();
    for s in stmts {
        if let StmtKind::Def { name, .. } = &s.kind {
            funcs.insert(name.clone());
        }
    }

    let mut globals = String::new();
    let mut defs = String::new();

    for s in stmts {
        if let StmtKind::Def {
            name,
            params,
            defaults,
            body,
            ..
        } = &s.kind
        {
            if !defaults.is_empty() {
                return Err(format!(
                    "line {}: default arguments aren't in the native backend yet",
                    s.line
                ));
            }
            let (def, g) = emit_function(name, params, body, &funcs)?;
            defs.push_str(&def);
            defs.push('\n');
            globals.push_str(&g);
        }
    }

    let top: Vec<&Stmt> = stmts
        .iter()
        .filter(|s| !matches!(s.kind, StmtKind::Def { .. }))
        .collect();
    let (main_def, main_g) = emit_main(&top, &funcs)?;
    globals.push_str(&main_g);

    Ok(format!(
        "; LLVM IR — rust-p2w native (Pico) backend\n{RUNTIME_DECLS}\n{globals}\n{defs}{main_def}"
    ))
}

fn emit_function(
    name: &str,
    params: &[String],
    body: &[Stmt],
    funcs: &HashSet<String>,
) -> Result<(String, String), String> {
    let mut f = FuncEmitter::new(funcs, name);
    for (i, p) in params.iter().enumerate() {
        let ptr = f.var_slot(p);
        f.line(&format!("store i64 %a{i}, ptr {ptr}"));
    }
    f.block(body)?;
    if !f.terminated {
        let none = f.temp();
        f.line(&format!("{none} = call i64 @p2w_none()"));
        f.body.push_str(&format!("  ret i64 {none}\n"));
    }
    let sig: Vec<String> = (0..params.len()).map(|i| format!("i64 %a{i}")).collect();
    let def = format!(
        "define i64 @{name}({}) {{\nentry:\n{}{}}}\n",
        sig.join(", "),
        f.allocas,
        f.body
    );
    Ok((def, f.globals))
}

fn emit_main(top: &[&Stmt], funcs: &HashSet<String>) -> Result<(String, String), String> {
    let mut f = FuncEmitter::new(funcs, "main");
    for s in top {
        f.stmt(s)?;
    }
    if !f.terminated {
        f.body.push_str("  ret i32 0\n");
    }
    let def = format!(
        "define i32 @main() {{\nentry:\n{}{}}}\n",
        f.allocas, f.body
    );
    Ok((def, f.globals))
}

/// Per-function emission state. Values are tagged `i64`; variables are
/// entry-block `alloca`s; control flow uses labelled basic blocks.
struct FuncEmitter<'a> {
    funcs: &'a HashSet<String>,
    /// Prefix for this function's string-constant globals (unique per function).
    gprefix: String,
    /// Module-level string-constant definitions produced by this function.
    globals: String,
    gcount: usize,
    /// Entry-block `alloca`s (kept separate so they sit at the top of `entry`).
    allocas: String,
    body: String,
    next_tmp: usize,
    next_label: usize,
    vars: Vec<String>,
    /// (continue-target, break-target) for each enclosing loop.
    loops: Vec<(String, String)>,
    terminated: bool,
}

impl<'a> FuncEmitter<'a> {
    fn new(funcs: &'a HashSet<String>, gprefix: &str) -> Self {
        FuncEmitter {
            funcs,
            gprefix: gprefix.to_string(),
            globals: String::new(),
            gcount: 0,
            allocas: String::new(),
            body: String::new(),
            next_tmp: 0,
            next_label: 0,
            vars: Vec::new(),
            loops: Vec::new(),
            terminated: false,
        }
    }

    fn temp(&mut self) -> String {
        let t = format!("%t{}", self.next_tmp);
        self.next_tmp += 1;
        t
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let l = format!("{prefix}{}", self.next_label);
        self.next_label += 1;
        l
    }

    fn line(&mut self, s: &str) {
        if self.terminated {
            let dead = self.fresh_label("dead");
            self.body.push_str(&format!("{dead}:\n"));
            self.terminated = false;
        }
        self.body.push_str("  ");
        self.body.push_str(s);
        self.body.push('\n');
    }

    fn terminator(&mut self, s: &str) {
        self.line(s);
        self.terminated = true;
    }

    fn place_label(&mut self, l: &str) {
        if !self.terminated {
            self.body.push_str(&format!("  br label %{l}\n"));
        }
        self.body.push_str(&format!("{l}:\n"));
        self.terminated = false;
    }

    fn br_to(&mut self, l: &str) {
        if !self.terminated {
            self.terminator(&format!("br label %{l}"));
        }
    }

    fn var_slot(&mut self, name: &str) -> String {
        let ptr = format!("%v_{name}");
        if !self.vars.iter().any(|v| v == name) {
            self.allocas.push_str(&format!("  {ptr} = alloca i64\n"));
            self.vars.push(name.to_string());
        }
        ptr
    }

    /// Call a runtime function that returns a value, into a fresh temp.
    fn call_value(&mut self, sig: &str) -> String {
        let t = self.temp();
        self.line(&format!("{t} = {sig}"));
        t
    }

    fn block(&mut self, stmts: &[Stmt]) -> Result<(), String> {
        for s in stmts {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                s.line
            ))
        };
        match &s.kind {
            StmtKind::Assign(name, value) => {
                let v = self.expr(value)?;
                let ptr = self.var_slot(name);
                self.line(&format!("store i64 {v}, ptr {ptr}"));
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return nope("print() with multiple arguments");
                    }
                    let v = self.expr(&args[0])?;
                    self.line(&format!("call void @p2w_print(i64 {v})"));
                    Ok(())
                }
                ExprKind::Call(..) => {
                    self.expr(e)?;
                    Ok(())
                }
                _ => nope("this statement"),
            },
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.expr(e)?,
                    None => self.call_value("call i64 @p2w_none()"),
                };
                self.terminator(&format!("ret i64 {v}"));
                Ok(())
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => self.emit_if(cond, body, elifs, else_body.as_deref()),
            StmtKind::While { cond, body } => self.emit_while(cond, body),
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => self.emit_for(var, start, end, step, body),
            StmtKind::Break => {
                let (_, brk) = self
                    .loops
                    .last()
                    .ok_or_else(|| format!("line {}: 'break' outside a loop", s.line))?;
                let brk = brk.clone();
                self.terminator(&format!("br label %{brk}"));
                Ok(())
            }
            StmtKind::Continue => {
                let (cont, _) = self
                    .loops
                    .last()
                    .ok_or_else(|| format!("line {}: 'continue' outside a loop", s.line))?;
                let cont = cont.clone();
                self.terminator(&format!("br label %{cont}"));
                Ok(())
            }
            _ => nope("this statement"),
        }
    }

    fn emit_if(
        &mut self,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: Option<&[Stmt]>,
    ) -> Result<(), String> {
        let end = self.fresh_label("ifend");
        let mut branches: Vec<(&Expr, &[Stmt])> = vec![(cond, body)];
        for (c, b) in elifs {
            branches.push((c, b));
        }
        for (c, b) in branches {
            let cv = self.cond_i1(c)?;
            let then = self.fresh_label("then");
            let next = self.fresh_label("elif");
            self.terminator(&format!("br i1 {cv}, label %{then}, label %{next}"));
            self.place_label(&then);
            self.block(b)?;
            self.br_to(&end);
            self.place_label(&next);
        }
        if let Some(eb) = else_body {
            self.block(eb)?;
        }
        self.br_to(&end);
        self.place_label(&end);
        Ok(())
    }

    fn emit_while(&mut self, cond: &Expr, body: &[Stmt]) -> Result<(), String> {
        let head = self.fresh_label("whead");
        let body_l = self.fresh_label("wbody");
        let end = self.fresh_label("wend");
        self.br_to(&head);
        self.place_label(&head);
        let cv = self.cond_i1(cond)?;
        self.terminator(&format!("br i1 {cv}, label %{body_l}, label %{end}"));
        self.place_label(&body_l);
        self.loops.push((head.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&head);
        self.place_label(&end);
        Ok(())
    }

    fn emit_for(
        &mut self,
        var: &str,
        start: &Expr,
        end_expr: &Expr,
        step: &Expr,
        body: &[Stmt],
    ) -> Result<(), String> {
        let step_lit = step_literal(step)
            .ok_or_else(|| "the native backend needs a literal range() step yet".to_string())?;
        if step_lit == 0 {
            return Err("range() step must not be zero".to_string());
        }
        let start_v = self.expr(start)?;
        let end_v = self.expr(end_expr)?;
        let step_v = self.call_value(&format!("call i64 @p2w_int(i32 {step_lit})"));
        let slot = self.var_slot(var);
        self.line(&format!("store i64 {start_v}, ptr {slot}"));

        let head = self.fresh_label("fhead");
        let body_l = self.fresh_label("fbody");
        let cont = self.fresh_label("fcont");
        let end = self.fresh_label("fend");

        self.br_to(&head);
        self.place_label(&head);
        let iv = self.temp();
        self.line(&format!("{iv} = load i64, ptr {slot}"));
        // Ascending loops compare with `<`, descending with `>` (Python range).
        let cmp_fn = if step_lit > 0 { "p2w_lt" } else { "p2w_gt" };
        let cmpv = self.call_value(&format!("call i64 @{cmp_fn}(i64 {iv}, i64 {end_v})"));
        let cond = self.temp();
        self.line(&format!("{cond} = call i1 @p2w_truthy(i64 {cmpv})"));
        self.terminator(&format!("br i1 {cond}, label %{body_l}, label %{end}"));

        self.place_label(&body_l);
        self.loops.push((cont.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&cont);

        self.place_label(&cont);
        let cur = self.temp();
        self.line(&format!("{cur} = load i64, ptr {slot}"));
        let inc = self.call_value(&format!("call i64 @p2w_add(i64 {cur}, i64 {step_v})"));
        self.line(&format!("store i64 {inc}, ptr {slot}"));
        self.br_to(&head);

        self.place_label(&end);
        Ok(())
    }

    /// Evaluate a condition to an `i1` via the runtime's truthiness.
    fn cond_i1(&mut self, cond: &Expr) -> Result<String, String> {
        let v = self.expr(cond)?;
        let t = self.temp();
        self.line(&format!("{t} = call i1 @p2w_truthy(i64 {v})"));
        Ok(t)
    }

    /// Evaluate an expression to a tagged-`i64` value operand.
    fn expr(&mut self, e: &Expr) -> Result<String, String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                e.line
            ))
        };
        match &e.kind {
            ExprKind::Int(n) => Ok(self.call_value(&format!("call i64 @p2w_int(i32 {})", *n as i32))),
            ExprKind::Bool(b) => {
                Ok(self.call_value(&format!("call i64 @p2w_bool(i1 {})", if *b { 1 } else { 0 })))
            }
            ExprKind::NoneLit => Ok(self.call_value("call i64 @p2w_none()")),
            ExprKind::Str(s) => {
                let bytes = s.as_bytes();
                let g = format!("@.str.{}.{}", self.gprefix, self.gcount);
                self.gcount += 1;
                self.globals.push_str(&format!(
                    "{g} = private unnamed_addr constant [{} x i8] c\"{}\"\n",
                    bytes.len(),
                    llvm_escape(bytes)
                ));
                Ok(self.call_value(&format!("call i64 @p2w_str(ptr {g}, i32 {})", bytes.len())))
            }
            ExprKind::Name(name) => {
                if !self.vars.iter().any(|v| v == name) {
                    return Err(format!("line {}: name '{name}' is not defined", e.line));
                }
                let ptr = format!("%v_{name}");
                Ok(self.call_value(&format!("load i64, ptr {ptr}")))
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.expr(inner)?;
                Ok(self.call_value(&format!("call i64 @p2w_neg(i64 {v})")))
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let v = self.expr(inner)?;
                Ok(self.call_value(&format!("call i64 @p2w_not(i64 {v})")))
            }
            ExprKind::Bin(op, a, b) => self.bin(*op, a, b),
            ExprKind::Call(name, args) => {
                if !self.funcs.contains(name) {
                    return nope("calling this function (only your own functions + print)");
                }
                let mut ops = Vec::with_capacity(args.len());
                for a in args {
                    ops.push(format!("i64 {}", self.expr(a)?));
                }
                Ok(self.call_value(&format!("call i64 @{name}({})", ops.join(", "))))
            }
            _ => nope("this expression"),
        }
    }

    fn bin(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        if matches!(op, BinOp::And | BinOp::Or) {
            return Err("`and`/`or` aren't in the native backend yet".to_string());
        }
        let rt = match op {
            BinOp::Add => "p2w_add",
            BinOp::Sub => "p2w_sub",
            BinOp::Mul => "p2w_mul",
            BinOp::Div => "p2w_div",
            BinOp::FloorDiv => "p2w_floordiv",
            BinOp::Mod => "p2w_mod",
            BinOp::Pow => "p2w_pow",
            BinOp::Lt => "p2w_lt",
            BinOp::Le => "p2w_le",
            BinOp::Gt => "p2w_gt",
            BinOp::Ge => "p2w_ge",
            BinOp::Eq => "p2w_eq",
            BinOp::Ne => "p2w_ne",
            _ => {
                return Err(format!(
                    "line {}: the native (Pico) backend doesn't handle this operator yet",
                    a.line
                ));
            }
        };
        let va = self.expr(a)?;
        let vb = self.expr(b)?;
        Ok(self.call_value(&format!("call i64 @{rt}(i64 {va}, i64 {vb})")))
    }
}

/// The integer value of a literal `step` (handling `-1` parsed as `Neg(1)`).
fn step_literal(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Unary(UnOp::Neg, inner) => match inner.kind {
            ExprKind::Int(n) => Some(-n),
            _ => None,
        },
        _ => None,
    }
}

/// Escape bytes for an LLVM `c"..."` string constant: printable ASCII (except
/// `"` and `\`) verbatim, everything else as `\XX`.
fn llvm_escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b == b'"' || b == b'\\' || !(0x20..=0x7e).contains(&b) {
            out.push_str(&format!("\\{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(src: &str) -> String {
        emit_llvm_ir(&parse(src)).unwrap()
    }

    fn parse(src: &str) -> Vec<Stmt> {
        crate::parser::parse(&crate::lexer::lex(src).unwrap()).unwrap()
    }

    #[test]
    fn module_declares_runtime_and_boxes_values() {
        let out = ir("print(6 * 7)\n");
        assert!(out.contains("declare i64 @p2w_add(i64, i64)"), "{out}");
        assert!(out.contains("declare void @p2w_print(i64)"), "{out}");
        assert!(out.contains("call i64 @p2w_int(i32 6)"), "{out}");
        assert!(out.contains("call i64 @p2w_int(i32 7)"), "{out}");
        assert!(out.contains("call i64 @p2w_mul(i64"), "{out}");
        assert!(out.contains("call void @p2w_print(i64"), "{out}");
        assert!(out.contains("ret i32 0"), "main exit: {out}");
    }

    #[test]
    fn strings_become_global_constants() {
        let out = ir("print(\"hi\")\n");
        assert!(out.contains("constant [2 x i8] c\"hi\""), "{out}");
        assert!(out.contains("call i64 @p2w_str(ptr @.str.main.0, i32 2)"), "{out}");
        // String concatenation goes through p2w_add (the runtime dispatches).
        let out = ir("x = \"a\" + \"b\"\n");
        assert!(out.contains("call i64 @p2w_add(i64"), "{out}");
    }

    #[test]
    fn string_escaping() {
        // A newline + quote must be hex-escaped in the c"..." literal.
        let out = ir("print(\"a\\n\\\"b\")\n");
        assert!(out.contains("\\0A"), "newline escaped: {out}");
        assert!(out.contains("\\22"), "quote escaped: {out}");
    }

    #[test]
    fn arithmetic_and_comparisons_route_through_runtime() {
        assert!(ir("print(7 / 2)\n").contains("call i64 @p2w_div(i64"));
        assert!(ir("print(7 // 2)\n").contains("call i64 @p2w_floordiv(i64"));
        assert!(ir("print(2 ** 10)\n").contains("call i64 @p2w_pow(i64"));
        assert!(ir("x = 1 < 2\n").contains("call i64 @p2w_lt(i64"));
        assert!(ir("y = not 0\n").contains("call i64 @p2w_not(i64"));
    }

    #[test]
    fn control_flow_uses_truthy_and_blocks() {
        let out = ir("x = 5\nif x < 1:\n    print(1)\nelse:\n    print(2)\n");
        assert!(out.contains("call i1 @p2w_truthy(i64"), "{out}");
        assert!(out.contains("br i1"), "{out}");
        assert!(out.contains("ifend"), "{out}");

        let out = ir("i = 0\nwhile i < 3:\n    i = i + 1\n");
        assert!(out.contains("whead"), "{out}");
        assert!(out.contains("br label %whead0"), "back-edge: {out}");
    }

    #[test]
    fn for_range_uses_value_ops() {
        let out = ir("for i in range(1, 5):\n    print(i)\n");
        assert!(out.contains("call i64 @p2w_lt(i64"), "ascending: {out}");
        assert!(out.contains("call i64 @p2w_add(i64"), "increment: {out}");
        let out = ir("for i in range(5, 0, -1):\n    print(i)\n");
        assert!(out.contains("call i64 @p2w_gt(i64"), "descending: {out}");
    }

    #[test]
    fn functions_take_and_return_values() {
        let out = ir("def double(n):\n    return n * 2\nprint(double(21))\n");
        assert!(out.contains("define i64 @double(i64 %a0)"), "{out}");
        assert!(out.contains("store i64 %a0, ptr %v_n"), "param slot: {out}");
        assert!(out.contains("ret i64"), "{out}");
        assert!(out.contains("call i64 @double(i64"), "{out}");
    }

    #[test]
    fn recursion_emits_self_call_and_none_fallthrough() {
        let out = ir(
            "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n",
        );
        assert!(out.contains("define i64 @fact(i64 %a0)"), "{out}");
        assert!(out.contains("call i64 @fact(i64"), "self-call: {out}");
        // A void function falls off the end returning None.
        let out = ir("def greet(name):\n    print(name)\ngreet(\"x\")\n");
        assert!(out.contains("call i64 @p2w_none()"), "implicit None: {out}");
    }

    #[test]
    fn unsupported_constructs_are_clean_errors() {
        assert!(
            emit_llvm_ir(&parse("ok = 1 < 2 and 3 < 4\n"))
                .unwrap_err()
                .contains("native")
        );
        assert!(
            emit_llvm_ir(&parse("xs = [1, 2]\n"))
                .unwrap_err()
                .contains("native")
        );
        assert!(
            emit_llvm_ir(&parse("for x in [1, 2]:\n    print(x)\n"))
                .unwrap_err()
                .contains("native")
        );
    }
}
