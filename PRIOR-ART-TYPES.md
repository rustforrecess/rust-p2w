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

---

## Recent research (2017–2026)

The compilers above answer *what the type system permits*. A separate body of
work answers *what the compiler should say when you get it wrong* — and it has
moved a lot recently. The short version: **the field has split into two camps
that do not read each other**, and the gap between them is where this project
sits.

### ⭐ The result that matters most: handwritten beats generated

**"Not the Silver Bullet: LLM-enhanced Programming Error Messages are
Ineffective in Practice"** — Santos & Becker, UKICER 2024.

106 participants, six buggy C programs, within-subjects, measuring time-to-fix
rather than expert opinion. GPT-4-generated error messages beat conventional
compiler messages in **1 of 6 tasks**. And:

> Handwritten explanations still outperform LLM and conventional error
> messages, both on objective and subjective measures.

Students *preferred* the GPT-4 explanations while not being any faster with
them. **Preference and effectiveness came apart**, which is the single most
useful thing to know before wiring an AI tutor to a compiler.

A 2025 follow-up sharpens it rather than overturning it: *fine-tuned* GPT-4o
messages did produce significantly faster fixes where baseline GPT-4o did not.
The active ingredient is authored pedagogical content, not model scale.

**What this means here:** `lint::Scaffold` — authored question/hint/fix ladders,
written by a teacher — is the intervention with the best evidence behind it, and
we already have the machinery. The copilot's job is to *route to* a ladder, not
to improvise one. An improvised explanation will feel better and not be better,
and the feeling is what gets it shipped.

### ⭐ Helium — the direct ancestor, and nobody continued it

**Heeren, Hage & Swierstra** — *Helium, for Learning Haskell* (Haskell Workshop
2003), *Scripting the Type Inference Process* (ICFP 2003), *Type Class
Directives*.

A Haskell compiler built specifically to give beginners good type errors. Two
ideas worth taking whole:

1. **Split inference into constraint GENERATION and constraint SOLVING.** Once
   those are separate, message quality becomes a property of the *solver* — you
   can reorder, reweight and explain without touching the language definition.
   This is the architectural decision that makes everything else possible.
2. **Type inference directives** — externally supplied instructions that script
   the inference process. Including **sibling functions**: declare which
   functions beginners commonly confuse, and when inference hits an
   inconsistency, the solver tries the sibling as a candidate fix.

**Directives are DATA, not compiler code.** A teacher who notices that their
class keeps confusing two things can say so, without touching Rust. That is the
same seam as `Scaffold`, one level deeper — and it maps cleanly onto a
constraint store: generation produces facts, directives are rules.

Helium is from 2003, targets Haskell, and never became the way people learn.
**The mechanism was proven and then left on the shelf.**

### Type error localization: it is a solved-ish research problem

- **SHErrLoc** (Zhang & Myers, TOPLAS 2017) — counter-factual unification plus
  **error-tolerant typing**: keep type-checking after an error instead of
  stopping. We already do the parser equivalent (recovery, partial blocks), so
  this is consistent with what exists.
- **Counter-factual typing** (Chen & Erwig) — the earlier formulation.
- **⭐ "Learning to blame: localizing novice type errors with data-driven
  diagnosis"** (Seidel et al., OOPSLA 2017). Trains on **pairs of ill-typed
  student programs and their fixed versions**, then predicts which
  sub-expression to blame. Top-1 accuracy **72%**, versus 44% for the OCaml
  compiler and 56% for SHErrLoc.

**The asset nobody else has:** that training corpus is *exactly* what the IDE
observes — a student's program before the fix and after it. We would be
collecting the highest-value dataset in this subfield as a side effect of
running lessons. The oracle corpus is a hand-built, 21-program version of the
same thing. (Consent and FERPA get decided before any of that, obviously.)

### Inference: static is beating ML again, and interpretably

- **⭐ Typify** (Aman, Asaduzzaman & Wang, **ICPC 2026**) — Python type
  inference by symbolic execution, iterative fixpoint analysis and dependency-
  graph traversal, **no machine learning**. Matches or surpasses Type4Py,
  HiTyper and Pyre on ManyTypes4Py/Typilus. Pitched explicitly as *"practical,
  interpretable, and computationally efficient."*
- **TypyBench** (ICML 2025) — LLMs score decently per annotation but **struggle
  with global consistency**, which is the one property a compiler cannot do
  without.

Both point the same way and both point at us. A glass-box compiler cannot use a
neural type inferencer — not because it would not work, but because "why does it
think that?" has to have an answer. Fixpoint analysis has one; a model does not.
And *interpretable* being the selling point of an ICPC 2026 paper says the wind
is behind this.

