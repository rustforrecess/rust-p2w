//! Execution tests: compile to WAT, run the WASM for real (wasmtime with
//! WASM-GC enabled, with the same `env.write_char` / `env.write_i32` host
//! imports the browser harness provides), and compare stdout against what
//! CPython prints.
//!
//! WAT-validation tests can't catch miscompiles that produce well-formed but
//! wrong code (bitwise `and`, truncating `//`, re-evaluated range bounds…) —
//! these can.

use wasmtime::{Caller, Config, Engine, Linker, Module, Store};

/// Compile and execute `src`, returning everything written via the host
/// imports, decoded as UTF-8.
fn run(src: &str) -> String {
    let wat = rust_p2w::compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
    let wasm = wat::parse_str(&wat).unwrap_or_else(|e| panic!("invalid WAT: {e}\n---\n{wat}"));

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    let engine = Engine::new(&config).expect("engine");
    let module = Module::new(&engine, &wasm[..]).expect("module");
    // Store data is the output byte buffer; write_char sends UTF-8 bytes.
    let mut store: Store<Vec<u8>> = Store::new(&engine, Vec::new());
    let mut linker: Linker<Vec<u8>> = Linker::new(&engine);
    linker
        .func_wrap(
            "env",
            "write_char",
            |mut caller: Caller<'_, Vec<u8>>, c: i32| {
                caller.data_mut().push(c as u8);
            },
        )
        .unwrap();
    linker
        .func_wrap(
            "env",
            "write_i32",
            |mut caller: Caller<'_, Vec<u8>>, v: i32| {
                caller
                    .data_mut()
                    .extend_from_slice(v.to_string().as_bytes());
            },
        )
        .unwrap();
    linker
        .func_wrap(
            "env",
            "write_f64",
            |mut caller: Caller<'_, Vec<u8>>, v: f64| {
                // Python-style: whole floats keep ".0" (repr(2.0) == "2.0");
                // otherwise Rust's shortest round-trip matches Python's for
                // everyday values. (Known divergence at extremes: Python
                // switches to scientific notation around 1e16.)
                let s = if v.is_finite() && v == v.trunc() {
                    format!("{v:.1}")
                } else {
                    format!("{v}")
                };
                caller.data_mut().extend_from_slice(s.as_bytes());
            },
        )
        .unwrap();

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let start = instance
        .get_typed_func::<(), i32>(&mut store, "_start")
        .expect("_start export with i32 result");
    let exit = start.call(&mut store, ()).expect("execution trapped");
    assert_eq!(exit, 0, "_start exit code");
    String::from_utf8(store.into_data()).expect("output is UTF-8")
}

#[track_caller]
fn assert_output(src: &str, expected: &str) {
    assert_eq!(run(src), expected, "program:\n{src}");
}

// --- and / or: Python value semantics + short-circuit ---

#[test]
fn and_or_return_the_deciding_operand() {
    assert_output("print(2 and 1)", "1\n");
    assert_output("print(0 and 1)", "0\n");
    assert_output("print(4 or 2)", "4\n");
    assert_output("print(0 or 7)", "7\n");
}

#[test]
fn and_or_short_circuit_skips_the_right_side() {
    // The right side would trap with a division by zero if it were evaluated.
    assert_output("x = 0\nprint(x and 7 // x)", "0\n");
    assert_output("x = 0\nprint(1 or 7 // x)", "1\n");
}

#[test]
fn truthiness_in_conditions() {
    assert_output(
        "if 2 and 4:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "yes\n",
    );
    assert_output(
        "if 0 or 0:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "no\n",
    );
}

// --- booleans are a real type ---

#[test]
fn booleans_print_as_true_false() {
    assert_output("print(True)", "True\n");
    assert_output("print(False)", "False\n");
    assert_output("print(1 == 1)", "True\n");
    assert_output("print(1 > 2)", "False\n");
    assert_output("print(5 > 4 > 3)", "True\n");
    assert_output("print(not True)", "False\n");
}

#[test]
fn booleans_count_as_one_and_zero_in_arithmetic() {
    assert_output("print(True + 1)", "2\n");
    assert_output("print(False * 10)", "0\n");
    assert_output("x = True\nprint(x + x)", "2\n");
}

// --- functions ---

#[test]
fn simple_function_call() {
    assert_output(
        "def add(a, b):\n    return a + b\nprint(add(2, 3))\nprint(add(\"ab\", \"cd\"))",
        "5\nabcd\n",
    );
}

