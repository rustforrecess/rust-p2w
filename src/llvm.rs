//! AST -> textual LLVM IR — the native Pico 2 W backend emitter (see
//! `PICO_BACKEND.md`).
//!
//! Like `codegen.rs` hand-emits WAT, this hand-emits LLVM IR as text, so the
//! crate needs no LLVM build dependency; turning the `.ll` into an RP2350 binary
//! (`llc`/`lld`/`picotool`) is a later, toolchain-gated phase.
//!
//! Scope: the **integer-typed** slice of the language — assignments, integer
//! arithmetic and comparisons, `not`, `if`/`elif`/`else`, `while`, counted `for`
//! (with a literal step), `break`/`continue`, `print(int)`, and **user functions**
//! (`def`/`return`/calls, all `i32`). Everything else — strings, lists, dicts,
//! `and`/`or`, for-each, default args — is a clean `Err`, pending the on-device
//! value model (the big phase in `PICO_BACKEND.md`). Variables are mutable
//! `alloca` slots (no phi nodes; `mem2reg` cleans them up later). `print` lowers
//! to a call into the device runtime `@p2w_print_int` (USB-CDC), the bare-metal
//! mirror of the browser's `env.write_char`.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// Emit a textual LLVM IR module for the supported integer subset of `stmts`,
/// or an error naming the first unsupported construct.
pub fn emit_llvm_ir(stmts: &[Stmt]) -> Result<String, String> {
    // Function names are collected up front so calls resolve regardless of order.
    let mut funcs = HashSet::new();
    for s in stmts {
        if let StmtKind::Def { name, .. } = &s.kind {
            funcs.insert(name.clone());
        }
    }

    let mut out = String::from(
        "; LLVM IR — rust-p2w native (Pico) backend (integer slice)\n\
         ; `print` calls the device runtime (USB-CDC), like env.write_char.\n\
         declare void @p2w_print_int(i32)\n\n",
    );

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
            out.push_str(&emit_function(name, params, body, &funcs)?);
            out.push('\n');
        }
    }

    let top: Vec<&Stmt> = stmts
        .iter()
        .filter(|s| !matches!(s.kind, StmtKind::Def { .. }))
        .collect();
    out.push_str(&emit_main(&top, &funcs)?);
    Ok(out)
}

/// Emit `define i32 @name(...)` for a user function.
fn emit_function(
    name: &str,
    params: &[String],
    body: &[Stmt],
    funcs: &HashSet<String>,
) -> Result<String, String> {
    let mut f = FuncEmitter::new(funcs);
    // Bring parameters in as mutable locals: alloca + store the incoming arg.
    for (i, p) in params.iter().enumerate() {
        let ptr = f.var_slot(p);
        f.line(&format!("store i32 %a{i}, ptr {ptr}"));
    }
    f.block(body)?;
    if !f.terminated {
        f.body.push_str("  ret i32 0\n"); // fell off the end -> 0 (None)
    }
    let sig: Vec<String> = (0..params.len()).map(|i| format!("i32 %a{i}")).collect();
    Ok(format!(
        "define i32 @{name}({}) {{\nentry:\n{}{}}}\n",
        sig.join(", "),
        f.allocas,
        f.body
    ))
}

/// Emit `define i32 @main()` from the top-level (non-`def`) statements.
fn emit_main(top: &[&Stmt], funcs: &HashSet<String>) -> Result<String, String> {
    let mut f = FuncEmitter::new(funcs);
    for s in top {
        f.stmt(s)?;
    }
    if !f.terminated {
        f.body.push_str("  ret i32 0\n");
    }
    Ok(format!(
        "define i32 @main() {{\nentry:\n{}{}}}\n",
        f.allocas, f.body
    ))
}