### The frame that goes furthest: errors against a *process*

**"A Design Recipe and Recipe-Based Errors for Regular Expressions"** —
Morazán et al., TFPiE 2025 (EPTCS 424).

Students follow an explicit design recipe; when something fails, the error names
**the step of the recipe not successfully completed**, not the implementation
failure. Messages are held to being *"concise, succinct, jargon-free, and
nonprescriptive."*

This is a different axis from better wording. It says an error should be
reported **against the process the student is being taught**, not against the
compiler's internal state. It generalises the fix ladder: a ladder helps you
repair a line; a recipe-based error tells you which part of *how to build this
kind of thing* you skipped. Worth holding against the lesson-player design as
much as the compiler.

### Also on the map

- **Gradual Soundness: Lessons from Static Python** (Lu, Greenman, Meyer,
  Viehland, Panse & Krishnamurthi, *Programming* 7:1, 2023). The soundness
  spectrum: **concrete** types (fully sound, but impose nonlocal constraints)
  versus **transient** types (shallow soundness, easier to adopt). Meta's Static
  Python blends them; the Instagram migration gained 3.7% throughput. Relevant
  because our annotations currently parse and mean nothing — "what does an
  annotation *do*" is a question with a real design space behind it.
- **Compiler Error Messages Considered Unhelpful: The Landscape** (Becker et
  al., ITiCSE 2019 working group) — the survey everything above cites.
- On novices and errors generally: roughly **20% report panicking** at the sight
  of an error message, and about half of novices say they do not always
  understand what error messages mean — falling under 30% for advanced students.
  **Type mismatches appear in nearly every study** of what beginners get wrong.

### ⭐ What the research changes about the plan

**The two camps do not talk.** PL research treats type errors as a *localization*
problem (SHErrLoc, counter-factual typing, Nate): find the right expression to
blame. CS-education research treats them as a *communication* problem (Becker,
Santos, Denny): find the right words, and no, LLMs are not the answer.

**Helium is the only project that did both, and it stopped in 2003.**

So the opening is not "nobody has written good type errors." It is that **nobody
has built a compiler where pedagogy is a first-class input to the inference
engine, for a language children actually use.** Every piece has been
independently validated:

- the mechanism — Helium's directives and its generation/solving split;
- the content strategy — authored explanations beat generated ones, measured;
- the implementation route — static, interpretable inference now matches ML;
- the data — localization is learnable from before/after pairs, and we are the
  ones positioned to collect them;
- the framing — errors reported against a taught process, not a failed
  assertion.

Nobody has assembled them. That is the same shape of gap `PRIOR-ART-AGENTIC.md`
found, arrived at from the opposite direction.

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

Recent research:

- Santos & Becker, *Not the Silver Bullet: LLM-enhanced Programming Error
  Messages are Ineffective in Practice*, UKICER 2024 —
  <https://arxiv.org/abs/2409.18661>
- Heeren, Leijen & van IJzendoorn, *Helium, for Learning Haskell*, Haskell
  Workshop 2003 —
  <https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/helium.pdf>
- Heeren, Hage & Swierstra, *Scripting the Type Inference Process*, ICFP 2003 —
  <https://dl.acm.org/doi/10.1145/944705.944707>
- Seidel et al., *Learning to Blame: Localizing Novice Type Errors with
  Data-Driven Diagnosis*, OOPSLA 2017 — <https://arxiv.org/abs/1708.07583>
- Zhang & Myers, *SHErrLoc: A Static Holistic Error Locator*, TOPLAS 39(4) 2017
  — <https://dl.acm.org/doi/10.1145/3121137>
- Aman, Asaduzzaman & Wang, *Typify: A Lightweight Usage-driven Static Analyzer
  for Precise Python Type Inference*, ICPC 2026 —
  <https://arxiv.org/abs/2604.05067>
- *TypyBench: Evaluating LLM Type Inference for Untyped Python Repositories*,
  ICML 2025 — <https://arxiv.org/abs/2507.22086>
- Morazan et al., *A Design Recipe and Recipe-Based Errors for Regular
  Expressions*, TFPiE 2025 (EPTCS 424) — <https://arxiv.org/abs/2508.03639>
- Lu, Greenman, Meyer, Viehland, Panse & Krishnamurthi, *Gradual Soundness:
  Lessons from Static Python*, Programming 7:1, 2023 —
  <https://arxiv.org/abs/2206.13831>
- Becker et al., *Compiler Error Messages Considered Unhelpful: The Landscape*,
  ITiCSE-WGR 2019 — <https://amirkamil.com/papers/iticse19.pdf>

No source code from any of these projects was read. See `NOTICE`.
