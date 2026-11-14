# Class layout — proposal

**Status (2026-11-13): steps 1, 2 and 3 (float fields) BUILT on both
backends; steps 2b, 4 and 5 PROPOSED.** Claims about
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
| an instance | `$OBJECT` = class + `$ITEMS` slot array + `$FITEMS` float array (steps 2, 3) | `[tag][rc][class_id][counts][slots…][f64 fields…]` (steps 2, 3) |
| a float field `self.x` | in a method: `array.get $FITEMS` at a constant index, a raw `f64`; elsewhere `$obj_getattr_f` by attribute id | in a method: `p2w_fslot_get(self, f)`, a raw `double`; elsewhere `p2w_getattr_f` by attribute id |
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
3. **Float fields.** A field whose every assignment is provably a float
   holds the raw `f64`, as typed locals already do (`VALUE_MODEL.md`). Ints
   need nothing: a small int is already an immediate tagged value, so only
   floats allocate. The biggest win for robot code full of numbers.
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

- A class's attributes include what the methods `__init__` calls on `self`
  set, transitively and through the base chain: `def __init__(self):
  self.reset()` gives the object what `reset` sets (since 2026-11-11).
- In a class's own methods, `self.x = …` (also `+=` and unpacking targets)
  for an `x` the class doesn't have is an error, with did-you-mean:
  "`Robot` has no attribute `speeed` to set: a `Robot` gets its attributes in
  `__init__`, so `boost` can only change those. Did you mean `speed`?"
- Any other object, inside a method (`other.tag = …`, `me = self;
  me.x = …`) or outside, has a class not known here yet, so only an
  attribute no class in the program has is refused ("no class in this
  program has an attribute `speeed` to set …"). Step 2b tightens this to the
  object's own class. Until then an assignment the rule lets through can
  still fail at run time (`def tag(o): o.y = 5` with an `o` whose class has
  no `y`); WASM, native and the step debugger all raise AttributeError
  there.
- With `__slots__`, the slots are the set: `A` with `__slots__ = ('x',)`
  whose `__init__` sets `self.y` is refused, as CPython raises
  `AttributeError: 'A' object has no attribute 'y'`. A slotted class's
  class variables are read-only through an instance (CPython: "'A' object
  attribute 'count' is read-only") and take no slot.
- Class variables set through the class name (`Counter.made = …`) are the
  existing class pass's business, unchanged.
- A class whose base isn't in the program has unknown attributes and is not
  checked.

FEATURE_PROBES.md now records `__slots__` as enforced.

A review on 2026-11-11 (a second model, probing CPython, WASM, native and
the step debugger) found and fixed: a base method's fast-path read of
`self.x` trapping when a *subclass* supplies `x` as a class variable (an
unset slot now falls back to the full lookup instead of trapping); a
comprehension variable named `self` rebinding `self` unseen, so the fast
path read and wrote another object's slots (`attrs::rebinds` now sees
comprehension targets); `obj.x = f()` and `xs[i] = f()` evaluating the
target before the value, which on native wrote into freed memory when `f`
dropped the object (both backends now evaluate the value first, as
CPython does); native rejecting attributes as unpacking targets; and the
step debugger accepting attribute stores Run refuses. Each is now a
probe in "Object attributes in fixed slots".

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

## Step 3, float fields — built (2026-11-13)

**Which fields.** Decided for the whole program, per attribute NAME
(`repr::infer_float_attrs`): `x` is a float field only if every assignment
to any `.x`, on `self` or any other object, is provably a float. One int
(`self.x = 0`) keeps the field boxed, because a float slot would print
`0.0` where CPython prints `0`. So the pass changes representation only,
never what a program prints, like the slot inference for locals. The proof
is circular on purpose (`self.x = self.x + self.v` is a float only if `x`
and `v` are): it assumes every candidate is a float field and removes any
name with a write that isn't, until nothing changes.

A name can't be a float field if any class has a class variable of that
name (an unset slot there means "read the class variable"), or if a module
exports it (`math.pi`, `robot.tilt`); so an attribute read never needs to
know what kind of thing its receiver is. Typing by name rather than by
class is what makes an access from outside the class (`def nudge(b):
b.x = b.x + 0.25`) sound without knowing `b`'s class: every object with an
`x` stores it the same way.

**Where they live.** Native: raw `f64`s after the value slots, the base's
first, the two counts packed in one header word; only value slots are
released on free. WASM: a second array, `$FITEMS`. An unset field holds a
NaN bit pattern no arithmetic produces (`0x7ff8dead00000000`), checked on
read, so reading a field `__init__` set only on some paths is still
CPython's AttributeError.

**How they flow.** A read of a float field is a proven float for the shared
inference (recorded under `repr::float_attr_key` in the map
`infer_expr_repr` already takes), so `self.x + self.v` is raw arithmetic and
`t = self.x` gives `t` a raw slot. In a method that never rebinds `self`,
access is by constant index; elsewhere by attribute id through
`p2w_getattr_f` / `$obj_getattr_f`. A dynamic sink (`print`, a list, an
untyped parameter) boxes as for any raw float.

**Measured** on the same step program (3 million calls):

| | before (slots, boxed floats) | after (float fields) |
|---|---|---|
| native allocations | 3,000,005 | 3 |
| native, median of nine | 401 ms | 275 ms (start-up is about 27 ms of each) |
| WASM (wasmtime runner), median of seven | 7,452 ms | 5,050 ms |

Same output on CPython, WASM and native; `live=0`. What remains in that
loop is the method call and the boxed `self.n + 1`: step 4.

## Plan for the rest

| Step | Work | Gate |
|---|---|---|
| 2b | The checker types objects, so attribute assignments outside methods check against the object's own class rather than "some class has it" | a typo through a typed variable (`r.speeed = 5` after `r = Robot()`) refused naming `Robot`; the class probes and oracle programs unchanged; zero backend divergence |
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
