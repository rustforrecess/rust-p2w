# The compiler frontier — pitch + scoped tasks

> For a PL/compilers person deciding whether this project is worth their time.
> Everything in the "already true" section is reproducible from this repo today;
> the tasks section is the open frontier, each with an interface and an
> executable acceptance gate.

## The pitch, one paragraph

**Python ergonomics with Rust-class memory, proven on a $7 microcontroller and
as a WebAssembly component.** rust-p2w compiles a Python subset from one AST to
two backends: WASM-GC for the browser IDE, and LLVM + a `no_std` runtime for
bare-metal (RP2350 / Cortex-M33) and linear-memory WASM. The native side has no
GC: compiler-inserted reference counting with **Perceus-style drop-reuse** —
"functional-but-in-place" (FBIP). The interesting part: this is FBIP applied to
*Python* (free aliasing, no ownership in the source), kept sound by runtime
uniqueness guards and a conservative-by-construction liveness — and the
remaining open problems are exactly the fun ones (full liveness, escape
inference, cycles, type-driven reuse widening).

## Already true — proof, not promises

Reproduce with `tools/native_run.sh` (correctness oracle) and
`tools/reuse_bench.sh` (allocation/peak bench); both need clang + cargo.

| program shape | naive scope-end RC | with the landed reuse tier |
|---|---|---|
| 3-stage comprehension pipeline (`wl_chain`) | 10 allocs, peak 3 | **3 allocs, peak 1** — the pipeline runs in ONE buffer |
| 3× literal reassignment (`wl_realloc`) | 6 allocs, peak 2 | **2 allocs, peak 1** |
| unique self-map (`fbip_unique`) | 4 allocs | **2 allocs** (in-place) |
| 8-iteration string-append loop (`wl_concat`) | 17 allocs | **4 allocs** (in-place growth + interned literals) |
| 10-iteration string peel loop, `s = s[1:]` (`wl_slice`) | 11 allocs, peak 3 | **2 allocs, peak 2** — the loop runs in ONE buffer |
| comprehension over a source dying at an if/else join (`wl_branch`) | 6 allocs, peak 2 | **3 allocs, peak 1** (the taken arm steals the buffer) |
| typed-call comprehension `[dbl(x) for x in a]`, `dbl -> int` (`wl_typedcall`) | 6 allocs, peak 2 | **3 allocs, peak 1** (inference proves the element int) |
| unannotated big-int accumulator loop (`wl_accum`) | 6 allocs, peak 2 | **1 alloc, peak 1** (slot inference: raw i32, no boxing) |

- **Zero-allocation steady state** for map pipelines and reassignment churn —
  what a sensor loop or game loop on a 520 KB-RAM device needs.
- **153-case run-oracle**: every case's output is diffed against CPython *and*
  the runtime's live-object counter must end at **0** (leak-free RC), including
  adversarial cases that attack each soundness guard (aliased sources,
  borrowed-param theft, freed-cell reuse corruption, container-reading
  elements, wrong-tag reuse of a string-holding slot).
- **Typed hot paths compile like C**: an annotated int function body emits zero
  runtime calls (`VALUE_MODEL.md`); packed `list[int]`/`list[float]` arrays.
- **Cross-compiles and links for the RP2350** (Cortex-M33, ~8–9 KB ELF), and
  compiles to a **linear-memory WASM Component-Model component** that runs
  correct and leak-free in a real component host — no WASM-GC dependency, which
  is what makes compiled Python viable as a sandboxed-activity guest (see
  the PXC standard work).

What's landed of the Perceus staging (`REUSE_PLAN.md` has the detail):
last-mention liveness (`src/reuse.rs`) → precise drops at last use →
dying-source map reuse (`try_reuse_map`) → assign-site literal reuse
(`try_reuse_literal`) → append/extend growth (`p2w_add_assign`) → per-site
interned literals → slice-steal (`p2w_slice_assign`: peel/pop-front loops
compact in place) → reuse tokens distributed into mutually-exclusive if/else
arms — each runtime-guarded (`p2w_unique` / `p2w_can_reuse_*` / `a != b`) so
aliasing silently degrades to copy semantics. The original wishlist is
closed; what remains below is the deeper analysis work.

## Why it's interesting work

