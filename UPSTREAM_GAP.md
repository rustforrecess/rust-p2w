# Where we stand against upstream p2w

`rust-p2w` is a derived work of [abilian/p2w](https://github.com/abilian/p2w)
(MIT; see `NOTICE`). This compares against upstream's `docs/architecture.md`,
which describes **0.2.2**.

Two things this is *not*: a to-do list — much of what is missing we may never
want, and `SUBSET_POLICY.md` decides that — and a claim that closing the gap is
progress.

## How this was checked

**By compiling a program per feature, not by grepping for keywords.** The first
attempt grepped AST node names and was wrong twice: it missed features that are
*desugared* (lambdas become `def`s, f-strings become concatenation, so neither
has a node of its own), and it reported **inheritance as missing** because the
probe was `class B(A): pass` — which fails on `pass` in a class body, not on
inheritance.

The probes now live in `tests/upstream_gap.rs` and regenerate
`FEATURE_PROBES.md`, with a test that fails when the table drifts —
**a row moving is a feature landing or a regression.** They started in a
scratch directory, which would have guaranteed this document went stale.

## Implemented

Upstream phases 1, 5, 6 and most of 3. Verified compiling: classes with
**inheritance, `super()` and class variables**; nested `def`s and closure
*capture*; recursion and mutual recursion; annotated parameters; default
arguments and keyword arguments at call sites; `elif`, `break`/`continue`,
`not in`, bare `return`; list/dict/set/tuple literals and comprehensions
(including nested and filtered) plus generator *expressions*; slicing with
steps and negative indices; f-strings and `.format()`; the string, list and
dict method sets; `del d[k]`; chained comparison; chained and augmented
assignment; tuple unpacking in `for`; `import math`; and the numeric builtins.

Parsing is a **hand-written lexer and parser** — upstream is written in Python
and gets its front end from the `ast` module for free.

## Not implemented — verified by rejection

### Whole features

| | note |
|---|---|
| `with` / context managers | |
| `match` / `case` | |
| walrus `:=` | |
| `yield` / generators | generator *expressions* work; generator *functions* do not |
| `try` / `except` / `finally`, `raise` | `SUBSET_POLICY.md` blocks these at gate 1 |
| `assert` | |
| `global` / `nonlocal` | |
| `is` / `is not` | |
| decorators (`@`) | |
| `*args` / `**kwargs` | at the definition site; keyword args at *call* sites work |
| `from X import Y` | plain `import math` works |
| bytes literals | |
| `del name` | `del d[k]` works, and the error explains the difference |
| multiple inheritance | rejected deliberately, with a message saying so |
| `for`/`while` … `else` | |
| `%` string formatting | `.format()` and f-strings both work |
| integers past 2³¹ | today a compile error / trap — see below |

⚠ **`__slots__` compiles but means nothing.** A class can declare it and then
still take a new attribute at runtime, so it is parsed as an ordinary class
variable. Worse than absent: it looks supported.

### ⭐ One root cause with many symptoms: functions are not values

These all fail, and all for the same reason:

```
g = f                      'f' is a function — call it with f(...)
apply(fn, v)               'd' is a function — call it with d(...)
return inner               (a returned function cannot then be called)
map(abs, xs)               unknown name 'abs'
sorted(xs, key=len)        unknown name 'len'
```

`hoist.rs` does **lambda lifting** — capture becomes extra parameters — which
covers closures that are *called where they are defined* but cannot make a
function a value, because there is nothing to hold.

Upstream's answer is a **calling convention**: every function has the signature
`(args: PAIR chain, env: ENV) -> value` and lives in a **funcref table** reached
by `call_indirect`. A closure is then an environment pointer plus a table index.

**So `map`, `filter`, `sorted(key=)` and every callback API are not separate
gaps — they are downstream of one decision.** And that decision is bigger than
`SUBSET_POLICY.md`'s Tier 1 entry implies: it changes how every function is
called, and interacts with the reuse tier and the native backend.

⚠ It also touches gate 2. A funcref table makes the call graph harder to
enumerate, and `capabilities()` depends on enumerating it. Still decidable while
table indices cannot be computed from data — but no longer free.

### Also absent, unrelated

`divmod`, `type()`, `isinstance()`.

## Upstream designs worth taking rather than inventing

**Large integers — `$INT64`.** Upstream keeps small integers as `i31ref` and
boxes anything outside that range into an `$INT64` struct. We stop at 32 bits:
the literal range is a compile error and overflowing arithmetic now traps
(`980c9a0`, `655c73c`). Trapping was the right interim — a loud failure beats a
silent wrong answer — but the widening path `RUNTIME_SEMANTICS.md` calls
"separate work tied to the value model" **already has a reference
implementation.**

**Their WASM-GC module has a linear memory.** String data lives there, interned
from offset 2048, with a bump pointer for runtime concatenation. Ours has none —
strings are GC arrays, and the emitted module contains no `(memory` and no
`(data`. Neither is wrong, but "adding a linear memory would be a departure" is
false: for the upstream design it is the norm.

**Their JS interop is handle-based** — opaque i31 handles into a JS-side object
table, with ~25 specialised DOM wrappers. We do capabilities instead
(`HOST_INTERFACE.md`), which is narrower and auditable, and which the capability
manifest depends on. Different answers to the same question.

## What we have that upstream does not

The native LLVM backend and `no_std` runtime; the Perceus-style reuse/FBIP
tier; the Component-Model converter; the stepping interpreter; lints with fix
ladders; variable-role classification; Blockly round-tripping; the capability
manifest; the two-backend differential harness; and the fuel-metered runner.

That list is the answer to "why not just use p2w."

## ⚠ One caution about upstream's Phase 4

Their type inference is **forward-flow, local, and aimed at unboxing** — it
proves a variable does not escape so it can live in a native local. It is not a
checker, it produces no student-facing errors, and nothing in it reports a cause
rather than a symptom.

So "upstream already has type inference" must not be read as
`TYPE_CHECKER_BRIEF.md` being done. If anything it argues for the split: the
unboxing half is tractable and has a reference implementation, and the hard half
— errors a twelve-year-old can act on — is still unclaimed.