/// Per-function emission state. Variables are `alloca` slots; control flow uses
/// labelled basic blocks; `terminated` tracks whether the current block already
/// ended (so we never append after a `br`/`ret`).
struct FuncEmitter<'a> {
    funcs: &'a HashSet<String>,
    /// `alloca`s, kept separate so they all sit at the top of the entry block —
    /// an alloca inside a loop would allocate every iteration (stack leak) and
    /// also blocks `mem2reg`. Entry-block allocas are the standard pattern.
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
    fn new(funcs: &'a HashSet<String>) -> Self {
        FuncEmitter {
            funcs,
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

    /// Append an instruction. If the current block is already terminated, open a
    /// fresh (unreachable) block first so the IR stays well-formed.
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

    /// Append a terminator (br/ret) and mark the block ended.
    fn terminator(&mut self, s: &str) {
        self.line(s);
        self.terminated = true;
    }

    /// Start a labelled block, ending the previous one with a fall-through `br`
    /// if it wasn't already terminated.
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

    /// The `alloca` pointer register for a variable, creating it on first use.
    fn var_slot(&mut self, name: &str) -> String {
        let ptr = format!("%v_{name}");
        if !self.vars.iter().any(|v| v == name) {
            // Always emitted in the entry block (see `allocas` field).
            self.allocas.push_str(&format!("  {ptr} = alloca i32\n"));
            self.vars.push(name.to_string());
        }
        ptr
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
                self.line(&format!("store i32 {v}, ptr {ptr}"));
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return nope("print() with multiple arguments");
                    }
                    let v = self.expr(&args[0])?;
                    self.line(&format!("call void @p2w_print_int(i32 {v})"));
                    Ok(())
                }
                ExprKind::Call(..) => {
                    self.expr(e)?; // a call used for its effect/return
                    Ok(())
                }
                _ => nope("this statement"),
            },
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.expr(e)?,
                    None => "0".to_string(),
                };
                self.terminator(&format!("ret i32 {v}"));
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
                let (_, brk) = self.loops.last().ok_or_else(|| {
                    format!("line {}: 'break' outside a loop", s.line)
                })?;
                let brk = brk.clone();
                self.terminator(&format!("br label %{brk}"));
                Ok(())
            }
            StmtKind::Continue => {
                let (cont, _) = self.loops.last().ok_or_else(|| {
                    format!("line {}: 'continue' outside a loop", s.line)
                })?;
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
        // Each (condition, body) branch; the trailing `next` block hosts the next
        // test, or finally the else.
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
        // The step must be a literal so we can pick the comparison direction at
        // compile time (range(n)/range(a,b) -> +1; range(a,b,-1) -> -1).
        let step_lit = step_literal(step).ok_or_else(|| {
            "the native backend needs a literal range() step yet".to_string()
        })?;
        if step_lit == 0 {
            return Err("range() step must not be zero".to_string());
        }
        // Evaluate the bounds once (Python evaluates range args once).
        let start_v = self.expr(start)?;
        let end_v = self.expr(end_expr)?;
        let slot = self.var_slot(var);
        self.line(&format!("store i32 {start_v}, ptr {slot}"));

        let head = self.fresh_label("fhead");
        let body_l = self.fresh_label("fbody");
        let cont = self.fresh_label("fcont");
        let end = self.fresh_label("fend");

        self.br_to(&head);
        self.place_label(&head);
        let iv = self.temp();
        self.line(&format!("{iv} = load i32, ptr {slot}"));
        let cmp = self.temp();
        let pred = if step_lit > 0 { "slt" } else { "sgt" };
        self.line(&format!("{cmp} = icmp {pred} i32 {iv}, {end_v}"));
        self.terminator(&format!("br i1 {cmp}, label %{body_l}, label %{end}"));

        self.place_label(&body_l);
        self.loops.push((cont.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&cont);

        self.place_label(&cont);
        let cur = self.temp();
        self.line(&format!("{cur} = load i32, ptr {slot}"));
        let inc = self.temp();
        self.line(&format!("{inc} = add i32 {cur}, {step_lit}"));
        self.line(&format!("store i32 {inc}, ptr {slot}"));
        self.br_to(&head);

        self.place_label(&end);
        Ok(())
    }

    /// Evaluate a condition to an `i1` (Python truthiness = nonzero).
    fn cond_i1(&mut self, cond: &Expr) -> Result<String, String> {
        let v = self.expr(cond)?;
        let t = self.temp();
        self.line(&format!("{t} = icmp ne i32 {v}, 0"));
        Ok(t)
    }

    /// Evaluate an integer expression; returns its LLVM operand (an immediate or
    /// a register).
    fn expr(&mut self, e: &Expr) -> Result<String, String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                e.line
            ))
        };
        match &e.kind {
            ExprKind::Int(n) => Ok((*n as i32).to_string()),
            ExprKind::Bool(b) => Ok(if *b { "1" } else { "0" }.to_string()),
            ExprKind::Name(name) => {
                if !self.vars.iter().any(|v| v == name) {
                    return Err(format!("line {}: name '{name}' is not defined", e.line));
                }
                let ptr = format!("%v_{name}");
                let t = self.temp();
                self.line(&format!("{t} = load i32, ptr {ptr}"));
                Ok(t)
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.expr(inner)?;
                let t = self.temp();
                self.line(&format!("{t} = sub i32 0, {v}"));
                Ok(t)
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let v = self.expr(inner)?;
                let c = self.temp();
                self.line(&format!("{c} = icmp eq i32 {v}, 0"));
                let t = self.temp();
                self.line(&format!("{t} = zext i1 {c} to i32"));
                Ok(t)
            }
            ExprKind::Bin(op, a, b) => self.bin(*op, a, b),
            ExprKind::Call(name, args) => {
                if !self.funcs.contains(name) {
                    return nope("calling this function (only your own functions + print)");
                }
                let mut ops = Vec::with_capacity(args.len());
                for a in args {
                    ops.push(format!("i32 {}", self.expr(a)?));
                }
                let t = self.temp();
                self.line(&format!("{t} = call i32 @{name}({})", ops.join(", ")));
                Ok(t)
            }
            _ => nope("this expression"),
        }
    }

    fn bin(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        // `and`/`or` need short-circuit control flow + a value model; defer.
        if matches!(op, BinOp::And | BinOp::Or) {
            return Err("`and`/`or` aren't in the native backend yet".to_string());
        }
        let va = self.expr(a)?;
        let vb = self.expr(b)?;
        let arith = |op: &str| op.to_string();
        let opcode = match op {
            BinOp::Add => arith("add"),
            BinOp::Sub => arith("sub"),
            BinOp::Mul => arith("mul"),
            BinOp::FloorDiv => arith("sdiv"),
            BinOp::Mod => arith("srem"),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                let pred = match op {
                    BinOp::Lt => "slt",
                    BinOp::Le => "sle",
                    BinOp::Gt => "sgt",
                    BinOp::Ge => "sge",
                    BinOp::Eq => "eq",
                    BinOp::Ne => "ne",
                    _ => unreachable!(),
                };
                let c = self.temp();
                self.line(&format!("{c} = icmp {pred} i32 {va}, {vb}"));
                let t = self.temp();
                self.line(&format!("{t} = zext i1 {c} to i32"));
                return Ok(t);
            }
            _ => {
                return Err(format!(
                    "line {}: the native (Pico) backend doesn't handle this operator yet",
                    a.line
                ));
            }
        };
        let t = self.temp();
        self.line(&format!("{t} = {opcode} i32 {va}, {vb}"));
        Ok(t)
    }
}

