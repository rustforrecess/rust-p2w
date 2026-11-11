# Units as casts — proposal

**Status: PROPOSAL, not built (2026-11-08).** Nothing here is implemented.
Every "p2w today" claim below was checked by running the program on p2w and
on CPython 3 the same day.

## The proposal in one paragraph

A unit name converts its argument into that unit, the way `int()` and
`float()` already do. `inches(12)` makes a distance; `feet(d)` shows the same
distance in feet; `float(...)` gets a plain number back. Distances compare
and add correctly whatever unit each was made in. There is exactly one way to
write a unit, following Go's habit of one obvious style. Angles work the same
way. The robot module's `distance()` and `tilt()` return these values, so a
student's first contact with units is a sensor reading that knows what it
measures.

```python
import robot
from robot import inches, feet, cm

d = robot.distance(0)        # a distance
print(feet(d))               # 0.98 feet
print(feet(inches(12)))      # 1.0 feet
print(inches(12) < cm(50))   # True: lengths compare across units
x = float(inches(d))         # a plain number again: 11.81

if d < inches(12):
    robot.set_motor(100, 100)
```

## Why

- **Kids think "12 inches", not "12".** A bare `300` from a distance sensor
  hides the question every science lesson asks: 300 of what?
- **Mixing units is a real bug class, not a toy one.** Adding millimetres to
  inches gives a wrong number with no error in plain floats.
- **Casts are already known.** `int("12")` and `float(x)` are in the first
  weeks of any Python course. Units add no new syntax and no new idea: a unit
  is one more thing you can convert to.
- **It is valid Python.** The same source runs under CPython with the same
  answers, as every p2w program must (`SUBSET_POLICY.md`).

## How standard practice does it

Both Python and Rust land on the same design: the value carries its unit,
stores one base unit inside, and conversion names only the unit you want.

| | Python | Rust |
|---|---|---|
| In the standard library | `datetime.timedelta`: build in any unit, stored as one base unit, compares and adds, `.total_seconds()` out | `std::time::Duration`: `from_millis`, `as_secs_f64` |
| The de facto units library | `pint`: `3 * ureg.meter`, `.to("inch")` | `uom`: `Length::new::<millimeter>(300.0)`, `.get::<inch>()` |
| A unit mistake is caught | when the program runs (`DimensionalityError`) | when it compiles |
| Runtime cost | one object per value | none: newtypes are erased after type checking |

We can take the semantics from Python and the cost model from Rust, because
we own the compiler: a class that runs identically in CPython, which the
checker understands and the native backend can store as a bare number.

Two styles were considered and rejected in favour of one:

| Style | Example | Why not |
|---|---|---|
| Keyword constructor (timedelta) | `Distance(inches=12)` | wordy; a second spelling next to casts |
| Multiply by a unit (pint) | `12 * inches` | closest to "12 inches", but a second spelling; units-as-casts covers conversion too |

## Design

### The rule

- `inches(n)` with a number: a distance of `n` inches.
- `inches(d)` with a distance: the same length, shown in inches.
- `float(d)`: the number in the unit `d` is shown in.
- `print(d)` / `str(d)`: the number and the unit, `1.0 feet`.

First units: `mm`, `cm`, `m`, `inches`, `feet` for distance; `degrees`,
`radians` for angles.

### Exact where it can be

Stored in millimetres, `feet(inches(12))` prints `0.9999999999999998 feet` in
CPython: 12 × 25.4 ÷ 304.8 rounds badly. Stored in **tenths of a
millimetre** (inch = 254, foot = 3048, mm = 10, cm = 100, m = 10000), whole
conversions between these units are exact: checked in CPython, 12 inches is
1.0 feet, 18 inches 1.5 feet, 1 foot 12.0 inches. Metric to imperial
(30 cm = 11.81 inches) is inexact by nature and prints as such.

### Unlike Go, a cast changes the number

In Go, `Feet(x)` on `type Feet float64` relabels: 12 inches becomes "12
feet", a known trap. Here a cast scales, which is what a student means by
"convert".

