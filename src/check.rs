//! The type checker, phase A: ADVISORY. (docs/TYPE_CHECKER_DESIGN.md)
//!
//! A standalone flow-sensitive pass over the shared AST that infers types
//! and reports the confident mistakes — with a PROVENANCE LEDGER, so every
//! message can name the line that made a value what it is: "`age` is text
//! because line 1 assigned it" rather than "cannot add str and int".
//!
//! Phase A changes NO behavior: findings ride `p2w check` as their own
//! section, compilation is untouched, and anything unprovable is `Dyn` —
//! an honest "don't know", never an error. The open pedagogy questions
//! (branch disagreement, type-changing reassignment, mixed lists) are
//! deliberately NOT flagged here; they are `Dyn` until phase C decides
//! them. The false-positive gate is executable: every `tests/oracle/ok/`
//! program must produce ZERO findings.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind};
use std::collections::HashMap;

/// One advisory finding: a stable key, a derivation-rendered message, and
/// where to point.
#[derive(Debug, Clone)]
pub struct TypeFinding {
    pub line: usize,
    pub span: (usize, usize),
    /// Stable key, e.g. `type.str-in-arithmetic` — the corpus and the IDE
    /// hang behavior off this, never off the wording.
    pub code: &'static str,
    pub message: String,
}

/// The tier-1 lattice (design D2). `Dyn` is the honest top: it compiles
/// exactly as today and is never reported.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
    NoneT,
    List(Box<Ty>),
    Tuple,
    Dict,
    Set,
    Func,
    Class,
    Dyn,
}

impl Ty {
    fn numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float | Ty::Bool)
    }
    /// The student-facing name — matches the runtime's type_name vocabulary.
    fn name(&self) -> &'static str {
        match self {
            Ty::Int => "a whole number",
            Ty::Float => "a decimal number",
            Ty::Bool => "True/False",
            Ty::Str => "text",
            Ty::NoneT => "None",
            Ty::List(_) => "a list",
            Ty::Tuple => "a tuple",
            Ty::Dict => "a dict",
            Ty::Set => "a set",
            Ty::Func => "a function",
            Ty::Class => "a class",
            Ty::Dyn => "unknown",
        }
    }
}

/// What a name is, and WHY — the provenance the messages are built from.
#[derive(Debug, Clone)]
struct Fact {
    ty: Ty,
    /// Where it became this type.
    line: usize,
    /// A short phrase completing "line N …", e.g. "assigned it \"12\" (text)".
    why: String,
}

/// A function's declared surface, for call-site checking. Only ANNOTATED
/// parts participate in phase A — an unannotated function is `Dyn` all over
/// and stays silent.
#[derive(Debug, Clone)]
struct FnSig {
    line: usize,
    params: Vec<(String, Ty)>,
    ret: Ty,
    n_required: usize,
    n_params: usize,
}

pub fn type_findings(stmts: &[Stmt]) -> Vec<TypeFinding> {
    let mut ck = Checker {
        sigs: HashMap::new(),
        out: Vec::new(),
    };
    // Pass 1: function surfaces (a call may precede the def textually).
    for s in stmts {
        if let StmtKind::Def {
            name,
            params,
            param_types,
            defaults,
            return_type,
            ..
        } = &s.kind
        {
            ck.sigs.insert(
                name.clone(),
                FnSig {
                    line: s.line,
                    params: params
                        .iter()
                        .zip(param_types)
                        .map(|(p, t)| (p.clone(), ann_ty(t.as_ref())))
                        .collect(),
                    ret: ann_ty(return_type.as_ref()),
                    n_required: params.len().saturating_sub(defaults.len()),
                    n_params: params.len(),
                },
            );
        }
    }
    // Pass 2: module flow, then each function body in its own scope.
    let mut env: HashMap<String, Fact> = HashMap::new();
    ck.block(stmts, &mut env, None);
    for s in stmts {
        if let StmtKind::Def {
            name,
            params,
            param_types,
            return_type,
            body,
            ..
        } = &s.kind
        {
            let mut fenv: HashMap<String, Fact> = HashMap::new();
            for (p, t) in params.iter().zip(param_types) {
                let ty = ann_ty(t.as_ref());
                let why = match t {
                    Some(_) => format!("declared it {} in `{name}`'s signature", ty.name()),
                    None => "made it a parameter (no annotation, so its type is open)".into(),
                };
                fenv.insert(
                    p.clone(),
                    Fact {
                        ty,
                        line: s.line,
                        why,
                    },
                );
            }
            let ret = return_type
                .as_ref()
                .map(|r| (ann_ty(Some(r)), s.line, name.as_str()));
            ck.block(
                body,
                &mut fenv,
                ret.as_ref().map(|(t, l, n)| (t.clone(), *l, *n)),
            );
        }
    }
    ck.out
}

