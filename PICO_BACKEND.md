# Pico 2 W native backend — design & plan

> The eventual second backend: our AST → **LLVM** → bare-metal **RP2350**
> (Raspberry Pi Pico 2 / Pico 2 W). **We compile; we do not interpret** — no
> MicroPython, no on-device interpreter. This is the same compile-don't-interpret
> philosophy that produced the WASM-GC backend (see the project notes). Companion
> to `GRAMMAR_ARCHITECTURE.md` (one front-end, multiple backends) and
> `DEBUGGER_ARCHITECTURE.md` (the on-device debug transports).

## Status (verified, Jun 2026)
The compile-and-link path to the real board's CPU is **proven end to end**, off
the host with no board:

- **IR → Cortex-M33 machine code.** The same LLVM IR the host run-oracle uses
  cross-compiles with `clang --target=thumbv8m.main-none-eabi -mcpu=cortex-m33`.
  Typed paths lower to native instructions with no boxing — `def sq(n: int): return
  n * n` becomes a single `mul r0, r0, r0`.
- **Runtime builds for the device.** `p2w-rt` compiles for
  `thumbv8m.main-none-eabi` (`rustup target add` once) — `core` + compiler-builtins
  resolve, no `std`.
- **Full image links.** Program + runtime + bare-metal glue (`device/boot.c`
  vector table/reset + UART `p2w_putc`, `device/rp2350.ld` flash/RAM map) link with
  `ld.lld` into a complete Cortex-M33 ELF. Footprint is **~8–9 KB** of flash
  (code + data); the 64 KB arena is NOBITS BSS in RAM.
- Repeatable via **`tools/pico_build.sh`** (skips cleanly without clang/lld or the
  Rust target). The device-build counterpart of `tools/native_run.sh`.

### Hardware-gated work — status now that a board exists (2026-10-17)
1. ✅ **RP2350 bootrom IMAGE_DEF block — DONE, verified on real hardware.**
   `device/boot.c` embeds the minimum valid 20-byte/5-word block from RP2350
   datasheet section 5.9.5.1 (`PICOBIN_BLOCK_MARKER_START` /
   `ITEM_1BS_IMAGE_TYPE` / `ITEM_2BS_LAST` / self-loop / `BLOCK_MARKER_END`),
   placed via a new `.image_def` linker section right after `.vectors` in
   `device/rp2350.ld`. `picotool info -a` confirms it decodes correctly
   (`target chip: RP2350`, `image type: ARM Secure`, next-block address ==
   its own address). **Flashed to a real Pico 2 W: the board booted —
   the RPI-RP2 bootloader drive disappeared and did not reappear (no crash
   loop).** This is the first p2w-compiled program to run on physical
   hardware.
