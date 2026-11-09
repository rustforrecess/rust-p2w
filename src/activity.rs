//! UML 2.5.1 activity diagrams: a read-only projection over the shared AST.
//!
//! Pass 1 (`build`, `build_program`) turns statements into an `Activity`,
//! with each `if`/loop body nested in a `Structured` node. Pass 2
//! (`flatten`, then `layout`) turns that into one positioned graph.
//!
//! The IR follows the OMG metamodel: `Decision` and `Merge` are separate
//! metaclasses, and an edge has no "kind". A back-edge or a `break` is just
//! an edge, identified by what it connects.
//!
//! **Licensing, if this is rendered anywhere**: cite OMG UML 2.5.1, carry the
//! UML(R) trademark line, never copy spec text or figures.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, Span, Stmt, StmtKind};

pub type NodeId = usize;

/// One self-contained graph: a function, a method, module-level code, or a
/// `Structured` node's body. `NodeId`s are local to the `Activity`.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub nodes: Vec<Node>,
    pub edges: Vec<ControlFlow>,
    pub initial: NodeId,
    /// Where control leaves: the `ActivityFinal`, or a body's `Merge`.
    pub exit: NodeId,
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
    /// Label is `source[span.0..span.1]`.
    Action(Span),
    /// Label is `source[span.0..span.1]`.
    Decision(Span),
    Merge,
    Structured {
        stereotype: Stereotype,
        setup: Option<LoopSetup>,
        body: Activity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stereotype {
    Loop,
    Conditional,
}

/// A counted `for`'s header. `start` and `step` are `(0, 0)` when omitted
/// from the `range(...)` call.
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

/// An edge's destination. A jump's target lives in an enclosing `Activity`
/// with its own id space, so Pass 1 records the intent and flattening
/// resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local(NodeId),
    /// `break`: the nearest enclosing loop's exit.
    LoopExit,
    /// `continue`: the nearest enclosing loop's `Decision`.
    LoopBack,
    /// `return`: the enclosing function's `ActivityFinal`.
    Return,
}

/// The `[cond]` / `[else]` label on an edge leaving a `Decision`.
#[derive(Debug, Clone, PartialEq)]
pub enum Guard {
    Cond(Span),
    Else,
}

/// Build one `Activity` from module-level code or a function body.
pub fn build(stmts: &[Stmt]) -> Activity {
    let mut b = Builder::default();
    let initial = b.push(ActivityNode::Initial, 0);
    let fin = b.push(ActivityNode::ActivityFinal, 0);
    let outcome = b.block(stmts);
    b.connect(initial, outcome.entry().unwrap_or(Target::Local(fin)), None);
    if let BlockOutcome::Flows(_, tail) = outcome {
        b.connect(tail, Target::Local(fin), None);
    }
    b.finish(initial, fin)
}

/// One named `Activity`: `"<module>"`, a function's name, or `"Class.method"`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedActivity {
    pub name: String,
    pub activity: Activity,
}

/// Build the module's `Activity` plus one per top-level `def` and method.
/// A `def` nested inside anything else gets no diagram of its own.
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

/// How a sequence of statements ends.
enum BlockOutcome {
    /// Produced no node; control falls straight through.
    Empty,
    /// `(entry, tail)`: control falls through past `tail`.
    Flows(NodeId, NodeId),
    /// Ends in a jump, already wired from any node before it. `None` means
    /// the jump came first, so there's no node to enter.
    Diverts(Option<NodeId>, Target),
}

impl BlockOutcome {
    /// Where an edge entering this block should go. `None` for an empty one.
    fn entry(&self) -> Option<Target> {
        match *self {
            BlockOutcome::Empty => None,
            BlockOutcome::Flows(entry, _) | BlockOutcome::Diverts(Some(entry), _) => {
                Some(Target::Local(entry))
            }
            BlockOutcome::Diverts(None, target) => Some(target),
        }
    }
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

    fn finish(self, initial: NodeId, exit: NodeId) -> Activity {
        Activity {
            nodes: self.nodes,
            edges: self.edges,
            initial,
            exit,
        }
    }

