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

3. **The verification job (planned, and the reason to believe any of this):**
   compile the profile-clean oracle programs with the real Mojo compiler and
   diff their output — the same differential pattern as the CPython oracle
   and BACKEND_DIVERGENCE. Until it lands, "believed valid Mojo" is a
   documented intent, not a verified claim, and this file says so.

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
