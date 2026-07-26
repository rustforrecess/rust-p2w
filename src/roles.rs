//! Deterministic variable-role recognition, read off the owned AST.
//!
//! Companion to `evidence.rs`: where that reports *which concepts* a program
//! exercises, this reports the *role each loop variable plays* — the Sajaniemi
//! "roles of variables" vocabulary that CausalCode validated as unambiguous
//! specs (see `education/causalcode/docs/spec-validation.md`). Because we own
//! the AST, recognition is a **decidable function**, not an LLM guess.
//!
//! v1 covers the scalar **accumulator family** — the roles that share a
//! self-referential arithmetic update `v = v <op> e` (Python's `v += e`
//! desugars to exactly this in the p2w AST, so there is no augmented-assign
//! special case). The role is decided by three AST-checkable questions:
//!
//! ```text
//! operand e involves data (a driver / a subscript / a data-derived var) ?
//!   yes -> reset to a literal elsewhere in the loop ?  ResetGatherer : Gatherer
//!   no  -> reset to a literal ?      ResetCounter
//!          gated by a data condition (inside an `if`) ? Counter
//!          else (unconditional constant step)          Stepper
//! ```
//!
//! Also covered: the assignment-based scalar roles that don't need liveness —
//! **most-wanted-holder** (`if x > best: best = x`, or `best = max(best, x)`)
//! and **one-way-flag** (`if cond: found = True`, a single latched constant; a
//! two-way toggle is *not* a flag).
//!
//! Deferred: **follower** and **temporary** are the same syntax (`prev = x`, an
//! unconditional element copy) distinguished only by read/write order within
//! the iteration — that needs `reuse.rs` liveness. **transformation**,
//! **fixed-value**, **organizer**, **walker**, **container** are later
//! predicates. The two static approximations here (data-flow taint; treating a
//! `while` condition's names as loop drivers) are where `reuse.rs` liveness and
//! the `debug.rs` `Vm` make recognition exact rather than approximate — the
//! point of running on the owned substrate. See `docs/CAUSALCODE_INTEGRATION.md`.

use crate::ast::{CompClause, Expr, ExprKind, Stmt, StmtKind};
use std::collections::{HashMap, HashSet};

/// A recognized scalar variable role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    // Accumulator family (self-referential arithmetic update `v = v <op> e`).
    Stepper,
    Counter,
    Gatherer,
    ResetCounter,
    ResetGatherer,
    // Assignment-based roles.
    /// Best-so-far value, conditionally replaced (`if x > best: best = x`, or
    /// `best = max(best, x)`).
    MostWantedHolder,
    /// A two-state variable latched to the other state and never reset
    /// (`if cond: found = True`).
    OneWayFlag,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Stepper => "stepper",
            Role::Counter => "counter",
            Role::Gatherer => "gatherer",
            Role::ResetCounter => "reset_counter",
            Role::ResetGatherer => "reset_gatherer",
            Role::MostWantedHolder => "most_wanted_holder",
            Role::OneWayFlag => "one_way_flag",
        }
    }
}

/// A variable found to play a recognized role, with the source line of its
/// first in-loop update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarRole {
    pub name: String,
    pub role: Role,
    pub line: usize,
}

/// The recognized scalar variable roles in `source`, ordered by source line
/// then name. Unparseable source yields an empty list (best-effort,
/// error-recovering parse).
pub fn variable_roles(source: &str) -> Vec<VarRole> {
    let Ok(tokens) = crate::lexer::lex(source) else {
        return Vec::new();
    };
    let (stmts, _) = crate::parser::parse_recovering(&tokens);
    roles_of(&stmts)
}

/// The recognized scalar variable roles in an already-parsed program.
pub fn roles_of(stmts: &[Stmt]) -> Vec<VarRole> {
    let mut a = Analysis::default();
    a.collect_drivers(stmts);
    a.taint(stmts);
    a.walk(stmts, false, false);
    a.finish()
}

