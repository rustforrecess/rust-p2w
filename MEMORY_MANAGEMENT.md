# Memory management — design & the research we're drawing on

> Goal: **"Rust-small on embedded, Python syntax up top."** This records how the
> two backends manage memory, the tiered strategy, and the published research
> each tier draws from (with citations). Companion to `PICO_BACKEND.md` (value
> model) and `DEBUGGER_ARCHITECTURE.md`.
>
> Honesty note: the paper summaries below are from abstracts / conference
> listings, not full reads yet — treat them as vetted *starting points* to
> deep-read before implementing, not digested conclusions.

## Per-backend memory model (they don't have to match)
The shared contract between backends is the **language semantics, not the memory
strategy**:

- **Browser (WASM-GC):** the engine provides the collector for free, with lots of
  RAM. Right call there; nothing to build.
- **Pico 2 W (native):** 520 KB SRAM, no free GC. We go **lean + static**, the way
  Rust does — recover as much compile-time discipline as analysis allows, and pay
  runtime cost only for the genuinely dynamic remainder.

## How Rust does it (the model we're emulating)
Rust pushes memory management to **compile time**, so the runtime has no GC and no
default refcount — just compiler-inserted `drop`s:

- **Ownership** — each value has one owner.
- **Move semantics** — assignment/passing transfers ownership; source invalidated.
- **Borrowing + lifetimes** — references checked by the borrow checker (no
  dangling, no alias-and-mutate).
- **RAII / `Drop`** — owner goes out of scope → destructor frees, deterministically.
- **Opt-in shared ownership** — `Rc`/`Arc` (explicit reference counting); `Rc`
  **leaks cycles**, broken manually with `Weak`.

**Why Python can't free-ride:** Python has no ownership/moves in the source (free
aliasing, rebinding, shared mutable containers), so the compiler can't statically
know a value's last use → that's why dynamic languages fall back to GC/RC. Our
job is to claw back Rust's compile-time discipline *where we can* and pay RC only
for the rest.

## Our tiered strategy (build all of it; games/sensor-logs need the dynamic tier)
Allocate less the more the program tells us; reclaim mid-run when it must run long.

| # | Tier | Rust analogue | Reclaims |
|---|---|---|---|
| 1 | **Don't allocate** — inline scalars (int/bool/None) + **monomorphized** typed scalars/arrays | unboxed values, `no_std` no-heap | n/a |
| 2 | **RAII via escape analysis** — free non-escaping objects at scope end | `Drop` at scope end | deterministic, scope exit |
| 3 | **Arena** — bump allocator, reset per run | `Box`/`Vec` + allocator | at program end |
| 4 | **Reference counting** — `retain`/`release`, free at zero | `Rc<T>` | promptly, mid-run |
| 5 | **Cycle handling** — weak refs or a small cycle collector | `Weak<T>` | cycles |
| 6 | *(fallback)* small tracing GC | (Rust has none) | everything |

Run-to-completion programs lean on tiers 1–3; long-running ones (a game loop, a
sensor logger) need tier 4 (+5 for cyclic object graphs).

## The research we're using (2021 → 2026)

### Runtime reclamation — precise RC + reuse (our tiers 4–5)
- **Perceus: Garbage-Free Reference Counting with Reuse** — PLDI 2021 (Reinking,
  Xie, de Moura, Leijen). The compiler inserts **precise** `dup`/`drop` so
  cycle-free programs are **garbage-free** (freed the instant dead — like `Drop`),
  plus **reuse analysis**: "free then alloc same size" becomes a **guaranteed
  in-place update** ("functional but in-place", FBIP). Now powers **Lean 4**'s
  memory. *Use:* our RC tier should be Perceus-style, not naïve refcounting —
  reuse is the biggest embedded win (a game/sensor-log **mutates cells in place**
  instead of churning the allocator). Same cycle caveat → tier 5.
  <https://www.microsoft.com/en-us/research/publication/perceus-garbage-free-reference-counting-with-reuse/>
