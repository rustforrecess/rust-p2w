# Why we build the compiler and only *mine* the alternatives

*A decision record. Short version: the requirements that make this good for kids
form a specific **conjunction** that no existing project satisfies. Every
alternative is excellent at its own goal — but adopting any one forces us to give
up a requirement that is load-bearing for a K‑12 learning tool. So we reuse ideas
and implementations (everything here is MIT), while owning the compiler. Not
because of Not‑Invented‑Here — because the intersection is genuinely unoccupied,
which is also the moat.*

---

## The thing that keeps happening

Every time we look at joining an existing project, the answer comes back:
*"Yes — as long as you give up X."* And X is always one of the three things that
make the tool good for a twelve‑year‑old. That is not stubbornness on our part and
it is not a flaw in the other project. It is the signal that our requirement set is
an unusual intersection. This document names the requirements, then shows, project
by project, exactly which one each alternative asks us to surrender.

## What "what we want" actually is — the five non‑negotiables

1. **Runs in a locked‑down school browser, client‑side.** A student on a
   Chromebook opens a page and hits **Run** — no install, no server, no toolchain,
   and a payload small enough for school Wi‑Fi. This is the anti‑Pyodide
   requirement.
2. **It's real Python, so learning transfers.** What a kid learns this year has to
   carry to the Python they meet next year. That means standard syntax and
   CPython‑exact behaviour — not a look‑alike variant.
3. **It's glass‑box / assessable.** The system must *see what the student did* —
   which concept they used — to give scaffolded hints, emit evidence, sync
   blocks ⇄ code, and step‑debug. This requires owning the AST and the compiler.
   A black‑box interpreter structurally cannot do it. **This is the education half,
   and the moat.**
4. **A path to the $7 robot.** The same source should eventually reach a
   microcontroller (Raspberry Pi Pico 2 W: ~520 KB RAM, no OS) — which needs a
   controllable, no‑GC memory model, not a heap‑heavy interpreter.
5. **Approachable for a beginner.** Friendly, K‑12‑grade error messages; a
   constrained subset; no mandatory ceremony before the first `print`.

The load‑bearing insight: **#1 (in‑browser, tiny) + #2 (real Python) + #3
(glass‑box) is the unoccupied corner.** Speed‑focused *variants* give up #2.
*Interpreters* give up #1 and #3. Nobody sits in all three at once — so to have all
three, we own the compiler.

