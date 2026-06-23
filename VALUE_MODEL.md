# Value Model — typed monomorphization & the boxed↔unboxed contract

*Design note for Task 1 in `docs/TASKS.md`. Settle the contract here before
touching the emitter — same discipline as the RC ownership contract.*

## Goal & non-goals

**Goal.** Stop boxing everything. Use the type annotations the parser already
accepts (`x: int`, `-> T`, `xs: list[int]`) to represent annotated scalars
**unboxed** (raw machine values in registers) and homogeneous collections as
**packed arrays**, boxing only at the **dynamic boundary**. This is tier 1 of the
memory model ("don't allocate") and removes most RC traffic at the root — a boxed
int's retain/release are already no-ops; unboxing removes the box and the arena
bump entirely.

**Non-goals (v1).** No whole-program type inference. No generic
specialization-by-call-site (annotations fix signatures). No arbitrary-precision
ints. Unannotated code stays **byte-for-byte the current tagged-i32 path** — the
value model is strictly additive and opt-in.

## Representations

A value has a **representation (`Repr`)**, tracked by the emitter alongside its
LLVM operand:

| Repr | LLVM type | RC? | Notes |
|------|-----------|-----|-------|
| `Boxed` | `i32` (tagged) | yes if heap | today's universal value; the dynamic default |
| `Int(w)` | `i32`/`i64`/`i16`/`i8` | no | unboxed integer, width `w` |
| `Float` | `double` | no | unboxed f64 |
| `Bool` | `i1` | no | unboxed boolean |
| `IArray(elem)` | heap buffer of raw `elem` | the array yes, elems no | packed homogeneous list |

**Hazard to design around:** a `Boxed` `i32` and an unboxed `Int(32)` are *both*
`i32` in LLVM but mean different things (tagged vs raw). The emitter must therefore
carry `(operand, Repr)` everywhere — `expr()` returns a typed value, not a bare
string. This is the central refactor.

## What decides a representation (v1 scope — keep it small)

**Annotation-driven, with local bottom-up typing of expressions. No flow/join
analysis in v1.**

- **Literals:** int literal → `Int(default)`, float → `Float`, bool → `Bool`,
  str/list/dict/None → `Boxed`.
- **Annotated params & returns:** carry their declared `Repr` end to end. These
  **are already in the AST** (`Def.param_types: Vec<Option<Expr>>` parallel to
  `params`, and `Def.return_type: Option<Expr>`), so Phases A/B can proceed with
  no front-end change.
- **Annotated locals (`x: int = …`) are NOT yet in the AST** — `Assign` is
  `(String, Expr)` with no type. Unboxing a hot local like `total` therefore needs
  a small **front-end prerequisite**: add an optional type to `Assign` (or an
  `AnnAssign` variant) + parser support. Until then, v1 unboxing is driven by
  function signatures + literals/operator propagation only.
- **Operators propagate** the obvious result repr: `Int op Int → Int`,
  `Int +-*/ Float → Float` (promotion; `/` is always `Float`), comparisons →
  `Bool`, etc. If any operand is `Boxed`, the result is `Boxed` (fall back).
- **Unannotated names default to `Boxed`.** (Inferring unboxed *unannotated*
  locals — the join analysis for `if c: y=1 else: y=2`, etc. — is a deliberate
  future extension, not v1. Document that annotating a hot local unlocks the
  speedup; this keeps v1 predictable for both kids and the compiler hire.)

So: annotated ⇒ unboxed end-to-end; unannotated ⇒ boxed end-to-end; conversions
happen only where the two worlds meet (next section).

## The coexistence contract (the heart)

Two coercions, emitted **only at boundaries**:

**`unbox(v: Boxed) -> Repr`** — when a boxed value flows into a context with a
known unboxed type:
- a boxed arg passed to an annotated param; a boxed value in typed arithmetic.
- emits a tag check + extract (and a `trap` on type mismatch — a runtime
  TypeError). If the static type is already that `Repr`, **no unbox is emitted**.

**`box(v: Repr) -> Boxed`** — when an unboxed value flows into a dynamic sink:
- `print(x)`; an element of a heterogeneous/boxed list or dict; an arg to an
  un-annotated function; a value stored into an un-annotated (boxed) variable; the
  return value of a function whose return type is dynamic.
