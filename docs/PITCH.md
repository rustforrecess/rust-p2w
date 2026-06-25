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

The headline — **functional-but-in-place comprehensions** — is no longer "building
toward," it's shipped and measured:

```python
data = [x * x for x in data]   # over a unique list[int]
```

compiles to an **in-place map: zero allocation** (the buffer is reused), guarded
by a runtime `rc==1` check so an aliased array still gets copy semantics. Measured:
2 allocs in-place vs 4 copying, on a 4-element map — a steady-state sensor loop
allocates nothing.

## Why it's not vaporware — the evidence

One front-end (lexer/parser → spanned AST) drives **two compiled backends** off
the same tree:

- **Browser:** AST → WAT/WASM-GC. ~26 KB `.wasm` / ~9 KB gzipped for a full
  program+runtime (~6.5× smaller than comparable Python-subset-in-WASM efforts).
- **Native (Pico):** AST → textual **LLVM IR** → `clang`/`lld` → RP2350. Values
  are tagged **i32 offsets into a static arena** (not machine pointers), which has
  a useful consequence below.

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
- **FBIP drop-reuse** for the self-map (above), `rc==1`-guarded.
- Ints are full `i32` (heap-boxed beyond the 30-bit inline range — no silent
  truncation). Unannotated code stays a dynamic tagged-`i32` path, byte-identical
  to before; the whole thing is opt-in via annotations.

Memory management is **precise and validated**: the emitter inserts retain/release
under a documented transfer-ownership model (with borrow-on-read and borrowed
parameters as landed optimizations); a small `no_std` runtime (`p2w-rt`)
implements the value rep + arena + strings/lists/dicts/packed-arrays/iteration +
RC. 27 runtime + 157 lib + 163 integration tests.

### The part compiler people tend to like

Because values are arena *offsets* rather than pointers, the emitted IR + runtime
are **host-portable**: `clang` compiles the IR, links the runtime, and the program
**runs on a normal dev machine** — no board, no QEMU. The runtime exposes a
live-object counter (`p2w_live()`) and an allocation counter (`p2w_allocs()`), so
`tools/native_run.sh` is a **mechanical correctness + cost oracle**: it compiles
each program through real LLVM, runs it, diffs stdout against CPython, asserts
**`live_objects == 0`** at exit, and lets us *measure* the reuse win in allocations.

**61 cases pass that gate**, all ending `live == 0`. It caught a real double-free
during bring-up (a dict-update freeing a key the runtime already owned). Memory-
safety work here is **verifiable, offline, in seconds** — not "looks right in
review."

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
acceptance gate (`live==0` + output diff + alloc/RC counts). See `docs/TASKS.md`.

1. **General Perceus reuse** — drop/reuse tokens beyond the self-map special case
   (in-place updates for any unique construction, FBIP in full).
2. **Full last-use (Perceus)** — release at last use via per-block liveness;
   shrinks lifetimes and unlocks more reuse.
3. **Reachability-type escape inference** — generalize today's syntactic
   borrowed-param analysis into proper flow-sensitive escape/ownership inference
   (more borrowed params, more reuse, a principled static tier).
4. **Verified RC pass** — the "language can't, the compiler can" angle: prove the
   insertion sound (RustBelt/VerusBelt lineage), turning the oracle's test-assured
   safety into proof-assured safety.

Grounding: Perceus (PLDI 2021) and drop-guided reuse; reachability types /
"Free to Move" (2025), Polymorphic Reachability Types (OOPSLA 2024); Tree Borrows
(PLDI 2025); VerusBelt (PLDI 2026). Full citations in `MEMORY_MANAGEMENT.md`.

## Status

Native backend covers the teaching subset (ints, floats, strings, lists, dicts,
packed `list[int/float]`, control flow, functions+recursion, iteration, methods,
list/dict comprehensions) with a **complete value model**, precise validated RC,
and FBIP in-place reuse. Host run-oracle green: **61 cases, `live == 0`**; tests:
157 lib, 27 runtime, 163 integration. **Next hardware milestone:** on-device
flash/run (`.uf2`) + the on-chip temperature sensor — the toolchain (`clang`/
`lld`) is in hand; it's gated on the board.