2. ✅ **Clock + UART pin init — DONE, verified on real hardware (2026-11-05).**
   `device/boot.c` runs `clocks_and_uart_init` before `main`: starts the 12 MHz
   crystal with an explicit `XOSC_STARTUP` delay (the field is undefined at
   reset), switches `clk_ref` onto it (ROSC, what the chip boots from, is
   documented as too imprecise for "frequency sensitive interfaces such as USB
   or UART" — datasheet 8.1), enables `clk_peri`, and only then releases
   UART0/IO_BANK0/PADS_BANK0 from reset, matching the SDK's clocks-first order.
   It routes UART0 TX/RX onto GPIO0/GPIO1 and programs 115200 8N1 off the
   precise 12 MHz (no PLL needed for this milestone).

   Confirmed with a Raspberry Pi Debug Probe on GP0 + GND: a p2w program printing
   a counter in a `while True` loop ran at full line rate (about 11.5 kB/s), and
   8,200 consecutive numbers in a capture stepped by exactly 1. Register
   addresses and fields were checked against the pico-sdk RP2350 headers.

   Two things to know when repeating this. A program that prints with no gaps
   gives the receiver no idle line to sync on, so attaching mid-stream can lock
   onto the wrong start bit: the bytes look like garbage but are regular, and
   re-framing the bit stream recovers the text. And the Pico 2 W's LED is on the
   radio chip, so UART is the simplest sign of life.
3. ✅ **`.uf2` packaging — DONE.** `picotool uf2 convert <elf> <uf2> --family
   rp2350-arm-s` produces a real, flashable `.uf2` from the metadata-bearing
   ELF. `picotool` itself builds clean with no `sudo`/system packages at all
   — `-DPICOTOOL_NO_LIBUSB=1` (we only need offline file conversion, not
   live USB device access) plus `cmake` via a user-local `pixi` install;
   `PICO_SDK_PATH` only needs a plain `git clone` for its header-only
   `add_subdirectory` targets, no build of the SDK itself.

### On the board: `input()` and `read_temperature()` (2026-11-05)
Both verified on a Pico 2 W through a Debug Probe.

- **`input()`** reads UART0 and echoes as a terminal driver would. A serial line
  has no end-of-input, so it waits for the next byte, and CR counts as Enter.
- **`read_temperature()`** returns degrees C as a float: the on-chip sensor, ADC
  input 4, averaged over 64 samples. Every other host reports the fixed
  `SIMULATED_TEMPERATURE_C` (21.0), so backends and lessons stay in agreement.

Measured: 60 readings had a standard deviation of 0.27 C, down from 0.87 C before
the sampling fix. A fingertip on the chip took a flat 18.5 C to 21.5 C within
about 25 seconds, falling back slowly on release.

Known limits. The absolute value is uncalibrated: it uses the SDK's nominal
formula and a nominal 3.3 V reference, and it measures the die, not the room. The
first reading after power-up runs 1 to 3 C high, so a lesson should discard it.
The ADC runs from the 12 MHz crystal, not the 48 MHz the SDK uses.

### On the board: `every()` and the robot scheduler (2026-11-08)

A program registers callbacks and returns (`docs/ROBOT.md`); `boot.c` then runs
a scheduler forever. Handlers cross as dispatch ids: the module's
`p2w_dispatch(id)` switches to the handler, numbered like the WASM backend's
`__dispatch`, so both backends agree on what an id means.

- **`every(ms, handler)`** — up to 16 timers on TIMER0's microsecond count,
  earliest due first, then registration order (the rule
  `rust_p2w::sim::Timers` uses). Verified on a Pico 2 W: a 100 ms and a 250 ms
  timer fired 50 and 20 times in 5 s, averaging 99.1 ms and 247.6 ms apart
  as timed from the serial port.
- **`robot.on_tilt` / `robot.on_tag`** — up to 16 handlers. Every 10 ms,
  after any timers due at that moment, the scheduler classifies `tilt()` into
  `tilt_state()` (with 10 degrees of hysteresis) and runs the handlers of each
  state that changed, in registration order (the rule
  `rust_p2w::sim::Subscriptions` uses). Built and cross-linked
  (`tools/pico_build.sh`, `robot_events`); **not yet seen firing on
  hardware**, because `tilt()` is still the table's stand-in until the IMU is
  wired (`ROBOT.md` step 4), and `tag_state()` stays `FREE` until the IR
  receiver is (step 5).

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
   the **full teaching subset** incl. sets, immutable tuples, slices, f-strings
   and list/dict comprehensions (classes remain WASM-backend-only). The
   **runtime crate** (`runtime/`, `p2w-rt`) is `no_std` + host-tested over the
   concrete tagged-`i32` rep (2-bit tag: small int / heap ptr / immediate
   singleton). Done: int/bool/None/**float** + arithmetic (`+ - * / // % **`, neg)
   with **int→float promotion** (Python: `/` is always float; `**` stays int for
   `int ** non-negative-int`) + comparisons + equality (cross-type `1 == 1.0`)
   + truthiness + `not` + `print`; a **static-arena allocator**
   (first-fit free list) + **strings, lists, dicts** (literals, subscript get/set,
   `len`, concat, structural eq, print), the **iteration protocol**, **methods**
   (append/pop), and ref-count primitives (`p2w_retain`/`release`, buffers freed).
   **The emitter RC pass is in and host-validated — and the Perceus reuse tier
   on top of it** (Jul 2026): last-mention liveness + precise drops, borrowed
   params, and drop-reuse in four forms (dying-source maps, literal
   reassignment, append/extend growth, interned literals) — measured
   `wl_chain` 10→3 allocs, `wl_realloc` 6→2, `wl_concat` 17→4;
   `tools/native_run.sh` runs **195 programs** ending `live==0`, plus a
   differential fuzzer (see `docs/REUSE_PLAN.md`). **Still to do:** cycles
   (design sketched from Nim ORC — `docs/COMPILER_FRONTIER.md` task 5),
   real device `p2w_putc` over USB-CDC, then the on-device toolchain spike
   (phase 1: `llc`/`picotool` → `.uf2`) to flash + run on a board.
4. **I/O + peripherals:** USB-CDC `print`, the temp sensor, GPIO.
5. **Debug transports:** USB stub, then SWD/probe-rs + DWARF.
6. **RISC-V** target variant.

**Phase 1 — validated on the host (2026-06-23).** `clang` (real LLVM) compiles
our emitted IR, and because values are i32 *arena offsets* (not machine
pointers) the IR + `p2w-rt` link and **run on the host** with output matching
CPython. `tools/native_run.sh` is the run-oracle: IR → `clang` `.o` → link the
runtime staticlib (`cargo rustc --crate-type staticlib -- -C panic=abort`) + a
`p2w_putc` stub → execute → diff (7 cases at the time; **195 today**). The runtime now carries a
`#[cfg(not(test))] #[panic_handler]` (needed for any no_std artifact — this lib
and the device build). **Still device-gated:** the on-board path (`.o` → RP2350
boot block → ELF → UF2 via `llc`/`lld`/`picotool`, run over USB-CDC) — but host
execution already proves IR validity + runtime correctness without hardware.

Design for the front-end (lexer/parser/AST) to stay 100% shared with the WASM
backend; only the emitter + runtime + toolchain differ.

**2026-08-18: `import math` and `import random` work natively.**
`math.exp/log/log2/log10/sqrt/pow` call the runtime's libm — the same pinned
libm the WASM backend compiled to WAT, so the targets agree to the last bit —
and random is the same xorshift64* as the WAT and the Stepper, bit-identical
by test (the run oracle pins the seed-42 sequence to the WASM backend's exact
output). The seed arrives through `p2w_host_seed()`, the native analog of the
WASM `env.seed` import: the host shims return a fixed 42 for determinism; a
real host supplies a per-attempt value. On the device this becomes one more
symbol the board support code provides, like `p2w_putc`.

## The easy sibling: Raspberry Pi 4/5 (verified 2026-08-31)

The Pi 4/5 are not Picos — they run Linux, and that makes them the *easy*
device target, close to free with what exists today. Both halves of the
claim were proven by running, not argued:

```bash
# 1. The runtime cross-compiles to the Pi's architecture unchanged (3.6 s):
rustup target add aarch64-unknown-linux-gnu
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib --target aarch64-unknown-linux-gnu -- -C panic=abort
# -> .../aarch64-unknown-linux-gnu/release/libp2w_rt.a

# 2. The emitted IR is target-neutral (no triple/datalayout pinned), so the
#    same .ll the x86 oracle validates compiles for ARM:
p2w-emit-ll program.py > program.ll        # cargo run --example emit_ll
clang --target=aarch64-unknown-linux-gnu -Wno-override-module \
  -c program.ll -o program.o               # -> ELF 64-bit ARM aarch64
```

The unproven remainder needs an aarch64 libc sysroot — which is exactly what
the Pi itself is. On the device the chain closes as ordinary C linking:

```bash
cc program.o putc.c libp2w_rt.a -lm -o program && ./program
```

`putc.c` on Linux is the real thing (`putchar`, `getchar`, a clock), not a
shim. And since `p2w` is plain Rust it also builds for aarch64, so a Pi can
be a self-contained compile-and-run box with no cross toolchain anywhere:
`apt install clang`, copy the repo, done.

Why this earns a rung on the device ladder rather than a footnote:

- **browser (WASM-GC) → Pi (native ELF, Linux) → Pico (bare metal).** Each
  rung drops one layer of infrastructure. Classrooms that already own Pis
  get the native backend *now*, with zero bring-up work, while the RP2350
  boot/UF2 story is built.
- **First honest test of the capability broker outside the browser.** The
  Pi has an OS to mediate: files, GPIO (`/dev/gpiochip*`), network. The
  WASI-preopens position in HOST_INTERFACE.md wants a target where the host
  actually has authority to hand out; the Pico (no OS, all symbols trusted)
  can't provide that and the browser sandboxes it away. The Pi is where
  `capabilities()` meets a real operating system.
- **Same divergence discipline.** Nothing Pi-specific may enter the
  emitter: the Pi consumes the identical IR + runtime the host oracle
  already pins to CPython. If a Pi-only behaviour ever appears it is a bug
  in this section's premise, not a porting cost.

Non-goals: no Pi-specific backend, no apt packaging yet, no promise about
the Pi as a *deployment* target for the IDE (that is the browser's job).
This section exists because the target costs almost nothing and answers a
real classroom question ("we have a cart of Pi 400s — can we use them?").

### The Zero 2 W (verified 2026-10-09) — same rung, one real difference

A Zero 2 W (BCM2710A1, quad-core Cortex-A53 — the Pi 3's SoC, not the
original Zero W's much weaker single-core ARMv6) is still a Linux box, so
everything above applies unchanged: same `-lm`/`putc.c`/ordinary-C-linking
close, same "nothing device-specific may enter the emitter" rule. One
thing is genuinely different and worth checking rather than assuming,
which is why this is its own entry: **unlike the Pi 4/5, the Zero 2 W is
commonly run on EITHER bit-width** — 64-bit Raspberry Pi OS is supported
and recommended, but a lot of existing Zero 2 W setups still run the
32-bit image (historically the Zero line's default, for peripheral/
software compatibility). So both targets were proven, not just the one:

```bash
rustup target add armv7-unknown-linux-gnueabihf   # 32-bit OS path
cargo rustc --manifest-path runtime/Cargo.toml --release \
  --crate-type staticlib --target armv7-unknown-linux-gnueabihf -- -C panic=abort
clang --target=armv7-unknown-linux-gnueabihf -Wno-override-module \
  -c program.ll -o program.o            # -> ELF 32-bit ARM, EABI5
```

Same `program.ll`, same emitter, two valid object files (confirmed with
`llvm-objdump -f`: `elf32-littlearm` / `aarch64`) — the "target-neutral
IR" claim now checked on a second architecture, not just asserted by
analogy to the Pi 4/5 aarch64 case.

**What's proven vs. what isn't.** Proven: the runtime cross-compiles for
both bit-widths, and the emitted IR for both is a valid object file —
i.e., a cross-compiled p2w *program* runs on a Zero 2 W exactly like it
runs on a Pi 4/5. **Not proven, and not casually assumed:** the Pi 4/5
section's "self-contained compile-and-run box, no cross toolchain" claim.
That means building the *whole* p2w compiler crate (~20k lines, not the
kilobyte-scale runtime) on-device, and the Zero 2 W's 512MB RAM — a
fraction of even a Pi 4's — is the kind of constraint that bites during
linking specifically, not compiling. Until measured, treat the Zero 2 W
as a proven *run* target (cross-compile elsewhere, copy the binary over)
and an unverified *compile* target.

**Distro choice is a non-issue — the thing that matters is the libc, not
the branding.** Jason's own box will run Ubuntu; a classroom full of
these will likely run whatever each teacher's Pi Imager defaults to
(Raspberry Pi OS, DietPi, Armbian, Ubuntu, …). A Rust target triple names
(architecture, kernel, ABI) — `aarch64-unknown-linux-gnu` means "any
aarch64 Linux with glibc," which is every distro listed above. None of
them change which object file gets produced; the two already proven here
cover all of them. The only distro family that *would* need a third
target is a musl-based one (Alpine's ARM images exist but are rare on Pi
hardware specifically) — not worth building for until someone actually
runs one.

**Checked specifically for Ubuntu, since that's the one someone here is
actually running:** Canonical's own support matrix lists the Zero 2 W as
server-supported from 22.04 through 26.04 LTS, but **only as a 64-bit
image** — there is no 32-bit Ubuntu build for this board. So on Jason's
box specifically, only `aarch64-unknown-linux-gnu` is in play; the
armv7 path above is for Raspberry Pi OS users choosing the 32-bit image,
not an Ubuntu concern at all. One more-basic fit question surfaced
alongside this, independent of anything p2w does: a Raspberry Pi Forums
thread reported Ubuntu Server 24.04 LTS 64-bit failing to boot on a
Zero 2 W, with a poster attributing it to Ubuntu wanting closer to 1GB
RAM than the board's 512MB gives — unconfirmed against Ubuntu's own
docs, but consistent with the compile-side RAM caution above. Worth a
quick boot test with whichever LTS release is current before assuming
the OS itself is comfortable, let alone anything built on top of it.