*(Two further non‑negotiables — **#6 client‑side *compilation*** and **#7 custom
emit targets** — were made explicit later; they're what make rust‑p2w
*specifically* non‑substitutable, and they kill SPy and MicroPython respectively.
See the [Jul 17 addendum](#session-addendum--jul-17-2026-micropython-deepdive-pxc-the-ruff-frontend-and-the-hire).)*

## The alternatives, and the exact X each one costs

| Project | Great at | The X it makes you give up |
|---|---|---|
| **SPy** (spylang) | Fast **compiled** Python‑variant; the blue/red phase split | **#2 transfer** (it's a *variant* — `i32`/`@blue`/`dynamic`; the `int→i32` alias softens this) **and #1 browser** (compiler is Python **and emits C** → in a browser that's Pyodide + interpreter, or a toolchain) |
| **Pyodide** | Max capability — real CPython, numpy/sklearn, real error messages | **#1 tiny payload** (~6–10 MB) and, in practice, **#3** (glass‑box is *possible* via `ast`/`settrace`, but it's heavy and you don't own the pipeline) |
| **MicroPython** | Fits the Pico; huge driver/`machine`/PIO ecosystem | **#3 glass‑box** (black‑box interpreter — no AST, no lint/evidence/scaffold hooks). Great **robot** runtime; never the browser |
| **RustPython** | CPython‑exact *behaviour*, in Rust; healthy MIT community | Browser = **interpreter‑in‑a‑tab** (gives up #1 payload/speed **and** #3 glass‑box); Pico = **can't fit** (`std`, GC, size → gives up #4) |
| **Skulpt** | In‑browser JS Python, historically | **#2** (dated, partial) and **#3**; not a serious base today |
| **ruff / rustpython‑parser / ‑unparser** | *Parts* (parse / unparse), not runtimes | **Stance updated Jul 17 — see the addendum.** ruff's `ruff_python_parser` / `ruff_python_ast` are now the **recommended front‑end to *adopt via an adapter*** (Rust, full‑Python AST + spans, MIT) — keeping our own AST + compiler. The old "heavy → **#1**" worry is now a **measured gate** (wasm‑size delta); the "unstable internal crate" risk is mitigated by **git‑pin + adapter shield**. rustpython‑parser is **frozen/superseded**; the unparser is a 2‑star project on that dead AST. Net: **adopt ruff's *parser part*, still own the compiler.** |

### In prose, the load‑bearing ones

- **SPy** is the closest sibling — a compiled Python variant — and we verified it
  from source. Its *type* objection is weaker than it first looks (it already ships
  `int → i32` / `float → f64` aliases, so `def inc(x: int) -> int` is valid SPy
  *and* valid Python). But two things are structural, not cosmetic: its compiler is
  **written in Python and emits C**, so in a browser you pay Pyodide's ~10 MB *and*
  fall back to the slow interpreted path — you cannot finish a compile client‑side.
  And it is **v0.1.0, "design‑incomplete"**, with a currently non‑functional
  `pip install`. You cannot build a curriculum on that yet.

  *Nuance (their compiler is Python → costs Pyodide in a tab):* this is **not**
  strictly permanent. SPy's roadmap names **self‑hosting** ("write the SPy
  interpreter in SPy itself") as its *north star*, which would eventually remove
  Pyodide. But (a) it's their **most distant** milestone — near‑term is still
  f‑strings, `*args`, multi‑arg `print` (features rust‑p2w already ships); (b) it's
  the **interpreter**, so it yields a lean in‑browser SPy *interpreter*
  (MicroPython‑shaped), not in‑browser *compilation* of the student's code; and
  (c) their roadmap has **zero education / assessment focus** (its one browser goal
  is "share button and snippet links").

  *Honest correction on #3:* SPy is **not** a black box as a *compiler* — it has a
  full typed AST (`spy.ast`), a scope+type `symtable`, and a debugger (`spdb`), all
  in Python. Its blue/red (redshift) layering is in fact **well‑suited** to
  analysis (analyze the pre‑redshift tree, exactly the parse→lower split rust‑p2w
  needs). So the assessment layer is **buildable** on SPy — the objection is not
  impossibility, it's **fit and sovereignty**: you'd couple your ~8.3k‑line moat to
  (i) a **v0.1.0, "design‑incomplete"** AST that's still being redesigned, (ii) a
  **different language's** AST (`@blue`/`struct`/`i32`/generics — Python
  teaching‑concepts map onto it only partially), and (iii) **undocumented internals
  of a one‑dev project** (same risk class as ruff's "internal crates," which even
  RustPython won't depend on directly). Own the AST because it's your product and it
  must match the language kids learn — not because SPy can't host it.

- **Pyodide** is the one honest branch where *adopt* can beat *build* — see the
  caveat at the end. It is real CPython, so transfer is perfect and the intro‑AI
  course becomes real. What it costs is #1 (the download) and the fact that you no
  longer own the pipeline. It is the right answer **if** the audience is
  early‑college doing data/AI, and **if** real‑time (games, animation) is off the
  table — because interpreters can't sustain it (Pyodide *and* MicroPython both
  fail 120 Hz).

- **MicroPython** is not a competitor for the browser at all — it's the **robot**
  answer, and it's already in our plan for exactly that. Its cost is glass‑box:
  it's a black box, so none of the assessment layer can attach.

- **RustPython** is healthy and admirable, but it can only be *mined*. In the
  browser it is a full interpreter compiled to WASM — a smaller Pyodide, still a
  black box, still interpreted. On the Pico it can't run (it's `std`, GC'd, large;
  the RP2350 is why MicroPython exists). Its real value to us is as a **reference**
  for CPython‑exact behaviour when we implement exceptions and `random`.

## The through‑line

Every project here is *good*. None is *bad*. The reason none fits is that our
requirement set is an unusual **conjunction**, and "give up X" is always "give up
transfer, or glass‑box, or client‑side‑and‑tiny" — i.e. one of the three things
that make it a *learning* tool rather than just a Python runtime. **The empty
intersection is the reason the project is worth doing.** A skeptic who says "just
use Pyodide/MicroPython/SPy" is, every time, proposing to drop one of the three.

## So the strategy is: mine, don't merge — and still be part of a community

Owning the compiler does **not** mean working alone. Everything above is MIT, so:

- **Mine ideas and implementations (credit in `NOTICE`):** SPy's blue/red
  compile‑time/runtime split; RustPython's CPython‑exact implementations of
  exceptions and `random`; the "annotated Python as house style" idea (SPy shipping
  `int = i32` aliases *validates* the direction we already took with `x: int`).
- **Contribute where it's a genuine gift** and where the people are: a
  direct‑to‑WASM backend is SPy's one structural gap; CPython‑behaviour fixes land
  in RustPython. This is how you meet a potential successor **without** giving up
  the product.
- **Handoff is a people problem, not an architecture problem.** The person who
  could inherit this lives in the ruff / RustPython / SPy communities. You reach
  them by showing up there — and, for **ruff specifically, by adopting a *part*
  (its parser/AST — see the Jul 17 addendum) while still owning the compiler.**
  Adopting a *library* ≠ adopting a *runtime*; the moat is untouched.

## Then why is an education / AI person doing compiler work at all?

The honest tension: the person driving this is a **curriculum / AI‑education**
person, not a compiler person. The compiler is a *means*, never the goal. So why is
there a compiler here, and why would an education person own one?

**Answer: they don't own it — but the requirements *force* a compiler into the
stack, and someone has to.** The forcing function is one requirement we can't drop:

- **Real‑time browser performance is core** — the concept‑gated voxel/"program‑the‑
  world" engine, game templates, on‑frame animation. Kids need smooth interactive
  graphics, not a REPL.
- **Interpreters cannot do real‑time.** Pyodide *and* MicroPython both fail to
  sustain 120 Hz (measured — Łukasz Langa, Feb 2025); RustPython‑in‑wasm is slower
  still. So real‑time is a **hard interpreter‑exclusion.**
- Therefore a **compiler is mandatory** — not an indulgence, not scope‑creep, but
  the direct logical consequence of "real‑time + client‑side + glass‑box." And of
  every compiler, **only rust‑p2w does fast + client‑side + *today*** (SPy's fast
  output is build‑time / years from client‑side; Pyodide is an interpreter). The
  substrate question is settled.

**So the compiler is forced infrastructure — and the fix for "I'm not a compiler
person" is not to adopt someone's compiler (there is no client‑side‑real‑time one
to adopt), it's to *split the ownership*.** The codebase is already architected for
exactly this: the assessment layer is **parse‑only, ~8.3k lines, with zero codegen
dependency**; the engine is a separate ~21k. So:

