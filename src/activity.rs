//! UML 2.5.1 activity diagrams as a fourth projection over the shared AST —
//! a read-only view, not a new execution surface. Built directly against
//! the real OMG metamodel (`DecisionNode`/`MergeNode` kept distinct,
//! `StructuredActivityNode` with a `«loop»`/`«conditional»` stereotype for
//! loops/conditionals, no synthetic edge "kinds" UML doesn't have) rather
//! than an ad-hoc shape reshaped toward UML later — one retrofit instead
//! of two.
//!
//! **Licensing, if this is ever rendered anywhere**: cite OMG UML 2.5.1,
//! carry the UML(R) trademark line, never copy spec text or figures.
//!
//! No synthetic edge kinds for loop-back/break/continue/return: an edge is
//! just an edge, and the special meaning (it's a loop back-edge, a break)
//! is purely a consequence of which nodes it connects, not a label this
//! pass needs to invent — a layout pass can infer the special routing from
//! topology instead.
//!
//! **Narrow slice, deliberately**: `Assign`/`AnnAssign`/`SetIndex`/
//! `SetAttr`/`UnpackAssign`/bare `Expr` as plain `Action`s, `If`/`While`/
//! `ForEach`/`For` (counted) as `Structured` nodes, `Break`/`Continue`/
//! `Return` as a routing decision on an enclosing block's tail rather
//! than a node of their own (see `ControlFlow`'s `Target` and
//! `Builder::block`'s doc comments), `Pass` a true no-op needing no
//! special case at all. `build_program` walks top-level `Def`/
//! `ClassDef` into their own `Activity` each (one per function/method,
//! same shape as the module-level one), so this can be verified end to
//! end on a small slice before expanding to a nested-`Def` case.

use crate::ast::{Expr, Span, Stmt, StmtKind};

pub type NodeId = usize;

/// One activity. Eventually one per top-level `def`/`Method`; for now, one
/// for the whole program's module-level code, since `Def`/`Method` aren't
/// walked into yet. `Initial`/`ActivityFinal` mark function start/return —
/// they never appear inside a nested `Structured` node's own `body`.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub nodes: Vec<Node>,
    pub edges: Vec<ControlFlow>,
    pub initial: NodeId,
    pub finals: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub kind: ActivityNode,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActivityNode {
    Initial,
    ActivityFinal,
    /// label = `source[span.0..span.1]`.
    Action(Span),
    /// The guarded condition of an `If`/`While` — label = the source slice.
    Decision(Span),
    /// Where a `Decision`'s branches rejoin. Kept distinct from `Decision`
    /// — UML defines them as separate metaclasses (one-in/many-out vs.
    /// many-in/one-out), not one diamond concept reused at each end.
    Merge,
    Structured {
        stereotype: Stereotype,
        setup: Option<LoopSetup>,
        body: Activity,
    },
    CallBehavior {
        callee: String,
        call_span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stereotype {
    Loop,
    Conditional,
}

/// A `For` loop's init — not produced by this narrow slice yet.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopSetup {
    pub var: String,
    pub start: Span,
    pub end: Span,
    pub step: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlFlow {
    pub from: NodeId,
    pub to: Target,
    pub guard: Option<Guard>,
}

/// An edge's destination. Usually a plain local node — but `break`/
/// `continue` target the nearest enclosing loop's `exit`/`decision`,
/// which live in an ANCESTOR `Structured` node's own `Activity`: a
/// statement built deep inside a nested body has no local id for them
/// (each nested body is its own self-contained `NodeId` space, by
/// design — see `flatten_and_layout`'s doc comment). Recording the
/// *intent* here and resolving it to a concrete id during flattening
/// (which already walks the full nesting and knows every loop's global
/// ids by the time it reaches anything inside that loop's body) avoids
/// needing Pass 1 to reach outside the `Builder` that's currently
/// running — the same "resolve from topology, don't pre-digest" split
/// this module already uses for back-edges, just one layer deeper: the
/// *destination itself*, not merely its presentational meaning, isn't
/// knowable without more context than one `Builder` has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local(NodeId),
    /// `break` — the nearest enclosing loop's `exit` (`Merge`).
    LoopExit,
    /// `continue` — the nearest enclosing loop's own `Decision`.
    LoopBack,
    /// `return` — this `Activity`'s own `ActivityFinal` (there's exactly
    /// one per function, same as the module-level one `build()` already
    /// creates — every `return`, wherever nested, converges on it, the
    /// same way every loop's `break` converges on one `exit`). Resolved
    /// against a context established once per function `Activity` and
    /// inherited through every nested `Structured` body unchanged
    /// (never re-established at a loop/conditional boundary the way
    /// `LoopExit`/`LoopBack`'s context is) — a `return` two loops deep
    /// still means THIS function's own end, not something loop-scoped.
    Return,
}

/// The `[cond]` / `[else]` label UML puts on an edge leaving a `Decision`.
#[derive(Debug, Clone, PartialEq)]
pub enum Guard {
    Cond(Span),
    Else,
}

/// Build the module-level activity for a parsed program.
/// Build one self-contained `Activity` for a sequence of statements —
/// module-level code, or (via `build_program`) one function/method's own
/// body. Nothing here is module-specific: `return` diverges to
/// `Target::Return` exactly the same way `break`/`continue` divert to
/// `Target::LoopExit`/`LoopBack` (see `Builder::block`'s doc comment),
/// and `BlockOutcome::Diverts(Some(entry), _)` already connects `initial`
/// to the right entry regardless of what it diverts to — `block()` wired
/// the actual diverging edge itself, so this function never needs to
/// know or care whether a divert was a `return`.
pub fn build(stmts: &[Stmt]) -> Activity {
    let mut b = Builder::default();
    let initial = b.push(ActivityNode::Initial, 0);
    let fin = b.push(ActivityNode::ActivityFinal, 0);
    match b.block(stmts) {
        BlockOutcome::Flows(entry, tail) => {
            b.connect(initial, Target::Local(entry), None);
            b.connect(tail, Target::Local(fin), None);
        }
        BlockOutcome::Empty => b.connect(initial, Target::Local(fin), None),
        // `break`/`continue`/`return` at module level, outside any
        // loop/function — not valid Python, so not a case this view
        // needs to render nicely. Fall back to whatever node exists (if
        // any) and otherwise to the empty-program behavior, rather than
        // crashing on malformed input a real compile would already have
        // rejected elsewhere.
        BlockOutcome::Diverts(Some(entry), _) => b.connect(initial, Target::Local(entry), None),
        BlockOutcome::Diverts(None, _) => b.connect(initial, Target::Local(fin), None),
    }
    Activity {
        nodes: b.nodes,
        edges: b.edges,
        initial,
        finals: vec![fin],
    }
}

/// One named `Activity` — the module's own top-level code (`"<module>"`),
/// a top-level function (its name), or a class method (`"Class.method"`).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedActivity {
    pub name: String,
    pub activity: Activity,
}

