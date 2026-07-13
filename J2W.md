# J2W — Java → WASM-GC scoping spec (v0, 2026-07)

**Status: SCOPED, NOT SCHEDULED.** Written while the thinking is fresh; build
only when Java tutoring firms up (likely but not imminent). Nothing in the
platform blocks on this.

The one-liner: a second teaching compiler beside rust-p2w — the AP CS A / CS1
Java subset compiled to WASM-GC — plugging into the SAME IDE, designer,
runner, activity interface, lesson format, and evidence pipeline. Java code
becomes a first-class citizen of the substrate, not a new platform.

## Why hand-built (not TeaVM / CheerpJ / J2CL)

Same argument that produced rust-p2w instead of Pyodide: those are runtimes,
not substrates. Owning the compiler is what makes the moat possible —
teaching lints, ErrorKind classification, concept evidence, a steppable AST,
scaffold ladders. That logic is language-independent.

## What transfers unchanged (~70% of the platform)

- IDE shell, designer/formir, templates, lesson player, lesson.zip
  (a page's activity can be `01.java`), xAPI emitter, e2e harness, WCAG work.
- **The host-capability seam — unchanged.** A provided static API class maps
  1:1 onto the existing `env.*` imports:

  ```java
  Acorn.setText("#msg", "Correct!");     // -> env.dom_set_text
  Acorn.on("#go", "click", Main::check); // -> env.dom_on
  Acorn.report(100, "right answer");     // -> env.report
  Acorn.setPosition("#box", 120, 80);    // -> env.set_position
  ```

  Same runner, same teardown, same xAPI, same lesson player. The activity
  interface (and its PXC alignment) is language-agnostic by construction.
- Frameworks-as-patterns: ErrorKind (javac's "cannot find symbol" IS our
  Name Error), LintKind + scaffolds, the concept-evidence vocabulary —
  which becomes CROSS-LANGUAGE (one student's Python and Java streams land
  in one assessment model; the "shared semantics layer, not shared parser"
  bet from the grammar-engine decision, cashed).

## What is genuinely new (a second compiler, comparable magnitude)

Lexer, parser, codegen, runtime, debugger (tree-walk Vm), blocks
generator/decompiler, lint analyses. Do NOT attempt to share the Python AST —
share the OUTPUT contracts (WAT emit helpers, host import names, error/lint
shapes) and, only if it stays natural, the `emit` module.

## Why Java is friendlier terrain than Python was

- **WASM-GC was designed for this shape**: GC structs + vtables ARE
  class layout + virtual dispatch (J2CL, Kotlin/Wasm are existence proofs).
- **Static typing deletes p2w's hardest machinery**: no runtime dispatch
  (`py_add`), no i31 boxing ambiguity — `int + int` is `i32.add`; types are
  known at every site. In exchange: class metadata, vtables, overload
  resolution, and a String/collections runtime — bounded, well-trodden.

## The subset (AP CS A-shaped, phased)

**Phase 0 — procedural Java (the tutoring fast path; first half of any
intro course):**
`class Main { public static void main(String[] args) }`, `int double boolean
String char`, arithmetic/comparison/logical ops, `if/else`, `for`, `while`,
arrays (`int[]`, `new int[n]`, `.length`), static methods + overloads,
`System.out.println/print`, `Math.*` (whitelist), String basics (`length,
charAt, substring, equals, indexOf, +` concat), casts, `final`.

**Phase 1 — objects:** classes with fields/constructors/instance methods,
`this`, single inheritance + `super`, method overriding (vtables), `static`
vs instance, encapsulation modifiers (parsed; enforcement = lint at most),
`toString`, `null` (with a friendly NullPointer story — trap + message).

**Phase 2 — the CS1 collections + interfaces:** `ArrayList<T>` and
`HashMap<K,V>` as ERASED whitelisted generics (no user generics), `interface`
+ `implements` (itables or single-interface simplification), enhanced-for,
`equals/compareTo` conventions.

**Explicit non-goals (defer indefinitely):** exceptions/try-catch (wasm-EH
exists but is a project; teaching subset traps with friendly messages),
user-defined generics, threads, lambdas/streams, reflection, packages beyond
one file, the wider stdlib. Add only when a course actually requires them.

## Lowering sketch (WASM-GC)

- Class -> `(struct $C (field $vt (ref $C_vt)) fields...)`; vtable struct of
  funcrefs per class; `new` = struct.new + vtable global; virtual call =
  struct.get vt -> call_ref. `super` calls = direct call.
- Primitives: `int`->i32, `double`->f64, `boolean`->i32, `char`->i32 (UTF-16
  unit). Arrays -> `(array (mut T))` with bounds-trap messages.
- String: immutable `(array i8)` UTF-8 internally with charAt/substring
  documented as O(n) (teaching honesty) — or `(array i16)` UTF-16 for
  spec-faithful charAt; DECIDE AT BUILD TIME (leaning UTF-16: matches Java
  semantics kids will be taught, and charAt is the intro workhorse).
- Overloads resolved at compile time (static types make this a table).
- ArrayList/HashMap: runtime structs over GC arrays; erased element type =
  `(ref null eq)` + compile-time checked inserts (whitelist keeps this sound).
- Host calls: `Acorn.*` statics compile straight to the existing `env.*`
  imports; string args ride the existing marshalling seam (LIFO pop-N).

## IDE integration

- Language switch on the editor (per-file: .py or .java); ONE toolbox, blocks
  stay concept-level (`when #go clicked`, `set position`) with per-language
  generators/decompilers — the blocks are the cross-language layer made
  visible.
- Error headlines: reuse ErrorKind; add Java-vocabulary messages ("cannot
  find symbol 'answr' — did you mean 'answer'?" = the same did-you-mean
  machinery).
- Debugger: a Java Vm mirroring the Python one (statements are simpler; the
  object model is the new part).

## Effort (honest gut, in sessions like current ones)

- Phase 0 runnable (console + host calls + arrays): a handful of sessions.
- Tutoring-ready (Phase 0–1 in the IDE, error headlines, templates): low
  weeks.
- Parity with today's p2w polish (debugger, blocks fidelity, lint family,
  e2e nets): the long tail, months of accumulation — same curve p2w rode.

## Build triggers / first moves when green-lit

1. Confirm the course's actual syllabus subset (AP CS A vs university CS1).
2. `rust-j2w` crate; steal the emit/WAT-builder + test harness patterns.
3. Phase 0 vertical slice: `Hello world` -> `Acorn.setText` quiz clone of the
   Python template — the moment BOTH languages run the same lesson page, the
   cross-language thesis is demonstrated.
