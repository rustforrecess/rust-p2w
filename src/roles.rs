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
//! Also: **follower** vs **temporary** — the same syntax (`prev = x`, an
//! unconditional element copy) distinguished by *read/write order within the
//! iteration*. A follower is **read before it is written** (it carries the
//! previous value forward); a temporary is **written before it is read** (a
//! swap helper). The order is computed from `reuse.rs`'s tested `vars_read` /
//! `vars_assigned` primitives — the first hookup to the liveness substrate.
//!
//! The remaining scalar roles: **transformation** (a computed derived value
//! `y = 2*x`, no self-reference or carry-across) and **fixed-value** (a scalar
//! literal set before the loop, read inside it, never reassigned).
//!
//! And the data-structure roles: **organizer** (a list whose existing elements
//! are moved in place — `a[i] = a[j]`, a swap — a map like `a[i] = a[i]*2` is
//! *not* one), **container** (a collection with both add and remove — a stack /
//! queue / worklist), and **walker** (a position advanced inside a *data-driven*
//! loop or branch, e.g. `while j < n and a[j] < x: j += 1`, vs a plain
//! fixed-range index which is a stepper).
//!
//! All 14 roles are now recognized. Remaining refinement: the structural
//! detection here (and the two static approximations — data-flow taint, and
//! treating a `while` condition's names as drivers) is where the `debug.rs`
//! `Vm`'s observable invariants (multiset-preserved for an organizer; sortedness
//! per pass; data-driven traversal) make recognition *exact* rather than
//! syntactic. See `docs/CAUSALCODE_INTEGRATION.md`.

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Stmt, StmtKind, UnOp};
use std::collections::{BTreeSet, HashMap, HashSet};

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
    /// Remembers the current element across iterations — an unconditional copy
    /// `prev = x` that is *read before it is written* each iteration.
    Follower,
    /// A short-lived scratch value — an unconditional copy `t = a[i]` that is
    /// *written before it is read* each iteration (the classic swap helper).
    Temporary,
    /// A fresh value computed from the data each iteration, with no
    /// self-reference and no carry-across (`y = 2*x + 7`, `c = (f-32)*5//9`).
    Transformation,
    /// A constant parameter: set to a literal before the loop and only read
    /// inside it, never reassigned (`limit = 100` used in `if x < limit`).
    FixedValue,
    // Data-structure roles.
    /// A list rearranged *in place* — its existing elements moved between
    /// positions (swaps, shifts), preserving the multiset (`a[i], a[j] = ...`).
    Organizer,
    /// A position index whose path/stop is *data-driven* — advanced inside a
    /// loop or branch whose condition inspects the data (search, binary search,
    /// two-pointer), not a fixed range.
    Walker,
    /// A collection whose membership changes both ways — elements added *and*
    /// removed during the loop (a stack / queue / worklist).
    Container,
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
            Role::Follower => "follower",
            Role::Temporary => "temporary",
            Role::Transformation => "transformation",
            Role::FixedValue => "fixed_value",
            Role::Organizer => "organizer",
            Role::Walker => "walker",
            Role::Container => "container",
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

/// Like [`variable_roles`], but with the data-structure tags **confirmed on the
/// `Vm`**: a syntactic organizer/container whose observed invariant *fails* is
/// dropped (`confirm_* == Some(false)`), while a confirmed (`Some(true)`) or
/// merely-unobservable (`None`) tag is kept. This is recognition made *exact*
/// where the interpreter can see the difference — e.g. a lossy shift that looks
/// like an organizer, or a dead-branch `pop` that looks like a container, are
/// removed from the output.
pub fn variable_roles_verified(source: &str) -> Vec<VarRole> {
    let mut roles = variable_roles(source);
    roles.retain(|vr| match vr.role {
        Role::Organizer => confirm_organizer_multiset(source, &vr.name) != Some(false),
        Role::Container => confirm_container_shrinks(source, &vr.name) != Some(false),
        Role::Walker => match list_indexed_by(source, &vr.name) {
            Some(list) => confirm_walker_is_data_driven(source, &vr.name, &list) != Some(false),
            None => true, // can't pair a list to observe -> keep
        },
        _ => true,
    });
    roles
}

/// The name of a list `walker` is used to index (`a[walker]`), or None — so a
/// walker can be Vm-confirmed against the data it navigates.
fn list_indexed_by(source: &str, walker: &str) -> Option<String> {
    let tokens = crate::lexer::lex(source).ok()?;
    let (stmts, _) = crate::parser::parse_recovering(&tokens);
    let mut found = None;
    for_each_expr(&stmts, &mut |e| {
        if found.is_some() {
            return;
        }
        if let ExprKind::Index(base, idx) = &e.kind {
            if let ExprKind::Name(a) = &base.kind {
                let mut refs = BTreeSet::new();
                crate::reuse::vars_read(idx, &mut refs);
                if refs.contains(walker) {
                    found = Some(a.clone());
                }
            }
        }
    });
    found
}

/// Confirm an **organizer** by *observation* on the `Vm`, not syntax: a genuine
/// in-place rearrangement preserves the multiset of the list (same elements,
/// reordered). Runs `source` on the step-interpreter and compares the list's
/// initial literal to its final value.
///
/// - `Some(true)`  — multiset preserved: a real organizer.
/// - `Some(false)` — a value was lost / duplicated / changed: a *lossy* "move"
///   or a map that the syntactic check ([`variable_roles`]) can't tell from a
///   real rearrangement (e.g. a shift `a[i]=a[i+1]` with no wrap-around).
/// - `None` — can't be observed here (no integer-literal initializer for `list`,
///   a runtime error, or a non-integer element).
///
/// This is the `debug.rs` `Vm`-hookup: recognition proposes structurally, the
/// interpreter's observed invariant confirms. The same pattern extends to the
/// other data-structure roles (a sort's per-pass sortedness; a walker's
/// data-driven path). See `docs/CAUSALCODE_INTEGRATION.md`.
pub fn confirm_organizer_multiset(source: &str, list: &str) -> Option<bool> {
    let mut before = initial_int_list(source, list)?;
    let mut st = crate::debug::Stepper::new(source).ok()?;
    st.run(&[]);
    if !matches!(st.status(), crate::debug::Status::Finished) {
        return None; // runtime error or unsupported construct — can't observe
    }
    let mut after = parse_int_list(&st.eval_watch(list).ok()?)?;
    before.sort_unstable();
    after.sort_unstable();
    Some(before == after)
}

