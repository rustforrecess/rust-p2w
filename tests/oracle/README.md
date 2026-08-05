# Type-system oracles

**This is a specification by example for a type checker that does not exist
yet.** It says what the checker must accept, what it must reject, and which
questions it must answer — without saying anything about how it should work.

That separation is deliberate. Whoever builds inference should be free to
choose the approach (constraint-based, Datalog over `sequent`, something else)
and defend it. These files tell them when they are done; they do not tell them
what to write.

## Layout

```
ok/              must compile — today, and still after typing lands
must-reject/     compiles today, MUST be rejected once typing exists
open-question/   the right answer is not decided; whoever builds it decides
```

Each file opens with a comment saying *why it is here*. That comment is the
actual specification — the code is just the example.

## How they are enforced

`tests/oracle.rs` runs all three directories:

- **`ok/`** — asserted now. A regression here breaks the build immediately.
- **`must-reject/`** — asserted to *currently compile*, which documents the
  gap. When a checker lands these tests fail, and that failure is the signal to
  flip them deliberately rather than discovering the change by accident.
- **`open-question/`** — compiled, not judged. They exist to force a decision.

## Adding a case

Keep it minimal — one idea per file, no cleverness. A case that fails for two
reasons teaches nothing about either. Say in the comment what a student would
have been trying to do, because the error message has to make sense to them.

## What the messages have to do

Rejection is half the job. The other half is that a twelve-year-old can act on
what they are told, which means:

- **Point at the cause, not the symptom.** *"`total` is a string because line 3
  assigned it `input()`"* beats *"type mismatch at line 40."* This is the single
  biggest failure of type errors in teaching languages, and it is the reason a
  derivation-carrying checker is worth considering.
- **Name what the student was probably trying to do.**
- **Carry a fix ladder** — question, hint, fix — matching `lint::Scaffold`.
