# What a type error says to a twelve-year-old

A draft of the messages a type checker should produce, one per case in
`tests/oracle/must-reject/`. **The wording here is a starting point to argue
with, not a specification to implement verbatim** — it is the part of this work
that is pedagogy rather than compilers, and it should be corrected by someone
who has watched students hit these.

## Why this is a deliverable and not a detail

The strongest empirical result in this area says the messages *are* the
intervention. Santos & Becker (UKICER 2024) put 106 participants through six
buggy programs and measured time-to-fix:

> Handwritten explanations still outperform LLM and conventional error
> messages, both on objective and subjective measures.

GPT-4-generated messages beat conventional compiler output in **one of six**
tasks. Students preferred them anyway — preference and effectiveness came
apart. So the authored ladder is the thing that works, and an AI tutor's job is
to *route to* one, not improvise a replacement. See `PRIOR-ART-TYPES.md`.

## The rules

**1. Point at the cause, not the symptom.** *"`total` is a string because line 3
assigned it `input()`"* beats *"type mismatch at line 40."* This is the single
biggest failure of type errors in teaching languages and the main reason a
derivation-carrying checker is worth the trouble.

**2. Name what they were probably trying to do.** A student who writes
`age + 1` wants next year's age. Say so, then say why it doesn't work.

**3. Every message carries a ladder** — question, hint, fix — matching
`lint::Scaffold`. The question comes first because a student who answers it has
learned something; a student who reads the fix has only moved on.

**4. Concise, jargon-free, nonprescriptive** (Morazán et al., TFPiE 2025). No
"expected", no "operand", no "unsupported", no type-theory vocabulary.

**5. Never blame the student for our gap.** If the subset doesn't support
something Python does, say *that*. `'a' < 'b'` currently reports "this operator
needs numbers on both sides", which is false — Python compares strings fine.
Ours doesn't. Those are different sentences.

**6. Use Python's own error names** (`TypeError`, `NameError`) so the
vocabulary transfers to CPython, which `ErrorKind` already does.

---

## The messages

Each block: the program, where the error goes, the one-line message, and the
ladder.

### `age = "12"` then `age + 1`

The canonical one, and the reason cause-not-symptom matters: nothing is wrong
on the line the error appears on.

- **at** the `+`, **also pointing at** the line that made it text
- **TypeError — `age` holds text, not a number, so 1 can't be added to it.**
  Line 1 put quotes around `12`, and quotes make text.

| | |
|---|---|
| question | Line 1 gave `age` the value `"12"`. Is that a number, or text that looks like a number? |
| hint | The quotes make it text. Python can join text to text and add numbers to numbers, but it can't add a number to text. |
| fix | `int(age) + 1` turns the text into a number first. |

### `count: int = "none yet"`

The easiest possible message, because the intent is written on the same line.

- **at** the value
- **TypeError — you said `count` would be a whole number, but gave it text.**

| | |
|---|---|
| question | You wrote `count: int`. Is `"none yet"` a whole number? |
| hint | `: int` is a promise about what this name will hold. This value breaks the promise on the same line you made it. |
| fix | Start it at `0`, or change the label to `count: str` if text is what you meant. |

### `def double(n: int) -> int` returning `"twice " + str(n)`

The promise and the break are on different lines, so both belong in the message.

- **at** the `return`, **quoting** the signature
- **TypeError — `double` promises to give back a whole number (`-> int`, line
  1), but this returns text.**

| | |
|---|---|
| question | The first line promises `-> int`. What kind of thing does this `return` actually hand back? |
| hint | `str(n)` makes text, and joining text to text keeps it text. The promise and the return have to agree. |
| fix | Return `n * 2` to keep the promise, or change it to `-> str` if text is what you want. |

### `area("3", 4)` where `def area(width: int, height: int)`

The function is correct. The caller is wrong. **A message pointing inside
`area` would send the student to edit working code** — the worst thing an error
can do.

- **at** the argument, **not** the function body
- **TypeError — `area` wants a whole number for `width`, but this passes text.**

| | |
|---|---|
| question | `area` asks for `width: int`. Is `"3"` a number, or text? |
| hint | The quotes make it text. The function is fine — it's this call that hands it the wrong kind of thing. |
| fix | `area(3, 4)`, without the quotes. |

### `area(3)` where `area` takes two things

**Already correct today**, and worth keeping as the model: *"area() is missing a
required argument 'height'."* It names the function, the count, and the missing
parameter **by name**.

| | |
|---|---|
| question | How many things does `area` ask for on line 1, and how many did you give it? |
| hint | It needs a `width` and a `height`. Only the width arrived. |
| fix | `area(3, 4)` — the second number is the height. |

### `total = 5` then `total(3)`

⚠ **Currently WRONG.** It says *"unknown function 'total'"*, and `total` is not
unknown at all — it is a number, bound one line up. A student reads that and
hunts for a misspelling that does not exist. Fixing it needs the compiler to
know what the name **is**, which is exactly the missing piece.

- **at** the call, **pointing at** the assignment
- **TypeError — `total` is a number (line 1), not something you can call.**

| | |
|---|---|
| question | What did line 1 put in `total`? |
| hint | Round brackets after a name mean "run this function". `total` holds `5`, and a number isn't something that runs. |
| fix | Did you mean `total * 3`? |

### `score = 42` then `score[0]`

- **at** the brackets, **pointing at** the assignment
- **TypeError — `score` holds one number, so there's no `[0]` inside it to get.**

| | |
|---|---|
| question | Square brackets pull one item out of a group. What group is `score`? |
| hint | Line 1 gave it a single number, not a list. A single number has no first item — it *is* the value. |
| fix | Use `score` on its own. If you meant a list, write `score = [42]`. |

### `"hello world" - "world"`

The message must **acknowledge that the expectation is reasonable**: `+` joins
text and `*` repeats it, so two of the three operators a student knows do work.

- **at** the `-`
- **TypeError — `+` joins text together, but there's no way to subtract one
  piece of text from another.**

| | |
|---|---|
| question | `"ab" + "cd"` gives `"abcd"`. What would `"abcd" - "cd"` mean — remove it from the end, from anywhere, or every copy? |
| hint | That's exactly why it isn't allowed: there's more than one sensible answer, so Python makes you say which you want. |
| fix | `full.replace("world", "")` removes it. `full[:5]` keeps the first five letters. |

---

## Two existing messages that need rewriting

Found by `RUNTIME_SEMANTICS.md`. Neither is a type-checker problem — both can be
fixed today.

**Iterating a number** currently says *"object of type 'int' has no len()"*. The
student never mentioned `len`; that is our implementation talking. It should say
**"`n` is one number, so there's nothing to go through one at a time. Did you
mean `range(n)`?"** — which also answers the question they actually had.

**`'a' < 'b'`** currently says *"this operator needs numbers on both sides"*,
which is **false** — Python compares text fine, alphabetically. Ours doesn't
yet. It should say **"comparing text with `<` isn't supported yet — this one is
our gap, not your mistake."** Rule 5.

## What is deliberately not decided here

The **format** — how a message, its cause line, and its ladder are rendered in
the IDE versus in `p2w check --json` — is `lint::Scaffold` plus the harness's
existing shape, and needs no new invention.

The **open questions** in `tests/oracle/open-question/` have no messages here
on purpose. Each needs a decision first, and the message follows from it. The
one that most changes its message is whether a function must return on every
path: if it must, the error becomes *"what should this give back when the number
is small?"*, which is a better question than anything the optional-type answer
produces.