### Arithmetic

| Expression | Result |
|---|---|
| distance + distance, distance − distance | a distance, shown in the left operand's unit |
| distance × number, distance ÷ number | a distance |
| distance ÷ distance | a plain number (a ratio) |
| distance < distance, ==, … | compares lengths |
| distance × distance | an error for now: area is a later unit |
| distance + number, distance < number | an error with the student's words: "`d` is a distance; say which unit `12` is, like `inches(12)`" |

`inches(12) == 12` is `False` in CPython, not an error, so the checker must
flag it (below): the runtime alone would hide the mistake.

### The robot module

- `robot.distance(n)` returns a distance; it replaces `read_distance(n)`.
- `robot.tilt()` returns an angle.
- **Tilt states are angles.** `robot.Tilt.FLIPPED` *is* 150 degrees, not a
  label that holds it: anywhere code says `robot.Tilt.FLIPPED` it could say
  `degrees(150)` and mean the same thing. Python's pattern for this is the
  one `IntEnum` uses (members that are real ints): an enum with a data-type
  mixin. Python packages that combination as a single base name, `IntEnum`,
  so code never writes two bases; `robot` does the same with `AngleEnum`.

  ```python
  # in the robot module
  class Tilt(AngleEnum):
      LEVEL = 0
      TILTED = 30
      FLIPPED = 150

  # student code: each pair of lines is the same question
  if robot.tilt() >= robot.Tilt.FLIPPED:
  if robot.tilt() >= degrees(150):

  if robot.tilt_state() == robot.Tilt.FLIPPED:
  if robot.tilt_state() == degrees(150):

  print(robot.tilt_state())        # Tilt.FLIPPED
  ```

  Checked in CPython: `Tilt.FLIPPED == degrees(150)` is `True`, it passes
  `isinstance(..., Angle)`, it compares with any angle, it prints its name,
  and `Tilt(150)` looks the member up by angle. The thresholds and the states
  are one set of names; there is no separate `FLIPPED_DEG`.

  `robot.Tag` (FREE, TAGGED, IMMUNE) is a plain enum: a tag state isn't a
  measurement. So `Tag.IMMUNE == Tilt.FLIPPED` is `False`, and the checker
  can flag the comparison.

  **The one subtlety is hysteresis.** As an angle, FLIPPED is where the state
  begins. `tilt_state()` stays FLIPPED until the robot comes back below 140
  degrees, so between 140 and 150 on the way down,
  `robot.tilt() >= robot.Tilt.FLIPPED` is `False` while
  `robot.tilt_state() == robot.Tilt.FLIPPED` is still `True`. Both are right
  answers to different questions: "is the angle past 150 now?" against "is
  the robot in the flipped state?". Lessons should say so; the margin stays
  a robot setting, not an enum member.

- The host boundary does not change: values still cross as scalars, and the
  module's Python side wraps them. A test host still prints the same thing
  everywhere.

## What p2w is missing (measured 2026-11-08)

The cast sketch runs in CPython and fails on p2w at four points, and the
state enums add two more:

| Gap | p2w today | Needed for |
|---|---|---|
| `isinstance(x, C)` with a user class | "unknown name 'C'" | `feet(d)` telling a number from a distance |
| `__float__` dispatch | "expected a number, got 'D'" | `float(inches(d))` |
| `str(float)` | "str() of 'float' values isn't supported yet" | `__str__` printing `1.0 feet` |
| A module written partly in Python | host modules are native function tables only | shipping the classes |
| `class C(Enum)` | "unknown base class 'Enum'" | `robot.Tag`, and students' own state machines |
| `robot.AngleEnum`: an enum whose members are angles | not present | `robot.Tilt` being an angle; see below |

What already works and this builds on: user classes, `__lt__`/`__add__` and
the other dispatched dunders, `__str__` in `print`, and the host table
(`src/hostmod.rs`) that every surface derives from. The checker already types
robot calls from that table.