/// Confirm a **container** by *tracing* it on the `Vm`: a genuine
/// stack/queue/worklist actually shrinks at some point (a `pop` fires), not just
/// grows. Steps through `source` and watches `name`'s length.
///
/// - `Some(true)`  — the collection's length decreased at least once: a real
///   add-and-remove container.
/// - `Some(false)` — it never shrank: the `pop` the syntactic check
///   ([`variable_roles`]) saw is textual but never fires (a dead branch), so at
///   runtime this is an append-only collection.
/// - `None` — can't be observed (runtime error, a non-integer collection, or the
///   step budget was exhausted).
///
/// Complements [`confirm_organizer_multiset`]: that observes a before/after
/// invariant; this observes the *trace*. Same recognition-proposes /
/// `Vm`-confirms pattern.
pub fn confirm_container_shrinks(source: &str, name: &str) -> Option<bool> {
    let mut st = crate::debug::Stepper::new(source).ok()?;
    let mut prev: Option<usize> = None;
    let mut observed = false;
    let mut decreased = false;
    let mut budget = 100_000;
    loop {
        if let Some(len) = st
            .eval_watch(name)
            .ok()
            .and_then(|r| parse_int_list(&r))
            .map(|v| v.len())
        {
            observed = true;
            if prev.is_some_and(|p| len < p) {
                decreased = true;
            }
            prev = Some(len);
        }
        if !st.is_paused() || budget == 0 {
            break;
        }
        st.step();
        budget -= 1;
    }
    if budget == 0 || matches!(st.status(), crate::debug::Status::Error { .. }) || !observed {
        return None;
    }
    Some(decreased)
}

/// Which sort is it? Distinguish **selection / insertion / bubble** by their
/// *per-pass* invariant, observed on the `Vm` — the round-13 discriminator done
/// by observation rather than syntax. Snapshots `list` at each change of the
/// outer index `outer`, then inspects the last not-yet-sorted pass:
///
/// - **selection** — the sorted **prefix** is *final* (matches the fully-sorted
///   result): elements reach their final position front-to-back.
/// - **bubble** — the sorted **suffix** is final: back-to-front.
/// - **insertion** — the prefix is *internally* sorted but **not** final yet
///   (values in order, but not the final ones).
///
/// Returns the sort name, or `None` if it can't be observed / doesn't match a
/// known shape. (A heuristic over one representative pass — enough to tell the
/// three canonical shapes apart; not a proof.)
pub fn classify_sort_by_passes(source: &str, list: &str, outer: &str) -> Option<&'static str> {
    let mut full = initial_int_list(source, list)?;
    let n = full.len();
    if n < 4 {
        return None;
    }
    full.sort_unstable();

    let mut st = crate::debug::Stepper::new(source).ok()?;
    st.set_watchpoints(&[outer.to_string()]);
    let mut snaps: Vec<Vec<i64>> = Vec::new();
    let mut budget = 1000;
    loop {
        st.run(&[]);
        if let Some(v) = st.eval_watch(list).ok().and_then(|r| parse_int_list(&r)) {
            if v.len() == n {
                snaps.push(v);
            }
        }
        match st.status() {
            crate::debug::Status::Finished => break,
            crate::debug::Status::Error { .. } => return None,
            _ => {}
        }
        budget -= 1;
        if budget == 0 {
            return None;
        }
    }

    // Vote across every not-yet-sorted pass. Per pass:
    //  - insertion: the prefix is sorted but NOT final (internally-sorted length
    //    exceeds the final-prefix length) and nothing is placed at the back.
    //  - selection: finals accumulate at the FRONT (final-prefix > final-suffix).
    //  - bubble: finals accumulate at the BACK (final-suffix > final-prefix).
    let (mut sel, mut ins, mut bub) = (0, 0, 0);
    for s in &snaps {
        if s.len() != n || *s == full {
            continue;
        }
        let fp = final_prefix_len(s, &full);
        let fs = final_suffix_len(s, &full);
        let ip = sorted_prefix_len(s);
        if ip > fp + 1 && fs <= fp + 1 {
            ins += 1;
        } else if fp > fs {
            sel += 1;
        } else if fs > fp {
            bub += 1;
        }
    }
    if ins >= sel && ins >= bub && ins > 0 {
        Some("insertion")
    } else if sel >= bub && sel > 0 {
        Some("selection")
    } else if bub > 0 {
        Some("bubble")
    } else {
        None
    }
}

fn final_prefix_len(s: &[i64], full: &[i64]) -> usize {
    s.iter().zip(full).take_while(|(a, b)| a == b).count()
}

fn final_suffix_len(s: &[i64], full: &[i64]) -> usize {
    s.iter().rev().zip(full.iter().rev()).take_while(|(a, b)| a == b).count()
}

fn sorted_prefix_len(s: &[i64]) -> usize {
    let mut k = 1;
    while k < s.len() && s[k - 1] <= s[k] {
        k += 1;
    }
    k.min(s.len())
}

/// Does an organizer actually **sort**? A step up from role to algorithm-level:
/// an organizer preserves the multiset, but only *observation* tells a sort from
/// a mere rearrangement — a reverse and a bubble sort are both organizers, yet
/// only one produces ordered output. Runs `source` and checks the final list is
/// the initial one in ascending (or descending) order.
///
/// - `Some(true)`  — sorted output (a genuine sort).
/// - `Some(false)` — a permutation that is not ordered (reverse, rotate, a
///   partial/partition rearrangement, or a buggy sort).
/// - `None` — unobservable (no integer-literal `list`, runtime error).
///
/// (The *per-pass* sortedness invariant — the prefix sorted after pass k for
/// selection, the growing-prefix for insertion, the suffix for bubble — is what
/// distinguishes *which* sort; that is the follow-on. This confirms *that* it
/// sorts.)
pub fn confirm_sorts(source: &str, list: &str) -> Option<bool> {
    let before = initial_int_list(source, list)?;
    let after = parse_int_list(&run_and_read(source, list)?)?;
    let mut asc = before;
    asc.sort_unstable();
    let desc: Vec<i64> = asc.iter().rev().copied().collect();
    Some(after == asc || after == desc)
}

