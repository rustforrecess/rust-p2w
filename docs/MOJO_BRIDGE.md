# The Mojo bridge — valid Python always, valid Mojo when it counts

Decided 2026-08-21, the week Mojo 1.0 shipped (Aug 11), its compiler went
Apache 2.0 (Aug 18), and Modular formally abandoned the Python-superset goal.

## The decision it implements

**Look, don't shift.** Mojo occupies none of this project's load-bearing axes
(no wasm/browser target, no MCU story, a compiler we wouldn't own, no classes,
not Python) — but the syntax overlap between our typed house style and Mojo's
def-mode is convergent evolution: both landed on no type-changing rebinding,
homogeneous lists, machine-width ints. **Our typed tier approximates the
Python∩Mojo intersection**, so instead of moving toward Mojo we formalize the
intersection and make membership checkable.

The claim, precisely: every p2w program is valid Python (the permanent
invariant, unchanged); a p2w program with **zero findings from
`p2w check --profile mojo`** is additionally believed to compile and run as
Mojo 1.0 when concatenated with `tools/mojo/p2w_prelude.mojo`.

## The three pieces

1. **The profile** (`p2w check --profile mojo`, `lint::mojo_profile_warnings`)
   flags what is outside the intersection: classes (Mojo's Phase 3 hasn't
   begun), f-strings (found in the token stream — they desugar before the
   AST), heterogeneous literal lists, `import random` (no prelude mapping
   yet), and type-changing rebinding (the existing type-churn lint, which IS
   Mojo's variable rule, included verbatim). Findings are teaching output,
   never gates — the program stays valid p2w.

2. **The prelude** (`tools/mojo/p2w_prelude.mojo`) aliases Python's lowercase
   type names to Mojo's capitals (`alias int = Int`, …) so the shim lives on
   Mojo's side. p2w programs never contain a non-Python spelling.
   (`def f(x: Int)` would NameError under today's CPython at def time —
   Python 3.14's lazy annotations soften this eventually, but we don't lean
   on it.)

3. **The verification job (`mojo-bridge` in CI, tools/mojo/mojo_diff.sh):**
   every program in `tests/mojo_bridge/` the profile calls ready is compiled
   and run by the REAL Mojo compiler (installed from Modular's conda channel
   via pixi), with the prelude prepended and top-level statements wrapped in
   `def main()` by `tools/mojo/wrap.py`; output must match CPython
   byte-for-byte. `not_*.py` cases assert the profile REFUSES them.

## What first contact with Mojo 1.0.0 taught (2026-08-21)

The typed-procedural core passed unmodified on the first run — real Mojo
compiled and ran the arithmetic and float cases with CPython-identical
output. Three corrections came from the compiler, not the docs:

- **`len(<str>)` is a hard ERROR in Mojo** — UTF-8 makes a single length
  ambiguous (bytes? code points? graphemes?), and Python's `len(str)` means
  code points. No clean prelude shim exists, so the profile flags visible
  cases and the differential job catches dynamic ones.
- **Implicit variable declaration is deprecated even in `def`** (a warning
  in 1.0; presumably gone in 2.0). The annotated first assignment —
  `s: str = ...`, valid Python, our typed house style — is the
  future-proof spelling. The intersection is narrowing toward exactly the
  house style we already teach.
- **`alias` became `comptime`** in 1.0; the prelude uses the new keyword.

## Honest gaps that no bridge closes

- **Classes.** The `class Dog(Animal)` curriculum arc has no Mojo form until
  their Phase 3 ships. The bridge covers the typed procedural tier.
- **Integer edges.** The same program has three behaviors at the extremes:
  CPython bignums, p2w traps at i32, Mojo wraps at i64. Classroom-scale
  values agree; the boundaries don't, and the profile does not pretend
  otherwise.
- **Stdlib surface.** `math` mostly maps; `random` doesn't yet; anything
  else is discovered by the differential job, not asserted here.

## Reopen conditions (for the larger shift-to-Mojo question)

Re-evaluate the look-don't-shift decision if any of: Mojo ships a
wasm/browser target; Mojo ships classes plus an education push; Mojo appears
in K-12 standards. None exists as of 2026-08-21.