- **The education / AI person owns the product:** the teaching layer (lints,
  evidence, scaffolds, blocks ⇄ code, step‑debug), the curriculum, and the **spec of
  the library API** — *which* numpy/pandas surface the lessons use. This is the moat
  and it is language‑ and engine‑independent.
- **A compiler person owns the engine:** codegen (WASM‑GC browser + Pico native),
  the no‑GC/FBIP runtime, speed, and the **compiled array/dataframe library.** This
  is a hire / funded collaborator / recruit — not the educator's job.
- **The seam between them is the AST + the analysis APIs**, which are already clean.

So "an AI person doing compiler work" is the wrong frame. The AI person owns the
teaching layer and the API contract; the compiler is *forced infrastructure* owned
by a compiler person. **The technical question is fully resolved. The one real open
problem is a *people* problem — finding and funding the person who owns the
engine** — which is a hiring / grant / community question, not an architecture one.
(This is exactly why "mine ideas and contribute" above matters: the SPy / RustPython
/ ruff communities are the *talent pool* for that person, not a substrate to adopt.)

### Libraries transfer too: match the real API, compile the subset

numpy/pandas follow the same principle as the language. We don't ship real numpy
(that's Pyodide's slow, black‑box, ~10 MB path, and real‑time rules it out). We
implement the **taught subset** with an **API identical to numpy/pandas** — so
`np.array(...).dot(...)`, `df['col']`, `df.mean()` are the exact code a student will
write in a real data‑science course later. The *implementation* is glass‑box and
**compiled** (real‑time‑fast, kids see the matrix math); the *API* is the real
tool's (it transfers). It's a proven pattern (ulab, CuPy), APIs aren't copyrightable
(so matching numpy's is free), and it is **differential‑tested against real numpy**
— the same oracle discipline as CPython. This is the one place we beat Pyodide on
its home turf: real numpy is slow + opaque + heavy; an API‑faithful compiled subset
is fast + transparent + tiny. (Full pandas/sklearn for an advanced data track, if
ever needed, is a *separate* Pyodide‑backed mode — a clean boundary, not the core.)

So the transfer thesis now spans **language (match CPython) + libraries (match
numpy/pandas) + robot (same source → Pico)** — all compiled for real‑time, all
glass‑box, all differential‑tested. That is the coherent, unoccupied product.

## The one caveat that could still flip this

Real‑time‑is‑core closes the old "just use Pyodide" door: with real‑time on the
table, interpreters are excluded and the compiler pays for itself. One door stays
open, honestly:

- **The education half turns out not to be ours to keep** — a successor wants "just
  runs in the browser" and doesn't value the assessment layer. Then the moat is
  moot and a lighter path wins. Until that's true, the conjunction holds, the
  intersection stays empty, and the rational move is: **own the teaching layer,
  hire the engine, mine the rest.**

---

## Session addendum — Jul 17 2026: MicroPython deep‑dive, PXC, the ruff front‑end, and the hire

This session pressure‑tested "own the compiler" against MicroPython one more time,
resolved the last open question (worker vs director), corrected one mislabel (PXC),
and turned the ruff idea into a concrete plan. **Net: the conclusion holds, harder
than before — and the one real move left is to find a compiler person.**

### Two non‑negotiables that were implicit are now explicit — and decisive

Add to the five above:

6. **Client‑side *compilation*, not just client‑side output.** The *compiler
   itself* must run in the browser as wasm and compile the student's code in the
   tab. rust‑p2w already does this (Rust → `wasm32`, called from the Dioxus IDE).
   Sharper than #1, and it's what **kills SPy** (its compiler is Python → needs
   Pyodide to run in‑browser).