/// Confirm a **walker** by *differential execution*: a genuinely data-driven
/// position ends somewhere that *depends on the data*, so running the program on
/// two different inputs makes it stop in different places. Runs `source` and a
/// variant with `list` reversed, comparing `walker`'s final value.
///
/// - `Some(true)`  — the walker landed in different places on different data: its
///   path is data-driven (a real walker).
/// - `Some(false)` — it landed in the same place regardless of the data: a fixed
///   traversal, i.e. a stepper, not a walker.
/// - `None` — can't be observed (no integer-literal `list`, too short or a
///   palindrome, a runtime error, or the literal couldn't be substituted).
///
/// The third `Vm` observation mode after [`confirm_organizer_multiset`]
/// (before/after invariant) and [`confirm_container_shrinks`] (single-run trace).
pub fn confirm_walker_is_data_driven(source: &str, walker: &str, list: &str) -> Option<bool> {
    let base = initial_int_list(source, list)?;
    if base.len() < 2 {
        return None;
    }
    let alt: Vec<i64> = base.iter().rev().copied().collect();
    if alt == base {
        return None; // palindrome — can't distinguish
    }
    let src2 = replace_list_literal(source, list, &alt)?;
    let end1 = run_and_read(source, walker)?;
    let end2 = run_and_read(&src2, walker)?;
    Some(end1 != end2)
}

/// Run `source` to completion and read `var`'s final repr (or None on error).
fn run_and_read(source: &str, var: &str) -> Option<String> {
    let mut st = crate::debug::Stepper::new(source).ok()?;
    st.run(&[]);
    if !matches!(st.status(), crate::debug::Status::Finished) {
        return None;
    }
    st.eval_watch(var).ok()
}

/// Replace the literal list on `list`'s assignment line with `values`. Returns
/// None if the `list = [...]` assignment isn't found on a single line.
fn replace_list_literal(source: &str, list: &str, values: &[i64]) -> Option<String> {
    let newlit = format!(
        "[{}]",
        values
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut out = Vec::new();
    let mut done = false;
    for line in source.lines() {
        let t = line.trim_start();
        let is_target = !done
            && (t.starts_with(&format!("{list} =")) || t.starts_with(&format!("{list}=")))
            && line.contains('[')
            && line.contains(']');
        if is_target {
            let start = line.find('[')?;
            let end = line.rfind(']')?;
            out.push(format!("{}{}{}", &line[..start], newlit, &line[end + 1..]));
            done = true;
        } else {
            out.push(line.to_string());
        }
    }
    done.then(|| out.join("\n") + "\n")
}

/// The integer elements of `list`'s initializer literal (`list = [1, 2, 3]`) at
/// the top level, or None if it isn't a literal list of integers.
fn initial_int_list(source: &str, list: &str) -> Option<Vec<i64>> {
    let tokens = crate::lexer::lex(source).ok()?;
    let (stmts, _) = crate::parser::parse_recovering(&tokens);
    for s in &stmts {
        if let StmtKind::Assign(name, value) = &s.kind {
            if name == list {
                if let ExprKind::List(items) = &value.kind {
                    return items.iter().map(int_of).collect();
                }
            }
        }
    }
    None
}

fn int_of(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Unary(UnOp::Neg, inner) => match &inner.kind {
            ExprKind::Int(n) => Some(-*n),
            _ => None,
        },
        _ => None,
    }
}

/// Parse a `Vm` list repr like `"[1, 2, -3]"` into its integer elements.
fn parse_int_list(repr: &str) -> Option<Vec<i64>> {
    let inner = repr.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|p| p.trim().parse::<i64>().ok()).collect()
}