/// The integer value of a literal `step` (handling `-1` parsed as `Neg(1)`), or
/// None if it isn't a literal.
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
    fn emits_module_scaffold_and_print() {
        let out = ir("print(6 * 7)\n");
        assert!(out.contains("declare void @p2w_print_int(i32)"), "{out}");
        assert!(out.contains("define i32 @main()"), "{out}");
        assert!(out.contains("mul i32 6, 7"), "{out}");
        assert!(out.contains("call void @p2w_print_int(i32 %t0)"), "{out}");
        assert!(out.contains("ret i32 0"), "{out}");
    }

    #[test]
    fn assignment_loads_and_stores() {
        let out = ir("x = 2 + 3 * 4\nprint(x)\n");
        assert!(out.contains("%v_x = alloca i32"), "{out}");
        assert!(out.contains("mul i32 3, 4"), "{out}");
        assert!(out.contains("store i32"), "{out}");
        assert!(out.contains("load i32, ptr %v_x"), "{out}");
    }

    #[test]
    fn arithmetic_opcodes() {
        assert!(ir("print(10 - 4)\n").contains("sub i32 10, 4"));
        assert!(ir("print(9 // 2)\n").contains("sdiv i32 9, 2"));
        assert!(ir("print(9 % 2)\n").contains("srem i32 9, 2"));
        assert!(ir("print(-5)\n").contains("sub i32 0, 5"));
    }

    #[test]
    fn comparison_and_not() {
        let out = ir("x = 1 < 2\ny = not 0\n");
        assert!(out.contains("icmp slt i32 1, 2"), "{out}");
        assert!(out.contains("zext i1"), "{out}");
        assert!(out.contains("icmp eq i32 0, 0"), "{out}");
    }

    #[test]
    fn if_elif_else_branches() {
        let out = ir("x = 5\nif x < 1:\n    print(1)\nelif x < 9:\n    print(2)\nelse:\n    print(3)\n");
        assert!(out.contains("icmp ne i32"), "truthiness test: {out}");
        assert!(out.contains("br i1"), "{out}");
        assert!(out.contains("then"), "{out}");
        assert!(out.contains("ifend"), "{out}");
    }

    #[test]
    fn while_loop_has_head_body_end() {
        let out = ir("i = 0\nwhile i < 3:\n    i = i + 1\n");
        assert!(out.contains("whead"), "{out}");
        assert!(out.contains("wbody"), "{out}");
        assert!(out.contains("wend"), "{out}");
        assert!(out.contains("br label %whead0"), "back-edge: {out}");
    }

    #[test]
    fn for_range_lowers_to_a_counted_loop() {
        let out = ir("for i in range(1, 5):\n    print(i)\n");
        assert!(out.contains("icmp slt i32"), "ascending compare: {out}");
        assert!(out.contains("add i32"), "increment: {out}");
        assert!(out.contains("fhead"), "{out}");
        // descending: range(5, 0, -1) compares with sgt and increments by -1.
        let out = ir("for i in range(5, 0, -1):\n    print(i)\n");
        assert!(out.contains("icmp sgt i32"), "descending compare: {out}");
        assert!(out.contains("add i32") && out.contains(", -1"), "step -1: {out}");
    }

    #[test]
    fn break_and_continue_branch_to_loop_labels() {
        let out =
            ir("i = 0\nwhile i < 10:\n    i = i + 1\n    if i == 3:\n        continue\n    if i > 7:\n        break\n");
        // continue jumps to the while head; break to its end.
        assert!(out.contains("br label %whead0"), "{out}");
        assert!(out.contains("br label %wend"), "{out}");
    }

    #[test]
    fn functions_define_and_call() {
        let out = ir("def double(n):\n    return n * 2\nprint(double(21))\n");
        assert!(out.contains("define i32 @double(i32 %a0)"), "{out}");
        assert!(out.contains("store i32 %a0, ptr %v_n"), "param slot: {out}");
        assert!(out.contains("mul i32"), "{out}");
        assert!(out.contains("ret i32"), "{out}");
        assert!(out.contains("call i32 @double(i32 21)"), "{out}");
    }

    #[test]
    fn recursion_emits_self_call() {
        let out = ir(
            "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n",
        );
        assert!(out.contains("define i32 @fact(i32 %a0)"), "{out}");
        assert!(out.contains("call i32 @fact(i32"), "self-call: {out}");
    }

    #[test]
    fn unsupported_constructs_are_clean_errors() {
        assert!(emit_llvm_ir(&parse("s = \"hi\"\n")).unwrap_err().contains("native"));
        assert!(
            emit_llvm_ir(&parse("ok = 1 < 2 and 3 < 4\n"))
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