#[test]
fn recursion_factorial_and_fib() {
    assert_output(
        "def fact(n):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(10))",
        "3628800\n",
    );
    assert_output(
        "def fib(n):\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nprint(fib(15))",
        "610\n",
    );
}

#[test]
fn mutual_recursion() {
    let src = "\
def is_even(n):
    if n == 0:
        return True
    return is_odd(n - 1)
def is_odd(n):
    if n == 0:
        return False
    return is_even(n - 1)
print(is_even(10), is_odd(7))
";
    assert_output(src, "True True\n");
}

#[test]
fn functions_read_globals_and_locals_shadow() {
    assert_output(
        "bonus = 10\ndef score(p):\n    return p + bonus\nprint(score(5))",
        "15\n",
    );
    assert_output(
        "x = 1\ndef f():\n    x = 2\n    return x\nprint(f(), x)",
        "2 1\n",
    );
}

#[test]
fn implicit_and_bare_return_give_none() {
    assert_output("def f():\n    x = 1\nprint(f())", "None\n");
    assert_output("def f():\n    return\nprint(f())", "None\n");
    assert_output("print(None)", "None\n");
    assert_output(
        "print(None == None, None == 0, None == \"\")",
        "True False False\n",
    );
    assert_output(
        "if None:\n    print(\"y\")\nelse:\n    print(\"n\")\n",
        "n\n",
    );
}

#[test]
fn bare_call_statement_runs_for_effects() {
    assert_output(
        "total = 0\ndef bump():\n    print(\"bump\")\nbump()\nbump()",
        "bump\nbump\n",
    );
}

#[test]
fn function_with_loop_and_early_return() {
    assert_output(
        "def first_div(n, d):\n    for i in range(1, n):\n        if i % d == 0:\n            return i\n    return -1\nprint(first_div(100, 7))\nprint(first_div(5, 9))",
        "7\n-1\n",
    );
}

// --- strings are values ---

#[test]
fn string_variables_and_printing() {
    assert_output("x = \"hello\"\nprint(x)", "hello\n");
    assert_output(
        "s = \"caf\u{e9} \u{1f980}\"\nprint(s)",
        "caf\u{e9} \u{1f980}\n",
    );
    assert_output("x = \"hi\"\nprint(x, 5, x)", "hi 5 hi\n");
}

#[test]
fn string_concatenation() {
    assert_output("print(\"ab\" + \"cd\")", "abcd\n");
    assert_output("s = \"na\"\nprint(s + s + \" batman\")", "nana batman\n");
    assert_output(
        "s = \"\"\nfor i in range(3):\n    s = s + \"ab\"\nprint(s)",
        "ababab\n",
    );
}

#[test]
fn string_equality_is_by_value() {
    assert_output("print(\"abc\" == \"abc\")", "True\n");
    assert_output("print(\"abc\" == \"abd\")", "False\n");
    assert_output("print(\"abc\" != \"abd\")", "True\n");
    assert_output("a = \"x\"\nb = \"x\"\nprint(a == b)", "True\n");
    // String vs number is False, never an error (like Python).
    assert_output("print(\"1\" == 1)", "False\n");
}

#[test]
fn string_truthiness() {
    assert_output(
        "if \"\":\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "no\n",
    );
    assert_output(
        "if \"x\":\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "yes\n",
    );
    assert_output("print(\"\" or \"fallback\")", "fallback\n");
    assert_output("print(\"first\" and \"second\")", "second\n");
}

// --- floats ---

#[test]
fn floats_print_python_style() {
    assert_output("print(3.5)", "3.5\n");
    assert_output("print(2.0)", "2.0\n"); // whole floats keep .0
    assert_output("print(-0.25)", "-0.25\n");
    assert_output("print(0.1 + 0.2)", "0.30000000000000004\n"); // IEEE, like Python
}

#[test]
fn true_division_always_returns_float() {
    assert_output("print(7 / 2)", "3.5\n");
    assert_output("print(4 / 2)", "2.0\n");
    assert_output("print(1.0 / 4)", "0.25\n");
}

#[test]
fn mixed_arithmetic_promotes_to_float() {
    assert_output("print(1.5 + 2)", "3.5\n");
    assert_output("print(2 * 1.5)", "3.0\n");
    assert_output("print(5 - 0.5)", "4.5\n");
    assert_output("x = 2.5\nprint(x * 2 - 1)", "4.0\n");
    assert_output("print(-(1.5))", "-1.5\n");
}