/// Build every `Activity` a program has: one for module-level code
/// (exactly `build()`'s own behavior — top-level `Def`/`ClassDef`
/// statements already produce no node there, same as any other
/// still-deferred kind, so nothing needs filtering out), plus one more
/// per top-level `Def` and per `ClassDef` method, each built with the
/// SAME `build()` — a function/method body is just another self-
/// contained sequence of statements with its own `Initial`/
/// `ActivityFinal`. Nested `Def`s (a function defined inside another
/// function's body, a loop, or an `if`) are a deliberate follow-up, not
/// walked into yet — this is top-level only, matching `Activity`'s own
/// original doc comment ("one per top-level def/Method").
pub fn build_program(stmts: &[Stmt]) -> Vec<NamedActivity> {
    let mut out = vec![NamedActivity {
        name: "<module>".to_string(),
        activity: build(stmts),
    }];
    for s in stmts {
        match &s.kind {
            StmtKind::Def { name, body, .. } => out.push(NamedActivity {
                name: name.clone(),
                activity: build(body),
            }),
            StmtKind::ClassDef { name, methods, .. } => {
                for m in methods {
                    out.push(NamedActivity {
                        name: format!("{name}.{}", m.name),
                        activity: build(&m.body),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    edges: Vec<ControlFlow>,
}

/// What a sequence of statements did, control-flow-wise.
enum BlockOutcome {
    /// No node produced at all (every statement was `pass` or a deferred
    /// kind) — execution just falls through.
    Empty,
    /// `(entry, tail)` — the first node reached and the last; execution
    /// falls through past `tail` normally.
    Flows(NodeId, NodeId),
    /// The block ends by jumping to `target` (`break`/`continue`)
    /// instead of falling through. `Some(entry)` if some node existed
    /// before the jump (the caller wires its own incoming edge to it,
    /// same as `Flows`); `None` if the jump was the first thing in the
    /// block (the caller must wire its own incoming edge straight to
    /// `target` instead).
    Diverts(Option<NodeId>, Target),
}

impl Builder {
    fn push(&mut self, kind: ActivityNode, line: usize) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { id, kind, line });
        id
    }

    fn connect(&mut self, from: NodeId, to: Target, guard: Option<Guard>) {
        self.edges.push(ControlFlow { from, to, guard });
    }

    /// Build a sequence of statements, wiring each to the next.
    fn block(&mut self, stmts: &[Stmt]) -> BlockOutcome {
        let mut result: Option<(NodeId, NodeId)> = None;
        for s in stmts {
            // `break`/`continue` produce no node of their own — UML has
            // no shape for "jump," and a diamond/box here would be
            // decoration, not information. They're purely a routing
            // decision: whatever this block's tail would otherwise
            // connect to, it connects to the loop's exit/decision
            // instead. Everything after one is genuinely unreachable, so
            // the block stops here rather than building dead nodes.
            if matches!(s.kind, StmtKind::Break | StmtKind::Continue) {
                let target = if matches!(s.kind, StmtKind::Break) {
                    Target::LoopExit
                } else {
                    Target::LoopBack
                };
                return match result {
                    Some((first, prev_tail)) => {
                        self.connect(prev_tail, target, None);
                        BlockOutcome::Diverts(Some(first), target)
                    }
                    // Nothing before it in this block — there's no node
                    // to wire the divert from; the caller (e.g.
                    // `wire_branch`) must route its own incoming edge
                    // straight to `target` instead of to an entry node.
                    None => BlockOutcome::Diverts(None, target),
                };
            }
            // `return` is the same routing decision as `break`/`continue`
            // — but unlike them it can carry a value worth showing, so
            // `return x + 1` (unlike a bare `return`) gets its own
            // `Action` node labeled with that expression before
            // diverging, the same way an `Assign` gets one.
            if let StmtKind::Return(ret_expr) = &s.kind {
                let own_node = ret_expr
                    .as_ref()
                    .map(|e| self.push(ActivityNode::Action(e.span), s.line));
                return match (result, own_node) {
                    (Some((first, prev_tail)), Some(n)) => {
                        self.connect(prev_tail, Target::Local(n), None);
                        self.connect(n, Target::Return, None);
                        BlockOutcome::Diverts(Some(first), Target::Return)
                    }
                    (Some((first, prev_tail)), None) => {
                        self.connect(prev_tail, Target::Return, None);
                        BlockOutcome::Diverts(Some(first), Target::Return)
                    }
                    (None, Some(n)) => {
                        self.connect(n, Target::Return, None);
                        BlockOutcome::Diverts(Some(n), Target::Return)
                    }
                    (None, None) => BlockOutcome::Diverts(None, Target::Return),
                };
            }
            if let Some((entry, tail)) = self.stmt(s) {
                result = match result {
                    None => Some((entry, tail)),
                    Some((first, prev_tail)) => {
                        self.connect(prev_tail, Target::Local(entry), None);
                        Some((first, tail))
                    }
                };
            }
        }
        match result {
            Some((entry, tail)) => BlockOutcome::Flows(entry, tail),
            None => BlockOutcome::Empty,
        }
    }

    /// Build one statement. Returns `(entry, tail)` for the node(s) it
    /// produced, or `None` for a deferred kind.
    fn stmt(&mut self, s: &Stmt) -> Option<(NodeId, NodeId)> {
        match &s.kind {
            StmtKind::Assign(..)
            | StmtKind::AnnAssign { .. }
            | StmtKind::SetIndex { .. }
            | StmtKind::SetAttr { .. }
            | StmtKind::UnpackAssign { .. }
            | StmtKind::Expr(_) => {
                let n = self.push(ActivityNode::Action(s.span), s.line);
                Some((n, n))
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => Some(self.build_if(s.line, cond, body, elifs, else_body.as_deref())),
            StmtKind::While { cond, body } => Some(self.build_while(s.line, cond, body)),
            StmtKind::ForEach { iterable, body, .. } => {
                Some(self.build_foreach(s.line, iterable, body))
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => Some(self.build_for(s.line, var, start, end, step, body)),
            // `Break`/`Continue`/`Return` are handled one level up, by
            // `block()` itself (see its own doc comment) — they change
            // how the *block* wires its own tail, which `stmt()`'s
            // uniform `(entry, tail)` shape can't express. `block()`
            // intercepts them before ever calling `stmt()`, so this arm
            // is unreachable in practice; it exists only so the match
            // stays exhaustive.
            StmtKind::Break | StmtKind::Continue | StmtKind::Return(_) => None,
            // `Pass` is a true no-op — already correctly handled by
            // falling through to `_ => None` below: a block containing
            // only `pass` produces no node, which `BlockOutcome::Empty`
            // (and every caller's handling of it, e.g. `while x: pass`'s
            // self-loop) already covers correctly. Never actually needed
            // special-casing, despite being listed as deferred earlier.
            _ => None,
        }
    }

    /// `for var in iterable: ...` — like `build_while`, but the Decision's
    /// label is the iterable's own source text (there's no boolean
    /// condition to slice a span from), roughly "more items in `iterable`?"
    fn build_foreach(&mut self, line: usize, iterable: &Expr, body: &[Stmt]) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(iterable.span), line);
        let exit = inner.push(ActivityNode::Merge, line);
        match inner.block(body) {
            BlockOutcome::Flows(entry, tail) => {
                inner.connect(
                    decision,
                    Target::Local(entry),
                    Some(Guard::Cond(iterable.span)),
                );
                inner.connect(tail, Target::Local(decision), None);
            }
            BlockOutcome::Empty => inner.connect(
                decision,
                Target::Local(decision),
                Some(Guard::Cond(iterable.span)),
            ),
            // The body always diverges (e.g. `for x in xs: break`) — no
            // back-edge; the divert was already wired above by `block`,
            // or (if it was the very first thing) wired right here.
            BlockOutcome::Diverts(Some(entry), _) => {
                inner.connect(
                    decision,
                    Target::Local(entry),
                    Some(Guard::Cond(iterable.span)),
                );
            }
            BlockOutcome::Diverts(None, target) => {
                inner.connect(decision, target, Some(Guard::Cond(iterable.span)));
            }
        }
        inner.connect(decision, Target::Local(exit), Some(Guard::Else));
        let body_activity = Activity {
            nodes: inner.nodes,
            edges: inner.edges,
            initial: decision,
            finals: vec![exit],
        };
        let node = self.push(
            ActivityNode::Structured {
                stereotype: Stereotype::Loop,
                setup: None,
                body: body_activity,
            },
            line,
        );
        (node, node)
    }

    /// `for var in range(start, end, step): ...` — the counted fast path.
    /// Shaped like `build_while`/`build_foreach`, with `end`'s own span as
    /// the Decision's label (`end` is always a genuinely parsed `Expr` —
    /// the only positional form the parser accepts, `range(end)`, still
    /// parses it as a real expression; `start`/`step` are what get
    /// synthesized when omitted). `LoopSetup` carries all three spans on
    /// the outer `Structured` node for a renderer that wants the full
    /// `for var in range(...)` text — `start`/`step` there CAN be the
    /// synthesized placeholder's `(0, 0)` span when omitted (confirmed by
    /// running the parser, not assumed), same "no real source text here,
    /// fall back to a minimal hand-written label" situation this design's
    /// own `with`-desugar note already accepted, not a new problem.
    fn build_for(
        &mut self,
        line: usize,
        var: &str,
        start: &Expr,
        end: &Expr,
        step: &Expr,
        body: &[Stmt],
    ) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(end.span), line);
        let exit = inner.push(ActivityNode::Merge, line);
        match inner.block(body) {
            BlockOutcome::Flows(entry, tail) => {
                inner.connect(decision, Target::Local(entry), Some(Guard::Cond(end.span)));
                inner.connect(tail, Target::Local(decision), None);
            }
            BlockOutcome::Empty => {
                inner.connect(
                    decision,
                    Target::Local(decision),
                    Some(Guard::Cond(end.span)),
                );
            }
            // The body always diverges (e.g. `for i in range(n): break`) —
            // no back-edge; the divert was already wired above by
            // `block`, or (if it was the very first thing) wired right
            // here.
            BlockOutcome::Diverts(Some(entry), _) => {
                inner.connect(decision, Target::Local(entry), Some(Guard::Cond(end.span)));
            }
            BlockOutcome::Diverts(None, target) => {
                inner.connect(decision, target, Some(Guard::Cond(end.span)));
            }
        }
        inner.connect(decision, Target::Local(exit), Some(Guard::Else));
        let body_activity = Activity {
            nodes: inner.nodes,
            edges: inner.edges,
            initial: decision,
            finals: vec![exit],
        };
        let node = self.push(
            ActivityNode::Structured {
                stereotype: Stereotype::Loop,
                setup: Some(LoopSetup {
                    var: var.to_string(),
                    start: start.span,
                    end: end.span,
                    step: step.span,
                }),
                body: body_activity,
            },
            line,
        );
        (node, node)
    }

    fn build_if(
        &mut self,
        line: usize,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: Option<&[Stmt]>,
    ) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(cond.span), line);
        let merge = inner.push(ActivityNode::Merge, line);
        inner.wire_branch(decision, Guard::Cond(cond.span), body, merge);

        let mut last_decision = decision;
        for (econd, ebody) in elifs {
            let d2 = inner.push(ActivityNode::Decision(econd.span), line);
            inner.connect(last_decision, Target::Local(d2), Some(Guard::Else));
            inner.wire_branch(d2, Guard::Cond(econd.span), ebody, merge);
            last_decision = d2;
        }
        match else_body {
            Some(eb) => inner.wire_branch(last_decision, Guard::Else, eb, merge),
            None => inner.connect(last_decision, Target::Local(merge), Some(Guard::Else)),
        }

        let body_activity = Activity {
            nodes: inner.nodes,
            edges: inner.edges,
            initial: decision,
            finals: vec![merge],
        };
        let node = self.push(
            ActivityNode::Structured {
                stereotype: Stereotype::Conditional,
                setup: None,
                body: body_activity,
            },
            line,
        );
        (node, node)
    }

    /// Wire `from --guard--> (branch's entry)`, and the branch's tail (or
    /// `from` itself, guarded, if the branch produced no node) `--> join`
    /// (unguarded — `join` is a plain `Merge`, not another decision).
    /// If the branch diverges (`break`/`continue`) instead of falling
    /// through, it never connects to `join` at all — that edge was
    /// already wired (or is wired right here) straight to the loop
    /// target instead.
    fn wire_branch(&mut self, from: NodeId, guard: Guard, stmts: &[Stmt], join: NodeId) {
        match self.block(stmts) {
            BlockOutcome::Flows(entry, tail) => {
                self.connect(from, Target::Local(entry), Some(guard));
                self.connect(tail, Target::Local(join), None);
            }
            BlockOutcome::Empty => self.connect(from, Target::Local(join), Some(guard)),
            BlockOutcome::Diverts(Some(entry), _) => {
                self.connect(from, Target::Local(entry), Some(guard));
            }
            BlockOutcome::Diverts(None, target) => self.connect(from, target, Some(guard)),
        }
    }

    fn build_while(&mut self, line: usize, cond: &Expr, body: &[Stmt]) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(cond.span), line);
        let exit = inner.push(ActivityNode::Merge, line); // "after the loop"
        match inner.block(body) {
            BlockOutcome::Flows(entry, tail) => {
                inner.connect(decision, Target::Local(entry), Some(Guard::Cond(cond.span)));
                // The back-edge: an ordinary edge that happens to point
                // backward. A layout pass infers the special routing from
                // topology, not from a label built here.
                inner.connect(tail, Target::Local(decision), None);
            }
            // `while cond: pass` — the body is a no-op, so the loop's own
            // decision is an immediate self-loop. Correct, not a hack.
            BlockOutcome::Empty => {
                inner.connect(
                    decision,
                    Target::Local(decision),
                    Some(Guard::Cond(cond.span)),
                );
            }
            // The body always diverges (e.g. `while True: break`) — no
            // back-edge; the divert was already wired by `block`, or (if
            // it was the very first thing) wired right here.
            BlockOutcome::Diverts(Some(entry), _) => {
                inner.connect(decision, Target::Local(entry), Some(Guard::Cond(cond.span)));
            }
            BlockOutcome::Diverts(None, target) => {
                inner.connect(decision, target, Some(Guard::Cond(cond.span)));
            }
        }
        inner.connect(decision, Target::Local(exit), Some(Guard::Else));
        let body_activity = Activity {
            nodes: inner.nodes,
            edges: inner.edges,
            initial: decision,
            finals: vec![exit],
        };
        let node = self.push(
            ActivityNode::Structured {
                stereotype: Stereotype::Loop,
                setup: None,
                body: body_activity,
            },
            line,
        );
        (node, node)
    }
}

