# Perceus reuse tier — implementation plan (onboarding for the compiler hire)

**Status: steps 1–2 LANDED (last-mention liveness + precise drops); step 3
LANDED for the map-chain case (dying-source drop-reuse); rest open.** This is the staging, the invariants, and the acceptance contract for the
precise-RC + reuse work (native/Pico backend only — the browser uses WASM-GC).
Read alongside `MEMORY_MANAGEMENT.md` (the research + tiers) and
`PICO_BACKEND.md` (the value model).

## TL;DR for the new compiler person

The foundation is built and correct: the emitter (`src/llvm.rs`) inserts
`retain`/`release`, the runtime (`runtime/src/lib.rs`) is RC-correct with a
`[tag][refcount][len][data]` object layout, and there's a `live == 0`
run-oracle. The staging, and where it stands:

1. **Last-use analysis** — ✅ landed as **last-mention liveness** in
   `src/reuse.rs` (statement granularity; assignments count as mentions so an
   early release can never precede a reassignment's release-of-old — no double
   free; def/class bodies pin their reads; loops are opaque units). Upgrading to
   full backward liveness (early release *before* a reassignment) is open.
2. **Precise drops** — ✅ landed: `block_precise`/`early_releases` in `llvm.rs`
   release heap slots right after their last mention and zero the slot (so the
   scope-exit release stays a no-op). Applied to function-body top level +
   `main` (nested bodies stay scope-end). Measured: `wl_chain` peak live objects
   4 → 3 (`p2w_peak` watermark).
3. **General drop-reuse** — ✅ landed for the flagship case: `try_reuse_map` in
   `llvm.rs` lowers `dst = [f(x) for x in src]` *where src dies at that
   statement* (its token from the liveness) to an in-place map over src's
   buffer when unique at runtime, transferring the buffer to dst — zero
   allocation. Guards: tokens are top-level-statement-only (`stmt()` takes
   `self.dying` on entry so loop bodies never see one), borrowed params are
   never stolen (rc==1 is the caller's count), and the element must pass a
   conservative syntactic type whitelist (`elem_matches_repr`) since dst may
   be unannotated. Measured: `wl_chain` 10 → 3 allocs, peak 3 → 1 — a 3-stage
   pipeline runs in ONE buffer. **Also landed:** assign-site literal reuse
   (`try_reuse_literal`): `xs = [lit…]` over an existing slot overwrites the
   dead old collection in place when a runtime `p2w_can_reuse_*` guard passes
   (tag + unique + exact length — the tag test keeps a Boxed slot holding a
   string/tuple safe); element writes are synthesized `SetIndex` statements so
   boxed and packed slots keep normal transfer semantics. Measured:
   `wl_realloc` 6 → 2 allocs, peak 2 → 1. **And append/extend reuse**
   (`try_add_assign` + runtime `p2w_add_assign`): `x = x + e` consumes the old
   x — a unique string grows in place inside its block's spare capacity (the
   allocator's size header = free capacity metadata), realloc'ing with 2×
   slack when full (amortized O(1), the CPython refcount-1 trick); a unique
   list extends in place. The runtime guards (`rc == 1` + `a != b`) are
   *complete* — any other live reference implies rc ≥ 2 — so no emitter-side
   expression restrictions are needed. Measured: `wl_concat` 17 → 10 allocs
   (the rest are per-iteration suffix-literal allocations). **Still open in
   step 3:** literal hoisting/interning (the `wl_concat` remainder), widening
   the element whitelist with real type inference, and reuse across further
   statement shapes.
4. **Escape / borrowed-param inference** (tier 2) and **cycle handling** (tier 5)
   — later. Cycles are the gate for making linear-memory the safe default in the
   browser/component build (see `acornstem/ACTIVITY_INTERFACE.md`).

**Acceptance contract (non-negotiable):** every change keeps `tools/native_run.sh`
green — output matches CPython **and** `live == 0` (no leaks), incl. the
`drop_*` adversarial cases (alias-into-container, freed-cell reuse, in-function).
Use-after-free shows up as wrong/garbage output or a trap (the oracle caps
runtime). Wins are measured by `tools/reuse_bench.sh` — **allocs** (drop-reuse's
metric) and **peak** (precise drops' metric) — not guessed.

**Third net — differential fuzzing:** `tools/fuzz_native.sh` (with
`tools/gen_program.py`, dependency-free, seeded/reproducible) generates
programs that are *safe by construction* (magnitude-tracked ints, non-negative
`//`/`%` operands, ASCII strings, in-bounds indexing, bounded loops) and
*weighted at the reuse machinery* (comp chains, literal reassign, append,
aliases), then diffs CPython vs native output and gates `live == 0` per seed.
Any DIFF/LEAK is a real finding with a one-command repro
(`python3 tools/gen_program.py <seed>`). Run a large batch after any
reuse-path change: `FUZZ_N=200 tools/fuzz_native.sh`.

## What already exists (don't rebuild)

- **Emitter RC pass** (`llvm.rs`): transfer-based model, documented in the big
  comment near the top. Owned slots `+1`; every `ret` releases live locals
  (`emit_exit_releases`); reassignment releases the old value; borrowed params
  aren't released. **Plus precise drops** (`block_precise`/`early_releases`):
  top-level heap slots are released at their last mention, slot zeroed so exit
  releases stay no-ops.
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