/// The recognized scalar variable roles in an already-parsed program.
pub fn roles_of(stmts: &[Stmt]) -> Vec<VarRole> {
    let mut a = Analysis::default();
    a.collect_drivers(stmts);
    a.taint(stmts);
    a.walk(stmts, false, false);
    a.classify_followers(stmts);
    a.classify_fixed_values(stmts);
    a.classify_structures(stmts);
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
    lits: HashSet<LitVal>,
    lit_gated: bool,
    /// Set by the loop-level follower/temporary pass (`reuse.rs` read/write
    /// order): an unconditional element copy read-before-write vs write-before-read.
    follower: bool,
    temporary: bool,
    /// A computed derived value (`y = 2*x`), set in the main walk.
    transform: bool,
    /// A constant parameter, set by the whole-program fixed-value pass.
    fixed: bool,
    /// Data-structure roles, set by the structure pass.
    organizer: bool,
    walker: bool,
    container: bool,
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
                // A computed derived value of the data (not self-ref, not a bare
                // copy, not a literal) — a transformation.
                let is_transform = selfref_data.is_none()
                    && !is_maxmin
                    && !is_lit
                    && is_computed_from_data(value, &self.drivers, &self.data_vars);
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
                } else if is_transform {
                    e.transform = true;
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

    // ---- follower vs temporary: read/write order over each loop body ----
    // Both are an unconditional element copy `v = <elem>`; they differ only by
    // whether `v` is read *before* it is written within the iteration (follower,
    // carries the previous value) or written first (temporary, scratch). Order
    // is computed from `reuse.rs`'s tested `vars_read` / `vars_assigned`.
    fn classify_followers(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::For { body, .. }
                | StmtKind::ForEach { body, .. }
                | StmtKind::While { body, .. } => {
                    self.followers_in_loop(body);
                    self.classify_followers(body);
                }
                StmtKind::If {
                    body,
                    elifs,
                    else_body,
                    ..
                } => {
                    self.classify_followers(body);
                    for (_, b) in elifs {
                        self.classify_followers(b);
                    }
                    if let Some(b) = else_body {
                        self.classify_followers(b);
                    }
                }
                StmtKind::Def { body, .. } => self.classify_followers(body),
                StmtKind::ClassDef { methods, .. } => {
                    for m in methods {
                        self.classify_followers(&m.body);
                    }
                }
                _ => {}
            }
        }
    }

    fn followers_in_loop(&mut self, body: &[Stmt]) {
        // Top-level (unconditional) bare copies of the current element.
        let mut candidates: Vec<(String, usize)> = Vec::new();
        for s in body {
            if let StmtKind::Assign(name, value) = &s.kind {
                if !self.drivers.contains(name)
                    && selfref_operand(name, value).is_none()
                    && !maxmin_selfref(name, value)
                    && !is_literal(value)
                    && is_copy_of_data(value, &self.drivers, &self.data_vars)
                {
                    candidates.push((name.clone(), s.line));
                }
            }
        }
        for (v, line) in candidates {
            if let Some(read_first) = first_mention_is_read(body, &v) {
                let e = self.updates.entry(v).or_default();
                if read_first {
                    e.follower = true;
                } else {
                    e.temporary = true;
                }
                if e.line == 0 {
                    e.line = line;
                }
            }
        }
    }

    // ---- fixed value: a literal set before the loop, read inside it, never
    // reassigned in any loop (a constant parameter used in the body's logic). ----
    fn classify_fixed_values(&mut self, stmts: &[Stmt]) {
        let mut lit_outside: HashMap<String, usize> = HashMap::new();
        let mut assigned_in_loop: HashSet<String> = HashSet::new();
        let mut read_in_loop: HashSet<String> = HashSet::new();
        scan_fixed(
            stmts,
            false,
            &mut lit_outside,
            &mut assigned_in_loop,
            &mut read_in_loop,
        );
        for (name, line) in lit_outside {
            if !assigned_in_loop.contains(&name)
                && read_in_loop.contains(&name)
                && !self.drivers.contains(&name)
            {
                let e = self.updates.entry(name).or_default();
                e.fixed = true;
                if e.line == 0 {
                    e.line = line;
                }
            }
        }
    }

    // ---- data-structure roles: organizer, container, walker ----
    fn classify_structures(&mut self, stmts: &[Stmt]) {
        let orgs = organized_lists(stmts);
        let conts = container_lists(stmts);
        let mut walkers = HashMap::new();
        find_walkers(stmts, false, &mut walkers);
        for (name, line) in &conts {
            let e = self.updates.entry(name.clone()).or_default();
            e.container = true;
            if e.line == 0 {
                e.line = *line;
            }
        }
        for (name, line) in orgs {
            if !conts.contains_key(&name) {
                let e = self.updates.entry(name).or_default();
                e.organizer = true;
                if e.line == 0 {
                    e.line = line;
                }
            }
        }
        for (name, line) in walkers {
            let e = self.updates.entry(name).or_default();
            e.walker = true;
            if e.line == 0 {
                e.line = line;
            }
        }
    }

    fn finish(self) -> Vec<VarRole> {
        let Analysis {
            drivers, updates, ..
        } = self;
        let mut out: Vec<VarRole> = updates
            .into_iter()
            .filter_map(|(name, info)| {
                // A walker overrides the driver exclusion: a data-driven `while`
                // index (e.g. `while j < n and a[j] < x: j += 1`) is a driver
                // syntactically but plays the walker role.
                let role = if info.walker {
                    Role::Walker
                } else if drivers.contains(&name) {
                    return None;
                } else if info.seen_update {
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
                } else if info.follower {
                    Role::Follower
                } else if info.temporary {
                    Role::Temporary
                } else if info.transform {
                    Role::Transformation
                } else if info.fixed {
                    Role::FixedValue
                } else if info.organizer {
                    Role::Organizer
                } else if info.container {
                    Role::Container
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

/// Within one iteration of `body`, is `v`'s first mention a *read* (→ follower,
/// it uses the value carried from the previous iteration) or a *write* (→
/// temporary, it is created fresh)? `None` if `v` is never mentioned. Reads and
/// writes come from `reuse.rs`'s tested primitives.
fn first_mention_is_read(body: &[Stmt], v: &str) -> Option<bool> {
    for s in body {
        let reads = reads_name(s, v);
        let writes = writes_name(s, v);
        if reads && !writes {
            return Some(true);
        }
        if writes {
            return Some(false);
        }
    }
    None
}

/// Does statement `s` (recursively) read `v`?
fn reads_name(s: &Stmt, v: &str) -> bool {
    let mut out = BTreeSet::new();
    collect_reads(s, &mut out);
    out.contains(v)
}

/// Does statement `s` (recursively) assign `v`?
fn writes_name(s: &Stmt, v: &str) -> bool {
    let mut out = BTreeSet::new();
    collect_writes(s, &mut out);
    out.contains(v)
}

fn collect_reads(s: &Stmt, out: &mut BTreeSet<String>) {
    use crate::reuse::vars_read;
    match &s.kind {
        StmtKind::Expr(e) | StmtKind::Assign(_, e) => vars_read(e, out),
        StmtKind::AnnAssign { ann, value, .. } => {
            vars_read(ann, out);
            vars_read(value, out);
        }
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            vars_read(target, out);
            vars_read(index, out);
            vars_read(value, out);
        }
        StmtKind::SetAttr { obj, value, .. } => {
            vars_read(obj, out);
            vars_read(value, out);
        }
        StmtKind::UnpackAssign { targets, value } => {
            for t in targets {
                if !matches!(t.kind, ExprKind::Name(_)) {
                    vars_read(t, out);
                }
            }
            vars_read(value, out);
        }
        StmtKind::Return(Some(e)) => vars_read(e, out),
        StmtKind::If {
            cond,
            body,
            elifs,
            else_body,
        } => {
            vars_read(cond, out);
            for st in body {
                collect_reads(st, out);
            }
            for (c, b) in elifs {
                vars_read(c, out);
                for st in b {
                    collect_reads(st, out);
                }
            }
            if let Some(b) = else_body {
                for st in b {
                    collect_reads(st, out);
                }
            }
        }
        StmtKind::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            vars_read(start, out);
            vars_read(end, out);
            vars_read(step, out);
            for st in body {
                collect_reads(st, out);
            }
        }
        StmtKind::ForEach { iterable, body, .. } => {
            vars_read(iterable, out);
            for st in body {
                collect_reads(st, out);
            }
        }
        StmtKind::While { cond, body } => {
            vars_read(cond, out);
            for st in body {
                collect_reads(st, out);
            }
        }
        StmtKind::Def { body, .. } => {
            for st in body {
                collect_reads(st, out);
            }
        }
        _ => {}
    }
}

fn collect_writes(s: &Stmt, out: &mut BTreeSet<String>) {
    crate::reuse::vars_assigned(s, out);
    match &s.kind {
        StmtKind::If {
            body,
            elifs,
            else_body,
            ..
        } => {
            for st in body {
                collect_writes(st, out);
            }
            for (_, b) in elifs {
                for st in b {
                    collect_writes(st, out);
                }
            }
            if let Some(b) = else_body {
                for st in b {
                    collect_writes(st, out);
                }
            }
        }
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::While { body, .. }
        | StmtKind::Def { body, .. } => {
            for st in body {
                collect_writes(st, out);
            }
        }
        _ => {}
    }
}

// ---- data-structure role detection ----

/// Visit every statement (recursing into control-flow bodies).
fn for_each_stmt(stmts: &[Stmt], f: &mut dyn FnMut(&Stmt)) {
    for s in stmts {
        f(s);
        match &s.kind {
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                for_each_stmt(body, f);
                for (_, b) in elifs {
                    for_each_stmt(b, f);
                }
                if let Some(b) = else_body {
                    for_each_stmt(b, f);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. }
            | StmtKind::Def { body, .. } => for_each_stmt(body, f),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    for_each_stmt(&m.body, f);
                }
            }
            _ => {}
        }
    }
}