**No multiple inheritance.** In CPython, `AngleEnum` is simply
`class AngleEnum(Angle, Enum)`. p2w provides it built in instead, the way
Python provides `IntEnum`, and does not add general multiple inheritance
(today: "multiple inheritance isn't supported — one base class only"):

- `super()` would mean "next in the object's MRO", not "my parent"; p2w
  resolves it at compile time from the single base (`CLASSES_DESIGN.md`).
- Object layout is *not* the obstacle today: on both backends an instance
  is its class plus an attribute dict (WASM `$OBJECT`, native
  `[tag][rc][class_id][attrs]`). It would become one if the deferred
  `__slots__` pass gives classes fixed struct layouts, since a WASM-GC
  struct extends exactly one supertype.
- Dispatch would follow the C3 MRO, including diamonds, and the checker
  would have to reason about it.
- No early lesson needs it, and style guides steer even professionals to
  composition.

What `Tilt` needs is the mixin case only (`Enum` adds behaviour, no fields),
and one built-in base covers it. Student code writes one base, which p2w
already allows. A restricted multiple inheritance (one data base plus
mixins) is noted as a possible later feature in `CLASSES_DESIGN.md`; it
would let the CPython spelling compile but would not replace `AngleEnum`,
since an enum whose members are angles is Enum's own machinery, not
inheritance.

The first three, and `Enum`, are general Python features; every program gains from them,
not just units. The fourth is new: a `robot` module whose Python file ships
with the compiler. That same file becomes the CPython stub
(`acornstem/stdlib/robot`), which is currently the obsolete June draft, so
one file serves both and cannot drift.

## Plan

Each step is tested against CPython on WASM, native and the debugger, as the
rest of the language is.

1. **`str(float)`, `__float__`, `isinstance` with classes, `Enum`.**
   General features; gate: probes in the shared matrix agree with CPython, zero
   backend divergence.
2. **Python-written modules.** `robot` = a Python file plus the host table.
   Gate: `import robot` behaves identically before and after on every
   surface, and CPython runs the same file.
3. **`Distance` and `Angle` with casts, `robot.Tilt` and `robot.Tag`
   enums**; `robot.distance()` and `robot.tilt()` return the unit types and
   `tilt_state()` / `tag_state()` the enum members. Gate: the examples in this document print
   the same on p2w and CPython.
4. **The checker learns the types.** `d < 12`, `inches(12) == 12` and
   `distance + angle` are flagged before running, with the provenance
   ledger's wording ("`d` is a distance because line 3 read the sensor").
5. **Erasure on the board, if measured.** One object per sensor read every
   10 ms is probably fine on a Pico 2 W; if it isn't, the native backend
   stores a statically known distance as a bare number, as Rust's newtypes
   are. Measure first.

Steps 1 and 2 are worth doing even if units stall. Step 4 is where the
teaching value is, and depends on step 3.

## Risks and open questions

- **Cost on the board.** Unmeasured. Step 5 is the answer if needed.
- **Dyn programs.** A value the checker can't type (a parameter with no
  annotation) gets runtime errors only. The error text must name the unit
  question, not say "AttributeError: 'int' object has no attribute 'mm'".
- **Thresholds.** Settled: `robot.Tilt.FLIPPED` is an angle, so states and
  thresholds are one set of names (above).
- **A shorter name?** `robot.FLIPPED` as an alias would read better but give
  each state two names. Undecided.
- **Temperature.** `read_temperature()` is a candidate, but Celsius to
  Fahrenheit is not a scale (it has an offset), and adding two temperatures
  is meaningless. Leave it out of the first slice.
- **Student-defined units** (`class Mph`?) wait for a lesson that needs them.
- **One style vs. Python's variety.** `math.degrees()`, `timedelta` and
  `pint` stay valid Python; p2w doesn't reject them by policy. The house
  style teaches one way and the lints can suggest it.

## Not in scope

Areas, speeds (distance ÷ time), user-defined dimensions, and a general
dimensional-analysis engine. Each is a later unit with its own lesson.
