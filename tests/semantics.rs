//! Generates `RUNTIME_SEMANTICS.md` by RUNNING programs, not by reading
//! codegen.
//!
//! A type system has to explain the behaviour that already exists. Writing that
//! behaviour down by hand means someone reverse-engineers 8,500 lines of
//! codegen and gets it subtly wrong; worse, the description rots the first time
//! the runtime changes. So this probes the compiler and the real WASM with a
//! matrix of small programs and records what actually happens.
//!
//! The distinction the document exists to capture is **where a mistake is
//! caught** — compile time, or at runtime as a trap. Every row marked `trap` is
//! a candidate for becoming a compile error once there are types, and the
//! `must-reject` half of `tests/oracle/` is the subset we have decided should
//! move.
//!
//! `cargo test --test semantics` checks the committed document is current.
//! `P2W_BLESS=1 cargo test --test semantics` rewrites it.

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
enum Outcome {
    /// Compiled and ran; this is what it printed.
    Value(String),
    /// The compiler refused it. Already a compile-time error today.
    CompileError(String),
    /// It compiled, then trapped. The text is what the program printed first —
    /// our runtime writes the message and then executes `unreachable`.
    Trap(String),
}

fn probe(src: &str) -> Outcome {
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
struct Probe {
    /// What to show in the document.
    shown: &'static str,
    /// The program actually compiled and run.
    src: &'static str,
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

struct Group {
    title: &'static str,
    /// Why a type-system designer should care about this group.
    note: &'static str,
    probes: Vec<Probe>,
}

fn groups() -> Vec<Group> {
    vec![
        Group {
            title: "Arithmetic on numbers",
            note: "⚠ `int` is **32-bit** on this backend, not 64 — the literal \
                   range is checked at compile time but arithmetic that \
                   overflows it is not. Mixed int/float promotes and `/` is \
                   always float division; both are CPython's rules and both \
                   must survive typing.",
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
            note: "⚠ `**` REJECTS FLOATS AT RUNTIME, and says so with a message \
                   that contradicts itself — a float is a number. Everything \
                   else here is fine, so this is an isolated gap in `$py_pow`, \
                   not a float problem.",
            probes: vec![
                e!("2.5 + 1.5"),
                e!("2.5 * 2.0"),
                e!("2.5 - 1.0"),
                e!("5.0 / 2.0"),
                e!("2.0 ** 2.0"),
                e!("2.0 ** 2"),
                e!("2 ** 0.5"),
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
                   operators they know do work. ⚠ Note also that ORDERING \
                   COMPARISONS ON STRINGS ARE NOT SUPPORTED (`'a' < 'b'`), \
                   though CPython allows them — a real subset gap, and the \
                   message blames the operator rather than naming the gap.",
            probes: vec![
                e!("'ab' + 'cd'"),
                e!("'ab' * 3"),
                e!("3 * 'ab'"),
                e!("'abc'[1]"),
                e!("len('abc')"),
                e!("'a' < 'b'"),
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

fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# What the runtime does today\n\n\
         **Generated by `tests/semantics.rs` — do not edit by hand.** Every row \
         below was produced by compiling the program and running the real WASM \
         under wasmtime, so this describes the compiler as it is, not as anyone \
         remembers it.\n\n\
         Regenerate with `P2W_BLESS=1 cargo test --test semantics`; the test \
         fails if this file drifts.\n\n\
         ## Why this exists\n\n\
         A type system has to explain behaviour that already exists. The column \
         that matters is **where** — whether a mistake is caught by the compiler \
         or reaches the runtime as a trap:\n\n\
         | where | meaning |\n|---|---|\n\
         | `value` | compiled, ran, printed this |\n\
         | `compile error` | the compiler already refuses it |\n\
         | `trap` | it compiled, then failed at runtime with this message |\n\n\
         **Every `trap` row is a candidate to become a compile error** once \
         there are types. `tests/oracle/must-reject/` is the subset we have \
         decided should move, and `tests/oracle/README.md` says what the \
         messages then have to do.\n\n\
         Traps print their message and then execute `unreachable`. That is \
         CPython's uncaught-exception behaviour minus the traceback, which is \
         why the absence of `try`/`except` costs less than it sounds like it \
         should.\n",
    );

    for g in groups() {
        out.push_str(&format!("\n## {}\n\n{}\n\n", g.title, g.note));
        out.push_str("| program | where | result |\n|---|---|---|\n");
        for pr in &g.probes {
            let (kind, text) = match probe(pr.src) {
                Outcome::Value(v) => ("value", v),
                Outcome::CompileError(e) => ("compile error", e),
                Outcome::Trap(t) => ("trap", t),
            };
            // Keep each row on one line and safe inside a Markdown table.
            let text = text.replace('\n', " ⏎ ").replace('|', "\\|");
            let text = if text.len() > 160 {
                format!("{}…", &text[..160])
            } else {
                text
            };
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                pr.shown.replace('|', "\\|"),
                kind,
                if text.is_empty() {
                    "(no output)".to_string()
                } else {
                    format!("`{text}`")
                }
            ));
        }
    }
    out
}

#[test]
fn runtime_semantics_document_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("RUNTIME_SEMANTICS.md");
    let generated = render();
    if std::env::var("P2W_BLESS").is_ok() {
        std::fs::write(&path, &generated).expect("write");
        eprintln!("blessed {}", path.display());
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    // Compare ignoring line endings — git normalises these on Windows.
    assert_eq!(
        committed.replace("\r\n", "\n"),
        generated,
        "RUNTIME_SEMANTICS.md is out of date. The runtime's behaviour changed \
         (or the probe list did). Review the diff — a change here is a change \
         to the language — then run: P2W_BLESS=1 cargo test --test semantics"
    );
}
