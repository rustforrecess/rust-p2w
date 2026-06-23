//! AST -> textual LLVM IR — the native Pico 2 W backend emitter (see
//! `PICO_BACKEND.md`).
//!
//! Like `codegen.rs` hand-emits WAT, this hand-emits LLVM IR as text, so the
//! crate needs no LLVM build dependency; turning the `.ll` into an RP2350 binary
//! (`llc`/`lld`/`picotool`) is a later, toolchain-gated phase.
//!
//! **Value model (phase 3):** every Python value is a uniform tagged `i32`, and
//! this emitter is **representation-agnostic** — it never assumes a bit layout,
//! it only *calls* a small **runtime ABI** of `p2w_*` functions (declared at the
//! top of the module). The device runtime owns the actual rep + allocator. This
//! is the same "box values + call runtime ops" split the WASM backend uses, and
//! it's what lets strings (and later lists/dicts) drop in without touching the
//! control-flow machinery.
//!
//! Supported now: ints, floats, bools, strings, `print`, arithmetic
//! (`+ - * / // % **`), comparisons, `not`, `and`/`or`, `if`/`elif`/`else`,
//! `while`, counted `for` (literal step), `break`/`continue`, for-each,
//! lists/dicts/indexing/methods, and user functions (`def`/`return`/calls,
//! incl. recursion). Not yet (clean errors): tuples, comprehensions, classes,
//! default args.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// The runtime ABI the emitted module depends on (implemented by the device
/// runtime). Declared at the top of every module.
const RUNTIME_DECLS: &str = "\
; runtime ABI — values are opaque tagged i32; the device runtime owns the rep.
declare i32 @p2w_int(i32)
declare i32 @p2w_float(double)
declare i32 @p2w_bool(i1)
declare i32 @p2w_none()
declare i32 @p2w_str(ptr, i32)
declare i32 @p2w_add(i32, i32)
declare i32 @p2w_sub(i32, i32)
declare i32 @p2w_mul(i32, i32)
declare i32 @p2w_div(i32, i32)
declare i32 @p2w_floordiv(i32, i32)
declare i32 @p2w_mod(i32, i32)
declare i32 @p2w_pow(i32, i32)
declare i32 @p2w_neg(i32)
declare i32 @p2w_lt(i32, i32)
declare i32 @p2w_le(i32, i32)
declare i32 @p2w_gt(i32, i32)
declare i32 @p2w_ge(i32, i32)
declare i32 @p2w_eq(i32, i32)
declare i32 @p2w_ne(i32, i32)
declare i32 @p2w_not(i32)
declare i1 @p2w_truthy(i32)
declare void @p2w_print(i32)
; reference counting (no-ops for inline int/bool/None at runtime)
declare void @p2w_retain(i32)
declare void @p2w_release(i32)
; containers
declare i32 @p2w_list_new()
declare i32 @p2w_list_append(i32, i32)
declare i32 @p2w_dict_new()
declare i32 @p2w_index(i32, i32)
declare void @p2w_setindex(i32, i32, i32)
declare i32 @p2w_len(i32)
; method dispatch by name (runtime resolves on the receiver's type)
declare i32 @p2w_method0(i32, ptr, i32)
declare i32 @p2w_method1(i32, ptr, i32, i32)
declare i32 @p2w_method2(i32, ptr, i32, i32, i32)
; iteration protocol (for-each)
declare i32 @p2w_iter(i32)
declare i1 @p2w_iter_has(i32)
declare i32 @p2w_iter_next(i32)
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
        f.line(&format!("store i32 %a{i}, ptr {ptr}"));
    }
    f.block(body)?;
    if !f.terminated {
        // Implicit `return None` on fall-through: release locals first.
        let none = f.call_value("call i32 @p2w_none()");
        f.emit_exit_releases();
        f.body.push_str(&format!("  ret i32 {none}\n"));
    }
    let sig: Vec<String> = (0..params.len()).map(|i| format!("i32 %a{i}")).collect();
    let def = format!(
        "define i32 @{name}({}) {{\nentry:\n{}{}}}\n",
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
        // Release all top-level locals so a finished program ends at live==0.
        f.emit_exit_releases();
        f.body.push_str("  ret i32 0\n");
    }
    let def = format!(
        "define i32 @main() {{\nentry:\n{}{}}}\n",
        f.allocas, f.body
    );
    Ok((def, f.globals))
}

