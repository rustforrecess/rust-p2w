//! Static value representations and the slot/expression inference that
//! assigns them — SHARED by both emitters (`llvm` and `codegen`), because two
//! inferences that can drift is how the backends became "different languages
//! in places nobody chose" (`BACKEND_DIVERGENCE.md`). The native emitter uses
//! every variant; the GC emitter (typed-slot tier) uses `Int`/`Float`/`Boxed`
//! today and ignores the packed-array reprs.

use std::collections::{BTreeSet, HashMap};

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
        // `obj.x` where `x` is a float field (`infer_float_attrs`): every
        // object that has an `x` stores it as a raw float. A module's
        // attribute (`math.pi`, `robot.tilt`) is not an object's.
        ExprKind::Attr(obj, name) => {
            if let ExprKind::Name(m) = &obj.kind
                && look(m).is_none()
                && (m == "math" || m == "random" || crate::hostmod::module(m).is_some())
            {
                return None;
            }
            rets.get(&float_attr_key(name)).copied()
        }
        _ => None,
    }
}

/// The key a float field is recorded under in the function-return map that
/// `infer_expr_repr` already takes (`rets`). A leading `.` can't be a
/// function name, so the two never collide; this keeps one inference
/// signature for every caller instead of a second map threaded everywhere.
pub(crate) fn float_attr_key(attr: &str) -> String {
    format!(".{attr}")
}

/// `rets` with every float field added under its `float_attr_key`.
pub(crate) fn with_float_attrs(
    rets: &HashMap<String, Repr>,
    float_attrs: &BTreeSet<String>,
) -> HashMap<String, Repr> {
    let mut out = rets.clone();
    for a in float_attrs {
        out.insert(float_attr_key(a), Repr::Float);
    }
    out
}

