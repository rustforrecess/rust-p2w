# Debugger architecture — source-level stepping, browser → Pico

> Design record. Captures how step-through debugging (step, breakpoints, watches,
> watchpoints, variable/scope inspection, call stack) is meant to work across
> *both* execution targets — the browser WASM-GC runner and the eventual
> Raspberry Pi Pico 2 W native backend — without forking the UI or the semantics.
> Companion to `GRAMMAR_ARCHITECTURE.md` and the Pico target notes. Status: the
> browser adapter (step 1) is **shipped** — `Stepper` + the CPS `Vm` in
> `src/debug.rs`, wired into the IDE's Debug mode; the native LLVM backend the
> Pico adapters need also exists now (`PICO_BACKEND.md`). Steps 3–4 (the
> on-device transports) remain the shape to build toward.

## Why we can do this at all

We **own the whole compiler** (lexer → parser → AST → codegen) and every AST node
carries its 1-based source line. That is exactly what a source-level debugger
needs, and it's the concrete payoff of compiling our own Python rather than
shipping an interpreter (MicroPython/Pyodide): because we control codegen we can
emit either DWARF debug info *or* our own per-statement step hooks. With a
third-party interpreter we'd be stuck with *its* debugger, not ours.

## The one idea: a `DebugAdapter` seam, three transports

Define a single interface and implement it three times behind the **same** IDE UI
(Step / Step-into / Step-over / Step-out / Continue, breakpoint gutter, watches
panel, variables panel, call stack):

```
trait DebugAdapter {
    fn step(&mut self) -> Stopped;              // run to the next statement
    fn continue_to(&mut self, bps: &BreakSet) -> Stopped;
    fn read(&mut self, expr: &Expr) -> Value;   // evaluate a watch / hover / var
    fn set_watchpoint(&mut self, target: &Expr);// break when this value changes
    fn stack(&self) -> Vec<Frame>;              // call stack, each with a scope
    fn locals(&self) -> Scope;                  // current frame's vars
}
```

| Adapter | Runs where | How `read`/watches evaluate |
|---|---|---|
| `InterpreterAdapter` | Browser (and CI) | the AST tree-walking interpreter — free |
| `UsbStubAdapter` | Pico 2 W, **no probe** | on-device stub reads our known memory layout |
| `ProbeAdapter` (DAP) | Pico 2 W, **SWD probe** | DWARF variable locations + DWT watchpoints |

**Watch expressions are always parsed by our own front-end** (`parse_expr(text)` →
`Expr`), so a watch can never disagree with the program's semantics. Only *where
the read happens* differs between adapters. Step/Continue/breakpoints/variables/
call-stack look identical whether the code runs in the tab or on the desk.

## Browser — `InterpreterAdapter` (build this first)

A small tree-walking interpreter over the same AST, used only for **Debug** mode;
the WASM-GC backend stays the fast **Run** path. Stepping is just a pause point in
the eval loop, so breakpoints, watches, watchpoints (eval a target each statement,
compare to previous), variable inspection, and the call stack all fall out for
free. No `SharedArrayBuffer` / Web Worker / COOP-COEP headers needed.

Drift risk ("two engines that must agree") is bounded: the interpreter shares the
lexer/parser/AST — only the evaluator differs — and runs through the **same
CPython differential corpus** the WASM backend uses, so divergence shows up as a
failing test.

