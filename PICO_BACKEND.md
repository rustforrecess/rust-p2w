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
0. **AST → textual LLVM IR for the integer subset** — ✅ *this commit*
   (`src/llvm.rs`, `compile_to_llvm_ir`). Text only; unit-tested by string
   assertions. No device binary yet.
1. **Toolchain spike (gated test):** run `llc`+`lld` on the emitted `.ll` for the
   integer subset, link a tiny C/asm runtime + startup, produce an ELF, then a
   `.uf2`; smoke it (QEMU or a real board). Gated like `e2e`.
2. **Control flow + functions** in the LLVM emitter (if/while/for, calls) — the
   integer-typed slice of the language.
3. **Value model + runtime** (the crux above): allocator, strings, lists, dicts.
4. **I/O + peripherals:** USB-CDC `print`, the temp sensor, GPIO.
5. **Debug transports:** USB stub, then SWD/probe-rs + DWARF.
6. **RISC-V** target variant.

Design for the front-end (lexer/parser/AST) to stay 100% shared with the WASM
backend; only the emitter + runtime + toolchain differ.
