# rust-p2w

A teaching subset of **Python that compiles ahead-of-time to native code with
no garbage collector** — Rust-class memory management (Perceus-style reference
counting + reuse, type-driven monomorphization, escape analysis) for a
kid-friendly language, targeting a **$7 microcontroller** (Raspberry Pi Pico 2 W /
RP2350) as well as the browser.

One front-end (lexer → parser → spanned AST) drives two **compiled** backends:

- **Browser:** AST → WAT / **WASM-GC** (~26 KB `.wasm`, ~9 KB gzipped per program).
- **Native (Pico):** AST → textual **LLVM IR** → `clang`/`lld`, with a small
  `no_std` runtime (`p2w-rt`). We compile — we don't ship an interpreter.

The headline, shipped and measured: an in-place comprehension over a uniquely-owned
packed array does **zero allocation** —

```python
data = [x * x for x in data]   # data: list[int]  →  in-place map, 0 allocs
```

guarded by a runtime `rc == 1` check so an aliased array still gets copy semantics
("functional but in-place", à la Perceus FBIP).

## Status

The native value model is **complete** — typed code lowers to register/buffer code
with no boxing and no refcount traffic:

- typed scalars `: int`/`: float` — native `add`/`mul`/`fdiv`/`icmp`/`fcmp`, int↔
  float promotion, params/returns/locals, `for` and `while` loops
- packed `list[int]`/`list[float]` — flat i32/f64 buffers, bounds-checked
- list & dict comprehensions (dynamic or packed, `if` filters, `range` sources)
- FBIP in-place reuse for the self-map; full-`i32` ints (no silent truncation)
- precise, validated RC (transfer-ownership insertion, borrow-on-read, borrowed
  params for read-only Boxed/array params)

Unannotated code stays a dynamic tagged-`i32` path — the typed paths are opt-in.

**Validated without hardware.** Because values are i32 arena *offsets* (not machine
pointers), the emitted IR + runtime compile with `clang` and run on the host.
`tools/native_run.sh` is a mechanical oracle: it runs each program through real
LLVM, diffs stdout against CPython, and asserts `p2w_live() == 0` (no leaks) at
exit — 60+ cases green. On-device flash/run (`.uf2`) + the temperature sensor is
the next hardware milestone.

## Quick start

```sh
cargo test                          # front-end + both emitters (lib + integration)
cargo run --example demo            # compile a sample program to WAT
bash tools/native_run.sh            # the host run-oracle (needs clang); GATE_LEAKS=1
(cd runtime && cargo test)          # the native runtime (p2w-rt)
```

## Layout

- `src/` — lexer, parser, AST, the two emitters (`codegen.rs` = WASM, `llvm.rs` =
  native), debugger, lints.
- `runtime/` — `p2w-rt`, the `no_std` native runtime (value rep, arena, RC,
  strings/lists/dicts/packed-arrays).
- `tools/native_run.sh` — the host correctness + alloc-count oracle.
- `examples/` — `demo` (→ WAT), `emit_ll` (→ LLVM IR, drives the oracle).

## Docs

- `docs/PITCH.md` — one-page technical pitch (audience: PL / compiler engineers).
- `docs/TASKS.md` — scoped frontier tasks for a contributor, each with the
  `live==0` oracle as acceptance gate.
- `VALUE_MODEL.md` — the boxed↔unboxed representation contract.
- `MEMORY_MANAGEMENT.md` — the memory model and the PL research it draws on.
- `PICO_BACKEND.md` — native backend design and status.

## License

MIT (see `LICENSE`). This is a Rust reimplementation of the MIT-licensed **p2w**
Python-subset compiler; attribution is in `NOTICE`.
