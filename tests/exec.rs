//! Execution tests: compile to WAT, run the WASM for real (wasmi, with the
//! same `env.write_char` / `env.write_i32` host imports the browser harness
//! provides), and compare stdout against what CPython prints.
//!
//! WAT-validation tests can't catch miscompiles that produce well-formed but
//! wrong code (bitwise `and`, truncating `//`, re-evaluated range bounds…) —
//! these can.

use wasmi::{Caller, Engine, Linker, Module, Store};

/// Compile and execute `src`, returning everything written via the host
/// imports, decoded as UTF-8.
fn run(src: &str) -> String {
    let wat = rust_p2w::compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
    let wasm = wat::parse_str(&wat).unwrap_or_else(|e| panic!("invalid WAT: {e}\n---\n{wat}"));

    let engine = Engine::default();
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

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate")
        .start(&mut store)
        .expect("start");
    let start = instance
        .get_typed_func::<(), i32>(&store, "_start")
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

/// Programs that print only ints and strings (the backend prints booleans as
/// 0/1 and rejects `/`, so those stay out of the differential corpus). All
/// intermediate values must fit in i32: runtime arithmetic still wraps where
/// Python promotes to bignum — a known divergence not yet handled.
const DIFFERENTIAL_CORPUS: &[&str] = &[
    "print(2 and 1)\nprint(0 and 1)\nprint(4 or 2)\nprint(0 or 7)",
    "print(7 // 2, -7 // 2, 7 // -2, -7 // -2)",
    "print(7 % 2, -7 % 2, 7 % -2, -7 % -2)",
    "n = 3\nfor i in range(0, n):\n    n = n + 1\nprint(n)",
    "for i in range(3):\n    i = i * 10\nprint(i)",
    "for i in range(5, 0, -1):\n    print(i)",
    "x = 5\nif 0 <= x <= 10:\n    print(\"in\")\nelse:\n    print(\"out\")",
    "print(-(-2147483647) - 1)",
    "total = 0\nfor i in range(1, 101):\n    total = total + i\nprint(total)",
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
