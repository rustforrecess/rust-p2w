# Classes — design (implemented)

Status: **v1 implemented** (5 slices, all green vs CPython). This ports the
reference p2w object model into rust-p2w, adapted to the infrastructure we
already have. Reviewed and adjusted here *before* codegen, per the "scout the
hard thing carefully" plan; kept as the record of what shipped.

Shipped, slice by slice: (1) core — `$CLASS`/`$OBJECT`/`$METHOD`/`$MFUNC`
types, two-pass registration, method codegen + arg-list unpack, construction,
attr get/set via `$DICT`, `call_ref` dispatch, default `<Name object>` print;
(2) single inheritance + `super()` (compile-time resolution from the enclosing
class's base via `$dispatch_from`); (3) `__repr__`/`__str__` in print
(`$object_display`); (4) operator dunders — `__add__`/`__sub__`/`__mul__`,
`__eq__` (reflected + identity fallback), `__lt__`/`__le__`/`__gt__`/`__ge__`,
`__len__`, `__getitem__`; (5) class variables (instance → class namespace
fallback; reading a method as a value is a clean error).

## Guiding principle

Port the reference's *proven* representation and semantics; adapt only where our
existing infrastructure lets us reuse instead of rebuild, or where an enabled
WASM feature gives a cleaner mechanism. Innovate on the product, not the object
model.

## What we reuse vs. what's new

p2w hand-rolled a lot of machinery (PAIR cons-cells for attrs/methods/args, a
CLOSURE type, `call_indirect` through a function table) because, when classes
were added, it didn't have a dict/list to lean on. **We do.** So:

| Need | p2w | rust-p2w (this proposal) |
|---|---|---|
| instance attributes | PAIR-chain, linear scan | **reuse `$DICT`** (name→value) |
| class method table | PAIR-chain | **reuse `$DICT`** (name→`$METHOD`) |
| method arguments | PAIR-chain | **reuse `$LIST`** |
| method dispatch | `call_indirect` + func table | **`call_ref`** (typed function references — already enabled in the harness via `wasm_function_references(true)`) |
| top-level `def` | everything is a CLOSURE | **unchanged** — keep the existing direct-call positional path; only *methods* use the indirect path |

The single genuinely-new mechanism is **`call_ref` / typed function references**
(rust-p2w has only ever emitted direct `call $f_name`). It's a well-defined
WASM-GC feature, already on in our runtime config, and the differential harness
validates it. Everything else is `$DICT`/`$LIST` reuse or mirrors existing
function codegen.

## Runtime types (new)

```wat
;; uniform method signature so methods can be dispatched indirectly
(type $MFUNC (func (param $self (ref null eq)) (param $args (ref null eq))
                   (result (ref null eq))))
;; a method = a boxed function reference (so it can live in a $DICT value slot)
(type $METHOD (struct (field $fn (ref $MFUNC))))
;; the class object: name + method table + single base (eqref, like p2w)
(type $CLASS (struct (field $name (ref $STR))
                     (field $methods (ref $DICT))      ;; name -> $METHOD
                     (field $base (ref null $CLASS))))
;; an instance: class pointer + attribute dict
(type $OBJECT (struct (field $class (ref $CLASS))
                      (field $attrs (ref $DICT))))     ;; name -> value
```

Instances are `(ref null eq)` like every other value — they flow through the
same boxed pipeline as ints/lists/dicts, so `print`, variables, lists-of-objects,
etc. all work for free.

## Operations

- **Class definition** → emit each method as an `$MFUNC` function
  (`$m_<Class>_<method>`), build the class's methods `$DICT`, and create a module
  global `$g_class_<Name>` holding the `$CLASS` struct (name, methods, base).
  Registered in pass 1 alongside functions so construction/dispatch resolve.
- **Construction `Cls(args)`** → `struct.new $OBJECT` (class ref + fresh empty
  `$DICT`), look up `__init__` in the class chain, `call_ref` it with
  `self` + an `$LIST` of args, drop its result, yield the instance.
- **Attribute read `obj.attr`** → `dict_get(obj.attrs, "attr")`; on miss, raise
  `AttributeError` (instance-attrs only in v1 — see Deferred).
- **Attribute write `obj.attr = x`** → `dict_set(obj.attrs, "attr", x)`.
- **Method call `obj.method(args)`** → walk `obj.class` → `base` → … doing
  `dict_get(methods, "method")` until found; `ref.cast` to `$METHOD`; build args
  `$LIST`; `call_ref` with `self`=obj. Miss → `AttributeError`. Arity mismatch
  (declared params minus `self` vs. supplied) → `TypeError`, at compile time when
  statically known, else a runtime check.
- **`self` and method bodies** → a method compiles like a function but with the
  `(self, args-list)` signature; its prologue unpacks the `$LIST` into the
  declared parameter locals. `self.x` is getattr/setattr on `self`.
- **`super().method(args)`** → compiled *directly*: resolve starting from the
  lexically-enclosing class's `base` (we know the current class at compile time),
  `call_ref` with the original `self`. No runtime `$SUPER` proxy needed because v1
  only supports the `super().method(...)` call form, not first-class super.
- **Printing** → `$print_value` gains an `$OBJECT` branch: look up `__repr__`
  (then `__str__`) in the class chain; if present, `call_ref` and print the
  returned `$STR`; else print the default `<Name object>` using
  `obj.class.name`.

## v1 scope (the OOP a beginner actually writes)

Single inheritance · `__init__` · instance attributes · **class variables** ·
methods · `self` · `super().__init__(...)`/`super().method(...)` ·
`__repr__`/`__str__` for printing · `Cls(args)` construction · **operators on
custom objects** — `__eq__`, `__add__`/`__sub__`/`__mul__`,
`__lt__`/`__le__`/`__gt__`/`__ge__`, `__len__`, `__getitem__`.

Rationale for including operators: the test for a *good* deferral is that its
absence is invisible or a clean error — never *silently wrong*. Identity-only
`==` fails that test (two equal-valued Points compare unequal — a footgun), and
the operator dunders as a group are cheap given dispatch (each is "from the
operator site, if the operand is an `$OBJECT`, run the lookup-and-dispatch we
already built") and central to the canonical lessons (Vector, Fraction, a
Deck with `len`/indexing). So they're v1, not phase 2.

That's a real, teachable OOP subset:

```python
class Animal:
    def __init__(self, name):
        self.name = name
    def speak(self):
        return self.name + " makes a sound"

class Dog(Animal):
    def __init__(self, name):
        super().__init__(name)
        self.tricks = []
    def speak(self):
        return self.name + " barks"
    def learn(self, t):
        self.tricks.append(t)

d = Dog("Rex")
d.learn("sit")
print(d.speak())      # Rex barks
print(d.tricks)       # ['sit']
```

## Deferred (later phases, each independently)

Each deferral below is *invisible* or a *clean error* in its absence — never
silently wrong (that's the line operator dunders failed, which is why they're
in v1 above).

- **`@staticmethod` / `@classmethod` / `@property`** — gated on decorator
  support (which we don't have at all yet) + the `$PROPERTY`/wrapper structs
  p2w uses; not a beginner's first reach.
- **First-class bound methods / closures-as-values / first-class `super`** —
  v1 only calls methods via `obj.method(args)` syntax; making a method a value
  you can store/pass needs the closure machinery we're otherwise avoiding.
- **`__slots__` optimization** (per-class typed struct, O(1) fields) — a perf
  pass; v1 uses the `$DICT` attrs uniformly (fine at classroom scale).
- **Multiple inheritance / MRO / metaclasses / descriptors** — p2w doesn't do
  these either; likely never for the K-12 scope.

## Two-color / RT interaction

Objects are heap-allocated (`$OBJECT` + `$DICT`), so they live in the **dynamic
tier only**. A `@realtime`/no-alloc function may not instantiate or use objects.
No redesign — this is the boundary we already drew, and it's a clean
compile-time rejection (good error: "objects aren't allowed in a @realtime
function").

## Visualizer payoff

`$OBJECT` carries its class name and a real attribute `$DICT`, so the
execution-visualizer can introspect an instance directly (class + live
attributes) for the notional-machine view. Falls out of the representation; not
a v1 requirement.

## Parser / AST additions

- `StmtKind::ClassDef { name, base: Option<String>, methods: Vec<Method> }`
  (methods carry the same data as `Def`).
- `StmtKind::SetAttr { obj, attr, value }` (the assignment path already parses
  expression-first, then branches on target shape — add the `Attr` target).
- `ExprKind::Attr(Box<Expr>, String)` — `obj.attr` (postfix `.name` *not*
  followed by `(`; we already special-case `.name(args)` as `MethodCall`).
- `super().method(args)` needs no keyword — it's a `MethodCall` whose receiver
  is `Call("super", [])`, detected in codegen.

## Test plan

Differential vs. CPython is ideal here (CPython has classes), so the corpus
covers: construction + `__init__`, attribute round-trip, method calls,
inheritance + overriding, `super().__init__`, `__repr__`/`__str__` printing,
lists/dicts of instances, instances passed to functions (reference semantics),
and the error paths (AttributeError on missing attr, TypeError on arity).

## Open questions for review

1. **Method arg convention.** Proposal uses a uniform `(self, args-list)`
   signature with the body unpacking the `$LIST`. Alternative: per-arity typed
   funcrefs (faster, no list build, but the method table can't be one `$DICT`
   of one `$METHOD` type). Recommendation: uniform list for v1 simplicity;
   revisit if dispatch shows up in profiles.
2. **(resolved)** `__eq__` and the operator dunders are in v1 (see scope
   rationale) — identity-only `==` was silently-wrong, so it's promoted.
3. **(resolved)** Class variables are in v1; attribute read falls back
   instance → class chain. Accessing a *method* as a value (`f = d.speak`)
   remains the deferred bound-method feature (clean error if attempted).
