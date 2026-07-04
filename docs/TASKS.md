# Frontier tasks — moved

**This file is superseded by [`COMPILER_FRONTIER.md`](COMPILER_FRONTIER.md)**
(the pitch + the scoped open tasks with interfaces and acceptance gates) and
[`REUSE_PLAN.md`](REUSE_PLAN.md) (the Perceus staging, invariants, and the
three acceptance nets).

Of the tasks originally listed here (Jun 2026):

- **Task 1 (full last-use release)** — LANDED as last-mention liveness +
  precise drops (`src/reuse.rs`, `block_precise`/`early_releases`); the *full
  backward liveness* upgrade remains open as `COMPILER_FRONTIER.md` task 2.
- **Task 2 (general Perceus reuse)** — LANDED: dying-source map reuse,
  assign-site literal reuse, append/extend growth, interned literals.
  Measured: `wl_chain` 10→3 allocs, `wl_realloc` 6→2, `wl_concat` 17→4.
- **Task 3 (reachability-type escape inference)** — open, now
  `COMPILER_FRONTIER.md` task 4.
- **Task 4 (verified RC pass)** — open, now `COMPILER_FRONTIER.md` task 7.
- **Cycle collector** — open, now `COMPILER_FRONTIER.md` task 5 (design
  sketched from Nim ORC).

The acceptance machinery described here grew too: the oracle is 175 cases
(`tools/native_run.sh`), the bench reports allocs/peak/live
(`tools/reuse_bench.sh`), and a differential fuzzer generates fresh cases
against CPython (`tools/fuzz_native.sh`).
