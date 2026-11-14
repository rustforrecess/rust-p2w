//! The probe matrix, shared by the two documents generated from it:
//! `RUNTIME_SEMANTICS.md` (what the WASM-GC backend does) and
//! `BACKEND_DIVERGENCE.md` (where the linear-memory backend disagrees).
//!
//! One matrix, two backends. Every divergence found so far was found by
//! accident; the point of sharing the list is to find them on purpose.

#![allow(dead_code)]

use std::sync::OnceLock;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_gc(true);
        config.wasm_function_references(true);
        config.cranelift_opt_level(wasmtime::OptLevel::None);
        Engine::new(&config).expect("engine")
    })
}

/// What happened to one probe program.
pub enum Outcome {
    /// Compiled and ran; this is what it printed.
    Value(String),
    /// The compiler refused it. Already a compile-time error today.
    CompileError(String),
    /// It compiled, then trapped. The text is what the program printed first —
    /// our runtime writes the message and then executes `unreachable`.
    Trap(String),
}

pub fn probe(src: &str) -> Outcome {
    let wat = match rust_p2w::compile_to_wat(src) {
        Ok(w) => w,
        Err(e) => return Outcome::CompileError(e.to_string()),
    };
    let wasm = wat::parse_str(&wat).expect("invalid WAT");
    let module = Module::new(engine(), &wasm[..]).expect("module");
    let mut store: Store<Vec<u8>> = Store::new(engine(), Vec::new());
    let mut linker: Linker<Vec<u8>> = Linker::new(engine());
    define_host_stand_ins(&mut linker, engine());
    linker
        .func_wrap("env", "write_char", {
            // Fn+Send+Sync required: surrogate state as an atomic (0 = none).
            let hi = std::sync::atomic::AtomicU32::new(0);
            move |mut c: Caller<'_, Vec<u8>>, ch: i32| {
                // UTF-16 code units in (surrogate pairs across calls),
                // UTF-8 bytes out.
                let u = ch as u32 as u16;
                use std::sync::atomic::Ordering;
                let prev = hi.swap(0, Ordering::Relaxed);
                let cp: u32 = match (prev, u) {
                    (0, 0xD800..=0xDBFF) => {
                        hi.store(0x1_0000 | u as u32, Ordering::Relaxed);
                        return;
                    }
                    (p, 0xDC00..=0xDFFF) if p != 0 => {
                        let h = p & 0xFFFF;
                        0x10000 + (((h - 0xD800) << 10) | (u as u32 - 0xDC00))
                    }
                    (_, _) => u as u32,
                };
                let ch = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                let mut buf = [0u8; 4];
                c.data_mut()
                    .extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        })
        .unwrap();
    linker
        .func_wrap(
            "env",
            "write_str",
            |mut c: Caller<'_, Vec<u8>>, s: Option<wasmtime::Rooted<wasmtime::ExternRef>>| {
                let text = js_str(&mut c, &s);
                c.data_mut().extend_from_slice(text.as_bytes());
            },
        )
        .unwrap();
    linker
        .func_wrap("env", "write_i32", |mut c: Caller<'_, Vec<u8>>, v: i32| {
            c.data_mut().extend_from_slice(v.to_string().as_bytes());
        })
        .unwrap();
    linker
        .func_wrap("env", "write_f64", |mut c: Caller<'_, Vec<u8>>, v: f64| {
            let s = rust_p2w::py_float_repr(v);
            c.data_mut().extend_from_slice(s.as_bytes());
        })
        .unwrap();
    linker
        .func_wrap("env", "read_char", |_: Caller<'_, Vec<u8>>| -> i32 { -1 })
        .unwrap();
    // The per-attempt seed (42 everywhere in tests, so `import random`
    // probes pin one reproducible sequence).
    linker
        .func_wrap("env", "read_temperature", |_: Caller<'_, Vec<u8>>| -> f64 {
            rust_p2w::SIMULATED_TEMPERATURE_C
        })
        .unwrap();
    linker
        .func_wrap("env", "seed", |_: Caller<'_, Vec<u8>>| -> i32 { 42 })
        .unwrap();

    add_js_string_builtins(&mut linker);
    define_string_literals(&mut linker, &mut store, &module);
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => return Outcome::CompileError(format!("instantiate: {e}")),
    };
    let start = instance
        .get_typed_func::<(), i32>(&mut store, "_start")
        .expect("_start");
    let result = start.call(&mut store, ());
    let out = String::from_utf8_lossy(&store.into_data()).into_owned();
    match result {
        Ok(_) => Outcome::Value(out.trim_end().to_string()),
        Err(_) => Outcome::Trap(out.trim().to_string()),
    }
}