/// Visit every expression in the program (through statements and sub-exprs).
fn for_each_expr(stmts: &[Stmt], f: &mut dyn FnMut(&Expr)) {
    for_each_stmt(stmts, &mut |s| match &s.kind {
        StmtKind::Expr(e) | StmtKind::Assign(_, e) | StmtKind::Return(Some(e)) => {
            for_each_subexpr(e, f)
        }
        StmtKind::AnnAssign { ann, value, .. } => {
            for_each_subexpr(ann, f);
            for_each_subexpr(value, f);
        }
        StmtKind::If { cond, .. } | StmtKind::While { cond, .. } => for_each_subexpr(cond, f),
        StmtKind::For {
            start, end, step, ..
        } => {
            for_each_subexpr(start, f);
            for_each_subexpr(end, f);
            for_each_subexpr(step, f);
        }
        StmtKind::ForEach { iterable, .. } => for_each_subexpr(iterable, f),
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            for_each_subexpr(target, f);
            for_each_subexpr(index, f);
            for_each_subexpr(value, f);
        }
        StmtKind::SetAttr { obj, value, .. } => {
            for_each_subexpr(obj, f);
            for_each_subexpr(value, f);
        }
        StmtKind::UnpackAssign { targets, value } => {
            for t in targets {
                for_each_subexpr(t, f);
            }
            for_each_subexpr(value, f);
        }
        _ => {}
    });
}

/// Is `e` exactly `list[..]` — a bare subscript read of `list` (a moved
/// element), not a computed expression that merely mentions it?
fn is_bare_index_into(e: &Expr, list: &str) -> bool {
    if let ExprKind::Index(base, _) = &e.kind {
        matches!(&base.kind, ExprKind::Name(n) if n == list)
    } else {
        false
    }
}

