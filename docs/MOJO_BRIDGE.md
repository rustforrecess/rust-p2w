# The Mojo bridge — valid Python always, valid Mojo when it counts

Decided 2026-08-21, the week Mojo 1.0 shipped (Aug 11), its compiler went
Apache 2.0 (Aug 18), and Modular formally abandoned the Python-superset goal.

## PAUSED 2026-10-19

The `mojo-bridge` CI job (`tools/mojo/mojo_diff.sh`) is disabled
(`if: false` in `.github/workflows/ci.yml`), not deleted. What broke it:
Mojo 1.1 made `var` mandatory for every new local binding — annotated or
not. The "What first contact with Mojo 1.0.0 taught" section below already
spotted the warning and predicted the future-proof spelling would be an
annotated first assignment (`s: str = ...`); that prediction didn't survive
contact with 1.1 — Mojo rejects the annotated form too, `var` is the only
thing that satisfies it.

That alone would just be a syntax inconvenience, except `var` is
**block-scoped** in Mojo, while implicit Python assignment (and this
project's whole semantic model) is **function-scoped** — a name assigned
anywhere in a function is visible throughout it. So there's no safe
mechanical fix: inserting `var` at a name's first assignment changes
*behavior*, not just syntax, for any name first assigned inside an `if`/
`for`/`while` and read afterward:

```python
if x > 0:
    total = 1
else:
    total = -1
print(total)
```

`var`-at-first-assignment would give each branch its own block-scoped
`total`, invisible outside the `if` — `print(total)` breaks. That pattern
is completely ordinary code, not an edge case, so there's no honest way to
auto-patch around it. (Scoping this to only unconditionally-first-assigned
names was considered — it would have covered the test cases we actually
hit — but it still abandons the bridge's real claim, see below, so it
wasn't pursued either.)

This matters more than a compatibility wrinkle because it breaks the
specific claim the bridge existed to make: **the same text, unchanged, is
valid in both languages.** A mechanical rewrite step (even a correct one)
is a different, weaker claim — "Python code that can be converted to
Mojo" — and this one can't even be made correct in general. Per the design
principle at the top of this doc, a bridge that's sometimes silently wrong
is worse than no bridge: exactly the "silent divergence" failure class the
whole profile exists to refuse, just relocated from text-splitting to
variable scoping.

Modular's own announcement of the `var` requirement left room for it to
be revisited post-1.0 (cited as an additive change that would preserve
1.0 source compatibility), so this is parked pending that, not closed for
good. The `## Reopen conditions` section below predates this and lists
different triggers (Mojo shipping wasm/classes/K-12 presence) — none of
those fired; this is a new condition the doc didn't anticipate.

Rest of this document is kept as-is below — the historical record of what
was tried and verified before this.

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
Mojo 1.0 when concatenated with `tools/mojo/p2w_prelude.mojo`, **with the
same output for text whose characters are single code points** (composed
accents, single emoji — keyboard-typed text). Text that splits — combining
marks, ZWJ emoji — is REFUSED by the profile where it appears in the
program, because Python and Mojo would both run it and print different
counts: silent divergence, the one failure class worse than an error.
Exotic text arriving at RUNTIME (typed into `input()`) is outside any
static claim; that residual gap is documented below, not hidden.

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

## What deeper probing taught (2026-08-22)

**Mojo's `for c in s` yields GRAPHEME CLUSTERS, not code points** — Swift's
layer, the characters a person sees (their 1.0 also added UAX #29
segmentation and `count_graphemes()`). Python iterates code points. The two
agree exactly when every grapheme is one code point: composed accents,
single emoji — keyboard-typed text. A decomposed accent (e + combining
mark) or ZWJ emoji splits them (Python 2/3 pieces, Mojo 1). So the counting
loop transfers for everyday text, and the bridge's claim carries that
precision. Delicious for the three-lengths lesson: within Mojo itself,
`len` refuses to pick a layer while iteration picked the HUMAN one — even
one language answers "how long is a string" two ways.

Their trajectory, for the record: pre-1.0 `len(String)` returned BYTES;
1.0b1 deprecated it toward `byte_length()`/`count_codepoints()`; 1.0
removed it with the teaching error and flipped iteration to graphemes. No
proposal to restore a default exists — for them it is settled.

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
- **Runtime text.** The profile refuses grapheme-splitting LITERALS, but a
  string typed into `input()` at runtime reaches iteration unchecked — a
  decomposed accent or ZWJ emoji there prints different counts on the two
  sides. No static check can close this; it is the price of the layers
  disagreeing, and the three-lengths lesson is where a student learns it.

## Known divergences — tracked, not gating (updated 2026-10-24)

CPython compatibility is the priority when these conflict with Mojo; the
bridge stays opt-in and advisory. Kept here as one scannable list so a
future Mojo release can be checked against it without re-deriving each
point from the sections above.

| divergence | since | watch for |
|---|---|---|
| `var` mandatory + block-scoped vs. Python's function-scoped implicit assignment — the job-disabling one | Mojo 1.1 | Modular relaxing the `var` requirement post-1.0 (left open in their own announcement) |
| `len(<str>)` is a hard error in Mojo (bytes/codepoints/graphemes ambiguity) | Mojo 1.0 | a default `len` landing for `String` |
| `for c in s` yields grapheme clusters in Mojo, code points in Python — diverges on decomposed accents / ZWJ emoji | Mojo 1.0 | unlikely to change; Swift-style grapheme iteration looks settled on their side |
| No classes in Mojo (Phase 3 unstarted) | ongoing | Mojo shipping classes |
| Integer edges: CPython bignums / p2w traps at i32 / Mojo wraps at i64 | ongoing | n/a — three-way, not a pairwise fix |
| `random` stdlib has no prelude mapping | ongoing | low priority, nobody's blocked on it |

When any "watch for" fires, re-run `tools/mojo/mojo_diff.sh` by hand against
a current Mojo install before touching CI — don't re-enable the `mojo-bridge`
job speculatively, confirm first that the specific blocking divergence is
actually gone.

## Reopen conditions (for the larger shift-to-Mojo question)

Re-evaluate the look-don't-shift decision if any of: Mojo ships a
wasm/browser target; Mojo ships classes plus an education push; Mojo appears
in K-12 standards. None exists as of 2026-08-21.