- `Int(w)` boxes to a tagged immediate when it fits the 30-bit small-int range,
  else to a **heap int box** (see decisions). `Float` boxes to the existing heap
  float. `Bool` → `V_TRUE`/`V_FALSE`.

**Rule of thumb:** unbox is "I know this is an int, prove it"; box is "hand this to
the dynamic world." Both vanish when both sides already agree on the repr — which
is the common case inside an annotated function, where the goal is *zero*
coercions on the hot path.

## RC interaction (this is the cleanup the value model buys)

Ownership/RC becomes **repr-aware**: only `Boxed` (and the `IArray` object itself)
participate. Concretely, in `src/llvm.rs`:
- `expr_borrow`, `release`, `emit_exit_releases`, container-transfer, etc. all
  check `Repr`: unboxed scalars are **never** retained/released and their slots
  are **not** released at exit (no code emitted, not just a runtime no-op).
- `box(Int)` to a heap int box produces an owned `Boxed` (enters the RC world);
  `unbox` of a heap-boxed value borrows then is released per the usual rules.
- A packed `IArray` is one heap object whose *elements* are raw (no per-element
  RC) — cheaper to free than a boxed list (no child release loop).

Net: the leftover no-op RC ops the borrow-on-read pass left behind disappear,
because the ints that generated them are no longer boxed.

## Decisions to lock before coding

1. **Default int width for `: int`.** Recommend **`i32`** (one RP2350 register;
   matches the device word; `i64` available via explicit annotation later).
2. **Overflow semantics** (typed ints are fixed-width, unlike Python's bignum).
   Recommend **wraparound by default** (free, matches the hardware/C mental model)
   with a future `--checked` mode that traps — pedagogically honest when debugging.
   *This is a real divergence from CPython; document it for learners.*
3. **Boxing a full-width int.** The boxed immediate is only 30-bit, so boxing an
   unboxed `i32` outside that range needs a **heap int box (`T_INT`)** fallback.
   Recommend adding it (rare path; keeps boxing total and semantics clean) rather
   than clamping unboxed ints to 30-bit.
4. **Unboxed unannotated locals.** v1: **no** (boxed). Revisit as a join/liveness
   extension (pairs naturally with Task 2 last-use).

## Phasing

- **A — unboxed scalars** (`Int`, `Float`, `Bool`) + the `(operand, Repr)` emitter
  refactor + `box`/`unbox` at boundaries + repr-aware RC. *The bulk of the
  speed/RAM win (arithmetic, loops).* Validate: annotated int loop emits no
  `p2w_int`/RC in its body; output matches; `live==0`.
- **B — typed function signatures.** Annotated params/returns become unboxed in
  the LLVM signature; callers coerce at the call. (Recursion like `fib(n: int)`
  becomes raw-`i32` throughout.)
- **C — packed arrays** (`list[int]`/`list[float]`): a raw-element collection;
  index/append/iterate stay unboxed; box only on escape to a dynamic sink. *The
  memory win for sensor logs / game state.*

## Validation & backward compatibility

- The host run-oracle (`tools/native_run.sh`) gates both worlds: existing
  unannotated cases must stay **bit-identical** (regression guard), and new typed
  cases assert correct output + `live==0`. Add the **before/after numbers** the
  pitch promises: alloc count and RC-op count for an annotated loop vs its
  unannotated twin.
- Because unannotated code is untouched, the blast radius is the new typed paths
  only — and every typed path has an oracle case.

## Touch points

- `src/llvm.rs`: `expr()` returns `(String, Repr)`; new `box`/`unbox` helpers;
  repr-aware ownership; typed function signatures; annotated `var_slot` typing.
- `runtime/src/lib.rs`: heap int box (`T_INT`) if decision 3 is yes; packed-array
  type + ops for phase C; typed extract/construct ABI as needed.
- Annotations: surfaced from the parser's existing `: T` / `-> T` into the AST the
  emitter reads (confirm what's already carried vs dropped).
- Related: `MEMORY_MANAGEMENT.md` (tier 1), `RC_PASS_TODO.md` (RC is now
  repr-aware), `docs/TASKS.md` Task 1.
