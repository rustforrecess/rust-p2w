# rust-p2w — a no-GC Python that compiles to bare metal

*One-page technical pitch. Audience: systems / PL / compiler engineers.*

## Thesis

A teaching subset of **Python that compiles — ahead of time — to native code with
Rust-class memory management and no garbage collector**, proven on a **$7
microcontroller** (Raspberry Pi Pico 2 W / RP2350, Cortex-M33).

The interesting claim isn't "another block-coding tool." It's the *combination*:
Python's ergonomics, **no GC and no runtime interpreter**, and a memory model
built from published PL research — **Perceus-style precise reference counting +
reachability-type escape analysis + type-driven monomorphization** — applied
somewhere that combination hasn't been: a kid-friendly language on bare metal.

The headline we're building toward: **functional-but-in-place comprehensions on a
microcontroller.** `readings = [scale(r) for r in readings]` reusing the input
buffer in place — zero steady-state allocation in a sensor loop, written in
ordinary Python.

## Why it's not vaporware — the evidence

One front-end (lexer/parser → spanned AST) drives **two compiled backends** off
the same tree:

- **Browser:** AST → WAT/WASM-GC. ~26 KB `.wasm` / ~9 KB gzipped for a full
  program+runtime (~6.5× smaller than comparable Python-subset-in-WASM efforts).
- **Native (Pico):** AST → textual **LLVM IR** → `clang`/`lld` → RP2350. Values
  are tagged **i32 offsets into a static arena** (not machine pointers), which has
  a useful consequence below.

Memory management on the native side is **already correct and validated**:

- A small `no_std` runtime (`p2w-rt`) implements the value rep + a first-fit arena
  + strings/lists/dicts/iteration/methods + precise RC. 23 host tests.
- The **emitter inserts retain/release** under a documented transfer ownership
  model (every expr owned; borrowing ops release temps; transfer sites move
  ownership; scope exit releases slots + loop temps).
- Two optimizations already landed: **borrow-on-read** (a name read into a
  borrowing op touches no refcount) and **borrowed parameters** (a conservative
  escape analysis lets read-only params skip RC entirely).

### The part compiler people tend to like

Because values are arena *offsets* rather than pointers, the emitted IR + runtime
are **host-portable**: `clang` compiles the IR, links the runtime, and the program
**runs on a normal dev machine** — no board, no QEMU. On top of that the runtime
exposes a live-object counter (`p2w_live()`), so `tools/native_run.sh` is a
**mechanical correctness oracle**: it compiles each program through real LLVM,
runs it, diffs stdout against CPython, and asserts **`live_objects == 0`** at exit.

That gate caught a real double-free during bring-up (a dict-update freeing a key
the runtime already owned). **Memory-safety work here is verifiable, offline, in
seconds** — not "looks right in review."

## Architecture, at a glance

```
source ─► lexer ─► parser ─► spanned AST ─┬─► WAT/WASM-GC emitter   (browser)
                                          └─► LLVM-IR emitter + p2w-rt (Pico)
                              run-oracle: IR ─► clang ─► link p2w-rt ─► run ─► live==0
```

- Ownership contract documented in `src/llvm.rs` (above `FuncEmitter`).
- Memory model + the research it draws on: `MEMORY_MANAGEMENT.md`.
- Native backend plan + status: `PICO_BACKEND.md`. RC plan: `RC_PASS_TODO.md`.

## Open problems (where a compiler person would have real impact)

Each sits on the finished value model, has a published spec, and a ready-made
acceptance gate (`live==0` + output diff + alloc/RC counts). See `docs/TASKS.md`.

1. **Type-driven monomorphization** — unbox annotated scalars (`i32/i64/i16/i8`)
   and pack homogeneous arrays; box only at the dynamic boundary. The foundation
   that makes "compiled" a real win and removes most RC traffic at the root.
2. **Full last-use (Perceus)** — release at last use via per-block liveness; the
   enabler for in-place reuse.
3. **Drop-reuse / FBIP over comprehensions** — reuse a unique, dead buffer in
   place instead of free+alloc. The "functional but in-place" win, on bare metal.
4. **Reachability-type escape inference** — generalize today's syntactic
   borrowed-param analysis into proper flow-sensitive escape/ownership inference.

Grounding: Perceus (PLDI 2021) and drop-guided reuse; reachability types /
"Free to Move" (2025), Polymorphic Reachability Types (OOPSLA 2024); Tree Borrows
(PLDI 2025). Full citations in `MEMORY_MANAGEMENT.md`.

## Status

Native backend covers the teaching subset (ints, floats, strings, lists, dicts,
control flow, functions+recursion, iteration, methods) with correct, validated RC.
Host run-oracle green (25 cases, `live==0`). Remaining: the value model, then the
FBIP frontier above; on-device flash/run (`.uf2`) is the next hardware milestone.
