# Perceus reuse tier — implementation plan (onboarding for the compiler hire)

**Status: prep done, analysis not started.** This is the staging, the invariants,
and the acceptance contract for the precise-RC + reuse work (native/Pico backend
only — the browser uses WASM-GC). Read alongside `MEMORY_MANAGEMENT.md` (the
research + tiers) and `PICO_BACKEND.md` (the value model).

## TL;DR for the new compiler person

The foundation is built and correct: the emitter (`src/llvm.rs`) inserts
`retain`/`release` (naive, scope-end), the runtime (`runtime/src/lib.rs`) is
RC-correct with a `[tag][refcount][len][data]` object layout, and there's a
`live == 0` run-oracle. **Your job is the analysis the naive emitter is missing**,
in this order:

1. **Last-use analysis** (the keystone) — backward liveness over the AST so a
   value can be released at its last use, not at scope end.
2. **Precise drops** — move releases from scope-end to last-use, using (1).
3. **General drop-reuse** — pair a death with a following same-size allocation →
   in-place update (FBIP). Generalize the one shipped special case.
4. **Escape / borrowed-param inference** (tier 2) and **cycle handling** (tier 5)
   — later.

**Acceptance contract (non-negotiable):** every change keeps `tools/native_run.sh`
green — output matches CPython **and** `live == 0` (no leaks). Use-after-free
shows up as a wrong/garbage output or a trap (the oracle caps runtime). Reuse
*wins* are measured by `tools/reuse_bench.sh` (allocation counts), not guessed.

## What already exists (don't rebuild)

- **Emitter RC pass** (`llvm.rs`): transfer-based model, documented in the big
  comment near the top (~line 261). Owned slots `+1`; every `ret` releases live
  locals (`emit_exit_releases`); reassignment releases the old value; borrowed
  params aren't released. **Naive: releases at scope end, no last-use precision**
  (`llvm.rs:287`).
- **Runtime** (`runtime/src/lib.rs`): `p2w_retain`/`p2w_release`, the unique test
  (`refcount == 1`), `p2w_live()` (births − frees) and `p2w_allocs()` (total
  births) for the oracle/bench, and the heap object layout.
- **One reuse case**: `try_inplace_map` in `llvm.rs` — `data = [f(x) for x in data]`
  becomes an in-place map *when the array is uniquely owned at runtime*
  (`if unique(data)` → overwrite, zero allocations; copy otherwise). **This is the
  pattern to generalize**, lifted from a hand-written special case to a result of
  the last-use + reuse analysis.
- **The acceptance net**: `tools/native_run.sh` (correctness + `live == 0`,
  `GATE_LEAKS=1` by default), with a rich corpus incl. RC stress, borrowed
  params, the FBIP unique/alias cases, and comprehensions.

## The seam you fill in (`src/reuse.rs`)

`reuse.rs` holds the analysis scaffold, decoupling the emitter from the algorithm:

- `vars_read(expr)` / `vars_assigned(stmt)` — correct, tested AST primitives
  (syntactic name occurrences) to build dataflow on.
- `Liveness::analyze(body)` — **currently conservative** (nothing dies before
  scope end), which is exactly today's emitter behavior. `dead_after(idx)` returns
  the bindings whose last use is statement `idx` (empty in the stub).

**Integration plan:** the emitter, after emitting statement `idx` of a body,
releases the heap-typed bindings in `dead_after(idx)` *and* removes them from the
scope-exit release set (so no double-release). With the conservative stub that set
is always empty → no behavior change → `live == 0` stays green. You replace the
body of `analyze` with real backward liveness; the emitter and oracle don't move.
(The scaffold is a starting shape, not frozen — refine the interface if the
algorithm needs richer granularity, e.g. per-block / per-use-site.)

### Algorithm notes (starting points, deep-read first)

- Backward dataflow: a var is *live* at a point if some path from there reads it
  before reassignment. Last use = the point after which it's no longer live.
- Mind the non-trivial scopes the primitives don't resolve for you:
  comprehension-bound vars (`for v in it`) are local to the comprehension; function
  params are pre-bound (the borrowed-param convention already exists); loop bodies
  re-enter (a var read at the top of a loop is live across the back-edge).
- Reuse (step 3): at a death site, if the next allocation is the same size class,
  hand the freed cell straight to it (Perceus reuse). The runtime's first-fit free
  list + `refcount == 1` test already support in-place mutation; the `try_inplace_map`
  branch shows the shape (`if unique(x)` → overwrite).
- Papers (in `MEMORY_MANAGEMENT.md`): Perceus (PLDI'21) for dup/drop + reuse;
  Free-to-Move / Reachability Types for the static escape tier; Tree Borrows for
  the in-place-mutation soundness model.

## Measuring wins

`tools/reuse_bench.sh` compiles reuse-target programs, runs them, and reports
`P2W_ALLOCS` (total allocations) and `P2W_LIVE` (must be 0) per case. The
**wishlist** cases at the bottom are programs that *should* reuse but don't yet —
their alloc counts are the baseline to beat. "Reuse works" = those numbers drop
while `native_run.sh` stays green.

## Scope reminder

Native/Pico only. The browser backend (`codegen.rs` → WASM-GC) needs none of this
— the engine collects. The Layer-3 reuse *visualization* (see `RICH_OUTPUT.md`)
renders this analysis's *decisions* (backend-agnostic), so it can consume
`reuse.rs` output regardless of which backend runs.
