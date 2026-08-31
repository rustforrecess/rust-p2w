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
- **195-case run-oracle**: every case's output is diffed against CPython *and*
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

**Measured against C** (`tools/safety_cost.sh`, host x86-64, medians, ±0.15):
no-heap scalar loop **1.15x** clang -O2; heap-resident reads **1.17x**; 600k
allocate-and-die churn **1.40x** — with loop reuse still off (task 2's
headroom). For scale, Fil-C reports ~4x for memory-safe C in bad cases.

**Third-party programs as the honesty check:** upstream p2w ships the Alioth
suite; `spectralnorm`, `fannkuchredux` and `mandelbrot` now compile, run
natively, and match CPython byte-for-byte (`nbody` waits on task 8). Feeding
the suite foreign code found three real bugs the 550-test suite could not —
annotated locals only read were never bound, packed-array loops walked nothing
on re-entry, and `def main` collided with the entry point at link.

**And the honest timing split** (same host, medians): `mandelbrot` n=1000
**1.63x** gcc, `fannkuchredux` n=10 **3.17x** — real programs sit well above
the 1.15–1.40x microbenchmarks, and that gap is the open work, not a footnote.
One anomaly is deliberately left unprofiled for whoever takes this on:
`spectralnorm` n=150 is correct with LIVE=0 and only 330 allocations, but at
n=1000 it slows past 90 s and then exhausts the arena with a near-empty live
set — an allocator/arena-scaling question, not an RC leak.

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

**Status update (measured):** loop bodies are conservative BY CONSTRUCTION —
they get plain `block()` walks, no early releases, no tokens — so this is
purely an optimisation task, not a correctness one. The cost is now
quantified: the `churn` case in `tools/safety_cost.sh` (600k allocate-and-die
pairs) runs **1.40x hand-written C** with **1.2M allocations** that loop-aware
liveness would collapse to ~2; the no-heap control case runs 1.15x, so the
recoverable gap is real. Loop re-entry adversaries (`loop_comp_reentry`,
`loop_foreach_packed`, `nested_for_packed`, …) are already in the oracle as
your regression net. References: the Perceus paper / Koka's Parc pass
(Apache-2.0) for the dup/drop/reuse discipline; note Koka has no back-edges
(loops are tail calls there), so the imperative fixpoint itself is textbook
dataflow — rustc's MIR liveness (MIT/Apache) is the production-shaped
reference.

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
for the language.** *The lint's compiler half is LANDED* —
`rust_p2w::type_churn_warnings(source) -> Vec<(line, message)>`
(`src/lint.rs`): a pure, additive analysis (zero codegen/soundness surface)
that flags a name reused across value *categories* (number/text/list/dict/
set/tuple), deliberately NOT firing on int→float numeric progression or
dynamic-source reassignment, and scoped per function. It classifies the
conversion builtins (`int`/`str`/`input`/…) so the canonical
`age = input()` / `age = int(age)` case DOES surface — that's the evidence
we want. Remaining: the IDE surfaces it as soft squiggles + the
level-gating dial.

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

### 8. Module-level globals readable from functions (native backend)

The WASM backend has real globals (`(global $g_x …)`); the native backend
emits top-level bindings as allocas inside `main`, so a function body reading
a module constant fails with "name is not defined". This is the last blocker
for the fourth Alioth program: `nbody` opens with

    SOLAR_MASS: float = 4.0 * PI * PI

and reads it from every function. Students write module constants constantly,
so this is a language gap wearing a benchmark costume.

**Interface:** promote module-level slots that any `def` reads to LLVM
`@globals` (zero-init like slots today; `main` still runs the initialising
statements in order, so a def called before the binding reads the zero value —
the same program is broken under CPython too). `p2w_dispose` — already emitted,
currently empty — releases heap-valued globals so `live == 0` stays exact.
**Acceptance:** `nbody` compiles, runs, matches CPython; oracle green with new
global-read/write/shadow adversaries; dispose keeps the leak gate at zero.

### 7. Stretch: a verified RC pass (the research angle)

The RustBelt/VerusBelt lineage (see `MEMORY_MANAGEMENT.md`) makes it plausible
to *prove* the emitter's ownership discipline sound rather than just test it —
"safety the language can't guarantee, enforced by the compiler" is literally
this project's thesis (POPL'26's *Semantic Back-Translation* framing). A
mechanized argument for the transfer model + reuse tokens would be a
publishable result on its own. **Acceptance:** a machine-checked statement of
the invariant the oracle currently samples (output ≡ CPython ∧ live == 0).

