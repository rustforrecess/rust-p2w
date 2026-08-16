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

## Stage 3 spec — decided 2026-08-16, execute mechanically

**Value model.** A string is a JS string stored INTERNALIZED in the
universal value: `(any.convert_extern <externref>)` produces `(ref null
any)` and lives anywhere a value lives. At use sites, externalize back
(`extern.convert_any`) and call the builtin. Type dispatch: there is no
`ref.test` for internalized externs — `is_str(v)` = externalize + call
`wasm:js-string.test`. (Externalized i31s/structs become JS numbers/
opaque objects; `test` correctly answers 0 for them.) Every polymorphic
helper's string arm switches from `ref.test (ref $STR)` to this.

**Encoding note.** $STR was UTF-8 bytes, so len()/indexing counted BYTES
(wrong vs CPython off-ASCII). JS strings are UTF-16 units — for ASCII
identical (suite-neutral), for BMP text actually CLOSER to CPython.
Document, don't fight.

**Host seam changes riding in this stage** (they delete complexity the
cutover would otherwise have to bridge; both hosts are trivial):
- `env.write_str (param externref)` — print's string arm becomes ONE
  call (replaces the per-byte write_char walk).
- `env.f64_str (param f64) (result externref)` — str(float); both hosts
  own the CPython-exact repr already (py_float_repr / IDE formatter).
- `env.read_line (result externref)` — input() stops byte-assembling.
- `s_str (param externref)` — the arg-stack push for DOM/report/
  evidence/fields becomes one externref (subsumes most of stage 4;
  scratch-page batching remains only for gv/gf INBOUND, which also
  simplify: `gv_fetch`/`gf_fetch` RETURN externref directly).

**Literals.** `str_lit(text)` interns text -> one imported global
`(import "'" "<text>" (global $lit_N (ref extern)))`; use =
`(any.convert_extern (global.get $lit_N))`. Gen keeps the intern map;
imports rendered before other imports. WAT-escape the name.

**Lowering table** (helpers keep their names where possible):
| today | becomes |
|---|---|
| $str_eq | `equals` builtin |
| $str_lt | `compare` builtin < 0 |
| $py_add str arm | `concat` |
| $py_len str arm | `length` |
| index s[i] | `substring i i+1` (negative-index fixups stay) |
| slice | `substring` (existing bound clamps stay) |
| $str_contains/$str_find/$str_count/$str_match_at | charCodeAt loops over both strings (keep algorithms, swap array.get_u -> charCodeAt) |
| upper/lower/capitalize/title | charCodeAt + ASCII case math + build via scratch (array (mut i16)) + fromCharCodeArray (matches current ASCII-only behavior) |
| strip family | charCodeAt trims + substring |
| split/join/replace/zfill/pad | same loop rewrites over charCodeAt/concat/substring |
| $str_to_int/$str_to_float | charCodeAt parsing walks (algorithms unchanged) |
| $i32_to_str | digit loop into scratch i16 array + fromCharCodeArray |
| $to_str float arm | env.f64_str |
| print/repr str arms | env.write_str (repr adds quotes via concat with literal `'`) |
| $marshal_str | externalize + `s_str` host call |
| input() | env.read_line |
| dict/set membership | VERIFY: containers are linear-scan over $py_eq (no string hashing) — then equals-builtin via $py_eq is sufficient. If a hash exists anywhere, charCodeAt-loop it |
| $read_char | stays for the char-level API only if something still uses it; else delete |

**One scratch `(array (mut i16))` type** (`$U16S`) added to the type
section for build-a-string loops and intoCharCodeArray staging.

**Order of execution** (each step compiles; suite green only at the end
of the batch — this stage is atomic-ish, budget a full session):
1. Type section: drop nothing yet; add $U16S + the builtin imports
   (emitted only when strings are used — but strings are used by print
   of anything via repr paths, so effectively always; fine).
2. str_lit -> literal-global interning.
3. The is_str dispatch swap in every polymorphic helper.
4. The lowering table, top to bottom.
5. Host seams (exec/common/harness/runner get write_str/f64_str/
   read_line/s_str/gv/gf-externref).
6. Delete $STR array type + dead helpers; suite + probe docs
   byte-identical; e2e 18/18.

## Status

## Status

- [x] Stage 0: survey — 197 eq-universal sites, 1 `ref.eq`, 233 `$STR` touches
- [x] Stage 1: universal value is (ref null any); ref.eq guarded; suite 100% green unchanged
- [x] Stage 2: wasmtime polyfill of wasm:js-string (tests/common/mod.rs) + literal mechanism DECIDED = the quote-module imports (import name IS the literal; V8 gets importedStringConstants at compile); contract test tests/js_string_host.rs green
- [x] Stage 3 EXECUTED: strings are JS strings. ONE DESIGN CHANGE from the
  spec: strings are NOT bare internalized externs — they are wrapped in
  `(type $JSSTR (struct (field (ref extern))))`, because wasmtime 44
  panics its is_subtype libcall on ANY concrete ref.test that sees an
  internalized extern (and i32.or chains are not short-circuit, so
  ordering alone cannot protect them). The wrapper makes is_str one
  native ref.test (faster than the builtin `test` host call) and kills
  the panic class by construction. Encoding shift verified: hosts now
  assemble UTF-16 units (surrogate pairs included — café 🦀 prints).
  Suite 100% green, RUNTIME_SEMANTICS byte-identical, $STR deleted.
- [x] Stage 4: IDE runner passes the js-string compile options via Reflect (acornstem-ide); full e2e 18/18 on the JS-strings build. gv/gf inbound stays on the scratch protocol (works; externref returns = optional later).
- [x] Stage 5: real-Chrome bench (acornstem-ide bench_js_strings, data-URL page, median of 7): strbuild pre-cutover 279.2ms -> JS strings 73.3ms (~3.8x). Campaign COMPLETE.
