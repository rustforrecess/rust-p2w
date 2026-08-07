# The consumer boundary — CausalCode and this repo

> **Scope: what THIS repo guarantees to a downstream causal-tutoring consumer,
> and why the licence boundary sits where it does.**
>
> The list of hooks CausalCode still wants lives in that project
> (`education/causalcode/docs/next-steps.md`), because it is a request *from*
> there. Keeping it here is what let it go stale: this file claimed "nothing
> built" long after `src/roles.rs` shipped the largest item on it, and nobody
> reads a p2w design record when a p2w feature lands.

## The boundary (read first — it's load-bearing)

**rust-p2w stays permissive (MIT today, Apache-2.0 per
`docs/LICENSING_PROPOSAL.md`). CausalCode is GPL-3.0.** Therefore:

- CausalCode is a **separate, downstream consumer** of this crate's exports — it
  does **not** get merged into this tree. Putting GPL code in `rust-p2w/` would
  make the whole component copyleft and defeat the reach-and-company-friendly
  strategy that is the reason for going permissive rather than AGPL.
- Direction of dependence is one-way: **CausalCode → rust-p2w.**

Both projects have the same author, so this is a *chosen* boundary rather than
an imposed one — which makes it easier to erode by accident and worth restating
here.

## Two surfaces, one shared substrate

```
        rust-p2w  (MIT — this repo: AST, reuse/liveness, Vm, evidence)
       /                                              \
acornstem-ide (AGPL-3.0 — product/UI)         CausalCode (GPL-3.0 — tutor engine)
       \____ CausalCode surfaces in the IDE ________/
```

- **rust-p2w** produces the causal facts — the *recognition* input.
- **acornstem-ide** hosts the *interaction* — rendering, debugger highlight,
  run/activity ECD, audio.
- acornstem-ide is **AGPL-3.0** and CausalCode is **GPL-3.0** — compatible
  (GPLv3 §13), so those two integrate tightly and the combined product is AGPL.
  This repo stays **MIT** and keeps the copyleft on the far side of its API.

## What this repo provides

The payoff of owning the whole compiler: most of what a causal-tutoring layer
needs is already computed here, and its job is largely to *read* it.

| need | provided by |
|---|---|
| Structural representation | `src/ast.rs` — typed, spanned AST; every node carries its source line, and `PartialEq` is **structure-only** ("equal when they mean the same thing") |
| Lifetime / reuse facts | `src/reuse.rs` — Perceus last-mention **liveness** plus rc/drop analysis (computed internally; **not yet exported**) |
| Execution trace | `src/debug.rs` — the `Vm`, a resumable step-interpreter with per-statement hooks, scope inspection and watchpoints (per-step value diff = state-change detection) |
| Concept evidence | `src/evidence.rs` — which CS concepts a program exercises (loop, nested_loop, recursion, comprehension, indexing…), as automatic evidence for an ECD model |
| **Variable roles** | `src/roles.rs` — deterministic Roles-of-Variables classification over the AST, with `variable_roles` and a `_verified` variant |
| Capability manifest | `capabilities()` — every host function a program can reach |
| Per-node highlight | the debugger highlights the current line and block per step |

## Constraints on anything added for a consumer

- **Generic, not CausalCode-specific.** Every hook is a plain export; no GPL and
  no CausalCode types cross the boundary.
- **Debug/analysis profile only.** Trace and role labelling ride the
  debug/analysis path; Run stays the fast optimised backend — the same split the
  debugger already uses.
- **Semantics stay single-source.** The `Vm` is the reference oracle against the
  CPython corpus, so causal facts inherit that guarantee.
- Each export must be **independently useful to this repo**, so that none of it
  depends on CausalCode existing.
