# Pico 2 W native backend — design & plan

> The eventual second backend: our AST → **LLVM** → bare-metal **RP2350**
> (Raspberry Pi Pico 2 / Pico 2 W). **We compile; we do not interpret** — no
> MicroPython, no on-device interpreter. This is the same compile-don't-interpret
> philosophy that produced the WASM-GC backend (see the project notes). Companion
> to `GRAMMAR_ARCHITECTURE.md` (one front-end, multiple backends) and
> `DEBUGGER_ARCHITECTURE.md` (the on-device debug transports).

## Why native, not MicroPython
We own the AST, so we can emit real machine code with DWARF debug info — which is
exactly what lets the *same* source-level debugger follow onto the board. Shipping
MicroPython would mean interpreting on-device and inheriting *its* debugger, the
opposite of the whole project. Cost: a native embedded backend is real
compiler-engineering work; this doc phases it so each step is shippable.

## Target
- **First target: Arm Cortex-M33** — triple `thumbv8m.main-none-eabihf` (the M33
  has an FPU + DSP). RP2350 has dual M33.
- **Later: RISC-V Hazard3** — RP2350 also has dual Hazard3 cores (you pick Arm or
  RISC-V at boot). `riscv32imac`-ish. LLVM targets both, so the front-end is
  unchanged; only the target triple + a little runtime differ.