    /// Build a sequence of statements, wiring each to the next. A jump ends
    /// the block: anything after it is unreachable.
    fn block(&mut self, stmts: &[Stmt]) -> BlockOutcome {
        let mut chain: Option<(NodeId, NodeId)> = None;
        for s in stmts {
            if let Some((entry, tail)) = self.stmt(s) {
                chain = match chain {
                    None => Some((entry, tail)),
                    Some((first, prev_tail)) => {
                        self.connect(prev_tail, Target::Local(entry), None);
                        Some((first, tail))
                    }
                };
            }
            let target = match s.kind {
                StmtKind::Break => Target::LoopExit,
                StmtKind::Continue => Target::LoopBack,
                StmtKind::Return(_) => Target::Return,
                _ => continue,
            };
            if let Some((_, tail)) = chain {
                self.connect(tail, target, None);
            }
            return BlockOutcome::Diverts(chain.map(|(first, _)| first), target);
        }
        match chain {
            Some((entry, tail)) => BlockOutcome::Flows(entry, tail),
            None => BlockOutcome::Empty,
        }
    }

    /// Build one statement's node(s), returning `(entry, tail)`. `None` means
    /// it draws nothing here: jumps are edges, and `def`/`class` get their
    /// own `Activity`.
    fn stmt(&mut self, s: &Stmt) -> Option<(NodeId, NodeId)> {
        match &s.kind {
            StmtKind::Assign(..)
            | StmtKind::AnnAssign { .. }
            | StmtKind::SetIndex { .. }
            | StmtKind::SetAttr { .. }
            | StmtKind::UnpackAssign { .. }
            | StmtKind::Expr(_) => {
                // Desugared statements have no source the student wrote.
                if s.span == (0, 0) {
                    return None;
                }
                let n = self.push(ActivityNode::Action(action_span(s)), s.line);
                Some((n, n))
            }
            StmtKind::Return(Some(_)) => {
                let n = self.push(ActivityNode::Action(action_span(s)), s.line);
                Some((n, n))
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => Some(self.build_if(s.line, cond, body, elifs, else_body.as_deref())),
            StmtKind::While { cond, body } => {
                Some(self.build_loop(s.line, cond.full_span, None, body))
            }
            StmtKind::ForEach { iterable, body, .. } => {
                let header = (s.span.0, iterable.full_span.1);
                Some(self.build_loop(s.line, header, None, body))
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
                header,
            } => {
                let setup = LoopSetup {
                    var: var.clone(),
                    start: start.span,
                    end: end.span,
                    step: step.span,
                };
                Some(self.build_loop(s.line, *header, Some(setup), body))
            }
            StmtKind::Return(None)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Global(_)
            | StmtKind::Def { .. }
            | StmtKind::ClassDef { .. }
            | StmtKind::Import(_) => None,
        }
    }

    fn build_loop(
        &mut self,
        line: usize,
        label: Span,
        setup: Option<LoopSetup>,
        body: &[Stmt],
    ) -> (NodeId, NodeId) {
        let mut inner = Builder::default();
        let decision = inner.push(ActivityNode::Decision(label), line);
        let exit = inner.push(ActivityNode::Merge, line);
        let outcome = inner.block(body);
        // An empty body loops straight back to its own decision.
        let into_body = outcome.entry().unwrap_or(Target::Local(decision));
        inner.connect(decision, into_body, Some(Guard::Cond(label)));
        if let BlockOutcome::Flows(_, tail) = outcome {
            inner.connect(tail, Target::Local(decision), None);
        }
        inner.connect(decision, Target::Local(exit), Some(Guard::Else));
        self.structured(Stereotype::Loop, setup, inner.finish(decision, exit), line)
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
        let decision = inner.push(ActivityNode::Decision(cond.full_span), line);
        let merge = inner.push(ActivityNode::Merge, line);
        inner.wire_branch(decision, Guard::Cond(cond.full_span), body, merge);

        let mut last_decision = decision;
        for (econd, ebody) in elifs {
            let d2 = inner.push(ActivityNode::Decision(econd.full_span), econd.line);
            inner.connect(last_decision, Target::Local(d2), Some(Guard::Else));
            inner.wire_branch(d2, Guard::Cond(econd.full_span), ebody, merge);
            last_decision = d2;
        }
        inner.wire_branch(last_decision, Guard::Else, else_body.unwrap_or(&[]), merge);
        self.structured(
            Stereotype::Conditional,
            None,
            inner.finish(decision, merge),
            line,
        )
    }

    fn structured(
        &mut self,
        stereotype: Stereotype,
        setup: Option<LoopSetup>,
        body: Activity,
        line: usize,
    ) -> (NodeId, NodeId) {
        let kind = ActivityNode::Structured {
            stereotype,
            setup,
            body,
        };
        let node = self.push(kind, line);
        (node, node)
    }

    /// Wire `from --guard--> branch --> join`. A branch that ends in a jump
    /// never reaches `join`.
    fn wire_branch(&mut self, from: NodeId, guard: Guard, stmts: &[Stmt], join: NodeId) {
        let outcome = self.block(stmts);
        let into_branch = outcome.entry().unwrap_or(Target::Local(join));
        self.connect(from, into_branch, Some(guard));
        if let BlockOutcome::Flows(_, tail) = outcome {
            self.connect(tail, Target::Local(join), None);
        }
    }
}

/// A simple statement's text, stopping where its value ends so a trailing
/// comment stays out.
fn action_span(s: &Stmt) -> Span {
    let value = match &s.kind {
        StmtKind::Assign(_, value)
        | StmtKind::AnnAssign { value, .. }
        | StmtKind::SetIndex { value, .. }
        | StmtKind::SetAttr { value, .. }
        | StmtKind::UnpackAssign { value, .. }
        | StmtKind::Expr(value)
        | StmtKind::Return(Some(value)) => value,
        _ => return s.span,
    };
    if value.full_span == (0, 0) {
        return s.span;
    }
    (s.span.0, value.full_span.1)
}

/// What a flattened node draws as. `Structured` has no entry because
/// flattening splices it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatKind {
    Initial,
    ActivityFinal,
    Action(Span),
    Decision(Span),
    Merge,
}

