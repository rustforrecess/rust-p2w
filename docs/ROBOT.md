# The `robot` module — design

Status (2026-11-08): steps 1–3 of the order of work are built: the API
below on every backend, the browser simulator, `every` and the `on_`
subscriptions on the board scheduler. No motors or sensors yet: on the board
they are stand-ins. Originally written 2026-11-07; supersedes the June draft in
`acornstem/stdlib/robot/`, which was written for a compiler layout that was
never built (`__hal_call__`, an embassy HAL). Its function names are kept
where they still fit.

## What it is for

One 150 g robot design for a whole class, under $80, programmed in p2w. The
first games are sumo with judo-style scoring, tag, and human/code handover.
Every program runs in the browser simulator and on a Raspberry Pi Pico 2 W
unchanged: **both targets or neither**, as for the rest of the language.

## Programs are callbacks, not loops

A robot program registers callbacks and returns. The host — the browser
simulator or the board firmware — then calls them. There is no `while True`
loop and no blocking delay.

```python
import robot

def drive():                      # a timer callback
    if robot.read_line(0):        # ring edge: most important rule first
        robot.set_motor(-60, -60)
    elif robot.read_distance(0) < 300:
        robot.set_motor(100, 100)
    else:
        robot.set_motor(40, -40)

def tilt_changed():               # an event callback
    if robot.tilt_state() == robot.FLIPPED:
        print("help!")

every(50, drive)
robot.on_tilt(tilt_changed)
```

Why:

- **The browser cannot block.** Run calls the program's start function
  directly; a `while True` loop would hold the tab. Callbacks need no worker,
  no special headers and no interpreter.
- **It is how industry robots work.** A ROS 2 node registers timer and
  subscription callbacks and an executor calls them. `every` is what ROS calls
  a timer; `robot.on_tilt` is a subscription. Lessons name ROS's names when
  the concept is taught (see `ROS.md`); nothing here depends on ROS.
- **It already half exists.** `every(ms, handler)` and the `on_` handlers are
  existing builtins in the browser. `every` is reused as-is rather than adding
  a second `robot.every`.

## Design patterns, named for lessons and lints

| Pattern | Where | Kid version |
|---|---|---|
| Observer (publish/subscribe) | `robot.on_tilt`, `robot.on_tag` | "Tell me when this changes" |
| Reactor / event loop | the host calling callbacks | "Don't call us, we'll call you" |
| Timer | `every(50, drive)` | "Do this every 50 ms" |
| State machine | `tilt_state()`, `tag_state()`; a game's hunting, fleeing, frozen, immune | "What mode am I in?" |
| Strategy | human/code handover | "Who is driving right now?" |
| Subsumption (layered behaviours) | edge check before attack before search | "Most important rule wins" |
| Arbiter / guard | firmware rules over student commands | "Rules your code can't break" |

Subsumption order is checkable. A lint can notice that an edge check comes
after an attack rule, so the attack always wins and the robot drives off the
ring, and say so about the student's own code.

## API (first slice)

Units are for people, not physics: speed −100..100, distances in mm,
angles in degrees.

| Call | Returns | Notes |
|---|---|---|
| `robot.set_motor(left, right)` | — | −100..100 each; the arbiter may limit or zero it |
| `robot.stop()` | — | both motors to 0 |
| `robot.read_distance(n)` | `int` mm | distance sensor `n` (time-of-flight) |
| `robot.read_line(n)` | `bool` | line sensor `n` sees the ring edge |
| `robot.tilt()` | `float` degrees | angle between the robot's up and real up: 0 level, 180 upside down |
| `robot.TILTED_DEG`, `robot.FLIPPED_DEG` | `float` | threshold angles, starting at 30 and 150 |
| `robot.tilt_state()` | `int` | `robot.LEVEL`, `robot.TILTED` or `robot.FLIPPED` |
| `robot.tag_state()` | `int` | `robot.FREE`, `robot.TAGGED` (frozen by an IR tag) or `robot.IMMUNE` |
| `robot.on_tilt(fn)` | — | `fn` runs whenever `tilt_state()` changes |
| `robot.on_tag(fn)` | — | `fn` runs whenever `tag_state()` changes |
| `every(ms, fn)` | — | existing builtin; minimum interval 10 ms |

`read_ir` from the June draft is renamed `read_line`: "IR" now also means IR
tag.

### Measurements and states

A sensor a game cares about has two faces. `tilt()` is the **measurement**,
a float in degrees; `tilt_state()` is the **state** the host classified it
as, one of a few named ints. Measurements are compared with `>=` against
the `_DEG` thresholds; states are compared with `==`:

```python
if robot.tilt_state() == robot.FLIPPED:
    print("help!")
elif robot.tilt_state() == robot.TILTED:
    robot.set_motor(100, 100)        # push back while lifted

if robot.tilt() >= robot.FLIPPED_DEG:   # the same question, asked of the angle
    print("help!")
```

The thresholds are ranges, not exact angles: a flipped robot reads 179.4 or
176.8, almost never 180.0. The firmware's scoring uses the same states, so a
student's code and the referee agree on what "flipped" means.

