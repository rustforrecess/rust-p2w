# rust-p2w — a no-GC Python that compiles to bare metal

*One-page technical pitch. Audience: systems / PL / compiler engineers.*

## Thesis

A teaching subset of **Python that compiles — ahead of time — to native code with
Rust-class memory management and no garbage collector**, targeting a **$7
microcontroller** (Raspberry Pi Pico 2 W / RP2350, Cortex-M33).

The interesting claim isn't "another block-coding tool." It's the *combination*:
Python's ergonomics, **no GC and no runtime interpreter**, and a memory model
built from published PL research — **Perceus-style precise reference counting +
reuse, type-driven monomorphization, escape analysis** — applied somewhere that
combination hasn't been: a kid-friendly language on bare metal.

The headline — **the Perceus drop-reuse tier** — is no longer "building
toward," it's shipped and measured:

```python
a = [1, 2, 3]
b = [x + 1 for x in a]     # b is built inside a's dying buffer
c = [y * 2 for y in b]     # and c inside b's — the pipeline runs in ONE buffer
```

Values are released at their **last mention** (a liveness pass), and a death
right before a matching allocation becomes an **in-place update**, always
guarded by a runtime `rc==1` check so aliased data keeps copy semantics.
Measured: a 3-stage pipeline **10 → 3 allocs**, 3× list reassignment **6 → 2**,
an 8× string-append loop **17 → 4** (in-place growth + interned literals) — a
steady-state sensor loop allocates nothing. The same compiled Python also
builds as a **linear-memory WASM Component-Model component** (no WASM-GC) that
runs correct and leak-free in a real component host.

## Why it's not vaporware — the evidence

One front-end (lexer/parser → spanned AST) drives **two compiled backends** off
the same tree:

- **Browser:** AST → WAT/WASM-GC. ~26 KB `.wasm` / ~9 KB gzipped for a full
  program+runtime (~6.5× smaller than comparable Python-subset-in-WASM efforts).
- **Native (Pico):** AST → textual **LLVM IR** → `clang`/`lld` → RP2350. Values
  are tagged **i32 offsets into a static arena** (not machine pointers), which has
  a useful consequence below. **This cross-compiles and links to a real Cortex-M33
  image today** (see the next section) — not a plan, a built artifact.

Plus a third execution path off the same AST — a **step-debugger interpreter**
(`p2w-rt`-independent) kept at parity with the compiled backends, so Run (WASM),
Debug (interpreter), and the native target agree on semantics (sets, tuples, …).

**The native value model is complete — typed code lowers to register-and-buffer
code, no boxing, no refcount traffic:**

- **Typed scalars** (`: int`/`: float`): native `add`/`mul`/`fdiv`/`icmp`/`fcmp`,
  int↔float promotion, params/returns/locals, and *both* loop forms. `def fact(n:
  int) -> int: ...` emits `icmp` + `mul` — **zero runtime calls**, identical to C.
- **Packed arrays** (`list[int]`/`list[float]`): a flat `i32`/`f64` buffer, not
  boxed elements — construct/index/append/iterate/param, bounds-checked. The RAM
  win for sensor logs and game state.
- **Comprehensions** (list + dict): dynamic *or* packed (target-typed), with `if`
  filters and `range` sources; compose with promotion and true division.