/// One node ready to render, flattened out of an `Activity` and all of
/// its nested `Structured` bodies into a single space. `Activity`'s own
/// `NodeId` is only unique *within* one `Activity` — a nested body's
/// ids restart at 0 — so flattening assigns a fresh, globally unique
/// `id` to every node, outer and nested alike.
#[derive(Debug, Clone)]
pub struct FlatNode {
    pub id: usize,
    pub kind: ActivityNode,
    pub line: usize,
}

/// One edge between two `FlatNode::id`s (already remapped from
/// whichever `Activity`'s own local ids it was declared with).
#[derive(Debug, Clone)]
pub struct FlatEdge {
    pub from: usize,
    pub to: usize,
    pub guard: Option<Guard>,
}

/// Pass 2: flatten `activity` and all nested `Structured` bodies into
/// one node/edge list with globally unique ids, and lay out each node's
/// 2D position in the same pass. Plain `(f64, f64)` — no UI/rendering
/// dependency belongs here (a renderer converts these to whatever its
/// own coordinate type is; that conversion is the only UI-specific
/// part).
///
/// A `Structured` node's `body` is flattened into the SAME coordinate
/// space as its parent (laid out starting right where the `Structured`
/// node itself sits) — v1 doesn't draw the dashed `«loop»`/
/// `«conditional»` container box yet, per the design plan, so there's
/// nothing to offset the body relative to.
///
/// **Known rough edges, left for when there's a real render to look
/// at**:
/// - where two branches of a `Decision` reconverge at their shared
///   `Merge`, that node's position is claimed by whichever branch's
///   walk reaches it first — if the branches are very unequal in
///   length, the merge can end up a little higher than ideal.
///   Topologically correct either way; only the exact Y looks a bit off
///   in the asymmetric case.
/// - a `Structured` node (an `if`/loop) always gets a plain fallthrough
///   edge to whatever follows it in the OUTER sequence, even when every
///   branch inside it actually diverges (every path `return`s, say) and
///   that edge can never fire. Suppressing it needs each `Structured`
///   node to report its own reachability up through `stmt()`'s uniform
///   return shape — a real refactor, not a one-line fix — so it's a
///   deliberately accepted dead edge for this narrow slice, not a
///   correctness bug: everything it points at still resolves to a real,
///   correctly-shared node.
///
/// `(flattened nodes, flattened edges, node id -> (x, y))`.
pub type FlatActivity = (
    Vec<FlatNode>,
    Vec<FlatEdge>,
    std::collections::HashMap<usize, (f64, f64)>,
);