/// A boolean or small-integer literal (the values a one-way flag latches to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LitVal {
    B(bool),
    I(i64),
}

#[derive(Default)]
struct Info {
    // accumulator family
    seen_update: bool,
    data: bool,
    gated: bool,
    reset: bool,
    // assignment-based
    /// `v = max(v, ..)` / `v = min(v, ..)` — a most-wanted holder.
    maxmin: bool,
    /// `v = <copy of the current element>` under an `if` — a holder.
    copy_cond: bool,
    /// Literal constants assigned to `v` inside the loop, and whether any such
    /// assignment was conditional (a one-way flag latches a single value under
    /// a condition; a two-way toggle assigns two distinct values).
    lits: std::collections::HashSet<LitVal>,
    lit_gated: bool,
    line: usize,
}

#[derive(Default)]
struct Analysis {
    /// Loop control variables: `for` targets and names in a `while` condition.
    /// (Approximation; `reuse.rs` gives exact induction variables.)
    drivers: HashSet<String>,
    /// Variables carrying input-derived data (subscript / driver / transitively).
    /// (Approximation; `reuse.rs` def-use gives exact data flow.)
    data_vars: HashSet<String>,
    updates: HashMap<String, Info>,
}

impl Analysis {
    // ---- drivers: for/foreach targets + names in a while condition ----
    fn collect_drivers(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::For { var, body, .. } | StmtKind::ForEach { var, body, .. } => {
                    self.drivers.insert(var.clone());
                    self.collect_drivers(body);
                }
                StmtKind::While { cond, body } => {
                    add_names(cond, &mut self.drivers);
                    self.collect_drivers(body);
                }
                StmtKind::If {
                    body,
                    elifs,
                    else_body,
                    ..
                } => {
                    self.collect_drivers(body);
                    for (_, b) in elifs {
                        self.collect_drivers(b);
                    }
                    if let Some(b) = else_body {
                        self.collect_drivers(b);
                    }
                }
                StmtKind::Def { body, .. } => self.collect_drivers(body),
                StmtKind::ClassDef { methods, .. } => {
                    for m in methods {
                        self.collect_drivers(&m.body);
                    }
                }
                _ => {}
            }
        }
    }

    // ---- taint: a var assigned from a data-referencing expr is itself data ----
    fn taint(&mut self, stmts: &[Stmt]) {
        let mut assigns: Vec<(&str, &Expr)> = Vec::new();
        collect_assigns(stmts, &mut assigns);
        loop {
            let before = self.data_vars.len();
            let mut add: Vec<String> = Vec::new();
            for (name, value) in &assigns {
                if !self.data_vars.contains(*name)
                    && refs_data(value, &self.drivers, &self.data_vars)
                {
                    add.push((*name).to_string());
                }
            }
            for n in add {
                self.data_vars.insert(n);
            }
            if self.data_vars.len() == before {
                break;
            }
        }
    }

    // ---- classification pass: collect per-variable update facts inside loops ----
    fn walk(&mut self, stmts: &[Stmt], in_loop: bool, in_if: bool) {
        for s in stmts {
            self.walk_stmt(s, in_loop, in_if);
        }
    }

    fn walk_stmt(&mut self, s: &Stmt, in_loop: bool, in_if: bool) {
        match &s.kind {
            StmtKind::Assign(name, value) if in_loop => {
                // Compute the facts with immutable borrows first, then record.
                let selfref_data = selfref_operand(name, value)
                    .map(|op| refs_data(op, &self.drivers, &self.data_vars));
                let is_maxmin = selfref_data.is_none() && maxmin_selfref(name, value);
                let lit = literal_value(value);
                let is_lit = is_literal(value);
                let is_cond_copy = in_if
                    && selfref_data.is_none()
                    && !is_maxmin
                    && !is_lit
                    && is_copy_of_data(value, &self.drivers, &self.data_vars);
                let line = s.line;

                let e = self.updates.entry(name.clone()).or_default();
                if e.line == 0 {
                    e.line = line;
                }
                if let Some(data) = selfref_data {
                    e.seen_update = true;
                    e.data |= data;
                    e.gated |= in_if;
                } else if is_maxmin {
                    e.maxmin = true;
                } else if is_lit {
                    e.reset = true; // an in-loop literal is the accumulator reset
                    if let Some(lv) = lit {
                        e.lits.insert(lv);
                        e.lit_gated |= in_if;
                    }
                } else if is_cond_copy {
                    e.copy_cond = true;
                }
                // An unconditional bare copy (`prev = x`) is a follower OR a
                // temporary — the two differ only by read/write order within the
                // iteration, which needs `reuse.rs` liveness; deferred to that
                // step. A computed `v = f(x)` transformation is deferred too.
            }
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                self.walk(body, in_loop, true);
                for (_, b) in elifs {
                    self.walk(b, in_loop, true);
                }
                if let Some(b) = else_body {
                    self.walk(b, in_loop, true);
                }
            }
            // Entering a loop body: in_loop, and reset in_if (the data-gate that
            // matters is one *inside* this loop).
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => self.walk(body, true, false),
            StmtKind::Def { body, .. } => self.walk(body, false, false),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    self.walk(&m.body, false, false);
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Vec<VarRole> {
        let Analysis {
            drivers, updates, ..
        } = self;
        let mut out: Vec<VarRole> = updates
            .into_iter()
            .filter(|(name, _)| !drivers.contains(name))
            .filter_map(|(name, info)| {
                let role = if info.seen_update {
                    // accumulator family
                    if info.data {
                        if info.reset {
                            Role::ResetGatherer
                        } else {
                            Role::Gatherer
                        }
                    } else if info.reset {
                        Role::ResetCounter
                    } else if info.gated {
                        Role::Counter
                    } else {
                        Role::Stepper
                    }
                } else if info.maxmin || info.copy_cond {
                    Role::MostWantedHolder
                } else if info.lits.len() == 1 && info.lit_gated {
                    // Latched a single constant under a condition, never the
                    // other value (two distinct literals = a two-way toggle).
                    Role::OneWayFlag
                } else {
                    return None;
                };
                Some(VarRole {
                    name,
                    role,
                    line: info.line,
                })
            })
            .collect();
        out.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
        out
    }
}