7. **Multiple custom emit targets from one source: WASM‑GC (browser) + Pico‑native.**
   MicroPython's client‑side compiler emits *only its own bytecode* — it can't
   produce either target. Owning the compiler is what lets you control what it
   emits. This is what **kills MicroPython as the browser substrate** even though it
   passes #6.

**Together #6 + #7 make rust‑p2w non‑substitutable:** *a client‑side‑wasm compiler
emitting your custom targets* is exactly and only rust‑p2w. No adopt‑path provides
both.

### The worker‑vs‑director question — RESOLVED: worker

The recurring crux: is the student's Python the *worker* (heavy per‑frame compute →
must compile) or the *director* (poll‑don't‑push → a lean interpreter suffices)?
Jason's answer, concrete: **kids loop over cells/voxels in their own Python, every
frame.** That's the worker model — the exact workload every interpreter fails
(Langa: Pyodide *and* MicroPython both drop below 120 Hz). So **compilation is
mandatory, confirmed by example**, not just in principle. The adopt‑a‑runtime thread
is closed for the browser, for good.

### MicroPython, verified from source (not assumed)

Four checks, because "just adopt MicroPython" kept resurfacing:

- **"Fast subset mode" (`@micropython.native` / `@viper`) does NOT help the
  browser.** The native emitters emit real CPU machine code; the assembler backends
  are `asmarm / asmthumb / asmx64 / asmx86 / asmxtensa / asmrv32` — **there is no
  `asmwasm`**, and `MICROPY_EMIT_NATIVE` gates on those arch emitters. The wasm port
  runs **bytecode only**. So viper gives fast loops **on the Pico (ARM)** but
  **nothing in the browser**.
- **Adding a WASM native emitter to MicroPython is research‑grade**, not a friendly
  PR: `emitnative.c` is 132 KB over a **register‑machine, goto‑style** interface;
  WASM is a **stack machine with structured control flow** (needs a relooper), and
  you'd have to JIT wasm from *inside* the wasm sandbox (async
  `WebAssembly.instantiate`). Almost certainly why it doesn't exist.