/// A node with an id that's unique across the whole flattened graph.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatNode {
    pub id: usize,
    pub kind: FlatKind,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlatEdge {
    pub from: usize,
    pub to: usize,
    pub guard: Option<Guard>,
}

/// A flattened graph with no positions, so any layout can be applied to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatGraph {
    pub nodes: Vec<FlatNode>,
    pub edges: Vec<FlatEdge>,
}

/// A graph plus positions. They're plain `(x, y)` pairs, so a renderer
/// converts them itself and a 3D view can use its own layout instead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatActivity {
    pub nodes: Vec<FlatNode>,
    pub edges: Vec<FlatEdge>,
    pub positions: HashMap<usize, (f64, f64)>,
}

/// Pass 2 with the default 2D layout.
pub fn flatten_and_layout(activity: &Activity) -> FlatActivity {
    let graph = flatten(activity);
    let positions = layout(&graph, |_| DEFAULT_WIDTH);
    FlatActivity {
        nodes: graph.nodes,
        edges: graph.edges,
        positions,
    }
}

/// Flatten `activity` and its nested bodies into one graph.
///
/// A `Structured` node is spliced out: edges into it land on its body's
/// entry and edges out of it leave from its body's exit. Anything
/// unreachable from the start is then dropped, such as the `Merge` of an
/// `if` whose branches all `return`.
pub fn flatten(activity: &Activity) -> FlatGraph {
    let mut f = Flattener::default();
    let placed = f.walk_activity(activity, Role::Function, Jumps::default());
    let mut out = f.out;

    let mut reachable = HashSet::from([placed.entry]);
    let mut pending = vec![placed.entry];
    while let Some(id) = pending.pop() {
        for e in out.edges.iter().filter(|e| e.from == id) {
            if reachable.insert(e.to) {
                pending.push(e.to);
            }
        }
    }
    out.nodes.retain(|n| reachable.contains(&n.id));
    out.edges.retain(|e| reachable.contains(&e.from));
    out
}

