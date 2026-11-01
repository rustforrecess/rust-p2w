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
//! `SetAttr`/`UnpackAssign`/bare `Expr` as plain `Action`s, `If`/`While` as
//! `Structured` nodes. `For`/`ForEach`/`Break`/`Continue`/`Return`/`Pass`/
//! `Def`/`ClassDef`/`Method` are a deliberately sequenced follow-up, so
//! this can be verified end to end on a small slice before expanding.

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
    pub to: NodeId,
    pub guard: Option<Guard>,
}

/// The `[cond]` / `[else]` label UML puts on an edge leaving a `Decision`.
#[derive(Debug, Clone, PartialEq)]
pub enum Guard {
    Cond(Span),
    Else,
}

/// Build the module-level activity for a parsed program.
pub fn build(stmts: &[Stmt]) -> Activity {
    let mut b = Builder::default();
    let initial = b.push(ActivityNode::Initial, 0);
    let fin = b.push(ActivityNode::ActivityFinal, 0);
    match b.block(stmts) {
        Some((entry, tail)) => {
            b.connect(initial, entry, None);
            b.connect(tail, fin, None);
        }
        None => b.connect(initial, fin, None),
    }
    Activity {
        nodes: b.nodes,
        edges: b.edges,
        initial,
        finals: vec![fin],
    }
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    edges: Vec<ControlFlow>,
}

impl Builder {
    fn push(&mut self, kind: ActivityNode, line: usize) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { id, kind, line });
        id
    }

    fn connect(&mut self, from: NodeId, to: NodeId, guard: Option<Guard>) {
        self.edges.push(ControlFlow { from, to, guard });
    }

    /// Build a sequence of statements, wiring each to the next. Returns
    /// `(entry, tail)` — the first node reached and the last — or `None`
    /// if the block produced no node at all (every statement was a
    /// deferred kind; impossible once `For`/etc. land, but real today for
    /// e.g. a body that's just `pass`).
    fn block(&mut self, stmts: &[Stmt]) -> Option<(NodeId, NodeId)> {
        let mut result: Option<(NodeId, NodeId)> = None;
        for s in stmts {
            if let Some((entry, tail)) = self.stmt(s) {
                result = match result {
                    None => Some((entry, tail)),
                    Some((first, prev_tail)) => {
                        self.connect(prev_tail, entry, None);
                        Some((first, tail))
                    }
                };
            }
        }
        result
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
            _ => None,
        }
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
            inner.connect(last_decision, d2, Some(Guard::Else));
            inner.wire_branch(d2, Guard::Cond(econd.span), ebody, merge);
            last_decision = d2;
        }
        match else_body {
            Some(eb) => inner.wire_branch(last_decision, Guard::Else, eb, merge),
            None => inner.connect(last_decision, merge, Some(Guard::Else)),
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
    fn wire_branch(&mut self, from: NodeId, guard: Guard, stmts: &[Stmt], join: NodeId) {
        match self.block(stmts) {
            Some((entry, tail)) => {
                self.connect(from, entry, Some(guard));
                self.connect(tail, join, None);
            }
            None => self.connect(from, join, Some(guard)),
        }
    }

    fn build_while(&mut self, line: usize, cond: &Expr, body: &[Stmt]) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(cond.span), line);
        let exit = inner.push(ActivityNode::Merge, line); // "after the loop"
        match inner.block(body) {
            Some((entry, tail)) => {
                inner.connect(decision, entry, Some(Guard::Cond(cond.span)));
                // The back-edge: an ordinary edge that happens to point
                // backward. A layout pass infers the special routing from
                // topology, not from a label built here.
                inner.connect(tail, decision, None);
            }
            // `while cond: pass` — the body is a no-op, so the loop's own
            // decision is an immediate self-loop. Correct, not a hack.
            None => inner.connect(decision, decision, Some(Guard::Cond(cond.span))),
        }
        inner.connect(decision, exit, Some(Guard::Else));
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
/// **Known rough edge, left for when there's a real render to look at**:
/// where two branches of a `Decision` reconverge at their shared
/// `Merge`, that node's position is claimed by whichever branch's walk
/// reaches it first — if the branches are very unequal in length, the
/// merge can end up a little higher than ideal. Topologically correct
/// either way; only the exact Y looks a bit off in the asymmetric case.
/// `(flattened nodes, flattened edges, node id -> (x, y))`.
pub type FlatActivity = (
    Vec<FlatNode>,
    Vec<FlatEdge>,
    std::collections::HashMap<usize, (f64, f64)>,
);

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
fn place_activity(
    activity: &Activity,
    x0: f64,
    y0: f64,
    next_id: &mut usize,
    nodes: &mut Vec<FlatNode>,
    edges: &mut Vec<FlatEdge>,
    positions: &mut std::collections::HashMap<usize, (f64, f64)>,
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

        if let ActivityNode::Structured { body, .. } = &activity.nodes[local_id].kind {
            y_after = place_activity(body, x, y_after, next_id, nodes, edges, positions);
        }

        let outs: Vec<&ControlFlow> = activity
            .edges
            .iter()
            .filter(|e| e.from == local_id)
            .collect();
        for e in &outs {
            let to_gid = assign(activity, e.to, local_to_global, next_id, nodes);
            edges.push(FlatEdge {
                from: gid,
                to: to_gid,
                guard: e.guard.clone(),
            });
        }

        match outs.len() {
            0 => y_after,
            1 => place(
                activity,
                outs[0].to,
                x,
                y_after,
                local_to_global,
                next_id,
                nodes,
                edges,
                positions,
                visited,
            ),
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
                if let Some(to) = cond {
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
                    ));
                }
                if let Some(to) = other {
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
                    ));
                }
                max_y
            }
        }
    }

    let mut local_to_global = std::collections::HashMap::new();
    let mut visited = std::collections::HashSet::new();
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
                .any(|e| e.to == decision_id && e.guard.is_none())
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
                .any(|e| e.from == decision_id && e.to == decision_id)
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
}