// ---- expression helpers ----

fn is_name(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Name(n) if n == name)
}

fn is_literal(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_)
    )
}

/// A boolean / small-int literal value (for one-way-flag latch detection).
fn literal_value(e: &Expr) -> Option<LitVal> {
    match &e.kind {
        ExprKind::Bool(b) => Some(LitVal::B(*b)),
        ExprKind::Int(i) => Some(LitVal::I(*i)),
        _ => None,
    }
}

/// Is `value` a bare copy of the current element — a name that is a loop driver
/// or data-derived, or a subscript read? (Not a computed expression.)
fn is_copy_of_data(value: &Expr, drivers: &HashSet<String>, data: &HashSet<String>) -> bool {
    match &value.kind {
        ExprKind::Name(n) => drivers.contains(n) || data.contains(n),
        ExprKind::Index(..) => true,
        _ => false,
    }
}

/// Is `value` a `max(name, ..)` / `min(name, ..)` call — a most-wanted holder
/// written with the built-in rather than an explicit `if`?
fn maxmin_selfref(name: &str, value: &Expr) -> bool {
    if let ExprKind::Call(f, args) = &value.kind {
        if f == "max" || f == "min" {
            return args.iter().any(|a| is_name(a, name));
        }
    }
    false
}

/// If `value` is `name <op> e` or `e <op> name`, return the other operand `e`.
fn selfref_operand<'a>(name: &str, value: &'a Expr) -> Option<&'a Expr> {
    if let ExprKind::Bin(_, a, b) = &value.kind {
        if is_name(a, name) {
            return Some(b);
        }
        if is_name(b, name) {
            return Some(a);
        }
    }
    None
}

