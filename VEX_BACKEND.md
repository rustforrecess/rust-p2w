# VEX robotics as a compile target (design sketch)

> For schools that own **VEX robotics** kits instead of a Raspberry Pi Pico 2 W.
> Status: **research + design only** (Jul 2026) — nothing built. Companion to
> `PICO_BACKEND.md`; VEX is a third backend on the same "one AST → N targets"
> spine (the browser WASM-GC backend and the Pico LLVM/native backend are the
> other two). Everything below is sourced from the community (vexide, PROS,
> Purdue SIGBots, the VEX EULA + V5RC rules), not from VEX's proprietary SDK.

## The hardware, and why it fits our architecture

**VEX V5 Brain** (the competition tier, mid/high school) is a Xilinx Zynq
XC7Z010 SoC: dual **Cortex-A9 @ 667 MHz (ARMv7-A)** + an FPGA + two Cortex-M0s.
Two facts decide the design:

1. **The app CPU is a first-class LLVM target** (`armv7a`). Our textual-IR
   emitter (`src/llvm.rs`) retargets with a triple change, not a rewrite — the
   same move that already produces Cortex-M33 for the Pico. The A9 even has an
   FPU/NEON, welcome for robotics float math, and far more RAM/clock than the
   M33 (so our memory-tightness story matters less here — but the
   *no-interpreter* story still holds).
2. **You run HOSTED under VEXos, not bare metal.** Unlike the Pico (where *we
   are* the firmware), VEXos boots and owns the hardware, then hands a loaded
   user program a **jump table at `0x037FC000`** — function pointers to every
   SDK call (motors, sensors, screen, controller). That is "the closest thing
   to a syscall" on the V5.

Point 2 is the load-bearing one: it maps **exactly** onto our host-capability
seam. `env.write_char` on WASM and `p2w_putc`/`p2w_getc` on the Pico become
**jump-table calls** on VEX, and motor/sensor/screen operations become
host-capability builtins — the same shape as the interactive-web `env.*`
builtins (`docs/INTERACTIVE_WEB.md`). No new architecture; a new transport.

## The precedent that de-risks it: vexide (MIT)

We are not first. **`vexide`** is an MIT-licensed Rust toolchain whose
`vex-sdk-jumptable` crate is "an open-source implementation of VEXos system APIs
using firmware jump addresses" — `no_std` Rust, its own runtime, compiled code
on the A9, linking the jump table **directly** and **bypassing VEX's proprietary
SDK**. That is our exact model already working on this board. We can read/reuse
it (with a NOTICE ideas-not-code entry) and lean on its published "V5 serial
protocol" writeup for the upload path.

**Do NOT build on PROS.** PROS is open-source but depends on VEX's *proprietary*
SDK by a special internal-docs arrangement — canonical for them, but not a clean
or redistributable seam for us. vexide's from-scratch jump-table binding is the
right model.

## Two targets, two tiers

### VEX V5 → Tier B (LLVM object → VEXos jump table) — the strategic path

`AST → src/llvm.rs (armv7a) → object → a p2w-rt "vex" platform layer linked
against the jump table → VEXos loads it`. Keeps the **whole** backend: the value
model, the Perceus reuse tier, the no-GC RC runtime — running our *compiled*
Python on the V5, calling motors/sensors as host capabilities. This is the Pico
backend "hosted under VEXos instead of bare-metal," and it restates our thesis
for a second board schools already own: **compiled, not interpreted** — VEX's
own on-device Python is **MicroPython** (an interpreter), the exact thing
rust-p2w exists to avoid.

