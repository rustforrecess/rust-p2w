# Perceus reuse tier — implementation plan (onboarding for the compiler hire)

**Status: steps 1–3 LANDED — the original reuse wishlist is CLOSED, plus two
frontier-task-6 stretch shapes** (last-mention liveness, precise drops, and
drop-reuse in six forms: dying-source maps, literal reassignment,
append/extend growth, interned literals, slice-steal, and reuse across
if/else join points — `wl_chain` 10→3 allocs, `wl_realloc` 6→2, `wl_concat`
17→4, `wl_slice` 11→2, `wl_branch` 6→3).
Open: full backward liveness, type inference, escape inference, cycles
(`COMPILER_FRONTIER.md`). This is the staging, the invariants, and the acceptance contract for the
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
   (the rest were per-iteration suffix-literal allocations). **And interned
   literals**: every string-literal SITE gets a lazily-filled module-global
   cache (loop literals materialize once; `main` frees the cache at exit so
   `live == 0` stays exact; the cache's permanent +1 doubles as the mutation
   guard — a cached literal can never be rc==1 in a consumer's hands, so
   in-place growth can't touch it). `wl_concat` now 17 → **4** allocs (peak
   3 → 4: pinned literals count toward peak — churn collapsed, the right
   trade on-device). **And slice-steal** (`try_slice_assign` + runtime
   `p2w_slice_assign`): `s = s[1:]` / `xs = xs[1:]` (the assign kills the old
   value) and `ys = xs[a:b]` over a dying source consume the source — a
   unique string compacts its bytes in place, a unique list keeps the taken
   elements and releases the dropped ones (in-place only for `step >= 1`,
   where write index `j` never passes read index `start + j*step`; reversal
   and aliases fall back to copy + release). The peel loop (`wl_slice`) went
   11 → **2** allocs — the whole loop runs in one buffer (the 2 = the interned
   literal + one first-iteration copy: the cache's pin correctly refuses to
   mutate the literal itself). **And reuse across if/else join points**
   (`arm_block` in `llvm.rs`): a token whose name's last mention is an `if`
   statement is re-placed at the name's last mention inside EACH mutually
   exclusive arm (`stmt_mentions_name`), so the taken branch's comprehension
   or slice steals the buffer; every consuming/releasing path zeroes the
   slot, so the join-point early release no-ops where the value already died
   and does the real release on paths (untaken conds, missing else) that
   never dropped it. `wl_branch` went 6 → **3** allocs, peak 2 → 1. **The
   original wishlist is fully closed and two of frontier task 6's stretch
   shapes are in.** **And type inference (frontier task 3, both halves):**
   `infer_expr_repr` (literals + typed slots + annotated signatures + `len` +
   packed indexing + numeric promotion) REPLACED the syntactic element
   whitelist at the reuse-map gate — typed-call elements
   (`[dbl(x) for x in a]`, `dbl -> int`) now steal the dying buffer, and the
   whitelist's int-literal-into-float-buffer hole (`[7 for x in floats]`
   printed `7.0`, CPython prints `7`) is closed. `infer_slot_reprs` gives
   unannotated scalar locals raw Int/Float slots by a fixpoint join over
   every binding (assigns, loop vars, unpack targets), demoting to Boxed on
   any disagreement — type churn (`x = 1; x = "hi"`) and int/float mixing
   keep today's boxed path and CPython-identical output. **Still open in
   step 3:** container slot inference (`xs = [1, 2, 3]` → packed — needs
   mutation-site constraints), dict-comprehension reuse, and append-then-die
   builders (which want task 2's full liveness).
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
