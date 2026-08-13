//! Static value representations and the slot/expression inference that
//! assigns them — SHARED by both emitters (`llvm` and `codegen`), because two
//! inferences that can drift is how the backends became "different languages
//! in places nobody chose" (`BACKEND_DIVERGENCE.md`). The native emitter uses
//! every variant; the GC emitter (typed-slot tier) uses `Int`/`Float`/`Boxed`
//! today and ignores the packed-array reprs.

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// Static representation of a value as the emitter tracks it. `Boxed` is the
/// universal tagged-`i32` (the dynamic default). Unboxed reprs are raw machine
/// values produced where the type is statically known; `as_boxed` coerces back
/// at dynamic sinks. See VALUE_MODEL.md. (Stage 1: `Int` only; `Float`/`Bool`/
/// packed arrays follow.)
#[derive(Clone, Copy, PartialEq)]
pub enum Repr {
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

/// The `Repr` an annotation denotes. `: int` ⇒ unboxed `Int`; everything else
/// (unannotated, `float`/`str`/`list[...]`, ...) stays `Boxed` for now. See
/// VALUE_MODEL.md (Float/packed-array reprs are later phases).
pub(crate) fn repr_of_ann(ann: &Option<Expr>) -> Repr {
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

/// Conservative syntactic typing for a reuse-map element: is `e` *guaranteed*
/// to produce the packed buffer's element type? Int buffers: integer arithmetic
/// over the loop var and int literals (no `/` — true division makes floats; no
/// `**` — negative exponents do too). Float buffers: float arithmetic over the
/// loop var and numeric literals. Anything else (calls, strings, comparisons,
/// other names) → no reuse. Widen with real type inference later (the hire).
/// The inference core behind `FuncEmitter::infer_repr` and the slot-inference
/// pre-pass: `look` resolves a name to a PROVEN repr (or None). Trusting a
/// `-> int` annotation is sound because the typed call convention already
/// unboxes the return — a lying annotation traps at the call, with or
/// without us. Every unknown shape errs toward `None`.
pub(crate) fn infer_expr_repr(
    e: &Expr,
    look: &dyn Fn(&str) -> Option<Repr>,
    rets: &HashMap<String, Repr>,
) -> Option<Repr> {
    match &e.kind {
        ExprKind::Int(_) => Some(Repr::Int),
        ExprKind::Float(_) => Some(Repr::Float),
        ExprKind::Name(n) => look(n),
        ExprKind::Unary(UnOp::Neg, i) => match infer_expr_repr(i, look, rets) {
            r @ Some(Repr::Int | Repr::Float) => r,
            _ => None,
        },
        ExprKind::Bin(op, a, b) => {
            let ta = infer_expr_repr(a, look, rets)?;
            let tb = infer_expr_repr(b, look, rets)?;
            if !matches!(ta, Repr::Int | Repr::Float) || !matches!(tb, Repr::Int | Repr::Float) {
                return None;
            }
            match op {
                BinOp::Div => Some(Repr::Float), // true division: always float
                BinOp::Add | BinOp::Sub | BinOp::Mul => {
                    if ta == Repr::Float || tb == Repr::Float {
                        Some(Repr::Float)
                    } else {
                        Some(Repr::Int)
                    }
                }
                // Float floor/mod use runtime semantics — don't claim them.
                BinOp::FloorDiv | BinOp::Mod if ta == Repr::Int && tb == Repr::Int => {
                    Some(Repr::Int)
                }
                _ => None,
            }
        }
        ExprKind::Call(f, _) => {
            if f == "len" {
                return Some(Repr::Int);
            }
            match rets.get(f) {
                Some(r @ (Repr::Int | Repr::Float)) => Some(*r),
                _ => None,
            }
        }
        ExprKind::Index(obj, _) => match infer_expr_repr(obj, look, rets) {
            Some(Repr::IntArray) => Some(Repr::Int),
            Some(Repr::FloatArray) => Some(Repr::Float),
            _ => None,
        },
        _ => None,
    }
}

/// First-assignment slot inference (task 3, stage 2): join the provable repr
/// of every binding of each unannotated local — plain assignments AND
/// loop-variable bindings — and give a name whose bindings all agree on
/// Int/Float a typed (unboxed) slot; ANY disagreement or unknown demotes to
/// Boxed, exactly today's behavior. The join demotes even on int/float
/// mixing: a mixed name in a Float slot would print `1.0` where CPython
/// prints `1`. So inference can only change representation, never observable
/// behavior — this is deliberately the silent-demote arm of the
/// reject-vs-lint policy question recorded in `docs/COMPILER_FRONTIER.md`.
///
/// `fixed` holds names whose repr is authoritative and NOT ours to infer
/// (params from the signature, annotated names from their annotation) — they
/// resolve reads but are excluded from the output. Runs to a fixpoint so
/// `t = 0; t = t + 1` and loop-carried reads resolve regardless of order;
/// each name moves monotonically unknown → known → Boxed, so it terminates.
pub(crate) fn infer_slot_reprs(
    body: &[Stmt],
    fixed: &HashMap<String, Repr>,
    rets: &HashMap<String, Repr>,
) -> HashMap<String, Repr> {
    // Annotated names are authoritative wherever the annotation appears.
    let mut fixed = fixed.clone();
    fn collect_ann(body: &[Stmt], fixed: &mut HashMap<String, Repr>) {
        for s in body {
            match &s.kind {
                StmtKind::AnnAssign { name, ann, .. } => {
                    fixed
                        .entry(name.clone())
                        .or_insert_with(|| repr_of_ann(&Some(ann.clone())));
                }
                StmtKind::If {
                    body,
                    elifs,
                    else_body,
                    ..
                } => {
                    collect_ann(body, fixed);
                    for (_, b) in elifs {
                        collect_ann(b, fixed);
                    }
                    if let Some(b) = else_body {
                        collect_ann(b, fixed);
                    }
                }
                StmtKind::For { body, .. }
                | StmtKind::ForEach { body, .. }
                | StmtKind::While { body, .. } => collect_ann(body, fixed),
                _ => {}
            }
        }
    }
    collect_ann(body, &mut fixed);

    let mut env: HashMap<String, Repr> = HashMap::new();
    for _ in 0..8 {
        let before = env.clone();
        walk_bindings(body, &fixed, rets, &mut env);
        if env == before {
            break;
        }
    }
    env.retain(|_, r| matches!(r, Repr::Int | Repr::Float));
    env
}

/// One fixpoint round of `infer_slot_reprs`: join each binding's repr into
/// `env`. `Boxed` is the poison value (any disagreement / unknown).
pub(crate) fn walk_bindings(
    body: &[Stmt],
    fixed: &HashMap<String, Repr>,
    rets: &HashMap<String, Repr>,
    env: &mut HashMap<String, Repr>,
) {
    let join = |env: &mut HashMap<String, Repr>, name: &str, r: Option<Repr>| {
        if fixed.contains_key(name) {
            return; // authoritative elsewhere; not ours
        }
        let r = match r {
            Some(x @ (Repr::Int | Repr::Float)) => x,
            _ => Repr::Boxed,
        };
        match env.get(name) {
            None => {
                env.insert(name.to_string(), r);
            }
            Some(cur) if *cur == r => {}
            _ => {
                env.insert(name.to_string(), Repr::Boxed);
            }
        }
    };
    // Reads resolve fixed names first, then the env under construction.
    let snapshot = env.clone();
    let look = |n: &str| -> Option<Repr> {
        if let Some(r) = fixed.get(n) {
            return match r {
                Repr::Boxed | Repr::Bool => None,
                r => Some(*r),
            };
        }
        match snapshot.get(n) {
            Some(r @ (Repr::Int | Repr::Float | Repr::IntArray | Repr::FloatArray)) => Some(*r),
            _ => None,
        }
    };
    for s in body {
        match &s.kind {
            StmtKind::Assign(name, value) => {
                join(env, name, infer_expr_repr(value, &look, rets));
            }
            StmtKind::UnpackAssign { targets, .. } => {
                for t in targets {
                    if let ExprKind::Name(n) = &t.kind {
                        join(env, n, None);
                    }
                }
            }
            StmtKind::For { var, body, .. } => {
                join(env, var, Some(Repr::Int)); // native range counter
                walk_bindings(body, fixed, rets, env);
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                let elem = match infer_expr_repr(iterable, &look, rets) {
                    Some(Repr::IntArray) => Some(Repr::Int),
                    Some(Repr::FloatArray) => Some(Repr::Float),
                    _ => None,
                };
                join(env, var, elem);
                walk_bindings(body, fixed, rets, env);
            }
            StmtKind::While { body, .. } => walk_bindings(body, fixed, rets, env),
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                walk_bindings(body, fixed, rets, env);
                for (_, b) in elifs {
                    walk_bindings(b, fixed, rets, env);
                }
                if let Some(b) = else_body {
                    walk_bindings(b, fixed, rets, env);
                }
            }
            // Defs/classes are separate scopes; nothing else binds names.
            _ => {}
        }
    }
}