**Scope (hardware-gated, like the Pico's `.uf2` step):**
- retarget the emitter to `armv7a-none-eabi` (target triple + a couple of attrs;
  the textual IR is unchanged);
- a `p2w-rt` **vex platform layer**: `p2w_putc` → screen print via the jump
  table; controller input → `p2w_getc`; plus new host functions for
  motors/sensors/screen exposed to the language as builtins;
- a VEXos-shaped bootstrap + the program-container/`.bin` packaging VEXos
  expects (analog of the Pico bootrom block + `.uf2`);
- on-device validation — **needs a physical V5 brain**.
- **Unknowns to verify on hardware:** the custom-binary upload protocol
  (`vexcom` / USB serial — vexide documents it) and that a non-vexide binary
  loads cleanly.

### VEX IQ → Tier A emitting **C++** (compiled on-device, legally cleanest)

IQ (the younger tier) runs a **MicroPython VM** with tight RAM and is
programmable **only via VEXcode** (Blocks / C++ / Python). Crucially, **no
third-party toolchain supports IQ** — PROS is V5-and-old-Cortex only, vexide is
V5-only — so nobody has cracked IQ's binary loader, and a Tier-B custom binary
there is unproven and high-risk.

The good answer is **transpile the AST to VEXcode C++** (not Python) and let
VEX's toolchain compile it:
- **VEXcode C++ is compiled on-device** (VEXcode *Python* is the MicroPython
  interpreter), so we keep "the same kid's program, compiled, on their robot";
- we reuse VEX's motor/sensor **C++ API** as the capability layer;
- it **sidesteps the no-binary-loader problem** entirely — we produce standard
  VEXcode-toolchain input;
- it is **legally the cleanest path** (below): VEXcode used exactly as intended,
  no reverse engineering, no jump-table gray area.

The only thing given up on IQ is *our* RC/reuse runtime (VEX's C++ runtime
manages memory there) — and IQ's audience is young kids, where the memory-model
differentiator matters least. This is the right call, not a compromise.

Bonus: the **AST → VEX-C++ transpiler is a third emitter** (sibling of
`codegen.rs` = WASM and `llvm.rs` = native), reusable both for IQ and as a
quick-bring-up / fallback on V5.

### Tier C (replace VEXos, bare-metal) — impossible

VEXos owns the hardware and cannot be replaced (unlike the Pico, where we are
the firmware). Not a real option, and competition rules forbid altering Brain
firmware anyway (see below).

## Licensing & competition legality (verified)

Honoring the standing "no licensing surprises" rule — this corrects an initial
optimistic read:

- **Running a third-party compiled runtime as a USER PROGRAM on UNMODIFIED
  VEXos is permitted and intended** — it is literally what PROS and vexide do,
  **including in V5RC competition**. The competition rule that "Robot Brain
  firmware may NOT be altered in ANY way" targets **VEXos / motor firmware**,
  which we do **not** touch — a loaded user program is not firmware. Don't let a
  naive read of that rule kill the idea.
- **The narrow, real risks:** (1) VEX's Software EULA forbids
  modify / decompile / reverse-engineer / redistribute / repackage /
  **commercially-exploit** "the Product." (2) The V5 jump-table binding avoids
  VEX's SDK, but vexide's own authors flag it as **legally untested** ("we are
  not lawyers"). (3) PROS's SDK-by-arrangement is not a seam we can reuse.
- **Therefore:** the **IQ transpile-to-C++ path is the lowest-risk** (uses
  VEXcode as intended). The **V5 jump-table path is low-but-nonzero risk** for a
  **free / educational** product; if AcornSTEM ever becomes a **paid** product
  targeting VEX hardware, the "commercially-exploit" clause plus the untested
  jump-table legality deserve a real legal review **before shipping**.

## Where this sits

VEX is the third point on the backend axis of `GRAMMAR_ARCHITECTURE.md`
(per-language front-ends → shared representation → per-target backends). It reuses
the host-capability seam, the "we compile, we don't interpret" philosophy, and —
on V5 — the entire memory model. It is a *should-we / here's-how*, not a
build-it-now: both tiers are hardware-gated (a V5 brain and/or an IQ brain to
validate on), exactly like the Pico's remaining on-device step.