- **MicroPython has NO reusable AST.** No `ast` module (checked `extmod/`). Its
  "tree" is `mp_parse_node_t` — a **bit‑packed, pointer‑encoded** parse CST in C,
  consumed straight into the 150 KB bytecode compiler. Tooling‑hostile, wrong
  language (C vs your Rust), unstable internal. **Do not build the analysis layer on
  MicroPython's parts.**
- **The supported way to "add on to MicroPython" is user C modules** (the ulab
  pattern) — compiled kernels/libraries callable from Python. Real and useful for
  the **Pico‑runtime / kernel** role (numpy‑style array ops), but it's *adding
  native functions*, not reusing their front‑end, and it is not the browser
  compiler.

**MicroPython's role is unchanged and now precisely bounded:** interim **Pico
runtime** for robotics‑now + compiled‑kernel host via user C modules. Never the
browser, never the front‑end.

### PXC — corrected: it's the *packaging* layer, not a codegen target

PXC = `edly-io/pxc` (Apache‑2.0): *"a new standard for learning activities to
replace SCORM, H5P, LTI."* It's a **learning‑activity packaging / LMS‑interop
standard** (Canvas, Open edX) — your **Lesson Player** export format. It **wraps**
the compiled activity + `.py` source + xAPI/CASE metadata; it does **not** dictate
the compiler and it is **not** a code‑generation backend. (Briefly mislabeled a
codegen emit target earlier in the session — corrected.) You emit PXC from your
*tooling*, on top of whatever runs underneath. More evidence the **product = the
education / lesson / assessment layer** — portable, runtime‑independent.

### The reuse that *does* pay: ruff's parser/AST (NOT MicroPython's)

If the goal is "stand on an existing parser instead of maintaining one," the right
donor is **ruff (Astral)** — `ruff_python_parser` / `ruff_python_ast`: Rust (no
FFI), a **real full‑CPython AST with source spans on every node**, tooling‑grade
(what ruff/ty lint on), MIT. The opposite of MicroPython's embedded CST.

**Adoption architecture — the adapter, then the hybrid:**

- **Step 1 — adapter (contained):** keep your own `ast.rs`; ruff parses; a new
  `ruff_lower.rs` lowers ruff‑AST → your `StmtKind`/`ExprKind`, with friendly
  *"not in this subset yet"* rejections for out‑of‑subset nodes. **Delete
  `lexer.rs` (926) + `parser.rs` (2,449) ≈ 3.4k; add ≈ 1k.** All ~24k lines of
  downstream consumers (lint / evidence / blockly / reuse / hoist / codegen / llvm /
  debug / component + both backends) stay **unchanged**. Net LOC ≈ flat — the win is
  *you stop owning a Python grammar*, plus **spans on every node** (today only binary
  ops carry spans) and **full‑Python parsing** (robust recovery; subset enforced at
  lowering).
- **Why NOT rewrite everything onto ruff's AST directly ("full B"):** rust‑p2w is a
  **permanent subset** — you'll never implement all of CPython. A "full‑Python in →
  subset out" boundary must exist *forever*; the clean design puts it in **one
  lowering step**, not smeared across every backend/analysis pass. Full‑B would
  (a) rewrite ~24k lines onto ruff's ~2×‑larger node set with `_ => unsupported`
  arms everywhere, (b) couple your *entire* codebase to a fast‑moving external AST
  forever (vs. only the adapter), (c) discard the "small AST = subset firewall"
  invariant, and (d) not even remove the work — your subset‑specific distinctions
  (`For` range‑fast‑path vs `ForEach`; bare‑name `Call`) get re‑derived at every use
  site instead of once.
- **The right long‑term shape is the hybrid**, reached incrementally: **ruff AST on
  top** for parse + diagnostics + spans (error underlining, "did you mean", the
  deferred set‑notation glyphing); **lower to your subset AST** at the codegen
  boundary; **codegen / llvm / debug stay on your lean AST.** Climb the ruff AST
  *upward* into the span‑hungry passes as it pays; stop at the firewall.