- **The Perceus continuation line (the hire's reading order after Perceus):**
  **Frame-Limited Reuse** — ICFP 2022 (Lorenzen, Leijen): bounds reuse-token
  lifetimes to the current frame, fixing pathologies where a token outlives its
  usefulness — directly upgrades our dying-token protocol. **FP²: Fully
  in-Place Functional Programming** — ICFP 2023: a type discipline guaranteeing
  *zero* allocation for whole functions. **The Functional Essence of Imperative
  Binary Search Trees** — PLDI 2024: FBIP at real data-structure scale.
- **Production analogs (design docs, not papers — ideas-not-code):**
  **Nim ARC/ORC** — ARC = compile-time RC with move semantics + `sink`/`lent`
  inference; **ORC = ARC + a trial-deletion cycle collector run only over
  candidates whose *type* is statically judged potentially-cyclic** — the
  closest production system to our tier-5 plan (see the cycle-design sketch in
  `docs/REUSE_PLAN.md`). **Roc** — Perceus-style RC **plus automatic borrow
  inference** to erase RC traffic on non-owning uses (our escape/borrow task,
  the practical companion to Reachability Types). **Lobster** — claims ~95% of
  RC ops eliminated by compile-time analysis; small, readable design notes.

### Static discipline for a higher-level language (our tiers 1–2)
- **Reachability Types** — bringing Rust-style reasoning (aliasing, **separation**,
  ownership transfer, **safe deallocation**) to higher-level/functional languages,
  on separation-logic foundations, proven sound in Coq. This is *our exact
  situation* (Python syntax, want Rust-like deallocation).
  - **Free to Move: Reachability Types with Flow-Sensitive Effects for Safe
    Deallocation and Ownership Transfer** — 2025. The principled basis for *when
    the compiler can statically free or move* a value in a high-level language →
    the theory under our escape/ownership tier. <https://arxiv.org/pdf/2510.08939>
  - **Polymorphic Reachability Types** — OOPSLA 2024. Extends it to generics /
    higher-order with a "freshness" qualifier + transitive-closure-on-demand →
    relevant when inference must scale to functions/containers.
    <https://2024.splashcon.org/details/splash-2024-oopsla/150/Polymorphic-Reachability-Types-Tracking-Freshness-Aliasing-and-Separation-in-Highe>

### Aliasing model / in-place-mutation soundness (supports tier-4 reuse)
- **Tree Borrows** — PLDI 2025, Distinguished Paper (Villani, Jung, et al.).
  Replaces Stacked Borrows' *stack* with a *tree* (references form parent/child
  relations a stack can't model); rejects **54% fewer** programs and preserves the
  compiler's optimizations — the "Rust got it right, a cleaner CS/math structure
  improves it" exemplar. *Use:* the soundness model for **in-place mutation /
  aliasing**, which reuse analysis depends on. <https://iris-project.org/pdfs/2025-pldi-treeborrows.pdf>

### Mental model + teaching
- **A Grounded Conceptual Model for Ownership Types in Rust** — CACM. Reframes
  borrow checking as **flow-sensitive permissions on paths into memory** — a clean
  model to build a lightweight borrow-like check from, *and* a ready-made way to
  explain memory to kids (K-12 bonus). <https://dl.acm.org/doi/10.1145/3796537>

### Idea: `del` as a *visible* last-use / reuse hint (+ lifetime overlay)

Python already has `del x` (unbinds a name). Most teaching languages either hide
memory entirely or expose a raw "free now" (Binarian's `drop` — see NOTICE,
ideas-not-code). The opportunity unique to us: make `del` the **pedagogical and
the compiler-facing marker at once**, and *show* what it does.

- **Compiler:** treat `del x` (and inferred last uses) as the point where the
  drop-reuse / `rc == 1` fast path fires — the in-place reuse Perceus gives us
  (tiers 4–5). `del` becomes an explicit, kid-authored hint that a cell is free
  to reuse, on top of the inferred last-use analysis (the `live == 0` oracle
  stays the acceptance test; an explicit `del` should never *worsen* it).
- **IDE / teaching:** an **AST view with a lifetime/reuse overlay** — annotate
  where each value is born, last used, dropped, and *reused in place*. This is
  the visual half of the story: a learner literally sees "this list cell is
  reused, nothing is allocated here." Reuses the `DEBUGGER_ARCHITECTURE.md` seam;
  the Blockly blocks are already an AST view to hang the overlay on. This is the
  *reuse-legible lowering* goal made concrete, and it's novel — nobody teaches
  K-12 memory by making reuse visible.
- **Boundary:** keep it Python (`del`, not a new `drop` keyword) — grammar stays
  a Python superset. The "make the model visible" framing pairs with the
  Grounded Conceptual Model / permissions-on-paths teaching angle above.

Status: idea, not built. Lands when the Perceus reuse tier + the emitter's
`dup`/`drop` wiring exist (see Status); the overlay is an IDE follow-on.

### Formal foundations — extend/verify the type system (optional future)
- **VerusBelt: A Semantic Foundation for Verus's Proof-Oriented Extensions to the
  Rust Type System** — PLDI 2026, Distinguished Paper (Hance, Elbeheiry, Dreyer,
  Matsushita). The RustBelt/Iris separation-logic lineage; the rigorous math to
  *extend* Rust's type system. *Use:* if we ever want to **prove** our typed
  subset memory-safe, not just enforce it.
- **Endangered by the Language But Saved by the Compiler: Robust Safety via
  Semantic Back-Translation** — POPL 2026 (Mück, Georges, Dreyer, Garg, Sammler).
  Safety the *language* can't guarantee, enforced by the *compiler* — essentially
  our thesis (Python gives no ownership; our compiler manages memory).
  - Both via MPI-SWS PL&V news: <https://www.mpi-sws.org/news/programming-languages-and-verification/>
- Foundations these build on: **RustBelt** (POPL 2018, separation logic / Iris)
  and **Stacked Borrows** (POPL 2020).

## The synthesis (what we actually adopt)
The 2021→2026 frontier matches the plan:
1. **RC tier → Perceus-style precise RC + reuse** (not naïve refcounting); reuse =
   the embedded win; cycles handled by tier 5.
2. **Static tier → reachability-type-style escape/ownership inference** (Free to
   Move / Polymorphic Reachability Types) to recover Rust's `Drop` discipline for
   Python syntax.
3. **Tree Borrows** as the soundness model when reuse does in-place mutation.
4. **VerusBelt/RustBelt** foundations available if we later want *verified* safety
   for the typed subset.

## Perceus → concrete implementation plan (RC + reuse tier)
*(From the published algorithm + abstract; a line-by-line read of the PDF is
still worth doing before final implementation.)*

Mechanisms we adopt:
- **Ownership + dup/drop insertion (compile-time):** values are owned; emit
  `dup` (= `p2w_retain`) on every non-last use, `drop` (= `p2w_release`) at
  last-use or when a binding is dead. The runtime ops are trivial; **the insertion
  analysis is the real work.**
- **Garbage-free:** drops at last-use (not scope-end) → objects die immediately,
  like Rust's `Drop`.
- **Reuse analysis (`drop-reuse`):** when a `drop` frees a *unique* object (rc→0)
  and a *same-size* allocation follows, the drop returns a **reuse token** (the
  freed cell, or null if it was shared) and the allocation **reuses it in place**
  → zero allocator traffic; safe when shared (null → fresh alloc).
- **Reuse specialization (FIP):** reusing the same constructor only rewrites
  changed fields.
- **Borrowed params:** read-only args are *borrowed* → no dup at call, no drop in
  callee (kills churn, e.g. `len(xs)`).
- **FBIP:** in-place "match-and-rebuild" with no allocation.
- **Cycles leak** — and unlike Koka (mostly immutable), **Python has mutable
  containers → real cycles**, so the cycle tier is mandatory for us.

The embedded payoff = the named use cases: a **sensor-log append** / **game-state
update** in a loop becomes **in-place reuse → near-zero per-iteration allocation**
on the 520 KB chip. That's why RC must be Perceus-style, not naïve.

ABI additions for reuse: `p2w_drop_reuse(v) -> token` and reuse-token constructor
variants (`p2w_list_new_reuse(token, …)`, str/dict similarly).

Implementation order: (1) emit naïve retain/release (correct, drop at scope end);
(2) move drops to last-use (precise/garbage-free); (3) borrowed-param analysis;
(4) drop-reuse + token constructors + reuse specialization; (5) cycle collector
(trial-deletion / CPython-style, or weak refs).

## Cycle-free fast path: no mutation ⇒ drop the collector
Key fact: **you cannot build a heap cycle without mutation.** `a = [a]` is a
NameError; the only way to make an object reach itself is to *mutate* an existing
one (`a.append(a)`, `node.parent = p`, `d[k] = d`). So the sound criterion is
**"the program performs no mutating operations"** (no `append`/`pop`, no
`xs[i] = v`, no `obj.attr = v`, no mutating method) — lists/dicts may still be
*constructed and read*, just not mutated. No mutation ⇒ DAG, never a cycle ⇒ the
**cycle collector (tier 5) can be omitted entirely** (smaller binary, no pauses,
fully garbage-free RC — the Koka/Lean sweet spot). And with reuse analysis,
immutable "build a new value" code still runs **in place** (FBIP), so this is a
perf *enabler*, not a penalty.

Deliver both:
- **Automatic detection (default):** scan for any mutating op; if none, omit the
  collector and take the lean path automatically ("pay for cycles only if you
  mutate"). The IDE can surface it ("✓ allocation-lean: no cycle collector").
- **Explicit `--no-mutation` / "pure" mode:** *enforce* the no-mutation subset
  (compile error on mutation) for a guaranteed lean binary + to teach functional
  style (which FBIP reuse makes efficient).

Coarse rule first (any mutation → keep the collector); a finer analysis (prove a
specific mutation can't create a back-edge) is a later refinement. This is our
compiler-checkable version of how Rust programmers structure around `Rc` cycles.

## Real-time / ROS (micro-ROS) considerations
On the Pico, "ROS" means **micro-ROS**, which runs on an **RTOS** (FreeRTOS/
Zephyr) — so robotics pulls in real-time + concurrency constraints. RC is a good
fit (no stop-the-world GC), but the RC pass must keep two seams open, plus one
invariant:

- **Atomicity swap point.** `retain`/`release` are non-atomic (single-threaded).
  If a value is shared across RTOS tasks they must become atomic (CAS) + a locked
  arena. Isolated in `rc_inc`/`rc_dec` so it's swappable without touching the
  insertion pass. *(Implemented as a seam.)*
- **Deferrable free.** A `release` of a large graph cascades into many frees → an
  unbounded pause in a control loop. `free_object` is the single free seam so it
  can later become **incremental** (enqueue children, free a bounded amount per
  tick). *(Implemented as a seam.)*
- **Invariant — single-threaded interpreter, marshal at the boundary.** p2w runs
  as the logic inside one ROS node/task; the host bridges ROS topics ↔ p2w via
  the host-import seam (`p2w_ros_publish`, subscriber callbacks), and values are
  **copied** in/out at the boundary, never shared as live heap refs across tasks.
  This keeps RC single-threaded and the arena lock-free.

**Why reuse matters for robotics:** a control loop runs forever (the games/
sensor-log case → needs RC), and must not churn memory each tick. Perceus
**reuse** → a **zero-allocation steady state** (no allocator on the hot path),
which is exactly what RT/micro-ROS wants (it favors static pools). The
no-mutation fast path compounds this.

Watch-items for an RT-hardening phase (not blocking): O(1) size-class allocator +
coalescing (our first-fit fragments over long runs — reuse mostly sidesteps it);
fault-safe behavior for `trap`/OOM in a robot (halt motors, not loop forever);
float/fixed-point for robotics math (M33 has an FPU).

## Status (Jul 2026)
- **Built and measured — the Perceus tier is live** (see `docs/REUSE_PLAN.md`
  for detail and `docs/COMPILER_FRONTIER.md` for the proof table):
  - the runtime (`p2w-rt`): arena + first-fit free list (block-size headers =
    capacity metadata), full RC across strings/lists/dicts/sets/tuples/packed
    arrays, `p2w_unique`, `p2w_can_reuse_*`, `p2w_add_assign`, and the
    `p2w_live`/`p2w_allocs`/`p2w_peak` counters;
  - the emitter RC pass (transfer-based, borrowed params) **plus** last-mention
    liveness (`src/reuse.rs`), precise drops at last use, dying-source map
    reuse, assign-site literal reuse, append/extend growth, per-site interned
    literals, slice-steal (`p2w_slice_assign` — peel/pop-front loops compact
    in place), and reuse tokens distributed into mutually-exclusive `if`/`else`
    arms;
  - **type inference (frontier task 3)**: `infer_expr_repr` widens the
    reuse-map gate (typed-call elements steal buffers; closed the
    int-literal-into-float-buffer output bug), and `infer_slot_reprs` gives
    unannotated scalar locals raw Int/Float slots by a demote-on-conflict
    fixpoint join — the typed tier now fires on the unannotated code kids
    actually write, with type churn keeping today's boxed path;
  - measured: `wl_chain` 10→3 allocs, `wl_realloc` 6→2, `wl_concat` 17→4,
    `wl_slice` 11→2, `wl_branch` 6→3, self-map 4→2 — map pipelines,
    reassignment churn, and peel loops run at or near zero-allocation steady
    state, no GC.
- **Verified by three nets:** the 191-case CPython-diff + `live == 0` oracle
  (`tools/native_run.sh`), the alloc/peak bench (`tools/reuse_bench.sh`), and
  the differential fuzzer (`tools/fuzz_native.sh` — the generator emits slice
  shapes and type-churn adversaries; 200 seeds green).
- **Next (the frontier — `docs/COMPILER_FRONTIER.md`):** full backward
  liveness, container slot inference (mutation-site constraints), escape/
  reachability inference (tier 2 generalized), and cycle handling (tier 5 —
  design sketched from Nim ORC; gates making linear-memory the
  browser/component default).