const ROW_H: f64 = 70.0;
const COL_W: f64 = 180.0;
const DEFAULT_WIDTH: f64 = 140.0;
/// Space between neighbours in a row.
const GAP: f64 = 40.0;

/// Top-to-bottom layered layout. A node's row is its longest path from the
/// start, so a merge sits below all its branches. Its column follows its
/// parents, with `[cond]` left and `[else]` right, then each row is spread
/// out so no two nodes overlap. `width` is how wide the renderer draws a node.
pub fn layout(graph: &FlatGraph, width: impl Fn(&FlatNode) -> f64) -> HashMap<usize, (f64, f64)> {
    let Some(start) = graph.nodes.iter().find(|n| n.kind == FlatKind::Initial) else {
        return HashMap::new();
    };
    let forward = forward_edges(graph, start.id);

    let mut incoming: HashMap<usize, usize> = HashMap::new();
    for e in &forward {
        *incoming.entry(e.to).or_default() += 1;
    }
    let mut row: HashMap<usize, usize> = HashMap::from([(start.id, 0)]);
    let mut ready = vec![start.id];
    while let Some(id) = ready.pop() {
        let next = row[&id] + 1;
        for e in forward.iter().filter(|e| e.from == id) {
            let r = row.entry(e.to).or_default();
            *r = (*r).max(next);
            let left = incoming.get_mut(&e.to).expect("counted above");
            *left -= 1;
            if *left == 0 {
                ready.push(e.to);
            }
        }
    }

    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); row.values().max().map_or(0, |m| m + 1)];
    for n in &graph.nodes {
        rows[row[&n.id]].push(n.id);
    }

    let widths: HashMap<usize, f64> = graph.nodes.iter().map(|n| (n.id, width(n))).collect();
    let mut x: HashMap<usize, f64> = HashMap::new();
    for ids in &rows {
        let mut wanted: Vec<(f64, usize)> = ids
            .iter()
            .map(|&id| {
                let from_parents: Vec<f64> = forward
                    .iter()
                    .filter(|e| e.to == id)
                    .map(|e| x[&e.from] + branch_offset(e))
                    .collect();
                let sum: f64 = from_parents.iter().sum();
                (sum / from_parents.len().max(1) as f64, id)
            })
            .collect();
        wanted.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut right_edge = f64::NEG_INFINITY;
        for (want, id) in wanted {
            let half = widths[&id] / 2.0;
            let at = want.max(right_edge + GAP + half);
            x.insert(id, at);
            right_edge = at + half;
        }
    }

    graph
        .nodes
        .iter()
        .map(|n| (n.id, (x[&n.id], row[&n.id] as f64 * ROW_H)))
        .collect()
}

fn branch_offset(e: &FlatEdge) -> f64 {
    match e.guard {
        Some(Guard::Cond(_)) => -COL_W / 2.0,
        Some(Guard::Else) => COL_W / 2.0,
        None => 0.0,
    }
}

/// Every edge except back-edges, so what's left has no cycles.
fn forward_edges(graph: &FlatGraph, start: usize) -> Vec<&FlatEdge> {
    fn visit<'a>(
        graph: &'a FlatGraph,
        id: usize,
        active: &mut HashSet<usize>,
        done: &mut HashSet<usize>,
        out: &mut Vec<&'a FlatEdge>,
    ) {
        active.insert(id);
        for e in graph.edges.iter().filter(|e| e.from == id) {
            if active.contains(&e.to) {
                continue;
            }
            out.push(e);
            if !done.contains(&e.to) {
                visit(graph, e.to, active, done, out);
            }
        }
        active.remove(&id);
        done.insert(id);
    }
    let mut out = Vec::new();
    visit(
        graph,
        start,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut out,
    );
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Function,
    Loop,
    Conditional,
}