**Two gates before adopting ruff at all (measure, don't argue):**
1. **wasm‑size delta** — ruff's crates + deps add to the *client‑side compiler*
   binary (#1). ruff does compile to wasm (its playground runs in‑browser), so it's
   feasible, but the size cost must be measured.
2. **beginner error‑quality regression** — deleting `parser.rs` loses your
   K‑12‑tuned *parse* errors (missing colon, did‑you‑mean keyword). ruff's syntax
   errors are excellent but *general*; subset‑rejection errors you still own at
   lowering, but raw syntax errors become ruff's. Measure the regression on typical
   beginner typos; re‑map if adopting.

**Dependency note:** the ruff crates hit crates.io only **2026‑07‑16, at v0.0.5**
(too green) → **git‑pin to a ruff release tag**; the adapter shields downstream from
AST churn.

### The conclusion Jason reached himself: **"I need a compiler person."**

Everything converges on split ownership — now stated by Jason directly. The
technical question is settled; the bottleneck is **people.** The role, bounded:

**Compiler‑person remit (hire / fund / recruit):**
- Owns the **engine**: WASM‑GC (browser) + Pico‑native (LLVM/RP2350) codegen; the
  no‑GC/FBIP runtime; speed.
- Owns the **compiled numpy/pandas‑API subset** (array/dataframe kernels;
  differential‑tested vs real numpy).
- Owns the **ruff front‑end integration** (adapter → hybrid above) and the emit
  targets (incl. whatever the PXC packaging needs from the compiler — little; PXC is
  mostly tooling).
- Owns **exceptions** and **first‑class functions** (the two open language features;
  first‑class fns break the current lambda‑lifting closure equivalence → need real
  environments / escape analysis).

**Educator (Jason) keeps:** the teaching layer (lints / evidence / scaffolds /
blocks / step‑debug), curriculum, the **library‑API spec**, and the **PXC /
Lesson‑Player packaging + xAPI/CASE** — the parse‑only ~8.3k‑line moat with zero
codegen dependency.

**Where the person lives:** the **ruff/Astral, SPy, PyPy, RustPython** communities —
Python‑compiler people. The "mine ideas + contribute a direct‑to‑WASM backend to
SPy" play is *also* the recruiting channel. **This is a hiring / grant / community
problem, not an architecture one.**

---

## Status — decided vs. open (as of Jul 17 2026)

A one‑screen summary for a collaborator picking this up cold.

**Decided:**
- **Own the compiler (rust‑p2w).** #6 client‑side *compilation* + #7 custom emit
  targets make it non‑substitutable; no adopt‑path provides both.
- **Split ownership.** Educator owns the teaching layer + library‑API spec + PXC /
  Lesson‑Player packaging; a compiler person owns the engine. The seam (AST +
  analysis APIs) is already clean — analysis is parse‑only ~8.3k lines, zero codegen
  dependency.
- **MicroPython = interim Pico runtime + kernel host (user C modules) only.** Never
  the browser, never the front‑end. Verified from source (no viper in the wasm port,
  no reusable AST, worker‑model excludes it).
- **PXC = packaging / LMS‑interop layer, not a codegen target.**
- **Adopt ruff's parser/AST as the front‑end *via an adapter*** (own AST + compiler
  kept; long‑term hybrid) — *pending the two gates below.*

**Open — all people / measurement, not architecture:**
- **The hire.** Find / fund the compiler person. The one real blocker; everything
  else is downstream of it.
- **The ruff spike.** Measure the **wasm‑size delta** + **beginner‑error
  regression** before committing (~a day; natural first task for the hire).
- **Language features.** Exceptions; first‑class functions (breaks the current
  lambda‑lifting closure equivalence → needs real environments / escape analysis).
- **The thesis question.** Is the no‑GC/FBIP **Pico‑native backend** a real
  deliverable (CCSU / paper) or a nice‑to‑have? If nice‑to‑have, MicroPython keeps
  the robot and the native backend can wait; if a deliverable, it's on the engine
  owner's plate. This is the last genuinely open *design* decision.
