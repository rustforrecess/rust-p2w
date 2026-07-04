# Emitter RC Pass — DONE, then superseded by the reuse tier (historical)

**Historical doc.** The RC pass landed 2026-06-23 (21 oracle cases at the
time), validated on the host: `clang` compiles our IR, links `p2w-rt`, and the
oracle asserts **`live == 0`** at exit. The "remaining precision/optimization"
work it anticipated has SINCE LANDED (Jul 2026): last-mention liveness +
precise drops, borrowed params, and drop-reuse in four forms (dying-source
maps, literal reassignment, append/extend growth, interned literals) —
measured `wl_chain` 10→3 allocs, `wl_realloc` 6→2, `wl_concat` 17→4, under a
180-case oracle + a differential fuzzer. **The living docs are
`docs/REUSE_PLAN.md` (staging + nets) and `docs/COMPILER_FRONTIER.md` (open
tasks: full backward liveness, type inference, escape inference, cycles).**
The narrative below is the original bring-up record — the gate caught a real
double-free (dict-update released a key the runtime already owned). A board is
still only needed for *on-device* confirmation.

Why this was possible offline: values are i32 *offsets* into a static arena, not
machine pointers, so the emitted IR + runtime are host-portable.

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

### 0. Run-oracle so we can VALIDATE — mostly DONE (host), no board needed

- [x] **Host run-oracle exists:** `tools/native_run.sh` — emitted IR → `clang`
      `.o` → link `p2w-rt` staticlib + `p2w_putc` stub → run on host → diff vs
      expected. 7 cases pass (ints/floats/loops/funcs/lists/strcat). This works
      because values are i32 arena offsets, not machine pointers.
- [ ] **Add alloc/free accounting** — the actual RC gate. Runtime counters
      (`allocs`, `frees`, `live_objects`) behind a tiny ABI (e.g. `p2w_live()`);
      have the harness emit a trailing call and assert **`live_objects == 0`** at
      program end. THIS is the acceptance gate for every RC step below. (No board
      required — runs in `native_run.sh`.)
- [ ] *(later, board-gated, NOT needed for RC correctness)* Phase-1 device spike:
      `llc`/`picotool` + `thumbv8m.main-none-eabihf` → ELF → UF2; flash + run over
      USB-CDC. This confirms *on-device*, but host accounting already proves the RC.

### 1. Emitter insertion pass — DONE (naive, validated)

Implemented in `src/llvm.rs` (release at scope end; no last-use precision yet).
- [x] `Name` load → `p2w_retain` (loads become owned).
- [x] Assignment → release old slot value before store; transfer RHS.
- [x] Operand-temp release after arithmetic/compare/unary/truthy/print/len/index.
- [x] **Call args transferred to the callee** (callee owns params, releases at
      exit) — not released by the caller. Method args transferred; receiver borrowed.
- [x] Container inserts (append, list/dict literals, setindex value & key): NO
      release of the inserted temp (transfer). Read-index key IS released (borrowed).
- [x] Scope/function exit: release all slots (zero-initialized so a never-assigned
      slot is a no-op) + pending loop temps (`cleanups`), at every `ret`.
- [x] Loop temps (iterator + sequence; counted bound) on `cleanups`, released
      after the loop and at early `return`. For-each releases the previous loop
      value each iteration. Short-circuit releases the discarded operand.
- [x] Structural test (`rc_pass_emits_retain_and_release`) guards the wiring.
- [x] **Validation gate passed:** `tools/native_run.sh` (GATE_LEAKS=1 default) —
      21 cases incl. reassignment, nesting, dicts+update, loops, functions,
      early return, foreach-over-strings, short-circuit, pop → all `live == 0`.

### 2. Perceus-style precision (after 1 is proven safe)
- [x] **Borrow-on-read (last-use core):** a `Name` read straight into a borrowing
      op (arith/compare/unary/print/len/condition/read-index/method receiver) is
      borrowed through its slot — no retain/release at all. `expr_borrow` returns
      `(value, owned)`; only genuinely-owned temps are released. Validated: all 21
      oracle cases still `live==0`; a trivial int loop dropped 14→6 RC ops (retain
      4→0). Tests: `borrow_on_read_skips_refcounting`.
- [ ] **Full last-use on owned slots:** release an owned variable at its last read
      rather than at scope end (needs per-block liveness; borrow-on-read already
      covers the common read-then-use). The leftover scope-end releases are mostly
      ints (runtime no-ops) — typed-int monomorphization would drop those too.
- [x] **Borrowed params:** a conservative escape analysis (`param_escapes` in
      llvm.rs) marks params used only for reading (never returned/assigned/passed
      onward/inserted/reassigned) as borrowable. The caller passes a named arg
      borrowed (no retain) and the callee doesn't release the param slot, so
      passing a named collection to a read-only helper costs zero refcount
      traffic. Escaping params keep the owned/transfer convention. Validated: 25
      oracle cases `live==0` (incl. borrowarg/borrowtwice/escarg/borrowstr);
      mutation through a borrowed param (`xs.append`, `xs[i]=`) matches Python
      pass-by-reference. Test: `borrowed_param_skips_retain_but_escaping_param_keeps_it`.
- [ ] **Drop-reuse tokens (FBIP):** when a unique value is dropped and a same-shape
      value is allocated, reuse the buffer in place (the big embedded win — mutate
      in place vs alloc/free churn). Needs a runtime reuse path + uniqueness
      (refcount==1) check; the accounting/run-oracle can measure the alloc drop.

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
