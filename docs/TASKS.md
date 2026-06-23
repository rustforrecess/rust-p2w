# Frontier tasks — scoped work for a compiler contributor

Each task below is self-contained, has a published spec, a clean interface, and a
**mechanical acceptance gate**. You do not need to understand the whole system to
land one — you need the AST, the emitter (`src/llvm.rs`), the runtime
(`runtime/src/lib.rs`), and the oracle (`tools/native_run.sh`).

## How work is accepted here (read first)

The native backend is validated **without hardware**: values are i32 arena
offsets (not machine pointers), so emitted LLVM IR + the `p2w-rt` runtime compile
with `clang` and **run on a normal host**.

`tools/native_run.sh` is the gate. For each case it: emits IR → `clang` → links
`p2w-rt` (built `--crate-type staticlib -- -C panic=abort`) + a `p2w_putc` stub →
runs → diffs stdout against the expected (CPython) output → asserts
**`p2w_live() == 0`** at exit (`GATE_LEAKS=1`, the default). A leak (`live > 0`) or
a double-free (`live < 0`, or a trap that spin-loops → the per-case timeout fires)
**fails the build**.

**Definition of done for every task:** all existing oracle cases stay green; new
cases covering the feature are added and green; `cargo test` (lib + runtime)
green; `cargo clippy` clean. Where the task claims a speed/size/alloc win, add a
before/after number from the oracle (it can report alloc and RC-op counts).

Dependency order: **Task 1 is the foundation; 2–4 sit on top of it.** Do not start
2–4 before 1 lands — they target the IR shape Task 1 produces.

---

## Task 1 — Type-driven monomorphization (the foundation)

**Goal.** Stop boxing everything. Use the type annotations the parser already
accepts (`x: int`, `-> T`) to represent annotated scalars **unboxed** (raw
`i32/i64/i16/i8`/`double`) and homogeneous collections as **packed arrays**; box
only at the **dynamic boundary** (a typed value flowing into a dynamic context —
`print`, a heterogeneous list, an un-annotated call).

**Why first.** On a 520 KB Cortex-M33 the per-integer box+arena-bump dominates the
loop-heavy code this language runs. Unboxing is the largest single speed/RAM win
and it erases most remaining RC traffic (boxed-int releases are already no-ops;
this removes the boxes entirely).

**Design before code (write a short note, like the RC contract).** The real work
is the **typed ↔ boxed coexistence contract**: where boxing/unboxing happens, how
typed values cross into dynamic positions, how mixed expressions are typed. Keep
it annotation-driven — *not* a full inference engine. Unannotated code keeps
today's tagged-i32 path unchanged.

**Interface / touch points.**
- A per-expression "representation" (boxed vs a concrete unboxed type), threaded
  through `expr()` in `src/llvm.rs`.
- `box`/`unbox` emission helpers at boundaries; the runtime ABI gains typed
  constructors/extractors as needed.
- RC interacts cleanly: unboxed scalars are never retained/released (no-ops today,
  absent tomorrow); heap values keep the existing model.

**Acceptance.** Oracle green; new typed cases (annotated int loop, packed array
sum, typed→dynamic `print`) green and `live==0`; report the alloc-count and
RC-op drop on an annotated loop vs the unannotated version.

---

## Task 2 — Full last-use release (Perceus precision)

**Goal.** Release an owned variable at its **last use**, not at scope end. Today
`borrow-on-read` already avoids RC for reads-into-borrowing-ops; this handles the
owned-slot case (a heap local last read mid-function is freed there, shrinking
peak live set). It is also the **enabler for Task 3** (reuse needs "unique and
dead here").

**Spec.** Perceus (PLDI 2021), §ownership/last-use. Per-block liveness over the
CFG the emitter already builds (labelled basic blocks, `loops`, `cleanups`).

**Interface / touch points.** A liveness pass over the function body; replace the
blanket scope-exit release (`emit_exit_releases`) with last-use-point releases,
keeping the cleanup-stack discipline for loop temps and early returns. Must remain
correct across branches, loops, break/continue, and early return.

**Acceptance.** Oracle green incl. all control-flow cases; `live==0` preserved; a
case where a large heap local is dead before function end shows reduced peak live
(extend the runtime counter with a high-water mark if useful).

---

## Task 3 — Drop-reuse / FBIP over comprehensions (the headline)

**Goal.** When a unique (`rc==1`), dead value is dropped and a **same-shape** value
is allocated nearby, **reuse the buffer in place** instead of free+alloc. The
canonical target is the in-place comprehension `data = [f(x) for x in data]`:
zero steady-state allocation in a sensor/data loop.

**Prerequisites (must exist first).**
- Comprehensions in the front-end + emitter, lowered so the
  "construct-new-collection-by-iterating-a-source" pattern is **legible in the
  IR** (not desugared into an opaque append-loop) — so reuse has a hook.
- A runtime **uniqueness query** (`rc==1`, cheap) — add this seam early even
  before the optimization.
- Task 2 (last-use) to establish "dead here."

**Spec.** Perceus reuse analysis / FBIP; "drop-guided reuse" (the simpler,
stronger successor) and "Reference Counting with Frame Limited Reuse" (MSR).

**Interface / touch points.** A reuse-token dataflow: a drop site that may be
unique yields a token; a matching same-size allocation consumes it (runtime
`alloc`/`free` gain a reuse fast path). Falls back to normal free+alloc when not
unique.

**Acceptance.** Oracle green and `live==0`; an in-place comprehension over a
unique list shows a **measured drop in alloc count** vs the naive build (criterion
#4 in `RC_PASS_TODO.md`); correctness holds when the source is *not* unique
(aliased) — must NOT reuse then.

---

## Task 4 — Reachability-type escape inference

**Goal.** Replace today's conservative *syntactic* borrowed-param analysis
(`param_escapes` in `src/llvm.rs`) with proper **flow-sensitive escape / ownership
inference**, so more params (and locals) are provably borrowed/non-escaping —
fewer retains, more reuse opportunities, and a principled basis for the static
tier of the memory model.

**Spec.** Reachability types: "Free to Move" (2025, flow-sensitive effects for
safe dealloc + ownership transfer), Polymorphic Reachability Types (OOPSLA 2024).

**Interface / touch points.** A per-function analysis producing escape/reachability
facts consumed by the emitter's ownership decisions (generalizes the borrow masks
already threaded through `FuncEmitter`). Stay sound: unknown ⇒ escapes.

**Acceptance.** Oracle green and `live==0`; strictly more params classified
borrowable than the syntactic analysis on a representative suite, with no
regressions; report the RC-op reduction.

---

## Pointers

- Ownership contract + emitter: `src/llvm.rs` (see the comment above `FuncEmitter`).
- Runtime + RC primitives + accounting: `runtime/src/lib.rs`.
- Memory model + research/citations: `MEMORY_MANAGEMENT.md`.
- Native backend plan + status: `PICO_BACKEND.md`; RC roadmap: `RC_PASS_TODO.md`.
- The gate: `tools/native_run.sh` (set `GATE_LEAKS=0` only to diagnose).
