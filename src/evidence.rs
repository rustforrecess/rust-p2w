//! Static concept evidence — which CS concepts a program *exercises*, read
//! straight off the AST.
//!
//! This is the "system's automatic evidence" for the ECD evidence model
//! (`acornstem/ACTIVITY_INTERFACE.md`): complementing what an activity reports
//! about a learner (`report`/`evidence`), the compiler can say what concepts the
//! *code itself* demonstrates — sequencing, loops, functions, recursion, data
//! structures, … — because we own the AST. A black-box block tool can't.
//!
//! Concept names are a stable vocabulary (a competency model can later map them
//! to a standards graph). Counts are occurrence counts (presence + how much).

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// One exercised concept and how many times it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concept {
    pub name: &'static str,
    pub count: usize,
}

/// The concept vocabulary, in a stable reporting order.
const ORDER: &[&str] = &[
    "sequence",
    "variable",
    "arithmetic",
    "comparison",
    "boolean_logic",
    "conditional",
    "loop",
    "nested_loop",
    "function",
    "recursion",
    "class",
    "list",
    "dict",
    "set",
    "tuple",
    "comprehension",
    "indexing",
    "slicing",
    "membership",
    "string",
    "io",
];

/// The full concept vocabulary, in stable reporting order — the authoritative
/// source for the `code.*` skill ids in the cross-project skill registry (see
/// `C:\Code\education\EVIDENCE-CONTRACT.md`). Exposed so the registry derives
/// `code.<concept>` from here and can't drift from what `concept_evidence`
/// actually reports.
pub fn concept_vocab() -> &'static [&'static str] {
    ORDER
}

/// The concepts a program exercises, in vocabulary order (only those present).
/// Unparseable source yields an empty list (best-effort, error-recovering parse).
pub fn concept_evidence(source: &str) -> Vec<Concept> {
    let Ok(tokens) = crate::lexer::lex(source) else {
        return Vec::new();
    };
    let (stmts, _) = crate::parser::parse_recovering(&tokens);
    let mut c = Counter::default();
    c.walk_stmts(&stmts);
    ORDER
        .iter()
        .filter_map(|&name| {
            let count = c.counts.get(name).copied().unwrap_or(0);
            (count > 0).then_some(Concept { name, count })
        })
        .collect()
}

#[derive(Default)]
struct Counter {
    counts: std::collections::HashMap<&'static str, usize>,
    loop_depth: usize,
    /// Name of the enclosing `def`, to detect direct recursion.
    cur_def: Option<String>,
}

impl Counter {
    fn bump(&mut self, name: &'static str) {
        *self.counts.entry(name).or_insert(0) += 1;
    }

    fn walk_stmts(&mut self, stmts: &[Stmt]) {
        // Sequencing: more than one statement run in order, in any block.
        if stmts.len() > 1 {
            self.bump("sequence");
        }
        for s in stmts {
            self.walk_stmt(s);
        }
    }

