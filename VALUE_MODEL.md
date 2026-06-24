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
- **Annotated locals (`x: int = …`) — DONE.** Added an `AnnAssign` AST variant +
  parser support (`name: T = value`); other backends treat it like `Assign`, the
  native one uses the annotation to pick the slot repr. So a hot local like
  `total: int = 0` is an unboxed Int slot, and a `while` loop with annotated
  locals + native compare/arith is **fully native** (no boxing, no runtime calls):
  `def s(n: int): total: int = 0; i: int = 0; while i < n: total = total + i;
  i = i + 1; return total` → a tight `icmp`/`add` loop. (Bare `x: int` without a
  value, and unboxing *unannotated* locals via inference, remain future work.)
- **Native counted-`for` — DONE.** `for i in range(...)` now uses an unboxed i32
  counter: native `icmp` guard + raw `add` increment (ascending `slt`/`+step`,
  descending `sgt`/`+negstep`), bound held as a raw i32. So `for i in range(n):
  total = total + i` with `total: int` is a fully native loop (zero runtime calls
  in the body).
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
3. **Boxing a full-width int. DONE.** The inline tagged int is 30-bit, so values
   outside ±2^29 now box to a heap `T_INT` (`[tag][rc][i32]`) instead of
   truncating. `make_int` wraps to `i32` (the value model's int width, matching
   native unboxed arithmetic) then picks inline-or-heap; `is_int`/`as_int` are
   heap-aware, so the whole numeric tower (arith/compare/print/eq/truthy) and
   `p2w_unbox_int` cover both forms with no emitter change (a heap int is an
   ordinary Boxed/refcounted value). `x = 2000000000; print(x)` → `2000000000`
   (was truncated), `live==0`. Ints are now consistently full `i32`.
4. **Unboxed unannotated locals.** v1: **no** (boxed). Revisit as a join/liveness
   extension (pairs naturally with Task 2 last-use).

## Phasing

- **A — unboxed scalars** (`Int`, `Float`, `Bool`) + the `(operand, Repr)` emitter
  refactor + `box`/`unbox` at boundaries + repr-aware RC. *The bulk of the
  speed/RAM win (arithmetic, loops).* Validate: annotated int loop emits no
  `p2w_int`/RC in its body; output matches; `live==0`.
  - *Done so far:* the `(operand, Repr)` plumbing (`expr_typed`/`as_boxed`,
    bit-identical) and **native unboxed integer `+`/`-`/`*`** — `print(2 + 3 * 4)`
    emits raw `add`/`mul i32` and boxes once at `print` (5 runtime calls → 1).
    `Float`, `//`/`%`, and unboxed *variables* (which need Phase B typed slots)
    remain. **Native integer comparisons + `Bool` repr also done:** `<`/`<=`/`>`/
    `>=`/`==`/`!=` on ints emit a raw `icmp` (unboxed `i1`), used directly as a
    branch condition with no `p2w_truthy`. So `if n < 2:` in a typed function is a
    single `icmp` — `def fact(n: int)` now has a fully native body (icmp + mul,
    zero runtime calls).
- **B — typed function signatures.** Annotated params/returns become unboxed in
  the LLVM signature; callers coerce at the call. (Recursion like `fib(n: int)`
  becomes raw-`i32` throughout.) **Int AND float DONE:** `Repr` now drives the
  LLVM slot/param/return type via `llvm_ty` (`Float`→`double`, else `i32`).
  `def dbl(x: float) -> float: return x * 2.0` is a native `double @dbl(double)`
  (alloca double / fmul / ret double); int args promote to float params (`sitofp`).
  Float locals (`total: float = 0.0`) get `alloca double` slots. The full scalar
  story (int + float: literals, arithmetic, comparisons, params, returns, locals,
  both loops) is now native, boxing only at dynamic sinks.
  - *Done (Int):* `: int` params get raw slots and `-> int` returns raw; the body
    runs native (`def sq(n: int) -> int: return n*n` emits just `load`/`mul`/`ret`
    — **zero** runtime calls). Coercions at the boundaries via `coerce` (box =
    `p2w_int`, unbox = new runtime `p2w_unbox_int`): boxed arg → int param unboxes,
    int result → `print` boxes. Validated: oracle `typedsq`/`typedfact`/
    `typedboxarg`/`typedreassign`, all `live==0`. *Float params (LLVM `double`
    signature) remain.* Note: boxing a >30-bit unboxed int is lossy until the
    heap-int box (decision 3) lands.
- **C — packed arrays.** `list[int]` DONE: a `Repr::IntArray` (heap ref, refcounted
  like Boxed) backed by the runtime `T_IARRAY` (flat i32 buffer, no per-element
  refcount). `xs: list[int] = [...]` builds packed (target type drives literal
  construction, incl. as a call arg); `xs[i]` read/write and `xs.append(n)` use
  the raw `p2w_iarray_*` ABI (bounds-checked); `for x in xs` lowers to a native
  index loop with raw `get`; `list[int]` params transfer (callee releases). Boxes
  only on escape (print handles `T_IARRAY`). Validated: oracle iarray/iarraysum/
  iarrayappend/iarrayset/iarrayliteralarg, all `live==0`. *`list[float]` remains
  (adds the f64 element width).*

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
