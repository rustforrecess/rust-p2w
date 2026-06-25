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

use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// The runtime ABI the emitted module depends on (implemented by the device
/// runtime). Declared at the top of every module.
const RUNTIME_DECLS: &str = "\
; runtime ABI — values are opaque tagged i32; the device runtime owns the rep.
declare i32 @p2w_int(i32)
declare i32 @p2w_unbox_int(i32)
declare i32 @p2w_float(double)
declare double @p2w_unbox_float(i32)
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
declare i1 @p2w_unique(i32)
; containers
declare i32 @p2w_list_new()
declare i32 @p2w_list_append(i32, i32)
declare i32 @p2w_dict_new()
declare i32 @p2w_iarray_new()
declare void @p2w_iarray_push(i32, i32)
declare i32 @p2w_iarray_get(i32, i32)
declare void @p2w_iarray_set(i32, i32, i32)
declare i32 @p2w_farray_new()
declare void @p2w_farray_push(i32, double)
declare double @p2w_farray_get(i32, i32)
declare void @p2w_farray_set(i32, i32, double)
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

    // Per-function signatures (borrow masks + parameter/return reprs), computed
    // up front so call sites (which may precede the definition) can emit the
    // matching convention and coerce args/results.
    let mut borrow_masks: HashMap<String, Vec<bool>> = HashMap::new();
    let mut param_reprs: HashMap<String, Vec<Repr>> = HashMap::new();
    let mut ret_reprs: HashMap<String, Repr> = HashMap::new();
    for s in stmts {
        if let StmtKind::Def {
            name,
            params,
            body,
            param_types,
            return_type,
            ..
        } = &s.kind
        {
            let mask = params.iter().map(|p| !param_escapes(body, p)).collect();
            borrow_masks.insert(name.clone(), mask);
            let preprs = param_types.iter().map(repr_of_ann).collect();
            param_reprs.insert(name.clone(), preprs);
            ret_reprs.insert(name.clone(), repr_of_ann(return_type));
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
            let (def, g) = emit_function(
                name,
                params,
                body,
                &funcs,
                &borrow_masks,
                &param_reprs,
                &ret_reprs,
            )?;
            defs.push_str(&def);
            defs.push('\n');
            globals.push_str(&g);
        }
    }

    let top: Vec<&Stmt> = stmts
        .iter()
        .filter(|s| !matches!(s.kind, StmtKind::Def { .. }))
        .collect();
    let (main_def, main_g) = emit_main(&top, &funcs, &borrow_masks, &param_reprs, &ret_reprs)?;
    globals.push_str(&main_g);

    Ok(format!(
        "; LLVM IR — rust-p2w native (Pico) backend\n{RUNTIME_DECLS}\n{globals}\n{defs}{main_def}"
    ))
}

#[allow(clippy::too_many_arguments)]
fn emit_function(
    name: &str,
    params: &[String],
    body: &[Stmt],
    funcs: &HashSet<String>,
    borrow_masks: &HashMap<String, Vec<bool>>,
    param_reprs: &HashMap<String, Vec<Repr>>,
    ret_reprs: &HashMap<String, Repr>,
) -> Result<(String, String), String> {
    let mut f = FuncEmitter::new(funcs, borrow_masks, param_reprs, ret_reprs, name);
    let mask = borrow_masks.get(name).cloned().unwrap_or_default();
    let preprs = param_reprs.get(name).cloned().unwrap_or_default();
    f.ret_repr = ret_reprs.get(name).copied().unwrap_or(Repr::Boxed);
    for (i, p) in params.iter().enumerate() {
        let pr = preprs.get(i).copied().unwrap_or(Repr::Boxed);
        let ptr = f.ensure_slot(p, pr); // typed slot (double for a float param)
        f.line(&format!("store {} %a{i}, ptr {ptr}", llvm_ty(pr)));
        // A heap-ref param (Boxed or a packed array) that doesn't escape is
        // borrowed: the caller keeps ownership, so we don't release it at exit.
        // Unboxed scalars carry no refcount, so borrow-tracking doesn't apply.
        if is_heap_repr(pr) && mask.get(i).copied().unwrap_or(false) {
            f.borrowed_params.push(p.clone());
        }
    }
    f.block(body)?;
    if !f.terminated {
        // Implicit return on fall-through: release locals, then return None
        // (boxed) or a raw zero for an unboxed-return function that fell off.
        f.emit_exit_releases();
        let r = match f.ret_repr {
            Repr::Int => "0".to_string(),
            Repr::Float => "0.0".to_string(),
            _ => f.call_value("call i32 @p2w_none()"),
        };
        f.body
            .push_str(&format!("  ret {} {r}\n", llvm_ty(f.ret_repr)));
    }
    let sig: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, _)| {
            format!(
                "{} %a{i}",
                llvm_ty(preprs.get(i).copied().unwrap_or(Repr::Boxed))
            )
        })
        .collect();
    let def = format!(
        "define {} @{name}({}) {{\nentry:\n{}{}}}\n",
        llvm_ty(f.ret_repr),
        sig.join(", "),
        f.allocas,
        f.body
    );
    Ok((def, f.globals))
}