/// Format an `f64` as an LLVM IR `double` constant. LLVM only accepts decimal
/// literals that are *exactly* representable; anything else must use the hex
/// form (`0x` + the 16-hex-digit IEEE-754 bit pattern), which is always exact.
fn fmt_double(f: f64) -> String {
    format!("0x{:016X}", f.to_bits())
}

// --- Ownership contract for the (upcoming) RC insertion pass ----------------
//
// The runtime is already RC-correct (free releases children; setindex/dict-update
// release the replaced value; index/iter_next return owned refs; pop transfers).
// What remains is the EMITTER inserting retain/release. The model (transfer-based,
// "Model A"):
//   - every expr() result is an OWNED reference (+1). Constructors (p2w_int/str/
//     list_new/add/call/index/iter_next/...) already return +1; a Name *load* is
//     BORROWED, so the pass emits `p2w_retain` after it to make it owned.
//   - assign `x = e`: release the OLD x, then store e (transfer — don't release e).
//   - container insert (append / list+dict literals / setindex value & key):
//     TRANSFER (don't release the inserted temp); the runtime owns + frees it
//     later. (List indices are ints, so transferring the index is a no-op.)
//   - borrowing ops (arithmetic/compare operands, unary, print, conditions,
//     len, index target+index, method receiver): release each operand temp
//     after the op consumes it.
//   - TRANSFER sites (no release — ownership moves in): assignment store (after
//     releasing the OLD slot value), container insert (list append, dict/list
//     literal elems, setindex value & key), method args, CALL ARGS (the callee
//     owns its params), and the returned value.
//   - every variable slot owns +1; a function exit (every `ret`) releases all
//     slots and any pending loop temps (`cleanups`). Slots are zero-initialized
//     so releasing a never-assigned slot is a safe no-op.
// Loop temps that outlive one iteration (the iterator + its sequence; a counted
// loop's end bound) are pushed on `cleanups`, released after the loop on the
// normal path and at every early `return`. Per-iteration: a loop variable that
// can hold a heap value (for-each) releases its previous value before storing
// the next, exactly like assignment.
// WIRED (step 1, naive — release at scope end, no last-use precision). Validated
// by tools/native_run.sh with GATE_LEAKS=1 (every case ends live==0). Precision
// (last-use), borrowed params, and reuse (drop-reuse) follow — see MEMORY_MANAGEMENT.md.