A state is entered at its threshold and left only 10 degrees below it
(hysteresis), so a reading wobbling around 150 is one flip, not a state
change every 10 ms. States are what the host last classified, not a fresh
reading: inside a handler, `tilt_state()` is the state the handler was
called for.

**The floating-point lint.** `robot.tilt() == robot.FLIPPED_DEG`, or
`== 180`, is almost never true, the same trap as comparing any float with
`== 0.0`. A lint flags `==` and `!=` on a float measurement and says so about
the student's own line: a robot's angle is almost never exactly 180; `>=`
means "at least this far over", and `tilt_state() == robot.FLIPPED` is the
exact question. The same lint covers floats generally, so the lesson is
"measurements are never exact, states are", not a robot special case.

States are plain ints, as in a C enum: `robot.FLIPPED` is 2 and so is
`robot.IMMUNE`, so comparing a tilt state with a tag state is a mistake
Python won't catch. A lint for it waits until a lesson shows the need.

Later slices: `robot.position(id)` and `robot.heading()` from the overhead
camera, `robot.mode()` for handover, a servo for flippers.

### Subscriptions: when an `on_` handler runs

One rule for every `on_` handler: it runs **whenever its state changes**,
and reads the new state itself. `on_tilt` runs on LEVEL to TILTED, TILTED to
FLIPPED, FLIPPED back to LEVEL and every other change; `on_tag` runs on each
change of `tag_state()`. A state that lasts runs nothing more: a robot lying
upside down for three seconds is one call. Several handlers on one state all
run, in the order they were registered.

```python
def tag_changed():
    if robot.tag_state() == robot.TAGGED:
        print("frozen")
    elif robot.tag_state() == robot.IMMUNE:
        print("safe for 3 s")

robot.on_tag(tag_changed)
```

A new sensor adds a measurement, a state, and an `on_` subscription of the
same shape, rather than one subscription per interesting event.

Hosts classify every 10 ms, after the `every` timers due at that moment. The
rule lives once per host kind: `rust_p2w::sim::Subscriptions` in the
simulator and `poll_subscriptions` in `device/boot.c` on the board. A test
keeps the board's thresholds, hysteresis and state numbers equal to
`src/hostmod.rs`.

The handler must be the name of a function with no arguments, as for
`every`; it crosses the host boundary as the same dispatch id. Hosts
without hardware (the debugger, test runners) accept the subscription and
never fire it, since their stand-ins never tip or tag. The flat simulator
does not tip a robot and has no IR tag yet, and the board's tilt is a
stand-in until the IMU is wired, so for now no host changes a state.
`TAGGED` and `IMMUNE` arrive with the IR tag, when the arbiter's tag rules
(below) produce them.

## The arbiter: rules the student's code cannot break

The arbiter sits between `set_motor` and the motors, in the browser simulator
and in the board firmware. Student code keeps running; only motion is
overridden. Every number is a setting, tuned in play.

| Rule | Starting value |
|---|---|
| Motor power cap (a full 2S battery on 6 V motors) | 70% |
| IR tag freezes motion while the beam is received | up to 10 s |
| Gap in the beam still counted as tagged | 0.2 s |
| Immunity after a tag releases | 3 s |
| Signal or power loss | stop |

Tilt is measured by the firmware, not reported by student code, so a
program cannot claim to be upright. Scoring for lifts and flips is the game
layer's job and reads these states.

## Two hosts

**Browser simulator.** A 2D arena with robots, the same arbiter, and its own
clock. The simulator advances time in fixed steps and calls due callbacks,
rather than using `setInterval`, so a run replays exactly and a test can step
time without waiting. Sensors read the simulated world.

**Board firmware.** `boot.c` (or its successor) becomes the executor: after
`main` registers callbacks it runs a scheduler forever, calling due timers
and pending events, with the arbiter applied before any motor write. This
needs callback registration in the native backend, which today rejects
`on_click` and has no `every`.

Pin plan (provisional, checked against the RP2350 datasheet before any
wiring):

| Use | Pins |
|---|---|
| UART debug (keep free) | GP0, GP1 |
| Motor driver (DRV8833, PWM) | GP6–GP9 |
| I2C: IMU and distance sensor | GP4 (SDA), GP5 (SCL) |
| Line sensors | GP26, GP27 |
| IR tag receiver | GP10 |

## Safety

From `robot-magnet-safety` in the design notes: plain enclosed N20 motors, no
magnetic encoders, no exposed or loose magnets, no magnets as a build part.
Motor and battery voltage stay off any breadboard. Robots stay out of pockets
and away from chests and heads, taught to everyone so nobody is singled out.

## Order of work

1. The language surface: `import robot` accepted, types known to the checker,
   the names known to lints and the blocks toolbox, stubs on every backend.
2. The browser simulator with the arbiter, deterministic clock and one arena.
3. Native callbacks and the firmware scheduler; `every` on the board, checked
   over UART timestamps on the existing Pico.
4. Real motors, sensors and the IMU once parts arrive.
5. IR tag, the overhead camera and radio.

## Open questions

- How a program running on a board without a given sensor reports it (an
  error at startup is the current lean).
- Tag rules, scoring thresholds and the spinner rule: decided in play.
- `import robot` under CPython: the stub in `acornstem/stdlib/robot` must
  match this API so programs stay valid Python.