### The block↔text payoff
Because the AST drives both the block↔text round-trip and the debugger, a step can
highlight the current **text line *and* the current block** at once, and a
watchpoint can glow the exact block that changed a value ("break when `score`
changes" → that block lights up). This teaching view is unique to our stack.

## Pico 2 W — "follow onto the board"

The native backend these adapters need exists (our AST → LLVM → bare-metal
RP2350, Cortex-M33; cross-compiles and links today — `PICO_BACKEND.md`); what
remains is the on-device debug transport itself. Two paths, deliberately kept
under the same `DebugAdapter`.

### Path 1 — real hardware debug (SWD + probe) — "pro mode"
- **Transport:** SWD (`SWCLK`/`SWDIO` on the debug header) via a debug probe (the
  ~$12 Raspberry Pi Debug Probe, or a second Pico as debugprobe). NB: this is the
  *probe's* USB, not the target's own USB.
- **Driver:** `probe-rs` (Rust-native, speaks DAP, supports RP2350 Arm + RISC-V,
  plus **RTT** for fast I/O — which maps onto our `env.write_char` host contract
  for `print()` from the board).
- **Source bridge:** **DWARF emitted by our LLVM backend.** Line table → PC ↔
  source line (source-level stepping); variable locations → read a var from its
  register/stack slot (variables + watches).
- **Watchpoints in silicon:** Cortex-M33 **DWT** comparators turn "watch when
  `score` changes" into a true hardware data watchpoint (zero overhead). **FPB**
  gives hardware breakpoints.
- **Limits (be honest):** needs the probe + SWD pins; debug builds must be `-O0`
  for stable variable locations and 1:1 line↔PC mapping; only a handful of HW
  breakpoint/watchpoint comparators exist; correct DWARF emission is real work.

### Path 2 — instrumented stub over the board's *own* USB — "no extra hardware"
- Literally the **same trick as the browser stepper**: in debug builds the backend
  emits a `__step(line)` call before each statement. On-device, `__step` talks over
  **USB-CDC** (the Pico's own cable) to the IDE — streaming the current line +
  watched variable values and waiting for step/continue.
- **Watches:** the stub reads variables from a lightweight **variable-layout map**
  we emit (our own format, no DWARF needed). **Breakpoints:** a line set the stub
  checks.
- **Answers the literal question:** step *into* the Pico over its own USB cable,
  no probe, identical UX to the browser.
- **Limits:** adds code size + slows debug builds; can't catch a hard fault (a
  crash bypasses the stub); no instruction-level stepping.

## Cross-cutting decisions

- **Watches must be side-effect-free.** They're expressions, so assignment can't
  appear; flag/deny calls to mutating methods in watch context (or warn). Same
  footgun every real debugger has.
- **Debug vs Run are separate profiles.** Run = optimized WASM/native. Debug =
  interpreter (browser) or `-O0` + instrumentation/DWARF (Pico).
- **One source of truth for semantics:** the CPython differential corpus gates the
  interpreter against the WASM/native backends.

## Build order

1. **`InterpreterAdapter` (browser)** — ✅ **shipped.** The `Stepper` lives in
   `src/debug.rs`: a resumable tree-walking interpreter (explicit control stack +
   one-statement-ahead `pending`, so it single-steps without blocking — works in
   the IDE's WASM). Public API: `Stepper::new/step/run`, `status`,
   `current_line`, `output`, `variables`, `eval_watch`. The IDE has a 🐞 Debug
   mode with Step/Continue/Stop, a live variables list, watch expressions, and a
   paused-line highlight. Covers the teaching subset; unsupported constructs stop
   with a friendly "use Run" message. A clickable line-number **breakpoint
   gutter** drives `run(breakpoints)`, and **watchpoints** (break-on-change:
   `set_watchpoints` + a per-step value diff, reported as `watch_hit`) pause the
   run when a watched expression's value changes. Each statement block carries
   its source line in Blockly's `data` field, so the paused line **glows the
   matching block** too (the line↔block payoff). User functions are supported via
   **step-over**: a `def` registers a `FuncDef`; a call runs the body atomically
   (an `exec_block`/`exec_atomic` recursive executor over the same `eval_in`),
   so programs with functions — including **recursion and default args** — are
   debuggable. **CPS VM shipped (`Vm`)** — an explicit-stack interpreter
   (per-frame work stack of `Task`s + operand stack) that suspends anywhere,
   including mid-expression at a call, giving true **step-into** and a live
   **call stack** (`call_stack()`). The IDE now runs on `Vm` (▸ Step steps into;
   a Call-stack panel shows the frames). At parity with `Stepper` for the
   teaching subset (control flow, functions+recursion+defaults, lists/dicts/
   index/methods/for-each, watches/watchpoints/breakpoints). The atomic
   `Stepper` is retained as the semantics **reference oracle** + regression
   suite, not the IDE engine. The IDE has the full stepping model — **Step into /
   Over / Out** (`step`/`step_over`/`step_out`) — plus Continue/Stop, breakpoints,
   watches, watchpoints, the variables list, the call-stack panel, and the
   line+block highlight. **Classes run in the Vm too** (Jul 2026): construction /
   `__init__` / attributes / methods / inheritance / `super()` — every one a
   real frame push, so kids step INTO their constructors and dunders — plus
   `__str__`/`__repr__` in print/str and the operator dunders
   (`__add__`-family, `__eq__` direct→reflected→identity, comparisons,
   `__len__`, `__getitem__`), with the same clean-error guards as the native
   backend (undispatched dunders, class vars). Instances are `Rc`-shared, so
   aliases see each other's attribute writes (true reference semantics).
   Debugger-only narrowings, documented here: instances nested inside a
   printed CONTAINER show the default `<Dog object>` (no user-code calls
   mid-format), and `in`/dict-key lookups compare instances by identity
   rather than `__eq__`. Remaining niceties: writing globals from a function
   (`global`), and calling user functions inside a watch expression.
2. Native LLVM backend — ✅ **exists** (textual-IR emitter + `no_std` RC
   runtime with the Perceus-style reuse tier; cross-compiles/links for RP2350
   and builds linear-memory WASM components — see `PICO_BACKEND.md` /
   `docs/COMPILER_FRONTIER.md`). On-device flash/run is the remaining
   hardware-gated step.
3. **`UsbStubAdapter`** — on-device step hooks + USB-CDC control channel + the
   variable-layout map. No probe, consistent UX.
4. **`ProbeAdapter`** — DWARF emission + `probe-rs`/DAP + DWT watchpoints. The
   gold-standard hardware path.

`Stepper`'s API *is* the de-facto `DebugAdapter` interface; steps 3–4 implement
the same surface so they slot in under the same UI with no rework.