fn emit_main(
    top: &[&Stmt],
    funcs: &HashSet<String>,
    borrow_masks: &HashMap<String, Vec<bool>>,
    param_reprs: &HashMap<String, Vec<Repr>>,
    ret_reprs: &HashMap<String, Repr>,
) -> Result<(String, String), String> {
    let mut f = FuncEmitter::new(funcs, borrow_masks, param_reprs, ret_reprs, "main");
    for s in top {
        f.stmt(s)?;
    }
    if !f.terminated {
        // Release all top-level locals so a finished program ends at live==0.
        f.emit_exit_releases();
        f.body.push_str("  ret i32 0\n");
    }
    let def = format!("define i32 @main() {{\nentry:\n{}{}}}\n", f.allocas, f.body);
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

/// Static representation of a value as the emitter tracks it. `Boxed` is the
/// universal tagged-`i32` (the dynamic default). Unboxed reprs are raw machine
/// values produced where the type is statically known; `as_boxed` coerces back
/// at dynamic sinks. See VALUE_MODEL.md. (Stage 1: `Int` only; `Float`/`Bool`/
/// packed arrays follow.)
#[derive(Clone, Copy, PartialEq)]
enum Repr {
    Boxed,
    Int,
    /// An unboxed `i1` — produced by native integer comparisons; used directly as
    /// a branch condition, boxed to True/False (`p2w_bool`) at a dynamic sink.
    Bool,
    /// An unboxed `double` — produced by float literals/arithmetic; boxed with
    /// `p2w_float` (a heap f64) at a dynamic sink. Transient (no float slots yet).
    Float,
    /// A `list[int]`: the value is a heap reference (an `i32`, like `Boxed`, and
    /// refcounted the same way), but elements are raw ints accessed via the
    /// `p2w_iarray_*` ABI. See VALUE_MODEL.md (Phase C).
    IntArray,
    /// A `list[float]`: like `IntArray` but elements are raw `double`s
    /// (`p2w_farray_*` ABI).
    FloatArray,
}

/// True if a value of this repr is a heap reference (`Boxed`/`IntArray`/
/// `FloatArray`) that participates in reference counting — retained on owned
/// load, released at scope exit. Unboxed scalars (Int/Float/Bool) are not.
fn is_heap_repr(r: Repr) -> bool {
    matches!(r, Repr::Boxed | Repr::IntArray | Repr::FloatArray)
}

/// True if boxing this repr (`as_boxed`) allocates a fresh owned temp — i.e. an
/// unboxed scalar. `Boxed`/`IntArray` are already Values, so `as_boxed` is a
/// no-op for them.
fn boxes_to_new_temp(r: Repr) -> bool {
    matches!(r, Repr::Int | Repr::Float | Repr::Bool)
}

/// Per-function emission state. Values are tagged `i32`; variables are
/// entry-block `alloca`s; control flow uses labelled basic blocks.
struct FuncEmitter<'a> {
    funcs: &'a HashSet<String>,
    /// Per-function borrowed-parameter masks (function name → one bool per param,
    /// `true` = borrowed). Used to emit the matching convention at call sites.
    borrow_masks: &'a HashMap<String, Vec<bool>>,
    /// Per-function parameter representations (function name → one `Repr` per
    /// param) and return representations, so call sites coerce args/results.
    param_reprs: &'a HashMap<String, Vec<Repr>>,
    ret_reprs: &'a HashMap<String, Repr>,
    /// This function's own parameters that are borrowed (slots we must NOT
    /// release at exit — the caller still owns them).
    borrowed_params: Vec<String>,
    /// This function's return representation (what `return` coerces to).
    ret_repr: Repr,
    /// Representation of each variable slot (default `Boxed`; typed params set
    /// theirs). Drives load typing, assignment coercion, and exit-release.
    var_reprs: HashMap<String, Repr>,
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
    fn new(
        funcs: &'a HashSet<String>,
        borrow_masks: &'a HashMap<String, Vec<bool>>,
        param_reprs: &'a HashMap<String, Vec<Repr>>,
        ret_reprs: &'a HashMap<String, Repr>,
        gprefix: &str,
    ) -> Self {
        FuncEmitter {
            funcs,
            borrow_masks,
            param_reprs,
            ret_reprs,
            borrowed_params: Vec::new(),
            ret_repr: Repr::Boxed,
            var_reprs: HashMap::new(),
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
            // Borrowed params are owned by the caller — don't release them.
            if self.borrowed_params.contains(&name) {
                continue;
            }
            // Only heap-ref slots (Boxed/IntArray) carry a refcount to release;
            // unboxed scalars don't.
            if !is_heap_repr(self.slot_repr(&name)) {
                continue;
            }
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

    /// Get the slot for `name`, creating it with representation `repr` (and the
    /// matching LLVM alloca type) on first use. An existing slot keeps its repr.
    fn ensure_slot(&mut self, name: &str, repr: Repr) -> String {
        let ptr = format!("%v_{name}");
        if !self.vars.iter().any(|v| v == name) {
            let ty = llvm_ty(repr);
            self.allocas.push_str(&format!("  {ptr} = alloca {ty}\n"));
            // Zero-init so the exit-release of a never-assigned (Boxed) slot is a
            // no-op (0 isn't a heap value, so p2w_release ignores it).
            self.allocas
                .push_str(&format!("  store {ty} {}, ptr {ptr}\n", zero_init(repr)));
            self.vars.push(name.to_string());
            self.var_reprs.insert(name.to_string(), repr);
        }
        ptr
    }

    /// The common case: a Boxed slot (plain `x = …` locals).
    fn var_slot(&mut self, name: &str) -> String {
        self.ensure_slot(name, Repr::Boxed)
    }

    /// The representation a variable slot holds (default `Boxed`).
    fn slot_repr(&self, name: &str) -> Repr {
        self.var_reprs.get(name).copied().unwrap_or(Repr::Boxed)
    }

    /// Load a variable as a typed value `(operand, repr)` — raw for an `Int`
    /// slot, the tagged value for a `Boxed` slot. No refcount traffic here; the
    /// caller decides whether to retain (owned use) or borrow.
    fn load_name(&mut self, name: &str) -> (String, Repr) {
        let repr = self.slot_repr(name);
        let t = self.call_value(&format!("load {}, ptr %v_{name}", llvm_ty(repr)));
        (t, repr)
    }

    /// Coerce a value from `from` to `to`, emitting a box/unbox only when needed.
    /// Slot/param/return targets are only `Boxed` or `Int` today.
    fn coerce(&mut self, op: String, from: Repr, to: Repr) -> String {
        if from == to {
            return op;
        }
        match to {
            Repr::Boxed => self.as_boxed(op, from),
            Repr::Int => match from {
                Repr::Float => {
                    let t = self.temp();
                    self.line(&format!("{t} = fptosi double {op} to i32"));
                    t
                }
                _ => {
                    // Box any non-boxed source, then unbox to a raw int.
                    let boxed = self.as_boxed(op, from);
                    self.call_value(&format!("call i32 @p2w_unbox_int(i32 {boxed})"))
                }
            },
            Repr::Float => self.promote_double(op, from),
            Repr::Bool => self.as_boxed(op, from), // no Bool targets yet
            // A packed-array slot is only ever fed a matching array (identity) or
            // built directly from a literal (build_*array, which bypasses coerce).
            Repr::IntArray | Repr::FloatArray => op,
        }
    }

    /// Evaluate `e` to a raw unboxed `i32`, unboxing (and releasing) a boxed
    /// result. Used for native loop bounds/counters.
    fn expr_int(&mut self, e: &Expr) -> Result<String, String> {
        let (v, vr) = self.expr_typed(e)?;
        Ok(match vr {
            Repr::Int => v,
            Repr::Boxed => {
                let raw = self.coerce(v.clone(), Repr::Boxed, Repr::Int);
                self.release(&v); // drop the boxed temp we unboxed
                raw
            }
            Repr::Bool => self.coerce(v, Repr::Bool, Repr::Int), // rare; i1, no RC
            Repr::Float => self.coerce(v, Repr::Float, Repr::Int), // fptosi (rare)
            Repr::IntArray | Repr::FloatArray => {
                // An array where an int is expected — coerce traps at runtime.
                let raw = self.coerce(v.clone(), vr, Repr::Int);
                self.release(&v);
                raw
            }
        })
    }

    /// Evaluate `e` to a raw unboxed `double` (int → `sitofp`, float as-is, boxed
    /// → `p2w_unbox_float` + release). Used for packed float-array elements.
    fn expr_double(&mut self, e: &Expr) -> Result<String, String> {
        let (v, vr) = self.expr_typed(e)?;
        Ok(match vr {
            Repr::Float => v,
            Repr::Boxed => {
                let raw = self.promote_double(v.clone(), Repr::Boxed);
                self.release(&v); // drop the boxed temp we unboxed
                raw
            }
            _ => self.promote_double(v, vr), // Int -> sitofp; others trap at runtime
        })
    }

    /// Build a packed `list[int]` from literal elements (each lowered to a raw
    /// int). Returns an owned `IntArray` reference.
    fn build_iarray(&mut self, items: &[Expr]) -> Result<String, String> {
        let arr = self.call_value("call i32 @p2w_iarray_new()");
        for it in items {
            let raw = self.expr_int(it)?;
            self.line(&format!("call void @p2w_iarray_push(i32 {arr}, i32 {raw})"));
        }
        Ok(arr)
    }

    /// Build a packed `list[float]` from literal elements (each lowered to a raw
    /// double). Returns an owned `FloatArray` reference.
    fn build_farray(&mut self, items: &[Expr]) -> Result<String, String> {
        let arr = self.call_value("call i32 @p2w_farray_new()");
        for it in items {
            let raw = self.expr_double(it)?;
            self.line(&format!(
                "call void @p2w_farray_push(i32 {arr}, double {raw})"
            ));
        }
        Ok(arr)
    }

    /// Evaluate `value` for assignment into a slot of representation `slot`. A
    /// packed-array slot fed a list literal is built packed; a comprehension is
    /// built to match the slot; everything else is the usual typed evaluation.
    fn eval_for_slot(&mut self, slot: Repr, value: &Expr) -> Result<(String, Repr), String> {
        match &value.kind {
            ExprKind::List(items) => match slot {
                Repr::IntArray => return Ok((self.build_iarray(items)?, Repr::IntArray)),
                Repr::FloatArray => return Ok((self.build_farray(items)?, Repr::FloatArray)),
                _ => {}
            },
            ExprKind::ListComp { element, clauses } => {
                return Ok((self.build_comprehension(slot, element, clauses)?, slot));
            }
            _ => {}
        }
        self.expr_typed(value)
    }

    /// FBIP drop-reuse: lower `data = [f(x) for x in data]` over a packed array
    /// to an in-place map *when the array is uniquely owned at runtime*. Emits a
    /// branch: `if unique(data)` → overwrite each element in place (zero
    /// allocation); else → build a fresh array and reassign (so an aliased
    /// original is never mutated). Returns `true` if it handled the assignment.
    ///
    /// Only fires for a filterless, single-target self-map whose element doesn't
    /// read the array — exactly the case where in-place equals copy semantics.
    fn try_inplace_map(
        &mut self,
        name: &str,
        element: &Expr,
        clauses: &[CompClause],
    ) -> Result<bool, String> {
        if !self.vars.iter().any(|v| v == name) {
            return Ok(false);
        }
        let slot_repr = self.slot_repr(name);
        let elem_repr = match slot_repr {
            Repr::IntArray => Repr::Int,
            Repr::FloatArray => Repr::Float,
            _ => return Ok(false),
        };
        // Exactly `for x in <same name>` — no filters (length-preserving).
        if clauses.len() != 1 {
            return Ok(false);
        }
        let loopvar = match &clauses[0] {
            CompClause::For { vars, iter }
                if vars.len() == 1 && matches!(&iter.kind, ExprKind::Name(n) if n == name) =>
            {
                vars[0].clone()
            }
            _ => return Ok(false),
        };
        // The element must not read the array it overwrites (else in-place would
        // change values a later iteration reads).
        if expr_uses_name(element, name) {
            return Ok(false);
        }

        let line = element.line;
        let arr = self.call_value(&format!("load i32, ptr %v_{name}")); // borrow
        let uniq = self.temp();
        self.line(&format!("{uniq} = call i1 @p2w_unique(i32 {arr})"));
        let reuse = self.fresh_label("reuse");
        let copy = self.fresh_label("copy");
        let endl = self.fresh_label("mapend");
        self.terminator(&format!("br i1 {uniq}, label %{reuse}, label %{copy}"));

        // Reuse: for __i in range(len(data)): x = data[__i]; data[__i] = f(x)
        self.place_label(&reuse);
        self.ensure_slot(&loopvar, elem_repr); // native typed loop var
        let id = self.next_label;
        self.next_label += 1;
        let iname = format!("__map{id}");
        let mk = |k: ExprKind| Expr { kind: k, line };
        let arr_name = || mk(ExprKind::Name(name.to_string()));
        let idx = || mk(ExprKind::Name(iname.clone()));
        let body = vec![
            Stmt {
                kind: StmtKind::Assign(
                    loopvar.clone(),
                    mk(ExprKind::Index(Box::new(arr_name()), Box::new(idx()))),
                ),
                line,
            },
            Stmt {
                kind: StmtKind::SetIndex {
                    target: arr_name(),
                    index: idx(),
                    value: element.clone(),
                },
                line,
            },
        ];
        let len = mk(ExprKind::Call("len".to_string(), vec![arr_name()]));
        self.emit_for(
            &iname,
            &mk(ExprKind::Int(0)),
            &len,
            &mk(ExprKind::Int(1)),
            &body,
        )?;
        self.br_to(&endl);

        // Copy: build a fresh array and reassign (releases the old binding).
        self.place_label(&copy);
        let new = self.build_comprehension(slot_repr, element, clauses)?;
        let ptr = format!("%v_{name}");
        self.store_var(name, &ptr, new, slot_repr);
        self.br_to(&endl);

        self.place_label(&endl);
        Ok(true)
    }

    /// Build the nested loop/filter body of a comprehension: each `for` clause
    /// becomes a loop (counted `range` or iterating) wrapping the rest, each `if`
    /// a guard around it. `inner` is the innermost statement (the append or
    /// dict-set). Supports multiple `for`s — nested comprehensions. Tuple targets
    /// (`for a, b in ...`) aren't handled yet.
    fn comp_body(clauses: &[CompClause], inner: Stmt, line: usize) -> Result<Vec<Stmt>, String> {
        let Some((first, rest)) = clauses.split_first() else {
            return Ok(vec![inner]);
        };
        match first {
            CompClause::If(cond) => Ok(vec![Stmt {
                kind: StmtKind::If {
                    cond: cond.clone(),
                    body: Self::comp_body(rest, inner, line)?,
                    elifs: vec![],
                    else_body: None,
                },
                line,
            }]),
            CompClause::For { vars, iter } => {
                if vars.len() != 1 {
                    return Err(format!(
                        "line {line}: tuple targets in comprehensions aren't in the native backend yet"
                    ));
                }
                let var = vars[0].clone();
                let body = Self::comp_body(rest, inner, line)?;
                // `range(...)` isn't an iterable object — lower it to a counted
                // loop, like a `for i in range(...)` statement; else iterate.
                let kind = match &iter.kind {
                    ExprKind::Call(n, args) if n == "range" => {
                        let lit = |v: i64| Expr {
                            kind: ExprKind::Int(v),
                            line,
                        };
                        let (start, end, step) = match args.len() {
                            1 => (lit(0), args[0].clone(), lit(1)),
                            2 => (args[0].clone(), args[1].clone(), lit(1)),
                            3 => (args[0].clone(), args[1].clone(), args[2].clone()),
                            _ => return Err(format!("line {line}: range() takes 1-3 arguments")),
                        };
                        StmtKind::For {
                            var,
                            start,
                            end,
                            step,
                            body,
                        }
                    }
                    _ => StmtKind::ForEach {
                        var,
                        iterable: iter.clone(),
                        body,
                    },
                };
                Ok(vec![Stmt { kind, line }])
            }
        }
    }

    /// Lower `[element for ... (if ...)*]` into a fresh result collection (typed
    /// to `result_repr`) plus the nested loop that appends each produced element.
    /// Reuses the loop + typed-`append` machinery, so it works over dynamic lists
    /// and packed `list[int]`/`list[float]` alike. Returns an owned reference.
    fn build_comprehension(
        &mut self,
        result_repr: Repr,
        element: &Expr,
        clauses: &[CompClause],
    ) -> Result<String, String> {
        let line = element.line;
        let id = self.next_label;
        self.next_label += 1;
        let rname = format!("__comp{id}");
        let ptr = self.ensure_slot(&rname, result_repr);
        let empty = match result_repr {
            Repr::IntArray => self.call_value("call i32 @p2w_iarray_new()"),
            Repr::FloatArray => self.call_value("call i32 @p2w_farray_new()"),
            _ => self.call_value("call i32 @p2w_list_new()"),
        };
        self.store_var(&rname, &ptr, empty, result_repr);

        let mk = |k: ExprKind| Expr { kind: k, line };
        let append = Stmt {
            kind: StmtKind::Expr(mk(ExprKind::MethodCall(
                Box::new(mk(ExprKind::Name(rname.clone()))),
                "append".to_string(),
                vec![element.clone()],
            ))),
            line,
        };
        let body = Self::comp_body(clauses, append, line)?;
        self.block(&body)?;

        let (t, _) = self.load_name(&rname);
        self.retain(&t);
        Ok(t)
    }

    /// Lower `{key: value for ... (if ...)*}` into a fresh (dynamic) dict, setting
    /// each key/value pair. Returns an owned reference.
    fn build_dict_comprehension(
        &mut self,
        key: &Expr,
        value: &Expr,
        clauses: &[CompClause],
    ) -> Result<String, String> {
        let line = key.line;
        let id = self.next_label;
        self.next_label += 1;
        let rname = format!("__comp{id}");
        let ptr = self.ensure_slot(&rname, Repr::Boxed); // dicts are dynamic
        let empty = self.call_value("call i32 @p2w_dict_new()");
        self.store_var(&rname, &ptr, empty, Repr::Boxed);

        let mk = |k: ExprKind| Expr { kind: k, line };
        let set = Stmt {
            kind: StmtKind::SetIndex {
                target: mk(ExprKind::Name(rname.clone())),
                index: key.clone(),
                value: value.clone(),
            },
            line,
        };
        let body = Self::comp_body(clauses, set, line)?;
        self.block(&body)?;

        let (t, _) = self.load_name(&rname);
        self.retain(&t);
        Ok(t)
    }

    /// Store value `(v, vr)` into the slot for `name`, coercing to the slot's
    /// repr. A Boxed slot releases its previous binding then transfers in; an
    /// unboxed Int slot stores raw (unboxing a boxed RHS drops that temp).
    fn store_var(&mut self, name: &str, ptr: &str, v: String, vr: Repr) {
        let slot = self.slot_repr(name);
        if is_heap_repr(slot) {
            let cv = self.coerce(v, vr, slot);
            let old = self.temp();
            self.line(&format!("{old} = load i32, ptr {ptr}"));
            self.release(&old); // drop the previous binding (no-op if 0/inline)
            self.line(&format!("store i32 {cv}, ptr {ptr}"));
        } else {
            // Unboxed slot (Int or Float): coerce to the slot's repr and store
            // with its LLVM type — no refcount. Unboxing a boxed RHS drops it.
            let raw = if vr == Repr::Boxed {
                let r = self.coerce(v.clone(), Repr::Boxed, slot);
                self.release(&v);
                r
            } else {
                self.coerce(v, vr, slot)
            };
            self.line(&format!("store {} {raw}, ptr {ptr}", llvm_ty(slot)));
        }
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
                // FBIP: `data = [f(x) for x in data]` over a unique packed array
                // maps in place (zero allocation); otherwise it falls through.
                if let ExprKind::ListComp { element, clauses } = &value.kind
                    && self.try_inplace_map(name, element, clauses)?
                {
                    return Ok(());
                }
                // Reassigning an existing IntArray slot with a literal rebuilds it
                // packed; a new/Boxed slot evaluates normally.
                let slot = if self.vars.iter().any(|x| x == name) {
                    self.slot_repr(name)
                } else {
                    Repr::Boxed
                };
                let (v, vr) = self.eval_for_slot(slot, value)?;
                let ptr = self.var_slot(name); // new locals are created Boxed
                self.store_var(name, &ptr, v, vr);
                Ok(())
            }
            StmtKind::AnnAssign { name, ann, value } => {
                // On first definition the annotation picks the slot's repr (and
                // alloca type): `total: int = 0`, `x: float = …`, `xs: list[int] = […]`.
                let slot_repr = repr_of_ann(&Some(ann.clone()));
                let (v, vr) = self.eval_for_slot(slot_repr, value)?;
                let ptr = self.ensure_slot(name, slot_repr);
                self.store_var(name, &ptr, v, vr);
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
                let (t, trepr, towned) = self.expr_borrow_typed(target)?;
                if trepr == Repr::IntArray {
                    // Packed array: raw index + raw value, bounds-checked set.
                    let i = self.expr_int(index)?;
                    let val = self.expr_int(value)?;
                    self.line(&format!(
                        "call void @p2w_iarray_set(i32 {t}, i32 {i}, i32 {val})"
                    ));
                    self.release_if_owned(&t, towned);
                    return Ok(());
                }
                if trepr == Repr::FloatArray {
                    let i = self.expr_int(index)?;
                    let val = self.expr_double(value)?;
                    self.line(&format!(
                        "call void @p2w_farray_set(i32 {t}, i32 {i}, double {val})"
                    ));
                    self.release_if_owned(&t, towned);
                    return Ok(());
                }
                let tb = self.as_boxed(t, trepr);
                let i = self.expr(index)?; // dict: key transferred to the runtime
                let v = self.expr(value)?; //       value transferred too
                self.line(&format!(
                    "call void @p2w_setindex(i32 {tb}, i32 {i}, i32 {v})"
                ));
                // Only the container is borrowed. The index/key is NOT released
                // here: for a list it's an inline int position; for a dict the
                // runtime takes ownership of the key (storing it, or releasing it
                // as redundant on update) — releasing it here would double-free.
                self.release_if_owned(&tb, towned || boxes_to_new_temp(trepr));
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
                let ret = self.ret_repr;
                let (v, vr) = match value {
                    // Build to the return repr, so `return [..]` from a
                    // `-> list[int]` function produces a packed array, not a
                    // dynamic list.
                    Some(e) => self.eval_for_slot(ret, e)?,
                    None => (self.call_value("call i32 @p2w_none()"), Repr::Boxed),
                };
                let r = if vr == Repr::Boxed && ret != Repr::Boxed {
                    // Returning a boxed value as an unboxed type: unbox, then drop
                    // the boxed temp.
                    let raw = self.coerce(v.clone(), Repr::Boxed, ret);
                    self.release(&v);
                    raw
                } else {
                    self.coerce(v, vr, ret)
                };
                self.emit_exit_releases();
                self.terminator(&format!("ret {} {r}", llvm_ty(ret)));
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
        // A counted range loop is fully native: a raw i32 counter, an `icmp`
        // guard, and a raw `add` increment — no boxing, no runtime calls. The
        // bound is a raw i32 held across the loop (no refcount, so no cleanup).
        let start_v = self.expr_int(start)?;
        let end_v = self.expr_int(end_expr)?;
        // Only an existing boxed binding of this name needs releasing before we
        // repurpose the slot as a native int counter; a fresh counter does not.
        let existed = self.vars.iter().any(|v| v == var);
        let slot = self.var_slot(var);
        if existed && self.slot_repr(var) == Repr::Boxed {
            let old = self.temp();
            self.line(&format!("{old} = load i32, ptr {slot}"));
            self.release(&old);
        }
        self.var_reprs.insert(var.to_string(), Repr::Int);
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
        let pred = if step_lit > 0 { "slt" } else { "sgt" };
        let cond = self.temp();
        self.line(&format!("{cond} = icmp {pred} i32 {iv}, {end_v}"));
        self.terminator(&format!("br i1 {cond}, label %{body_l}, label %{end}"));

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

    /// `for var in iterable:` over the runtime iteration protocol
    /// (`p2w_iter` / `p2w_iter_has` / `p2w_iter_next`).
    fn emit_foreach(&mut self, var: &str, iterable: &Expr, body: &[Stmt]) -> Result<(), String> {
        let (seq, srepr) = self.expr_typed(iterable)?; // owned
        if srepr == Repr::IntArray {
            return self.emit_foreach_array(var, seq, Repr::Int, "p2w_iarray_get", body);
        }
        if srepr == Repr::FloatArray {
            return self.emit_foreach_array(var, seq, Repr::Float, "p2w_farray_get", body);
        }
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

    /// `for var in xs:` over a packed array — lowered to a native index loop with
    /// a raw element getter (`get_fn`), so the loop variable is an unboxed scalar
    /// of `elem` repr (Int for list[int], Float for list[float]).
    fn emit_foreach_array(
        &mut self,
        var: &str,
        seq: String,
        elem: Repr,
        get_fn: &str,
        body: &[Stmt],
    ) -> Result<(), String> {
        // seq is an owned array ref; it must survive the loop and early return.
        self.cleanups.push(seq.clone());
        let lenb = self.call_value(&format!("call i32 @p2w_len(i32 {seq})"));
        let len = self.call_value(&format!("call i32 @p2w_unbox_int(i32 {lenb})"));
        self.release(&lenb); // drop the boxed length temp (no-op for a small int)
        // Hidden index counter (not a user variable).
        let id = self.next_label;
        self.next_label += 1;
        let ix = format!("%ix{id}");
        self.allocas
            .push_str(&format!("  {ix} = alloca i32\n  store i32 0, ptr {ix}\n"));
        let existed = self.vars.iter().any(|v| v == var);
        let slot = self.ensure_slot(var, elem); // typed loop-var slot (i32 / double)
        if existed && llvm_ty(self.slot_repr(var)) != llvm_ty(elem) {
            return Err(format!(
                "the loop variable '{var}' is reused with an incompatible type"
            ));
        }
        if existed && is_heap_repr(self.slot_repr(var)) {
            let old = self.temp();
            self.line(&format!("{old} = load i32, ptr {slot}"));
            self.release(&old);
        }
        self.var_reprs.insert(var.to_string(), elem); // loop var is an unboxed scalar

        let head = self.fresh_label("ahead");
        let body_l = self.fresh_label("abody");
        let cont = self.fresh_label("acont");
        let end = self.fresh_label("aend");

        self.br_to(&head);
        self.place_label(&head);
        let i = self.temp();
        self.line(&format!("{i} = load i32, ptr {ix}"));
        let c = self.temp();
        self.line(&format!("{c} = icmp slt i32 {i}, {len}"));
        self.terminator(&format!("br i1 {c}, label %{body_l}, label %{end}"));

        self.place_label(&body_l);
        let iv = self.temp();
        self.line(&format!("{iv} = load i32, ptr {ix}"));
        let ety = llvm_ty(elem);
        let e = self.call_value(&format!("call {ety} @{get_fn}(i32 {seq}, i32 {iv})"));
        self.line(&format!("store {ety} {e}, ptr {slot}")); // unboxed scalar: no RC
        self.loops.push((cont.clone(), end.clone()));
        self.block(body)?;
        self.loops.pop();
        self.br_to(&cont);

        self.place_label(&cont);
        let cur = self.temp();
        self.line(&format!("{cur} = load i32, ptr {ix}"));
        let inc = self.temp();
        self.line(&format!("{inc} = add i32 {cur}, 1"));
        self.line(&format!("store i32 {inc}, ptr {ix}"));
        self.br_to(&head);

        self.place_label(&end);
        self.cleanups.pop();
        self.release(&seq);
        Ok(())
    }

    /// Evaluate a condition to an `i1` via the runtime's truthiness.
    fn cond_i1(&mut self, cond: &Expr) -> Result<String, String> {
        let (v, repr, owned) = self.expr_borrow_typed(cond)?;
        // A native comparison is already an i1 — branch on it directly, no
        // boxing and no p2w_truthy.
        if repr == Repr::Bool {
            return Ok(v);
        }
        let boxed = self.as_boxed(v, repr);
        let t = self.temp();
        self.line(&format!("{t} = call i1 @p2w_truthy(i32 {boxed})"));
        // Release the operand if it was owned, or if boxing it made a temp.
        self.release_if_owned(&boxed, owned || boxes_to_new_temp(repr));
        Ok(t)
    }

    /// Evaluate an expression to a typed value `(operand, Repr)`. Most arms are
    /// `Boxed` (the universal tagged-`i32`); unboxed reprs are produced where the
    /// static type is known. `as_boxed` coerces back at dynamic sinks. See
    /// VALUE_MODEL.md.
    fn expr_typed(&mut self, e: &Expr) -> Result<(String, Repr), String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                e.line
            ))
        };
        match &e.kind {
            // A raw int literal is unboxed; it only becomes a boxed p2w_int when
            // it reaches a dynamic sink (via as_boxed).
            ExprKind::Int(n) => Ok((format!("{}", *n as i32), Repr::Int)),
            // An unboxed double literal (hex form is exact); boxed only at a sink.
            ExprKind::Float(f) => Ok((fmt_double(*f), Repr::Float)),
            ExprKind::Bool(b) => {
                let v = self.call_value(&format!(
                    "call i32 @p2w_bool(i1 {})",
                    if *b { 1 } else { 0 }
                ));
                Ok((v, Repr::Boxed))
            }
            ExprKind::NoneLit => Ok((self.call_value("call i32 @p2w_none()"), Repr::Boxed)),
            ExprKind::Str(s) => {
                let bytes = s.as_bytes();
                let g = self.intern_str(bytes);
                let v =
                    self.call_value(&format!("call i32 @p2w_str(ptr {g}, i32 {})", bytes.len()));
                Ok((v, Repr::Boxed))
            }
            ExprKind::Name(name) => {
                if !self.vars.iter().any(|v| v == name) {
                    return Err(format!("line {}: name '{name}' is not defined", e.line));
                }
                let (t, repr) = self.load_name(name);
                // Only a heap-bearing load (Boxed/IntArray) retains to become
                // owned; an unboxed scalar carries no refcount.
                if is_heap_repr(repr) {
                    self.retain(&t);
                }
                Ok((t, repr))
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let (v, o) = self.expr_borrow(inner)?;
                let r = self.call_value(&format!("call i32 @p2w_neg(i32 {v})"));
                self.release_if_owned(&v, o);
                Ok((r, Repr::Boxed))
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let (v, o) = self.expr_borrow(inner)?;
                let r = self.call_value(&format!("call i32 @p2w_not(i32 {v})"));
                self.release_if_owned(&v, o);
                Ok((r, Repr::Boxed))
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
                    return Ok((r, Repr::Boxed));
                }
                if !self.funcs.contains(name) {
                    return nope("calling this function (only your own functions, len, + print)");
                }
                // Coerce each argument to the callee's parameter repr and match
                // its ownership convention. An `Int` param takes a raw value (no
                // refcount). A `Boxed` param is borrowed (no transfer) or owned
                // (transferred — the callee releases it) per the borrow mask.
                let mask = self.borrow_masks.get(name).cloned().unwrap_or_default();
                let preprs = self.param_reprs.get(name).cloned().unwrap_or_default();
                let ret_repr = self.ret_reprs.get(name).copied().unwrap_or(Repr::Boxed);
                let mut ops = Vec::with_capacity(args.len());
                let mut to_release = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let prepr = preprs.get(i).copied().unwrap_or(Repr::Boxed);
                    let borrowable = mask.get(i).copied().unwrap_or(false);
                    if !is_heap_repr(prepr) {
                        // Unboxed scalar param (Int/Float): pass by value, no
                        // refcount. A boxed arg unboxes (dropping its temp).
                        let (v, vr) = self.eval_for_slot(prepr, a)?;
                        let raw = if vr == Repr::Boxed {
                            let r = self.coerce(v.clone(), Repr::Boxed, prepr);
                            self.release(&v);
                            r
                        } else {
                            self.coerce(v, vr, prepr)
                        };
                        ops.push(format!("{} {raw}", llvm_ty(prepr)));
                    } else if borrowable && matches!(&a.kind, ExprKind::Name(_)) {
                        // Borrowed heap param (Boxed/array) fed a named value:
                        // load it without retaining and don't release — the
                        // caller keeps ownership, the callee won't free it.
                        let (v, _, _) = self.expr_borrow_typed(a)?;
                        ops.push(format!("i32 {v}"));
                    } else {
                        // Transfer (callee releases), or a borrowed param fed a
                        // fresh temp (the caller releases after the call). Either
                        // way build an owned value; coerce is identity among heap
                        // reprs and builds a packed array from a literal.
                        let (v, vr) = self.eval_for_slot(prepr, a)?;
                        let v = self.coerce(v, vr, prepr);
                        if borrowable {
                            to_release.push(v.clone());
                        }
                        ops.push(format!("i32 {v}"));
                    }
                }
                let r = self.call_value(&format!(
                    "call {} @{name}({})",
                    llvm_ty(ret_repr),
                    ops.join(", ")
                ));
                // Release borrowed args that were owned temps (a Name borrow isn't).
                for v in to_release {
                    self.release(&v);
                }
                Ok((r, ret_repr))
            }
            ExprKind::List(items) => {
                let list = self.call_value("call i32 @p2w_list_new()");
                for it in items {
                    let v = self.expr(it)?;
                    self.line(&format!("call i32 @p2w_list_append(i32 {list}, i32 {v})"));
                }
                Ok((list, Repr::Boxed))
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
                Ok((dict, Repr::Boxed))
            }
            ExprKind::Index(obj, idx) => {
                let (o, orepr, oo) = self.expr_borrow_typed(obj)?;
                if orepr == Repr::IntArray {
                    // Packed array: raw bounds-checked get, unboxed Int result.
                    let i = self.expr_int(idx)?;
                    let r = self.call_value(&format!("call i32 @p2w_iarray_get(i32 {o}, i32 {i})"));
                    self.release_if_owned(&o, oo);
                    return Ok((r, Repr::Int));
                }
                if orepr == Repr::FloatArray {
                    let i = self.expr_int(idx)?;
                    let r =
                        self.call_value(&format!("call double @p2w_farray_get(i32 {o}, i32 {i})"));
                    self.release_if_owned(&o, oo);
                    return Ok((r, Repr::Float));
                }
                let ob = self.as_boxed(o, orepr);
                let (i, oi) = self.expr_borrow(idx)?;
                let r = self.call_value(&format!("call i32 @p2w_index(i32 {ob}, i32 {i})"));
                self.release_if_owned(&ob, oo || boxes_to_new_temp(orepr));
                self.release_if_owned(&i, oi);
                Ok((r, Repr::Boxed))
            }
            ExprKind::MethodCall(obj, method, args) => {
                Ok((self.method_call(obj, method, args)?, Repr::Boxed))
            }
            // A comprehension with no typing context builds a dynamic list;
            // eval_for_slot builds a packed one when the target is list[int/float].
            ExprKind::ListComp { element, clauses } => Ok((
                self.build_comprehension(Repr::Boxed, element, clauses)?,
                Repr::Boxed,
            )),
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => Ok((
                self.build_dict_comprehension(key, value, clauses)?,
                Repr::Boxed,
            )),
            _ => nope("this expression"),
        }
    }

    /// Evaluate an expression to a boxed tagged-`i32` value — the representation
    /// every runtime ABI call expects. Coerces unboxed reprs at the boundary.
    fn expr(&mut self, e: &Expr) -> Result<String, String> {
        let (op, repr) = self.expr_typed(e)?;
        Ok(self.as_boxed(op, repr))
    }

    /// Coerce a typed value to the boxed representation, emitting the box only
    /// when it isn't already boxed.
    fn as_boxed(&mut self, op: String, repr: Repr) -> String {
        match repr {
            Repr::Boxed => op,
            Repr::Int => self.call_value(&format!("call i32 @p2w_int(i32 {op})")),
            Repr::Bool => self.call_value(&format!("call i32 @p2w_bool(i1 {op})")),
            Repr::Float => self.call_value(&format!("call i32 @p2w_float(double {op})")),
            Repr::IntArray | Repr::FloatArray => op, // already a heap-ref Value
        }
    }

    /// Coerce a numeric operand to a raw `double` (int → `sitofp`, float as-is,
    /// boxed → `p2w_unbox_float`). Used by native float arithmetic.
    fn promote_double(&mut self, op: String, from: Repr) -> String {
        match from {
            Repr::Float => op,
            Repr::Int => {
                let t = self.temp();
                self.line(&format!("{t} = sitofp i32 {op} to double"));
                t
            }
            _ => self.call_value(&format!("call double @p2w_unbox_float(i32 {op})")),
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
        let (recv, rrepr, rowned) = self.expr_borrow_typed(obj)?;
        // Packed array: xs.append(n) pushes a raw int and returns None.
        if rrepr == Repr::IntArray && method == "append" && args.len() == 1 {
            let raw = self.expr_int(&args[0])?;
            self.line(&format!(
                "call void @p2w_iarray_push(i32 {recv}, i32 {raw})"
            ));
            self.release_if_owned(&recv, rowned);
            return Ok(self.call_value("call i32 @p2w_none()"));
        }
        if rrepr == Repr::FloatArray && method == "append" && args.len() == 1 {
            let raw = self.expr_double(&args[0])?;
            self.line(&format!(
                "call void @p2w_farray_push(i32 {recv}, double {raw})"
            ));
            self.release_if_owned(&recv, rowned);
            return Ok(self.call_value("call i32 @p2w_none()"));
        }
        let recv = self.as_boxed(recv, rrepr);
        let recv_owned = rowned || boxes_to_new_temp(rrepr);
        let mut argvals = Vec::with_capacity(args.len());
        for a in args {
            argvals.push(self.expr(a)?); // method args are transferred (owned)
        }
        let name_g = self.intern_str(method.as_bytes());
        let nlen = method.len();
        let extra: String = argvals.iter().map(|v| format!(", i32 {v}")).collect();
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
        let (op, repr, owned) = self.expr_borrow_typed(e)?;
        let boxed = self.as_boxed(op, repr);
        // Boxing an unboxed scalar makes a fresh owned temp; a heap ref's
        // as_boxed is a no-op, so its ownership is unchanged.
        let owned = owned || boxes_to_new_temp(repr);
        Ok((boxed, owned))
    }

    /// Like `expr_borrow`, but preserves the operand's `Repr` so callers can take
    /// a native fast path. Returns `(operand, repr, owned)`; `owned` is true only
    /// for a Boxed value the caller must release — a borrowed `Name` and any
    /// unboxed scalar are not owned.
    fn expr_borrow_typed(&mut self, e: &Expr) -> Result<(String, Repr, bool), String> {
        if let ExprKind::Name(name) = &e.kind {
            if !self.vars.iter().any(|v| v == name) {
                return Err(format!("line {}: name '{name}' is not defined", e.line));
            }
            let (t, repr) = self.load_name(name); // borrowed: no retain
            return Ok((t, repr, false));
        }
        let (op, repr) = self.expr_typed(e)?;
        let owned = is_heap_repr(repr); // a fresh heap ref (Boxed/IntArray) is owned
        Ok((op, repr, owned))
    }

    /// Release a borrowed operand only if it was actually an owned temp.
    fn release_if_owned(&mut self, v: &str, owned: bool) {
        if owned {
            self.release(v);
        }
    }

    fn bin(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<(String, Repr), String> {
        if matches!(op, BinOp::And | BinOp::Or) {
            return Ok((self.short_circuit(op, a, b)?, Repr::Boxed));
        }
        let (va, ar, ao) = self.expr_borrow_typed(a)?;
        let (vb, br, bo) = self.expr_borrow_typed(b)?;

        // Native fast path: both operands are unboxed ints and the op is a simple
        // wraparound — emit a raw LLVM instruction, no boxing, no runtime call,
        // no refcount traffic. The result stays an unboxed Int and only boxes if
        // it later reaches a dynamic sink (via as_boxed).
        if let (Some(instr), Repr::Int, Repr::Int) = (int_native_op(op), ar, br) {
            let r = self.temp();
            self.line(&format!("{r} = {instr} i32 {va}, {vb}"));
            return Ok((r, Repr::Int));
        }
        // Native integer comparison: a raw `icmp` yielding an unboxed Bool (i1).
        if let (Some(pred), Repr::Int, Repr::Int) = (int_cmp_pred(op), ar, br) {
            let r = self.temp();
            self.line(&format!("{r} = icmp {pred} i32 {va}, {vb}"));
            return Ok((r, Repr::Bool));
        }
        // Native float arithmetic/comparison: when an operand is Float (or `/`,
        // which is always float), and both are statically numeric (Int/Float).
        // Ints are promoted to double (sitofp). // % ** fall back to the runtime
        // (Python's float floor/mod/pow are special).
        let numeric = |r: Repr| matches!(r, Repr::Int | Repr::Float);
        let float_op = float_native_op(op).is_some() || float_cmp_pred(op).is_some();
        if float_op
            && (matches!(op, BinOp::Div) || ar == Repr::Float || br == Repr::Float)
            && numeric(ar)
            && numeric(br)
        {
            let a_f = self.promote_double(va, ar);
            let b_f = self.promote_double(vb, br);
            let r = self.temp();
            if let Some(instr) = float_native_op(op) {
                self.line(&format!("{r} = {instr} double {a_f}, {b_f}"));
                return Ok((r, Repr::Float));
            }
            let pred = float_cmp_pred(op).unwrap();
            self.line(&format!("{r} = fcmp {pred} double {a_f}, {b_f}"));
            return Ok((r, Repr::Bool));
        }

        // Boxed fallback: box each operand, call the runtime, release temps.
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
        // Boxing an unboxed scalar operand yields a fresh owned temp (must
        // release); a heap-ref operand keeps its borrow/own status.
        let a_owned = boxes_to_new_temp(ar) || ao;
        let b_owned = boxes_to_new_temp(br) || bo;
        let va = self.as_boxed(va, ar);
        let vb = self.as_boxed(vb, br);
        let r = self.call_value(&format!("call i32 @{rt}(i32 {va}, i32 {vb})"));
        self.release_if_owned(&va, a_owned);
        self.release_if_owned(&vb, b_owned);
        Ok((r, Repr::Boxed))
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

// --- Borrowed-parameter escape analysis -------------------------------------
//
// A parameter is *borrowable* when the function only ever READS it — never
// transfers it onward (return, assignment, call/method arg, container insert) or
// reassigns its name. For a borrowable param the caller keeps ownership and the
// callee neither retains nor releases it, so passing a named heap value to a
// read-only helper costs zero refcount traffic. We compute the opposite —
// "does `p` escape?" — and default to `true` (escapes ⇒ owned, today's safe
// behavior) for any construct we don't explicitly recognize as read-only.

/// Whether parameter `p` escapes anywhere in `body` (⇒ not borrowable).
fn param_escapes(body: &[Stmt], p: &str) -> bool {
    body.iter().any(|s| stmt_escapes(s, p))
}

fn block_escapes(body: &[Stmt], p: &str) -> bool {
    body.iter().any(|s| stmt_escapes(s, p))
}

fn stmt_escapes(s: &Stmt, p: &str) -> bool {
    match &s.kind {
        // Reassigning the name (`p = …`, or a loop var shadowing it) means the
        // slot no longer holds the borrowed value — treat as an escape.
        StmtKind::Assign(name, value) => name == p || expr_escapes(value, true, p),
        StmtKind::AnnAssign { name, value, .. } => name == p || expr_escapes(value, true, p),
        StmtKind::Return(Some(e)) => expr_escapes(e, true, p),
        StmtKind::Return(None) => false,
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Call(n, args) if n == "print" => {
                args.iter().any(|a| expr_escapes(a, false, p))
            }
            _ => expr_escapes(e, false, p),
        },
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            expr_escapes(target, false, p)
                || expr_escapes(index, false, p)
                || expr_escapes(value, true, p) // the inserted value is transferred
        }
        StmtKind::If {
            cond,
            body,
            elifs,
            else_body,
        } => {
            expr_escapes(cond, false, p)
                || block_escapes(body, p)
                || elifs
                    .iter()
                    .any(|(c, b)| expr_escapes(c, false, p) || block_escapes(b, p))
                || else_body.as_deref().is_some_and(|b| block_escapes(b, p))
        }
        StmtKind::While { cond, body } => expr_escapes(cond, false, p) || block_escapes(body, p),
        StmtKind::For {
            var,
            start,
            end,
            step,
            body,
        } => {
            var == p
                || expr_escapes(start, false, p)
                || expr_escapes(end, false, p)
                || expr_escapes(step, false, p)
                || block_escapes(body, p)
        }
        StmtKind::ForEach {
            var,
            iterable,
            body,
        } => var == p || expr_escapes(iterable, false, p) || block_escapes(body, p),
        StmtKind::Break | StmtKind::Continue => false,
        _ => true, // unknown statement (e.g. a nested def) — assume it escapes
    }
}

/// Whether `p` escapes within `e`. `owning` is true when `e`'s value is itself
/// transferred (so a bare `p` here escapes); operands of read-only ops are not.
fn expr_escapes(e: &Expr, owning: bool, p: &str) -> bool {
    match &e.kind {
        ExprKind::Name(n) => owning && n == p,
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::NoneLit => false,
        ExprKind::Unary(_, x) => expr_escapes(x, false, p),
        // and/or yield one operand as the result, so they inherit the context.
        ExprKind::Bin(BinOp::And | BinOp::Or, a, b) => {
            expr_escapes(a, owning, p) || expr_escapes(b, owning, p)
        }
        ExprKind::Bin(_, a, b) => expr_escapes(a, false, p) || expr_escapes(b, false, p),
        ExprKind::Index(o, i) => expr_escapes(o, false, p) || expr_escapes(i, false, p),
        ExprKind::List(items) => items.iter().any(|it| expr_escapes(it, true, p)),
        ExprKind::Dict(pairs) => pairs
            .iter()
            .any(|(k, v)| expr_escapes(k, true, p) || expr_escapes(v, true, p)),
        ExprKind::Call(n, args) if n == "len" => args.iter().any(|a| expr_escapes(a, false, p)),
        ExprKind::Call(_, args) => args.iter().any(|a| expr_escapes(a, true, p)),
        ExprKind::MethodCall(obj, _, args) => {
            expr_escapes(obj, false, p) || args.iter().any(|a| expr_escapes(a, true, p))
        }
        _ => true, // unknown expression — assume it escapes
    }
}

/// The `Repr` an annotation denotes. `: int` ⇒ unboxed `Int`; everything else
/// (unannotated, `float`/`str`/`list[...]`, ...) stays `Boxed` for now. See
/// VALUE_MODEL.md (Float/packed-array reprs are later phases).
fn repr_of_ann(ann: &Option<Expr>) -> Repr {
    match ann {
        Some(e) => match &e.kind {
            ExprKind::Name(n) if n == "int" => Repr::Int,
            ExprKind::Name(n) if n == "float" => Repr::Float,
            // `list[int]` parses as a subscript of `list`.
            ExprKind::Index(base, elem)
                if matches!(&base.kind, ExprKind::Name(n) if n == "list")
                    && matches!(&elem.kind, ExprKind::Name(n) if n == "int") =>
            {
                Repr::IntArray
            }
            ExprKind::Index(base, elem)
                if matches!(&base.kind, ExprKind::Name(n) if n == "list")
                    && matches!(&elem.kind, ExprKind::Name(n) if n == "float") =>
            {
                Repr::FloatArray
            }
            _ => Repr::Boxed,
        },
        None => Repr::Boxed,
    }
}

/// The LLVM type a slot/param/return of this repr uses. Only `Float` is `double`;
/// `Bool` is `i1`; everything else (`Boxed`/`Int`/`IntArray` — a heap ref) is `i32`.
fn llvm_ty(repr: Repr) -> &'static str {
    match repr {
        Repr::Float => "double",
        Repr::Bool => "i1",
        Repr::Boxed | Repr::Int | Repr::IntArray | Repr::FloatArray => "i32",
    }
}

