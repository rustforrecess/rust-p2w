# Class layout — proposal

**Status (2026-11-10): steps 1 and 2 BUILT on both backends; steps 3–5
PROPOSED.** Claims about
p2w today were checked by running programs and reading the generated code
the same day.

## The proposal in one paragraph

An object's shape is set when it is built: its attributes are the ones its
class gives it in `__init__` (or declares in `__slots__`), and nothing else.
With that rule every class has a fixed, known set of attributes, so each
class can get a fixed layout, `self.speed` can become one field read instead
of a dictionary lookup, numbers in fields can be stored unboxed, and methods
can be called through a per-class table instead of a dictionary. It is the
design statically compiled object languages use, and the one fast Python
implementations rebuild at runtime with hidden classes; we get it at compile
time because the compiler sees every class.

## p2w today

| | WASM | native |
|---|---|---|
| an instance | `$OBJECT` = class + `$ITEMS` slot array (since step 2) | `[tag][rc][class_id][slot count][slots…]` (since step 2) |
| `self.speed` | in a method: `array.get` at a constant index; elsewhere: `$obj_getattr` by attribute id through the class's `$SLOTMAP` (since step 2) | in a method: `p2w_slot_get(self, k)`, a direct slot read; elsewhere: `p2w_getattr` by attribute id (since step 2) |
| a method | in a `$DICT` on `$CLASS`; uniform `(self, args)` signature, so each call builds and unpacks an argument list | compile-time switch on the class id |

Until step 1, a misspelt attribute was silent: `r.speeed = 5` created a new
attribute and the program printed the old `speed`.

## The design

1. **Fixed attribute sets (the subset rule).** A class's attributes are its
   class variables, the attributes its `__init__` sets on `self`, and its
   base's. A class that declares `__slots__` gets exactly those names (plus
   class variables), and even its `__init__` is held to them, as in CPython.
   Every accepted program is valid Python with the same behavior. CPython
   itself applies this rule to slotted classes, with an AttributeError.
2. **Fixed layouts.** Each class's attributes get fixed positions: a typed
   WASM-GC struct, a fixed offset natively. `self.speed` is one field read.
   A subclass's fields extend its base's as a prefix, which WASM-GC struct
   subtyping supports directly (a struct declares one supertype).
3. **Unboxed fields.** Where the checker knows a field is an int or a float,
   it holds the raw number, as typed locals already do (`VALUE_MODEL.md`).
   The biggest win for robot code full of numbers.
4. **Method tables.** Each class has an array of method references at fixed
   indices, built at compile time, with real typed parameters instead of an
   argument list. A call is one indexed load and a call; when the exact class
   is known, or no subclass overrides the method, a direct call.
5. **Single inheritance for data, mixins for behavior.** The rule recorded
   in `CLASSES_DESIGN.md` (one data base plus method-only mixins, `super()`
   resolved per class by compiling a copy of each mixin method per class).
   Mixins add no fields, so layouts stay fixed. Not scheduled.

A value whose class the checker can't determine goes through the same
per-class tables with a small runtime check; the attribute dictionary is no
longer needed, since every class is known at compile time.

## Step 1 — built (2026-11-08)

`src/attrs.rs`, run in the shared front end (`hoist_nested_functions`), so
WASM, native and both debugger engines refuse the same programs with the
same words:

- In a class's own methods, `self.x = …` (also `+=` and unpacking targets)
  for an `x` the class doesn't have is an error, with did-you-mean:
  "`Robot` has no attribute `speeed` to set: a `Robot` gets its attributes in
  `__init__`, so `boost` can only change those. Did you mean `speed`?"