### 9. Checked sums: per-class types, then exhaustiveness as a guarantee

**Why it's cheap here:** the closed world already did the hard part. One
program, no dynamic loading, no dynamic name resolution — so every base
class's set of subclasses is completely known at compile time. That is the
property nominal languages bolt on with `sealed`/`final`; here every
class-hierarchy sum (`Part = Motor | Servo | Buzzer`) is sealed by
construction. What's missing is only representation and checking:
`check.rs` collapses every class to one coarse `Ty::Class`, so the checker
cannot say "this is a Buzzer and Buzzer has no stop()". The
`variant_missing_method` lint is exhaustiveness-as-advisory; per-class
types graduate it to a guarantee for the method-per-variant form.
**Interface:** `Ty::Class` → `Ty::Class(name)` (or an interned id) in the
lattice; join of two classes = nearest common ancestor, else `Dyn`; the
gate stays confined to the confident shapes per D6(b) — a method missing
on the *inferred* class of a receiver joins the GATED list only once the
oracle shows zero false positives on the must-accept corpus.
**Elimination form later, not now:** `match`/`case` is Python 3.10 syntax,
so adopting it is Tier-1-legal when wanted; until then method-per-variant
dispatch IS the elimination form and the lint/gate covers it.
**Acceptance:** the Buzzer program flips from advisory to compile error on
every surface with a derivation message; oracle must-accept stays green;
BACKEND_DIVERGENCE stays at zero.

### 10. Zero-cost generics: PEP 695 monomorphization (after the unboxing rework)

**Why it fits the language promise:** `def first[T](xs: list[T]) -> T` is
real CPython 3.12 syntax — the spelling costs nothing against the
every-p2w-program-is-valid-Python invariant (today it's a syntax error
here; the lexer/parser work is the small half). And the closed world +
no-dynamic-dispatch rules mean *full* whole-program monomorphization: every
call site's `T` is known, each instantiation becomes a separate compiled
function, nothing generic survives to runtime — no dict passing, no boxing
fallback, no trait objects. That is the zero-cost half, and it is exactly
the compile-time evaluation the blue-tier rule permits: terminating (bound
instantiation depth to reject polymorphic recursion), effect-free,
capability-neutral. See SUBSET_POLICY.md when this lands — the rule that a
blue phase must stay non-Turing-complete gets written down there the day
this task starts, because monomorphization is its first resident.
**Sequencing:** after the typed-tier representation rework (unboxed
container storage, i64, the marshalling batch). Before it, generics would
be checked-but-boxed — the annotations would verify and buy nothing at
runtime, which teaches the wrong lesson about what the types are for.
**Interface:** instantiation pass between `check` and both backends — the
front-end collects call-site type arguments, clones the AST per distinct
tuple, renames (`first§int`), and hands both backends a monomorphic
program; neither backend learns what a type parameter is.
**Acceptance:** the generic identity/`first`/`swap` trio runs on all three
surfaces with outputs matching CPython 3.12; emitted WAT/LLVM for
`first[int]` is byte-identical to the hand-written monomorphic version
(that IS the zero-cost claim, checked in IR text); a polymorphic-recursion
probe is rejected with a written error message, not a hang.

## The verification frontier (added 2026-08-18) — TODO, each with why

A 2026-08 research survey (searched, not recalled) found the field converging
on this project's positions: the DORA 2026 report calls generation "solved
enough" and names specification/verification as where gains are won; MSR RiSE
declared intent formalization a grand challenge (2026-03); SPLASH 2026 hosts
the first SpecOps workshop. Every item below is a TODO **because a specific
recent result made it cheaper or newly legitimate** — the note under each is
that reason.

### V1. Runtime contracts in the verify-rust-std style

**Why now:** the Rust Foundation/AWS `verify-rust-std` effort (29 published
challenges; accepted tools Kani, VeriFast, Flux, ESBMC-transcoder) verified
contracts over the ACTUAL standard library's unsafe code — the same
discipline our three Kani harnesses apply to the arena, with a
lessons-learned paper to crib from. Adopting their `requires`/`ensures`
contract style makes our proofs legible to that ecosystem and lets harnesses
derive from contracts instead of restating them.
**Acceptance:** contracts on the runtime's unsafe entry points; Kani proofs
reference them; Miri/Kani CI unchanged and green.

### V2. Flux over the arena offset arithmetic