/// `activity` is always one whole, self-contained function-or-module
/// body — `build()`'s module activity, or one of `build_program`'s
/// per-function/per-method activities — so this is always where a
/// `Target::Return` context gets established, never a nested call.
pub fn flatten_and_layout(activity: &Activity) -> FlatActivity {
    let mut next_id = 0usize;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut positions = std::collections::HashMap::new();
    place_activity(
        activity,
        0.0,
        0.0,
        &mut next_id,
        &mut nodes,
        &mut edges,
        &mut positions,
        false,
        None,
        true,
        None,
    );
    (nodes, edges, positions)
}

/// Walk one `Activity`'s own local structure (and, recursively, every
/// nested body's), assigning each local `NodeId` it encounters a fresh
/// global id via `next_id`. `local_to_global` is created fresh per
/// `Activity` — local ids are only meaningful within the one `Activity`
/// that assigned them, so a child body's map must never be confused
/// with its parent's. Returns the Y coordinate just past everything
/// this activity placed.
///
/// `is_loop` / `inherited_loop_ctx`: if this activity is a loop's own
/// body (`is_loop`), its `decision`/`exit` get resolved to global ids
/// up front — before anything inside it is walked — so any `break`/
/// `continue` found anywhere within it (however deeply nested inside
/// further non-loop `Structured` bodies) can resolve a `Target::LoopExit`/
/// `LoopBack` to a concrete id the moment it's encountered. A non-loop
/// body (an `if`'s) just inherits whatever loop context was already
/// active around it — a `break` inside an `if` inside a `while` still
/// means the `while`, so the context has to see through non-loop
/// nesting rather than reset at every `Structured` boundary.
///
/// `establish_fn_ctx` / `inherited_fn_ctx`: same idea, one layer up —
/// `true` only when `activity` is itself a whole function/module body
/// (every call from `flatten_and_layout` directly; never from a nested
/// `Structured` recursion), resolving `activity.finals[0]` (there's
/// exactly one, same guarantee `build()` already gives the module
/// activity) up front so `Target::Return` can resolve from anywhere
/// inside, however many loops or `if`s deep. Unlike `loop_ctx`, this
/// NEVER gets re-established at a nested boundary — a `return` two
/// loops deep still means THIS function's own end, not the nearest
/// loop's.
#[allow(clippy::too_many_arguments)]
fn place_activity(
    activity: &Activity,
    x0: f64,
    y0: f64,
    next_id: &mut usize,
    nodes: &mut Vec<FlatNode>,
    edges: &mut Vec<FlatEdge>,
    positions: &mut std::collections::HashMap<usize, (f64, f64)>,
    is_loop: bool,
    inherited_loop_ctx: Option<(usize, usize)>,
    establish_fn_ctx: bool,
    inherited_fn_ctx: Option<usize>,
) -> f64 {
    const ROW_H: f64 = 70.0;
    const COL_W: f64 = 180.0;

    fn assign(
        activity: &Activity,
        local_id: NodeId,
        local_to_global: &mut std::collections::HashMap<NodeId, usize>,
        next_id: &mut usize,
        nodes: &mut Vec<FlatNode>,
    ) -> usize {
        *local_to_global.entry(local_id).or_insert_with(|| {
            let gid = *next_id;
            *next_id += 1;
            let n = &activity.nodes[local_id];
            nodes.push(FlatNode {
                id: gid,
                kind: n.kind.clone(),
                line: n.line,
            });
            gid
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        activity: &Activity,
        local_id: NodeId,
        x: f64,
        y: f64,
        local_to_global: &mut std::collections::HashMap<NodeId, usize>,
        next_id: &mut usize,
        nodes: &mut Vec<FlatNode>,
        edges: &mut Vec<FlatEdge>,
        positions: &mut std::collections::HashMap<usize, (f64, f64)>,
        visited: &mut std::collections::HashSet<NodeId>,
        loop_ctx: Option<(usize, usize)>,
        fn_ctx: Option<usize>,
    ) -> f64 {
        let gid = assign(activity, local_id, local_to_global, next_id, nodes);
        if !visited.insert(local_id) {
            // Already placed — either a loop's back-edge (correctly
            // stops the recursion there) or the second branch reaching a
            // shared Merge (see the "known rough edge" note above).
            return y;
        }
        positions.insert(gid, (x, y));
        let mut y_after = y + ROW_H;

        if let ActivityNode::Structured {
            stereotype, body, ..
        } = &activity.nodes[local_id].kind
        {
            y_after = place_activity(
                body,
                x,
                y_after,
                next_id,
                nodes,
                edges,
                positions,
                *stereotype == Stereotype::Loop,
                loop_ctx,
                false,
                fn_ctx,
            );
        }

        let outs: Vec<&ControlFlow> = activity
            .edges
            .iter()
            .filter(|e| e.from == local_id)
            .collect();
        for e in &outs {
            // `Target::LoopExit`/`LoopBack`/`Return` resolve against
            // whatever context is already active here (see
            // `place_activity`'s doc comment) rather than through
            // `activity`'s own local node list — their destination was
            // never local to begin with. `None` here only happens for a
            // `break`/`continue`/`return` outside any loop/function,
            // which isn't valid Python; skip the edge rather than
            // invent a destination for input a real compile would
            // already have rejected.
            let to_gid = match e.to {
                Target::Local(local) => {
                    Some(assign(activity, local, local_to_global, next_id, nodes))
                }
                Target::LoopExit => loop_ctx.map(|(_, exit)| exit),
                Target::LoopBack => loop_ctx.map(|(decision, _)| decision),
                Target::Return => fn_ctx,
            };
            if let Some(to_gid) = to_gid {
                edges.push(FlatEdge {
                    from: gid,
                    to: to_gid,
                    guard: e.guard.clone(),
                });
            }
        }
        // Branch count — and which side each guard lays out on — is
        // about how many guarded edges exist, regardless of whether a
        // given one's destination is local. Recursion itself is skipped
        // per-branch below when it isn't: `LoopExit`/`LoopBack` already
        // got their edge above and point at a node some ancestor
        // activity positions itself, so there's nothing left to walk.
        match outs.len() {
            0 => y_after,
            1 => match outs[0].to {
                Target::Local(to) => place(
                    activity,
                    to,
                    x,
                    y_after,
                    local_to_global,
                    next_id,
                    nodes,
                    edges,
                    positions,
                    visited,
                    loop_ctx,
                    fn_ctx,
                ),
                Target::LoopExit | Target::LoopBack | Target::Return => y_after,
            },
            _ => {
                // A Decision: its guarded edges branch side by side —
                // Cond left, Else right, matching the "[cond]"/"[else]"
                // convention the guard labels themselves use.
                let cond = outs
                    .iter()
                    .find(|e| matches!(e.guard, Some(Guard::Cond(_))))
                    .map(|e| e.to);
                let other = outs
                    .iter()
                    .find(|e| matches!(e.guard, Some(Guard::Else)))
                    .map(|e| e.to);
                let mut max_y = y_after;
                if let Some(Target::Local(to)) = cond {
                    max_y = max_y.max(place(
                        activity,
                        to,
                        x - COL_W / 2.0,
                        y_after,
                        local_to_global,
                        next_id,
                        nodes,
                        edges,
                        positions,
                        visited,
                        loop_ctx,
                        fn_ctx,
                    ));
                }
                if let Some(Target::Local(to)) = other {
                    max_y = max_y.max(place(
                        activity,
                        to,
                        x + COL_W / 2.0,
                        y_after,
                        local_to_global,
                        next_id,
                        nodes,
                        edges,
                        positions,
                        visited,
                        loop_ctx,
                        fn_ctx,
                    ));
                }
                max_y
            }
        }
    }

    let mut local_to_global = std::collections::HashMap::new();
    let mut visited = std::collections::HashSet::new();
    let loop_ctx = if is_loop {
        let decision_gid = assign(
            activity,
            activity.initial,
            &mut local_to_global,
            next_id,
            nodes,
        );
        let exit_gid = assign(
            activity,
            activity.finals[0],
            &mut local_to_global,
            next_id,
            nodes,
        );
        Some((decision_gid, exit_gid))
    } else {
        inherited_loop_ctx
    };
    let fn_ctx = if establish_fn_ctx {
        // Unlike a loop's `exit` (always reached by the Decision's own
        // `Guard::Else` edge, so `place()`'s normal recursion always
        // positions it), a function's `ActivityFinal` can be reached
        // ONLY via `Target::Return` edges (e.g. every branch of the
        // function's top-level `if` returns) — `place()` never visits
        // it locally in that case, so it'd otherwise end up with a
        // `FlatNode` but no entry in `positions`. Give it a provisional
        // one now; a later genuine local visit overwrites it with the
        // real computed position, same `positions.insert` either way.
        let final_gid = assign(
            activity,
            activity.finals[0],
            &mut local_to_global,
            next_id,
            nodes,
        );
        positions.entry(final_gid).or_insert((x0, y0));
        Some(final_gid)
    } else {
        inherited_fn_ctx
    };
    place(
        activity,
        activity.initial,
        x0,
        y0,
        &mut local_to_global,
        next_id,
        nodes,
        edges,
        positions,
        &mut visited,
        loop_ctx,
        fn_ctx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn build_src(src: &str) -> Activity {
        let toks = crate::lexer::lex(src).unwrap();
        let stmts = parser::parse(&toks).unwrap();
        build(&stmts)
    }

    fn structured(a: &Activity) -> &ActivityNode {
        &a.nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Structured { .. }))
            .expect("expected a Structured node")
            .kind
    }

    #[test]
    fn a_straight_line_program_is_a_simple_chain() {
        let a = build_src("x = 1\ny = 2\n");
        // Initial, ActivityFinal, and two Actions — order isn't load-bearing.
        assert_eq!(a.nodes.len(), 4);
        assert_eq!(
            a.nodes
                .iter()
                .filter(|n| matches!(n.kind, ActivityNode::Action(_)))
                .count(),
            2
        );
        assert!(
            a.nodes
                .iter()
                .any(|n| matches!(n.kind, ActivityNode::Initial))
        );
        assert!(
            a.nodes
                .iter()
                .any(|n| matches!(n.kind, ActivityNode::ActivityFinal))
        );
        assert_eq!(a.edges.len(), 3);
        assert!(a.edges.iter().all(|e| e.guard.is_none()));
    }

    #[test]
    fn an_if_else_is_a_structured_conditional_with_a_decision_and_a_merge() {
        let a = build_src("if x > 0:\n    y = 1\nelse:\n    y = 2\n");
        assert_eq!(a.nodes.len(), 3); // Initial, Structured, ActivityFinal
        let ActivityNode::Structured {
            stereotype, body, ..
        } = structured(&a)
        else {
            unreachable!()
        };
        assert_eq!(*stereotype, Stereotype::Conditional);
        // Decision, Merge, then-Action, else-Action
        assert_eq!(body.nodes.len(), 4);
        let decisions = body
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .count();
        let merges = body
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Merge))
            .count();
        assert_eq!(decisions, 1);
        assert_eq!(merges, 1);
        // Both guarded edges out of the decision exist.
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        let out: Vec<_> = body
            .edges
            .iter()
            .filter(|e| e.from == decision_id)
            .collect();
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|e| matches!(e.guard, Some(Guard::Cond(_)))));
        assert!(out.iter().any(|e| matches!(e.guard, Some(Guard::Else))));
    }

    #[test]
    fn an_elif_chain_adds_a_decision_per_condition() {
        let a = build_src(
            "if a:\n    x = 1\nelif b:\n    x = 2\nelif c:\n    x = 3\nelse:\n    x = 4\n",
        );
        let ActivityNode::Structured { body, .. } = structured(&a) else {
            unreachable!()
        };
        let decisions = body
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .count();
        assert_eq!(decisions, 3); // if, elif, elif
        let merges = body
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Merge))
            .count();
        assert_eq!(merges, 1); // every branch rejoins at ONE merge
    }

    #[test]
    fn a_while_loop_is_a_structured_loop_with_a_back_edge() {
        let a = build_src("while x > 0:\n    x = x - 1\n");
        let ActivityNode::Structured {
            stereotype, body, ..
        } = structured(&a)
        else {
            unreachable!()
        };
        assert_eq!(*stereotype, Stereotype::Loop);
        // Decision, Merge (the loop's own exit), one Action for the body.
        assert_eq!(body.nodes.len(), 3);
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        // A plain (unguarded) edge lands back on the decision — the
        // back-edge, inferred from topology, not labeled here.
        assert!(
            body.edges
                .iter()
                .any(|e| e.to == Target::Local(decision_id) && e.guard.is_none())
        );
    }

    #[test]
    fn an_empty_while_body_is_a_self_loop_not_a_crash() {
        let a = build_src("while x > 0:\n    pass\n");
        let ActivityNode::Structured { body, .. } = structured(&a) else {
            unreachable!()
        };
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        assert!(
            body.edges
                .iter()
                .any(|e| e.from == decision_id && e.to == Target::Local(decision_id))
        );
    }

    #[test]
    fn flatten_assigns_every_node_a_unique_global_id() {
        // Nested (If inside While) so an inner body's locally-scoped
        // NodeIds collide with the outer activity's — exactly the case
        // that broke the earlier HashMap<NodeId, _> design.
        let a = build_src("while x > 0:\n    if x > 1:\n        y = 1\n    else:\n        y = 2\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        let mut ids: Vec<usize> = nodes.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            nodes.len(),
            "every flattened node must have a unique id"
        );
        assert_eq!(
            positions.len(),
            nodes.len(),
            "every flattened node must get a position"
        );
    }

    #[test]
    fn a_straight_line_chain_lays_out_as_one_column_of_increasing_rows() {
        let a = build_src("x = 1\ny = 2\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        let xs: Vec<f64> = nodes.iter().map(|n| positions[&n.id].0).collect();
        assert!(
            xs.iter().all(|&x| x == xs[0]),
            "a straight chain stays in one column"
        );
        let mut ys: Vec<f64> = nodes.iter().map(|n| positions[&n.id].1).collect();
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ys.dedup();
        assert_eq!(ys.len(), nodes.len(), "a straight chain never reuses a row");
    }

    #[test]
    fn an_if_else_s_two_branches_land_in_different_columns() {
        let a = build_src("if x > 0:\n    y = 1\nelse:\n    y = 2\n");
        let (nodes, edges, positions) = flatten_and_layout(&a);
        let decision = nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap();
        let cond_to = edges
            .iter()
            .find(|e| e.from == decision.id && matches!(e.guard, Some(Guard::Cond(_))))
            .unwrap()
            .to;
        let else_to = edges
            .iter()
            .find(|e| e.from == decision.id && matches!(e.guard, Some(Guard::Else)))
            .unwrap()
            .to;
        assert_ne!(
            positions[&cond_to].0, positions[&else_to].0,
            "the two branches of a decision must land in different columns"
        );
    }

    #[test]
    fn a_while_loop_s_back_edge_does_not_infinite_loop_the_layout() {
        // Regression: flatten_and_layout must terminate on a cyclic
        // Activity graph (the loop's own back-edge).
        let a = build_src("while x > 0:\n    x = x - 1\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }

    #[test]
    fn a_foreach_loop_is_a_structured_loop_with_a_back_edge() {
        let a = build_src("for x in xs:\n    y = x\n");
        let ActivityNode::Structured {
            stereotype, body, ..
        } = structured(&a)
        else {
            unreachable!()
        };
        assert_eq!(*stereotype, Stereotype::Loop);
        // Decision, Merge (the loop's own exit), one Action for the body.
        assert_eq!(body.nodes.len(), 3);
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        assert!(
            body.edges
                .iter()
                .any(|e| e.to == Target::Local(decision_id) && e.guard.is_none())
        );
    }

    #[test]
    fn an_empty_foreach_body_is_a_self_loop_not_a_crash() {
        let a = build_src("for x in xs:\n    pass\n");
        let ActivityNode::Structured { body, .. } = structured(&a) else {
            unreachable!()
        };
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        assert!(
            body.edges
                .iter()
                .any(|e| e.from == decision_id && e.to == Target::Local(decision_id))
        );
    }

    #[test]
    fn a_foreach_loop_flattens_and_lays_out_without_crashing() {
        let a = build_src("for x in xs:\n    y = x\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }

    #[test]
    fn a_counted_for_is_a_structured_loop_carrying_its_loop_setup() {
        let a = build_src("for i in range(3):\n    y = i\n");
        let ActivityNode::Structured {
            stereotype, setup, ..
        } = structured(&a)
        else {
            unreachable!()
        };
        assert_eq!(*stereotype, Stereotype::Loop);
        let setup = setup.as_ref().expect("counted for must carry a LoopSetup");
        assert_eq!(setup.var, "i");
        // `range(3)` — a single positional arg is `end`; `start`/`step`
        // are synthesized defaults with no real source to point at.
        assert_eq!(setup.start.0, 0);
        assert_eq!(setup.start.1, 0);
        assert_eq!(setup.step.0, 0);
        assert_eq!(setup.step.1, 0);
        assert_ne!(setup.end, (0, 0), "end is always a real parsed Expr");
    }

    #[test]
    fn a_counted_for_s_three_arg_range_gives_every_field_a_real_span() {
        let a = build_src("for i in range(1, 6, 2):\n    y = i\n");
        let ActivityNode::Structured { setup, .. } = structured(&a) else {
            unreachable!()
        };
        let setup = setup.as_ref().unwrap();
        assert_ne!(setup.start, (0, 0));
        assert_ne!(setup.end, (0, 0));
        assert_ne!(setup.step, (0, 0));
    }

    #[test]
    fn a_counted_for_flattens_and_lays_out_without_crashing() {
        let a = build_src("for i in range(3):\n    y = i\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }

    #[test]
    fn break_inside_a_counted_for_resolves_to_its_exit() {
        // The same cross-Activity-boundary case already covered for
        // while/for-each, now exercised on the newly-built counted for.
        let a = build_src(
            "for i in range(10):\n    if arr[i] == target:\n        found = i\n        break\n",
        );
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let exit_id = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, ActivityNode::Merge) && {
                    let froms: std::collections::HashSet<_> = edges
                        .iter()
                        .filter(|e| e.to == n.id)
                        .map(|e| e.from)
                        .collect();
                    froms.len() >= 2
                }
            })
            .map(|n| n.id);
        assert!(
            exit_id.is_some(),
            "the break and the loop's own Else path must both resolve to the same exit"
        );
    }

    #[test]
    fn a_bare_return_resolves_to_the_function_s_activity_final() {
        let a = build_src("if x:\n    return\ny = 1\n");
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let fin_id = nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::ActivityFinal))
            .unwrap()
            .id;
        let if_decision = nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        assert!(
            edges
                .iter()
                .any(|e| e.from == if_decision && e.to == fin_id),
            "a bare return nested in an if must resolve to this Activity's own final"
        );
    }

    #[test]
    fn a_return_with_a_value_gets_its_own_labeled_action_node() {
        let a = build_src("return x + 1\n");
        let returns: Vec<_> = a
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Action(_)))
            .collect();
        assert_eq!(returns.len(), 1, "return's own expression gets an Action");
    }

    #[test]
    fn a_bare_return_gets_no_node_of_its_own() {
        let a = build_src("return\n");
        assert!(
            a.nodes
                .iter()
                .all(|n| !matches!(n.kind, ActivityNode::Action(_))),
            "a bare return is pure routing, same as break/continue"
        );
    }

    #[test]
    fn multiple_returns_in_one_function_converge_on_the_same_final() {
        let a = build_src("if x:\n    return 1\nelse:\n    return 2\n");
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let fin_id = nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::ActivityFinal))
            .unwrap()
            .id;
        let returns: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Action(_)))
            .map(|n| n.id)
            .collect();
        assert_eq!(
            returns.len(),
            2,
            "both return expressions got their own node"
        );
        let froms: std::collections::HashSet<_> = edges
            .iter()
            .filter(|e| e.to == fin_id)
            .map(|e| e.from)
            .collect();
        assert!(
            returns.iter().all(|r| froms.contains(r)),
            "both returns' own Action nodes must reach the SAME final"
        );
        // Known rough edge (same spirit as the asymmetric-merge-Y-position
        // one documented on `flatten_and_layout`): the `if`'s own outer
        // "box" ALSO gets a dead fallthrough edge to `fin`, since
        // `build_if` always returns a plain pass-through node regardless
        // of whether its merge is actually reachable — here it never is
        // (both branches return), so this edge can never fire. Cosmetic,
        // not a correctness bug; not worth the reachability-propagation
        // refactor it'd take to suppress for this narrow slice.
        assert_eq!(
            froms.len(),
            3,
            "includes the if's own dead fallthrough edge"
        );
    }

    #[test]
    fn build_program_collects_the_module_plus_one_activity_per_def_and_method() {
        let src = "x = 1\n\
                   def f():\n\
                   \x20   return 1\n\
                   class C:\n\
                   \x20   def m1(self):\n\
                   \x20       pass\n\
                   \x20   def m2(self):\n\
                   \x20       pass\n";
        let toks = crate::lexer::lex(src).unwrap();
        let stmts = parser::parse(&toks).unwrap();
        let program = build_program(&stmts);
        let names: Vec<&str> = program.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["<module>", "f", "C.m1", "C.m2"]);
        // The module's own activity still renders x=1 as an Action even
        // though Def/ClassDef statements sit alongside it (unchanged
        // `build()` behavior — they just produce no node there).
        assert_eq!(
            program[0]
                .activity
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, ActivityNode::Action(_)))
                .count(),
            1
        );
    }

    /// `Def` is still deferred (see the module's own doc comment on why) —
    /// confirm it's a graceful `None`, not a panic, so a program using it
    /// still renders everything around it.
    #[test]
    fn deferred_statement_kinds_are_skipped_not_a_crash() {
        let a = build_src("x = 1\ndef f():\n    pass\ny = 2\n");
        // x=1 and y=2 survive as Actions; the def is skipped.
        assert_eq!(
            a.nodes
                .iter()
                .filter(|n| matches!(n.kind, ActivityNode::Action(_)))
                .count(),
            2
        );
        assert!(
            a.nodes
                .iter()
                .all(|n| !matches!(n.kind, ActivityNode::Structured { .. })),
            "a deferred statement kind should produce no node at all, not a broken one"
        );
    }

    #[test]
    fn a_bare_break_wires_the_decision_straight_to_the_loop_s_exit() {
        let a = build_src("while True:\n    break\n");
        let ActivityNode::Structured { body, .. } = structured(&a) else {
            unreachable!()
        };
        // Just a Decision and a Merge (exit) — `break` produced no node.
        assert_eq!(body.nodes.len(), 2);
        let decision_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .unwrap()
            .id;
        let exit_id = body
            .nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Merge))
            .unwrap()
            .id;
        // `break` (the Cond edge) is recorded as a symbolic `LoopExit` —
        // it's the SAME decision's own `exit`, but `Target` doesn't know
        // that at this (unflattened) level; the Else edge (the loop's
        // own ordinary "after the loop" path) is a plain local edge to
        // that same node. `flatten_and_layout` is what proves they
        // resolve to the same place (see the nested-break test below).
        let targets: Vec<_> = body
            .edges
            .iter()
            .filter(|e| e.from == decision_id)
            .map(|e| e.to)
            .collect();
        assert_eq!(targets, vec![Target::LoopExit, Target::Local(exit_id)]);
    }

    #[test]
    fn a_break_nested_inside_an_if_inside_a_while_resolves_at_flatten_time() {
        // The case the whole design exists for: `break` is built by the
        // `if`'s own separate Builder/Activity, with no local id for the
        // while's exit at all — only `flatten_and_layout` (which knows
        // every loop's global ids by the time it reaches this) can
        // resolve it.
        let a = build_src("while x > 0:\n    if x > 1:\n        break\n    x = x - 1\n");
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node still gets placed");
        let exit_id = nodes
            .iter()
            .find(|n| matches!(n.kind, ActivityNode::Merge))
            .unwrap()
            .id;
        let decision_ids: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .map(|n| n.id)
            .collect();
        // The if's own Decision has an edge straight to the while's exit
        // — the break, resolved to a concrete global id, not a crash and
        // not left dangling as a `Target::LoopExit` placeholder.
        assert!(
            edges
                .iter()
                .any(|e| decision_ids.contains(&e.from) && e.to == exit_id),
            "the nested break must resolve to the enclosing loop's exit"
        );
    }

    #[test]
    fn continue_nested_in_an_if_resolves_to_the_loop_s_own_decision() {
        let a = build_src("for x in xs:\n    if x < 0:\n        continue\n    y = x\n");
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let loop_decision = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, ActivityNode::Decision(_)) && {
                    // the loop's own decision is the one with a self/back edge
                    // into it from somewhere, as opposed to the if's decision
                    edges.iter().any(|e| e.to == n.id && e.from != n.id)
                }
            })
            .map(|n| n.id);
        assert!(loop_decision.is_some());
        let if_decisions: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Decision(_)) && Some(n.id) != loop_decision)
            .map(|n| n.id)
            .collect();
        assert!(
            edges
                .iter()
                .any(|e| if_decisions.contains(&e.from) && Some(e.to) == loop_decision),
            "continue must resolve to the loop's own decision, not its exit"
        );
    }

    #[test]
    fn a_heap_sort_sift_down_s_control_flow_renders_without_crashing() {
        // `Def` isn't walked into yet, so this is the loop's body at
        // module level rather than wrapped in a function — still
        // exercises the real shape this design exists for: two `break`s
        // at different nesting depths inside the same `while True`, one
        // of them behind two levels of `if`.
        let a = build_src(
            "root = start\n\
             while True:\n\
             \x20   child = 2 * root + 1\n\
             \x20   if child > end:\n\
             \x20       break\n\
             \x20   if child + 1 <= end:\n\
             \x20       if heap[child] < heap[child + 1]:\n\
             \x20           child = child + 1\n\
             \x20   if heap[root] < heap[child]:\n\
             \x20       tmp = heap[root]\n\
             \x20       heap[root] = heap[child]\n\
             \x20       heap[child] = tmp\n\
             \x20       root = child\n\
             \x20   else:\n\
             \x20       break\n",
        );
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node gets placed");
        let exit_id = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, ActivityNode::Merge) && {
                    // the loop's own exit is the Merge nothing points INTO
                    // except via a break/the loop's own Else — distinguish
                    // it from the three `if`s' own merges by checking it's
                    // targeted from more than one distinct Decision.
                    let froms: std::collections::HashSet<_> = edges
                        .iter()
                        .filter(|e| e.to == n.id)
                        .map(|e| e.from)
                        .collect();
                    froms.len() >= 2
                }
            })
            .map(|n| n.id);
        assert!(
            exit_id.is_some(),
            "both breaks must resolve to the SAME loop exit, reached from multiple decisions"
        );
    }

    #[test]
    fn bubble_sort_s_no_swaps_early_exit_resolves_through_a_sibling_loop() {
        // The canonical real-world `break` idiom: a flag set inside one
        // loop (the inner pass) is read by an `if` that's a SIBLING of
        // that loop, not inside it — `break`'s own `Builder` has nothing
        // to do with the inner loop's at all, just the outer one's.
        let a = build_src(
            "swapped = True\n\
             while swapped:\n\
             \x20   swapped = False\n\
             \x20   i = 0\n\
             \x20   while i < n:\n\
             \x20       if arr[i] > arr[i + 1]:\n\
             \x20           tmp = arr[i]\n\
             \x20           arr[i] = arr[i + 1]\n\
             \x20           arr[i + 1] = tmp\n\
             \x20           swapped = True\n\
             \x20       i = i + 1\n\
             \x20   if not swapped:\n\
             \x20       break\n",
        );
        let (nodes, edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node gets placed");
        // Two loops, two Decisions with a back-edge each, two Merges. The
        // OUTER loop's Merge is the one reachable from the no-swaps `if`.
        let decisions: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Decision(_)))
            .map(|n| n.id)
            .collect();
        assert_eq!(
            decisions.len(),
            4,
            "outer while, inner while, the swap-check if, the no-swaps if"
        );
        let merges: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, ActivityNode::Merge))
            .map(|n| n.id)
            .collect();
        assert_eq!(
            merges.len(),
            4,
            "each loop's own exit, plus each if's own join"
        );
        let if_decision = decisions
            .iter()
            .find(|&&d| {
                // the if's own decision has exactly one outgoing edge
                // that's a plain `Local` to a Merge (the taken/not-taken
                // join) and the OTHER lands straight on a Merge too via
                // the break — i.e. BOTH its outs land on some Merge,
                // unlike the loop decisions (one out is a back-edge to
                // a Decision).
                edges
                    .iter()
                    .filter(|e| e.from == d)
                    .all(|e| merges.contains(&e.to))
            })
            .copied();
        assert!(
            if_decision.is_some(),
            "the no-swaps if's break must resolve to a concrete Merge, not dangle"
        );
    }

    #[test]
    fn break_outside_any_loop_is_skipped_not_a_crash() {
        // Not valid Python, but shouldn't panic the flowchart view either.
        let a = build_src("x = 1\nbreak\ny = 2\n");
        let (nodes, _edges, positions) = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }
}
