# Emitter RC Pass — Resume When a Board Arrives

**Status: PAUSED (2026-06-22).** The runtime is RC-correct and host-tested; the
emitter does **not** yet emit `retain`/`release`. We paused here deliberately:
the emitter insertion is memory-safety-critical and **cannot be validated
offline** — it needs to *run*, which needs a board (or QEMU) + the Phase-1
toolchain. Building it blind banks unproven safety-critical code, so we wait.

See also: `MEMORY_MANAGEMENT.md` (model + research), `PICO_BACKEND.md` (phases),
the ownership-contract comment in `src/llvm.rs` (above `struct FuncEmitter`).

---

## Where we are (done + committed)

- **Runtime (`runtime/src/lib.rs`) is RC-correct** under the transfer model and
  this half is host-testable, so it's verified:
  - `free_object` releases container children (lists → elements; dicts → key+value)
  - `p2w_setindex` / dict-update release the replaced value (+ redundant key)
  - `p2w_index` and iteration (`element_at`) return **owned** refs (retain)
  - list concat retains copied elements; `pop` transfers; append/literals transfer
  - RC seams in place: `rc_inc`/`rc_dec` (atomicity swap point), `free_object`
    (deferrable-free seam), `owned()`. `p2w_retain`/`p2w_release` route through them.
  - **16 runtime tests pass** (incl. child-release, setindex-release-old, owned-index).
- **Emitter ABI wired:** `p2w_retain`/`p2w_release` declared in `RUNTIME_DECLS`.
- **Ownership contract documented** in `src/llvm.rs` (the rules below).

## The ownership contract (Model A — transfer-based)

Every `expr()` result is an **owned** (+1) reference.

| Site | Action |
|---|---|
| Constructors (`p2w_int`/`str`/`list_new`/`add`/`call`/`index`/`iter_next`…) | already return +1 — nothing to emit |
| `Name` **load** (borrowed) | emit `p2w_retain` after load → make it owned |
| `x = e` (assign) | `release` OLD x, store e (**transfer** — don't release e) |
| container insert (append, list/dict literal elems, setindex value & key) | **transfer** — don't release the temp (runtime owns + frees later) |
| list index value (it's an int) | transfer is a no-op |
| borrowing ops (arith/compare operands, `print`, conditions, index target+index, call args) | `release` each operand temp after the op |
| scope / function exit | `release` all locals |

---

## TODO — resume order (each step is one focused commit)

### 0. Prereq: stand up Phase-1 toolchain so we can VALIDATE (board/QEMU)
This is the whole reason we paused. Without it, steps 1–3 are unverifiable.
- [ ] Install/confirm `llc`, `lld` (or `ld.lld`), `picotool`, target
      `thumbv8m.main-none-eabihf`. QEMU `qemu-system-arm` for boardless runs if possible.
- [ ] Phase-1 spike (see PICO_BACKEND.md): emitted IR → `.o` → link RP2350 boot
      block → ELF → UF2; flash + run "hello + print(int)" over USB-CDC.
- [ ] **Add an alloc/free balance assert harness:** runtime counters
      (`allocs`, `frees`, `live_objects`) exposed via a debug host-import; a test
      program must end with `live_objects == 0`. This is our offline-proxy oracle
      and the acceptance gate for every RC step below.

### 1. Emitter insertion pass (the big one — gated on step 0)
Implement the contract in `src/llvm.rs`. Naive first (release at scope end; no
last-use precision). Touch points:
- [ ] `Name` load path → emit `p2w_retain` (make loads owned).
- [ ] Assignment → release old slot value before store; transfer RHS.
- [ ] Operand-temp release after arithmetic/compare/`not`/truthy/print.
- [ ] Call args: release argument temps after the call returns.
- [ ] Container inserts: ensure NO release of the inserted temp (transfer).
- [ ] Scope/function exit: release all live locals (track a per-scope owned set).
- [ ] **Structural tests:** assert emitted IR contains retain/release at the
      expected sites (string-match on the IR). These prove *shape*, not safety.
- [ ] **Validation gate:** run the alloc/free balance harness from step 0 on a
      suite of programs (scalars, lists, dicts, nested, loops, functions,
      reassignment, early return, break/continue) → all must finish `live == 0`.

### 2. Perceus-style precision (after 1 is proven safe)
- [ ] **Last-use analysis:** release at last use, not scope end (shorter lifetimes).
- [ ] **Borrowed params:** params that don't escape are borrowed (no retain/release).
- [ ] **Drop-reuse tokens (FBIP):** when a unique value is dropped and a same-shape
      value is allocated, reuse the buffer in place (the big embedded win — mutate
      in place vs alloc/free churn). Needs uniqueness (refcount==1) check.

### 3. Cycles + flags
- [ ] No-mutation auto-detect: if a program has no mutating ops it's provably
      acyclic → omit cycle collector (smaller, garbage-free, no pauses).
- [ ] `--no-mutation` / pure mode that enforces it. Coarse rule first (any
      mutation → keep collector). Surface the choice in the IDE.
- [ ] Cycle handling for the mutating case (weak refs or a small cycle collector).

### 4. RT/ROS hardening (later)
- [ ] O(1) size-class allocator; bounded per-tick free (incremental cascading free
      via the `free_object` seam); fault-safe trap/OOM.
- [ ] Confirm single-threaded + marshal-at-boundary invariant holds for micro-ROS.

---

## Acceptance criteria for "RC pass done"
1. Every program in the validation suite ends with `live_objects == 0` (no leaks).
2. No use-after-free under the run harness (would manifest as wrong output/trap).
3. Structural IR tests green; runtime tests green; clippy clean.
4. Reuse measurably reduces alloc count on an in-place-update microbenchmark.
