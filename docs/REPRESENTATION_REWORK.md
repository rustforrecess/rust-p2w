# Strings become JS strings — the staged representation rework

> **Scope: the GC backend's `$STR (array (mut i8))` becomes an externref JS
> string via the `wasm:js-string` builtins.** Zero-copy host calls, engine-rope
> concat, and the marshalling scratch-page becomes unnecessary for strings.
> **No semantic change**: the existing suite (191 exec + probes + e2e) is the
> oracle and must pass UNCHANGED at every stage.
>
> **s64 is deliberately NOT here.** Int width is a *language* change chained to
> the native value model by "both targets or neither" — its own campaign, with
> an llvm.rs/runtime design first. Bundling it here would couple a
> semantics-preserving refactor to a semantics-changing one.

## Why (measured)

- Strings crossed the host boundary per byte ×3 hops until the scratch-page
  batching (bdd038a: 9.8× fewer crossings, 5.8× under glue weight). Builtins
  delete the copy entirely: the externref IS the JS string.
- `wasm:js-string` shipped in Chrome and Firefox; the imports are specified as
  polyfillable, so wasmtime tests provide host implementations — the
  differential rig keeps working.

## The type-hierarchy fact that shapes everything

`externref` is NOT under `anyref` — but `any.convert_extern` /
`extern.convert_any` bridge the two hierarchies, and internalized externs live
under `any`, **not under `eq`**. Today's universal value is `(ref null eq)`
(197 sites). Therefore:

**Stage 1 — universal value migrates `eq` → `any`.** Purely mechanical
(i31/struct/array all sit under `any`), semantics-preserving, suite-verified.
The single `ref.eq` identity site ($py_eq's object-identity arm) gets guarded:
test both sides `(ref eq)` before casting, so a future non-eq value compares
unequal instead of trapping.

## Stages

| # | change | verification |
|---|---|---|
| 1 | universal `(ref null eq)` → `(ref null any)`; guard the ref.eq site | suite unchanged, 100% green |
| 2 | wasmtime harness implements `wasm:js-string` (externref strings) + pick the literal mechanism (`"'"`-module imports vs init-time `fromCharCodeArray`; must be expressible in both V8 and the wasmtime polyfill) | harness unit test: builtins round-trip |
| 3 | `$STR` → externref: literals, concat, equals/compare, length, charCodeAt indexing, substring slicing via builtins; ops with NO builtin (upper/lower/strip/split/replace/format, dict-key hashing) enumerate from the 233 `$STR` sites and lower via `intoCharCodeArray` scratch round-trips | suite unchanged; probe docs byte-identical |
| 4 | marshalling: strings pass as externref straight through host calls (runner.rs + exec host signature change); scratch batching stays for `get_value`-style inbound | e2e 18/18; marshal bench |
| 5 | bench + record: strbuild, marshal suite, IDE memory profile | numbers into the memory entry |

## Status

- [x] Stage 0: survey — 197 eq-universal sites, 1 `ref.eq`, 233 `$STR` touches
- [x] Stage 1: universal value is (ref null any); ref.eq guarded; suite 100% green unchanged
- [ ] Stage 2–5