/// One row: the expression as written, wrapped in whatever makes it printable.
pub struct Probe {
    /// What to show in the document.
    pub shown: &'static str,
    /// The program actually compiled and run.
    pub src: &'static str,
}

const fn p(shown: &'static str, src: &'static str) -> Probe {
    Probe { shown, src }
}

/// `print(<expr>)` for the common case where the shown text IS the expression.
macro_rules! e {
    ($x:expr) => {
        p($x, concat!("print(", $x, ")\n"))
    };
}

pub struct Group {
    pub title: &'static str,
    /// Why a type-system designer should care about this group.
    pub note: &'static str,
    pub probes: Vec<Probe>,
}

pub fn groups() -> Vec<Group> {
    vec![
        Group {
            title: "Arithmetic on numbers",
            note: "`int` is **32-bit**, not 64. Arithmetic that leaves that \
                   range now TRAPS on every surface — WASM, the native \
                   runtime, and the Stepper's interpreter — rather than \
                   wrapping silently and printing a wrong answer. CPython has \
                   arbitrary-precision ints, so this is still a divergence, \
                   but a loud one; widening the value model is separate work \
                   tied to the memory model. Otherwise: mixed int/float \
                   promotes and `/` is always float division, both CPython's \
                   rules, both must survive typing.",
            probes: vec![
                e!("1 + 2"),
                e!("2 - 5"),
                e!("3 * 4"),
                e!("7 / 2"),
                e!("7 // 2"),
                e!("-7 // 2"),
                e!("7 % 2"),
                e!("-7 % 2"),
                e!("2 ** 10"),
                e!("1 + 2.5"),
                e!("3 * 1.5"),
                e!("2147483647"),
                e!("2147483648"),
                e!("2147483647 + 1"),
                e!("1000000 * 1000000"),
            ],
        },
        Group {
            title: "Floats",
            note: "Full fractional `**` and math.exp/log/log2/log10/pow, \
                   via libm compiled to WAT (src/math_wat.rs) — the same \
                   pinned libm the native runtime links, so both backends \
                   agree to the LAST BIT. `x ** 0.5` stays one f64.sqrt \
                   instruction. Known bounded caveat: CPython defers to \
                   the PLATFORM libm, so a last-ULP digit can differ from \
                   a given host's CPython (exp(1.0) on Windows ucrt) — \
                   exactly as two CPythons differ from each other.",
            probes: vec![
                e!("2.5 + 1.5"),
                e!("2.5 * 2.0"),
                e!("2.5 - 1.0"),
                e!("5.0 / 2.0"),
                e!("2.0 ** 2"),
                e!("2 ** 0.5"),
                e!("9 ** 0.5"),
                e!("2.25 ** 0.5"),
                // A computed exponent, not a literal — the check is at runtime.
                p("h = 0.5; 16 ** h", "h = 0.5\nprint(16 ** h)\n"),
                // The remaining honest gap: everything that is not a half.
                e!("8 ** 0.3333"),
                e!("2 ** 2.5"),
                e!("2 ** -1.5"),
                p("math.exp(1.0)", "import math\nprint(math.exp(1.0))\n"),
                p(
                    "read_temperature() [simulated]",
                    "print(read_temperature())\n",
                ),
                p(
                    "robot module [stand-ins]",
                    "import robot\nrobot.set_motor(50, -50)\nprint(robot.read_distance(0))\nprint(robot.read_line(1))\nprint(robot.tilt())\nprint(robot.tilt() >= robot.FLIPPED_DEG)\nprint(robot.FLIPPED_DEG)\nprint(robot.tilt_state() == robot.LEVEL)\nprint(robot.FLIPPED)\nprint(robot.tag_state() == robot.FREE)\nrobot.stop()\n",
                ),
                p(
                    "robot.on_tilt / on_tag [no events without hardware]",
                    "import robot\ndef tilt_changed():\n    if robot.tilt_state() == robot.FLIPPED:\n        print(\"help!\")\ndef tag_changed():\n    if robot.tag_state() == robot.TAGGED:\n        print(\"frozen\")\ndef setup():\n    robot.on_tag(tag_changed)\nrobot.on_tilt(tilt_changed)\nsetup()\nprint(\"waiting\")\n",
                ),
                p(
                    "random.randint(1, 6) [seed 42]",
                    "import random\nprint(random.randint(1, 6))\n",
                ),
                p("math.log(10.0)", "import math\nprint(math.log(10.0))\n"),
                p("math.log2(8.0)", "import math\nprint(math.log2(8.0))\n"),
                p(
                    "math.log10(1000.0)",
                    "import math\nprint(math.log10(1000.0))\n",
                ),
                p("math.pow(2, 10)", "import math\nprint(math.pow(2, 10))\n"),
                e!("2.0 ** 2.0"),
                e!("abs(-2.5)"),
                // fabs/floor/ceil/trunc, closed on native 2026-10-23 — the
                // first gap found in this session's parity audit (a
                // compile-time "doesn't handle math.{method}() yet" error,
                // not a trap — the mildest of the gaps closed this
                // session). floor/ceil/trunc return an int, not a float
                // (CPython's rule) — pinned on a negative input too, since
                // floor/ceil round toward DIFFERENT infinities there.
                p("math.fabs(-2.5)", "import math\nprint(math.fabs(-2.5))\n"),
                p("math.floor(2.7)", "import math\nprint(math.floor(2.7))\n"),
                p("math.floor(-2.7)", "import math\nprint(math.floor(-2.7))\n"),
                p("math.ceil(2.1)", "import math\nprint(math.ceil(2.1))\n"),
                p("math.ceil(-2.1)", "import math\nprint(math.ceil(-2.1))\n"),
                p("math.trunc(-2.9)", "import math\nprint(math.trunc(-2.9))\n"),
            ],
        },
        Group {
            title: "Division by zero",
            note: "Traps with a clear message. Proving these away needs \
                   interval analysis or an SMT solver; the message may simply \
                   be the right answer for a beginner.",
            probes: vec![e!("1 / 0"), e!("1 // 0"), e!("1 % 0"), e!("1.0 / 0.0")],
        },
        Group {
            title: "Booleans are integers",
            note: "CPython's rule, and it means a type system cannot treat \
                   `bool` as unrelated to `int`.",
            probes: vec![
                e!("True + True"),
                e!("True * 5"),
                e!("1 == 1.0"),
                e!("True == 1"),
            ],
        },
        Group {
            title: "Strings",
            note: "`+` joins and `*` repeats — which is exactly why `-` on \
                   strings surprises students: two of the three arithmetic \
                   operators they know do work. Ordering comparisons ARE \
                   supported and lexicographic, matching CPython — including \
                   the two results that surprise people: uppercase sorts \
                   before lowercase, and a prefix is smaller than the word it \
                   begins.",
            probes: vec![
                e!("'ab' + 'cd'"),
                e!("'ab' * 3"),
                e!("3 * 'ab'"),
                e!("'abc'[1]"),
                e!("len('abc')"),
                e!("'a' < 'b'"),
                e!("'pear' < 'apple'"),
                e!("'a' <= 'a'"),
                e!("'b' > 'a'"),
                e!("'b' >= 'c'"),
                // Uppercase sorts before lowercase (code-point order), and a
                // prefix is smaller than the word it starts — the two results
                // that surprise people, so they are pinned.
                e!("'Zoe' < 'amy'"),
                e!("'app' < 'apple'"),
                // Non-ASCII: len/index/iteration mean CODE POINTS (CPython's
                // rule). Until 2026-08-22 the three surfaces gave three
                // different answers here (UTF-16 units / UTF-8 bytes / code
                // points) — and no probe was non-ASCII, so nothing noticed.
                e!("len('café')"),
                e!("len('🦀')"),
                e!("'café'[3]"),
                e!("'a🦀b'[1]"),
                p("for c in 'héé'", "for c in 'héé':\n    print(c)\n"),
                e!("'ab' - 'b'"),
                e!("'ab' + 1"),
                e!("1 + 'ab'"),
                e!("'ab' / 2"),
                // Case/strip methods, closed on native 2026-10-23 (previously
                // native-only had no string methods at all — WASM compiled,
                // native hit a hard "method not supported" trap). ASCII-only
                // on every surface by design (ASCII case shift is a single
                // in-range byte check; a non-ASCII UTF-8 byte is always
                // >= 0x80 and never mistaken for one), which is why 'café'
                // comes back with its accent untouched.
                e!("'  Hello World  '.strip()"),
                e!("'  Hello World  '.lstrip()"),
                e!("'  Hello World  '.rstrip()"),
                e!("'Hello World'.upper()"),
                e!("'Hello World'.lower()"),
                e!("'hELLO wORLD'.capitalize()"),
                e!("'hELLO wORLD'.title()"),
                e!("'café'.upper()"),
                // split/join, closed on native 2026-10-23 — previously the
                // one method that didn't just trap on native: it hung
                // (spin-looped), the generic "unknown method" trap's own
                // device-emulation behavior, with zero output to explain why.
                e!("'  a  bb   c  '.split()"),
                e!("'a,,b,'.split(',')"),
                e!("'xy'.split('')"),
                e!("', '.join(['a', 'bb', 'c'])"),
                e!("'-'.join([])"),
                // find/count/index/startswith/endswith/isdigit/isalpha/
                // zfill/replace, closed on native 2026-10-23 — same shape
                // as split/join: these hung rather than erroring. `count`/
                // `index` are generic (list/tuple get them for free too,
                // via the same `$py_count`/`$py_index_of` dispatch WASM
                // already used — see the List methods group below).
                e!("'abcabc'.find('bc')"),
                e!("'abcabc'.find('zz')"),
                e!("'abcabc'.count('bc')"),
                e!("'abcabc'.index('ca')"),
                e!("'hello'.startswith('he')"),
                e!("'hello'.endswith('lo')"),
                e!("'123'.isdigit()"),
                e!("'abc'.isalpha()"),
                e!("'42'.zfill(5)"),
                e!("'-42'.zfill(5)"),
                e!("'abcabc'.replace('a', 'XY')"),
                p(
                    "'abc'.index('zz') [ValueError]",
                    "print('abc'.index('zz'))\n",
                ),
            ],
        },
        Group {
            title: "Where a wrong type is caught TODAY",
            note: "⭐ The core table. Every `trap` row is a runtime failure a \
                   type checker could turn into a compile error with a span. \
                   Every `compile error` row is one that already is.",
            probes: vec![
                p("age = '12'; age + 1", "age = '12'\nprint(age + 1)\n"),
                p("n = 5; n[0]", "n = 5\nprint(n[0])\n"),
                p("n = 5; len(n)", "n = 5\nprint(len(n))\n"),
                p("n = 5; n.append(1)", "n = 5\nn.append(1)\n"),
                p("n = 5; for x in n", "n = 5\nfor x in n:\n    print(x)\n"),
                p("x: int = 'no'", "x: int = 'no'\nprint(x)\n"),
                p(
                    "def f() -> int: return 'x'",
                    "def f() -> int:\n    return 'x'\nprint(f())\n",
                ),
                p(
                    "def f(n: int) called with str",
                    "def f(n: int) -> int:\n    return n\nprint(f('x'))\n",
                ),
                p("wrong arity", "def f(a, b):\n    return a\nprint(f(1))\n"),
                p(
                    "return on some paths only",
                    "def f(n):\n    if n > 0:\n        return 1\nprint(f(0))\n",
                ),
                p(
                    "retry loop: while True with the return inside",
                    "def pick() -> int:\n    while True:\n        n = 3\n        if n > 2:\n            return n\nprint(pick())\n",
                ),
                p("call a number", "total = 5\nprint(total(3))\n"),
                p(
                    "read before assignment",
                    "def f():\n    return q + 1\nprint(f())\n",
                ),
            ],
        },
        Group {
            title: "Lists",
            note: "Index errors are the classic runtime failure. `for i in \
                   range(len(xs))` is the shape interval analysis would need \
                   to recognise to prove them away without a solver.",
            probes: vec![
                e!("[1, 2, 3][0]"),
                e!("[1, 2, 3][-1]"),
                e!("[1, 2, 3][5]"),
                e!("[1, 2, 3] + [4]"),
                e!("[0] * 3"),
                e!("len([1, 2, 3])"),
                e!("[1, 2, 3][1:3]"),
                e!("[1, 2, 3][::2]"),
                e!("[1, 2, 3][::0]"),
                e!("[1, 'two', 3.0]"),
                p(
                    "mixed list, element used",
                    "xs = [1, 'two']\nprint(xs[0] + 1)\n",
                ),
                // count/index, closed on native 2026-10-23 — the SAME
                // py_count/py_index_of dispatch that closed str.count/
                // str.index (see the Strings group above).
                e!("[1, 2, 3, 2].count(2)"),
                e!("[1, 2, 3, 2].index(2)"),
                // sort/reverse/extend/insert, closed on native 2026-10-23 —
                // same shape as the earlier string-method closures: these
                // all mutate in place and return None, so the probe mutates
                // then prints, rather than printing the call itself.
                p(
                    "xs = [3, 1, 2]; xs.sort(); xs",
                    "xs = [3, 1, 2]\nxs.sort()\nprint(xs)\n",
                ),
                p(
                    "xs = [1, 2, 3]; xs.reverse(); xs",
                    "xs = [1, 2, 3]\nxs.reverse()\nprint(xs)\n",
                ),
                p(
                    "xs = [1, 2]; xs.extend([3, 4]); xs",
                    "xs = [1, 2]\nxs.extend([3, 4])\nprint(xs)\n",
                ),
                p(
                    "xs = [1, 2, 3]; xs.insert(1, 9); xs",
                    "xs = [1, 2, 3]\nxs.insert(1, 9)\nprint(xs)\n",
                ),
            ],
        },
        Group {
            title: "Dicts and sets",
            note: "`dict.get(k, default)` is the Pythonic answer that removes \
                   the need to catch KeyError — worth teaching before \
                   exceptions are even discussed.",
            probes: vec![
                e!("{'a': 1}['a']"),
                e!("{'a': 1}['b']"),
                e!("{'a': 1}.get('b', 0)"),
                e!("'a' in {'a': 1}"),
                e!("len({1, 2, 2})"),
                e!("{1, 2} | {3}"),
                e!("{1, 2} & {2, 3}"),
                // update/clear/setdefault/keys/values/items, closed on
                // native 2026-10-23 — same shape as the list/string method
                // closures above: these used to hang on native, not error.
                p(
                    "d={'a':1}; d.update({'b':2}); d",
                    "d = {'a': 1}\nd.update({'b': 2})\nprint(d)\n",
                ),
                p(
                    "d={'a':1}; d.clear(); d",
                    "d = {'a': 1}\nd.clear()\nprint(d)\n",
                ),
                p(
                    "d={'a':1}; d.setdefault('b',2); d",
                    "d = {'a': 1}\nd.setdefault('b', 2)\nprint(d)\n",
                ),
                e!("{'a': 1, 'b': 2}.keys()"),
                e!("{'a': 1, 'b': 2}.values()"),
                e!("{'a': 1, 'b': 2}.items()"),
            ],
        },
        Group {
            title: "Conversions",
            note: "The `input()` -> `int()` path is where beginners meet types \
                   whether or not the language has them.",
            probes: vec![
                e!("int('42')"),
                e!("int('abc')"),
                // The value model is i32: a parse that doesn't fit is a LOUD
                // OverflowError on every surface (it silently wrapped once).
                e!("int('99999999999')"),
                e!("int('2147483648')"),
                e!("int('-2147483648')"),
                e!("int(3.9)"),
                e!("int(-3.9)"),
                e!("float('1.5')"),
                e!("float('abc')"),
                e!("str(42)"),
                e!("round(2.5)"),
                e!("round(3.5)"),
            ],
        },
        Group {
            title: "Truthiness and comparison across types",
            note: "Comparing unlike things is where Python's own rules are \
                   least obvious, so it is where a checker's message matters \
                   most.",
            probes: vec![
                e!("bool(0)"),
                e!("bool('')"),
                e!("bool([])"),
                e!("1 == 'one'"),
                e!("1 < 'one'"),
            ],
        },
        Group {
            title: "Unpacking",
            note: "A length mismatch is checkable statically whenever both \
                   sides are literals.",
            probes: vec![
                p("a, b = 1, 2", "a, b = 1, 2\nprint(a + b)\n"),
                p("a, b = 1, 2, 3", "a, b = 1, 2, 3\nprint(a)\n"),
                p("a, b = [1, 2]", "a, b = [1, 2]\nprint(a + b)\n"),
            ],
        },
        Group {
            title: "First-class functions (monomorphic case)",
            note: "A function value may only ever come from directly naming \
                   a known top-level `def` (never computed), so `fnval.rs` \
                   resolves every binding that provably holds exactly one \
                   function to a direct call at compile time, before either \
                   backend runs — neither sees anything but ordinary calls.",
            probes: vec![
                p(
                    "g = double; g(7)",
                    "def double(x):\n    return x * 2\ng = double\nprint(g(7))\n",
                ),
                p(
                    "apply(double, 5)",
                    "def double(x):\n    return x * 2\ndef apply(f, x):\n    return f(x)\nprint(apply(double, 5))\n",
                ),
            ],
        },
        Group {
            title: "sorted(xs, key=...)",
            note: "Decorate-sort-undecorate, built as ordinary synthesized \
                   Python AST (list literal, `.append`, `for`) run through \
                   the normal statement compiler on both backends — the \
                   sort-by-key step itself is the one new runtime primitive \
                   (`$py_sorted_by_key` / `p2w_sorted_by_key`), which \
                   mutates and returns the SAME pairs list, mirroring \
                   `sorted`'s own separation from its callers.",
            probes: vec![
                p(
                    "sorted(xs, key=len)",
                    "words = ['kiwi', 'fig', 'banana', 'ox']\nprint(sorted(words, key=len))\n",
                ),
                p(
                    "sorted(xs, key=len, reverse=True)",
                    "words = ['kiwi', 'fig', 'banana', 'ox']\nprint(sorted(words, key=len, reverse=True))\n",
                ),
                p(
                    "sorted(xs, key=<user def>)",
                    "def neg(x):\n    return 0 - x\nnums = [3, 1, 4, 1, 5]\nprint(sorted(nums, key=neg))\n",
                ),
            ],
        },
        Group {
            title: "Object attributes in fixed slots",
            note: "An object's attributes are fixed by its class \
                   (docs/CLASS_LAYOUT.md), so each has a slot: inherited ones \
                   at the base's index in every subclass, `self.x` in a method \
                   read and written directly, other objects through the \
                   attribute's id. A class variable set through an instance \
                   shadows it for that instance only.",
            probes: vec![
                p(
                    "subclass, class-variable shadow, set from outside",
                    "class Base:\n    count = 10\n    def __init__(self):\n        self.a = 1\n    def bump(self):\n        self.count = self.count + 1\n        return self.count\nclass Kid(Base):\n    def __init__(self):\n        super().__init__()\n        self.b = 2\n    def total(self):\n        return self.a + self.b\ndef poke(o):\n    o.a = 50\n    return o.a\nk = Kid()\nprint(k.total())\nprint(k.bump(), k.bump(), Base.count)\nprint(poke(k), k.total())\nother = Base()\nprint(other.count, other.a)\n",
                ),
                p(
                    "a subclass's class variable read through a base method",
                    "class M:\n    def __init__(self):\n        self.speed = 1\n    def get(self):\n        return self.speed\nclass Fast(M):\n    speed = 50\n    def __init__(self):\n        pass\nprint(M().get(), Fast().get())\n",
                ),
                p(
                    "a comprehension variable named self",
                    "class A:\n    def __init__(self, n):\n        self.m = n + \"-m\"\n        self.n = n + \"-n\"\n    def names(self, others):\n        return [self.n for self in others]\nclass Z:\n    def __init__(self):\n        self.a = \"z-a\"\n        self.m = \"z-m\"\n        self.n = \"z-n\"\nprint(A(\"A\").names([Z()]))\n",
                ),
                p(
                    "the value is evaluated before the object",
                    "class A:\n    def __init__(self):\n        self.x = 0\ng = A()\nh = g\ndef f():\n    global g\n    g = A()\n    return 5\ng.x = f()\nprint(g.x, h.x)\nxs = [1, 2, 3]\ndef k():\n    global xs\n    xs = [9, 9, 9]\n    return 7\nxs[0] = k()\nprint(xs)\n",
                ),
                p(
                    "attributes as unpacking targets",
                    "class P:\n    def __init__(self, a, b):\n        self.a, self.b = a, b\n    def swap(self):\n        self.a, self.b = self.b, self.a\np = P(\"x\", [1])\np.swap()\nprint(p.a, p.b)\n",
                ),
                p(
                    "__init__ calls a helper that sets attributes",
                    "class R:\n    def __init__(self):\n        self.reset()\n    def reset(self):\n        self.speed = 0\n        self.heading = 0\n    def go(self):\n        self.speed = 1\n        return self.speed\nr = R()\nprint(r.go(), r.heading)\n",
                ),
            ],
        },
        Group {
            title: "Float fields",
            note: "An attribute whose every assignment in the program is \
                   provably a float is stored raw in the object \
                   (docs/CLASS_LAYOUT.md, step 3), so `self.x + self.v` \
                   allocates nothing. One int write keeps a field boxed \
                   (`mixed` below prints 0, then 1.0), so only the \
                   representation changes, never what a program prints.",
            probes: vec![p(
                "float fields, a mixed field, a subclass, access from a function and a loop",
                "class Body:\n    def __init__(self):\n        self.x = 0.0\n        self.v = 1.5\n        self.n = 0\n        self.mixed = 0\n    def step(self):\n        self.x = self.x + self.v\n        self.n = self.n + 1\n        self.mixed = self.mixed + 0.5\n    def speed_up(self, by: float):\n        self.v = self.v * by\n    def where(self):\n        t = self.x\n        return t / 2.0\n\nclass Rocket(Body):\n    def __init__(self):\n        super().__init__()\n        self.fuel = 10.0\n    def burn(self):\n        self.fuel = self.fuel - 2.5\n        self.step()\n        return self.fuel\n\ndef nudge(b):\n    b.x = b.x + 0.25\n    return b.x\n\nb = Body()\nprint(b.x, b.v, b.n, b.mixed)\nb.step()\nb.step()\nprint(b.x, b.v, b.n, b.mixed)\nb.speed_up(2.0)\nprint(b.v, b.where())\nprint(nudge(b), b.x > 3.0, b.x == 3.25)\nprint(\"x is\", b.x, b.v, [b.x, b.v])\nr = Rocket()\nprint(r.burn(), r.x, r.fuel, nudge(r))\nbodies = [Body(), Rocket()]\ntotal = 0.0\nfor it in bodies:\n    it.step()\n    total = total + it.x\nprint(total)\nb.x = b.x * -1.0\nprint(b.x, abs(b.x), int(b.x), round(b.x))\n",
            )],
        },
        Group {
            title: "Comprehension variables have their own scope",
            note: "As in Python 3, a comprehension's loop variables don't leak: \
                   `x = 5; ys = [x * 2 for x in range(3)]` leaves `x` at 5. The \
                   shared front end renames them inside the comprehension, so \
                   both backends agree by construction. The first iterable is \
                   evaluated in the enclosing scope, and a nested \
                   comprehension can read the outer one's variables.",
            probes: vec![
                p(
                    "module, function, dict, first iterable, nesting, tuple targets, packed",
                    "x = 5\nys = [x * 2 for x in range(3)]\nprint(x, ys)\ndef f():\n    x = 7\n    ys = [x * 2 for x in range(3)]\n    d = {x: x + 1 for x in range(2)}\n    return x, ys, d\nprint(f())\nx = [1, 2, 3]\nprint([x * 10 for x in x])\nrows = [[1, 2], [3, 4]]\nprint([v for row in rows for v in row if v % 2 == 0])\nprint([[x + y for y in range(2)] for x in range(3)])\nprint([[x for x in range(x)] for x in range(3)])\npairs = [(1, \"a\"), (2, \"b\")]\na = \"outer\"\nprint([a * 2 for a, b in pairs], a)\nnums: list[int] = [1, 2, 3]\nsq: list[int] = [n * n for n in nums]\nn = 99\nprint(sq, n)\n",
                ),
                p(
                    "a comprehension variable named self leaves self alone",
                    "class A:\n    def __init__(self, n):\n        self.n = n\n    def others(self, xs):\n        ys = [self.n for self in xs]\n        return ys, self.n\nprint(A(\"a\").others([A(\"z\")]))\n",
                ),
            ],
        },
        Group {
            title: "global",
            note: "A function's `global` names are the module's variables: \
                   assigning one sets the module variable on both backends, \
                   from a def, a method, a loop variable, an unpacking and a \
                   lifted nested def. Natively they are `@g_<name>` globals, \
                   always boxed; in WASM, module globals the inference leaves \
                   boxed because a function writes them.",
            probes: vec![
                p(
                    "counter",
                    "score = 0\ndef hit():\n    global score\n    score = score + 1\nhit()\nhit()\nprint(score)\n",
                ),
                p(
                    "created",
                    "def setup():\n    global name\n    name = \"robot\"\nsetup()\nprint(name)\n",
                ),
                p(
                    "retype",
                    "x = 1\ndef f():\n    global x\n    x = \"now a string\"\nf()\nprint(x)\n",
                ),
                p(
                    "loopvar",
                    "total = 0\ndef add_all():\n    global total, i\n    for i in range(4):\n        total += i\nadd_all()\nprint(total, i)\n",
                ),
                p(
                    "method",
                    "best = 0\nclass Bot:\n    def score(self, n):\n        global best\n        if n > best:\n            best = n\nb = Bot()\nb.score(3)\nb.score(1)\nprint(best)\n",
                ),
                p(
                    "nested",
                    "x = 5\ndef outer():\n    x = 1\n    def inner():\n        global x\n        x = x + 10\n    inner()\n    return x\nprint(outer(), x)\n",
                ),
                p(
                    "unpack",
                    "a = 0\nb = 0\ndef swap():\n    global a, b\n    a, b = 1, 2\nswap()\nprint(a, b)\n",
                ),
                p("toplevel", "global q\nq = 3\nprint(q)\n"),
            ],
        },
    ]
}