- **FBIP drop-reuse in six forms**, all `rc==1`-guarded: dying-source map
  chains, literal reassignment, `x = x + e` in-place growth (2× slack,
  amortized), per-site interned literals (a loop's literal materializes
  once; the cache's permanent +1 doubles as the mutation guard), slice-steal
  (`s = s[1:]` peel loops compact in place), and reuse across `if`/`else`
  join points (the dying source's token follows into each arm).
- Ints are full `i32` (heap-boxed beyond the 30-bit inline range — no silent
  truncation). Unannotated code stays a dynamic tagged-`i32` path, byte-identical
  to before; the whole thing is opt-in via annotations.

The subset is broad and **consistent across all three execution paths**: ints,
floats, strings, lists, dicts, packed arrays, control flow, functions+recursion
(+ default/keyword args), iteration, methods, list/dict comprehensions, slices,
f-strings (incl. format specs), `input()`, **classes** (v1: attrs, methods,
single inheritance, `super()`, `__repr__`/`__str__`, class variables, and
the operator dunders — compile-time switch dispatch, no vtables), **sets** (full set theory + methods, sorted canonical
display), and **real tuples** — a distinct *immutable* type (item-assignment is
an error), which also lets sets reject mutable members (`{[1,2]}` → "use a
tuple") exactly like CPython.

Memory management is **precise and validated**: the emitter inserts retain/release
under a documented transfer-ownership model (with borrow-on-read and borrowed
parameters as landed optimizations); a small `no_std` runtime (`p2w-rt`)
implements the value rep + arena + strings/lists/dicts/packed-arrays/iteration +
RC. 35 runtime + 228 lib + 165 integration tests.

### It compiles to the board's actual CPU — shown, not promised

The same LLVM IR the host oracle runs **cross-compiles to Cortex-M33** with
`clang --target=thumbv8m.main-none-eabi -mcpu=cortex-m33`, the `p2w-rt` runtime
builds for that target, and program + runtime + bare-metal glue **link with
`ld.lld` into a complete Cortex-M33 ELF** — repeatably, via `tools/pico_build.sh`,
with no board. Two things worth seeing:

- A typed `def sq(n: int): return n * n` lowers to a **single `mul r0, r0, r0`** —
  no boxing, no runtime call. The value model isn't just "no GC"; on typed paths
  it's *no runtime at all*.
- The whole image is **~8–9 KB of flash** (code + initialised data); the arena is
  zero-init BSS in RAM.

What's left is genuinely hardware-gated and honestly scoped (`PICO_BACKEND.md`):
the RP2350 bootrom metadata block, clock/UART init, `.uf2` packaging, and the
on-device flash/run — each needs a board + `picotool` to validate, not more
compiler work.

### The part compiler people tend to like

Because values are arena *offsets* rather than pointers, the emitted IR + runtime
are **host-portable**: `clang` compiles the IR, links the runtime, and the program
**runs on a normal dev machine** — no board, no QEMU. The runtime exposes a
live-object counter (`p2w_live()`) and an allocation counter (`p2w_allocs()`), so
`tools/native_run.sh` is a **mechanical correctness + cost oracle**: it compiles
each program through real LLVM, runs it, diffs stdout against CPython, asserts
**`live_objects == 0`** at exit, and lets us *measure* the reuse win in allocations.

**195 cases pass that gate**, all ending `live == 0` — including adversaries
that attack each reuse guard (aliased sources, borrowed-param theft, freed-cell
corruption). It caught a real double-free during bring-up (a dict-update
freeing a key the runtime already owned). A **differential fuzzer**
(`tools/fuzz_native.sh`) generates fresh programs and diffs them against
CPython — 200 seeds green. Memory-safety work here is **verifiable, offline,
in seconds** — not "looks right in review."

## Architecture, at a glance

```
source ─► lexer ─► parser ─► spanned AST ─┬─► WAT/WASM-GC emitter   (browser)
                                          └─► LLVM-IR emitter + p2w-rt (Pico)
                  run-oracle: IR ─► clang ─► link p2w-rt ─► run ─► live==0 + alloc count
```

- Value model + ownership contract: `VALUE_MODEL.md`, and `src/llvm.rs` (above
  `FuncEmitter`).
- Memory model + the research it draws on: `MEMORY_MANAGEMENT.md`.
- Native backend plan + status: `PICO_BACKEND.md`.

## Open problems (where a compiler person would have real impact)

Each sits on the **finished** value model, has a published spec, and a ready-made
acceptance gate (`live==0` + output diff + alloc/peak counts + the fuzzer). The
scoped list with interfaces and acceptance criteria is
**`docs/COMPILER_FRONTIER.md`**; the headline items:

1. **Full backward liveness** — upgrade last-mention to release *before* a
   reassignment and inside branches; more deaths, more reuse. (Type inference
   landed: typed-call elements reuse, and unannotated scalars get raw slots
   via a demote-on-conflict join — the next inference rung is container slot
   inference with mutation-site constraints.)
2. **Reachability-type escape inference** — generalize the borrowed-param
   analysis into flow-sensitive escape/ownership inference.
3. **Cycle handling** — trial deletion over type-limited candidates (design
   sketched from Nim ORC); gates making linear-memory the default everywhere.
4. **Verified RC pass** — prove the insertion sound (RustBelt/VerusBelt
   lineage), turning test-assured safety into proof-assured safety.

Grounding: Perceus (PLDI 2021) and drop-guided reuse; reachability types /
"Free to Move" (2025), Polymorphic Reachability Types (OOPSLA 2024); Tree Borrows
(PLDI 2025); VerusBelt (PLDI 2026). Full citations in `MEMORY_MANAGEMENT.md`.

## Status

Native backend covers the teaching subset (ints, floats, strings, lists, dicts,
packed `list[int/float]`, control flow, functions+recursion, iteration, methods,
list/dict comprehensions, slices, f-strings, sets, immutable tuples) with a
**complete value model**, precise validated RC, and the **full drop-reuse
tier** (last-mention liveness, precise drops, four reuse forms — the original
reuse wishlist is closed). Host run-oracle green: **195 cases, `live == 0`**;
217 lib + 31 runtime tests; differential fuzzer at 120 seeds green.
**Cross-compiles + links to a Cortex-M33 image** (`tools/pico_build.sh`,
~8–9 KB) and **builds + runs as a linear-memory WASM component**
(`tools/wasm_poc.sh`). **Next hardware milestone:** on-device flash/run
(`.uf2`) + the on-chip temperature sensor — toolchain in hand, gated only on
the board.