## Pipeline
```
AST → LLVM IR (textual .ll)  →  llc  →  object (.o)
                              →  lld  →  ELF  (+ RP2350 boot block, linker script)
                              →  elf2uf2 / picotool  →  .uf2  →  drag-to-Pico
```
- **Emit textual LLVM IR**, the same way `codegen.rs` hand-emits WAT text. Rationale:
  zero heavy build dependency (no `inkwell`/LLVM-dev libs in *our* crate, which
  would break others' builds + CI); consistent with the existing emitter style;
  the LLVM/embedded toolchain (`llc`/`lld`/`picotool`) is invoked only in the
  device-build step, gated like the `e2e` tests so normal `cargo test` stays
  offline and fast.

## The crux: the on-device value model (the big open question)
The WASM backend leans on **WASM-GC** for heap objects (ints/strings/lists/dicts).
Bare metal has **no GC**. This is the hard part and the main multi-phase work:
- **Phase-0 (now): integers only.** `i32` values, arithmetic, `print(int)`. No
  heap. Proves the AST→LLVM-IR seam end to end as text.
- **Later: a tiny runtime + value representation.** Options to evaluate:
  - **Tagged union / NaN-boxing** of a dynamic `Value` (closest to today's
    semantics; needs a heap allocator + a strategy for strings/lists/dicts, and
    likely simple ref-counting or a bump/arena with scopes — full GC is probably
    overkill for kid programs).
  - **Monomorphization via the type annotations** (layers 3–4 already parse
    `: int` / `-> T`): typed functions compile to concrete machine types, far less
    runtime. Pairs beautifully with the typed-surfaces work.
  - Pragmatic mix: typed fast paths + a small boxed `Value` fallback.
- **Strings/lists/dicts** ride on whatever allocator we pick; start with a fixed
  arena, grow later.

## Value model — DECIDED (phase 3)
Every Python value is a uniform **tagged `i32`**, and **the emitter is
rep-agnostic**: it never assumes a bit layout — it only *calls* a small **runtime
ABI** of `p2w_*` functions that take/return `i32`. The device runtime (written
later in Rust/C for the target) owns the actual representation (low-bit tags on
an aligned word: small int inline, or a tagged pointer to a heap string/list/
dict) and the **allocator** (start with a bump arena reset per run; ref-counting
later — full GC is overkill for short kid programs). Same "box values + call
runtime ops" split the WASM backend uses (`ref.i31` + `py_add`).

**Why `i32`, not `i64`:** the universal value must be ≥ pointer width to hold a
heap reference; on the RP2350 a pointer is **32-bit**, so `i32` is the natural
word — one register, not two — and matches the established embedded/dynamic
runtimes (**MicroPython**'s word-sized `mp_obj_t`, **V8**'s 31-bit SMI). `i64`
would waste a register per value on a 32-bit core. (A value *narrower* than a
pointer — `i16`/`i8` — can't be the universal slot at all.) Trade-off: inline
ints are ~30-bit before the runtime promotes to a heap bignum — plenty for kids.

**Where `i8`/`i16`/`i32`-raw belong:** *not* the universal value, but the
**typed fast path** the annotations unlock — a `: int` local compiles to a raw
unboxed machine int (native `add`/`icmp`, no runtime call — literally Phase 2's
codegen reborn), and a typed *homogeneous* container (`list[int]`, bytes, audio,
pixels) packs into an `i8`/`i16`/`i32` array instead of boxed values (crucial on
a 520 KB MCU). So the design is two-tier: boxed `i32` for dynamic code, narrow
machine ints chosen *by type* for typed code. Correct first, fast later.

**Runtime ABI (the emitted IR `declare`s these):**
`p2w_int(i32)->i64`, `p2w_bool(i1)->i64`, `p2w_none()->i64`,
`p2w_str(ptr,i32)->i64`; `p2w_add/sub/mul/div/floordiv/mod/pow(i64,i64)->i64`,
`p2w_neg(i64)->i64`; `p2w_lt/le/gt/ge/eq/ne(i64,i64)->i64` (return a bool value);
`p2w_truthy(i64)->i1` (for conditions); `p2w_print(i64)`. Lists/dicts add
`p2w_list_new/append/get/set`, `p2w_dict_new/get/set`, `p2w_index`, `p2w_len`,
`p2w_iter*` next.

## Runtime & I/O (the host-import seam, on metal)
- `print` → a runtime `@p2w_print_int` / `@p2w_write_char` that pushes bytes over
  **USB-CDC** (the Pico's own cable) — the bare-metal mirror of the browser's
  `env.write_char`. Same seam, different host.
- **On-chip temperature sensor** → ADC read (`@p2w_read_temp`), answering the
  micro:bit "no sensors" gap; GPIO/PWM later.
- A minimal **startup**: vector table, reset handler, stack init, `.data`/`.bss`
  copy/zero, then call `@main`.

## Boot / image
RP2350's bootrom needs an **embedded block (IMAGE_DEF)** in flash to recognize the
image (and optionally a signature). Memory map: flash (XIP) at `0x10000000`, SRAM
at `0x20000000`. A linker script places the vector table + boot block; `picotool` /
`elf2uf2` produces the `.uf2`. (Details are a later phase.)

## Debugger
Reuses the `Vm`-shaped `DebugAdapter` surface from `DEBUGGER_ARCHITECTURE.md`:
- **USB stub** (no probe): the backend emits a `__step(line)` call per statement;
  an on-device stub talks the debug protocol over USB-CDC. Same step/watch UX as
  the browser.
- **SWD + probe-rs**: DWARF (emitted by LLVM) + DWT hardware watchpoints — the
  gold path. DWARF is a *reason* we emit via LLVM rather than MicroPython.

## Phases
0. **AST → textual LLVM IR for the integer subset** — ✅ done (`src/llvm.rs`,
   `compile_to_llvm_ir`). Text only; unit-tested by string assertions.
1. **Toolchain spike (gated test):** run `llc`+`lld` on the emitted `.ll`, link a
   tiny C/asm runtime + startup, produce an ELF, then a `.uf2`; smoke it (QEMU or
   a real board). Gated like `e2e`. *(Deferred — needs the LLVM/embedded
   toolchain installed; the emitter phases below are pure-Rust + offline-testable,
   so they went first.)*
2. **Control flow + functions** — ✅ done (block-structured: if/elif/else, while,
   counted for, break/continue, comparisons, `not`, user functions + recursion).
   (Originally `i32`; now superseded by the boxed model in phase 3.)
3. **Value model + runtime** — ✅ **decided + emitter converted.** Every value is
   a tagged `i32` (see the Value model section); the emitter is rep-agnostic and
   routes all value ops through the `p2w_*` runtime ABI. Done: ints, bools,
   **strings** (`p2w_str`), arithmetic (`+ - * / // % **`), comparisons, `not`,
   conditions (`p2w_truthy`), control flow, functions; **lists/dicts** (literals,
   subscript read/write via `p2w_index`/`p2w_setindex`, `len()`), **methods**
   (name-dispatched `p2w_method0/1/2`), and **for-each** (`p2w_iter`/`iter_has`/
   `iter_next`), and **`and`/`or`** (short-circuit, returns the deciding operand
   via a result slot). Vars are entry-block `alloca i32`. The emitter now covers
   the **full teaching subset** (sans tuples/comprehensions/classes). The
   **runtime crate** (`runtime/`, `p2w-rt`) is **started**: a `no_std`,
   host-testable impl of the `p2w_*` ABI over the concrete tagged-`i32` value
   rep (2-bit tag: small int / heap ptr / immediate singleton). Done so far —
   int/bool/None values, arithmetic (`+ - * // %`, neg), comparisons + equality,
   truthiness, `not`, and `print` (no-alloc int formatting). **Still to do:** the
   bump allocator + heap value types (strings/lists/dicts), float (which `/` and
   `**` need), and `p2w_putc` over USB-CDC. The emitted IR isn't runnable until
   the runtime is complete + the toolchain (phase 1) links it.
4. **I/O + peripherals:** USB-CDC `print`, the temp sensor, GPIO.
5. **Debug transports:** USB stub, then SWD/probe-rs + DWARF.
6. **RISC-V** target variant.

Design for the front-end (lexer/parser/AST) to stay 100% shared with the WASM
backend; only the emitter + runtime + toolchain differ.