- **Perceus/FBIP on Python is unclaimed territory.** Perceus (PLDI'21) powers
  Lean 4 and Koka — languages *designed* for it. Python has free aliasing and
  no source-level ownership; making reuse sound here means runtime uniqueness
  guards + static analysis meeting in the middle. The relevant literature is
  mapped in `MEMORY_MANAGEMENT.md` (Perceus, Reachability Types / Free-to-Move,
  Tree Borrows, the RustBelt lineage).
- **Small, legible codebase**: ~24k lines of dependency-light Rust (tests
  included); the emitter is textual LLVM IR (no LLVM build dep); the runtime is
  a single `no_std` file with an explicit `[tag][rc][len]` layout. The native
  backend + runtime + analysis — the parts this doc is about — are ~6k lines
  you can hold in your head.
- **A rare verification setup**: every change is gated by an executable
  contract — output ≡ CPython ∧ live == 0 — plus an allocation/peak bench, so
  aggressive optimization work lands with confidence instead of fear.
- The consumer is real: a K-12 IDE (browser) and a bare-metal board target,
  with the memory model as the differentiator, not an afterthought.

## How the work lands

Small PRs behind fixed seams. The acceptance contract for *everything*:
`tools/native_run.sh` stays green (CPython diff + live == 0, adversaries
included) while `tools/reuse_bench.sh` numbers move in the right direction.
The analysis seam is `src/reuse.rs` (the emitter consumes `Liveness::dead_after`
and the dying-token protocol); the ownership rules are documented at the top of
`src/llvm.rs` (transfer-based model: owned slots +1, borrowed params, transfer
sites).

## The open tasks (pick your poison)

### 1. ~~Literal hoisting / interning~~ — LANDED

Per-site lazy caching: every string-literal site gets a zero-init module
global; the first execution materializes via `p2w_str`, later executions
(loop iterations) `load + retain`. `main` frees the whole cache at exit, so
`live == 0` stays exact. The predicted pin hazard resolved *elegantly*: the
cache's permanent +1 pins rc ≥ 2 whenever a consumer holds a cached literal,
so `p2w_add_assign`'s uniqueness guard can never grow one in place — the pin
IS the mutation guard. Measured: `wl_concat` 17 → 10 → **4 allocs** (peak
3 → 4: pinned literals count toward peak; churn collapsed — the right trade
on-device). The original wishlist is now fully closed; remaining reuse work
is tasks 2–4 + 6 below.

### 2. Full backward liveness (upgrade the last-mention analysis)

`src/reuse.rs` deliberately counts assignments as mentions (no early release
before a reassignment → structurally no double-free). Full liveness would
release *before* a later reassignment and inside branches — more deaths, more
reuse — but requires coordinating with the assign-site release so the two
never double-release. **Interface:** replace `Liveness::analyze`'s body; the
`dead_after` contract and emitter stay put (extend the token protocol if you
need per-branch granularity). **Acceptance:** oracle green; peak numbers drop
on new bench cases that today's analysis can't catch.

### 3. ~~Type inference to widen the reuse whitelist~~ — LANDED (both halves)

**Half 1 — expression inference (`infer_expr_repr`, the `type_of`):**
conservative forward inference over literals, typed slots, annotated
signatures, `len`, packed-array indexing, and Python's numeric promotion
(`/` → float; float floor/mod stay runtime). It REPLACED the syntactic
whitelist at the reuse-map gate rather than falling back to it, because the
whitelist's `Int-literal-matches-anything` arm was a live output bug:
`[7 for x in floats]` adopted the float buffer and printed `7.0` where
CPython prints `7` (caught during bring-up; now an oracle regression case).
Typed-call elements (`[dbl(x) for x in a]` with `dbl -> int`) now steal the
dying buffer. Bring-up also flushed out a second pre-existing miscompile:
a raw `x: int` slot passed to a BORROWED unannotated (Boxed) param skipped
boxing entirely — the callee got an untagged word and trapped; fixed at the
call-site fast path (box + release-after-call).

**Half 2 — first-assignment slot inference (`infer_slot_reprs`):** a
fixpoint join over every binding of each unannotated local (plain assigns,
loop vars, unpack targets), demoting to Boxed on ANY disagreement or
unknown — including int/float mixing (a mixed name in a Float slot would
print `1.0` where CPython prints `1`). Names whose bindings all provably
agree get raw Int/Float slots: `x = 5; if x < 1:` is a native `icmp` with
no truthy call, `t = 0; t = t + i` loops with zero runtime calls, and >2^30
intermediates stop heap-boxing. Precedent: Go's `:=`, mypy's default,
RPython, Codon, Cython `infer_types`, Julia's type-stability culture. This
ships the **silent-demote arm** of the policy question below; containers
(`xs = [1, 2, 3]` → packed) are the remaining stretch — they need
mutation-site constraints (`.append`/setindex arg types) before the join is
sound.

**Open policy question (deliberately unresolved): what happens on a
cross-type reassignment.** The mechanism is policy-neutral — the conflict
site is one line: demote to Boxed (silent, CPython-identical — what shipped),
lint (teach the discipline softly), or reject (Codon/mypy-style; better
pedagogy for genuine type confusion, and Jason is sympathetic to it).
Evidence on each side: rejection breaks the canonical beginner pattern
`age = input(...)` / `age = int(age)` (str→int churn — a top real-world mypy
complaint) and int→float accumulator churn, and it breaks PYTHON_COMPAT's
guiding rule; but `x = 1; x = "hi"` IS a bug in waiting and mypy will tell
them so later. **Plan: add the IDE lint behind a strictness seam (the
`STRICT_TYPES` precedent in the blocks layer; Hedy-style level-gating is the
model), measure what the lint actually fires on in student code, and only
then decide whether to promote it to an error — per classroom level, not
for the language.**

**Design decision — deliberately NOT Hindley–Milner (Jul 2026).** Types here
only *gate optimizations*: `type_of` returning `None` means "stay boxed," so
an inference miss is a missed alloc win, never a rejected program or a wrong
answer — which flips the usual power/complexity trade. What HM would add:
backward unification for empty-container builders (`ys = []` + appends →
`list[int]`), unannotated function boundaries, recursive return types, and a
principal-types completeness guarantee. What it costs: a unification engine
that fights Python semantics — reassignment/mutation break let-polymorphism
(value-restriction territory), and `x = 1; x = "hi"` is legal Python that
unification rejects, which is exactly how Codon makes HM work (by rejecting
dynamic programs — the one move ruled out here; no production Python checker
uses HM either, for the same reason). Every HM-only win is recoverable with
one annotation (`ys: list[int] = []`), which is curriculum, not a tax, in a
K-12 tool. **If more inference power is ever wanted, the upgrade path is
call-site monomorphization (Julia-style specialization — fits our
whole-program, no-separate-compilation setup and subsumes the unannotated-
function case), then flow-based dataflow with widening (the mypy/Pyright
shape) — not HM.** Prior-art note: this was decided without reading Codon /
LPython source (ideas-not-code discipline, see NOTICE); the relevant
references are specs — CPython numeric semantics (already enforced
mechanically by the oracle) and PEP 484.

### 4. Escape / reachability inference (generalize borrow masks)

Parameters are borrowed today via a local escape check. Reachability-types
thinking (Free-to-Move, OOPSLA'24/arXiv'25) could generalize: which bindings
provably don't escape → stack-like discipline, fewer RC ops, more reuse
tokens. **Acceptance:** RC-traffic counts drop on the bench (add a
retain/release counter to `p2w-rt`); oracle green.

### 5. Cycle handling (tier 5 — the strategic one)

RC leaks cycles. **Design sketch (modeled on Nim ORC — trial deletion over
type-limited candidates; from their public docs only, see NOTICE):**

- **Layer 0 — program-level (exists):** the `may_form_cycle` lint gives a
  *whole-program* cycle-freedom guarantee; when it says no, the collector
  isn't enabled at all — zero overhead, and most K-12 programs land here.
  (Nim can't do this under separate compilation; we can — our biggest edge.)
- **Layer 1 — type-level (ORC's key move, stronger for us):** a cycle can
  only be *closed* by mutating a container (`T_LIST`/`T_DICT`/`T_SET`
  insertions); strings, packed arrays, floats are acyclic by construction.
  Only container-tagged objects ever become candidates — our runtime tag IS
  the classification Nim derives from type analysis + `.acyclic`.
- **Layer 2 — candidates + trial deletion:** O(1) registration of a container
  into a candidates buffer when a `p2w_release` decrement leaves rc > 0 (the
  only event that can strand a cycle); Lins/Bacon–Rajan trial deletion over
  the buffer at an allocation threshold. Bounded, incremental, no
  stop-the-world — ORC reports sub-millisecond latencies with this shape.

**This gates making linear-memory the default browser/component build** (today
WASM-GC covers the browser; the no-GC build is opt-in for device/component
targets). **Acceptance:** cyclic-program oracle cases end at live == 0 (or are
statically rejected with a friendly error); the acyclic bench is unchanged
(Layer 0 keeps today's fast path exactly).

### 6. Stretch: more reuse shapes (two of four LANDED)

~~Slicing that steals from a dying source~~ — landed as `p2w_slice_assign`
(`s = s[1:]` peel loops and `xs = xs[1:]` pop-fronts compact a unique
string/list in place; `wl_slice` 11 → 2 allocs). ~~Reuse across `if/else`
join points~~ — landed as arm-token distribution (`arm_block`: a token dying
at an `if` is re-placed inside each mutually-exclusive arm; `wl_branch`
6 → 3 allocs). Still open: **dict comprehensions** (kv-pair overwrite needs
a same-shape guard) and **`append`-then-die builders** (`ys = []; for x in
xs: ys.append(f(x))` stealing xs's buffer — wants task 2's cross-loop
liveness, since the source dies after the loop, not at a statement).

### 7. Stretch: a verified RC pass (the research angle)

The RustBelt/VerusBelt lineage (see `MEMORY_MANAGEMENT.md`) makes it plausible
to *prove* the emitter's ownership discipline sound rather than just test it —
"safety the language can't guarantee, enforced by the compiler" is literally
this project's thesis (POPL'26's *Semantic Back-Translation* framing). A
mechanized argument for the transfer model + reuse tokens would be a
publishable result on its own. **Acceptance:** a machine-checked statement of
the invariant the oracle currently samples (output ≡ CPython ∧ live == 0).

## Reading order

`README.md` → `REUSE_PLAN.md` (staging + invariants) → `src/reuse.rs` (the
seam) → the ownership comment atop `src/llvm.rs` → `MEMORY_MANAGEMENT.md`
(research map) → `VALUE_MODEL.md` / `PICO_BACKEND.md` (value model, device
target). Run `tools/native_run.sh` and `tools/reuse_bench.sh` first — the
gates are the ground truth.
