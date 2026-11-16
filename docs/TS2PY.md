# TypeScript to Python, deterministically (`ts2py`) — proposal

**Status: PROPOSAL, not built (2026-11-15).** Nothing here is implemented.
Two areas are deliberately left open: exceptions and async (see § Open
questions), because p2w's own direction on both is undecided.

## The proposal in one paragraph

A separate tool, `ts2py`, converts a small subset of TypeScript into p2w
Python by a fixed table of rules: the same TypeScript always gives the same
Python, every output line traces to a documented rule, and anything outside
the subset is refused in TypeScript's terms rather than approximated. The
output is ordinary p2w source, so every existing tool works on it, and
whether a conversion was right is checked by running both programs and
comparing what they print. It is a teaching bridge for students moving
between web work and the Python/robot track, shown side by side in the IDE.

## Why deterministic matters

Asking an AI to translate code gives a plausible answer that has to be
trusted. A rule-based converter gives an answer that can be checked:

- **Same input, same output.** No sampling, no hidden context; the result
  can be diffed, tested and cited.
- **Every line has a reason.** The IDE can show which rule produced a line,
  and the rules are a short document a student can read.
- **Correctness is testable.** Run the TypeScript under Node and the Python
  under CPython and p2w, and compare output. A mismatch is a converter bug.
- **Refusals are honest.** Where the languages differ in meaning, the tool
  says so instead of guessing.

That is the project's verification-literacy argument applied to
translation: a glass box whose answers can be checked.

## Shape

- **A separate tool, not part of the compiler.** It needs a TypeScript
  parser (`oxc` or `swc`, both Rust and both handle TypeScript); rust-p2w
  keeps its one runtime dependency.
- **Output is Python source**, not an internal syntax tree, so the result is
  a normal p2w program: lints, type checker, step debugger, blocks view and
  both backends work unchanged, it runs under CPython, and "every p2w
  program is valid Python" holds by construction.
- **A line map** from TypeScript spans to Python spans, so the IDE can show
  the two side by side and highlight corresponding lines. Comments carry
  across.
- **Possibly both directions** for the same subset (Python to TypeScript),
  so a student can start from either side. Not needed for a first version.

## The subset

Roughly a first programming course:

- functions with typed parameters and returns; `let` and `const`
- `number`, `string`, `boolean`; arrays (`number[]`, `string[]`)
- `if`, `for`, `while`, `break`, `continue`, `return`
- classes whose fields are declared with types and set in the constructor,
  methods, single inheritance (`extends`), `super`
- template literals; common string and array methods with a direct
  equivalent; `Math.*` with a `math` equivalent; `console.log`

Refused, in TypeScript's words ("`x.foo = 1` on line 7 adds a property the
class doesn't declare; declare `foo` in the class"):

- objects that gain properties at runtime, `any`, index signatures
- `==` with type coercion, `undefined` as a value
- closures that assign to an outer variable
- `var`, `this` outside a class method, prototypes
- modules and imports beyond a single file (first version)
- **exceptions and async: left open** (§ Open questions)

## Rules where the meanings differ

The useful lesson is where the languages disagree. Each row is a rule plus
a test program that runs under Node, CPython and p2w.

| | TypeScript | Python | Rule |
|---|---|---|---|
| Numbers | one `number` type, always a float | `int` and `float` | infer int or float from types and use; where it can't tell, refuse and ask for a clearer type |
| Printing a whole float | `console.log(3.0)` prints `3` | `print(3.0)` prints `3.0` | follow the inferred type; where that would change output, refuse or say so |
| Block scope | `let` is scoped to its block | a variable belongs to the whole function | rename when a block reuses an outer name, so behaviour is preserved |
| Division | `7 / 2` is `3.5`; `Math.floor(7 / 2)` is `3` | `7 / 2` is `3.5`; `7 // 2` is `3` | `Math.floor(a / b)` on ints becomes `a // b` |
| Equality | `===` | `==` | direct; `==` (coercing) is refused |
| Strings | `` `hi ${name}` `` | `f"hi {name}"` | direct |
| Counting loops | `for (let i = 0; i < n; i++)` | `for i in range(n)` | when the pattern matches exactly; otherwise a `while` loop |
| Fields | declared in the class body | set in `__init__` | matches p2w's fixed attribute sets (`docs/CLASS_LAYOUT.md`) |

## Checking correctness

The same approach as `tests/backend_diff.rs`, one level up:

1. Every rule has a TypeScript test program.
2. Run it under Node (TypeScript's type-stripping mode or `tsc`), convert
   it, and run the Python under CPython and p2w (WASM and native).
3. All outputs must match; a mismatch fails the build.

A generated table of rules and results, like `RUNTIME_SEMANTICS.md`, keeps
the documentation from drifting from the behaviour.

## Plan

| Step | Work | Gate |
|---|---|---|
| 1 | Parser, subset checker with TypeScript-worded refusals, and the converter for the subset above (one direction) | each rule's test program prints the same under Node, CPython and p2w |
| 2 | The line map and a side-by-side TypeScript / Python view in the IDE | highlighting a line in one pane highlights the matching line(s) in the other (e2e test) |
| 3 | Python to TypeScript for the same subset | round trip: TS → Py → TS gives the same program, up to formatting |
| 4 | Exceptions and async, once p2w's direction on them is decided | — |

## Open questions

1. **Exceptions.** p2w has no `try`/`except` or `raise` today (FEATURE_PROBES
   lists them as rejected). TypeScript code uses `try`/`catch`/`throw`
   heavily. Whether and how p2w gains exceptions decides what the converter
   can do; until then they are refused. Left open on purpose.
2. **Async.** TypeScript's `async`/`await` and promises have no p2w
   counterpart, and p2w's robot programs use callbacks instead. Whether p2w
   gains async, keeps callbacks, or maps one to the other is undecided.
   Left open on purpose.
3. Does the converter infer `int` vs `float`, or require TypeScript code
   written for it to say (`// int` comments, branded types)?
4. Where printing would differ (`3` vs `3.0`), refuse, or convert and show
   the difference as a teaching moment?
5. One direction first, or both from the start?
6. Which parser, `oxc` or `swc`? Decide by checking their TypeScript
   coverage and spans when step 1 starts.
