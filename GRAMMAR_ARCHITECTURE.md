# Grammar architecture — why a hand-written parser, not a grammar engine

> Decision record, 2026-06-20. Companion to `CLASSES_DESIGN.md`.

## The question

Should AcornSTEM's Python front-end (hand-rolled lexer → Pratt/recursive-descent
parser → codegen, plus `to_blocks` and the blocks→Python generator) be replaced
by a **bidirectional grammar engine** in the style of
[Grammatical Framework](https://www.grammaticalframework.org/) (GF) — an
abstract syntax + per-surface concrete syntaxes + a "every rule works both
directions" contract? The sibling project **loom** (CS Club) already is GF-in-
Rust for natural language, which makes a shared engine tempting — possibly one
that "loads different grammars" for English, Japanese, Python, and feeds a
shared relational-semantics layer.

## The decision

**Keep the hand-written Python front-end. Do not build or adopt a grammar engine
for the teaching front-end. Unify across domains at the _semantic_ layer, not the
_parser_ layer.**

## Why

1. **Hand-written parsers win for error UX.** GCC, Clang, rustc, and CPython all
   hand-roll their parsers despite 50 years of parser generators — because that's
   how you get good error *recovery*, "did you mean", partial results, and IDE
   latency. AcornSTEM's whole point is "kids must not be frustrated", and the
   error work shipped in June 2026 (per-block recovery, missing-colon coaching,
   `pint`→`print`, partial-block rendering) is only easy *because we own the
   parser*. A generic engine optimizes for "describe the grammar concisely" and
   hands back generic errors — it would regress the dimension we care most about.

2. **A grammar engine needs many grammars of one modality to pay off.**
   tree-sitter amortizes across ~hundreds of languages; GF across many natural
   languages. The teaching tool has **one** language (Python) today, with at most
   a *conditional single swap* to **Mojo** later. Mojo is a Python *superset*, so
   the hand-written parser is a head start — we'd extend it, not re-describe a
   grammar and lose the error UX. One-or-maybe-two languages is not a family;
   there's no engine payoff.

3. **GF gives _semantic_ round-trip, not _textual_.** A bidirectional grammar
   guarantees `text → AST → text` yields the same *AST*, but it drops comments
   and formatting (one AST has many surface forms). That's why rustfmt and
   Prettier are deliberately one-directional. Preserving the student's exact text
   needs a concrete-syntax-tree-with-trivia (Roslyn / rust-analyzer's Rowan) — a
   *different* architecture an engine wouldn't provide. AcornSTEM already chose
   **text-canonical / blocks-derived** to sidestep this.

4. **Two different "unifications" were being conflated.**
   - *Unify the parsing engine* (one engine parses English, Japanese, Python) —
     weak for code, for the reasons above.
   - *Unify the semantic representation* (all surfaces decompose into a shared
     relational model: predicate-argument structure → relations → a knowledge
     graph) — strong and genuinely modality-independent. "X causes Y" / "A is
     defined as B" / "this calls that" don't care whether they came from a
     Japanese sentence or a Python function.

   Only the second generalizes truthfully, and it does **not** require one parser.

## The endorsed long-term shape

```
English  ─┐                         ┌── loom GF multilingual NL engine ──┐
Japanese ─┘ (one abstract syntax)   │                                    │
                                    ▼                                    ▼
Python ── AcornSTEM hand-written parser ──► (Python→relations extractor) ──►  SHARED
                                                                              relational-
                                                                              semantics / KG
                                                                              + ECD assessment
                                                                              (the contract)
```

- **English + Japanese** run on loom's GF-style multilingual engine — GF's home
  turf (one abstract syntax, SVO/SOV concrete syntaxes, particles, morphology).
- **Python** keeps its hand-written parser.
- All feed a **shared relational-semantics / knowledge-graph + Evidence-Centered-
  Design assessment contract**. That shared *contract* is the interface (the
  neurosymbolic-stack principle), not a merged engine. "Understands the relation
  semantics" lives here.
- Note: **Python→relations (for the KG/assessment) is a separate artifact from
  Python→blocks→text (the IDE round-trip).** Different jobs, different
  requirements — don't let the semantic vision justify rewriting the editor's
  parser.

## Future pillar: natural-language → code ("interpretable vibe coding")

If students will eventually write English/Japanese describing intent and get
Python back — *interpretably*, not via an opaque model — does that change the
decision? No, but it sharpens where each piece belongs.

**Key distinction.** NL↔NL (English↔Japanese) is *translation*: both realize the
same abstract syntax, so GF linearization is the right mechanism. **NL→code is
NOT that.** A sentence and a program that implements it do **not** share an
abstract syntax — the program has loops, helpers, and edge cases the sentence
never mentions. Mapping intent→program is **program synthesis / semantic
parsing**, not grammar linearization. So a single grammar engine still isn't the
mechanism for NL→code.

**How the chosen architecture supports it (cleanly):**

```
English/Japanese ──► loom grammar (incl. CONTROLLED-NL) ──► intent / relations
                                                                  │
                                                                  ▼  synthesis
                                              Python AST  ◄────────┘  (rules for a
                                                  │                    controlled K-12
                          existing codegen ◄──────┼──────► existing     subset; optionally
                          (→ WASM)                 │        to_blocks    LLM-assisted)
                                                   ▼        (→ blocks)
```

- The hand-written **Python AST is the hub and the synthesis _target_**, and the
  existing back-end (`codegen` → WASM, `to_blocks` → blocks) is *reused for
  free*. Keeping the parser hand-written doesn't block NL→code — it *provides*
  the target and the back half.
- The NL side (NL→intent), multilingual and possibly **controlled-NL→intent**,
  is exactly loom/GF's wheelhouse — reinforcing "grammar engine for NL, not for
  the Python IDE."
- **Interpretability is the differentiator and is pedagogically essential.**
  "Interpretable vibe coding" must ground every generated code span back to the
  text span and the rule/derivation that produced it (loom already has
  `derivation.rs` / `observer.rs` / trace for exactly this). A kid has to *see
  why* to learn — opaque LLM output is the anti-goal. So this is neurosymbolic
  (symbolic intent + grounding, optionally LLM-assisted but always traced), not a
  grammar engine and not a bare LLM.
- For K-12, the tractable + valuable starting point is a **controlled-NL subset**
  (a small, teachable English/Japanese that maps to code with the mapping shown).
  It teaches precise specification — computational thinking — while staying
  interpretable. Open-ended NL is a later, LLM-grounded extension.

Net: NL→code **reinforces** this document's conclusion — unify at the
relational-semantics layer, run NL on loom's engine, keep Python's parser
hand-written (now also serving as the synthesis target), and deliver
interpretability via derivation traces. It is a long-horizon pillar the current
architecture already accommodates; nothing about the Python front-end needs to
change to keep the door open.

## Revisit trigger

Reconsider a grammar-driven engine **only if a genuine family of teaching
languages** (several, simultaneously, same modality) ever appears. A single
Python→Mojo swap does not trigger it.

## What we did instead (the bounded, always-worth-it parts)

- Kept per-domain front-ends focused.
- The bidirectional *contract* is captured as **tests** (round-trip and the live
  e2e), which catch drift between `to_blocks` and the blocks→Python generator —
  the small real risk — without an engine.