/// The zero-initializer literal for a slot of this repr.
fn zero_init(repr: Repr) -> &'static str {
    match repr {
        Repr::Float => "0.0",
        _ => "0",
    }
}

/// The LLVM instruction for a native (unboxed) integer binop, or `None` for ops
/// that fall back to the boxed runtime. `//`, `%`, `**` differ from LLVM's
/// truncating `sdiv`/`srem` (Python floors) or aren't a single instruction;
/// comparisons return a bool (a later repr). Native ops use i32 wraparound,
/// matching the value model's overflow decision.
fn int_native_op(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::Add => Some("add"),
        BinOp::Sub => Some("sub"),
        BinOp::Mul => Some("mul"),
        _ => None,
    }
}

/// The LLVM `icmp` predicate for a native integer comparison (signed), or `None`.
fn int_cmp_pred(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::Lt => Some("slt"),
        BinOp::Le => Some("sle"),
        BinOp::Gt => Some("sgt"),
        BinOp::Ge => Some("sge"),
        BinOp::Eq => Some("eq"),
        BinOp::Ne => Some("ne"),
        _ => None,
    }
}

/// The LLVM instruction for native float arithmetic, or `None`. `//`, `%`, `**`
/// fall back to the runtime (Python's float floor/mod/pow semantics).
fn float_native_op(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::Add => Some("fadd"),
        BinOp::Sub => Some("fsub"),
        BinOp::Mul => Some("fmul"),
        BinOp::Div => Some("fdiv"),
        _ => None,
    }
}