**Why now:** Flux (refinement types for Rust) is production-adjacent enough
to be a verify-rust-std accepted tool. Refinements can prove the
`rd`/`wr`/offset arithmetic in-bounds *by type*, without authoring a harness
per property — the cheap upgrade path beyond bounded model checking.
**Acceptance:** Flux passes on `runtime/` (or a written list of what blocked
it — that list is itself the finding).

### V3. sequent's certificate tier, per the published blueprint

**Why now:** "Verifying Datalog Reasoning with Lean" (ITP 2025) implements
exactly the architecture we planned for sequent — an untrusted, fast
reasoner emits a proof object; a Lean-verified checker validates it against
mechanized Datalog semantics. And "Capability Safety as Datalog: A
Foundational Equivalence" (2026) makes `capabilities()`-as-Datalog a
*theorem-shaped* claim, not an analogy. The design no longer needs
defending; it needs building against a citable spec.
**Acceptance:** `capabilities()` (or `may_form_cycle`) emits a derivation;
sequent independently checks it; a tampered certificate is REJECTED (the
negative test is the point).

### V4. Surface engine validation as a per-module certificate

**Why now:** nothing new had to be invented — the observation is that every
emitted module already passes V8/wasmtime *validation*, which IS a formal
typecheck of the artifact against the mechanized Wasm type system. We just
never surface it. One field in `p2w check --json` turns an invisible
guarantee into a visible one, and it is the first rung of the
certificate-per-artifact story the MDM/verifier work wants.
**Acceptance:** the JSON reports a validation verdict produced by actually
validating, not by claiming.

### V5. Translation validation between the two backends

**Why now:** the ladder already chose translation validation over
whole-compiler proof (churn-tolerant by construction); what's new is
precedent at both granularities — VeriISLE (ASPLOS 2024) SMT-verifies
Cranelift's *lowering rules* (partially covering our own trusted base, since
wasmtime runs on Cranelift), and Alive2-style per-program validation is
routine for LLVM. Our differential suite is testing; the typed tier's IR is
the trigger that makes per-program validation buildable.
**Acceptance:** for a compiled pair, a validator replays both against one
value-level semantics; the divergence suite becomes a special case of it.

### V6. Mechanize a Wasm-GC core (the open research contribution)

**Why now:** Iris-Wasm mechanized Wasm 1.0 (PLDI 2023) and the lineage now
covers MSWasm (OOPSLA 2024) and WasmFX (2025-26) — **nobody has mechanized
Wasm-GC**, the extension our browser backend and every Kotlin/Dart/J2CL
module stands on. We also hold a concrete motivating artifact: the wasmtime
`is_subtype` panic our string work found, a soundness-adjacent engine bug in
exactly the unmechanized territory. Pairs with the typed tier's planned Lean
soundness story; even a core-calculus fragment is publishable.
**Acceptance:** a mechanized fragment whose statement covers the `ref.test`/
subtyping corner the bug lives in.

### V7. The copilot gate grows proof obligations

**Why now:** the LLM-generates/checker-gates loop is now measured, not
speculative — DafnyPro proves 86% of DafnyBench (POPL 2026), AutoVerus cuts
Verus proof code ~80%, Clover's spec-consistency gate accepts 87% of correct
instances with zero adversarial passes. Our decided architecture (the
compiler as the agent's verifier, lints as feedback) is that loop with
weaker obligations — and the subset (decidable, no dynamic dispatch) is the
easiest spec-inference target in the field. When the type checker lands, its
judgments become obligations the copilot's output must discharge.
**Acceptance:** generated code reaches a student only through `p2w check`;
each strengthening of the checker strengthens the gate with no copilot
changes.

### V8. Pedagogy of verification (the Helium-shaped window)

**Why now:** the pieces just appeared — an experimental study of LLMs
helping *students* prove correctness in Dafny, miniF2F ported to Dafny, an
interactive proof mode softening Dafny's auto-active cliff — and nobody has
assembled them for a language children use. This is the same shape as the
type-error-pedagogy gap: two literatures, no assembly. Verification literacy
("the fast test that doesn't count" as a checked property; derivation-based
messages as baby proofs) has no incumbent.
**Acceptance:** one shipped lesson where the student's claim about a program
is discharged by an actual checker, and the evidence stream records it.

## Reading order

`README.md` → `REUSE_PLAN.md` (staging + invariants) → `src/reuse.rs` (the
seam) → the ownership comment atop `src/llvm.rs` → `MEMORY_MANAGEMENT.md`
(research map) → `VALUE_MODEL.md` / `PICO_BACKEND.md` (value model, device
target). Run `tools/native_run.sh` and `tools/reuse_bench.sh` first — the
gates are the ground truth.
