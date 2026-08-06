# Host interface: adopt the standard, justify the bespoke

A decision record for the `env.*` / `acorn:component/host` imports. Companion
to `SUBSET_POLICY.md` (what may enter the language) — this is about what a
program may *reach*.

## The default

> **Use the standard WASI interface. Bespoke needs a stated reason, in writing,
> per capability.**

The burden of proof is on inventing, not on adopting. Reasons, in order of how
much they matter here:

- **Maintenance is the scarce resource.** One person across several projects
  cannot also maintain an interface definition, its host implementations, and
  its tooling. A standard is a thing someone else maintains.
- **Tooling comes free.** `wit-bindgen` generates the glue; `wasmtime` runs
  WASI components with **no custom host at all** — which is precisely what the
  fuel-metered agentic runner wants. Every bespoke import is a host we write
  again in every environment.
- **Recruitment.** A grad student already knows WASI. Nobody knows
  `acorn:component/host` but us, and the cost of teaching it lands on the one
  person who has least time ([`PRIOR-ART-TYPES.md`](PRIOR-ART-TYPES.md) is the
  package they read; this should not add vocabulary to it).
- **It ages better.** Bespoke interfaces rot quietly; standards get
  implementations we did not write.

## Two reasons that do justify bespoke

**1. No standard exists.** There is no `wasi:stage`, no `wasi:lesson-field`, no
`wasi:water-sample`. `get_field`, `set_field`, `seed`, `on_frame`,
`set_position` and the DOM capabilities have no WASI equivalent, so this is not
"not invented here" — it is the absence of an alternative. The obligation that
remains is to **shape them as proper WIT interfaces** (which `component.rs`
already does) so they are legible, replaceable, and could be standardised later
rather than being ad-hoc function names forever.

**2. A specific semantic mismatch**, written down, not assumed. Note that this
bar is *harder to clear than it looks* — see `wasi:random` below, where the
mismatch I asserted turned out not to exist.

Width — preferring `get_field("ph")` over `wasi:filesystem` — is an argument for
choosing the **narrowest capability available**, including among standard ones.
It is not an argument for inventing. A narrow capability is both safer and more
teachable, and it keeps `capabilities()` (see `lib.rs`) worth reading: a
manifest line saying `wasi:filesystem/types` tells a teacher nothing about what
a program does.

## Corrections — two things asserted here that were false

**"WASI would put clocks, random, filesystem and stdio in the import list."**
False, and it described Preview 1. **Preview 2 is capability-based**: a
component *"starts with no ambient authority"*, no global namespaces at runtime,
no global functions at link time, unforgeable handles. Nothing is imported that
the world does not declare.

**"`wasi:random` is cryptographically secure and non-deterministic by design."**
False. `wasi:random/insecure` explicitly states it is *not* cryptographically
secure and requires no unpredictability; `insecure-seed` says its value *"may
even be entirely deterministic."*

## Capability by capability

### `wasi:cli/stdout` — reopened; the blocker is our shim generation, not WASI

Today: `p2w-putc: func(byte: s32)` plus a hand-written three-line C shim.
WASI: `get-stdout: func() -> output-stream`, where `output-stream` is a
**resource** from `wasi:io/streams`. That means a resource handle with a
lifetime, writing through a method, buffering (`list<u8>` versus our
byte-at-a-time `p2w_putc`), and `wasi:io/streams` + `wasi:io/error` in every
world.

**That cost is real but it is mostly an artifact of `component.rs` generating
the C shim BY HAND.** `wit-bindgen` exists to emit exactly that glue. So the
honest question is not "is WASI too expensive" but **"should the component
chain generate its shims with `wit-bindgen` instead of by hand?"** — and if the
answer is yes, most of the objection evaporates.

Two things that remain true regardless, and should be tested rather than
assumed: the world **does** get bigger, which costs something in a manifest
whose value is being short and specific; and **buffering changes when output
appears** — not hypothetical, since block-buffered stdout made every native
trap look silent in `tests/backend_diff.rs` and faked a whole run of results.

**Status: open.** Not "keep bespoke". The next step is to cost `wit-bindgen`
in the component chain, because that decides this and several others at once.

### `wasi:random` — reopened, and the case for bespoke is weaker than claimed

What we need is a **seeded, per-attempt, assessment-controlled** value so
grading reproduces and `BACKEND_DIVERGENCE.md` has something stable to compare.

`wasi:random/insecure-seed` returns a 128-bit value and permits full
determinism, so the "WASI can't do deterministic" objection is dead. What it
does not offer is any way for the *assessment layer* to choose the value for a
given attempt — its stated purpose is one-shot hash-map DoS protection, and
using it as an attempt seed would be borrowing an interface for something it
does not mean.

**That is a real but narrow objection**, and worth revisiting: a bespoke
`seed` whose WIT shape mirrors `insecure-seed` costs nothing extra and keeps
the door open.

### `wasi:clocks` — adopt it if time is ever needed

Right width, standard, nothing to invent. Determinism is handled by **not
granting it in graded contexts**, which is the capability model working rather
than something to route around. Nothing needs it yet.

### `wasi:filesystem` — not for student code

Wrong width for the task: descriptors, preopens and streams where a lesson
wants "the water-sample reading". `get_field`/`set_field` matches the task and
grants less. A headless tool is a different question — and a headless tool is
not the sandbox.

### Domain capabilities — bespoke by necessity

`seed`, `set_field`/`get_field`, `report`, `evidence`, `on_frame`, the DOM and
stage capabilities. No standard exists. Keep them shaped as WIT interfaces.

## Why the component chain targets `wasm32-unknown-unknown`

To keep each component's world **exactly** the capabilities its def group uses,
which is what `component.rs` generates. This is a statement about world size,
not about WASI being unsafe. It is worth re-testing if the chain moves to
`wit-bindgen`.

## The next decision

**Cost `wit-bindgen` in the component chain.** It gates `wasi:cli/stdout`, it
would remove the hand-written-shim maintenance, and it is the difference
between "we prefer our own" and "we could not afford theirs" — which are very
different positions to be defending in a year.
