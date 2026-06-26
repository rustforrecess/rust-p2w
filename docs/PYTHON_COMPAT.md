# Differences from CPython

rust-p2w compiles a **subset** of Python. On that subset it aims for the same
*results* as CPython, so what kids learn transfers. A few behaviors differ on
purpose (or pending work) — they're listed here. Unless noted, this describes the
**native (Pico) backend**; the browser (WASM-GC) backend shares the front-end and
semantics but uses the engine's garbage collector (so the cycle note below
doesn't apply there).

The guiding rule: **correct, portable Python behaves identically.** The
divergences below only surface in code that relies on unspecified behavior (e.g.
set ordering) or on features we don't implement yet.

## Sets

Set *values and operations* are faithful: `&` (intersection), `|` (union), `-`
(difference), `^` (symmetric difference), `in`/`not in`, `len`, iteration, and
deduplication all produce the same elements as CPython. `&`/`|`/`^` are also
integer bitwise operators, exactly like CPython (`6 & 3 == 2`).

Differences:

- **Print/`str()` order is sorted (canonical), not CPython's hash order.** When a
  set's elements are homogeneously orderable (all numbers, or all strings), it
  *displays* in sorted order — the canonical written form (`print({3, 1, 2})` →
  `{1, 2, 3}`), which is deterministic (stable answer keys) and reinforces that
  sets are unordered. Mixed-type sets (e.g. `{1, "a"}`) fall back to insertion
  order. This is display only: **iteration order is unspecified** (currently
  insertion order) — don't rely on it, exactly as in CPython. (On the native
  Pico backend, set *display* is still insertion order pending a `no_std` sort;
  the browser Run/Debug paths are sorted.)
- **Membership and set ops are O(n)** (a small, list-backed set), vs CPython's
  O(1) hashing. Same answers; only matters for large sets, which a teaching
  program on a microcontroller doesn't build. The backing store can be swapped
  for a hash table behind the same ABI if that ever changes.
- **Set members must be immutable, like CPython.** A list, dict, or set can't be
  a set element — it raises a friendly error (`unhashable type … use a tuple`). A
  **tuple** is allowed (tuples are immutable). We don't yet check a tuple's
  *contents*, so `{(1, [2])}` is accepted here though CPython rejects it — a minor
  leniency.
- **Set methods take a set argument.** `.add()`, `.remove()`, `.discard()`,
  `.pop()`, `.clear()`, `.copy()`, `.union()`, `.intersection()`, `.difference()`,
  `.symmetric_difference()`, `.issubset()`, `.issuperset()` all work; the
  binary-operation methods require a *set* argument, whereas CPython also accepts
  any iterable (`s.union([1, 2])`). Use a set literal or `set(...)` for the arg.

## Integers

- Typed integers (`x: int`, `def f(n: int)`) are **32-bit and wrap around**
  (matching the hardware), not Python's arbitrary-precision `int`. Dynamic
  (unannotated) ints are full `i32` too — heap-boxed beyond the 30-bit inline
  range, so they never silently truncate, but they still wrap at 2³¹.
- `/` is true division (always a float), `//` floors, `%` follows Python's sign
  rules — all matching CPython.

## Reference cycles (native backend only)

Memory is managed by precise reference counting with no garbage collector, so a
**reference cycle leaks** (e.g. a list that contains itself). CPython's cyclic GC
reclaims it. This only affects programs that build self-referential containers —
`rust_p2w::may_form_cycle(source)` reports whether a program is cycle-free (and
therefore leak-free). The browser backend uses WASM-GC and is unaffected.

## Other gaps (clean errors, not silent differences)

- **f-string format specs** (`f"{x:.2f}"`) aren't supported; plain `f"{x}"` is.
- **Tuples** are immutable by convention (lowered to lists internally).
- **Not yet implemented:** classes, generators, `lambda`, `*args`/`**kwargs`,
  exceptions. These are rejected with a clear "not in the native backend yet"
  message rather than miscompiling.

## What's faithful

For completeness, the supported subset matches CPython on: int/float arithmetic
and comparisons, strings (`+`, indexing, slicing, `in`), lists (incl.
`list[int]`/`list[float]`), dicts, control flow, functions + recursion, `for`/
`while`, list & dict comprehensions (nested, filters, `range`, tuple targets),
tuple unpacking, `str()`, `len()`, and `print()`.
