# Frontier tasks — scoped work for a compiler contributor

Each task below is self-contained, has a published spec, a clean interface, and a
**mechanical acceptance gate**. You do not need to understand the whole system to
land one — you need the AST, the emitter (`src/llvm.rs`), the runtime
(`runtime/src/lib.rs`), and the oracle (`tools/native_run.sh`).

## How work is accepted here (read first)

The native backend is validated **without hardware**: values are i32 arena
offsets (not machine pointers), so emitted LLVM IR + the `p2w-rt` runtime compile
with `clang` and **run on a normal host**.

`tools/native_run.sh` is the gate (61 cases today). For each case it: emits IR →
`clang` → links `p2w-rt` (built `--crate-type staticlib -- -C panic=abort`) + a
`p2w_putc` stub → runs → diffs stdout against the expected (CPython) output →
asserts **`p2w_live() == 0`** at exit (`GATE_LEAKS=1`, the default). A leak
(`live > 0`) or double-free (`live < 0`, or a trap that spin-loops → the per-case
timeout fires) **fails the build**. It also reports `p2w_allocs()` so reuse wins
are *measured*, not asserted.

**Definition of done for every task:** all existing oracle cases stay green; new
cases covering the feature are added and green; `cargo test` (lib + runtime)
green; `cargo clippy` clean. Where the task claims a speed/size/alloc win, add a
before/after number from the oracle.

## What's already shipped (the foundation you build on)

The **value model is complete** — typed code lowers to native register/buffer code:

- **Type-driven monomorphization** (was Task 1): `: int`/`: float` scalars are
  unboxed (`i32`/`double`); native `add`/`mul`/`fdiv`/`icmp`/`fcmp`, int↔float
  promotion, typed params/returns/locals, both loop forms. `box`/`unbox` only at
  the dynamic boundary. Unannotated code is the unchanged dynamic tagged-i32 path.
- **Packed arrays** `list[int]`/`list[float]` (`T_IARRAY`/`T_FARRAY`): flat
  i32/f64 buffers; construct/index/append/iterate/param, bounds-checked.
- **List & dict comprehensions**: dynamic or packed (target-typed), `if` filters,
  `range` sources.
- **FBIP drop-reuse for the self-map** (part of was-Task-3): `data = [f(x) for x
  in data]` over a unique array maps **in place, zero allocation**, guarded by a
  runtime `rc==1` check (`p2w_unique`); aliased arrays fall back to copy.
- **RC**: precise transfer-ownership insertion + `borrow-on-read` + borrowed
  params — a conservative syntactic escape analysis (`param_escapes`) lets a
  read-only param (Boxed *or* a packed array) be borrowed, so passing a named
  array to a helper costs zero refcount traffic.