/// Global ids that jumps resolve to. A loop body sets `loop_ids`, so `break`
/// binds to the nearest loop. `fn_final` is set once and never replaced.
/// Either is `None` only for a jump outside any loop or function, which
/// isn't valid Python, so that edge is skipped.
#[derive(Clone, Copy, Default)]
struct Jumps {
    /// `(decision, exit)`
    loop_ids: Option<(usize, usize)>,
    fn_final: Option<usize>,
}

impl Jumps {
    fn resolve(&self, target: Target) -> Option<usize> {
        match target {
            Target::Local(_) => None,
            Target::LoopExit => self.loop_ids.map(|(_, exit)| exit),
            Target::LoopBack => self.loop_ids.map(|(decision, _)| decision),
            Target::Return => self.fn_final,
        }
    }
}

struct Placed {
    entry: usize,
    exit: usize,
}

/// State for walking one `Activity`. `ids` is per-`Activity` because local
/// ids mean nothing outside it.
struct Walk<'a> {
    activity: &'a Activity,
    ids: HashMap<NodeId, usize>,
    visited: HashSet<NodeId>,
    jumps: Jumps,
}

#[derive(Default)]
struct Flattener {
    next_id: usize,
    out: FlatGraph,
}

impl Flattener {
    fn walk_activity(&mut self, activity: &Activity, role: Role, jumps: Jumps) -> Placed {
        let mut walk = Walk {
            activity,
            ids: HashMap::new(),
            visited: HashSet::new(),
            jumps,
        };
        let entry = self.global_id(&mut walk, activity.initial);
        let exit = self.global_id(&mut walk, activity.exit);
        match role {
            Role::Function => walk.jumps.fn_final = Some(exit),
            Role::Loop => walk.jumps.loop_ids = Some((entry, exit)),
            Role::Conditional => {}
        }
        self.walk_node(&mut walk, activity.initial);
        Placed { entry, exit }
    }

    fn global_id(&mut self, walk: &mut Walk, local: NodeId) -> usize {
        if let Some(&id) = walk.ids.get(&local) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        walk.ids.insert(local, id);
        let node = &walk.activity.nodes[local];
        let kind = match node.kind {
            ActivityNode::Initial => Some(FlatKind::Initial),
            ActivityNode::ActivityFinal => Some(FlatKind::ActivityFinal),
            ActivityNode::Action(span) => Some(FlatKind::Action(span)),
            ActivityNode::Decision(span) => Some(FlatKind::Decision(span)),
            ActivityNode::Merge => Some(FlatKind::Merge),
            ActivityNode::Structured { .. } => None,
        };
        if let Some(kind) = kind {
            self.out.nodes.push(FlatNode {
                id,
                kind,
                line: node.line,
            });
        }
        id
    }

