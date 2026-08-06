# Why the host interface is bespoke, and when it should not be

A decision record for the `env.*` / `acorn:component/host` imports: why they are
our own function names rather than standard WASI interfaces, capability by
capability. Companion to `SUBSET_POLICY.md` (what may enter the language) —
this is about what a program may *reach*.

## The objection that turned out to be wrong

The reflex answer, which I gave and which is **false**: "WASI would put clocks,
random, filesystem and stdio in the import list."

WASI **Preview 2 is capability-based**. A component *"starts with no ambient
authority"*; there are no global namespaces at runtime and no global functions
at link time, and handles are unforgeable. **Nothing is imported that the
world does not declare.** That describes Preview 1, not Preview 2, and it is
not a reason to avoid WASI.

So the question is not ambient authority. It is **width**.

## The rule

> **Prefer the narrowest capability that does the job. Use the standard
> interface when it happens to be the right width.**

`wasi:filesystem` grants *files*. `get_field("ph")` grants *one value*. The
second is a strictly better capability — less authority, and a name a student
can read. Narrowness is a security property and a pedagogical one at the same
time, which is rare enough to lean on.

This also keeps `capabilities()` (see `lib.rs`) meaningful. That function reads
the module's imports and calls them the program's complete reach. The list is
only interesting while each entry is *specific*. One line reading
`wasi:filesystem/types` tells a teacher nothing about what the program does.

## Capability by capability

### `wasi:cli/stdout` — the case that looked strongest, and isn't

Worth the most detail, because "your `p2w-putc` **is** stdout, so speak the
standard" is a genuinely good argument until you cost it.

What we have today is a flat function:

```wit
p2w-putc: func(byte: s32);
```

and a three-line C shim that forwards it. What `wasi:cli/stdout` is:

```wit
get-stdout: func() -> output-stream;   // output-stream from wasi:io/streams
```

`output-stream` is a **resource**, so adopting it means:

1. Acquiring and holding a resource handle in a generated C shim, with the
   lifetime discipline resources require.
2. Writing through a resource *method* (`blocking-write-and-flush`) rather than
   a function call.
3. **Buffering.** WASI writes `list<u8>`; our runtime emits a byte at a time via
   `p2w_putc`. A buffer means deciding when output appears — and a flush bug is
   not hypothetical here: block-buffered stdout made every native trap look
   silent in `tests/backend_diff.rs` and faked a whole run of results.
4. Pulling `wasi:io/streams` (and transitively `wasi:io/error`) into every
   component's world.

⇒ **The manifest gets bigger, not smaller**, and a hello-world component grows a
resource-bearing interface tree to express something it currently says in one
function. The portability win is real but smaller than it looks: supplying one
`p2w-putc` import is about three lines in any host — `tests/exec.rs` does
exactly that with `func_wrap`, and so would a wasmtime fuel-metered runner.

**Verdict: keep `p2w-putc`.** Revisit only if components need to run unmodified
in a host we do not write, which is not a goal today.

### `wasi:random` — no, for a requirements reason

`wasi:random` is cryptographically secure and non-deterministic **by design**.
We need the opposite: a **seeded, per-attempt, assessment-controlled** value so
that grading reproduces and the differential harness has anything to compare.
`env.seed` is not a poor imitation of `wasi:random`; it is a different thing
that WASI does not offer.

**Determinism is a feature here, not an accident.** No ambient clock and no
ambient randomness means a student program is a pure function of its inputs —
which is what makes reproducible grading, replayable stepping, and
`BACKEND_DIVERGENCE.md` possible at all.

### `wasi:clocks` — fine, if time is ever needed

Standard, right width, nothing bespoke to invent. The determinism concern is
handled by **not granting it in graded contexts**, which is the capability
model working rather than something to route around. Nothing needs it yet.

### `wasi:filesystem` — no for student code

Wrong width, per the rule. Descriptors, preopens and streams where a lesson
wants "the water-sample reading". `set_field`/`get_field` is the capability
that matches the task. A headless tool is a different question, but a headless
tool is not the sandbox.

## Why the component chain targets `wasm32-unknown-unknown`

Not because WASI is unsafe — see above — but because the component's world
should contain **exactly** the capabilities the def group uses, which is what
`component.rs` already generates. Targeting `wasm32-wasip2` to get components
"natively" would trade an auditable, minimal world for a build-step
convenience.

## What would change this

- A requirement that our components run in **someone else's** WASI host
  unmodified. That is the argument `wasi:cli/stdout` is waiting for.
- A lesson that genuinely needs wall-clock time ⇒ take `wasi:clocks`, and
  decide separately whether it is granted during assessment.
- **`wasi:io/streams` becoming cheap to shim** (a generator that emits the
  resource glue) would remove most of the stdout objection, since the argument
  above is about cost and world size, not about safety.
