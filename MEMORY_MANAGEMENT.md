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

## Status
- **Built:** the heap allocator (static arena + first-fit free list), the string
  heap type, and naïve RC primitives (`p2w_retain`/`p2w_release`) in `runtime/`
  (`p2w-rt`), host-tested.
- **Next:** lists/dicts; then upgrade RC to **Perceus-style (reuse)**; wire the
  emitter to emit `dup`/`drop`; cycle handling; then the static tiers (escape
  analysis, monomorphization from the type annotations).