/// Per-function emission state. Values are tagged `i32`; variables are
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
    /// Owned temps that outlive a single statement (loop iterators/sequences,
    /// counted-loop bounds) and must be released at every function exit.
    cleanups: Vec<String>,
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
            cleanups: Vec::new(),
            terminated: false,
        }
    }

    /// Release a heap value (no-op at runtime for inline ints/bools/None).
    fn release(&mut self, v: &str) {
        self.line(&format!("call void @p2w_release(i32 {v})"));
    }

    /// Retain a heap value, turning a borrowed reference into an owned one.
    fn retain(&mut self, v: &str) {
        self.line(&format!("call void @p2w_retain(i32 {v})"));
    }

    /// Release everything this function owns — pending loop temps and every
    /// variable slot — emitted before each `ret`. Slots are zero-initialized, so
    /// releasing a slot that was never assigned is a safe no-op.
    fn emit_exit_releases(&mut self) {
        let cleanups = self.cleanups.clone();
        for c in cleanups {
            self.release(&c);
        }
        let vars = self.vars.clone();
        for name in vars {
            let t = self.temp();
            self.line(&format!("{t} = load i32, ptr %v_{name}"));
            self.release(&t);
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
            self.allocas.push_str(&format!("  {ptr} = alloca i32\n"));
            // Zero-init so the exit-release of a never-assigned slot is a no-op
            // (0 isn't a heap value, so p2w_release ignores it).
            self.allocas.push_str(&format!("  store i32 0, ptr {ptr}\n"));
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

    /// Add a private string constant to the module and return its global name
    /// (used for both string literals and method names). Caller knows the byte
    /// length.
    fn intern_str(&mut self, bytes: &[u8]) -> String {
        let g = format!("@.str.{}.{}", self.gprefix, self.gcount);
        self.gcount += 1;
        self.globals.push_str(&format!(
            "{g} = private unnamed_addr constant [{} x i8] c\"{}\"\n",
            bytes.len(),
            llvm_escape(bytes)
        ));
        g
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
                let v = self.expr(value)?; // owned (transferred into the slot)
                let ptr = self.var_slot(name);
                let old = self.temp();
                self.line(&format!("{old} = load i32, ptr {ptr}"));
                self.release(&old); // drop the previous binding (no-op if 0/inline)
                self.line(&format!("store i32 {v}, ptr {ptr}"));
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return nope("print() with multiple arguments");
                    }
                    let (v, o) = self.expr_borrow(&args[0])?;
                    self.line(&format!("call void @p2w_print(i32 {v})"));
                    self.release_if_owned(&v, o); // print borrows the operand
                    Ok(())
                }
                // Any other expression statement (a call, a method call like
                // xs.append(1), ...) runs for its side effects; its owned result
                // is discarded, so release it.
                _ => {
                    let v = self.expr(e)?;
                    self.release(&v);
                    Ok(())
                }
            },
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                // The target is a reference value, so the runtime mutates the
                // shared heap object in place — no variable-slot special-casing.
                let t = self.expr(target)?;
                let i = self.expr(index)?; // dict: key transferred to the runtime
                let v = self.expr(value)?; //       value transferred too
                self.line(&format!("call void @p2w_setindex(i32 {t}, i32 {i}, i32 {v})"));
                // Only the container is borrowed. The index/key is NOT released
                // here: for a list it's an inline int position; for a dict the
                // runtime takes ownership of the key (storing it, or releasing it
                // as redundant on update) — releasing it here would double-free.
                self.release(&t);
                Ok(())
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => self.emit_foreach(var, iterable, body),
            StmtKind::Return(value) => {
                // The returned value is owned and transferred out, so release all
                // locals/cleanups *after* computing it (releasing a slot it was
                // loaded from is fine — the retained temp keeps its own ref).
                let v = match value {
                    Some(e) => self.expr(e)?,
                    None => self.call_value("call i32 @p2w_none()"),
                };
                self.emit_exit_releases();
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
        // end_v is read every iteration, so it must outlive the loop; release it
        // after the loop (and on early return) via cleanups.
        self.cleanups.push(end_v.clone());
        let step_v = self.call_value(&format!("call i32 @p2w_int(i32 {step_lit})"));
        let slot = self.var_slot(var);
        let old = self.temp();
        self.line(&format!("{old} = load i32, ptr {slot}"));
        self.release(&old); // drop any prior binding of this name
        self.line(&format!("store i32 {start_v}, ptr {slot}"));

        let head = self.fresh_label("fhead");
        let body_l = self.fresh_label("fbody");
        let cont = self.fresh_label("fcont");
        let end = self.fresh_label("fend");

        self.br_to(&head);
        self.place_label(&head);
        let iv = self.temp();
        self.line(&format!("{iv} = load i32, ptr {slot}"));
        // Ascending loops compare with `<`, descending with `>` (Python range).
        let cmp_fn = if step_lit > 0 { "p2w_lt" } else { "p2w_gt" };
        let cmpv = self.call_value(&format!("call i32 @{cmp_fn}(i32 {iv}, i32 {end_v})"));
        let cond = self.temp();
        self.line(&format!("{cond} = call i1 @p2w_truthy(i32 {cmpv})"));
        self.terminator(&format!("br i1 {cond}, label %{body_l}, label %{end}"));

        self.place_label(&body_l);
        self.loops.push((cont.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&cont);

        self.place_label(&cont);
        let cur = self.temp();
        self.line(&format!("{cur} = load i32, ptr {slot}"));
        let inc = self.call_value(&format!("call i32 @p2w_add(i32 {cur}, i32 {step_v})"));
        self.line(&format!("store i32 {inc}, ptr {slot}"));
        self.br_to(&head);

        self.place_label(&end);
        self.cleanups.pop();
        self.release(&end_v); // counter + step are ints; the bound may not be
        Ok(())
    }

    /// `for var in iterable:` over the runtime iteration protocol
    /// (`p2w_iter` / `p2w_iter_has` / `p2w_iter_next`).
    fn emit_foreach(&mut self, var: &str, iterable: &Expr, body: &[Stmt]) -> Result<(), String> {
        let seq = self.expr(iterable)?; // owned; the iterator borrows it
        let it = self.call_value(&format!("call i32 @p2w_iter(i32 {seq})"));
        // Both outlive the loop and must survive an early return — track them.
        self.cleanups.push(seq.clone());
        self.cleanups.push(it.clone());
        let slot = self.var_slot(var);

        let head = self.fresh_label("ehead");
        let body_l = self.fresh_label("ebody");
        let end = self.fresh_label("eend");

        self.br_to(&head);
        self.place_label(&head);
        let has = self.temp();
        self.line(&format!("{has} = call i1 @p2w_iter_has(i32 {it})"));
        self.terminator(&format!("br i1 {has}, label %{body_l}, label %{end}"));

        self.place_label(&body_l);
        // Drop the previous element before binding the next (iter_next is owned).
        let prev = self.temp();
        self.line(&format!("{prev} = load i32, ptr {slot}"));
        self.release(&prev);
        let cur = self.call_value(&format!("call i32 @p2w_iter_next(i32 {it})"));
        self.line(&format!("store i32 {cur}, ptr {slot}"));
        self.loops.push((head.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&head);

        self.place_label(&end);
        // Pop in reverse push order; release the iterator then its sequence.
        self.cleanups.pop();
        self.cleanups.pop();
        self.release(&it);
        self.release(&seq);
        Ok(())
    }

    /// Evaluate a condition to an `i1` via the runtime's truthiness.
    fn cond_i1(&mut self, cond: &Expr) -> Result<String, String> {
        let (v, o) = self.expr_borrow(cond)?;
        let t = self.temp();
        self.line(&format!("{t} = call i1 @p2w_truthy(i32 {v})"));
        self.release_if_owned(&v, o); // truthiness borrows the condition value
        Ok(t)
    }

    /// Evaluate an expression to a tagged-`i32` value operand.
    fn expr(&mut self, e: &Expr) -> Result<String, String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                e.line
            ))
        };
        match &e.kind {
            ExprKind::Int(n) => Ok(self.call_value(&format!("call i32 @p2w_int(i32 {})", *n as i32))),
            ExprKind::Float(f) => {
                // LLVM wants a portable double constant; hex float is exact.
                Ok(self.call_value(&format!("call i32 @p2w_float(double {})", fmt_double(*f))))
            }
            ExprKind::Bool(b) => {
                Ok(self.call_value(&format!("call i32 @p2w_bool(i1 {})", if *b { 1 } else { 0 })))
            }
            ExprKind::NoneLit => Ok(self.call_value("call i32 @p2w_none()")),
            ExprKind::Str(s) => {
                let bytes = s.as_bytes();
                let g = self.intern_str(bytes);
                Ok(self.call_value(&format!("call i32 @p2w_str(ptr {g}, i32 {})", bytes.len())))
            }
            ExprKind::Name(name) => {
                if !self.vars.iter().any(|v| v == name) {
                    return Err(format!("line {}: name '{name}' is not defined", e.line));
                }
                // A load is borrowed; retain to hand back an owned reference.
                let t = self.call_value(&format!("load i32, ptr %v_{name}"));
                self.retain(&t);
                Ok(t)
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let (v, o) = self.expr_borrow(inner)?;
                let r = self.call_value(&format!("call i32 @p2w_neg(i32 {v})"));
                self.release_if_owned(&v, o);
                Ok(r)
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let (v, o) = self.expr_borrow(inner)?;
                let r = self.call_value(&format!("call i32 @p2w_not(i32 {v})"));
                self.release_if_owned(&v, o);
                Ok(r)
            }
            ExprKind::Bin(op, a, b) => self.bin(*op, a, b),
            ExprKind::Call(name, args) => {
                // len() is the one builtin lowered to the runtime so far.
                if name == "len" {
                    if args.len() != 1 {
                        return nope("len() with other than one argument");
                    }
                    let (v, o) = self.expr_borrow(&args[0])?;
                    let r = self.call_value(&format!("call i32 @p2w_len(i32 {v})"));
                    self.release_if_owned(&v, o); // len borrows its argument
                    return Ok(r);
                }
                if !self.funcs.contains(name) {
                    return nope("calling this function (only your own functions, len, + print)");
                }
                let mut ops = Vec::with_capacity(args.len());
                for a in args {
                    ops.push(format!("i32 {}", self.expr(a)?));
                }
                Ok(self.call_value(&format!("call i32 @{name}({})", ops.join(", "))))
            }
            ExprKind::List(items) => {
                let list = self.call_value("call i32 @p2w_list_new()");
                for it in items {
                    let v = self.expr(it)?;
                    self.line(&format!("call i32 @p2w_list_append(i32 {list}, i32 {v})"));
                }
                Ok(list)
            }
            ExprKind::Dict(pairs) => {
                let dict = self.call_value("call i32 @p2w_dict_new()");
                for (k, v) in pairs {
                    let kv = self.expr(k)?;
                    let vv = self.expr(v)?;
                    self.line(&format!(
                        "call void @p2w_setindex(i32 {dict}, i32 {kv}, i32 {vv})"
                    ));
                }
                Ok(dict)
            }
            ExprKind::Index(obj, idx) => {
                let (o, oo) = self.expr_borrow(obj)?;
                let (i, oi) = self.expr_borrow(idx)?;
                let r = self.call_value(&format!("call i32 @p2w_index(i32 {o}, i32 {i})"));
                self.release_if_owned(&o, oo); // target + index borrowed; result owned
                self.release_if_owned(&i, oi);
                Ok(r)
            }
            ExprKind::MethodCall(obj, method, args) => self.method_call(obj, method, args),
            _ => nope("this expression"),
        }
    }

    /// `recv.method(args)` -> a name-dispatched runtime call (the runtime
    /// resolves the method on the receiver's type). 0–2 args for now.
    fn method_call(&mut self, obj: &Expr, method: &str, args: &[Expr]) -> Result<String, String> {
        if args.len() > 2 {
            return Err(format!(
                "line {}: the native backend handles methods with up to 2 arguments yet",
                obj.line
            ));
        }
        let (recv, recv_owned) = self.expr_borrow(obj)?;
        let mut argvals = Vec::with_capacity(args.len());
        for a in args {
            argvals.push(self.expr(a)?); // method args are transferred (owned)
        }
        let name_g = self.intern_str(method.as_bytes());
        let nlen = method.len();
        let extra: String = argvals
            .iter()
            .map(|v| format!(", i32 {v}"))
            .collect();
        let r = self.call_value(&format!(
            "call i32 @p2w_method{}(i32 {recv}, ptr {name_g}, i32 {nlen}{extra})",
            args.len()
        ));
        // The receiver is borrowed; method args are transferred (the runtime
        // method owns them — e.g. append stores its arg), so they aren't released.
        self.release_if_owned(&recv, recv_owned);
        Ok(r)
    }

    /// Evaluate `e` for a *borrowing* use — an operand of an op that reads but
    /// doesn't keep the reference (arithmetic, compare, print, len, condition,
    /// read-index, method receiver). Returns `(value, owned)`. A plain `Name` is
    /// borrowed through its variable slot, which already owns it for the duration
    /// of this op, so `owned = false` and there's no `retain`/`release` at all.
    /// Anything else is a freshly owned temp (`owned = true`) the caller must
    /// `release` after the op.
    /// Borrowing is sound here because a single op evaluates its operands and
    /// consumes them immediately — no statement runs in between to reassign the
    /// slot. (This is the practical core of Perceus last-use: the common
    /// read-then-use never touches a refcount.)
    fn expr_borrow(&mut self, e: &Expr) -> Result<(String, bool), String> {
        if let ExprKind::Name(name) = &e.kind {
            if !self.vars.iter().any(|v| v == name) {
                return Err(format!("line {}: name '{name}' is not defined", e.line));
            }
            let t = self.call_value(&format!("load i32, ptr %v_{name}"));
            return Ok((t, false));
        }
        Ok((self.expr(e)?, true))
    }

    /// Release a borrowed operand only if it was actually an owned temp.
    fn release_if_owned(&mut self, v: &str, owned: bool) {
        if owned {
            self.release(v);
        }
    }

    fn bin(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        if matches!(op, BinOp::And | BinOp::Or) {
            return self.short_circuit(op, a, b);
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
        let (va, oa) = self.expr_borrow(a)?;
        let (vb, ob) = self.expr_borrow(b)?;
        let r = self.call_value(&format!("call i32 @{rt}(i32 {va}, i32 {vb})"));
        // Operands are borrowed; the result is a new owned value.
        self.release_if_owned(&va, oa);
        self.release_if_owned(&vb, ob);
        Ok(r)
    }

    /// `and`/`or` with Python semantics: short-circuit, and the result is the
    /// *deciding operand* (not a bool). The left value goes in a slot; the right
    /// is evaluated (and overwrites the slot) only when needed.
    ///   and: keep left if falsy, else right.   or: keep left if truthy, else right.
    fn short_circuit(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        let id = self.next_label;
        self.next_label += 1;
        let slot = format!("%sc{id}");
        self.allocas.push_str(&format!("  {slot} = alloca i32\n"));

        let va = self.expr(a)?;
        self.line(&format!("store i32 {va}, ptr {slot}"));
        let c = self.temp();
        self.line(&format!("{c} = call i1 @p2w_truthy(i32 {va})"));

        let rhs = format!("scrhs{id}");
        let end = format!("scend{id}");
        // `and` evaluates the rhs when the lhs is truthy; `or` when it's falsy.
        if matches!(op, BinOp::And) {
            self.terminator(&format!("br i1 {c}, label %{rhs}, label %{end}"));
        } else {
            self.terminator(&format!("br i1 {c}, label %{end}, label %{rhs}"));
        }

        self.place_label(&rhs);
        // The left operand isn't the result on this path — drop it. (On the other
        // path it stays in the slot and becomes the owned result.)
        self.release(&va);
        let vb = self.expr(b)?;
        self.line(&format!("store i32 {vb}, ptr {slot}"));
        self.br_to(&end);

        self.place_label(&end);
        // The kept operand is loaded back as a single owned reference.
        Ok(self.call_value(&format!("load i32, ptr {slot}")))
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
        assert!(out.contains("declare i32 @p2w_add(i32, i32)"), "{out}");
        assert!(out.contains("declare void @p2w_print(i32)"), "{out}");
        assert!(out.contains("call i32 @p2w_int(i32 6)"), "{out}");
        assert!(out.contains("call i32 @p2w_int(i32 7)"), "{out}");
        assert!(out.contains("call i32 @p2w_mul(i32"), "{out}");
        assert!(out.contains("call void @p2w_print(i32"), "{out}");
        assert!(out.contains("ret i32 0"), "main exit: {out}");
    }

    #[test]
    fn strings_become_global_constants() {
        let out = ir("print(\"hi\")\n");
        assert!(out.contains("constant [2 x i8] c\"hi\""), "{out}");
        assert!(out.contains("call i32 @p2w_str(ptr @.str.main.0, i32 2)"), "{out}");
        // String concatenation goes through p2w_add (the runtime dispatches).
        let out = ir("x = \"a\" + \"b\"\n");
        assert!(out.contains("call i32 @p2w_add(i32"), "{out}");
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
        assert!(ir("print(7 / 2)\n").contains("call i32 @p2w_div(i32"));
        assert!(ir("print(7 // 2)\n").contains("call i32 @p2w_floordiv(i32"));
        assert!(ir("print(2 ** 10)\n").contains("call i32 @p2w_pow(i32"));
        assert!(ir("x = 1 < 2\n").contains("call i32 @p2w_lt(i32"));
        assert!(ir("y = not 0\n").contains("call i32 @p2w_not(i32"));
    }

    #[test]
    fn rc_pass_emits_retain_and_release() {
        // Transferring a *named* value (ys = xs) retains it; the slots are
        // released at exit. (Full memory-correctness is validated by
        // tools/native_run.sh; this just guards the wiring from removal.)
        let out = ir("xs = [1, 2]\nys = xs\nprint(len(ys))\n");
        assert!(out.contains("call void @p2w_retain(i32"), "retain on transfer: {out}");
        assert!(out.contains("call void @p2w_release(i32"), "release at exit: {out}");
    }

    #[test]
    fn borrow_on_read_skips_refcounting() {
        // A name read straight into a borrowing op (print) needs NO retain/release:
        // the slot owns it for the op's duration. This program transfers nothing.
        let out = ir("x = 5\nprint(x)\nprint(x + 1)\n");
        assert!(
            !out.contains("call void @p2w_retain"),
            "no retain for borrowed reads: {out}"
        );
    }

    #[test]
    fn float_literals_box_through_p2w_float() {
        let out = ir("x = 3.5\nprint(x)\n");
        assert!(out.contains("declare i32 @p2w_float(double)"), "{out}");
        // 3.5 is exactly representable; its bit pattern is 0x400C000000000000.
        assert!(
            out.contains("call i32 @p2w_float(double 0x400C000000000000)"),
            "{out}"
        );
    }

    #[test]
    fn control_flow_uses_truthy_and_blocks() {
        let out = ir("x = 5\nif x < 1:\n    print(1)\nelse:\n    print(2)\n");
        assert!(out.contains("call i1 @p2w_truthy(i32"), "{out}");
        assert!(out.contains("br i1"), "{out}");
        assert!(out.contains("ifend"), "{out}");

        let out = ir("i = 0\nwhile i < 3:\n    i = i + 1\n");
        assert!(out.contains("whead"), "{out}");
        assert!(out.contains("br label %whead0"), "back-edge: {out}");
    }

    #[test]
    fn for_range_uses_value_ops() {
        let out = ir("for i in range(1, 5):\n    print(i)\n");
        assert!(out.contains("call i32 @p2w_lt(i32"), "ascending: {out}");
        assert!(out.contains("call i32 @p2w_add(i32"), "increment: {out}");
        let out = ir("for i in range(5, 0, -1):\n    print(i)\n");
        assert!(out.contains("call i32 @p2w_gt(i32"), "descending: {out}");
    }

    #[test]
    fn functions_take_and_return_values() {
        let out = ir("def double(n):\n    return n * 2\nprint(double(21))\n");
        assert!(out.contains("define i32 @double(i32 %a0)"), "{out}");
        assert!(out.contains("store i32 %a0, ptr %v_n"), "param slot: {out}");
        assert!(out.contains("ret i32"), "{out}");
        assert!(out.contains("call i32 @double(i32"), "{out}");
    }

    #[test]
    fn recursion_emits_self_call_and_none_fallthrough() {
        let out = ir(
            "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n",
        );
        assert!(out.contains("define i32 @fact(i32 %a0)"), "{out}");
        assert!(out.contains("call i32 @fact(i32"), "self-call: {out}");
        // A void function falls off the end returning None.
        let out = ir("def greet(name):\n    print(name)\ngreet(\"x\")\n");
        assert!(out.contains("call i32 @p2w_none()"), "implicit None: {out}");
    }

    #[test]
    fn lists_dicts_index_and_setindex() {
        let out = ir("xs = [1, 2, 3]\nprint(xs[0])\nxs[1] = 9\n");
        assert!(out.contains("call i32 @p2w_list_new()"), "{out}");
        assert!(out.contains("call i32 @p2w_list_append(i32"), "{out}");
        assert!(out.contains("call i32 @p2w_index(i32"), "read: {out}");
        assert!(out.contains("call void @p2w_setindex(i32"), "write: {out}");

        let out = ir("d = {\"a\": 1, \"b\": 2}\nprint(d[\"a\"])\n");
        assert!(out.contains("call i32 @p2w_dict_new()"), "{out}");
        assert!(out.contains("call void @p2w_setindex(i32"), "dict build: {out}");
        assert!(out.contains("call i32 @p2w_index(i32"), "dict read: {out}");
    }

    #[test]
    fn methods_dispatch_by_name() {
        let out = ir("xs = [1]\nxs.append(2)\nlast = xs.pop()\n");
        assert!(out.contains("constant [6 x i8] c\"append\""), "method name: {out}");
        assert!(out.contains("call i32 @p2w_method1(i32"), "1-arg method: {out}");
        assert!(out.contains("constant [3 x i8] c\"pop\""), "{out}");
        assert!(out.contains("call i32 @p2w_method0(i32"), "0-arg method: {out}");
    }

    #[test]
    fn len_builtin_and_for_each() {
        assert!(ir("xs = [1, 2]\nprint(len(xs))\n").contains("call i32 @p2w_len(i32"));
        let out = ir("for x in [1, 2, 3]:\n    print(x)\n");
        assert!(out.contains("call i32 @p2w_iter(i32"), "{out}");
        assert!(out.contains("call i1 @p2w_iter_has(i32"), "{out}");
        assert!(out.contains("call i32 @p2w_iter_next(i32"), "{out}");
        assert!(out.contains("ehead"), "loop labels: {out}");
    }

    #[test]
    fn and_or_short_circuit_into_a_slot() {
        // `and`: lhs in a slot, rhs only on the truthy branch; result is loaded.
        let out = ir("ok = 1 < 2 and 3 < 4\n");
        assert!(out.contains("alloca i32"), "result slot: {out}");
        assert!(out.contains("call i1 @p2w_truthy(i32"), "{out}");
        assert!(out.contains("scrhs"), "rhs branch: {out}");
        assert!(out.contains("load i32, ptr %sc"), "result load: {out}");
        // `or` also compiles (different branch wiring).
        assert!(ir("x = 0 or 5\n").contains("scrhs"), "or compiles");
    }

    #[test]
    fn unsupported_constructs_are_clean_errors() {
        // Tuples/comprehensions are still pending.
        assert!(
            emit_llvm_ir(&parse("t = (1, 2)\n"))
                .unwrap_err()
                .contains("native")
        );
    }
}
