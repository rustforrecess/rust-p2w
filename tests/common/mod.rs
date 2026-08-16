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

// --- wasm:js-string (stage 2 of docs/REPRESENTATION_REWORK.md) --------------
//
// The browser gets these builtins natively (compile option
// `{ builtins: ['js-string'], importedStringConstants: "'" }`); wasmtime gets
// this polyfill — the spec REQUIRES the imports to be polyfillable, which is
// what keeps the differential rig honest once $STR becomes an externref.
// Strings live host-side as Rust `String`s wrapped in `ExternRef`.

use wasmtime::{AsContextMut, ExternRef, Global, GlobalType, Mutability, Rooted, Val};

/// Read the host String out of a js-string externref (trap-worthy if the ref
/// is null or not a string — mirrors the builtins' spec'd type errors).
fn js_str(store: &mut impl AsContextMut, r: &Option<Rooted<ExternRef>>) -> String {
    let r = r.as_ref().expect("js-string builtin: null externref");
    r.data(&mut *store)
        .expect("externref data")
        .expect("externref host data")
        .downcast_ref::<String>()
        .expect("externref is not a js-string")
        .clone()
}

fn js_new(store: &mut impl AsContextMut, s: String) -> Rooted<ExternRef> {
    ExternRef::new(&mut *store, s).expect("alloc externref")
}

/// Define every `wasm:js-string` builtin the emitter uses on `linker`.
pub fn add_js_string_builtins<T: 'static>(linker: &mut Linker<T>) {
    let m = "wasm:js-string";
    linker
        .func_wrap(
            m,
            "test",
            |mut c: Caller<'_, T>, r: Option<Rooted<ExternRef>>| -> i32 {
                // 1 iff the externref wraps a host string. Null and externalized
                // wasm values (i31s, structs — they arrive as non-string data)
                // answer 0, matching JS semantics where those surface as
                // numbers/objects.
                match r {
                    None => 0,
                    Some(r) => r
                        .data(&mut c)
                        .ok()
                        .flatten()
                        .map(|d| d.downcast_ref::<String>().is_some())
                        .unwrap_or(false) as i32,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "length",
            |mut c: Caller<'_, T>, s: Option<Rooted<ExternRef>>| -> i32 {
                js_str(&mut c, &s).encode_utf16().count() as i32
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "charCodeAt",
            |mut c: Caller<'_, T>, s: Option<Rooted<ExternRef>>, i: i32| -> i32 {
                js_str(&mut c, &s)
                    .encode_utf16()
                    .nth(i as usize)
                    .expect("charCodeAt out of bounds") as i32
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "concat",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> Rooted<ExternRef> {
                let s = js_str(&mut c, &a) + &js_str(&mut c, &b);
                js_new(&mut c, s)
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "equals",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> i32 {
                // Spec: equals accepts null (null == null is true).
                match (&a, &b) {
                    (None, None) => 1,
                    (Some(_), Some(_)) => (js_str(&mut c, &a) == js_str(&mut c, &b)) as i32,
                    _ => 0,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "compare",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> i32 {
                // UTF-16 code-unit order, the JS `<` on strings.
                let (a, b) = (js_str(&mut c, &a), js_str(&mut c, &b));
                let (au, bu): (Vec<u16>, Vec<u16>) =
                    (a.encode_utf16().collect(), b.encode_utf16().collect());
                match au.cmp(&bu) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "substring",
            |mut c: Caller<'_, T>,
             s: Option<Rooted<ExternRef>>,
             start: i32,
             end: i32|
             -> Rooted<ExternRef> {
                let units: Vec<u16> = js_str(&mut c, &s).encode_utf16().collect();
                let n = units.len() as i32;
                let (start, end) = (start.clamp(0, n) as usize, end.clamp(0, n) as usize);
                let out = if start >= end {
                    String::new()
                } else {
                    String::from_utf16_lossy(&units[start..end])
                };
                js_new(&mut c, out)
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "fromCharCode",
            |mut c: Caller<'_, T>, u: i32| -> Rooted<ExternRef> {
                let s = String::from_utf16_lossy(&[u as u16]);
                js_new(&mut c, s)
            },
        )
        .unwrap();
    // The two array builtins take the CONCRETE (array (mut i16)) type — the
    // generic Func::wrap ArrayRef signature doesn't unify with it, so their
    // types are built explicitly against the engine.
    let u16s = wasmtime::ArrayType::new(
        linker.engine(),
        wasmtime::FieldType::new(Mutability::Var, wasmtime::StorageType::I16),
    );
    let u16s_ref = wasmtime::ValType::Ref(wasmtime::RefType::new(
        true,
        wasmtime::HeapType::ConcreteArray(u16s),
    ));
    let extern_nonnull =
        wasmtime::ValType::Ref(wasmtime::RefType::new(false, wasmtime::HeapType::Extern));
    let from_ty = wasmtime::FuncType::new(
        linker.engine(),
        [
            u16s_ref.clone(),
            wasmtime::ValType::I32,
            wasmtime::ValType::I32,
        ],
        [extern_nonnull],
    );
    linker
        .func_new(m, "fromCharCodeArray", from_ty, |mut c, params, results| {
            let arr = params[0].unwrap_anyref().expect("null array");
            let arr = arr.unwrap_array(&mut c).expect("not an array");
            let (start, end) = (params[1].unwrap_i32(), params[2].unwrap_i32());
            let mut units = Vec::with_capacity((end - start).max(0) as usize);
            for i in start..end {
                let v = arr.get(&mut c, i as u32).expect("array get");
                units.push(v.unwrap_i32() as u16);
            }
            let s = String::from_utf16_lossy(&units);
            results[0] = Val::ExternRef(Some(ExternRef::new(&mut c, s).expect("alloc")));
            Ok(())
        })
        .unwrap();
    let into_ty = wasmtime::FuncType::new(
        linker.engine(),
        [
            wasmtime::ValType::EXTERNREF,
            u16s_ref,
            wasmtime::ValType::I32,
        ],
        [wasmtime::ValType::I32],
    );
    linker
        .func_new(m, "intoCharCodeArray", into_ty, |mut c, params, results| {
            let s = params[0].unwrap_externref().cloned();
            let text = js_str(&mut c, &s);
            let arr = params[1].unwrap_anyref().expect("null array");
            let arr = arr.unwrap_array(&mut c).expect("not an array");
            let start = params[2].unwrap_i32();
            let mut n = 0;
            for (i, u) in text.encode_utf16().enumerate() {
                arr.set(&mut c, start as u32 + i as u32, Val::I32(u as i32))
                    .expect("array set");
                n += 1;
            }
            results[0] = Val::I32(n);
            Ok(())
        })
        .unwrap();
}

/// Provide every `(import "'" "<literal>" (global (ref extern)))` the module
/// declares: the import NAME is the string value — the spec's
/// `importedStringConstants` mechanism, polyfilled.
pub fn define_string_literals<T>(linker: &mut Linker<T>, store: &mut Store<T>, module: &Module) {
    for imp in module.imports() {
        if imp.module() != "'" {
            continue;
        }
        let text = imp.name().to_string();
        let r = ExternRef::new(&mut *store, text).expect("literal externref");
        let ty = GlobalType::new(
            wasmtime::ValType::Ref(wasmtime::RefType::new(false, wasmtime::HeapType::Extern)),
            Mutability::Const,
        );
        let g = Global::new(&mut *store, ty, Val::ExternRef(Some(r))).expect("literal global");
        linker
            .define(&mut *store, "'", imp.name(), g)
            .expect("define literal");
    }
}
