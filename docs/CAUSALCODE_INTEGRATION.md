# CausalCode integration — the compiler-side plan

> Design record. How **CausalCode** (a neurosymbolic causal-tutoring engine —
> `C:\Code\education\causalcode`, GPL-3.0) will eventually plug into rust-p2w.
> **Status: nothing built.** This records what CausalCode needs *from* the
> compiler and the boundary that keeps it clean. Companion to
> `DEBUGGER_ARCHITECTURE.md`, `MEMORY_MANAGEMENT.md` / `docs/REUSE_PLAN.md`, and
> `src/evidence.rs`. CausalCode's own design lives in that repo's
> `docs/pattern-recognition.md` and `docs/adaptive-engine.md`.

## The boundary (read first — it's load-bearing)

**rust-p2w stays permissive (MIT today, Apache-2.0 per `docs/LICENSING_PROPOSAL.md`).
CausalCode is GPL-3.0.** Therefore:

- CausalCode is a **separate, downstream consumer** of rust-p2w's hooks — it does
  **not** get merged into this tree. Putting GPL code in `rust-p2w/` would make
  the whole component copyleft and defeat the "reach + company-friendly" strategy
  (the reason we're going *permissive*, not AGPL).
- Direction of dependence: **CausalCode → rust-p2w** (GPL consumes MIT/Apache,
  which is allowed one-way). rust-p2w gains **no** dependency on CausalCode and
  builds/ships without it.
- So the hooks below must be **generic** — useful to *any* consumer (evidence
  export, a trace stream, a liveness query), not "CausalCode mode." That keeps
  rust-p2w a clean reusable component and keeps the GPL on the far side of the
  API.
- Keep p2w's MIT notice in `NOTICE` regardless (rust-p2w derives from it).

Mirrors the split already recorded on the CausalCode side (Loom is Apache; the
reusable pieces stay permissive).

## Two surfaces, one shared substrate

CausalCode plugs into **two** places, and rust-p2w is the shared dependency of
both:

```
        rust-p2w  (MIT — this repo: AST, reuse/liveness, Vm, evidence)
       /                                              \
acornstem-ide (AGPL-3.0 — product/UI)         CausalCode (GPL-3.0 — tutor engine)
       \____ CausalCode surfaces in the IDE ________/
```

- **rust-p2w** produces the causal facts — the *recognition* input (this doc).
- **acornstem-ide** hosts the *interaction* — rendering, debugger highlight,
  run/activity ECD, and (new) audio (its `CAUSALCODE_INTEGRATION.md`).
- **Licensing note:** acornstem-ide is **AGPL-3.0**, CausalCode is **GPL-3.0** —
  compatible (GPLv3 §13), so those two integrate tightly and the combined product
  is AGPL. rust-p2w (this repo) stays **MIT** and keeps the copyleft on the far
  side of its API — exactly the boundary above.

## What rust-p2w already provides (the substrate)

The payoff of owning the whole compiler: most of what a causal-tutoring layer
needs is already computed here. CausalCode's job is largely to *read* it.

| CausalCode need | Already here |
| --- | --- |
| Structural representation | `src/ast.rs` — typed, spanned AST; every node carries its source line; `PartialEq` is **structure-only** ("equal when they mean the same thing"). |
| Lifetime / reuse facts | `src/reuse.rs` — Perceus last-mention **liveness** + rc/drop analysis. |
| Execution / causal trace | `src/debug.rs` — the **`Vm`**: a resumable step-interpreter with per-statement hooks, scope/variable inspection, and **watchpoints** (per-step value diff = state-change detection). |
| Concept evidence (for a mastery model) | `src/evidence.rs` — already reports *which CS concepts a program exercises* off the AST (loop, nested_loop, recursion, comprehension, indexing, …) as "the system's automatic evidence" for the ECD model. |
| Per-node highlight (for a pointing UI) | the debugger already highlights the current line **and** block per step. |

## The hooks CausalCode needs (generic additions here)

1. **A structured causal trace off the `Vm`.** The stepper already single-steps
   and diffs watched values; expose that as a *stream* (not just UI): per step,
   emit `(stmt/node, reads = REQUIRES, writes/state-changes = CAUSES,
   scope-enter/exit = binding create/destroy)`. This is the "learn causal laws
   from execution traces" substrate — and because the `Vm` is gated by the
   CPython differential corpus, the edges are trustworthy, not inferred.
2. **Extend `evidence.rs` from concept-counts to a causal graph + roles.** It
   already walks the AST emitting concept ids; add (a) **Roles of Variables**
   labelling (stepper / gatherer / most-wanted-holder / … over the AST +
   `reuse.rs` liveness) and (b) plan/pattern identification, emitting a graph
   bound to the same stable concept ids. Keep the existing count-based ECD output
   — it feeds the mastery model.
3. **A liveness/reuse query API.** Surface `reuse.rs`'s last-mention + rc facts
   as a read-only query so a consumer can render the reuse/lifetime view (already
   computed internally; just expose it).
4. **Stable node ids.** Nodes carry line/span; add a stable id so a consumer can
   address/highlight a specific causal node ("the *stepper*, here") and drive the
   line+block highlight from a spoken/rendered turn.
5. **Rendering already exists.** `docs/RICH_OUTPUT.md`'s `emit_html` channel is
   where the overlay draws — no new channel needed.

## Constraints (compiler-side)

- **Generic, not CausalCode-specific.** Every hook is a plain export; no GPL, no
  CausalCode types cross the boundary.
- **Debug/analysis profile only.** The trace + role labelling ride the debug/
  analysis path; Run stays the fast optimized backend (same split as the
  debugger).
- **Semantics stay single-source.** The `Vm` is already the reference oracle
  against the CPython corpus; causal facts inherit that guarantee.

## Cheapest-first sequencing

1. **Trace stream from the `Vm`** — the watchpoint value-diff already exists;
   turning it into a `(reads, writes, scope)` stream is the smallest step and
   unlocks the causal graph immediately.
2. **Role labelling in `evidence.rs`** — over the AST + `reuse.rs` liveness.
3. **Liveness query API** — expose what `reuse.rs` already computes.
4. **Node ids + overlay pointing** — build on the existing line/block highlight.

None of this requires CausalCode to exist yet; each is a standalone,
independently useful export that keeps rust-p2w a clean permissive component.
