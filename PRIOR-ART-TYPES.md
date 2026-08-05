# What everyone else decided

Companion to `PRIOR-ART-AGENTIC.md`, and the evidence behind the five files in
`tests/oracle/open-question/`. Every project here has faced the same forks. The
useful finding is **how little disagreement there is on most of them** — which
turns three of our five open questions from design work into a decision about
wording, and concentrates the real work in one place.

Verified from primary sources in August 2026 unless marked otherwise; links at
the bottom.

## The field

| | what it is | why it made these choices |
|---|---|---|
| **RPython** | PyPy's implementation subset, whole-program annotator | it has to compile to C |
| **Shed Skin** | Python → C++, whole-program inference | speed |
| **Codon** | Python-like → LLVM (Exaloop/MIT) | speed |
| **Mojo** | Python-family systems language (Modular) | speed, MLIR, hardware |
| **SPy** | Static Python (Antonio Cuni, PyPy/HPy/PyScript lineage) | speed with Pythonic feel |
| **Cython** | Python → C, gradual annotations | speed, incrementally |
| **mypy** | checker only, no codegen | it defines what Python programmers expect |

**None of them is a teaching language.** Every decision below is justified by
performance. That is the gap we are standing in, and it is why a decision can
legitimately go the other way here without being wrong.

---

## Q1 — may a name change type?

**Unanimous: no.**

- **Mojo** — *"the variable receives a type when it's created, and the type
  never changes."*
- **Codon** — variables are typed at compile time; dynamic modification of
  types is not allowed.
- **RPython** — *"variables should contain values of at most one type … at each
  control flow point."*
- **mypy** — `Incompatible types in assignment`. Pyright is looser: it narrows
  per assignment rather than erroring.
- **Cython** — the only split answer, and worth noting because it is the
  gradual one: an untyped name is a Python `object` and may change; a `cdef int
  x` may not. The choice is per variable, made by the programmer.

**⭐ Steal RPython's wording.** *"At most one type at each control flow point"*
answers `reassigned-to-another-type.py` **and** `branches-disagree.py` with a
single rule. Those two must get the same answer — two different answers to one
underlying question is how a language turns into folklore — and this phrasing
gets it for free.

The open part is not whether, it is **how loudly**. Nobody here has a category
for "legal but probably not what you meant", because a compiler chasing speed
has no use for one. We do — it is what `lint::Scaffold` already is.

## Q2 — heterogeneous lists?

**Unanimous: homogeneous.**

- **Codon** — requires homogeneous lists explicitly; `[1, "foo"]` is not a list.
- **RPython** — lists are allocated arrays with an element type.
- **Shed Skin** — its own known failures include mixing strings and lists of
  strings in one list.
- **mypy** — permits it by joining to `list[object]`, but mypy has no codegen to
  satisfy. Once there is a memory layout, the join stops being free.

**⭐ Codon's escape hatch is the answer to give a student**: tuples stay
heterogeneous, because their length and element layout are statically known.
"A list holds many of one thing; a tuple holds a few different things" is a
true sentence about the machine *and* a teachable one, which is rare.

This confirms the note in `mixed-list.py`: this was never only a type question.
`list[T]` is what makes unboxed elements possible, and unboxed elements are
what the Pico target needs.

## Q3 — how polymorphic is an unannotated function?

**⭐ THE ONE THAT IS ACTUALLY OPEN.** Four strategies, genuinely different:

- **Shed Skin — Cartesian Product Algorithm** (Agesen 1995). Duplicates a
  function per tuple of actual argument types, automatically; paired with
  Plevyak-style iterative flow analysis for data polymorphism. Best ergonomics
  of the five: the student writes nothing. Cost: whole-program analysis.
- **RPython — unify across all call sites** into one signature; if they do not
  unify, an annotation error. Opt-in `@specialize.argtype(n)` forces
  duplication. *(The decorator is from knowledge of `rpython.rlib.objectmodel`,
  not the fetched page.)* Simplest to build, **worst error messages** — the
  failure surfaces at a merge point far from either cause, which is precisely
  the failure mode `tests/oracle/README.md` exists to prevent.
- **Codon — monomorphization.** Instantiate per type, C++-template-shaped, with
  inference doing the work templates make you write.
- **Mojo — you write it.** Explicit parameters: `fn f[T: Trait](x: T)`.
- **SPy — evaluate it at compile time.** The *redshift* pass colours every
  expression **blue** (computable at compile time) or **red** (must run at
  runtime); generic machinery resolves during the blue phase, so the dynamism
  costs nothing because it is gone before runtime.

### Two things that fall out of this for us

**1. The standard objection to whole-program analysis does not apply here.**
CPA and RPython's annotator are usually dismissed because they break separate
compilation. We compile one program, in a browser, with no linking model and no
library ecosystem to link against. **The approach that is expensive for
everyone else is cheap for us.**

**2. Monomorphization and the FBIP plan are the same mechanism.** If `list[i64]`
must be unboxed on the Pico, we are already specialising per type. So Q3's
answer is partly *forced by the memory model* rather than free — which is an
argument for deciding it alongside the layout work, not before it.

## Q4 — a function that sometimes returns nothing

- **mypy** — `Missing return statement`, on by default.
- **Mojo, Codon** — explicit `Optional[T]`.
- **RPython** — ⭐ the interesting one: *"It is allowed to mix None … with
  wrapped objects, class instances, lists, dicts, strings, etc. but **not** with
  int, floats or tuples."*

**RPython's rule is derived from representation, not from taste.** Nullable
works where there is a pointer to make null, and does not where the value is
unboxed. That is our Pico constraint, reached independently by someone who had
to ship it. If we end up with unboxed scalars, `Optional[int]` costs a tag and
`Optional[Dog]` costs nothing — and that difference will exist whether or not we
choose to explain it.

The pedagogical option in `function-with-no-return.py` — require every path to
return, and make the error *"what should this give back when the number is
small?"* — has **no precedent among these projects**. It also has nothing
contradicting it. It is a teaching argument, and none of them were making one.

---

## What this changes

1. **Q1 and Q2 are settled by consensus.** Ours is a wording decision (error
   versus scaffolded lint), not a design decision. Cheap to close.
2. **Q3 is the real work**, and the two observations above narrow it: whole-
   program is affordable for us, and the memory model has a vote.
3. **Q4 has a representation-derived answer available** (RPython's) and a
   pedagogical one available (total functions) and they are not compatible.
   That one is Jason's.
4. **Everything here optimises for speed.** Every message these compilers emit
   was written for someone who already knows what a type is. That is the
   opening, and it is the same opening as `PRIOR-ART-AGENTIC.md` found: the
   machinery is well explored, the *pedagogical output* is not.

## Sources

- Mojo — <https://mojolang.org/docs/manual/variables>
- Codon — <https://docs.exaloop.io/language/overview/>
- RPython — <https://rpython.readthedocs.io/en/latest/rpython.html>
- SPy, "Inside SPy part 2: Language semantics" —
  <https://antocuni.eu/2026/03/25/inside-spy-part-2-language-semantics/>
  (summary via search; the page refused direct fetch)
- SPy repo — <https://github.com/spylang/spy>
- Agesen, *The Cartesian Product Algorithm* (ECOOP 1995) —
  <https://link.springer.com/chapter/10.1007/3-540-49538-X_2>
- mypy — <https://mypy.readthedocs.io/en/stable/type_inference_and_annotations.html>

No source code from any of these projects was read. See `NOTICE`.