/// Does `e` reference input data — a subscript/slice, or a name that is a loop
/// driver or (transitively) data-derived?
fn refs_data(e: &Expr, drivers: &HashSet<String>, data: &HashSet<String>) -> bool {
    let mut found = false;
    for_each_subexpr(e, &mut |x| match &x.kind {
        ExprKind::Index(..) | ExprKind::Slice { .. } => found = true,
        ExprKind::Name(n) if drivers.contains(n) || data.contains(n) => found = true,
        _ => {}
    });
    found
}

fn add_names(e: &Expr, set: &mut HashSet<String>) {
    for_each_subexpr(e, &mut |x| {
        if let ExprKind::Name(n) = &x.kind {
            set.insert(n.clone());
        }
    });
}

fn collect_assigns<'a>(stmts: &'a [Stmt], out: &mut Vec<(&'a str, &'a Expr)>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(n, v) => out.push((n.as_str(), v)),
            StmtKind::AnnAssign { name, value, .. } => out.push((name.as_str(), value)),
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                collect_assigns(body, out);
                for (_, b) in elifs {
                    collect_assigns(b, out);
                }
                if let Some(b) = else_body {
                    collect_assigns(b, out);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => collect_assigns(body, out),
            StmtKind::Def { body, .. } => collect_assigns(body, out),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    collect_assigns(&m.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Visit `e` and every sub-expression (pre-order).
fn for_each_subexpr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Unary(_, x) | ExprKind::Kwarg(_, x) | ExprKind::Attr(x, _) => {
            for_each_subexpr(x, f)
        }
        ExprKind::Bin(_, a, b) | ExprKind::Index(a, b) => {
            for_each_subexpr(a, f);
            for_each_subexpr(b, f);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                for_each_subexpr(a, f);
            }
        }
        ExprKind::MethodCall(recv, _, args) => {
            for_each_subexpr(recv, f);
            for a in args {
                for_each_subexpr(a, f);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) => {
            for x in xs {
                for_each_subexpr(x, f);
            }
        }
        ExprKind::Dict(pairs) => {
            for (k, v) in pairs {
                for_each_subexpr(k, f);
                for_each_subexpr(v, f);
            }
        }
        ExprKind::Slice {
            obj,
            start,
            stop,
            step,
        } => {
            for_each_subexpr(obj, f);
            for o in [start, stop, step].into_iter().flatten() {
                for_each_subexpr(o, f);
            }
        }
        ExprKind::ListComp { element, clauses } => {
            for_each_subexpr(element, f);
            clause_exprs(clauses, f);
        }
        ExprKind::DictComp {
            key,
            value,
            clauses,
        } => {
            for_each_subexpr(key, f);
            for_each_subexpr(value, f);
            clause_exprs(clauses, f);
        }
        ExprKind::SetComp { element, clauses } => {
            for_each_subexpr(element, f);
            clause_exprs(clauses, f);
        }
        ExprKind::IfExp { cond, then, orelse } => {
            for_each_subexpr(cond, f);
            for_each_subexpr(then, f);
            for_each_subexpr(orelse, f);
        }
        _ => {} // Int, Float, Bool, NoneLit, Str, Name — no children
    }
}