    /// Emit `local` and everything reachable after it in this `Activity`.
    fn walk_node(&mut self, walk: &mut Walk, local: NodeId) {
        let id = self.global_id(walk, local);
        if !walk.visited.insert(local) {
            return; // a back-edge, or a shared Merge
        }
        let activity = walk.activity;

        let mut from = id;
        if let ActivityNode::Structured {
            stereotype, body, ..
        } = &activity.nodes[local].kind
        {
            let role = match stereotype {
                Stereotype::Loop => Role::Loop,
                Stereotype::Conditional => Role::Conditional,
            };
            let placed = self.walk_activity(body, role, walk.jumps);
            // Every edge into this node was pushed before it was walked.
            for e in self.out.edges.iter_mut().filter(|e| e.to == id) {
                e.to = placed.entry;
            }
            from = placed.exit;
        }

        for e in activity.edges.iter().filter(|e| e.from == local) {
            let to = match e.to {
                Target::Local(next) => Some(self.global_id(walk, next)),
                jump => walk.jumps.resolve(jump),
            };
            if let Some(to) = to {
                self.out.edges.push(FlatEdge {
                    from,
                    to,
                    guard: e.guard.clone(),
                });
            }
            // A jump's target is walked by the `Activity` that owns it.
            if let Target::Local(next) = e.to {
                self.walk_node(walk, next);
            }
        }
    }
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
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
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
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        let decision = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::Decision(_)))
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
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
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
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
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
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }

    #[test]
    fn break_inside_a_counted_for_resolves_to_its_exit() {
        // The same cross-Activity-boundary case already covered for
        // while/for-each, now exercised on the newly-built counted for.
        let a = build_src(
            "for i in range(10):\n    if arr[i] == target:\n        found = i\n        break\n",
        );
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let exit_id = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, FlatKind::Merge) && {
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let fin_id = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::ActivityFinal))
            .unwrap()
            .id;
        let if_decision = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::Decision(_)))
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let fin_id = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::ActivityFinal))
            .unwrap()
            .id;
        let returns: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Action(_)))
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
        assert_eq!(froms.len(), 2, "nothing else reaches the final");
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node still gets placed");
        let exit_id = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::Merge))
            .unwrap()
            .id;
        let decision_ids: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Decision(_)))
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
        let loop_decision = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, FlatKind::Decision(_)) && {
                    // the loop's own decision is the one with a self/back edge
                    // into it from somewhere, as opposed to the if's decision
                    edges.iter().any(|e| e.to == n.id && e.from != n.id)
                }
            })
            .map(|n| n.id);
        assert!(loop_decision.is_some());
        let if_decisions: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Decision(_)) && Some(n.id) != loop_decision)
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node gets placed");
        let exit_id = nodes
            .iter()
            .find(|n| {
                matches!(n.kind, FlatKind::Merge) && {
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
        let FlatActivity {
            nodes,
            edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len(), "every node gets placed");
        // Two loops, two Decisions with a back-edge each, two Merges. The
        // OUTER loop's Merge is the one reachable from the no-swaps `if`.
        let decisions: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Decision(_)))
            .map(|n| n.id)
            .collect();
        assert_eq!(
            decisions.len(),
            4,
            "outer while, inner while, the swap-check if, the no-swaps if"
        );
        let merges: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Merge))
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

    /// The text of every `Decision` and `Action`, in graph order.
    fn labels(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for named in build_program(&parser::parse(&crate::lexer::lex(src).unwrap()).unwrap()) {
            for n in flatten_and_layout(&named.activity).nodes {
                if let FlatKind::Action(span) | FlatKind::Decision(span) = n.kind {
                    out.push(src[span.0..span.1].to_string());
                }
            }
        }
        out
    }

    #[test]
    fn decision_labels_are_the_whole_condition() {
        for cond in [
            "x == 1",
            "n % 2 == 0",
            "len(xs) > 0",
            "a and b",
            "a or b",
            "x in xs",
            "x not in xs",
            "foo(x)",
            "not done",
            "(a + b) * c > 0",
            "1 < x < 9",
            "a + 1 if c else d",
        ] {
            let src = format!("if {cond}:\n    y = 1\n");
            assert!(labels(&src).contains(&cond.to_string()), "{cond}");
            let src = format!("while {cond}:\n    y = 1\n");
            assert!(labels(&src).contains(&cond.to_string()), "{cond}");
        }
    }

    #[test]
    fn elif_decisions_have_their_own_text_and_line() {
        let src = "if a:\n    x = 1\nelif b == 2:\n    x = 2\n";
        assert!(labels(src).contains(&"b == 2".to_string()));
        let stmts = parser::parse(&crate::lexer::lex(src).unwrap()).unwrap();
        let flat = flatten_and_layout(&build(&stmts));
        let lines: Vec<usize> = flat
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Decision(_)))
            .map(|n| n.line)
            .collect();
        assert!(lines.contains(&1) && lines.contains(&3), "{lines:?}");
    }

    #[test]
    fn loop_decisions_show_the_whole_header() {
        for header in [
            "for i in range(1, 6, 2)",
            "for i in range(n - 1)",
            "for i in range(len(xs))",
            "for x in xs",
        ] {
            let src = format!("{header}:\n    y = 1\n");
            assert!(labels(&src).contains(&header.to_string()), "{header}");
        }
    }

    #[test]
    fn action_labels_are_the_statement_without_its_comment() {
        assert_eq!(labels("x = 1  # set it\n"), vec!["x = 1"]);
        assert_eq!(labels("xs[0] = y + 1  # note\n"), vec!["xs[0] = y + 1"]);
        assert_eq!(labels("print(x)  # show\n"), vec!["print(x)"]);
    }

    #[test]
    fn return_labels_include_the_keyword_and_whole_value() {
        let got = labels("def f(a):\n    return a + 1\n");
        assert!(got.contains(&"return a + 1".to_string()), "{got:?}");
        let got = labels("def f(a):\n    return a.b, 2\n");
        assert!(got.contains(&"return a.b, 2".to_string()), "{got:?}");
    }

    #[test]
    fn no_two_nodes_share_a_position() {
        let src = "if a:\n    if b:\n        p = 1\n    else:\n        q = 2\n\
                   else:\n    if c:\n        r = 3\n    else:\n        s = 4\n";
        let flat = flatten_and_layout(&build_src(src));
        let mut seen = std::collections::HashSet::new();
        for n in &flat.nodes {
            let (x, y) = flat.positions[&n.id];
            assert!(
                seen.insert((x.to_bits(), y.to_bits())),
                "overlap at {x},{y}"
            );
        }
    }

    #[test]
    fn a_merge_sits_below_both_of_its_branches() {
        let flat = flatten_and_layout(&build_src(
            "if a:\n    x = 1\n    y = 2\n    z = 3\nelse:\n    w = 4\nv = 5\n",
        ));
        let y_of = |id: usize| flat.positions[&id].1;
        let merge = flat
            .nodes
            .iter()
            .find(|n| n.kind == FlatKind::Merge)
            .unwrap()
            .id;
        assert!(
            flat.edges
                .iter()
                .filter(|e| e.to == merge)
                .all(|e| y_of(e.from) < y_of(merge))
        );
    }

    #[test]
    fn only_a_loop_s_back_edge_points_upward() {
        let flat = flatten_and_layout(&build_src(
            "while x > 0:\n    if x > 5:\n        break\n    x = x - 1\nz = 1\n",
        ));
        let y_of = |id: usize| flat.positions[&id].1;
        let up: Vec<_> = flat
            .edges
            .iter()
            .filter(|e| y_of(e.to) <= y_of(e.from))
            .collect();
        assert_eq!(up.len(), 1, "{up:?}");
    }

    #[test]
    fn layout_is_separate_from_the_graph() {
        let graph = flatten(&build_src("if a:\n    x = 1\n"));
        assert_eq!(layout(&graph, |_| 100.0).len(), graph.nodes.len());
    }

    #[test]
    fn wide_nodes_in_a_row_do_not_touch() {
        let graph = flatten(&build_src("if a:\n    x = 1\nelse:\n    y = 2\n"));
        let w = 600.0;
        let pos = layout(&graph, |_| w);
        let mut rows: HashMap<u64, Vec<f64>> = HashMap::new();
        for (x, y) in pos.values() {
            rows.entry(y.to_bits()).or_default().push(*x);
        }
        for xs in rows.values_mut() {
            xs.sort_by(f64::total_cmp);
            assert!(xs.windows(2).all(|p| p[1] - p[0] >= w), "{xs:?}");
        }
    }

    #[test]
    fn desugared_statements_draw_no_blank_boxes() {
        for src in [
            "a = b = 1\n",
            "with open(\"f\") as fh:\n    y = 1\n",
            "del xs[0], xs[1]\n",
        ] {
            let got = labels(src);
            assert!(got.iter().all(|l| !l.is_empty()), "{src}: {got:?}");
            assert!(!got.is_empty(), "{src}");
        }
        assert!(labels("with open(\"f\") as fh:\n    y = 1\n").contains(&"y = 1".to_string()));
    }

    /// Every id reachable from the `Initial` node by following flat edges.
    fn reached(nodes: &[FlatNode], edges: &[FlatEdge]) -> std::collections::HashSet<usize> {
        let start = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::Initial))
            .unwrap()
            .id;
        let mut seen = std::collections::HashSet::from([start]);
        let mut pending = vec![start];
        while let Some(id) = pending.pop() {
            for e in edges.iter().filter(|e| e.from == id) {
                if seen.insert(e.to) {
                    pending.push(e.to);
                }
            }
        }
        seen
    }

    #[test]
    fn every_flat_node_is_connected_from_start() {
        for src in [
            "if x > 0:
    y = 1
else:
    y = 2
z = 3
",
            "while x > 0:
    x = x - 1
z = 3
",
            "for i in range(3):
    if i > 1:
        break
    y = i
z = 3
",
            "for v in xs:
    if v:
        continue
    y = v
z = 3
",
        ] {
            let FlatActivity {
                nodes,
                edges,
                positions,
            } = flatten_and_layout(&build_src(src));
            assert_eq!(reached(&nodes, &edges).len(), nodes.len(), "{src}");
            assert_eq!(positions.len(), nodes.len(), "{src}");
            let fin = nodes
                .iter()
                .find(|n| matches!(n.kind, FlatKind::ActivityFinal))
                .unwrap();
            assert!(edges.iter().any(|e| e.to == fin.id), "{src}");
        }
    }

    #[test]
    fn the_branches_of_an_if_lead_to_the_statement_after_it() {
        let FlatActivity {
            nodes,
            edges,
            positions: _,
        } = flatten_and_layout(&build_src(
            "if x > 0:
    y = 1
else:
    y = 2
z = 3
",
        ));
        let merge = nodes
            .iter()
            .find(|n| matches!(n.kind, FlatKind::Merge))
            .unwrap()
            .id;
        let actions: Vec<usize> = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Action(_)))
            .map(|n| n.id)
            .collect();
        assert_eq!(actions.len(), 3);
        assert_eq!(edges.iter().filter(|e| e.to == merge).count(), 2);
        let after = edges.iter().find(|e| e.from == merge).unwrap().to;
        assert!(actions.contains(&after), "the merge continues to z = 3");
    }

    #[test]
    fn code_after_an_if_whose_branches_all_return_is_not_drawn() {
        let FlatActivity {
            nodes,
            edges: _,
            positions: _,
        } = flatten_and_layout(&build_src(
            "if x:
    return 1
else:
    return 2
z = 3
",
        ));
        assert!(!nodes.iter().any(|n| matches!(n.kind, FlatKind::Merge)));
        let actions = nodes
            .iter()
            .filter(|n| matches!(n.kind, FlatKind::Action(_)))
            .count();
        assert_eq!(actions, 2, "z = 3 can never run");
    }

    #[test]
    fn a_final_reached_only_by_return_sits_below_the_rest_of_the_chart() {
        let FlatActivity {
            nodes,
            edges: _,
            positions,
        } = flatten_and_layout(&build_src(
            "y = 1
return y
",
        ));
        let y_of = |pick: fn(&FlatKind) -> bool| {
            positions[&nodes.iter().find(|n| pick(&n.kind)).unwrap().id].1
        };
        let fin = y_of(|k| matches!(k, FlatKind::ActivityFinal));
        let start = y_of(|k| matches!(k, FlatKind::Initial));
        assert!(fin > start);
        assert!(positions.values().all(|p| p.1 <= fin));
    }

    #[test]
    fn break_outside_any_loop_is_skipped_not_a_crash() {
        // Not valid Python, but shouldn't panic the flowchart view either.
        let a = build_src("x = 1\nbreak\ny = 2\n");
        let FlatActivity {
            nodes,
            edges: _edges,
            positions,
        } = flatten_and_layout(&a);
        assert_eq!(positions.len(), nodes.len());
    }
}