#[test]
fn float_floordiv_and_mod_match_python() {
    assert_output("print(7.5 // 2)", "3.0\n");
    assert_output("print(-3.5 // 1)", "-4.0\n");
    assert_output("print(7.5 % 2)", "1.5\n");
    assert_output("print(-7.5 % 2)", "0.5\n"); // sign of the divisor
}

#[test]
fn float_comparisons_and_equality() {
    assert_output("print(1.5 < 2)", "True\n");
    assert_output("print(1 == 1.0)", "True\n");
    assert_output("print(2.0 == 2)", "True\n");
    assert_output("print(0.1 + 0.2 == 0.3)", "False\n"); // the classic
    assert_output("x = 98.7\nif x > 98.6:\n    print(\"fever\")\n", "fever\n");
}

#[test]
fn float_truthiness() {
    assert_output(
        "if 0.0:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "no\n",
    );
    assert_output(
        "if 0.5:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "yes\n",
    );
}

// --- chained comparisons ---

#[test]
fn chained_comparisons_match_python() {
    assert_output(
        "if 5 > 4 > 3:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "yes\n",
    );
    assert_output(
        "if 1 < 3 < 2:\n    print(\"yes\")\nelse:\n    print(\"no\")\n",
        "no\n",
    );
    assert_output(
        "x = 5\nif 0 <= x <= 10:\n    print(\"in range\")\n",
        "in range\n",
    );
}

// --- floor division and modulo ---

#[test]
fn floordiv_and_mod_use_floor_semantics() {
    assert_output("print(7 // 2)", "3\n");
    assert_output("print(-7 // 2)", "-4\n");
    assert_output("print(7 // -2)", "-4\n");
    assert_output("print(-7 // -2)", "3\n");
    assert_output("print(7 % 2)", "1\n");
    assert_output("print(-7 % 2)", "1\n");
    assert_output("print(7 % -2)", "-1\n");
    assert_output("print(-7 % -2)", "-1\n");
}

// --- range() semantics ---

#[test]
fn range_bounds_are_evaluated_once() {
    // Mutating the bound variable in the body must not extend the loop.
    assert_output(
        "n = 3\nfor i in range(0, n):\n    n = n + 1\nprint(n)",
        "6\n",
    );
}

#[test]
fn reassigning_the_loop_variable_does_not_change_iteration() {
    assert_output("for i in range(3):\n    i = 100\nprint(i)", "100\n");
}

#[test]
fn negative_step_counts_down() {
    assert_output("for i in range(5, 0, -1):\n    print(i)", "5\n4\n3\n2\n1\n");
    assert_output("for i in range(10, 0, -3):\n    print(i)", "10\n7\n4\n1\n");
}

// --- while / break / continue ---

#[test]
fn while_countdown() {
    assert_output(
        "i = 3\nwhile i > 0:\n    print(i)\n    i = i - 1",
        "3\n2\n1\n",
    );
}

#[test]
fn while_true_with_break() {
    assert_output(
        "i = 0\nwhile True:\n    i = i + 1\n    if i == 3:\n        break\nprint(i)",
        "3\n",
    );
}

#[test]
fn continue_in_for_still_increments() {
    // If continue skipped the counter increment, this would loop forever.
    assert_output(
        "for i in range(5):\n    if i % 2 == 0:\n        continue\n    print(i)",
        "1\n3\n",
    );
}

#[test]
fn continue_in_while_retests_condition() {
    assert_output(
        "i = 0\nwhile i < 5:\n    i = i + 1\n    if i % 2 == 0:\n        continue\n    print(i)",
        "1\n3\n5\n",
    );
}

#[test]
fn break_exits_only_the_inner_loop() {
    assert_output(
        "for i in range(2):\n    for j in range(5):\n        if j == 1:\n            break\n        print(i, j)",
        "0 0\n1 0\n",
    );
}

// --- integer literals ---

#[test]
fn i32_boundary_literals() {
    assert_output("print(2147483647)", "2147483647\n");
    assert_output("print(-2147483648)", "-2147483648\n");
}

// --- lexer: CRLF line continuation ---

#[test]
fn backslash_continuation_with_crlf() {
    assert_output("x = 1 + \\\r\n2\r\nprint(x)\r\n", "3\n");
}

// --- whole programs ---