/// The LLVM `fcmp` predicate for a native float comparison (ordered), or `None`.
fn float_cmp_pred(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::Lt => Some("olt"),
        BinOp::Le => Some("ole"),
        BinOp::Gt => Some("ogt"),
        BinOp::Ge => Some("oge"),
        BinOp::Eq => Some("oeq"),
        BinOp::Ne => Some("one"),
        _ => None,
    }
}

/// Whether `e` references the variable `name` anywhere. Conservative: an
/// unrecognized construct returns `true` (assume it might). Used to keep FBIP
/// in-place map sound — the element mustn't read the array it's overwriting.
fn expr_uses_name(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Name(n) => n == name,
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::NoneLit => false,
        ExprKind::Unary(_, x) => expr_uses_name(x, name),
        ExprKind::Bin(_, a, b) => expr_uses_name(a, name) || expr_uses_name(b, name),
        ExprKind::Index(o, i) => expr_uses_name(o, name) || expr_uses_name(i, name),
        ExprKind::Call(_, args) => args.iter().any(|a| expr_uses_name(a, name)),
        ExprKind::MethodCall(o, _, args) => {
            expr_uses_name(o, name) || args.iter().any(|a| expr_uses_name(a, name))
        }
        ExprKind::List(items) => items.iter().any(|i| expr_uses_name(i, name)),
        ExprKind::Dict(pairs) => pairs
            .iter()
            .any(|(k, v)| expr_uses_name(k, name) || expr_uses_name(v, name)),
        _ => true, // unknown — assume it might reference `name`
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
    fn module_declares_runtime_and_native_int_arithmetic() {
        let out = ir("print(6 * 7)\n");
        assert!(out.contains("declare i32 @p2w_add(i32, i32)"), "{out}");
        assert!(out.contains("declare void @p2w_print(i32)"), "{out}");
        // 6 * 7 is unboxed native integer multiply — no boxed operands, no
        // runtime mul call.
        assert!(out.contains("mul i32 6, 7"), "native mul: {out}");
        assert!(!out.contains("call i32 @p2w_mul"), "no boxed mul: {out}");
        // the native result is boxed exactly once, at the dynamic sink (print).
        assert!(
            out.contains("call i32 @p2w_int(i32 %"),
            "box result for print: {out}"
        );
        assert!(out.contains("call void @p2w_print(i32"), "{out}");
        assert!(out.contains("ret i32 0"), "main exit: {out}");
    }

    #[test]
    fn strings_become_global_constants() {
        let out = ir("print(\"hi\")\n");
        assert!(out.contains("constant [2 x i8] c\"hi\""), "{out}");
        assert!(
            out.contains("call i32 @p2w_str(ptr @.str.main.0, i32 2)"),
            "{out}"
        );
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
    fn non_native_ops_route_through_runtime() {
        // //, ** and `not` still use the runtime (Python-floor / pow semantics).
        assert!(ir("print(7 // 2)\n").contains("call i32 @p2w_floordiv(i32"));
        assert!(ir("print(2 ** 10)\n").contains("call i32 @p2w_pow(i32"));
        assert!(ir("y = not 0\n").contains("call i32 @p2w_not(i32"));
        // Integer comparison is now a native icmp boxed to a bool — no p2w_lt.
        let cmp = ir("x = 1 < 2\n");
        assert!(cmp.contains("icmp slt i32 1, 2"), "{cmp}");
        assert!(!cmp.contains("call i32 @p2w_lt"), "{cmp}");
        // True division `/` is native float now (promote + fdiv), not p2w_div.
        let div = ir("print(7 / 2)\n");
        assert!(div.contains("fdiv double"), "{div}");
        assert!(!div.contains("call i32 @p2w_div"), "{div}");
    }

    #[test]
    fn rc_pass_emits_retain_and_release() {
        // Transferring a *named* value (ys = xs) retains it; the slots are
        // released at exit. (Full memory-correctness is validated by
        // tools/native_run.sh; this just guards the wiring from removal.)
        let out = ir("xs = [1, 2]\nys = xs\nprint(len(ys))\n");
        assert!(
            out.contains("call void @p2w_retain(i32"),
            "retain on transfer: {out}"
        );
        assert!(
            out.contains("call void @p2w_release(i32"),
            "release at exit: {out}"
        );
    }

    #[test]
    fn typed_int_param_compiles_to_native_arithmetic() {
        // A `: int` param is an unboxed raw i32: the body is native integer math
        // with no boxing and no refcount traffic.
        let out = ir("def sq(n: int) -> int:\n    return n * n\nprint(sq(7))\n");
        assert!(out.contains("define i32 @sq(i32 %a0)"), "{out}");
        assert!(out.contains("mul i32"), "native mul: {out}");
        assert!(!out.contains("call i32 @p2w_mul"), "no boxed mul: {out}");
        assert!(
            !out.contains("call void @p2w_retain"),
            "no refcounting: {out}"
        );
    }

    #[test]
    fn fbip_self_map_emits_unique_branch_and_in_place_write() {
        let out = ir("data: list[int] = [1, 2]\ndata = [x * x for x in data]\nprint(data)\n");
        assert!(
            out.contains("call i1 @p2w_unique"),
            "runtime uniqueness check: {out}"
        );
        assert!(
            out.contains("call void @p2w_iarray_set"),
            "in-place element write: {out}"
        );
    }

    #[test]
    fn list_comprehension_into_packed_array() {
        let out = ir("xs: list[int] = [1, 2]\nys: list[int] = [x * x for x in xs]\nprint(ys)\n");
        assert!(
            out.contains("call i32 @p2w_iarray_new"),
            "packed result: {out}"
        );
        assert!(out.contains("mul i32"), "native element compute: {out}");
        assert!(
            out.contains("call void @p2w_iarray_push"),
            "raw append: {out}"
        );
    }

    #[test]
    fn nested_comprehension_and_typed_return() {
        // Nested `for`s lower to nested loops; a list[int] target stays packed.
        let out = ir("xs: list[int] = [x + y for x in range(2) for y in range(2)]\nprint(xs)\n");
        assert!(out.contains("call i32 @p2w_iarray_new"), "packed: {out}");
        assert!(
            out.matches("icmp slt i32").count() >= 2,
            "two counted loops: {out}"
        );
        // A comprehension returned from a `-> list[int]` function builds packed.
        let r = ir("def f(n: int) -> list[int]:\n    return [i for i in range(n)]\n");
        let fbody = r.split("define i32 @f").nth(1).unwrap_or("");
        assert!(
            fbody.contains("call i32 @p2w_iarray_new"),
            "typed-return comprehension is packed: {fbody}"
        );
    }

    #[test]
    fn dict_comprehension_builds_a_dict() {
        let out = ir("d = {x: x * x for x in range(3)}\nprint(d[2])\n");
        assert!(out.contains("call i32 @p2w_dict_new"), "dict result: {out}");
        assert!(
            out.contains("call void @p2w_setindex"),
            "key/value set: {out}"
        );
    }

    #[test]
    fn list_comprehension_with_filter_and_range() {
        // `if` clause + range source both lower without a runtime iterator.
        let out = ir("ev: list[int] = [n for n in range(6) if n % 2 == 0]\nprint(ev)\n");
        assert!(out.contains("icmp slt i32"), "counted range loop: {out}");
        assert!(
            out.contains("call i32 @p2w_iarray_push") || out.contains("call void @p2w_iarray_push"),
            "{out}"
        );
    }

    #[test]
    fn list_int_compiles_to_a_packed_array() {
        let out = ir("xs: list[int] = [10, 20]\nprint(xs[0])\nfor x in xs:\n    print(x)\n");
        assert!(
            out.contains("call i32 @p2w_iarray_new"),
            "packed construct: {out}"
        );
        assert!(
            out.contains("call void @p2w_iarray_push"),
            "raw push: {out}"
        );
        assert!(out.contains("call i32 @p2w_iarray_get"), "raw get: {out}");
        assert!(
            !out.contains("call i32 @p2w_list_new"),
            "not a dynamic list: {out}"
        );
    }

    #[test]
    fn list_float_compiles_to_a_packed_double_array() {
        let out = ir("xs: list[float] = [1.5, 2.5]\nprint(xs[0])\nfor x in xs:\n    print(x)\n");
        assert!(
            out.contains("call i32 @p2w_farray_new"),
            "packed construct: {out}"
        );
        assert!(
            out.contains("call void @p2w_farray_push(i32"),
            "raw push: {out}"
        );
        assert!(
            out.contains("call double @p2w_farray_get"),
            "raw double get: {out}"
        );
        assert!(
            !out.contains("call i32 @p2w_list_new"),
            "not a dynamic list: {out}"
        );
    }

    #[test]
    fn typed_float_param_is_a_native_double_function() {
        let out = ir("def dbl(x: float) -> float:\n    return x * 2.0\nprint(dbl(2.5))\n");
        assert!(
            out.contains("define double @dbl(double %a0)"),
            "double sig: {out}"
        );
        assert!(out.contains("alloca double"), "double slot: {out}");
        assert!(out.contains("fmul double"), "native fmul: {out}");
        assert!(out.contains("ret double"), "double return: {out}");
    }

    #[test]
    fn annotated_local_loop_is_native() {
        // total:int + i:int + native compare/add => a fully native while loop,
        // no boxing or runtime calls in the body.
        let out = ir(
            "def s(n: int) -> int:\n    total: int = 0\n    i: int = 0\n    while i < n:\n        total = total + i\n        i = i + 1\n    return total\n",
        );
        assert!(out.contains("icmp slt i32"), "native compare: {out}");
        assert!(out.contains("add i32"), "native add: {out}");
        assert!(!out.contains("call i32 @p2w_add"), "no boxed add: {out}");
        assert!(!out.contains("call i32 @p2w_int"), "no boxing: {out}");
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
    fn borrowed_list_int_param_is_not_retained_at_the_call() {
        // A read-only list[int] param is borrowed — even with an annotated local
        // (`s: int`) in the body, which the escape analysis must look through.
        // The call passes the named array with no retain in main.
        let out = ir(
            "def total(xs: list[int]) -> int:\n    s: int = 0\n    for x in xs:\n        s = s + x\n    return s\nys: list[int] = [1, 2, 3]\nprint(total(ys))\n",
        );
        let main = out.split("define i32 @main").nth(1).unwrap_or("");
        assert!(
            !main.contains("call void @p2w_retain"),
            "borrowed array param should not be retained at the call: {main}"
        );
    }

    #[test]
    fn borrowed_param_skips_retain_but_escaping_param_keeps_it() {
        // `peek` only reads xs (read-index + returns the element) -> borrowed, so
        // passing a named list needs no retain anywhere in the program.
        let borrowed = ir("def peek(xs):\n    return xs[0]\nys = [1, 2]\nprint(peek(ys))\n");
        assert!(
            !borrowed.contains("call void @p2w_retain"),
            "borrowed param should avoid retain: {borrowed}"
        );
        // `echo` returns xs itself -> escapes -> owned, so the caller must retain
        // the named argument it transfers in.
        let owned = ir("def echo(xs):\n    return xs\nys = [1, 2]\nzs = echo(ys)\nprint(zs)\n");
        assert!(
            owned.contains("call void @p2w_retain"),
            "escaping param should retain on transfer: {owned}"
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
    fn for_range_is_a_native_counter() {
        // The range counter is an unboxed i32: native icmp guard + raw add
        // increment, no p2w_lt/p2w_add/p2w_truthy for the loop control.
        let out = ir("for i in range(1, 5):\n    print(i)\n");
        assert!(out.contains("icmp slt i32"), "ascending: {out}");
        assert!(out.contains("add i32"), "increment: {out}");
        assert!(!out.contains("call i32 @p2w_lt"), "no boxed compare: {out}");
        let out = ir("for i in range(5, 0, -1):\n    print(i)\n");
        assert!(out.contains("icmp sgt i32"), "descending: {out}");
        assert!(out.contains("add i32 %",), "decrement via add: {out}");
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
        assert!(
            out.contains("call void @p2w_setindex(i32"),
            "dict build: {out}"
        );
        assert!(out.contains("call i32 @p2w_index(i32"), "dict read: {out}");
    }

    #[test]
    fn methods_dispatch_by_name() {
        let out = ir("xs = [1]\nxs.append(2)\nlast = xs.pop()\n");
        assert!(
            out.contains("constant [6 x i8] c\"append\""),
            "method name: {out}"
        );
        assert!(
            out.contains("call i32 @p2w_method1(i32"),
            "1-arg method: {out}"
        );
        assert!(out.contains("constant [3 x i8] c\"pop\""), "{out}");
        assert!(
            out.contains("call i32 @p2w_method0(i32"),
            "0-arg method: {out}"
        );
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