// --- wasm:js-string polyfill ------------------------------------------------
//
// ONE implementation, shared with the fuel harness (src/harness.rs — the
// REFERENCE HOST) by path-include: the `run` feature is off in the test
// profile, so the tests cannot reach it through the library.
#[path = "../../src/js_string_host.rs"]
mod js_string_host;
pub use js_string_host::{add_js_string_builtins, define_string_literals, js_str};

/// Every host-module import (`robot_tilt`, ...) answering with the table's
/// stand-in, so these programs run here exactly as with nothing attached.
fn define_host_stand_ins(linker: &mut Linker<Vec<u8>>, engine: &Engine) {
    use rust_p2w::HostTy;
    use wasmtime::{FuncType, Val, ValType};
    let vt = |t: HostTy| {
        if t == HostTy::Float {
            ValType::F64
        } else {
            ValType::I32
        }
    };
    for m in rust_p2w::HOST_MODULES {
        for f in m.fns {
            let params: Vec<ValType> = f.params.iter().map(|(_, t)| vt(*t)).collect();
            let results: Vec<ValType> = if f.ret == HostTy::None {
                vec![]
            } else {
                vec![vt(f.ret)]
            };
            let (ret, value) = (f.ret, f.stand_in);
            linker
                .func_new(
                    "env",
                    &f.import_name(m.name),
                    FuncType::new(engine, params, results),
                    move |_, _, out| {
                        match ret {
                            HostTy::None | HostTy::Handler => {}
                            HostTy::Float => out[0] = Val::F64(value.to_bits()),
                            HostTy::Int | HostTy::Bool => out[0] = Val::I32(value as i32),
                        }
                        Ok(())
                    },
                )
                .expect("define host stand-in");
        }
    }
}
