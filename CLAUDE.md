# rust-p2w

See the parent `CLAUDE.md` for build and shell gotchas. This covers what is
true about *this* crate.

## What it is

**Not only a compiler.** ~20k lines compile Python; ~12k are analyses built on
the same AST (`debug` = an interpreter and stepper, `lint`, `roles`, `blockly`,
`evidence`). The organising principle is **"shares the AST"** — all of it breaks
when the AST changes, which is why it is one crate. `README.md` has the table.

## Invariants — do not break these without saying so

**One runtime dependency (`ryu`).** `wasmtime` and `wat` are optional, behind
the `run` feature. Check with `cargo tree --depth 1 -e normal`: default must
resolve `ryu` alone.

**Every p2w program is valid Python — not the reverse.** Restricting is free;
extending beyond Python needs syntax CPython already parses and ignores
(annotations, decorators, `del`, comments). New keywords break the promise.
`SUBSET_POLICY.md`.

**No dynamic name resolution, permanently.** No `eval`, no `exec`, no computed
`getattr`, no `__import__`. It is what keeps the call graph decidable, which is
what makes `capabilities()` a complete statement of what a program can reach.

**Both targets or neither.** A feature that works in the browser but not on the
board means a program compiles at school and fails on the device.

**If you cannot write the error message, the feature is not understood well
enough to ship.**

## Generated documents — regenerate, never hand-edit

These are produced by running real programs, and a test fails if they drift.
That is deliberate: hand-written descriptions of behaviour rot.

```bash
P2W_BLESS=1 cargo test --test semantics           # RUNTIME_SEMANTICS.md
P2W_BLESS=1 cargo test --test upstream_gap        # UPSTREAM_GAP.md (feature probes)
P2W_DIFF=1 P2W_BLESS=1 cargo test --test backend_diff   # BACKEND_DIVERGENCE.md (~6 min, needs clang)
```

A diff in any of them is a change to the language. Read it before blessing.

## Before pushing

CI denies every clippy warning across all targets. Run what CI runs, not a
subset — a newer clippy than the local habit fires lints (collapsible_if in
let-chains) that a plain build never shows:

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo fmt --manifest-path runtime/Cargo.toml --check
```

After touching `runtime/` (unsafe offset arithmetic — CI enforces this too):

```bash
cargo +nightly miri test --manifest-path runtime/Cargo.toml
# and the bounded proofs (Kani has no Windows build — run via WSL):
wsl -d Ubuntu-24.04 -- bash -lc "cd /mnt/c/Code/P2W/rust-p2w/runtime && CARGO_TARGET_DIR=/tmp/kani-target RUSTC_WRAPPER= ~/.cargo/bin/cargo-kani kani"
```

Root `cargo fmt` does NOT reach `runtime/` or `tools/mathwat/` — they
have their own manifests, and CI checks the runtime separately.

## Where the decisions are written down

| file | |
|---|---|
| `SUBSET_POLICY.md` | what may enter the language; 3 tiers, 4 gates, one permanent rule |
| `HOST_INTERFACE.md` | why the host imports are bespoke, and when they should not be |
| `PRIOR-ART-TYPES.md` | what RPython/Shed Skin/Codon/Mojo/SPy/Cython/mypy decided, and the research |
| `PRIOR-ART-AGENTIC.md` | the compiler as an agent's inner loop |
| `TYPE_ERROR_MESSAGES.md` | what each type error should say to a twelve-year-old |
| `TYPE_CHECKER_BRIEF.md` | the handoff/recruitment document |
| `UPSTREAM_GAP.md` | what upstream p2w has that we do not |
| `tests/oracle/` | the type-system spec, as programs |

## Testing

```bash
cargo test                    # 548; the 189 exec tests run real WASM under wasmtime
cargo test --features run     # adds the fuel runner's tests
bash tools/native_run.sh      # ~200 cases through clang + the real runtime; >10 min
```

**`tools/native_run.sh` is the authority for the native backend.** Unit tests
assert on emitted IR text and will happily pass while the program is wrong.

## Two backends — now 3 of 108 probes apart

`compile_to_wat` (WASM-GC, browser) and `compile_to_llvm_ir` + `runtime/`
(linear memory, the board *and* the component/jco path). After the 2026-08
divergence sweep the disagreements are 3 of 108, each one recorded and
understood: annotation semantics (`x: int = 'no'` — GC demotes, native
trusts; the type checker settles it), native `dict.get`, and the native
unpack length check. `BACKEND_DIVERGENCE.md`.

Two structural facts keep it that way:

- **The native entry runs the WASM generator as a checker first** (see
  `compile_to_llvm_ir`) — compile-time checks apply to both targets by
  construction. Interim until the type checker extracts them into a real
  front-end pass; still: when adding a check, put it in the shared front-end.
- **Every runtime diagnostic's text lives in `src/messages.rs`** — one keyed
  table read by codegen, the native runtime (path-include), and the Stepper.
  Never write message text inline in a backend; add a table entry.

When adding a native host import, update BOTH C shims: the run oracle's
(`tools/native_run.sh`) and the differential harness's embedded one
(`tests/backend_diff.rs`) — missing the second turns every native probe into
a link error.