/// Lists rearranged in place: a position is written a *bare* element of the
/// same list (a move), via `a[i] = a[j]` or a tuple-swap `a[i], a[j] = a[j], a[i]`.
/// A computed write like `a[i] = a[i] * 2` is a map, not a move — excluded.
fn organized_lists(stmts: &[Stmt]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for_each_stmt(stmts, &mut |s| match &s.kind {
        StmtKind::SetIndex { target, value, .. } => {
            if let ExprKind::Name(a) = &target.kind {
                if is_bare_index_into(value, a) {
                    out.entry(a.clone()).or_insert(s.line);
                }
            }
        }
        StmtKind::UnpackAssign { targets, value } => {
            let lists: Vec<String> = targets
                .iter()
                .filter_map(|t| match &t.kind {
                    ExprKind::Index(base, _) => match &base.kind {
                        ExprKind::Name(a) => Some(a.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            let elems: Vec<&Expr> = match &value.kind {
                ExprKind::Tuple(xs) => xs.iter().collect(),
                _ => vec![value],
            };
            for a in lists {
                if elems.iter().any(|e| is_bare_index_into(e, &a)) {
                    out.entry(a).or_insert(s.line);
                }
            }
        }
        _ => {}
    });
    out
}

/// Collections whose membership changes both ways — a receiver with *both* an
/// add (`append`/`insert`/`push`) and a remove (`pop`/`remove`) method call.
fn container_lists(stmts: &[Stmt]) -> HashMap<String, usize> {
    let mut added: HashMap<String, usize> = HashMap::new();
    let mut removed: HashSet<String> = HashSet::new();
    for_each_expr(stmts, &mut |e| {
        if let ExprKind::MethodCall(recv, method, _) = &e.kind {
            if let ExprKind::Name(v) = &recv.kind {
                match method.as_str() {
                    "append" | "insert" | "push" | "add" => {
                        added.entry(v.clone()).or_insert(e.line);
                    }
                    "pop" | "popleft" | "remove" => {
                        removed.insert(v.clone());
                    }
                    _ => {}
                }
            }
        }
    });
    added
        .into_iter()
        .filter(|(v, _)| removed.contains(v))
        .collect()
}

/// Does `e` reference a subscript / slice anywhere (a data-navigation signal)?
fn expr_has_index(e: &Expr) -> bool {
    let mut found = false;
    for_each_subexpr(e, &mut |x| {
        if matches!(x.kind, ExprKind::Index(..) | ExprKind::Slice { .. }) {
            found = true;
        }
    });
    found
}

/// Is `value` a constant advance of a position — `x <+|-> <int>` (`j + 1`,
/// `mid - 1`)?
fn is_index_advance(value: &Expr) -> bool {
    if let ExprKind::Bin(op, a, b) = &value.kind {
        if matches!(op, BinOp::Add | BinOp::Sub) {
            return matches!(a.kind, ExprKind::Int(_)) || matches!(b.kind, ExprKind::Int(_));
        }
    }
    false
}

/// Walkers: a position advanced by a constant *inside a data-driven context* — a
/// `while`/`if` whose condition inspects the data (a subscript). A plain
/// `while i < n: i += 1` (no data in the condition) is a stepper, not a walker.
fn find_walkers(stmts: &[Stmt], data_ctx: bool, out: &mut HashMap<String, usize>) {
    for s in stmts {
        if data_ctx {
            if let StmtKind::Assign(name, value) = &s.kind {
                if is_index_advance(value) {
                    out.entry(name.clone()).or_insert(s.line);
                }
            }
        }
        match &s.kind {
            StmtKind::While { cond, body } => {
                find_walkers(body, data_ctx || expr_has_index(cond), out);
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                let dc = data_ctx || expr_has_index(cond);
                find_walkers(body, dc, out);
                for (c, b) in elifs {
                    find_walkers(b, data_ctx || expr_has_index(c), out);
                }
                if let Some(b) = else_body {
                    find_walkers(b, dc, out);
                }
            }
            StmtKind::For { body, .. } | StmtKind::ForEach { body, .. } => {
                find_walkers(body, data_ctx, out);
            }
            StmtKind::Def { body, .. } => find_walkers(body, false, out),
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    find_walkers(&m.body, false, out);
                }
            }
            _ => {}
        }
    }
}

/// Is `value` a computed expression (`Bin` or a `Call`) that references the
/// data — a transformation, as opposed to a self-ref update, a bare copy, or a
/// literal (which the caller has already ruled out)?
fn is_computed_from_data(value: &Expr, drivers: &HashSet<String>, data: &HashSet<String>) -> bool {
    matches!(value.kind, ExprKind::Bin(..) | ExprKind::Call(..)) && refs_data(value, drivers, data)
}

/// A scalar literal (int / float / bool / string) — the initializer shape of a
/// fixed-value parameter. Lists/dicts/tuples are excluded (the data, not a
/// constant parameter).
fn is_scalar_literal(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Str(_)
    )
}

/// Walk for the fixed-value pass: scalar-literal assignments made *outside* any
/// loop, plus everything assigned or read *inside* a loop body.
fn scan_fixed(
    stmts: &[Stmt],
    in_loop: bool,
    lit_outside: &mut HashMap<String, usize>,
    assigned_in_loop: &mut HashSet<String>,
    read_in_loop: &mut HashSet<String>,
) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, value) | StmtKind::AnnAssign { name, value, .. } => {
                if in_loop {
                    assigned_in_loop.insert(name.clone());
                } else if is_scalar_literal(value) {
                    lit_outside.insert(name.clone(), s.line);
                }
            }
            _ => {}
        }
        if in_loop {
            // This statement's own reads (its header/rhs, not nested bodies —
            // those recurse). A loop header read at `in_loop == false` is not
            // counted, so a bound like `range(n)` doesn't mark `n` as body-read.
            let mut r = BTreeSet::new();
            stmt_own_reads(s, &mut r);
            read_in_loop.extend(r);
        }
        match &s.kind {
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                scan_fixed(body, in_loop, lit_outside, assigned_in_loop, read_in_loop);
                for (_, b) in elifs {
                    scan_fixed(b, in_loop, lit_outside, assigned_in_loop, read_in_loop);
                }
                if let Some(b) = else_body {
                    scan_fixed(b, in_loop, lit_outside, assigned_in_loop, read_in_loop);
                }
            }
            StmtKind::For { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::While { body, .. } => {
                scan_fixed(body, true, lit_outside, assigned_in_loop, read_in_loop);
            }
            StmtKind::Def { body, .. } => {
                scan_fixed(body, false, lit_outside, assigned_in_loop, read_in_loop);
            }
            StmtKind::ClassDef { methods, .. } => {
                for m in methods {
                    scan_fixed(&m.body, false, lit_outside, assigned_in_loop, read_in_loop);
                }
            }
            _ => {}
        }
    }
}