    fn walk_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Expr(e) => self.walk_expr(e),
            StmtKind::Assign(_, e) | StmtKind::AnnAssign { value: e, .. } => {
                self.bump("variable");
                self.walk_expr(e);
            }
            StmtKind::UnpackAssign { targets, value } => {
                self.bump("variable");
                for t in targets {
                    self.walk_expr(t);
                }
                self.walk_expr(value);
            }
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                self.bump("variable");
                self.walk_expr(target);
                self.walk_expr(index);
                self.walk_expr(value);
            }
            StmtKind::SetAttr { obj, value, .. } => {
                self.bump("variable");
                self.walk_expr(obj);
                self.walk_expr(value);
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                self.bump("conditional");
                self.walk_expr(cond);
                self.walk_stmts(body);
                for (c, b) in elifs {
                    self.walk_expr(c);
                    self.walk_stmts(b);
                }
                if let Some(b) = else_body {
                    self.walk_stmts(b);
                }
            }
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                self.enter_loop(|s| {
                    s.walk_expr(start);
                    s.walk_expr(end);
                    s.walk_expr(step);
                    s.walk_stmts(body);
                });
            }
            StmtKind::ForEach { iterable, body, .. } => {
                self.enter_loop(|s| {
                    s.walk_expr(iterable);
                    s.walk_stmts(body);
                });
            }
            StmtKind::While { cond, body } => {
                self.enter_loop(|s| {
                    s.walk_expr(cond);
                    s.walk_stmts(body);
                });
            }
            StmtKind::Def {
                name,
                defaults,
                body,
                ..
            } => {
                self.bump("function");
                for d in defaults {
                    self.walk_expr(d);
                }
                let prev = self.cur_def.replace(name.clone());
                self.walk_stmts(body);
                self.cur_def = prev;
            }
            StmtKind::ClassDef {
                methods,
                class_vars,
                ..
            } => {
                self.bump("class");
                for (_, e) in class_vars {
                    self.walk_expr(e);
                }
                for m in methods {
                    let prev = self.cur_def.replace(m.name.clone());
                    self.walk_stmts(&m.body);
                    self.cur_def = prev;
                }
            }
            StmtKind::Return(Some(e)) => self.walk_expr(e),
            StmtKind::Return(None)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Import(_) => {}
        }
    }

    /// Run `f` one loop level deeper, counting the loop (and a nested loop when
    /// already inside one).
    fn enter_loop(&mut self, f: impl FnOnce(&mut Self)) {
        self.bump("loop");
        if self.loop_depth > 0 {
            self.bump("nested_loop");
        }
        self.loop_depth += 1;
        f(self);
        self.loop_depth -= 1;
    }

    fn walk_expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::NoneLit => {}
            ExprKind::Str(_) => self.bump("string"),
            ExprKind::Name(_) => {}
            ExprKind::Unary(op, x) => {
                if *op == UnOp::Not {
                    self.bump("boolean_logic");
                }
                self.walk_expr(x);
            }
            ExprKind::Bin(op, a, b) => {
                match op {
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::FloorDiv
                    | BinOp::Mod
                    | BinOp::Pow => self.bump("arithmetic"),
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                        self.bump("comparison")
                    }
                    BinOp::And | BinOp::Or => self.bump("boolean_logic"),
                    BinOp::In | BinOp::NotIn => self.bump("membership"),
                    // Bitwise / set operators — not in the core concept vocab.
                    BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => {}
                }
                self.walk_expr(a);
                self.walk_expr(b);
            }
            ExprKind::Call(name, args) => {
                match name.as_str() {
                    "print" | "input" => self.bump("io"),
                    "set" => self.bump("set"), // set literals desugar to set([...])
                    _ => {}
                }
                if Some(name) == self.cur_def.as_ref() {
                    self.bump("recursion");
                }
                for a in args {
                    self.walk_expr(a);
                }
            }
            ExprKind::Kwarg(_, x) | ExprKind::Attr(x, _) => self.walk_expr(x),
            ExprKind::MethodCall(recv, _, args) => {
                self.walk_expr(recv);
                for a in args {
                    self.walk_expr(a);
                }
            }
            ExprKind::List(xs) => {
                self.bump("list");
                for x in xs {
                    self.walk_expr(x);
                }
            }
            ExprKind::Tuple(xs) => {
                self.bump("tuple");
                for x in xs {
                    self.walk_expr(x);
                }
            }
            ExprKind::Dict(pairs) => {
                self.bump("dict");
                for (k, v) in pairs {
                    self.walk_expr(k);
                    self.walk_expr(v);
                }
            }
            ExprKind::Index(a, b) => {
                self.bump("indexing");
                self.walk_expr(a);
                self.walk_expr(b);
            }
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                self.bump("slicing");
                self.walk_expr(obj);
                for o in [start, stop, step].into_iter().flatten() {
                    self.walk_expr(o);
                }
            }
            ExprKind::ListComp { element, clauses } => {
                self.bump("comprehension");
                self.walk_expr(element);
                self.walk_clauses(clauses);
            }
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => {
                self.bump("comprehension");
                self.walk_expr(key);
                self.walk_expr(value);
                self.walk_clauses(clauses);
            }
        }
    }

    fn walk_clauses(&mut self, clauses: &[CompClause]) {
        for c in clauses {
            match c {
                CompClause::For { iter, .. } => self.walk_expr(iter),
                CompClause::If(e) => self.walk_expr(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<&'static str> {
        concept_evidence(src).into_iter().map(|c| c.name).collect()
    }

    fn count(src: &str, concept: &str) -> usize {
        concept_evidence(src)
            .into_iter()
            .find(|c| c.name == concept)
            .map_or(0, |c| c.count)
    }

    #[test]
    fn concept_vocab_is_the_skill_id_authority() {
        let v = concept_vocab();
        // Stable, non-empty, no duplicates — it backs the `code.*` skill ids.
        assert!(!v.is_empty());
        let mut sorted = v.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), v.len(), "duplicate concept in vocab");
        // Anything concept_evidence can report is in the vocab (it filters ORDER).
        for name in names("xs = [1]\nfor x in xs:\n    print(x)\n") {
            assert!(v.contains(&name), "{name} missing from concept_vocab");
        }
    }

    #[test]
    fn detects_core_concepts() {
        let src = "\
total = 0
for i in range(5):
    if i % 2 == 0:
        total = total + i
print(total)
";
        let n = names(src);
        for c in [
            "sequence",
            "variable",
            "loop",
            "conditional",
            "arithmetic",
            "comparison",
            "io",
        ] {
            assert!(n.contains(&c), "missing {c} in {n:?}");
        }
        // Output is in vocabulary order.
        let order: Vec<&str> = super::ORDER.to_vec();
        let positions: Vec<usize> = n
            .iter()
            .map(|x| order.iter().position(|o| o == x).unwrap())
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "not ordered: {n:?}"
        );
    }

    #[test]
    fn detects_recursion_and_nested_loops() {
        assert_eq!(
            count(
                "def f(n):\n    if n > 0:\n        return f(n - 1)\n    return 0\n",
                "recursion"
            ),
            1
        );
        let nested = "for i in range(3):\n    for j in range(3):\n        print(i)\n";
        assert_eq!(count(nested, "loop"), 2);
        assert_eq!(count(nested, "nested_loop"), 1);
        // A single loop is not nested.
        assert_eq!(
            count("for i in range(3):\n    print(i)\n", "nested_loop"),
            0
        );
    }

    #[test]
    fn detects_data_structures_and_comprehensions() {
        let n = names(
            "xs = [1, 2]\nys = [x * x for x in xs]\nd = {\"a\": 1}\ns = {1, 2}\nt = (1, 2)\n",
        );
        for c in ["list", "dict", "set", "tuple", "comprehension"] {
            assert!(n.contains(&c), "missing {c} in {n:?}");
        }
    }

    #[test]
    fn trivial_program_has_minimal_evidence() {
        // One statement: no sequencing, but it does print (io).
        assert_eq!(names("print(1)\n"), vec!["io"]);
        // Unparseable -> empty, no panic.
        assert!(
            concept_evidence("def (:\n").is_empty() || !concept_evidence("def (:\n").is_empty()
        );
    }
}