fn clause_exprs(clauses: &[CompClause], f: &mut dyn FnMut(&Expr)) {
    for c in clauses {
        match c {
            CompClause::For { iter, .. } => for_each_subexpr(iter, f),
            CompClause::If(e) => for_each_subexpr(e, f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Role of variable `name` in `src`, or None if it wasn't classified.
    fn role_of(src: &str, name: &str) -> Option<Role> {
        variable_roles(src)
            .into_iter()
            .find(|v| v.name == name)
            .map(|v| v.role)
    }

    #[test]
    fn stepper_vs_gatherer_is_const_vs_data() {
        // `t = t + 1` (constant) is a stepper; `s = s + x` (the element) gathers.
        let src = "a = [1, 2, 3]\nt = 0\ns = 0\nfor x in a:\n    t = t + 1\n    s = s + x\n";
        assert_eq!(role_of(src, "t"), Some(Role::Stepper));
        assert_eq!(role_of(src, "s"), Some(Role::Gatherer));
        // `x` is the driver, never a role.
        assert_eq!(role_of(src, "x"), None);
    }

    #[test]
    fn multiply_splits_on_the_operand_too() {
        // `p = p * 2` steps; `p = p * x` gathers. Same operator, operand decides.
        assert_eq!(
            role_of("a = [2, 3]\np = 1\nfor x in a:\n    p = p * 2\n", "p"),
            Some(Role::Stepper)
        );
        assert_eq!(
            role_of("a = [2, 3]\np = 1\nfor x in a:\n    p = p * x\n", "p"),
            Some(Role::Gatherer)
        );
    }

    #[test]
    fn counter_is_a_gated_constant_step() {
        // Increment by 1 only when a data condition holds -> counter, not stepper.
        let src = "a = [1, -2, 3]\nc = 0\nfor x in a:\n    if x > 0:\n        c = c + 1\n";
        assert_eq!(role_of(src, "c"), Some(Role::Counter));
    }

    #[test]
    fn reset_counter_and_reset_gatherer() {
        // longest-run style: gated +1 with an else-reset -> reset counter.
        let run = "a = [1, 1, 0, 1]\ncur = 0\nfor x in a:\n    if x == 1:\n        cur = cur + 1\n    else:\n        cur = 0\n";
        assert_eq!(role_of(run, "cur"), Some(Role::ResetCounter));
        // Kadane style: data accumulation with a reset-to-0 -> reset gatherer.
        let kad = "a = [1, -9, 3]\ncur = 0\nfor x in a:\n    cur = cur + x\n    if cur < 0:\n        cur = 0\n";
        assert_eq!(role_of(kad, "cur"), Some(Role::ResetGatherer));
    }

    #[test]
    fn most_wanted_holder_conditional_copy_and_maxmin() {
        // `if x > best: best = x` — conditional element copy is a holder.
        let cond = "a = [3, 1, 2]\nbest = a[0]\nfor x in a:\n    if x > best:\n        best = x\n";
        assert_eq!(role_of(cond, "best"), Some(Role::MostWantedHolder));
        // `best = max(best, x)` — the built-in form is the same role.
        let mx = "a = [3, 1, 4]\nbest = a[0]\nfor x in a:\n    best = max(best, x)\n";
        assert_eq!(role_of(mx, "best"), Some(Role::MostWantedHolder));
    }

    #[test]
    fn one_way_flag_latches_a_single_constant() {
        // Set True under a condition, never reset -> one-way flag.
        let src = "a = [1, 0, 2]\nfound = False\nfor x in a:\n    if x == 0:\n        found = True\n";
        assert_eq!(role_of(src, "found"), Some(Role::OneWayFlag));
    }

    #[test]
    fn a_two_way_toggle_is_not_a_flag() {
        // Assigns BOTH True and False in the loop -> a toggle, not a one-way
        // flag (its final value tracks only the last element). Unclassified.
        let src = "a = [1, -2, 3]\nf = False\nfor x in a:\n    if x > 0:\n        f = True\n    else:\n        f = False\n";
        assert_eq!(role_of(src, "f"), None);
    }

    #[test]
    fn data_flows_through_an_intermediate() {
        // `d = a[i] ...; s = s + d` -- taint carries the subscript through `d`,
        // so `s` is a gatherer, not a (constant) counter. The while index `j`
        // is a driver (it appears in the condition).
        let src = "\
v = [5, 1, 4]
n = 3
s = 0
j = 0
while j < n:
    d = v[j]
    s = s + d
    j = j + 1
";
        assert_eq!(role_of(src, "s"), Some(Role::Gatherer));
        assert_eq!(role_of(src, "j"), None); // driver (in the while condition)
    }

    #[test]
    fn unparseable_source_is_empty_not_a_panic() {
        assert!(variable_roles("def (:\n").is_empty());
    }
}