/// Which attribute names are FLOAT FIELDS: stored as a raw `f64` in every
/// object that has them (docs/CLASS_LAYOUT.md, step 3), so `self.x + self.v`
/// allocates nothing. Decided for the whole program, per attribute name:
/// `x` is a float field only if EVERY assignment to any `.x`, on `self` or
/// any other object, is provably a float. One int (`self.x = 0`) keeps the
/// field boxed, because a float slot would print `0.0` where CPython prints
/// `0`: this can only change representation, never behavior, like the
/// slot inference above.
///
/// `candidates` are the names allowed to be float fields at all (the caller
/// leaves out class-variable names, where an unset slot means "read the
/// class variable", and names a host module also exports).
///
/// The proof is circular on purpose: `self.x = self.x + self.v` is a float
/// only if `x` and `v` are. So it starts by assuming every candidate is a
/// float field and removes any name with a write that isn't, until nothing
/// changes. What remains is consistent: under the assumption that exactly
/// these are floats, every write to them is a float.
pub(crate) fn infer_float_attrs(
    program: &[Stmt],
    rets: &HashMap<String, Repr>,
    candidates: BTreeSet<String>,
) -> BTreeSet<String> {
    fn check(
        body: &[Stmt],
        look: &dyn Fn(&str) -> Option<Repr>,
        rets: &HashMap<String, Repr>,
        set: &BTreeSet<String>,
        bad: &mut BTreeSet<String>,
    ) {
        for s in body {
            match &s.kind {
                StmtKind::SetAttr { attr, value, .. } if set.contains(attr) => {
                    if infer_expr_repr(value, look, rets) != Some(Repr::Float) {
                        bad.insert(attr.clone());
                    }
                }
                // An unpacked element's type isn't tracked.
                StmtKind::UnpackAssign { targets, .. } => {
                    for t in targets {
                        if let ExprKind::Attr(_, attr) = &t.kind {
                            bad.insert(attr.clone());
                        }
                    }
                }
                _ => {}
            }
            crate::lint::for_each_child_block(s, |b, scope| {
                if scope == crate::lint::BlockScope::Same {
                    check(b, look, rets, set, bad);
                }
            });
        }
    }
    // One scope: its proven local reprs, then its writes.
    fn scope(
        body: &[Stmt],
        fixed: &HashMap<String, Repr>,
        rets: &HashMap<String, Repr>,
        set: &BTreeSet<String>,
        bad: &mut BTreeSet<String>,
    ) {
        let env = infer_slot_reprs(body, fixed, rets);
        let look = |n: &str| -> Option<Repr> {
            match fixed.get(n).or_else(|| env.get(n)) {
                Some(r @ (Repr::Int | Repr::Float | Repr::IntArray | Repr::FloatArray)) => Some(*r),
                _ => None,
            }
        };
        check(body, &look, rets, set, bad);
    }

    let mut set = candidates;
    loop {
        let rets = with_float_attrs(rets, &set);
        let mut bad = BTreeSet::new();
        // Module level: `infer_slot_reprs` skips defs and classes.
        scope(program, &HashMap::new(), &rets, &set, &mut bad);
        for s in program {
            match &s.kind {
                StmtKind::Def {
                    params,
                    param_types,
                    body,
                    ..
                } => {
                    let fixed = params
                        .iter()
                        .zip(param_types)
                        .map(|(p, t)| (p.clone(), repr_of_ann(t)))
                        .collect();
                    scope(body, &fixed, &rets, &set, &mut bad);
                }
                StmtKind::ClassDef { methods, .. } => {
                    for m in methods {
                        // Method parameters are untyped (boxed).
                        let fixed = m.params.iter().map(|p| (p.clone(), Repr::Boxed)).collect();
                        scope(&m.body, &fixed, &rets, &set, &mut bad);
                    }
                }
                _ => {}
            }
        }
        if bad.iter().all(|b| !set.contains(b)) {
            return set;
        }
        set.retain(|a| !bad.contains(a));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn float_attrs(src: &str, candidates: &[&str]) -> Vec<String> {
        let tokens = crate::lexer::lex(src).unwrap();
        let program = crate::parser::parse(&tokens).unwrap();
        infer_float_attrs(
            &program,
            &HashMap::new(),
            candidates.iter().map(|c| c.to_string()).collect(),
        )
        .into_iter()
        .collect()
    }

    /// A field is a float field only when every write to it is a float; the
    /// circular case (`x = x + v`) holds when both are.
    #[test]
    fn float_fields_need_every_write_to_be_a_float() {
        let body = "class Body:\n    def __init__(self):\n        self.x = 0.0\n        self.v = 1.5\n        self.n = 0\n    def step(self):\n        self.x = self.x + self.v\n        self.n = self.n + 1\n";
        assert_eq!(float_attrs(body, &["x", "v", "n"]), vec!["v", "x"]);

        // One int write keeps the field boxed: a float slot would print 0.0.
        let mixed = "class B:\n    def __init__(self):\n        self.x = 0\n    def step(self):\n        self.x = self.x + 0.5\n";
        assert!(float_attrs(mixed, &["x"]).is_empty());

        // A write from outside the class counts too, and one bad name takes
        // down a field that depends on it.
        let outside = "class B:\n    def __init__(self):\n        self.x = 0.0\n        self.v = 1.0\n    def step(self):\n        self.x = self.x + self.v\nb = B()\nb.v = \"fast\"\n";
        assert!(float_attrs(outside, &["x", "v"]).is_empty());
        let fine =
            "class B:\n    def __init__(self):\n        self.x = 0.0\nb = B()\nb.x = b.x * 2.0\n";
        assert_eq!(float_attrs(fine, &["x"]), vec!["x"]);

        // An untyped parameter proves nothing; an annotated one does.
        let param = "class B:\n    def __init__(self, x):\n        self.x = x\n";
        assert!(float_attrs(param, &["x"]).is_empty());
        let typed = "class B:\n    def __init__(self):\n        self.x = 0.0\ndef push(b, dx: float):\n    b.x = b.x + dx\n";
        assert_eq!(float_attrs(typed, &["x"]), vec!["x"]);

        // Unpacking into the field isn't tracked.
        let unpack = "class B:\n    def __init__(self):\n        self.x, self.y = 0.0, 1.0\n";
        assert!(float_attrs(unpack, &["x", "y"]).is_empty());
    }
}