/// Names read by a statement's *own* expressions (condition / rhs / index),
/// not descending into nested control-flow bodies.
fn stmt_own_reads(s: &Stmt, out: &mut BTreeSet<String>) {
    use crate::reuse::vars_read;
    match &s.kind {
        StmtKind::Expr(e) | StmtKind::Assign(_, e) => vars_read(e, out),
        StmtKind::AnnAssign { ann, value, .. } => {
            vars_read(ann, out);
            vars_read(value, out);
        }
        StmtKind::If { cond, .. } | StmtKind::While { cond, .. } => vars_read(cond, out),
        StmtKind::For {
            start, end, step, ..
        } => {
            vars_read(start, out);
            vars_read(end, out);
            vars_read(step, out);
        }
        StmtKind::ForEach { iterable, .. } => vars_read(iterable, out),
        StmtKind::SetIndex {
            target,
            index,
            value,
        } => {
            vars_read(target, out);
            vars_read(index, out);
            vars_read(value, out);
        }
        StmtKind::SetAttr { obj, value, .. } => {
            vars_read(obj, out);
            vars_read(value, out);
        }
        StmtKind::UnpackAssign { targets, value } => {
            for t in targets {
                if !matches!(t.kind, ExprKind::Name(_)) {
                    vars_read(t, out);
                }
            }
            vars_read(value, out);
        }
        StmtKind::Return(Some(e)) => vars_read(e, out),
        _ => {}
    }
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
    fn follower_is_read_before_written() {
        // `prev` is read (in the print) before it is reassigned each iteration —
        // it carries the previous element forward.
        let src = "a = [3, 1, 4]\nprev = a[0]\nfor x in a:\n    print(x - prev)\n    prev = x\n";
        assert_eq!(role_of(src, "prev"), Some(Role::Follower));
    }

    #[test]
    fn temporary_is_written_before_read() {
        // The swap helper `t` is written first, then read — scratch, not carried.
        let src = "a = [3, 1, 2]\nfor i in range(0, 2):\n    t = a[i]\n    a[i] = a[i + 1]\n    a[i + 1] = t\n";
        assert_eq!(role_of(src, "t"), Some(Role::Temporary));
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
    fn transformation_is_a_computed_derived_value() {
        // `y = 2*x + 7` — computed from the element, no self-reference. A bare
        // copy (`prev = x`) is not a transformation; a running sum is not either.
        let src = "a = [1, 2, 3]\nfor x in a:\n    y = 2 * x + 7\n    print(y)\n";
        assert_eq!(role_of(src, "y"), Some(Role::Transformation));
        // temp-unit conversion, driver `f`.
        let conv = "a = [32, 212]\nfor f in a:\n    c = (f - 32) * 5 // 9\n    print(c)\n";
        assert_eq!(role_of(conv, "c"), Some(Role::Transformation));
    }

    #[test]
    fn fixed_value_is_a_constant_parameter() {
        // `limit` is set to a literal before the loop and only read inside it.
        let src = "a = [1, 200, 3]\nlimit = 100\nc = 0\nfor x in a:\n    if x < limit:\n        c = c + 1\nprint(c)\n";
        assert_eq!(role_of(src, "limit"), Some(Role::FixedValue));
        // `c` is a counter, not fixed; the data list `a` is not a fixed value.
        assert_eq!(role_of(src, "c"), Some(Role::Counter));
        assert_eq!(role_of(src, "a"), None);
    }

    #[test]
    fn organizer_moves_elements_but_a_map_does_not() {
        // Bubble-style swap moves existing elements in place -> organizer.
        let swap = "a = [3, 1, 2]\nfor i in range(0, 2):\n    if a[i] > a[i + 1]:\n        a[i], a[i + 1] = a[i + 1], a[i]\n";
        assert_eq!(role_of(swap, "a"), Some(Role::Organizer));
        // A shift (`a[i] = a[i+1]`) also moves an element.
        let shift = "a = [1, 2, 3]\nfor i in range(0, 2):\n    a[i] = a[i + 1]\n";
        assert_eq!(role_of(shift, "a"), Some(Role::Organizer));
        // Writing a COMPUTED value is a map, not a move -> NOT an organizer.
        let map = "a = [1, 2, 3]\nfor i in range(0, 3):\n    a[i] = a[i] * 2\n";
        assert_eq!(role_of(map, "a"), None);
    }

    #[test]
    fn container_needs_both_add_and_remove() {
        // Push on positive, pop otherwise -> membership changes both ways.
        let stack = "a = [1, -2, 3]\nst = []\nfor x in a:\n    if x > 0:\n        st.append(x)\n    else:\n        st.pop()\n";
        assert_eq!(role_of(stack, "st"), Some(Role::Container));
        // Append-only is an accumulate collection, NOT a container.
        let collect = "a = [1, 2, 3]\nout = []\nfor x in a:\n    out.append(x)\n";
        assert_eq!(role_of(collect, "out"), None);
    }

    #[test]
    fn walker_is_a_data_driven_index() {
        // Scan-until: the while condition inspects the data (`a[j] < x`), so `j`
        // is a walker, not a plain stepper.
        let scan = "a = [1, 3, 5, 8]\nn = 4\nx = 6\nj = 0\nwhile j < n and a[j] < x:\n    j = j + 1\nprint(j)\n";
        assert_eq!(role_of(scan, "j"), Some(Role::Walker));
        // A plain fixed-range while index is NOT a walker (no data in the test).
        let plain = "a = [1, 2, 3]\nn = 3\ni = 0\ns = 0\nwhile i < n:\n    s = s + a[i]\n    i = i + 1\n";
        assert_ne!(role_of(plain, "i"), Some(Role::Walker));
        assert_eq!(role_of(plain, "s"), Some(Role::Gatherer));
    }

    #[test]
    fn vm_confirms_the_organizer_multiset_invariant() {
        // A real swap preserves the multiset -> confirmed by observation.
        let swap = "a = [3, 1, 2]\nfor i in range(0, 2):\n    if a[i] > a[i + 1]:\n        a[i], a[i + 1] = a[i + 1], a[i]\n";
        assert_eq!(role_of(swap, "a"), Some(Role::Organizer)); // syntactic
        assert_eq!(confirm_organizer_multiset(swap, "a"), Some(true)); // observed

        // A rotate with wrap-around also preserves the multiset.
        let rot = "a = [1, 2, 3]\nfirst = a[0]\nfor i in range(0, 2):\n    a[i] = a[i + 1]\na[2] = first\n";
        assert_eq!(confirm_organizer_multiset(rot, "a"), Some(true));

        // THE POINT: a bare shift `a[i]=a[i+1]` with NO wrap-around LOOKS like an
        // organizer syntactically, but it clobbers a value -> the Vm's observed
        // multiset invariant catches it. Observation > syntax.
        let lossy = "a = [3, 1, 2]\nfor i in range(0, 2):\n    a[i] = a[i + 1]\n";
        assert_eq!(role_of(lossy, "a"), Some(Role::Organizer)); // syntax says yes
        assert_eq!(confirm_organizer_multiset(lossy, "a"), Some(false)); // Vm says no
    }

    #[test]
    fn vm_confirms_the_container_actually_shrinks() {
        // A genuine stack: push positives, pop otherwise -> length goes up AND
        // down at runtime.
        let stack = "a = [5, 3, -1]\nst = []\nfor x in a:\n    if x > 0:\n        st.append(x)\n    else:\n        st.pop()\n";
        assert_eq!(role_of(stack, "st"), Some(Role::Container)); // syntactic
        assert_eq!(confirm_container_shrinks(stack, "st"), Some(true)); // observed

        // THE POINT: a `pop` in a branch the data never triggers is textually
        // present (so syntax says container) but never fires -- the collection
        // only grows. The Vm's trace catches it. Observation > syntax.
        let dead = "a = [1, 2, 3]\nst = []\nfor x in a:\n    st.append(x)\n    if x > 100:\n        st.pop()\n";
        assert_eq!(role_of(dead, "st"), Some(Role::Container)); // syntax says yes
        assert_eq!(confirm_container_shrinks(dead, "st"), Some(false)); // Vm says no
    }

    #[test]
    fn verified_roles_drop_tags_the_vm_disproves() {
        // A real swap: the organizer tag survives verification.
        let swap = "a = [3, 1, 2]\nfor i in range(0, 2):\n    if a[i] > a[i + 1]:\n        a[i], a[i + 1] = a[i + 1], a[i]\n";
        assert_eq!(role_of(swap, "a"), Some(Role::Organizer));
        assert!(
            variable_roles_verified(swap)
                .iter()
                .any(|v| v.name == "a" && v.role == Role::Organizer),
            "confirmed organizer is kept"
        );

        // A lossy shift: syntactic organizer, but the Vm disproves the multiset
        // invariant, so the verified output DROPS it.
        let lossy = "a = [3, 1, 2]\nfor i in range(0, 2):\n    a[i] = a[i + 1]\n";
        assert_eq!(role_of(lossy, "a"), Some(Role::Organizer));
        assert!(
            variable_roles_verified(lossy).iter().all(|v| v.name != "a"),
            "disproved organizer is dropped"
        );

        // A dead-branch pop: syntactic container, dropped after verification.
        let dead = "a = [1, 2, 3]\nst = []\nfor x in a:\n    st.append(x)\n    if x > 100:\n        st.pop()\n";
        assert_eq!(role_of(dead, "st"), Some(Role::Container));
        assert!(
            variable_roles_verified(dead).iter().all(|v| v.name != "st"),
            "disproved container is dropped"
        );

        // A genuine data-driven walker survives verification...
        let walk = "a = [1, 3, 5, 8]\nn = 4\nx = 6\nj = 0\nwhile j < n and a[j] < x:\n    j = j + 1\nprint(j)\n";
        assert!(
            variable_roles_verified(walk)
                .iter()
                .any(|v| v.name == "j" && v.role == Role::Walker),
            "confirmed walker is kept"
        );
        // ...but a data-predicate that never actually stops it early makes `j`
        // run to `n` regardless of the data -> a fixed traversal the Vm exposes.
        let fake = "a = [1, 3, 5, 8]\nn = 4\nj = 0\nwhile j < n and a[j] < 1000000:\n    j = j + 1\nprint(j)\n";
        assert_eq!(role_of(fake, "j"), Some(Role::Walker)); // syntax says walker
        assert!(
            variable_roles_verified(fake).iter().all(|v| v.name != "j"),
            "data-independent 'walker' is dropped"
        );
    }

    #[test]
    fn vm_confirms_the_walker_path_is_data_driven() {
        // Scan-until: where `j` stops depends on the contents, so on reversed
        // data it stops in a different place -> confirmed data-driven.
        let scan = "a = [1, 3, 5, 8]\nn = 4\nx = 6\nj = 0\nwhile j < n and a[j] < x:\n    j = j + 1\nprint(j)\n";
        assert_eq!(role_of(scan, "j"), Some(Role::Walker)); // syntactic
        assert_eq!(confirm_walker_is_data_driven(scan, "j", "a"), Some(true)); // observed

        // A plain fixed-range index ends at `n` no matter the data -> differential
        // execution shows it is NOT data-driven (it is a stepper, not a walker).
        let step = "a = [1, 2, 3, 4]\nn = 4\ni = 0\ns = 0\nwhile i < n:\n    s = s + a[i]\n    i = i + 1\nprint(i)\n";
        assert_eq!(confirm_walker_is_data_driven(step, "i", "a"), Some(false));
    }

    #[test]
    fn vm_tells_a_sort_from_a_mere_rearrangement() {
        // Both are organizers (move elements, preserve the multiset)...
        let bubble = "a = [4, 2, 6, 1, 5, 3]\nn = 6\nfor i in range(0, n):\n    for j in range(0, n - 1 - i):\n        if a[j] > a[j + 1]:\n            a[j], a[j + 1] = a[j + 1], a[j]\n";
        let rev = "a = [3, 1, 2]\nn = 3\nfor i in range(0, 1):\n    a[i], a[n - 1 - i] = a[n - 1 - i], a[i]\n";
        assert_eq!(role_of(bubble, "a"), Some(Role::Organizer));
        assert_eq!(role_of(rev, "a"), Some(Role::Organizer));
        // ...but only the bubble sort actually orders the data.
        assert_eq!(confirm_sorts(bubble, "a"), Some(true));
        assert_eq!(confirm_sorts(rev, "a"), Some(false));
    }

    #[test]
    fn vm_tells_which_sort_by_the_per_pass_invariant() {
        let data = "a = [5, 3, 8, 1, 9, 2, 7, 4]\nn = 8\n";
        let sel = format!("{data}for i in range(0, n):\n    m = i\n    for j in range(i + 1, n):\n        if a[j] < a[m]:\n            m = j\n    a[i], a[m] = a[m], a[i]\n");
        let ins = format!("{data}for i in range(1, n):\n    key = a[i]\n    j = i - 1\n    while j >= 0 and a[j] > key:\n        a[j + 1] = a[j]\n        j = j - 1\n    a[j + 1] = key\n");
        let bub = format!("{data}for i in range(0, n):\n    for j in range(0, n - 1 - i):\n        if a[j] > a[j + 1]:\n            a[j], a[j + 1] = a[j + 1], a[j]\n");
        let sel = sel.as_str();
        let ins = ins.as_str();
        let bub = bub.as_str();
        assert_eq!(classify_sort_by_passes(sel, "a", "i"), Some("selection"));
        assert_eq!(classify_sort_by_passes(ins, "a", "i"), Some("insertion"));
        assert_eq!(classify_sort_by_passes(bub, "a", "i"), Some("bubble"));
    }

    #[test]
    fn unparseable_source_is_empty_not_a_panic() {
        assert!(variable_roles("def (:\n").is_empty());
    }
}