So the tasks below now sit on a finished base; pick any (Task 1 — last-use — is
the enabler for fuller reuse, but they're largely independent).

---

## Task 1 — Full last-use release (Perceus precision)

**Goal.** Release an owned heap variable at its **last use**, not at scope end.
`borrow-on-read` already avoids RC for reads-into-borrowing-ops; this handles the
owned-slot case (a heap local last read mid-function is freed there, shrinking the
peak live set), and is the **enabler for general reuse** (Task 2 needs "dead here").

**Spec.** Perceus (PLDI 2021), §ownership/last-use. Per-block liveness over the
CFG the emitter already builds (labelled basic blocks, `loops`, `cleanups`).

**Interface / touch points.** A liveness pass over the function body; replace the
blanket scope-exit release (`emit_exit_releases`) with last-use-point releases,
keeping the cleanup-stack discipline for loop temps and early returns. Must remain
correct across branches, loops, break/continue, and early return.

**Acceptance.** Oracle green incl. all control-flow cases; `live==0` preserved; a
case where a large heap local is dead before function end shows reduced peak live
(add a high-water-mark counter to the runtime if useful).

---

## Task 2 — General Perceus reuse (FBIP in full)

**Goal.** Generalize the shipped self-map reuse into proper drop/reuse tokens: at
any point where a unique value is dropped and a same-shape value is constructed,
reuse the buffer — not just the `data = [f(x) for x in data]` special case.

**What exists.** The runtime `p2w_unique` seam + `p2w_allocs` counter, and the
self-map lowering (`try_inplace_map` in `src/llvm.rs`) as a worked example of the
unique-vs-copy branch. Generalizing means a reuse-token dataflow rather than a
syntactic pattern match.

**Spec.** Perceus reuse analysis / FBIP; "drop-guided reuse" (the simpler,
stronger successor); "Reference Counting with Frame Limited Reuse" (MSR).

**Interface / touch points.** A drop site that may be unique yields a reuse token;
a matching same-size allocation consumes it (the runtime `alloc`/`free` gain a
reuse fast path). Falls back to normal free+alloc when not unique. Builds on
Task 1's last-use info.

**Acceptance.** Oracle green and `live==0`; measured alloc-count drop on reuse-
heavy programs beyond the self-map; correctness when the source is aliased (must
NOT reuse) — mirror the `fbip_alias` test.

---

## Task 3 — Reachability-type escape inference

**Goal.** Replace today's conservative *syntactic* borrowed-param analysis
(`param_escapes` in `src/llvm.rs`) with proper **flow-sensitive escape / ownership
inference**, so more params (and locals) are provably borrowed/non-escaping —
fewer retains, more reuse, a principled static tier.

**Spec.** Reachability types: "Free to Move" (2025, flow-sensitive effects for
safe dealloc + ownership transfer), Polymorphic Reachability Types (OOPSLA 2024).

**Interface / touch points.** A per-function analysis producing escape/reachability
facts consumed by the emitter's ownership decisions (generalizes the borrow masks
threaded through `FuncEmitter`). Stay sound: unknown ⇒ escapes.

**Acceptance.** Oracle green and `live==0`; strictly more params classified
borrowable than the syntactic analysis on a representative suite, no regressions;
report the RC-op reduction.

---

## Task 4 — Verified RC pass (the "saved by the compiler" angle)

**Goal.** Prove the retain/release insertion sound, turning the oracle's
*test-assured* memory safety into *proof-assured* safety. This is the project's
thesis literalized: the source language can't state the invariant; a verified
compiler guarantees it.

**Spec.** RustBelt / Iris lineage; VerusBelt (PLDI 2026); "Endangered by the
Language but Saved by the Compiler" (POPL 2026). See `MEMORY_MANAGEMENT.md`.

**Interface / touch points.** Formalize the transfer-ownership contract (already
documented above `FuncEmitter`) and the runtime RC ABI; mechanize the proof for
the insertion pass. Largest/most-open task; highest research payoff.

**Acceptance.** A machine-checked soundness argument for the cycle-free subset; no
behavioral change to the emitter.

---

## Smaller polish (good first contributions)

- **Cycle collector / `--no-mutation` enforcement.** Detection already exists —
  `lint::may_form_cycle` (and `rust_p2w::may_form_cycle(source)`) soundly decides
  whether a program is cycle-free, so RC is leak-complete for the common case.
  What remains is acting on it: a collector for the mutating case, or a flag that
  enforces purity. See `MEMORY_MANAGEMENT.md`.

*(Recently shipped from this list: native int+float scalars, packed
`list[int]`/`list[float]`, list/dict comprehensions, nested comprehension `for`s,
typed-return comprehensions, FBIP self-map reuse, borrowed array params,
borrowed for-each iterables, cycle-freedom detection, tuples + unpacking
(incl. comprehension tuple targets).)*

## Pointers

- Value model + ownership contract: `VALUE_MODEL.md`; `src/llvm.rs` (above
  `FuncEmitter`).
- Runtime + RC primitives + accounting: `runtime/src/lib.rs`.
- Memory model + research/citations: `MEMORY_MANAGEMENT.md`.
- Native backend plan + status: `PICO_BACKEND.md`.
- The gate: `tools/native_run.sh` (set `GATE_LEAKS=0` only to diagnose).
