# Prior art — the compiler as an agentic inner loop

> Surveyed 2026-08-03, to answer two questions before building the headless
> harness: *is anyone already doing this*, and *what is actually different
> here*. Short answers: the mechanism is mature and well-published; the
> combination is not.
>
> **Evidence discipline.** Everything below was located by search and read at
> the level of titles, abstracts and project documentation. **No source code
> of any listed project was read**, matching the clean-room posture in
> [`NOTICE`](NOTICE). Where a claim comes from a project's own documentation
> it is quoted as such. Nothing here is a source-derived design.

---

## 1. Compiler-in-the-loop for LLM agents — established, not novel

Feeding structured compiler output back to a code-generating model is a mature
practice with an active research literature. Treat it as a solved technique to
adopt, not an idea to defend.

**In production tooling already:**

- `rustc` and `clippy` emit machine-readable diagnostics via
  `--message-format=json`.
- **LSP** is structured diagnostics by design — spans, severities, codes — and
  every mainstream language has a server.
- Agent frameworks routinely consume build and test output as their iteration
  signal.

**In the literature (found by search; abstracts only):**

- Retrieval-augmented, multi-tool repair loops combining compiler diagnostics
  with static analysis and symbolic execution.
- Reinforcement learning from compiler *and language-server* feedback.
- Agentic loops where a compiler judges proposed optimisations for legality and
  effect.
- Compiler-guided inference-time adaptation for **Idris** — a dependently typed
  language, where the compiler rejects far more and therefore says far more.

**The finding that matters for our thesis:** in strict, strongly-typed settings,
structured compiler feedback is reported as a stronger learning signal than
natural-language prompting alone. **Type strength converts into agent
accuracy.** That is an argument *for* a restrictive subset, published by people
with no stake in ours.

---

## 2. Restricting Python — the prior art, and what it admits about itself

This is where the survey earns its keep, because the incumbents document their
own limits.

**RestrictedPython** (Zope Foundation, maintained for two decades) defines a
subset of Python for executing untrusted code. Its documentation states plainly
that it is **"not a sandbox system or a secured environment"** — it helps
*define* a trusted environment rather than guarantee one. Commentary around it
notes that maintaining a fully secure subset is extremely hard because Python's
dynamism keeps producing new vectors: `__import__` abuse, deserialization,
attribute traversal.

**AgentRun** targets exactly our use case — running LLM-generated Python safely
— and does it with **RestrictedPython *plus* Docker**. The layering is the
significant part: the subset is not trusted on its own, so a container goes
underneath it.

**sandboxed-python (FPy)** advertises "a SAFE and FINITE subset of Python,"
single-file and stdlib-only, aimed at LLM tool calls and plugin systems.
Supports assignment, conditionals, try/except, basic containers and a
allow-listed set of builtins.

**What all three share: they restrict CPython at runtime.** They filter an
interpreter that is already capable of everything, which is why the approach is
adversarial maintenance rather than a settled guarantee, and why the most
mature of them still declines to claim security after twenty years.

---

## 3. Compiled Python subsets — real, but aimed elsewhere

**RPython** (PyPy's statically-compilable subset), **Shed Skin** (implicitly
typed subset → C++), **Codon**, **LPython**, **Mojo**.

These compile Python or Python-like source and several are excellent. **None is
safety-motivated, none is agent-facing, and none emits anything but errors and
performance data.** They exist to make Python fast.

---

## 4. What is actually different here

Three things, and only the third is unprecedented.

**(a) Compile, don't interpret — so there is nothing to escape into.**
We parse a subset and emit WASM or LLVM. There is no CPython beneath the
program. `__subclasses__` is not blocked; it does not exist, because it was
never implemented. The attack surface is not *reduced*, it is *absent*.

RestrictedPython's own caveat is the strongest available argument for this
distinction — an incumbent with twenty years of effort documenting that
filtering a dynamic interpreter does not fully work.

**(b) The subset is the security boundary, not a wall around one.**
The industry pattern is containment: E2B, Modal, Daytona, WASM jails,
containers — a general language plus a perimeter. Restricting *expressiveness*
so the dangerous operation is ungrammatical is a different move, and the
combination of that with an agent loop appears unoccupied.

**(c) Pedagogical output — no prior art found.**
The literature emits errors, security findings and performance numbers. Nothing
found emits **teaching lints** ("this is reaching for iteration and the bound is
wrong"), **concept tags**, or **assessment evidence**. A compiler whose output
feeds a curriculum rather than a build is the genuinely novel part, and it is
the part with no competitor to benchmark against.

---

## 5. Practical consequences for the harness

**Do not invent the diagnostic schema.** `rustc`'s JSON diagnostics and LSP's
`Diagnostic` are the de facto shapes; agents and editors already parse them.
Adopt one and carry the novel fields — `concepts`, `evidence` — as extensions.
Inventing a format costs tooling and buys nothing.

**Do not add serde to the library.** `rust-p2w` has exactly one runtime
dependency (`ryu`). The precedent is `sequent`'s `json.rs`: dependency-free
hand-written emission with the contract pinned by native tests. Keep the
library clean; put the serialisation in the binary.

**`wasmtime` is already a dev-dependency** (44.x, with `cranelift`, `gc`,
`gc-null`, `runtime`). The runner exists for tests; exposing it behind a
feature gate for a `run` subcommand is the same pattern `acornstem-ide` uses
for its `e2e` feature.

**The non-AI payoff lands first.** Compile-checking every code example in every
lesson on commit needs none of the agentic machinery and is worth building for
its own sake — a broken snippet in a lesson is currently discovered by a child.

---

## 6. What would change this assessment

- A compiled — not interpreted — Python subset appearing with an agent harness.
- Any compiler emitting concept-level or pedagogical diagnostics.
- Evidence that runtime-restricted Python subsets have become genuinely secure
  rather than defence-in-depth components.

Re-check before publishing any claim of novelty; this survey is a snapshot and
the area is moving quickly.
