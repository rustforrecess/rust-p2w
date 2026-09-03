# The type checker — working design (to argue with)

Status: EXECUTED through phase D (2026-08-23/24, commits acf7353 → the
rule-4 landing); the open pedagogy questions in D6 and tier-2 items remain.
Originally a PROPOSAL; kept as the design of record. This is the design half that
`TYPE_CHECKER_BRIEF.md` deliberately withheld — the brief stays a
requirements/recruitment document, and if a collaborator takes the project
they may replace everything below. Until one does, this is the plan of
record for building it ourselves, staged so every phase lands green.

## What it is

A standalone front-end pass over the shared AST (post-hoist), before either
backend. One pass, four customers:

1. **Students** — type mistakes become compile-time diagnostics with
   derivation-based messages ("`age` is text because line 3 assigned
   `input()`; you used it in arithmetic at line 40"), each with a fading
   ladder. `TYPE_ERROR_MESSAGES.md` is the wording spec; keys land in
   `src/messages.rs` like every other diagnostic.
2. **The native entry** — the checker-first double-compile in
   `compile_to_llvm_ir` is the enforcement mechanism that keeps both
   targets rejecting the same programs. It stays (see phase B); this pass
   is where every NEW check lives so nothing else ever accretes inside a
   backend.
3. **The backends' representation decisions** — `repr.rs` today proves
   Int/Float by local fixpoint; checker facts widen what it can prove
   (unboxing, packed lists, reuse — frontier task 3's continuation).
   Semantic types and representations stay SEPARATE layers: the checker
   says "this is an int", repr says "and it fits unboxed here".
4. **The last divergence rows** — annotation semantics (`x: int = 'no'`:
   GC demotes, native traps) is settled by making it a compile error.

## Decisions already made (do not re-litigate; sources in PRIOR-ART-TYPES)

- **The RPython rule**: at most one type per name at each control-flow
  point. Answers reassignment AND disagreeing branches with one rule.
- **Homogeneous lists; tuples are the heterogeneous few** (Codon's split —
  true about the machine, teachable at once).
- **None mixes with pointer-shaped types** (str, lists, dicts, instances),
  never with int/float/tuple (RPython's representation-derived rule = our
  Pico constraint reached independently).
- **Inference, not required annotations.** `x = 5` keeps working;
  annotations are how you SAY WHAT YOU MEAN, learned later, never a fee.

## Decisions this document proposes (react to these)

**D1 — Engine: hand-written flow-sensitive inference with a provenance
ledger; sequent later, as certificate checker, not as the engine.**
Syntax-directed bidirectional checking, flow-sensitive per the RPython
rule. Every inferred fact records `(line, reason, parent-facts)`; the
message renderer walks that ledger to produce cause-chain text. This gets
Helium's prize (derivations ARE the explanations) without a logic-engine
dependency in the per-keystroke path — one runtime dep stays `ryu`, IDE
latency stays predictable. The sequent integration from the verification
ladder remains: once the pass emits ledgers, sequent can CHECK them as
certificates (proof-carrying analysis) — a later tier, not a blocker.

**D2 — The Ty lattice, tier 1:**
`Int | Float | Bool | Str | NoneT | List(Ty) | Tuple(Vec<Ty>) |
Dict(Ty, Ty) | Set(Ty) | Class(name) | Func(sig) | Dyn`.
`Bool` is distinct but numeric contexts accept it (CPython: bool IS an
int). `Dyn` is the honest top — anything the pass can't prove stays `Dyn`
and compiles exactly as today. **The program ALWAYS compiles in phases
A–B**; `Dyn` is not an error, it is a fact the ladder can explain
(the Julia-@code_warntype pedagogy: "total is dynamic because line 7").

**D3 — Polymorphism, tier 1: infer one signature per function;
a conflicting second callsite is a diagnostic with a ladder** ("f worked
on numbers at line 4; line 9 hands it text — split the function or say
what it takes"). Whole-program callsite duplication (CPA) is cheap for us
and stays on the table for tier 2, decided ALONGSIDE the memory model
(monomorphisation is the unboxed-list mechanism).

**D4 — Staging: four phases, each independently green.**
- **Phase A — advisory.** The pass runs, its findings ride `p2w check`
  as a new JSON section and the IDE's panel. ZERO behavior change:
  RUNTIME_SEMANTICS byte-identical, no gating. The corpus of real
  student programs tells us the false-positive rate before anything blocks.
- **Phase B — single home for checks (SCOPE CORRECTED 2026-08-23).** The
  original premise — that `type_of` was the only front-end check inside
  codegen — was wrong: `generate()` carries ~60 distinct compile-time
  rejections (arity, keyword args, unknown names, format strings, range
  rules, statement placement, super(), modules …); the type checks are
  five of them. Deleting the double-compile after moving five would LOSE
  the other fifty-five natively. And a byte-identical move of even those
  five means replicating codegen's literal-only blindness, while using the
  pass's real inference for errors IS phase C's gating decision. So: the
  double-compile STAYS as the enforcement mechanism (correct by
  construction; cost = one extra kid-sized compile offline). Phase B's
  real content: the pass is the single home for every NEW check and for
  every check phase C promotes; legacy checks migrate opportunistically as
  phase C touches them, never as a relocation campaign.
- **Phase C — the confident rules gate. ✅ DONE, four commits, one per
  rule:** text in arithmetic (rule 1), calling a value (2), single-value
  index/len/loop (3), signature contradictions + text-vs-number comparison
  (4). Each moved its RUNTIME_SEMANTICS rows trap → compile error with
  TYPE_ERROR_MESSAGES' wording and a ladder, read and blessed. The
  Step/Run/native surfaces all gate through one `check::gate()`. What was
  DEFERRED from the original phase-C list, per the D6(b) decision:
  type-changing rebinding, disagreeing branches, heterogeneous lists —
  they RUN today and stay advisory-first.
- **Phase D — annotations mean what they say. ✅ DONE (rule 4).**
  `x: int = 'no'` is a compile error; the demote-vs-trust divergence row
  is gone — BACKEND_DIVERGENCE 3 → 2, the survivors being native
  `dict.get` and the unpack length check, both mechanical.

**D5 — Perf budget: ≤5ms on the largest oracle program, measured in CI.**
The pass runs per keystroke in the browser IDE. Single forward pass with
one fixpoint over loops (join per the RPython rule = fast by
construction); no solver, no SMT (decided — intervals later if wanted).

**D6 — The two open pedagogy questions, DECIDED (2026-08-23):**
(a) a function that sometimes returns nothing → total functions: if any
path returns a value, every path must ("what should it give back
when..."). ✅ IMPLEMENTED as `type.missing-return` (gated): value returns
on some paths with a fall-through or bare `return` elsewhere is a compile
error at the `def`, naming the returning line; the oracle's open question
moved to `must-reject/` as decided. REFINED 2026-08-31: divergence counts
as totality — a `while True:` with no `break` of its own never falls
through, so the retry loop (`while True:` with the `return` inside) and
the early-return-plus-forever-loop shape are accepted (both were live
false positives, found by probing; `must-accept` cases pin them). This is
an internal never/bottom: `definitely_returns` really asks "can control
fall off the end", and a diverging statement answers no. Only the literal
`True` condition counts — computed truthiness stays conservative. A
user-facing `NoReturn` annotation (real Python spelling) is deliberately
NOT added until a lesson needs it (the robot main loop is the candidate). (b) heterogeneous
lists / type-changing rebinding / disagreeing branches → advisory-first
for a full phase; gate only after the false-positive rate on real student
programs is known. Currently silent (`Dyn`) in the pass; the type-churn
lint carries the rebinding case as advice.

## Acceptance (executable, already in the tree)

- `tests/oracle.rs::the_remaining_gap_is_exactly_what_we_think_it_is`
  FAILS and names which must-reject cases moved — that failure is the
  checker's birth certificate. All 8 `ok/` still compile; all 8
  `must-reject/` caught with `TYPE_ERROR_MESSAGES.md` wording.
- BACKEND_DIVERGENCE: the annotation row leaves at Phase D (dict.get and
  unpack are separate mechanical fixes) → 0.
- `native_run.sh`, exec suite, Vm parity: green at every phase boundary.
- Every new diagnostic: a key in `messages.rs`, a ladder in `scaffold()`,
  and a row in the before/after corpus schema (the research asset).

## What this is NOT (tier 1)

No exceptions (types come first precisely because they delete the biggest
except-shaped category). No SMT/bounds proving (intervals are a later,
separate argument). No let-polymorphism machinery (D3's cheap rule until
tier 2). No repr.rs merge (consume facts, don't unify — churn isolation).
