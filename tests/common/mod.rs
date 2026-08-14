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
    linker
        .func_wrap(
            "env",
            "write_char",
            |mut c: Caller<'_, Vec<u8>>, ch: i32| {
                c.data_mut().push(ch as u8);
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
        .func_wrap("env", "seed", |_: Caller<'_, Vec<u8>>| -> i32 { 42 })
        .unwrap();

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
                e!("'ab' - 'b'"),
                e!("'ab' + 1"),
                e!("1 + 'ab'"),
                e!("'ab' / 2"),
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
            ],
        },
        Group {
            title: "Conversions",
            note: "The `input()` -> `int()` path is where beginners meet types \
                   whether or not the language has them.",
            probes: vec![
                e!("int('42')"),
                e!("int('abc')"),
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
    ]
}
