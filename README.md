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

The headline, shipped and measured — the **Perceus drop-reuse tier**: values are
released at their last mention, and a death right before a matching allocation
becomes an **in-place update** ("functional but in-place", FBIP), always guarded
by a runtime `rc == 1` check so aliased data keeps copy semantics:

```python
a = [1, 2, 3]                  # a 3-stage pipeline runs in ONE buffer:
b = [x + 1 for x in a]         # b is built inside a's dying buffer
c = [y * 2 for y in b]         # c inside b's — 10 allocs naive → 3
```

| program shape | naive RC | reuse tier |
|---|---|---|
| 3-stage comprehension pipeline | 10 allocs | **3** |
| 3× list reassignment | 6 | **2** |
| 8× string-append loop | 17 | **4** (in-place growth + interned literals) |

Plus a **Component-Model proof**: the same compiled Python builds as a
linear-memory WASM **component** (no WASM-GC) that runs correct and leak-free
in a real component host (`tools/wasm_poc.sh`) — the guest shape a
sandboxed-activity standard (PXC) needs.

## Status

The native value model is **complete** — typed code lowers to register/buffer code
with no boxing and no refcount traffic:

- typed scalars `: int`/`: float` — native `add`/`mul`/`fdiv`/`icmp`/`fcmp`, int↔
  float promotion, params/returns/locals, `for` and `while` loops
- packed `list[int]`/`list[float]` — flat i32/f64 buffers, bounds-checked
- list & dict comprehensions (dynamic or packed, `if` filters, `range` sources)
- **the drop-reuse tier**: last-mention liveness + precise drops
  (`src/reuse.rs`), dying-source map reuse, literal-reassignment reuse,
  `x = x + e` in-place growth (2× slack, amortized), per-site interned
  literals, slice-steal (`s = s[1:]` peel loops compact in place), and reuse
  across `if`/`else` join points; full-`i32` ints (no silent truncation)
- **type inference (no annotations needed)**: typed-call comprehension
  elements steal dying buffers, and unannotated scalars (`x = 5`, `t = t + i`)
  get raw i32/f64 slots via a demote-on-conflict join — type churn
  (`x = 1` then `x = "hi"`) keeps the dynamic path, output CPython-identical
- precise, validated RC (transfer-ownership insertion, borrow-on-read, borrowed
  params for read-only Boxed/array params)
- the broader subset too — slices, f-strings, **sets** (set theory + methods,
  sorted display) and **real immutable tuples** (so sets reject mutable members,
  like CPython) — all consistent across the WASM, native, and step-debugger paths

Unannotated code stays a dynamic tagged-`i32` path — the typed paths are opt-in.

**Validated without hardware, three ways.** Because values are i32 arena
*offsets* (not machine pointers), the emitted IR + runtime compile with `clang`
and run on the host. `tools/native_run.sh` is a mechanical oracle: real LLVM,
stdout diffed against CPython, `p2w_live() == 0` asserted at exit — **166 cases
green**, including adversaries that attack each reuse guard.
`tools/reuse_bench.sh` measures allocs/peak so wins are numbers, not claims.
`tools/fuzz_native.sh` differential-fuzzes generated programs against CPython
(dependency-free, seeded, reuse-shape-weighted incl. slices — 200 seeds green).

**Compiles to the board's CPU.** That same IR cross-compiles to **Cortex-M33**
(`clang --target=thumbv8m.main-none-eabi`), the runtime builds for the target, and
they link into a complete ~8–9 KB Cortex-M33 ELF — `tools/pico_build.sh`, no board
needed (a typed `n * n` becomes a single `mul r0, r0, r0`). On-device flash/run
(`.uf2` + bootrom block) + the temperature sensor is the next, hardware-gated step.

## Quick start

```sh
cargo test                          # front-end + both emitters (lib + integration)
cargo run --example demo            # compile a sample program to WAT
bash tools/native_run.sh            # the host run-oracle (needs clang); GATE_LEAKS=1
bash tools/pico_build.sh            # cross-compile+link to a Cortex-M33 ELF (clang/lld)
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

- `docs/PYTHON_COMPAT.md` — the supported Python subset and where it differs from
  CPython (sets, integers, cycles, current gaps).
- `docs/COMPILER_FRONTIER.md` — the pitch + scoped open tasks for a PL/compilers
  contributor (each with an interface and an executable acceptance gate).
- `docs/REUSE_PLAN.md` — the Perceus staging, invariants, and the three
  acceptance nets (oracle / bench / fuzzer).
- `docs/INTERACTIVE_WEB.md` / `docs/RICH_OUTPUT.md` — the browser capability
  palette (DOM/SVG/audio/timers) and the `emit_html`/`show()` rich-output
  channel.
- `VALUE_MODEL.md` — the boxed↔unboxed representation contract.
- `MEMORY_MANAGEMENT.md` — the memory model and the PL research it draws on.
- `PICO_BACKEND.md` — native backend design and status.

## License

MIT (see `LICENSE`). This is a Rust reimplementation of the MIT-licensed **p2w**
Python-subset compiler; attribution is in `NOTICE`.