#[test]
fn fizzbuzz_matches_python() {
    let src = "\
for i in range(1, 16):
    if i % 15 == 0:
        print(\"FizzBuzz\")
    elif i % 3 == 0:
        print(\"Fizz\")
    elif i % 5 == 0:
        print(\"Buzz\")
    else:
        print(i)
";
    assert_output(
        src,
        "1\n2\nFizz\n4\nBuzz\nFizz\n7\n8\nFizz\nBuzz\n11\nFizz\n13\n14\nFizzBuzz\n",
    );
}

#[test]
fn sum_of_evens_matches_python() {
    let src = "\
total = 0
for i in range(1, 6):
    if i % 2 == 0:
        total = total + i
print(\"sum of evens:\", total)
";
    assert_output(src, "sum of evens: 6\n");
}

// --- differential testing against real CPython, when available ---

/// Programs that print ints, bools, and strings (`/` is still rejected, so it
/// stays out of the corpus). All intermediate values must fit in i32: runtime
/// arithmetic still wraps where Python promotes to bignum — a known
/// divergence not yet handled.
const DIFFERENTIAL_CORPUS: &[&str] = &[
    "print(True)\nprint(False)\nprint(1 == 1, 2 < 1)\nprint(5 > 4 > 3)",
    "print(True + 1, False * 10, not False)",
    "x = 7\nprint(x and True, 0 or False)",
    "x = \"hello\"\nprint(x, x + \"!\")",
    "s = \"\"\nfor i in range(4):\n    s = s + \"ab\"\nprint(s)\nprint(s == \"abababab\")",
    "print(\"abc\" == \"abc\", \"abc\" == \"abd\", \"1\" == 1)",
    "print(7 / 2, 4 / 2, 1.0 / 4)",
    "print(0.1 + 0.2)\nprint(0.1 + 0.2 == 0.3)",
    "print(1.5 + 2, 2 * 1.5, 5 - 0.5, -(1.5))",
    "print(7.5 // 2, -3.5 // 1, 7.5 % 2, -7.5 % 2)",
    "print(1.5 < 2, 1 == 1.0, 2.0 == 2)",
    "x = 0.0\nif x:\n    print(\"t\")\nelse:\n    print(\"f\")",
    "def fib(n):\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nprint(fib(12))",
    "def greet(name):\n    return \"hi \" + name\nprint(greet(\"Felicia\"))",
    "bonus = 10\ndef score(p):\n    return p + bonus\nprint(score(5), score(0.5))",
    "def f():\n    x = 1\nprint(f(), None == None, not None)",
    "def is_even(n):\n    if n == 0:\n        return True\n    return is_odd(n - 1)\ndef is_odd(n):\n    if n == 0:\n        return False\n    return is_even(n - 1)\nprint(is_even(8), is_odd(8))",
    "if \"\":\n    print(\"y\")\nelse:\n    print(\"n\")\nprint(\"\" or \"fb\", \"a\" and \"b\")",
    "print(2 and 1)\nprint(0 and 1)\nprint(4 or 2)\nprint(0 or 7)",
    "print(7 // 2, -7 // 2, 7 // -2, -7 // -2)",
    "print(7 % 2, -7 % 2, 7 % -2, -7 % -2)",
    "n = 3\nfor i in range(0, n):\n    n = n + 1\nprint(n)",
    "for i in range(3):\n    i = i * 10\nprint(i)",
    "for i in range(5, 0, -1):\n    print(i)",
    "x = 5\nif 0 <= x <= 10:\n    print(\"in\")\nelse:\n    print(\"out\")",
    "print(-(-2147483647) - 1)",
    "total = 0\nfor i in range(1, 101):\n    total = total + i\nprint(total)",
    "i = 3\nwhile i > 0:\n    print(i)\n    i = i - 1",
    "i = 0\nwhile True:\n    i = i + 1\n    if i == 3:\n        break\nprint(i)",
    "for i in range(6):\n    if i % 2 == 0:\n        continue\n    if i == 5:\n        break\n    print(i)",
    "i = 0\nwhile i < 5:\n    i = i + 1\n    if i % 2 == 0:\n        continue\n    print(i)",
];

fn find_python() -> Option<&'static str> {
    for candidate in ["python", "python3"] {
        let ok = std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn differential_against_cpython() {
    let Some(python) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    for src in DIFFERENTIAL_CORPUS {
        let out = std::process::Command::new(python)
            .args(["-c", src])
            .output()
            .expect("run python");
        assert!(
            out.status.success(),
            "CPython rejected corpus program:\n{src}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = String::from_utf8(out.stdout)
            .expect("python output is UTF-8")
            .replace("\r\n", "\n");
        assert_eq!(run(src), expected, "differs from CPython for:\n{src}");
    }
}
