# Rich output — the `emit_html` / `_repr_html_` channel

**Status: Layer 1 shipped (Jun 2026).** A way for a kid's program to produce
*rendered HTML* inline in the IDE, not just text — for model/structure diagrams,
tables, charts, and (the headline) memory/data-flow visualizations. Inspired by
scikit-learn's `_repr_html_` (concept only, no code — see `NOTICE`).

## The one hard constraint that shapes everything

The IDE injects program output into the DOM via `innerHTML`
(`dangerous_inner_html`), and **`innerHTML` never executes inline `<script>`**
(HTML spec). So **rich output must be no-JS**: pure HTML + CSS. This isn't a
limitation to work around — it's the design. scikit-learn's diagrams already
prove the model: collapsible/expandable structure built with **CSS checkbox
tricks**, zero JavaScript. Anything JS-dependent silently does nothing through
our path, so we don't pretend to support it.

(For genuinely interactive output, the separate **interactive-web** path exists —
`run_interactive` + the capability palette + the callback seam — see
`INTERACTIVE_WEB.md`. Rich output is the *non-interactive, render-only* channel.)

## The seam (Layer 1 — shipped)

One builtin, lowering to one host import, reusing the existing string
marshalling:

- `emit_html(html)` — Python builtin (io category, in the builtin registry).
  Lowers to `env.emit_html` after marshalling its string argument with the
  shared `s_begin`/`s_byte`/`s_push` + `$marshal_str` machinery (the same
  forward-marshalling seam as the DOM string ops, `report`, `evidence`). Gated on
  `uses_emit_html` so a program that doesn't call it emits none of the plumbing.
- Host side (`acornstem-ide/src/runner.rs`, `run_wat`): the marshalled string is
  appended to an HTML buffer; `run_wat` returns `RunOutput { text, html }`. The
  IDE renders `html` (when non-empty) into `#pe-rich-output` via
  `dangerous_inner_html`, below the text output.
- The Vm (Debug mode) no-ops `emit_html` (no host), so stepping still traces.
- Verified: rust-p2w `emit_html_marshals_to_the_host`; IDE e2e section 2h drives
  a real `emit_html("<b>bold</b>")` and asserts it renders.

## Layer 2 — `_repr_html_` protocol (shipped)

Mirror Jupyter's thin protocol, made explicit (we have no auto-display of the last
expression): **`show(x)`** renders `x._repr_html_()` as HTML when the class defines
it, and falls back to printing `x` as text otherwise. We do **not** run
scikit-learn (C extensions, far outside the subset) — we provide the *protocol*,
and our own lightweight estimator-/structure-like objects author `_repr_html_` in
p2w itself.

- Prerequisite verified: the `ClassDef` subset supports a method returning a
  string (`repr_html_method_returns_a_string`).
- `show(x)` is an io builtin lowering to a `$show` helper: if `x` is an instance
  whose class has `_repr_html_`, it runtime-dispatches the method (mirroring
  `$object_to_str`'s `__str__` lookup via `$class_lookup_method`) and emits the
  result through the `emit_html` channel; otherwise it prints `x` as text. No new
  IDE wiring — it reuses the emit_html (rich) and write_char (text) paths.
- The Vm no-ops `show` (Debug still traces). Tests:
  `show_dispatches_repr_html_or_text` (rust-p2w) + IDE e2e 2i (rich render +
  text fallback in a real browser).

## Layer 3 — the visualizations (the actual payoff)

Three complementary reprs over one AST/graph, rendered as no-JS HTML/CSS:

1. **Structural** — scikit-learn-style nested boxes (what wraps what): pipelines,
   composite objects, data structures.
2. **Reuse / lifetime overlay** — *our differentiator.* Annotate where values are
   born, last used, dropped, and **reused in place** (`rc == 1`). For a pipeline:
   which steps run in-place vs copy. This is the same overlay engine as the
   `del`-as-reuse-hint idea (see `MEMORY_MANAGEMENT.md`) — the two project-idea
   threads converge on one primitive. Novel for K-12: memory made visible.
3. **Causal / data-flow** (CausalCode layer) — which step produces which
   intermediate, where shapes change, and the ZPD concepts at each node (linked
   to the knowledge graph).

These layer on the Blockly blocks (already an AST view) and reuse the debugger
seam (`DEBUGGER_ARCHITECTURE.md`).

## Trust note

Injecting program-emitted HTML is a trust surface, but it's the kid's own program
in their own session — the same boundary as the page designer — and the no-JS
constraint (`innerHTML` drops scripts) shrinks it further.
