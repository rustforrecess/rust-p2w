# What may be added to the subset

An admission test for new language features. The subset will grow — the point
of writing this down is that it grows *deliberately*, against stated criteria,
rather than one reasonable-looking feature at a time until the strengths are
gone.

Companion to `PRIOR-ART-TYPES.md` (what other Python compilers decided) and
`tests/oracle/` (what the type system must do).

## The invariant

> **Every p2w program is a valid Python program. Not the reverse.**

That asymmetry does most of the work here.

It is what makes this a teaching language rather than a toy: a student pastes
their code into CPython and it runs. What they learned transfers. The reverse
property — *every* Python program compiles — is impossible for anything that
targets bare metal, and we gave it up on purpose.

**The consequence is liberating.** Restricting costs nothing, and extending
*within* Python costs nothing, because in both cases the programs we accept
still run in CPython. We can forbid type-changing reassignment, require
homogeneous lists, demand that every path returns — none of it touches the
promise. Only one direction is dangerous, and it is a narrow one.

## Three tiers

### Tier 1 — completing Python

Features CPython has that we lack. Currently: exceptions, first-class
functions, the iterator protocol.

**No identity risk at all.** The only costs are implementation effort and the
memory model. Extend freely, subject to the gates below.

### Tier 2 — house style

Rules *stricter* than Python: the type system, "one type at each control flow
point", homogeneous lists, lints for legal-but-probably-wrong.

**Free by the asymmetry.** A program we accept still runs in CPython; we have
merely declined some programs CPython would have taken. This is where most of
the design energy should go, because it is the tier with no downside.

### Tier 3 — superset

Anything CPython does not have. **The only tier that can break the invariant** —
and it has a clean escape hatch:

> **A Tier 3 feature must be spelled in syntax CPython already parses and
> ignores.**

Annotations, decorators, `del`, comments, and ordinary-looking function calls
all survive a paste into CPython. **New keywords do not.**

This is why `del`-as-a-reuse-hint is the right shape and a `drop` keyword would
not have been — identical semantics, but only one of them keeps the promise.
Where a Tier 3 feature needs a name, give it a no-op CPython shim (a decorator
that returns its argument, a function that returns its input) and it stays
inside the invariant.

## The bright line

> **No dynamic name resolution. Ever.**
>
> No `eval`, no `exec`, no `getattr`/`setattr` with a computed string, no
> `__import__`, no monkeypatching, no constructing a call target at runtime.

This is not a performance rule and not a taste rule. It is the rule that keeps

    "this program's call graph reaches no host function outside the whitelist"

a **decidable question**. That property is what makes the subset an actual
security boundary for an agentic executor rather than a claim about one. Lose
it and the compiler stops being able to prove anything about what a generated
program can touch.

Every other restriction in this document is negotiable with a good argument.
**This one is permanent**, and it is written here so that nobody has to have
the argument again in year three.

## The gates

A proposed feature must pass all four.

**1. Both targets, or neither.**
A feature that works in the browser but not on the Pico means a student's
program compiles at school and fails on their board. For a teaching language
that is worse than not having the feature — it turns a language rule into
folklore about which machine you are on.

**2. It preserves the capability proof.**
See the bright line. A feature that makes the call graph undecidable is
rejected regardless of its other merits.

**3. You can write the error message.**
Specifically: what a twelve-year-old sees when they get this feature wrong, in
`lint::Scaffold` shape — question, hint, fix. **If the error message cannot be
written, the feature is not understood well enough to ship.** This gate does
more work than it looks like it does; it has caught more bad ideas than the
other three.

**4. A lesson is currently impossible without it.**
Not "would be nicer". Name the lesson. Features justified by completeness
rather than by teaching are how a subset stops being a subset.

## Worked examples

These are the three known Tier 1 gaps, run through the gates. They are recorded
here as much for the *method* as for the conclusions.

### Exceptions — blocked at gate 1

Cheap in the browser: codegen emits WASM-GC, so unwinding needs no cleanup
paths, and WASM has native exception instructions. Expensive on the Pico for
precisely the reason no-GC was chosen — unwinding needs cleanup and unwind
tables on the target with least room.

**Fails gate 1, and that is why it has not shipped.** The tempting middle option
(try/except with no propagation, handler in the same function) fails a
different test: it looks like Python and means something else, which is the one
thing Tier 1 must never do.

Worth noting that gate 4 is also weak here. Uncaught errors already print a
clear message and stop — that is CPython's uncaught behaviour minus the
traceback. And the classic motivating cases (validating input, missing file,
failed network call) mostly do not exist in this subset. Types delete more of
the remaining category than exceptions would (see `tests/oracle/`).

**⭐ But `result<T,E>` clears gate 1, and gate 1 was the blocker.** WIT's
`result` is error handling as a **return value** — no unwinding, so no cleanup
paths and no unwind tables, identical on both targets, and compatible with
reuse analysis. It also gets the pedagogy right where `try/except` does not: a
`result` cannot be ignored silently, whereas a beginner's `except: pass` exists
precisely to make the error go away.

Not decided — a `result` needs somewhere to put the error, and Python has no
`Result`, so it would arrive as a Tier 3 feature spelled in annotations with a
CPython shim. But it is the first proposal that gets past the gate that has
been stopping this. See `PRIOR-ART-TYPES.md`.

### First-class functions — passes, and is aligned

Passes all four gates. More usefully, it **pulls in the same direction as the
type work**: passing functions as values pushes toward monomorphising call
sites, which is the same mechanism as the answer to `open-question/
function-used-on-two-types.py`, which is the same mechanism as unboxed lists on
the Pico.

**Three separate goals wanting one piece of machinery is a strong signal about
sequencing.** Lambda lifting already exists (`hoist.rs`).

Gate 2 needs care but is satisfiable: functions as *values* keep the call graph
decidable as long as they cannot be *constructed from names at runtime*, which
the bright line already forbids.

### Iterator protocol — passes; generators do not ride along

`for` over a student-defined type is a real pedagogical win and gate 4 is easy
to satisfy.

But it is the doorway to generators, which need heap-allocated frames or a
state-machine transform — a gate 1 problem on the Pico. **The protocol passes;
generators are a separate decision and must not arrive on its coattails.**
Noting this in advance is the whole point of having a policy: the second
feature is the one that gets waved through.

## Proposing a feature

Answer the four gates, in writing, before writing code. Name the tier. If it is
Tier 3, show the CPython spelling that makes it a no-op. If gate 3 is the hard
one, that is the useful signal — write the error message first and the design
usually follows.
