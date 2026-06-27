# Interactive web — Python as the kid's JavaScript

> Build active web pages in our compiled Python: a page you add HTML elements and
> SVG shapes to, each **wired to run code on events** — from a beep to anything
> WASM can do. The browser stays the host; **HTML/SVG is the layout, Python is the
> behavior** (the "code-behind"). Compiled (~26 KB), not an interpreter — the same
> compile-don't-interpret philosophy as the rest of the project.

## The architecture: a capability palette + one callback seam
Everything — widgets, clickable SVG, audio — is the *same* mechanism, so new
capabilities are "a few more host imports," never new architecture:

```
kid Python  ──registers──>  handler functions
     │ calls
     ▼
capability palette (env host imports):  set_attr · set_text · play_sound · beep · …
     ▲
     │ events (click, timer) ──> __dispatch(id) ──> runs the Python handler
browser (HTML / SVG / Audio), the WASM instance kept ALIVE after _start
```

- **Capabilities = host imports**, exactly like the existing `env.write_char`
  (`codegen.rs` declares them; the IDE runner provides them).
- **Events = one callback seam**: a handler `def` is referenced by name as a
  value (→ a small integer **dispatch id**); the runner keeps the instance alive
  and calls the exported **`__dispatch(id)`** when an event fires.

## The two genuinely hard parts
1. **The live-instance callback path** (Layer 1–2). After `_start` registers
   handlers, the instance must persist and be re-entered via `__dispatch`. Today's
   runner instantiates → calls `_start` → *drops* everything; that must change.
2. **WASM-GC ↔ JS strings** (Layer 3). WASM-GC strings aren't linear-memory
   pointers (that's why output goes through `write_char` one byte at a time).
   Passing real selectors/colors/sound-names out to JS needs the same char-by-char
   marshalling. Layers 1–2 deliberately avoid strings (no-arg effect imports).

## Layered roadmap (incremental — not all at once)
- **Layer 1 — callback seam (codegen). DONE.** `on_click`/`flash`/`beep`
  builtins; a zero-arg `def` used as a value compiles to its dispatch id (boxed);
  an exported `__dispatch(id)` runs the matching handler; the `env` imports +
  `__dispatch` are gated on use (like `uses_input`), so ordinary programs are
  unchanged and the host stays minimal. Verified: `interactive_web_seam_compiles`
  (valid WASM + the right imports/export/id).
- **Layer 2 — IDE runner.** Keep the instance + import closures alive; render a
  preview (an `<svg>` with a `#box`); implement `on_click` (store id + add a JS
  click listener that calls `__dispatch`), `flash` (toggle the box fill), `beep`
  (a Web Audio tone). End-to-end "click the box → Python runs → it flashes + beeps";
  click-confirmed in a browser (`dx serve` / the thirtyfour e2e harness).
- **Layer 3 — strings. DONE.** A char-by-char marshalling protocol
  (`s_begin`/`s_byte`/`s_push` → a JS-side arg stack; `$marshal_str` in codegen)
  lets strings cross the WASM-GC↔JS boundary, unlocking `set_attr(sel, name, val)`,
  `set_text(sel, text)`, `play_sound(name)`, and the general `on(sel, event,
  handler)`. Gated by `uses_dom_str` so non-string programs stay minimal. Codegen
  verified (`interactive_web_string_ops_compile`); the runner implements the
  protocol + ops (browser-confirmed). Still reverse-only: reading a value *back*
  from JS (`input.value` → a WASM string) is the next sub-step (reverse
  marshalling). Kids can now drive arbitrary elements by selector.
- **Layer 4 — more capabilities.** input values, keyboard, timers / animation
  frame; a small curated kid API + starter templates.
- **Layer 5 (optional).** an HTML/form builder that *emits markup* (a projection,
  like blocks → Python). HTML is the layout IR, so any HTML tool interoperates and
  there's no bespoke designer to build or round-trip.

## Why this shape
- **HTML/SVG is the layout IR** — no invented format, no canvas to keep in sync
  with code, the browser renders for free. Matches the project's canonical-source
  + projections philosophy.
- **SVG (not `<canvas>`) for clickable graphics** — each shape is a DOM node with
  an id, so it's individually clickable/styleable through the *same* seam; canvas
  is immediate-mode pixels with no per-element identity.
- **Differentiated:** lean *compiled* Python driving the DOM, vs the multi-MB
  Pyodide/PyScript interpreters; real Python that transfers, vs Scratch.
- **Feeds the assessment layer:** the elements a kid wires and the events they
  handle are rich interaction evidence.

## Implementation hooks (where each piece lives)
- Host imports + `__dispatch` + the builtins + function-as-id: `src/codegen.rs`
  (`uses_dom` gate; `handler_defs`/`handler_id`; the `on_click`/`flash`/`beep`
  arms; `Name` emission for function-as-value).
- The runner that fulfils the imports + keeps the instance alive: `acornstem-ide`
  `src/runner.rs` (`run_wat` is the starting point) + a preview pane in `main.rs`.
