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
  insertion order) — don't rely on it, exactly as in CPython. All three paths
  (browser Run/Debug and native) display sorted.
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

## Default arguments and `input()` (small, deliberate divergences)

- **A default expression is evaluated per call, not once at `def` time** (both
  backends: compile-time substitution at the call site). For immutable
  defaults — the K-12 norm — this is identical to CPython. For a mutable
  default (`def f(xs=[])`) CPython shares ONE list across calls (the classic
  footgun); we give each call a fresh one. Deliberately kinder; noted for
  honesty.
- **`input()` at end-of-input returns `""`** instead of raising `EOFError`
  (no exceptions in the subset; friendlier on a device stream). The reference
  p2w has no `input()` at all.

## Classes (native v1)

Construction, `__init__`, instance attributes, methods, `self`, single
inheritance, `super().method(...)`, and `__repr__`/`__str__` in `print`/`str`
all work (compile-time switch dispatch on a per-instance class id; reference
semantics and leak-freedom are oracle-gated). Differences:

- **Default display is `<Dog object>`**, not CPython's
  `<__main__.Dog object at 0x...>` (no addresses on a deterministic teaching
  device — CPython's form isn't reproducible anyway).
- **Operator dunders are dispatched**: `__add__`/`__sub__`/`__mul__`,
  `__eq__` (direct, then reflected, then identity — so `!=`, `in`, and
  dict-key lookups all use it, and `obj == 5` is `False` like CPython),
  `__lt__`/`__le__`/`__gt__`/`__ge__`, `__len__`, `__getitem__`. A dunder the
  backend doesn't dispatch (e.g. `__setitem__`) is a clean compile error, as
  is a dispatched dunder with the wrong parameter count. No reflected
  arithmetic (`__radd__`-style) yet: `5 + obj` is a clean runtime error.
- **Class variables work** (instance attrs shadow them; the fallback walks
  the inheritance chain, nearest class first; `c.limit = 3` writes the
  INSTANCE, leaving the class value untouched — all CPython-matching).
  Reading via the class name (`Counter.limit`) isn't supported yet, and in
  the step debugger the values must be simple constants (like function
  defaults). **First-class methods** (`f = d.speak`) stay a clean error:
  "a method isn't a value yet — call it".

## Other gaps (clean errors, not silent differences)

- **f-string format specs** (`f"{x:.2f}"`, width/fill/align, `d`/`s`) work on
  **both** backends with the same compile-time spec parsing (float formatting
  rounds ties-to-even, like CPython); exotic specs are a clean "unsupported
  format spec" error.
- **Tuples** are immutable by convention (lowered to lists internally).
- **`sorted(seq, reverse=True)`** works on the browser backend (stable, both
  directions). `key=` needs first-class functions and isn't supported yet;
  `sorted` on native/in the step debugger is still pending (the Pico-parity
  track). A bad keyword is a clean error.
- **`list()` / `tuple()` / `dict()`** work: empty (`list()`, `dict()`) or from
  any iterable (`list("abc")`, `list(range(n))`, `tuple(a_set)`). `dict()` is
  empty only — `dict(mapping)` / `dict(pairs)` aren't supported yet (use `{}` or
  a dict comprehension). `list(range(n))` works on the browser backend but not
  yet on native or in the step debugger (both lack `range` as a first-class
  value — materialize with a comprehension there); every other iterable form
  works on all three.
- **`reversed(seq)`** desugars to the reverse slice `seq[::-1]`, so it works on
  both compiled backends (lists, strings, tuples). It yields a reversed *copy*
  rather than CPython's lazy iterator — identical when you iterate it, and
  `print(reversed(xs))` shows `[3, 2, 1]` instead of CPython's
  `<list_reverseiterator …>` (kinder). A `range` isn't sliceable, so
  `reversed(range(n))` needs a list first (`reversed(list(range(n)))`); like
  slicing, it isn't in the step debugger yet (use Run).
- **Starred unpacking** (`a, *rest = xs`) desugars to a temp plus indexed and
  sliced reads, so it runs on both compiled backends (but not the step
  debugger — it needs slicing, which the stepper doesn't have yet; use Run).
  With *too few* values and fixed targets on **both** sides (`a, *mid, b = [1]`),
  CPython raises `ValueError`; here the end targets can alias instead of
  erroring (the same length-leniency the plain unpack already has on native).
  Provide enough elements — the common `a, *rest` / `*init, last` forms bind
  exactly like CPython.
- **`lambda` works only as `name = lambda params: expr`** (all backends) — it
  desugars to the equivalent `def`, so functions still aren't first-class
  values. Any other lambda position is a friendly, specific error. Defaults
  work (`lambda n, k=10: ...`); blocks/text round-trips canonicalize the
  spelling to `def`.
- **Not yet implemented on native:** generators, `*args`/`**kwargs`,
  exceptions. These are rejected with a clear "not in the native backend yet"
  message rather than miscompiling.

## What's faithful

For completeness, the supported subset matches CPython on: int/float arithmetic
and comparisons, strings (`+`, indexing, slicing, `in`), f-strings (incl.
format specs), lists (incl. `list[int]`/`list[float]`), dicts, sets
(values/ops/methods — see the Sets section for the display note), tuples
(incl. as set elements), control flow, the **conditional expression**
(`a if cond else b` — right-associative, only the taken branch evaluated),
classes (v1 — see above), functions + recursion + default arguments + keyword
arguments, **sequence repetition** (`"=" * 40`, `[0] * n`, either order;
`n <= 0` gives an empty copy), the **`list()` / `tuple()` / `dict()`
constructors** (see the note below), `for`/`while`, **list, dict, and set
comprehensions** (nested,
filters, `range`, tuple targets), tuple unpacking (incl. **starred** — `a, *rest = xs`, `*init, last = xs`,
`a, *mid, b = xs`; see the note below), **chained assignment**
(`x = y = value` — value evaluated once, all names bound to it), **`del`** of a
list/dict item (`del xs[i]`, `del d[key]` — deleting a whole variable isn't
supported), `str()`, `len()`,
`input()`, and `print()` — all gated by the CPython differential oracle
(`tools/native_run.sh`), which also requires leak-freedom (`live == 0`).