- Elsewhere the object's class isn't known yet, so only an attribute no
  class in the program has is refused ("no class in this program has an
  attribute `speeed` to set …"). When the checker can type the object, step
  2's work, this tightens to the object's own class.
- With `__slots__`, the slots are the set: `A` with `__slots__ = ('x',)`
  whose `__init__` sets `self.y` is refused, as CPython raises
  `AttributeError: 'A' object has no attribute 'y'`.
- Class variables set through the class name (`Counter.made = …`) are the
  existing class pass's business, unchanged.
- A class whose base isn't in the program has unknown attributes and is not
  checked.

FEATURE_PROBES.md now records `__slots__` as enforced.

Not yet: dataclass-style bare annotations in the class body
(`speed: int`) don't parse; they would be a second way to declare an
attribute.

## Step 2, native — built (2026-11-10)

- `attrs::slot_layouts` gives every class its slots: the base's first, in
  the base's order, then the class's own new attributes, sorted. An
  inherited attribute therefore has the same index in every subclass.
- An instance is `[tag][rc][class_id][slot count][slots…]`. An unset slot
  holds 0, which no value is, so reading one is CPython's AttributeError.
- In a method that never rebinds `self`, `self.x` compiles to
  `p2w_slot_get(self, k)` / `p2w_slot_set(self, k, v)` with `k` known at
  compile time, valid for subclasses too. A read of an attribute that is
  also a class variable keeps the generic path, because an unset slot there
  means "read the class variable".
- Anywhere else, `obj.x` passes x's module-wide attribute id; the generated
  `p2w_slot_of(class_id, attr_id)` (a switch LLVM turns into jump tables)
  finds the slot. No attribute name strings are built for writes; reads
  still pass the interned name for the class-variable fallback.

**Measured** on the step program below (3 million `step()` calls, three
attributes, clang -O2, Windows x64, nine runs each, interleaved):

| | min | median |
|---|---|---|
| before (attribute dict) | 862 ms | 1,052 ms |
| after (slots) | 263 ms | 300 ms |

About 3.5× faster, same output, `live=0`. Both versions still allocate one
boxed float per `self.x + self.v`, 3 million in all: that is step 3's
target.

```python
class Body:
    def __init__(self):
        self.x = 0.0
        self.v = 1.5
        self.n = 0
    def step(self):
        self.x = self.x + self.v
        self.n = self.n + 1
```

## Step 2, WASM — built (2026-11-10)

Same layout and rules as native. `$OBJECT` holds its `$CLASS` and an
`$ITEMS` array of slots (null = not set). `$CLASS` gains a `$SLOTMAP`
(attribute id → slot, -1 for none) and its slot count, which
`$instantiate` uses to size the array. In a method that never rebinds
`self`, `self.x` is an `array.get`/`array.set` at a constant index, and the
attribute-name literal is built only on the error path; everything else goes
through `$obj_getattr`/`$obj_setattr` with the attribute id. The WASM
backend prints CPython's exact AttributeError for an unset or absent
attribute (`'A' object has no attribute 'x'`); the native runtime, which
has neither name at hand, prints the table's degraded form.

**Measured** under the wasmtime runner (`p2w run`, fuel-metered, seven runs
each, the same step program):

| | min | median |
|---|---|---|
| before (attribute dict) | 11,377 ms | 12,710 ms |
| after (slots) | 3,258 ms | 3,756 ms |

About 3.4× faster, same output. The fuel-metered runner is far slower than
a browser engine in absolute terms; the ratio is the claim.

A shared probe group ("Object attributes in fixed slots") runs a subclass,
a class variable shadowed through an instance, and an attribute set from a
plain function on both backends; both match CPython.

## Plan for the rest

| Step | Work | Gate |
|---|---|---|
| 2b | The checker types objects, so attribute assignments outside methods check against the object's own class rather than "some class has it" | a typo through a typed variable (`r.speeed = 5` after `r = Robot()`) refused naming `Robot`; the class probes and oracle programs unchanged; zero backend divergence |
| 3 | Unboxed int/float fields | as above, plus the existing leak gate (`live=0`) |
| 4 | Method tables with typed parameters; direct calls when the class is known | as above; a microbenchmark of method calls before and after |
| 5 | Mixins | when a lesson asks for them |

Steps 2–4 are a representation rework comparable to the value-model work,
on both backends. Measure first (step 2's gate includes a before/after on
attribute-heavy code, on the Pico as well as in the browser).

## Why this design

- **Speed:** attribute reads and method calls go from dictionary lookups and
  list building to about one instruction each, which matters most in a
  10 ms robot control loop.
- **Teaching:** "an object's shape is set when it's built" is a sound idea,
  catches one of the commonest beginner bugs, and carries into Java, C#,
  Rust structs and Python dataclasses.
- **Fit:** it matches the typed-Python house style, the subset's "no
  dynamic name resolution" rule, and the verification thesis: the checker
  can see the whole object model.