/// Map an annotation expression to a lattice type. Unknown names are `Dyn` —
/// annotations we don't understand stay silent, never wrong.
fn ann_ty(ann: Option<&Expr>) -> Ty {
    let Some(a) = ann else { return Ty::Dyn };
    match &a.kind {
        ExprKind::Name(n) => match n.as_str() {
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "str" => Ty::Str,
            _ => Ty::Dyn,
        },
        _ => Ty::Dyn, // list[int] etc. — tier 2
    }
}

struct Checker {
    sigs: HashMap<String, FnSig>,
    out: Vec<TypeFinding>,
}

impl Checker {
    /// Walk one block, threading the environment. `ret` = (declared return
    /// type, signature line, function name) when inside an annotated def.
    fn block(
        &mut self,
        stmts: &[Stmt],
        env: &mut HashMap<String, Fact>,
        ret: Option<(Ty, usize, &str)>,
    ) {
        for s in stmts {
            match &s.kind {
                StmtKind::Assign(name, value) => {
                    let (ty, _) = self.expr(value, env);
                    let why = describe_value(value, &ty);
                    env.insert(
                        name.clone(),
                        Fact {
                            ty,
                            line: s.line,
                            why,
                        },
                    );
                }
                StmtKind::AnnAssign { name, ann, value } => {
                    let declared = ann_ty(Some(ann));
                    let (actual, prov) = self.expr(value, env);
                    if declared != Ty::Dyn && actual != Ty::Dyn && declared != actual {
                        self.out.push(TypeFinding {
                            line: s.line,
                            span: s.span,
                            code: "type.annotation-contradicted",
                            message: format!(
                                "`{name}` says it will be {} — but this line gives it {}{}. \
                                 One of the two is what you meant",
                                declared.name(),
                                actual.name(),
                                prov_clause(&prov),
                            ),
                        });
                    }
                    // Downstream trusts the ANNOTATION (one finding, no cascade).
                    let why = format!("declared it {} here", declared.name());
                    env.insert(
                        name.clone(),
                        Fact {
                            ty: declared,
                            line: s.line,
                            why,
                        },
                    );
                }
                StmtKind::Expr(e) => {
                    self.expr(e, env);
                }
                StmtKind::If {
                    cond,
                    body,
                    elifs,
                    else_body,
                } => {
                    self.expr(cond, env);
                    // Branch environments walk from a copy; the join is the
                    // phase-C question, so names the branches disagree on
                    // simply become Dyn (honest, silent).
                    let before = env.clone();
                    let mut outcomes: Vec<HashMap<String, Fact>> = Vec::new();
                    let mut b1 = before.clone();
                    self.block(body, &mut b1, ret.clone());
                    outcomes.push(b1);
                    for (c, eb) in elifs {
                        self.expr(c, env);
                        let mut bi = before.clone();
                        self.block(eb, &mut bi, ret.clone());
                        outcomes.push(bi);
                    }
                    if let Some(eb) = else_body {
                        let mut be = before.clone();
                        self.block(eb, &mut be, ret.clone());
                        outcomes.push(be);
                    } else {
                        outcomes.push(before.clone());
                    }
                    join_into(env, &outcomes);
                }
                StmtKind::While { cond, body } => {
                    self.expr(cond, env);
                    self.loop_body(body, env, &ret);
                }
                StmtKind::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                } => {
                    for e in [start, end, step] {
                        self.expr(e, env);
                    }
                    env.insert(
                        var.clone(),
                        Fact {
                            ty: Ty::Int,
                            line: s.line,
                            why: "made it count through range() (whole numbers)".into(),
                        },
                    );
                    self.loop_body(body, env, &ret);
                }
                StmtKind::ForEach {
                    var,
                    iterable,
                    body,
                } => {
                    let (ity, prov) = self.expr(iterable, env);
                    let vty = match &ity {
                        Ty::List(t) => (**t).clone(),
                        Ty::Str => Ty::Str,
                        Ty::Dict | Ty::Set | Ty::Tuple | Ty::Dyn => Ty::Dyn,
                        Ty::Int | Ty::Float | Ty::Bool | Ty::NoneT => {
                            self.out.push(TypeFinding {
                                line: s.line,
                                span: s.span,
                                code: "type.for-over-single-value",
                                message: format!(
                                    "this loop needs something with items in it, but it was \
                                     given {}{} — a single value has nothing to step through. \
                                     range(n) counts, if counting was the plan",
                                    ity.name(),
                                    prov_clause(&prov),
                                ),
                            });
                            Ty::Dyn
                        }
                        _ => Ty::Dyn,
                    };
                    env.insert(
                        var.clone(),
                        Fact {
                            ty: vty,
                            line: s.line,
                            why: "the loop hands it each item in turn".into(),
                        },
                    );
                    self.loop_body(body, env, &ret);
                }
                StmtKind::Return(value) => {
                    let actual = match value {
                        Some(v) => self.expr(v, env),
                        None => (Ty::NoneT, None),
                    };
                    if let Some((declared, sig_line, fname)) = &ret
                        && *declared != Ty::Dyn
                        && actual.0 != Ty::Dyn
                        && actual.0 != *declared
                        && !(actual.0 == Ty::Int && *declared == Ty::Float)
                    {
                        self.out.push(TypeFinding {
                            line: s.line,
                            span: s.span,
                            code: "type.return-contradicts-signature",
                            message: format!(
                                "line {sig_line} promises `{fname}` gives back {} — this \
                                 return hands back {}{}. The promise and the return should \
                                 agree",
                                declared.name(),
                                actual.0.name(),
                                prov_clause(&actual.1),
                            ),
                        });
                    }
                }
                StmtKind::SetIndex {
                    target,
                    index,
                    value,
                } => {
                    self.expr(target, env);
                    self.expr(index, env);
                    self.expr(value, env);
                }
                StmtKind::SetAttr { obj, value, .. } => {
                    self.expr(obj, env);
                    self.expr(value, env);
                }
                StmtKind::UnpackAssign { targets, value } => {
                    self.expr(value, env);
                    for t in targets {
                        if let ExprKind::Name(n) = &t.kind {
                            env.insert(
                                n.clone(),
                                Fact {
                                    ty: Ty::Dyn,
                                    line: s.line,
                                    why: "unpacked it here".into(),
                                },
                            );
                        }
                    }
                }
                // Defs/classes register elsewhere; the NAME is callable here.
                StmtKind::Def { name, .. } => {
                    env.insert(
                        name.clone(),
                        Fact {
                            ty: Ty::Func,
                            line: s.line,
                            why: format!("defined the function `{name}` here"),
                        },
                    );
                }
                StmtKind::ClassDef { name, .. } => {
                    env.insert(
                        name.clone(),
                        Fact {
                            ty: Ty::Class,
                            line: s.line,
                            why: format!("defined the class `{name}` here"),
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// A loop body runs zero-or-more times: walk it once for findings, then
    /// demote to Dyn any name whose type the body changed (the honest join).
    fn loop_body(
        &mut self,
        body: &[Stmt],
        env: &mut HashMap<String, Fact>,
        ret: &Option<(Ty, usize, &str)>,
    ) {
        let before = env.clone();
        self.block(body, env, ret.clone());
        for (name, fact) in env.iter_mut() {
            if let Some(prev) = before.get(name) {
                if prev.ty != fact.ty {
                    fact.ty = Ty::Dyn;
                }
            } else {
                // Born inside the loop: may not exist after zero iterations.
                fact.ty = fact.ty.clone(); // type kept; existence is a lint's job
            }
        }
    }

    /// Infer an expression's type. The second value is provenance for the
    /// OUTERMOST name involved, when there is one — the ledger entry a
    /// message cites.
    fn expr(&mut self, e: &Expr, env: &mut HashMap<String, Fact>) -> (Ty, Option<Fact>) {
        match &e.kind {
            ExprKind::Int(_) => (Ty::Int, None),
            ExprKind::Float(_) => (Ty::Float, None),
            ExprKind::Bool(_) => (Ty::Bool, None),
            ExprKind::Str(_) => (Ty::Str, None),
            ExprKind::NoneLit => (Ty::NoneT, None),
            ExprKind::Name(n) => match env.get(n) {
                Some(f) => (f.ty.clone(), Some(f.clone())),
                None => (
                    if self.sigs.contains_key(n) {
                        Ty::Func
                    } else {
                        Ty::Dyn
                    },
                    None,
                ),
            },
            ExprKind::List(items) => {
                let mut elem = Ty::Dyn;
                let mut first = true;
                for it in items {
                    let (t, _) = self.expr(it, env);
                    if first {
                        elem = t;
                        first = false;
                    } else if elem != t {
                        elem = Ty::Dyn; // mixed list: phase-C question, silent
                    }
                }
                (Ty::List(Box::new(elem)), None)
            }
            ExprKind::Tuple(items) => {
                for it in items {
                    self.expr(it, env);
                }
                (Ty::Tuple, None)
            }
            ExprKind::Dict(pairs) => {
                for (k, v) in pairs {
                    self.expr(k, env);
                    self.expr(v, env);
                }
                (Ty::Dict, None)
            }
            ExprKind::Bin(op, a, b) => self.binop(e, *op, a, b, env),
            ExprKind::Unary(_, x) => {
                self.expr(x, env);
                (Ty::Dyn, None)
            }
            ExprKind::Index(obj, idx) => {
                let (oty, prov) = self.expr(obj, env);
                self.expr(idx, env);
                match oty {
                    Ty::List(t) => ((*t).clone(), None),
                    Ty::Str => (Ty::Str, None),
                    Ty::Dict | Ty::Tuple | Ty::Set | Ty::Dyn | Ty::Class | Ty::Func => {
                        (Ty::Dyn, None)
                    }
                    Ty::Int | Ty::Float | Ty::Bool | Ty::NoneT => {
                        self.out.push(TypeFinding {
                            line: e.line,
                            span: e.span,
                            code: "type.indexing-a-single-value",
                            message: format!(
                                "square brackets pick an item out of a collection, but this \
                                 is {}{} — a single value has no items to pick",
                                oty.name(),
                                prov_clause(&prov),
                            ),
                        });
                        (Ty::Dyn, None)
                    }
                }
            }
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                let (oty, _) = self.expr(obj, env);
                for part in [start, stop, step].into_iter().flatten() {
                    self.expr(part, env);
                }
                (oty, None)
            }
            ExprKind::Call(name, args) => self.call(e, name, args, env),
            ExprKind::MethodCall(obj, _, args) => {
                self.expr(obj, env);
                for a in args {
                    self.expr(a, env);
                }
                (Ty::Dyn, None)
            }
            ExprKind::Attr(obj, _) => {
                self.expr(obj, env);
                (Ty::Dyn, None)
            }
            ExprKind::IfExp { cond, then, orelse } => {
                self.expr(cond, env);
                let (t1, _) = self.expr(then, env);
                let (t2, _) = self.expr(orelse, env);
                (if t1 == t2 { t1 } else { Ty::Dyn }, None)
            }
            ExprKind::ListComp { element, clauses } | ExprKind::SetComp { element, clauses } => {
                // Comprehension scope: bind loop vars as Dyn, walk parts.
                let mut inner = env.clone();
                for c in clauses {
                    match c {
                        crate::ast::CompClause::For { vars, iter } => {
                            self.expr(iter, &mut inner);
                            for v in vars {
                                inner.insert(
                                    v.clone(),
                                    Fact {
                                        ty: Ty::Dyn,
                                        line: e.line,
                                        why: "comprehension variable".into(),
                                    },
                                );
                            }
                        }
                        crate::ast::CompClause::If(cond) => {
                            self.expr(cond, &mut inner);
                        }
                    }
                }
                self.expr(element, &mut inner);
                (
                    if matches!(e.kind, ExprKind::SetComp { .. }) {
                        Ty::Set
                    } else {
                        Ty::List(Box::new(Ty::Dyn))
                    },
                    None,
                )
            }
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => {
                let mut inner = env.clone();
                for c in clauses {
                    match c {
                        crate::ast::CompClause::For { vars, iter } => {
                            self.expr(iter, &mut inner);
                            for v in vars {
                                inner.insert(
                                    v.clone(),
                                    Fact {
                                        ty: Ty::Dyn,
                                        line: e.line,
                                        why: "comprehension variable".into(),
                                    },
                                );
                            }
                        }
                        crate::ast::CompClause::If(cond) => {
                            self.expr(cond, &mut inner);
                        }
                    }
                }
                self.expr(key, &mut inner);
                self.expr(value, &mut inner);
                (Ty::Dict, None)
            }
            _ => (Ty::Dyn, None),
        }
    }

    fn binop(
        &mut self,
        e: &Expr,
        op: BinOp,
        a: &Expr,
        b: &Expr,
        env: &mut HashMap<String, Fact>,
    ) -> (Ty, Option<Fact>) {
        let (ta, pa) = self.expr(a, env);
        let (tb, pb) = self.expr(b, env);
        match op {
            BinOp::Add => match (&ta, &tb) {
                (Ty::Str, Ty::Str) => (Ty::Str, None),
                (Ty::Str, t) | (t, Ty::Str) if t.numeric() => {
                    let (sp, op_word) = if ta == Ty::Str {
                        (&pa, "left")
                    } else {
                        (&pb, "right")
                    };
                    let _ = op_word;
                    self.out.push(TypeFinding {
                        line: e.line,
                        span: e.span,
                        code: "type.str-plus-number",
                        message: format!(
                            "`+` can join two pieces of text, or add two numbers — this mixes \
                             text{} with a number. int(...) turns text into a number; \
                             str(...) goes the other way",
                            prov_clause(sp),
                        ),
                    });
                    (Ty::Dyn, None)
                }
                _ if ta.numeric() && tb.numeric() => (num_join(&ta, &tb), None),
                (Ty::List(x), Ty::List(_)) => (Ty::List(x.clone()), None),
                _ => (Ty::Dyn, None),
            },
            BinOp::Sub | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::Pow => {
                for (t, p) in [(&ta, &pa), (&tb, &pb)] {
                    if matches!(t, Ty::Str) {
                        self.out.push(TypeFinding {
                            line: e.line,
                            span: e.span,
                            code: "type.str-in-arithmetic",
                            message: format!(
                                "this arithmetic was handed text{} — `+` joins text, but \
                                 there is no meaning for this operation on it. int(...) \
                                 turns numeric text into a number first",
                                prov_clause(p),
                            ),
                        });
                        return (Ty::Dyn, None);
                    }
                }
                if ta.numeric() && tb.numeric() {
                    if matches!(op, BinOp::Div) {
                        (Ty::Float, None)
                    } else {
                        (num_join(&ta, &tb), None)
                    }
                } else {
                    (Ty::Dyn, None)
                }
            }
            BinOp::Mul => match (&ta, &tb) {
                (Ty::Str, t) | (t, Ty::Str) if t.numeric() => (Ty::Str, None),
                _ if ta.numeric() && tb.numeric() => (num_join(&ta, &tb), None),
                _ => (Ty::Dyn, None),
            },
            BinOp::Eq | BinOp::Ne | BinOp::In | BinOp::NotIn => (Ty::Bool, None),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                // Mixed text/number ordering on NAMES is a real mistake, but
                // the literal case is a compile error already; stay quiet on
                // Dyn and let phase C revisit.
                if (ta == Ty::Str && tb.numeric()) || (tb == Ty::Str && ta.numeric()) {
                    let sp = if ta == Ty::Str { &pa } else { &pb };
                    self.out.push(TypeFinding {
                        line: e.line,
                        span: e.span,
                        code: "type.comparing-text-with-number",
                        message: format!(
                            "there is no way to say whether text{} is bigger than a number — \
                             compare two numbers (int(...) converts) or two pieces of text",
                            prov_clause(sp),
                        ),
                    });
                }
                (Ty::Bool, None)
            }
            BinOp::And | BinOp::Or => (if ta == tb { ta } else { Ty::Dyn }, None),
            _ => (Ty::Dyn, None),
        }
    }

    fn call(
        &mut self,
        e: &Expr,
        name: &str,
        args: &[Expr],
        env: &mut HashMap<String, Fact>,
    ) -> (Ty, Option<Fact>) {
        // A name that holds a VALUE, used with call parentheses.
        if let Some(f) = env.get(name).cloned()
            && !matches!(f.ty, Ty::Func | Ty::Class | Ty::Dyn)
        {
            self.out.push(TypeFinding {
                line: e.line,
                span: e.span,
                code: "type.calling-a-value",
                message: format!(
                    "`{name}` is {} — line {} {} — so it can't be called like a function. \
                     Did the parentheses mean something else, or was `{name}` meant to stay \
                     a function?",
                    f.ty.name(),
                    f.line,
                    f.why,
                ),
            });
            for a in args {
                self.expr(a, env);
            }
            return (Ty::Dyn, None);
        }
        // Annotated user function: check each argument against its promise.
        if let Some(sig) = self.sigs.get(name).cloned() {
            for (i, arg) in args.iter().enumerate() {
                let (at, prov) = self.expr(arg, env);
                if let Some((pname, want)) = sig.params.get(i)
                    && *want != Ty::Dyn
                    && at != Ty::Dyn
                    && at != *want
                    && !(at == Ty::Int && *want == Ty::Float)
                    && !(at == Ty::Bool && *want == Ty::Int)
                {
                    self.out.push(TypeFinding {
                        line: e.line,
                        span: e.span,
                        code: "type.argument-contradicts-signature",
                        message: format!(
                            "`{name}` asks for {} for `{pname}` (line {}) — this call hands \
                             it {}{}",
                            want.name(),
                            sig.line,
                            at.name(),
                            prov_clause(&prov),
                        ),
                    });
                }
            }
            if args.len() < sig.n_required || args.len() > sig.n_params {
                // Arity is a compile error already; no advisory duplicate.
            }
            return (sig.ret.clone(), None);
        }
        // Builtins the derivations lean on.
        let ret = match name {
            "input" => Ty::Str,
            "str" => Ty::Str,
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "len" => {
                if let Some(arg) = args.first() {
                    let (t, prov) = self.expr(arg, env);
                    if matches!(t, Ty::Int | Ty::Float | Ty::Bool | Ty::NoneT) {
                        self.out.push(TypeFinding {
                            line: e.line,
                            span: e.span,
                            code: "type.len-of-single-value",
                            message: format!(
                                "len() counts a collection's items, but this is {}{} — a \
                                 single value has no items to count",
                                t.name(),
                                prov_clause(&prov),
                            ),
                        });
                    }
                }
                return (Ty::Int, None);
            }
            "range" => Ty::Dyn,
            "abs" | "round" | "sum" | "min" | "max" => Ty::Dyn,
            "sorted" | "list" => Ty::List(Box::new(Ty::Dyn)),
            "set" => Ty::Set,
            "tuple" => Ty::Tuple,
            "dict" => Ty::Dict,
            "print" => Ty::NoneT,
            _ => Ty::Dyn,
        };
        for a in args {
            self.expr(a, env);
        }
        (ret, None)
    }
}

fn num_join(a: &Ty, b: &Ty) -> Ty {
    if *a == Ty::Float || *b == Ty::Float {
        Ty::Float
    } else {
        Ty::Int
    }
}

/// Join branch outcomes into `env`: agreement keeps the fact, disagreement
/// becomes Dyn (silent — the phase-C question).
fn join_into(env: &mut HashMap<String, Fact>, outcomes: &[HashMap<String, Fact>]) {
    let mut names: Vec<&String> = outcomes.iter().flat_map(|o| o.keys()).collect();
    names.sort();
    names.dedup();
    let mut joined: HashMap<String, Fact> = HashMap::new();
    for name in names {
        let facts: Vec<Option<&Fact>> = outcomes.iter().map(|o| o.get(name)).collect();
        let Some(Some(first)) = facts.first() else {
            continue;
        };
        let mut fact = (*first).clone();
        for f in facts.iter().skip(1) {
            match f {
                Some(f) if f.ty == fact.ty => {}
                _ => fact.ty = Ty::Dyn,
            }
        }
        joined.insert(name.clone(), fact);
    }
    *env = joined;
}

/// The ledger clause: " — line N assigned it …" when we know, empty when
/// we don't. Keeps every message honest about what it can actually cite.
fn prov_clause(p: &Option<Fact>) -> String {
    match p {
        Some(f) => format!(" (line {} {})", f.line, f.why),
        None => String::new(),
    }
}

/// A short phrase for WHY an assignment made a name its type — the ledger
/// entry later messages cite.
fn describe_value(value: &Expr, ty: &Ty) -> String {
    match &value.kind {
        ExprKind::Str(s) if s.len() <= 12 => format!("assigned it \"{s}\" (text)"),
        ExprKind::Str(_) => "assigned it text".into(),
        ExprKind::Int(n) => format!("assigned it {n} (a whole number)"),
        ExprKind::Float(_) => "assigned it a decimal number".into(),
        ExprKind::Call(n, _) if n == "input" => {
            "assigned it input(), and everything typed in arrives as TEXT".into()
        }
        ExprKind::Call(n, _) => format!("assigned it {n}(...)"),
        ExprKind::List(_) => "assigned it a list".into(),
        _ => format!("made it {}", ty.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(src: &str) -> Vec<TypeFinding> {
        let toks = crate::lexer::lex(src).expect("lex");
        let stmts = crate::parser::parse(&toks).expect("parse");
        let stmts = crate::hoist::hoist_nested_functions(stmts).expect("hoist");
        type_findings(&stmts)
    }

    #[test]
    fn the_flagship_derivation_names_the_cause_line() {
        // The message the whole design exists for: the error is at line 2,
        // the CAUSE is line 1, and the message says so.
        let f = findings("age = \"12\"\nnext_year = age + 1\nprint(next_year)\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].code, "type.str-plus-number");
        assert_eq!(f[0].line, 2);
        assert!(f[0].message.contains("line 1"), "{}", f[0].message);
        assert!(f[0].message.contains("\"12\""), "{}", f[0].message);
    }

    #[test]
    fn input_is_the_classic_cause_and_the_ledger_says_so() {
        let f = findings("name = input()\nprint(name + 5)\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("arrives as TEXT"), "{}", f[0].message);
    }

    #[test]
    fn every_must_reject_shape_produces_a_finding() {
        for (src, code) in [
            (
                "count: int = \"none yet\"\nprint(count)\n",
                "type.annotation-contradicted",
            ),
            (
                "def area(width: int, height: int) -> int:\n    return width * height\nprint(area(\"3\", 4))\n",
                "type.argument-contradicts-signature",
            ),
            ("total = 5\nprint(total(3))\n", "type.calling-a-value"),
            (
                "score = 42\nprint(score[0])\n",
                "type.indexing-a-single-value",
            ),
            (
                "def double(n: int) -> int:\n    return \"twice \" + str(n)\nprint(double(3))\n",
                "type.return-contradicts-signature",
            ),
            (
                "full = \"hello world\"\nshort = full - \"world\"\nprint(short)\n",
                "type.str-in-arithmetic",
            ),
            ("n = 5\nprint(len(n))\n", "type.len-of-single-value"),
            (
                "n = 5\nfor x in n:\n    print(x)\n",
                "type.for-over-single-value",
            ),
        ] {
            let f = findings(src);
            assert!(
                f.iter().any(|x| x.code == code),
                "expected {code} for {src:?}, got {f:?}"
            );
        }
    }

    #[test]
    fn the_ok_shapes_stay_silent() {
        // The false-positive gate in miniature (the oracle test covers the
        // real files): inference must survive beginner code untouched.
        for src in [
            "x = 5\ny = x + 3\nprint(y)\n",
            "a = 1 + 2.5\nb = 3 * 1.5\nc = 7 / 2\nprint(a)\nprint(b)\nprint(c)\n",
            "total = 0\nfor i in range(5):\n    total = total + i\nprint(total)\n",
            "s = \"\"\nfor i in range(3):\n    s = s + \"ab\"\nprint(s)\n",
            "def area(w: int, h: int) -> int:\n    return w * h\nprint(area(3, 4))\n",
            "def f(n: int) -> float:\n    return n / 2\nprint(f(5))\n",
            "xs = [1, 2, 3]\nprint(xs[0] + 1)\n",
            "name = input()\nprint(\"hi \" + name)\n",
            "x = 5\nif x > 3:\n    y = 1\nelse:\n    y = 2\nprint(y + 1)\n",
        ] {
            let f = findings(src);
            assert!(f.is_empty(), "false positive on {src:?}: {f:?}");
        }
    }

    #[test]
    fn dyn_stays_honest_and_silent() {
        // A name the pass can't type produces NOTHING — no finding, no guess.
        let f = findings("a, b = 1, 2\nprint(a + \"x\")\n");
        assert!(f.is_empty(), "unpack targets are Dyn in tier 1: {f:?}");
    }
}
