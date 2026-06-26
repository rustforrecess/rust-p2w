//! Execution tests: compile to WAT, run the WASM for real (wasmtime with
//! WASM-GC enabled, with the same `env.write_char` / `env.write_i32` host
//! imports the browser harness provides), and compare stdout against what
//! CPython prints.
//!
//! WAT-validation tests can't catch miscompiles that produce well-formed but
//! wrong code (bitwise `and`, truncating `//`, re-evaluated range bounds…) —
//! these can.

use std::sync::OnceLock;
use wasmtime::{Caller, Config, Engine, Linker, Module, OptLevel, Store};

/// One shared Engine for the whole suite: Engine is internally refcounted
/// and Send+Sync (sharing is the documented pattern), and opt-level None
/// skips Cranelift optimization — these programs run in microseconds, so
/// compile time dominates the suite.
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_gc(true);
        config.wasm_function_references(true);
        config.cranelift_opt_level(OptLevel::None);
        Engine::new(&config).expect("engine")
    })
}

/// Host I/O state: the output buffer plus a stdin byte buffer with a cursor
/// (so `env.read_char` can serve `input()`).
struct Io {
    out: Vec<u8>,
    input: Vec<u8>,
    pos: usize,
}

/// Compile and execute `src` with no stdin.
fn execute(src: &str) -> (String, Result<i32, wasmtime::Error>) {
    execute_io(src, "")
}

/// Compile and execute `src`, feeding `stdin` to `input()`; returns everything
/// written via the host imports plus `_start`'s result (Err = trapped).
fn execute_io(src: &str, stdin: &str) -> (String, Result<i32, wasmtime::Error>) {
    let wat = rust_p2w::compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
    let wasm = wat::parse_str(&wat).unwrap_or_else(|e| panic!("invalid WAT: {e}\n---\n{wat}"));

    let module = Module::new(engine(), &wasm[..]).expect("module");
    let mut store: Store<Io> = Store::new(
        engine(),
        Io {
            out: Vec::new(),
            input: stdin.as_bytes().to_vec(),
            pos: 0,
        },
    );
    let mut linker: Linker<Io> = Linker::new(engine());
    linker
        .func_wrap("env", "write_char", |mut caller: Caller<'_, Io>, c: i32| {
            caller.data_mut().out.push(c as u8);
        })
        .unwrap();
    linker
        .func_wrap("env", "write_i32", |mut caller: Caller<'_, Io>, v: i32| {
            caller
                .data_mut()
                .out
                .extend_from_slice(v.to_string().as_bytes());
        })
        .unwrap();
    linker
        .func_wrap("env", "write_f64", |mut caller: Caller<'_, Io>, v: f64| {
            // Python-style: whole floats keep ".0" (repr(2.0) == "2.0");
            // otherwise Rust's shortest round-trip matches Python's for
            // everyday values. (Known divergence at extremes: Python
            // switches to scientific notation around 1e16.)
            let s = if v.is_finite() && v == v.trunc() {
                format!("{v:.1}")
            } else {
                format!("{v}")
            };
            caller.data_mut().out.extend_from_slice(s.as_bytes());
        })
        .unwrap();
    // read_char: next stdin byte, or -1 at EOF (matches the browser harness).
    linker
        .func_wrap("env", "read_char", |mut caller: Caller<'_, Io>| -> i32 {
            let d = caller.data_mut();
            if d.pos < d.input.len() {
                let b = d.input[d.pos];
                d.pos += 1;
                b as i32
            } else {
                -1
            }
        })
        .unwrap();

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let start = instance
        .get_typed_func::<(), i32>(&mut store, "_start")
        .expect("_start export with i32 result");
    let result = start.call(&mut store, ());
    let out = String::from_utf8(store.into_data().out).expect("output is UTF-8");
    (out, result)
}

/// Run a program expected to succeed; returns its output.
fn run(src: &str) -> String {
    let (out, result) = execute(src);
    let exit = result.expect("execution trapped");
    assert_eq!(exit, 0, "_start exit code");
    out
}

#[track_caller]
fn assert_output(src: &str, expected: &str) {
    assert_eq!(run(src), expected, "program:\n{src}");
}

#[track_caller]
fn assert_io(src: &str, stdin: &str, expected: &str) {
    let (out, result) = execute_io(src, stdin);
    assert_eq!(result.expect("execution trapped"), 0, "_start exit code");
    assert_eq!(out, expected, "program:\n{src}");
}

/// Run a program that is expected to raise: returns everything written
/// before the trap (which includes the runtime's error message).
fn run_expect_error(src: &str) -> String {
    let (out, result) = execute(src);
    assert!(
        result.is_err(),
        "expected a runtime error, program ran fine:\n{out}"
    );
    out
}

#[track_caller]
fn assert_raises(src: &str, message_contains: &str) {
    let out = run_expect_error(src);
    assert!(
        out.contains(message_contains),
        "expected {message_contains:?} in output, got:\n{out}\nprogram:\n{src}"
    );
}

// --- type annotations: parsed, but runtime-ignored like Python ---

#[test]
fn type_annotations_compile_and_are_ignored() {
    // Annotated params and a return type must compile and run identically to
    // the untyped form — annotations are hints, not runtime checks.
    assert_output(
        "def add(a: int, b: int) -> int:\n    return a + b\nprint(add(2, 3))",
        "5\n",
    );
    // A "wrong" annotation doesn't change behaviour (no enforcement).
    assert_output(
        "def label(x: str) -> int:\n    return x\nprint(label(\"hi\"))",
        "hi\n",
    );
    // A subscripted type annotation (list[int]) is accepted too.
    assert_output(
        "def total(xs: list[int]) -> int:\n    s = 0\n    for v in xs:\n        s += v\n    return s\nprint(total([1, 2, 3]))",
        "6\n",
    );
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

// --- lists ---

#[test]
fn list_literals_print_like_python() {
    assert_output("print([1, 2, 3])", "[1, 2, 3]\n");
    assert_output("print([])", "[]\n");
    // Strings inside lists print with quotes (repr), like Python.
    assert_output(
        "print([1, \"a\", True, None, 2.5])",
        "[1, 'a', True, None, 2.5]\n",
    );
    assert_output("print([[1, 2], [3]])", "[[1, 2], [3]]\n");
}

#[test]
fn list_indexing_and_negative_indices() {
    assert_output(
        "xs = [10, 20, 30]\nprint(xs[0], xs[2], xs[-1], xs[-3])",
        "10 30 30 10\n",
    );
    assert_output("grid = [[1, 2], [3, 4]]\nprint(grid[1][0])", "3\n");
    assert_output("print(\"hello\"[1], \"hello\"[-1])", "e o\n");
}

#[test]
fn list_index_assignment() {
    assert_output(
        "xs = [1, 2, 3]\nxs[1] = 99\nxs[-1] = 0\nprint(xs)",
        "[1, 99, 0]\n",
    );
}

#[test]
fn append_grows_past_initial_capacity() {
    assert_output(
        "xs = []\nfor i in range(20):\n    xs.append(i * i)\nprint(len(xs), xs[0], xs[19])",
        "20 0 361\n",
    );
}

#[test]
fn len_builtin() {
    assert_output("print(len([1, 2, 3]), len([]), len(\"hello\"))", "3 0 5\n");
}

#[test]
fn for_in_list_and_string() {
    assert_output("for x in [3, 1, 2]:\n    print(x)", "3\n1\n2\n");
    assert_output("for c in \"abc\":\n    print(c)", "a\nb\nc\n");
    assert_output(
        "total = 0\nfor x in [1, 2, 3, 4]:\n    total = total + x\nprint(total)",
        "10\n",
    );
}

#[test]
fn list_equality_and_concat() {
    assert_output(
        "print([1, 2] == [1, 2], [1, 2] == [1, 3], [1] == 1)",
        "True False False\n",
    );
    assert_output("print([[1], \"a\"] == [[1], \"a\"])", "True\n");
    assert_output("print([1, 2] + [3])", "[1, 2, 3]\n");
}

#[test]
fn list_truthiness() {
    assert_output(
        "if []:\n    print(\"y\")\nelse:\n    print(\"n\")\nif [0]:\n    print(\"t\")\n",
        "n\nt\n",
    );
}

#[test]
fn lists_are_references_and_functions_take_them() {
    assert_output(
        "def push_twice(xs, v):\n    xs.append(v)\n    xs.append(v)\nys = [1]\npush_twice(ys, 7)\nprint(ys)",
        "[1, 7, 7]\n",
    );
}

// --- dicts ---

#[test]
fn dict_literals_print_like_python() {
    assert_output("print({})", "{}\n");
    assert_output("print({\"a\": 1, \"b\": 2})", "{'a': 1, 'b': 2}\n");
    assert_output(
        "print({1: \"one\", \"two\": 2, 2.5: True})",
        "{1: 'one', 'two': 2, 2.5: True}\n",
    );
}

#[test]
fn dict_get_set_update_insert() {
    assert_output(
        "d = {\"hp\": 10}\nd[\"hp\"] = d[\"hp\"] + 5\nd[\"mp\"] = 3\nprint(d[\"hp\"], d[\"mp\"], len(d))\nprint(d)",
        "15 3 2\n{'hp': 15, 'mp': 3}\n",
    );
    assert_output("d = {1: \"a\", 2: \"b\"}\nprint(d[2], d[1])", "b a\n");
}

#[test]
fn for_in_dict_iterates_keys_in_insertion_order() {
    assert_output(
        "d = {\"x\": 1, \"y\": 2, \"z\": 3}\nfor k in d:\n    print(k, d[k])",
        "x 1\ny 2\nz 3\n",
    );
}

#[test]
fn dict_equality_is_order_insensitive() {
    assert_output(
        "a = {\"x\": 1, \"y\": 2}\nb = {\"y\": 2, \"x\": 1}\nprint(a == b, a == {\"x\": 1}, {} == {})",
        "True False True\n",
    );
}

#[test]
fn dict_truthiness_and_references() {
    assert_output(
        "if {}:\n    print(\"y\")\nelse:\n    print(\"n\")\nif {\"k\": 0}:\n    print(\"t\")\n",
        "n\nt\n",
    );
    assert_output(
        "def bump(d, k):\n    d[k] = d[k] + 1\nscores = {\"sam\": 1}\nbump(scores, \"sam\")\nprint(scores)",
        "{'sam': 2}\n",
    );
}

#[test]
fn dicts_nest_with_lists() {
    assert_output(
        "d = {\"xs\": [1, 2]}\nd[\"xs\"].append(3)\nprint(d)\nprint([{\"a\": 1}])",
        "{'xs': [1, 2, 3]}\n[{'a': 1}]\n",
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

// --- runtime errors print friendly Python-style messages ---

#[test]
fn type_errors_name_the_offending_type() {
    assert_raises(
        "x = \"a\"\ny = 1\nprint(x - y)",
        "TypeError: expected a number, got 'str'",
    );
    assert_raises(
        "x = [1]\nprint(x * 2)",
        "TypeError: expected a number, got 'list'",
    );
    assert_raises(
        "x = None\nprint(x + 1)",
        "TypeError: expected a number, got 'NoneType'",
    );
}

#[test]
fn index_and_key_errors() {
    assert_raises(
        "xs = [1, 2]\nprint(xs[5])",
        "IndexError: list index out of range",
    );
    assert_raises(
        "s = \"ab\"\nprint(s[9])",
        "IndexError: string index out of range",
    );
    assert_raises(
        "d = {\"a\": 1}\nprint(d[\"missing\"])",
        "KeyError: 'missing'",
    );
    assert_raises(
        "x = 5\nprint(x[0])",
        "TypeError: 'int' object is not subscriptable",
    );
    assert_raises(
        "s = \"ab\"\ns[0] = \"x\"",
        "TypeError: 'str' object does not support item assignment",
    );
}

#[test]
fn zero_division_errors() {
    assert_raises(
        "a = 7\nb = 0\nprint(a // b)",
        "ZeroDivisionError: division by zero",
    );
    assert_raises("a = 7\nb = 0\nprint(a % b)", "ZeroDivisionError");
    assert_raises("a = 7.5\nb = 0\nprint(a / b)", "ZeroDivisionError");
}

#[test]
fn len_and_method_errors() {
    assert_raises(
        "x = 5\nprint(len(x))",
        "TypeError: object of type 'int' has no len()",
    );
    assert_raises(
        "x = 5\nx.append(1)",
        "AttributeError: 'int' object has no attribute 'append'",
    );
}

#[test]
fn unassigned_function_local_is_a_name_error() {
    assert_raises(
        "def f(flag):\n    if flag:\n        x = 1\n    return x\nprint(f(False))",
        "NameError: a variable was used before it was given a value",
    );
}

#[test]
fn output_before_the_error_is_preserved() {
    let out = run_expect_error("print(\"step 1\")\nprint(\"step 2\")\nxs = []\nprint(xs[0])");
    assert!(out.starts_with("step 1\nstep 2\n"));
    assert!(out.contains("IndexError"));
}

// --- f-strings and conversion builtins ---

#[test]
fn str_builtin_converts() {
    assert_output("print(str(42) + \"!\", str(-7), str(0))", "42! -7 0\n");
    assert_output(
        "print(str(True), str(False), str(None))",
        "True False None\n",
    );
    assert_output(
        "print(str(\"already\") + \" a string\")",
        "already a string\n",
    );
    assert_output("print(str(-2147483648))", "-2147483648\n"); // INT_MIN
    assert_output("print(len(str(12345)))", "5\n");
}

#[test]
fn fstrings_interpolate() {
    assert_output(
        "name = \"Felicia\"\nprint(f\"Hello, {name}!\")",
        "Hello, Felicia!\n",
    );
    assert_output(
        "a = 3\nb = 4\nprint(f\"{a} + {b} = {a + b}\")",
        "3 + 4 = 7\n",
    );
    assert_output("print(f\"\")\nprint(f\"plain\")", "\nplain\n");
    assert_output("print(f\"{{literal braces}}\")", "{literal braces}\n");
    assert_output(
        "score = 8\nprint(f\"Score: {score}/10 ({score * 10}%)\")",
        "Score: 8/10 (80%)\n",
    );
    assert_output(
        "xs = [1, 2]\nprint(f\"first={xs[0]} len={len(xs)}\")",
        "first=1 len=2\n",
    );
}

#[test]
fn abs_min_max_int_builtins() {
    assert_output("print(abs(-5), abs(5), abs(-2.5))", "5 5 2.5\n");
    assert_output(
        "print(min(3, 7), max(3, 7), min(2, 1.5), max(-1, -2))",
        "3 7 1.5 -1\n",
    );
    // min/max return the original value: int stays int.
    assert_output("print(min(1, 2.0), max(2.0, 1))", "1 2.0\n");
    assert_output(
        "print(int(3.7), int(-3.7), int(5), int(True))",
        "3 -3 5 1\n",
    );
    assert_output("print(abs(-2147483647))", "2147483647\n");
}

#[test]
fn fstring_and_str_error_cases() {
    // str(float) is still unsupported; str(list) now works.
    assert_raises(
        "print(str(2.5))",
        "TypeError: str() of 'float' values isn't supported yet",
    );
    assert_output("print(str([1, 2]))", "[1, 2]\n");
}

// --- the in operator ---

#[test]
fn in_lists_dicts_strings() {
    assert_output("print(2 in [1, 2, 3], 5 in [1, 2, 3])", "True False\n");
    assert_output(
        "d = {\"a\": 1}\nprint(\"a\" in d, \"b\" in d, \"b\" not in d)",
        "True False True\n",
    );
    assert_output(
        "print(\"cad\" in \"abracadabra\", \"xyz\" in \"abracadabra\", \"\" in \"a\")",
        "True False True\n",
    );
    assert_output("print(None in [None], [1] in [[1], [2]])", "True True\n");
}

#[test]
fn not_binds_looser_than_in() {
    // `not x in xs` is `not (x in xs)`, like Python.
    assert_output("print(not 5 in [1, 2])", "True\n");
}

#[test]
fn the_idiomatic_counter_finally_works() {
    let src = "\
counts = {}
for c in \"abracadabra\":
    if c in counts:
        counts[c] = counts[c] + 1
    else:
        counts[c] = 1
print(counts)
";
    assert_output(src, "{'a': 5, 'b': 2, 'r': 2, 'c': 1, 'd': 1}\n");
}

#[test]
fn in_error_cases() {
    assert_raises(
        "print(1 in 5)",
        "TypeError: argument of type 'int' is not iterable",
    );
    assert_raises(
        "print(1 in \"abc\")",
        "TypeError: 'in <string>' requires string as left operand, not 'int'",
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

// --- classes (slice 1: construction, attrs, methods, dispatch, inheritance) ---

#[test]
fn class_construction_attrs_and_methods() {
    assert_output(
        "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def total(self):\n        return self.x + self.y\np = Point(3, 4)\nprint(p.x, p.y, p.total())",
        "3 4 7\n",
    );
}

#[test]
fn class_method_mutates_attribute() {
    assert_output(
        "class Counter:\n    def __init__(self):\n        self.n = 0\n    def inc(self):\n        self.n = self.n + 1\nc = Counter()\nc.inc()\nc.inc()\nprint(c.n)",
        "2\n",
    );
}

#[test]
fn class_inheritance_resolves_methods_along_the_chain() {
    // Dog has no __init__ — construction finds Animal.__init__; speak is
    // overridden, kind is inherited.
    assert_output(
        "class Animal:\n    def __init__(self, name):\n        self.name = name\n    def speak(self):\n        return self.name + \" makes a sound\"\n    def kind(self):\n        return \"animal\"\nclass Dog(Animal):\n    def speak(self):\n        return self.name + \" barks\"\nd = Dog(\"Rex\")\nprint(d.speak())\nprint(d.kind())",
        "Rex barks\nanimal\n",
    );
}

#[test]
fn instances_have_reference_semantics() {
    assert_output(
        "class Box:\n    def __init__(self, v):\n        self.v = v\ndef bump(box):\n    box.v = box.v + 1\nb = Box(1)\nbump(b)\nbump(b)\nprint(b.v)",
        "3\n",
    );
}

#[test]
fn print_uses_str_then_repr() {
    // __str__ wins for print/str(); __repr__ is the fallback and the form
    // used inside containers.
    assert_output(
        "class Q:\n    def __init__(self, n):\n        self.n = n\n    def __str__(self):\n        return \"str-\" + str(self.n)\n    def __repr__(self):\n        return \"repr-\" + str(self.n)\nq = Q(5)\nprint(q)\nprint([q, Q(6)])",
        "str-5\n[repr-5, repr-6]\n",
    );
}

#[test]
fn repr_only_is_used_by_print_and_containers() {
    assert_output(
        "class P:\n    def __init__(self, x):\n        self.x = x\n    def __repr__(self):\n        return \"P(\" + str(self.x) + \")\"\nprint(P(3))\nprint([P(1), P(2)])",
        "P(3)\n[P(1), P(2)]\n",
    );
}

#[test]
fn instance_without_repr_prints_default() {
    assert_output(
        "class Empty:\n    def __init__(self):\n        self.x = 1\ne = Empty()\nprint(e)",
        "<Empty object>\n",
    );
}

#[test]
fn missing_attribute_raises_attribute_error() {
    assert_raises(
        "class A:\n    def __init__(self):\n        self.x = 1\na = A()\nprint(a.y)",
        "AttributeError",
    );
    assert_raises(
        "class A:\n    def __init__(self):\n        self.x = 1\na = A()\nprint(a.y)",
        "has no attribute 'y'",
    );
}

#[test]
fn missing_method_raises_attribute_error() {
    assert_raises(
        "class A:\n    def __init__(self):\n        self.x = 1\na = A()\nprint(a.nope())",
        "has no attribute 'nope'",
    );
}

#[test]
fn wrong_method_arity_raises_type_error() {
    assert_raises(
        "class A:\n    def __init__(self):\n        self.x = 1\n    def m(self, a):\n        return a\na = A()\nprint(a.m())",
        "wrong number of arguments",
    );
}

#[test]
fn super_init_chains_to_the_base() {
    assert_output(
        "class Animal:\n    def __init__(self, name):\n        self.name = name\nclass Dog(Animal):\n    def __init__(self, name):\n        super().__init__(name)\n        self.tricks = []\n    def learn(self, t):\n        self.tricks.append(t)\nd = Dog(\"Rex\")\nd.learn(\"sit\")\nprint(d.name, d.tricks)",
        "Rex ['sit']\n",
    );
}

#[test]
fn super_method_calls_the_overridden_base_method() {
    assert_output(
        "class Animal:\n    def speak(self):\n        return \"some sound\"\nclass Dog(Animal):\n    def speak(self):\n        return super().speak() + \" (woof)\"\nd = Dog()\nprint(d.speak())",
        "some sound (woof)\n",
    );
}

#[test]
fn super_outside_a_method_is_an_error() {
    let err = rust_p2w::compile_to_wat("x = super()").unwrap_err();
    assert!(err.to_string().contains("super()"), "{err}");
}

#[test]
fn super_without_a_base_is_an_error() {
    let err = rust_p2w::compile_to_wat("class A:\n    def m(self):\n        return super().m()")
        .unwrap_err();
    assert!(err.to_string().contains("base class"), "{err}");
}

#[test]
fn operator_dunders_drive_arithmetic_and_equality() {
    assert_output(
        "class V:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def __add__(self, o):\n        return V(self.x + o.x, self.y + o.y)\n    def __eq__(self, o):\n        return self.x == o.x and self.y == o.y\n    def __repr__(self):\n        return \"V(\" + str(self.x) + \", \" + str(self.y) + \")\"\nprint(V(1, 2) + V(3, 4))\nprint(V(1, 2) == V(1, 2))",
        "V(4, 6)\nTrue\n",
    );
}

#[test]
fn len_and_getitem_dunders() {
    assert_output(
        "class Deck:\n    def __init__(self):\n        self.cards = [7, 8, 9]\n    def __len__(self):\n        return len(self.cards)\n    def __getitem__(self, i):\n        return self.cards[i]\nd = Deck()\nprint(len(d))\nprint(d[1])",
        "3\n8\n",
    );
}

#[test]
fn objects_without_eq_compare_by_identity() {
    assert_output(
        "class A:\n    def __init__(self):\n        self.n = 1\na = A()\nb = A()\nprint(a == a, a == b)",
        "True False\n",
    );
}

#[test]
fn class_variables_are_shared_and_shadowed_by_instance_attrs() {
    // Read falls back to the class; writing creates an instance attr that
    // shadows it (Python semantics).
    assert_output(
        "class C:\n    x = 10\na = C()\nb = C()\nprint(a.x, b.x)\na.x = 99\nprint(a.x, b.x)",
        "10 10\n99 10\n",
    );
}

#[test]
fn reading_a_method_as_a_value_is_an_error() {
    assert_raises(
        "class A:\n    def m(self):\n        return 1\na = A()\nf = a.m",
        "can't be used as a value",
    );
}

#[test]
fn class_redefinition_is_an_error() {
    let err = rust_p2w::compile_to_wat("class A:\n    def m(self):\n        return 1\nclass A:\n    def m(self):\n        return 2").unwrap_err();
    assert!(err.to_string().contains("defined twice"), "{err}");
}

#[test]
fn unknown_base_class_is_an_error() {
    let err =
        rust_p2w::compile_to_wat("class Dog(Animal):\n    def speak(self):\n        return 1")
            .unwrap_err();
    assert!(err.to_string().contains("unknown base class"), "{err}");
}

// --- slicing ---

#[test]
fn list_and_string_slices() {
    assert_output("print([0, 1, 2, 3, 4][1:3])", "[1, 2]\n");
    assert_output("print(\"abcdef\"[2:])", "cdef\n");
    assert_output("print([1, 2, 3, 4, 5][::-1])", "[5, 4, 3, 2, 1]\n");
    assert_output("print(\"hello\"[::-1])", "olleh\n");
    assert_output("print([0, 1, 2, 3, 4, 5][1:5:2])", "[1, 3]\n");
}

#[test]
fn slice_negative_and_out_of_range_bounds() {
    assert_output("print([1, 2, 3, 4][-2:])", "[3, 4]\n");
    assert_output("print([1, 2, 3][5:])", "[]\n");
    assert_output("print([1, 2, 3][3:1])", "[]\n");
    assert_output("print(\"abc\"[-10:2])", "ab\n");
}

#[test]
fn slice_step_zero_raises() {
    assert_raises("print([1, 2, 3][::0])", "slice step cannot be zero");
}

#[test]
fn slice_assignment_is_rejected() {
    let err = rust_p2w::compile_to_wat("xs = [1, 2, 3]\nxs[0:1] = [9]").unwrap_err();
    assert!(err.to_string().contains("slice assignment"), "{err}");
}

#[test]
fn slicing_a_non_sequence_errors() {
    assert_raises("x = 5\nprint(x[1:2])", "subscriptable");
}

// --- iterable builtins (range value, enumerate, zip, dict views, sum, sorted) ---

#[test]
fn range_as_a_value() {
    assert_output("print(sum(range(5)))", "10\n");
    assert_output(
        "r = range(1, 4)\nprint(len(r))\nfor x in r:\n    print(x)",
        "3\n1\n2\n3\n",
    );
    assert_output("print(sorted(range(5, 0, -1)))", "[1, 2, 3, 4, 5]\n");
}

#[test]
fn enumerate_and_zip() {
    assert_output(
        "for i, c in enumerate(\"ab\"):\n    print(i, c)",
        "0 a\n1 b\n",
    );
    assert_output(
        "for i, x in enumerate([9, 8], 1):\n    print(i, x)",
        "1 9\n2 8\n",
    );
    assert_output(
        "print([a + b for a, b in zip([1, 2, 3], [10, 20])])",
        "[11, 22]\n",
    );
}

#[test]
fn dict_views() {
    assert_output(
        "d = {\"x\": 1, \"y\": 2}\nfor k, v in d.items():\n    print(k, v)",
        "x 1\ny 2\n",
    );
    assert_output(
        "d = {\"a\": 3, \"b\": 1}\nprint(sorted(d.values()))",
        "[1, 3]\n",
    );
}

#[test]
fn sum_and_sorted() {
    assert_output("print(sum([10, 20, 30]))", "60\n");
    assert_output("print(sorted([5, 2, 8, 1]))", "[1, 2, 5, 8]\n");
    assert_output("print(sorted(\"ceab\"))", "['a', 'b', 'c', 'e']\n");
}

// --- tuples and unpacking ---

#[test]
fn tuples_print_distinctly_from_lists() {
    assert_output("print((1, 2, 3))", "(1, 2, 3)\n");
    assert_output("print([1, 2, 3])", "[1, 2, 3]\n");
    assert_output("print((1,))", "(1,)\n"); // singleton keeps the comma
    assert_output("print(())", "()\n");
    assert_output("print((1, 2) == (1, 2), (1, 2) == [1, 2])", "True False\n");
}

#[test]
fn tuple_indexing_and_len() {
    assert_output("t = (10, 20, 30)\nprint(t[1], t[-1], len(t))", "20 30 3\n");
}

#[test]
fn unpacking_and_swap() {
    assert_output("a, b = 1, 2\nprint(a, b)", "1 2\n");
    assert_output("a, b = 1, 2\na, b = b, a\nprint(a, b)", "2 1\n");
    assert_output("x, y, z = [7, 8, 9]\nprint(x + y + z)", "24\n");
}

#[test]
fn unpacking_length_mismatch_raises() {
    assert_raises("a, b = [1, 2, 3]", "values to unpack");
    assert_raises("a, b, c = (1, 2)", "values to unpack");
}

#[test]
fn for_loop_tuple_target() {
    assert_output(
        "for i, c in [(0, \"a\"), (1, \"b\")]:\n    print(i, c)",
        "0 a\n1 b\n",
    );
}

// --- augmented assignment ---

#[test]
fn augmented_assignment_forms() {
    assert_output("x = 10\nx += 5\nprint(x)", "15\n");
    assert_output("x = 10\nx -= 3\nx *= 2\nprint(x)", "14\n");
    assert_output("xs = [1, 2, 3]\nxs[1] += 100\nprint(xs)", "[1, 102, 3]\n");
    assert_output("s = \"go\"\ns += \"!\"\nprint(s)", "go!\n");
}

#[test]
fn augmented_assignment_to_non_target_is_an_error() {
    let err = rust_p2w::compile_to_wat("5 += 1").unwrap_err();
    assert!(err.to_string().contains("can only use +="), "{err}");
}

// --- comprehensions ---

#[test]
fn list_comprehensions() {
    assert_output("print([x * x for x in range(5)])", "[0, 1, 4, 9, 16]\n");
    assert_output(
        "print([x for x in range(8) if x % 2 == 1])",
        "[1, 3, 5, 7]\n",
    );
    assert_output("print([c for c in \"abc\"])", "['a', 'b', 'c']\n");
    assert_output(
        "xs = [10, 20, 30]\nprint([x + 1 for x in xs])",
        "[11, 21, 31]\n",
    );
}

#[test]
fn nested_comprehensions_and_clauses() {
    assert_output(
        "print([x + y for x in range(2) for y in range(3)])",
        "[0, 1, 2, 1, 2, 3]\n",
    );
    assert_output(
        "print([[y for y in range(x)] for x in range(3)])",
        "[[], [0], [0, 1]]\n",
    );
}

#[test]
fn dict_comprehension() {
    assert_output(
        "print({x: x * x for x in range(4)})",
        "{0: 0, 1: 1, 2: 4, 3: 9}\n",
    );
}

#[test]
fn comprehension_in_function_does_not_leak() {
    // Python 3 scopes the loop variable to the comprehension; a fresh name is
    // undefined afterward.
    let err = rust_p2w::compile_to_wat(
        "def f():\n    ys = [i for i in range(3)]\n    return i\nprint(f())",
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown name"), "{err}");
}

#[test]
fn comprehension_tuple_target() {
    assert_output("print([a + b for a, b in [(1, 2), (3, 4)]])", "[3, 7]\n");
    assert_output(
        "print([a * b for a, b in zip([1, 2, 3], [4, 5, 6])])",
        "[4, 10, 18]\n",
    );
}

// --- any() / all() and default arguments ---

#[test]
fn any_and_all() {
    assert_output("print(any([0, 1, 0]), all([1, 1]))", "True True\n");
    assert_output("print(any([0, 0]), all([1, 0]))", "False False\n");
    assert_output("print(any([]), all([]))", "False True\n");
}

#[test]
fn default_arguments() {
    assert_output(
        "def f(a, b=10):\n    return a + b\nprint(f(5), f(5, 1))",
        "15 6\n",
    );
    assert_output(
        "def g(x, y=2, z=3):\n    return x * 100 + y * 10 + z\nprint(g(1), g(1, 5), g(1, 5, 9))",
        "123 153 159\n",
    );
}

#[test]
fn default_arguments_arity_errors() {
    let err = rust_p2w::compile_to_wat("def f(a, b=1):\n    return a\nprint(f())").unwrap_err();
    assert!(
        err.to_string().contains("missing a required argument"),
        "{err}"
    );
    let err =
        rust_p2w::compile_to_wat("def f(a, b=1):\n    return a\nprint(f(1, 2, 3))").unwrap_err();
    assert!(err.to_string().contains("too many positional"), "{err}");
}

// --- str() / repr() of collections ---

#[test]
fn str_of_collections() {
    assert_output("print(str([1, 2, 3]))", "[1, 2, 3]\n");
    assert_output("print(str((1,)))", "(1,)\n");
    assert_output("print(str({\"a\": 1}))", "{'a': 1}\n");
    assert_output(
        "print(str([1, \"x\", True, None]))",
        "[1, 'x', True, None]\n",
    );
    assert_output("print(\"v=\" + str([1, 2]))", "v=[1, 2]\n");
    assert_output("print(f\"{[1, 2, 3]}\")", "[1, 2, 3]\n"); // bare f-string of a list
}

#[test]
fn set_display_is_sorted_when_orderable() {
    // Sets display in canonical sorted order when homogeneously orderable (all
    // numbers, or all strings); mixed-type sets fall back to insertion order.
    assert_output("print(str(set([3, 1, 2])))", "{1, 2, 3}\n");
    assert_output("print(set([\"c\", \"a\", \"b\"]))", "{'a', 'b', 'c'}\n");
    assert_output("print(set([2, \"a\", 1]))", "{2, 'a', 1}\n"); // mixed -> insertion
    assert_output("print(repr(\"hi\"), repr(5))", "'hi' 5\n");
}

#[test]
fn str_of_object_uses_dunder() {
    assert_output(
        "class P:\n    def __repr__(self):\n        return \"P!\"\nprint(str(P()))\nprint(f\"{P()}\")",
        "P!\nP!\n",
    );
}

#[test]
fn str_of_float_still_unsupported() {
    assert_raises("print(str(3.14))", "str()");
    assert_raises("print(str([1.5]))", "str()"); // float element
}

// --- more string methods ---

#[test]
fn string_case_strip_zfill() {
    assert_output("print(\"hello WORLD\".capitalize())", "Hello world\n");
    assert_output("print(\"a nice day\".title())", "A Nice Day\n");
    assert_output("print(\"[\" + \"  x  \".lstrip() + \"]\")", "[x  ]\n");
    assert_output("print(\"[\" + \"  x  \".rstrip() + \"]\")", "[  x]\n");
    assert_output("print(\"42\".zfill(5), \"-3\".zfill(4))", "00042 -003\n");
}

// --- str.format() ---

#[test]
fn str_format_method() {
    assert_output("print(\"{} and {}\".format(1, 2))", "1 and 2\n");
    assert_output("print(\"{1}-{0}\".format(\"a\", \"b\"))", "b-a\n");
    assert_output("print(\"{:.2f}\".format(2.5))", "2.50\n");
    assert_output("print(\"[{:>4}]\".format(7))", "[   7]\n");
    assert_output("print(\"{{}} {}\".format(9))", "{} 9\n");
}

#[test]
fn str_format_errors_and_class_fallback() {
    let err = rust_p2w::compile_to_wat("print(\"{} {}\".format(1))").unwrap_err();
    assert!(err.to_string().contains("not enough arguments"), "{err}");
    let err = rust_p2w::compile_to_wat("print(\"{name}\".format(1))").unwrap_err();
    assert!(err.to_string().contains("named format fields"), "{err}");
    // a class method named `format` still dispatches
    assert_output(
        "class F:\n    def format(self):\n        return \"custom\"\nprint(F().format())",
        "custom\n",
    );
}

// --- dict and set methods ---

#[test]
fn dict_update_clear_setdefault() {
    assert_output(
        "d = {\"a\": 1}\nd.update({\"a\": 9, \"b\": 2})\nprint(d)",
        "{'a': 9, 'b': 2}\n",
    );
    assert_output(
        "d = {}\nprint(d.setdefault(\"x\", 5), d.setdefault(\"x\", 7))",
        "5 5\n",
    );
    assert_output(
        "d = {\"a\": 1, \"b\": 2}\nd.clear()\nprint(d, len(d))",
        "{} 0\n",
    );
}

#[test]
fn set_algebra_methods() {
    assert_output("print(sorted({1, 2, 3}.union({3, 4})))", "[1, 2, 3, 4]\n");
    assert_output(
        "print(sorted({1, 2, 3}.intersection([2, 3, 4])))",
        "[2, 3]\n",
    );
    assert_output("print(sorted({1, 2, 3}.difference({2})))", "[1, 3]\n");
    assert_output(
        "print({1, 2}.issubset({1, 2, 3}), {1, 2, 3}.issuperset({1, 2}), {1, 5}.issubset({1, 2}))",
        "True True False\n",
    );
}

// --- list methods ---

#[test]
fn list_sort_reverse() {
    assert_output("xs = [3, 1, 2]\nxs.sort()\nprint(xs)", "[1, 2, 3]\n");
    assert_output("xs = [1, 2, 3]\nxs.reverse()\nprint(xs)", "[3, 2, 1]\n");
    assert_output(
        "w = [\"pear\", \"apple\", \"fig\"]\nw.sort()\nprint(w)",
        "['apple', 'fig', 'pear']\n",
    );
}

#[test]
fn list_insert_extend() {
    assert_output(
        "xs = [1, 2, 3]\nxs.insert(1, 99)\nprint(xs)",
        "[1, 99, 2, 3]\n",
    );
    assert_output(
        "xs = [1]\nxs.insert(0, 0)\nxs.insert(99, 5)\nprint(xs)",
        "[0, 1, 5]\n",
    );
    assert_output(
        "xs = [1, 2]\nxs.extend([3, 4])\nprint(xs)",
        "[1, 2, 3, 4]\n",
    );
}

#[test]
fn list_count_index() {
    assert_output("print([1, 2, 2, 3, 2].count(2))", "3\n");
    assert_output("print([10, 20, 30].index(20))", "1\n");
    assert_raises("print([1, 2, 3].index(9))", "not in the sequence");
}

#[test]
fn str_index_and_count_still_work() {
    assert_output(
        "print(\"banana\".count(\"a\"), \"banana\".index(\"nan\"))",
        "3 2\n",
    );
    assert_raises("print(\"abc\".index(\"z\"))", "not in the sequence");
}

// --- set operations ---

#[test]
fn set_operations() {
    assert_output("print(sorted({1, 2, 3} | {3, 4, 5}))", "[1, 2, 3, 4, 5]\n");
    assert_output("print(sorted({1, 2, 3} & {2, 3, 4}))", "[2, 3]\n");
    assert_output("print(sorted({1, 2, 3} - {2}))", "[1, 3]\n");
    assert_output("print(sorted({1, 2, 3} ^ {3, 4}))", "[1, 2, 4]\n");
}

#[test]
fn set_op_augmented_and_precedence() {
    assert_output("s = {1}\ns |= {2, 3}\nprint(sorted(s))", "[1, 2, 3]\n");
    // & binds tighter than |
    assert_output("print(sorted({1, 2} | {3} & {3, 9}))", "[1, 2, 3]\n");
}

#[test]
fn set_op_on_non_set_is_an_error() {
    assert_raises("print(1 | 2)", "set operation");
}

// --- f-string format specs ---

#[test]
fn fstring_format_specs() {
    assert_output("print(f\"{3.14159:.2f}\")", "3.14\n");
    assert_output("print(f\"{1.5:.0f}\")", "2\n"); // ties to even
    assert_output("print(f\"[{42:5}]\")", "[   42]\n");
    assert_output("print(f\"[{42:<5}]\")", "[42   ]\n");
    assert_output("print(f\"[{42:^5}]\")", "[ 42  ]\n");
    assert_output("print(f\"[{7:03}]\")", "[007]\n");
    assert_output("print(f\"[{'hi':>6}]\")", "[    hi]\n");
}

#[test]
fn format_builtin_and_slice_colon_not_a_spec() {
    assert_output("print(format(255, \"d\"))", "255\n");
    assert_output("print(format(3.14159, \".3f\"))", "3.142\n");
    // a slice colon inside an f-string field is NOT a format spec
    assert_output("xs = [1, 2, 3, 4]\nprint(f\"{xs[1:3][0]}\")", "2\n");
}

#[test]
fn bad_format_spec_is_an_error() {
    let err = rust_p2w::compile_to_wat("x = 1\nprint(f\"{x:q}\")").unwrap_err();
    assert!(err.to_string().contains("unsupported format"), "{err}");
}

// --- sets ---

#[test]
fn set_basics() {
    // Insertion order is deterministic here (our sets iterate in insertion
    // order — a documented divergence from CPython's hash order).
    assert_output("print(set([1, 2, 3]))", "{1, 2, 3}\n");
    assert_output("print(set())", "set()\n");
    assert_output("print(len({1, 1, 2}))", "2\n");
    assert_output("print(3 in {1, 2, 3}, 9 in {1, 2, 3})", "True False\n");
}

#[test]
fn set_empty_is_falsy() {
    assert_output(
        "if set():\n    print(\"y\")\nelse:\n    print(\"n\")",
        "n\n",
    );
}

#[test]
fn set_add_discard_remove() {
    assert_output(
        "s = set()\ns.add(5)\ns.add(5)\ns.add(7)\nprint(sorted(s))",
        "[5, 7]\n",
    );
    assert_output(
        "s = set([1, 2, 3])\ns.discard(2)\ns.discard(9)\nprint(sorted(s))",
        "[1, 3]\n",
    );
    assert_raises("s = set([1])\ns.remove(2)", "KeyError");
}

// --- keyword arguments ---

#[test]
fn keyword_arguments() {
    assert_output(
        "def f(a, b=2, c=3):\n    return a * 100 + b * 10 + c\nprint(f(1), f(1, c=9), f(c=9, a=1))",
        "123 129 129\n",
    );
}

#[test]
fn keyword_argument_errors() {
    let err = rust_p2w::compile_to_wat("def f(a):\n    return a\nprint(f(b=1))").unwrap_err();
    assert!(
        err.to_string().contains("unexpected keyword argument"),
        "{err}"
    );
    let err = rust_p2w::compile_to_wat("def f(a, b):\n    return a\nprint(f(1, a=2))").unwrap_err();
    assert!(err.to_string().contains("multiple values"), "{err}");
    let err = rust_p2w::compile_to_wat("def f(a, b):\n    return a\nprint(f(b=2))").unwrap_err();
    assert!(
        err.to_string().contains("missing a required argument"),
        "{err}"
    );
    let err = rust_p2w::compile_to_wat("def f(a, b):\n    return a\nprint(f(a=1, 2))").unwrap_err();
    assert!(
        err.to_string().contains("positional argument can't follow"),
        "{err}"
    );
}

// --- generator expressions ---

#[test]
fn generator_expression_call_args() {
    assert_output("print(sum(x * x for x in range(4)))", "14\n");
    assert_output("print(max(x % 5 for x in [7, 12, 3]))", "3\n");
    assert_output("print(\",\".join(str(i) for i in range(3)))", "0,1,2\n");
}

#[test]
fn parenthesized_generator_expression() {
    assert_output("g = (x + 1 for x in [10, 20])\nprint(sum(g))", "32\n");
}

// --- math module ---

#[test]
fn math_module() {
    assert_output("import math\nprint(math.sqrt(9.0))", "3.0\n");
    assert_output(
        "import math\nprint(math.floor(2.9), math.ceil(2.1))",
        "2 3\n",
    );
    assert_output(
        "import math\nprint(math.trunc(-4.8), math.fabs(-3.0))",
        "-4 3.0\n",
    );
    assert_output("import math\nprint(math.pi)", "3.141592653589793\n");
}

#[test]
fn math_errors() {
    let err = rust_p2w::compile_to_wat("import os").unwrap_err();
    assert!(err.to_string().contains("isn't available"), "{err}");
    let err = rust_p2w::compile_to_wat("import math\nprint(math.nope(1))").unwrap_err();
    assert!(err.to_string().contains("no function"), "{err}");
}

// --- min/max iterable, bool, round ---

#[test]
fn min_max_over_iterable_and_args() {
    assert_output("print(min([4, 2, 7, 1]), max([4, 2, 7, 1]))", "1 7\n");
    assert_output("print(min(3, 9, 1), max(3, 9, 1))", "1 9\n");
    assert_output("print(min(\"cat\", \"ant\"))", "ant\n");
}

#[test]
fn min_of_empty_raises() {
    assert_raises("print(min([]))", "empty sequence");
}

#[test]
fn bool_builtin() {
    assert_output(
        "print(bool(0), bool(3), bool(\"\"), bool([1]))",
        "False True False True\n",
    );
}

#[test]
fn round_builtin() {
    assert_output("print(round(2.5), round(3.5), round(-0.5))", "2 4 0\n"); // ties to even
    assert_output("print(round(2.7), round(2.4))", "3 2\n");
    assert_output("print(round(3.14159, 2))", "3.14\n");
}

// --- float() ---

#[test]
fn float_builtin() {
    assert_output("print(float(\"3.5\"))", "3.5\n");
    assert_output("print(float(\"-2\"))", "-2.0\n");
    assert_output("print(float(5), float(2.5))", "5.0 2.5\n");
    assert_output("print(float(\"1.5e2\"))", "150.0\n");
}

#[test]
fn float_of_bad_string_raises() {
    assert_raises(
        "print(float(\"1.2.3\"))",
        "could not convert string to float",
    );
    assert_raises("print(float(\"abc\"))", "could not convert string to float");
}

// --- dict .get/.pop and list .pop ---

#[test]
fn dict_get_and_pop() {
    assert_output(
        "d = {\"a\": 1}\nprint(d.get(\"a\"), d.get(\"x\"), d.get(\"x\", 0))",
        "1 None 0\n",
    );
    assert_output(
        "d = {\"a\": 1, \"b\": 2}\nprint(d.pop(\"a\"))\nprint(d)",
        "1\n{'b': 2}\n",
    );
    assert_output("d = {}\nprint(d.pop(\"x\", -1))", "-1\n");
}

#[test]
fn dict_pop_missing_without_default_raises() {
    assert_raises("d = {}\nd.pop(\"x\")", "KeyError");
}

#[test]
fn list_pop() {
    assert_output("xs = [1, 2, 3]\nprint(xs.pop())\nprint(xs)", "3\n[1, 2]\n");
    assert_output("xs = [1, 2, 3]\nprint(xs.pop(0))\nprint(xs)", "1\n[2, 3]\n");
}

// --- power operator ---

#[test]
fn power_operator() {
    assert_output("print(2 ** 10)", "1024\n");
    assert_output("print(3 ** 0, 2 ** 1)", "1 2\n");
    assert_output("print(2 ** -1)", "0.5\n");
    assert_output("print(2.0 ** 3)", "8.0\n");
}

#[test]
fn power_precedence_and_associativity() {
    assert_output("print(-2 ** 2)", "-4\n"); // -(2 ** 2)
    assert_output("print((-2) ** 2)", "4\n");
    assert_output("print(2 ** 3 ** 2)", "512\n"); // right-assoc
    assert_output("print(2 * 3 ** 2)", "18\n"); // ** tighter than *
}

// --- string methods ---

#[test]
fn string_split() {
    assert_output("print(\"a b c\".split())", "['a', 'b', 'c']\n");
    assert_output("print(\"  x   y \".split())", "['x', 'y']\n");
    assert_output("print(\"a,b,,c\".split(\",\"))", "['a', 'b', '', 'c']\n");
    assert_output("print(\"\".split())", "[]\n");
    assert_output("print(\"\".split(\",\"))", "['']\n");
}

#[test]
fn string_strip_case_join() {
    assert_output("print(\"  hi  \".strip())", "hi\n");
    assert_output(
        "print(\"MixEd\".upper(), \"MixEd\".lower())",
        "MIXED mixed\n",
    );
    assert_output("print(\"-\".join([\"a\", \"b\", \"c\"]))", "a-b-c\n");
    assert_output("print(\"\".join([\"a\", \"b\"]))", "ab\n");
}

#[test]
fn string_count_find_classify() {
    assert_output(
        "print(\"banana\".count(\"a\"), \"banana\".count(\"na\"))",
        "3 2\n",
    );
    assert_output(
        "print(\"hello\".find(\"ll\"), \"hello\".find(\"x\"))",
        "2 -1\n",
    );
    assert_output(
        "print(\"42\".isdigit(), \"4a\".isdigit(), \"abc\".isalpha())",
        "True False True\n",
    );
}

#[test]
fn string_replace_and_affix_tests() {
    assert_output("print(\"a.b.c\".replace(\".\", \"/\"))", "a/b/c\n");
    assert_output("print(\"aaaa\".replace(\"aa\", \"b\"))", "bb\n");
    assert_output(
        "print(\"file.py\".endswith(\".py\"), \"file.py\".startswith(\"file\"))",
        "True True\n",
    );
    assert_output("print(\"hi\".startswith(\"hello\"))", "False\n");
}

#[test]
fn string_method_name_falls_back_to_class_method() {
    // A class defining `upper` still dispatches there (not the string helper).
    assert_output(
        "class Shout:\n    def __init__(self, s):\n        self.s = s\n    def upper(self):\n        return self.s + \"!!!\"\nprint(Shout(\"hi\").upper())",
        "hi!!!\n",
    );
}

// --- input() / stdin ---

#[test]
fn input_reads_a_line() {
    assert_io(
        "name = input()\nprint(\"Hello, \" + name + \"!\")",
        "World\n",
        "Hello, World!\n",
    );
}

#[test]
fn int_of_input_and_str_parsing() {
    assert_io("n = int(input())\nprint(n * n)", "7\n", "49\n");
    assert_io("print(int(\"  -42  \"))", "", "-42\n");
}

#[test]
fn int_of_bad_string_raises() {
    assert_raises("print(int(\"12x\"))", "invalid literal for int");
}

#[test]
fn input_loop_sums_n_numbers() {
    assert_io(
        "total = 0\nfor i in range(int(input())):\n    total += int(input())\nprint(total)",
        "3\n10\n20\n30\n",
        "60\n",
    );
}

#[test]
fn input_prompt_is_printed() {
    assert_io("x = input(\"name? \")\nprint(x)", "Sam\n", "name? Sam\n");
}

#[test]
fn no_input_means_no_read_char_import() {
    // A program that never calls input() must not import read_char (so existing
    // hosts that don't provide it keep working).
    let wat = rust_p2w::compile_to_wat("print(1)").unwrap();
    assert!(
        !wat.contains("read_char"),
        "read_char imported without input()"
    );
    let wat = rust_p2w::compile_to_wat("x = input()\nprint(x)").unwrap();
    assert!(wat.contains("read_char"), "read_char missing with input()");
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
    "print([1, \"a\", True, None, 2.5])\nprint([[1, 2], []])",
    "xs = [10, 20, 30]\nxs[1] = 99\nprint(xs[0], xs[-1], len(xs), xs)",
    "xs = []\nfor i in range(12):\n    xs.append(i)\nprint(len(xs), xs[-1], xs == [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11])",
    "total = 0\nfor x in [2, 4, 6]:\n    total = total + x\nfor c in \"ab\":\n    print(c)\nprint(total, [1] + [2, 3])",
    "def tail(xs):\n    return xs[-1]\nprint(tail([5, 6, 7]), tail(\"xyz\"))",
    "d = {\"a\": 1, \"b\": 2}\nd[\"a\"] = 10\nd[\"c\"] = 3\nprint(d, len(d), d[\"c\"])",
    "d = {\"x\": 1, \"y\": 2}\nfor k in d:\n    print(k, d[k])\nprint(d == {\"y\": 2, \"x\": 1})",
    "print({})\nprint({1: \"one\", \"two\": [2, 2.5]})",
    "counts = {}\nfor c in \"abracadabra\":\n    if c in counts:\n        counts[c] = counts[c] + 1\n    else:\n        counts[c] = 1\nprint(counts)",
    "print(2 in [1, 2], \"a\" in {\"a\": 1}, \"cad\" in \"abracadabra\", 9 not in [1], not 5 in [1])",
    "name = \"sam\"\nscore = 8\nprint(f\"{name}: {score}/10 = {score * 10}%\")",
    "print(str(42) + str(0) + str(-7), str(True), str(None))",
    "print(abs(-5), abs(-2.5), min(3, 7), max(3, 7), min(1, 2.0), int(3.7), int(-3.7), int(True))",
    "for i in range(3):\n    print(f\"line {i}: {i * i}\")",
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
    // --- classes (bare-object printing is excluded: CPython's default repr
    //     includes a module path and address we don't reproduce) ---
    "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def total(self):\n        return self.x + self.y\np = Point(3, 4)\nprint(p.x, p.y, p.total())",
    "class Counter:\n    def __init__(self):\n        self.n = 0\n    def inc(self):\n        self.n = self.n + 1\nc = Counter()\nc.inc()\nc.inc()\nc.inc()\nprint(c.n)",
    "class Animal:\n    def __init__(self, name):\n        self.name = name\n    def speak(self):\n        return self.name + \" makes a sound\"\n    def kind(self):\n        return \"animal\"\nclass Dog(Animal):\n    def speak(self):\n        return self.name + \" barks\"\nd = Dog(\"Rex\")\nprint(d.speak(), d.kind(), d.name)",
    "class Bag:\n    def __init__(self):\n        self.items = []\n    def add(self, x):\n        self.items.append(x)\n    def total(self):\n        t = 0\n        for it in self.items:\n            t = t + it\n        return t\nb = Bag()\nb.add(10)\nb.add(20)\nb.add(5)\nprint(b.items, b.total())",
    "class Box:\n    def __init__(self, v):\n        self.v = v\ndef bump(box):\n    box.v = box.v + 1\nboxes = [Box(1), Box(2)]\nbump(boxes[0])\nbump(boxes[1])\nprint(boxes[0].v, boxes[1].v)",
    "class Animal:\n    def __init__(self, name):\n        self.name = name\n    def speak(self):\n        return self.name + \" makes a sound\"\nclass Dog(Animal):\n    def __init__(self, name):\n        super().__init__(name)\n        self.tricks = []\n    def speak(self):\n        return super().speak() + \"; \" + self.name + \" barks\"\n    def learn(self, t):\n        self.tricks.append(t)\nd = Dog(\"Rex\")\nd.learn(\"sit\")\nd.learn(\"roll\")\nprint(d.speak())\nprint(d.tricks)",
    "class Q:\n    def __init__(self, n):\n        self.n = n\n    def __str__(self):\n        return \"str-\" + str(self.n)\n    def __repr__(self):\n        return \"repr-\" + str(self.n)\nq = Q(5)\nprint(q)\nprint([q, Q(6)])\nprint({1: Q(7)})",
    // operator dunders: arithmetic, ==, repr
    "class V:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n    def __add__(self, o):\n        return V(self.x + o.x, self.y + o.y)\n    def __sub__(self, o):\n        return V(self.x - o.x, self.y - o.y)\n    def __mul__(self, k):\n        return V(self.x * k, self.y * k)\n    def __eq__(self, o):\n        return self.x == o.x and self.y == o.y\n    def __repr__(self):\n        return \"V(\" + str(self.x) + \", \" + str(self.y) + \")\"\na = V(1, 2)\nb = V(3, 4)\nprint(a + b)\nprint(b - a)\nprint(a * 3)\nprint(a == V(1, 2), a == b)",
    // ordered comparisons
    "class T:\n    def __init__(self, c):\n        self.c = c\n    def __lt__(self, o):\n        return self.c < o.c\n    def __le__(self, o):\n        return self.c <= o.c\n    def __gt__(self, o):\n        return self.c > o.c\n    def __ge__(self, o):\n        return self.c >= o.c\na = T(10)\nb = T(20)\nprint(a < b, a > b, a <= T(10), b >= a)",
    // __len__ and __getitem__
    "class Deck:\n    def __init__(self):\n        self.cards = [10, 20, 30]\n    def __len__(self):\n        return len(self.cards)\n    def __getitem__(self, i):\n        return self.cards[i]\nd = Deck()\nprint(len(d), d[0], d[2])",
    // default __eq__ is identity; __eq__ also drives `in`
    "class A:\n    def __init__(self):\n        self.x = 1\na = A()\nb = A()\nprint(a == a, a == b, a != b)",
    "class P:\n    def __init__(self, n):\n        self.n = n\n    def __eq__(self, o):\n        return self.n == o.n\nps = [P(1), P(2), P(3)]\nprint(P(2) in ps, P(9) in ps)",
    // class variables: shared default, instance read falls back to class
    "class Dog:\n    species = \"Canis familiaris\"\n    legs = 4\n    def __init__(self, name):\n        self.name = name\nd = Dog(\"Rex\")\ne = Dog(\"Fido\")\nprint(d.name, d.species, d.legs)\nprint(e.species, e.legs)",
    // class variable inherited through the base, and shadowed per instance
    "class Base:\n    kind = \"base\"\nclass Sub(Base):\n    def __init__(self):\n        self.n = 1\ns = Sub()\nprint(s.kind)\ns.kind = \"local\"\nt = Sub()\nprint(s.kind, t.kind)",
    // list slicing: bounds, steps, negatives, reversal, empties
    "xs = [0, 1, 2, 3, 4, 5]\nprint(xs[1:4], xs[:3], xs[3:], xs[:])\nprint(xs[::2], xs[::-1], xs[1:5:2])\nprint(xs[-2:], xs[:-2], xs[-3:-1])",
    "xs = [1, 2, 3]\nprint(xs[5:], xs[1:1], xs[3:1], xs[-10:2], xs[::-2])",
    "xs = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]\nprint(xs[8:2:-1], xs[::-3], xs[7:0:-2])",
    // string slicing
    "s = \"abcdef\"\nprint(s[1:4], s[::-1], s[::2], s[2:], s[:3], s[-2:])\nprint(s[1:5:2] + s[::-1])",
    // slices compose with other features (function, len, concat)
    "def mid(xs):\n    return xs[1:-1]\nprint(mid([10, 20, 30, 40]), mid(\"hello\"), len([1,2,3,4][::2]))",
    // list comprehensions: range, filters, sequences, nesting
    "print([x * x for x in range(6)])\nprint([x for x in range(10) if x % 2 == 0])",
    "print([c for c in \"hello\" if c != \"l\"])\nxs = [1, 2, 3, 4, 5]\nprint([x * 10 for x in xs], [x for x in xs if x > 2])",
    "print([x + y for x in range(3) for y in range(3)])\nprint([[y for y in range(x)] for x in range(4)])",
    "print([x for x in range(10) if x in [2, 4, 6]])\nprint([x for x in range(20, 0, -5)])",
    // dict comprehensions
    "print({x: x * x for x in range(5)})\nprint({c: 1 for c in \"aba\"})",
    // comprehension inside a function (fresh loop var, doesn't leak)
    "def squares(n):\n    return [i * i for i in range(n)]\nprint(squares(4), squares(0))",
    // comprehension composed with len / concat / slice
    "ns = [x for x in range(8) if x % 3 != 0]\nprint(ns, len(ns), ns[::-1])",
    // augmented assignment on variables, indices, attributes
    "x = 5\nx += 3\nx *= 2\nx -= 1\nprint(x)\nn = 17\nn //= 5\nn %= 2\nprint(n)\ny = 10.0\ny /= 4\nprint(y)",
    "xs = [1, 2, 3]\nxs[0] += 10\nxs[-1] *= 2\nprint(xs)\ns = \"a\"\ns += \"bc\"\nprint(s)",
    "counts = {}\nfor c in \"abracadabra\":\n    if c in counts:\n        counts[c] += 1\n    else:\n        counts[c] = 1\nprint(counts)",
    "class C:\n    def __init__(self):\n        self.n = 0\n    def add(self, k):\n        self.n += k\nc = C()\nc.add(5)\nc.add(3)\nprint(c.n)",
    // tuples: literals, indexing, len, singleton/empty, equality, membership
    "t = (1, 2, 3)\nprint(t, t[0], t[-1], len(t))\nprint((1,), (), (1, 2))",
    "print((1, 2) == (1, 2), (1, 2) == (1, 3), (1,) == [1])\nprint(2 in (1, 2, 3), 5 in (1, 2, 3))",
    // unpacking assignment, swap, from a list, to index/attr targets
    "a, b = 10, 20\nprint(a, b)\na, b = b, a\nprint(a, b)\nx, y, z = [1, 2, 3]\nprint(x, y, z)",
    "xs = [0, 0]\nxs[0], xs[1] = 5, 6\nprint(xs)",
    "class P:\n    def __init__(self):\n        self.x = 0\n        self.y = 0\np = P()\np.x, p.y = 3, 4\nprint(p.x, p.y)",
    // for-loop tuple targets + tuple element comprehension
    "for k, v in [(1, \"a\"), (2, \"b\")]:\n    print(k, v)\nprint([(x, x * x) for x in range(4)])",
    "total = 0\nfor a, b in [(1, 2), (3, 4), (5, 6)]:\n    total += a + b\nprint(total)",
    // returning a tuple, then unpacking it
    "def minmax(xs):\n    lo = xs[0]\n    hi = xs[0]\n    for v in xs:\n        if v < lo:\n            lo = v\n        if v > hi:\n            hi = v\n    return lo, hi\nlow, high = minmax([4, 1, 8, 3])\nprint(low, high)",
    // range as a value: iterate, sum, len, membership
    "r = range(3)\nfor x in r:\n    print(x)\nprint(sum(range(5)), len(range(10)), 2 in range(5), 9 in range(5))\nprint(sum(range(1, 101)))",
    // enumerate (default and custom start)
    "for i, c in enumerate(\"abc\"):\n    print(i, c)\nfor i, x in enumerate([10, 20], 1):\n    print(i, x)",
    // zip (stops at the shorter input)
    "for a, b in zip([1, 2, 3], \"ab\"):\n    print(a, b)\nprint([a + b for a, b in zip([1, 2], [10, 20])])",
    // dict views: iterate keys / values / items
    "d = {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in d.keys():\n    print(k)\nfor v in d.values():\n    print(v)\nfor k, v in d.items():\n    print(k, v)\nprint(sorted(d.keys()))",
    // sum and sorted
    "print(sum([1, 2, 3, 4]), sum([1.5, 2.5]))\nprint(sorted([3, 1, 2]), sorted([3, 1.5, 2]))\nprint(sorted(\"dcba\"), sorted([\"banana\", \"apple\", \"cherry\"]))",
    // tuple-target comprehensions over items()/zip
    "d = {\"a\": 1, \"b\": 2}\nprint({v: k for k, v in d.items()})\nprint([k for k, v in d.items() if v > 1])",
    "print([a * b for a, b in zip([1, 2, 3], [4, 5, 6])])",
    // string methods: split, strip, upper/lower, join
    "print(\"hello world\".split())\nprint(\"a,b,,c\".split(\",\"))\nprint(\"a b c\".split(\" \"))",
    "print(\"  trim me  \".strip(), \"Hello\".upper(), \"Hello\".lower())",
    "print(\"-\".join([\"a\", \"b\", \"c\"]), \",\".join([str(i) for i in range(4)]))",
    "print(\"\".split(), \"\".split(\",\"))\nwords = \"the quick brown fox\".split()\nprint(len(words), words[0], words[-1])",
    "print(sum([int(x) for x in \"1 2 3 4\".split()]))",
    // power operator (** binds tighter than unary minus, right-associative)
    "print(2 ** 10, 3 ** 0, 5 ** 1)\nprint(2 ** -1, 2.0 ** 3, 10 ** -2)\nprint(-2 ** 2, (-2) ** 2, 2 ** 3 ** 2)",
    "n = 5\nprint(n ** 2 + 1)\nb = 2\nb **= 5\nprint(b)\nprint([i ** 2 for i in range(5)])",
    // string replace / startswith / endswith
    "print(\"a-b-c\".replace(\"-\", \"+\"))\nprint(\"hello\".replace(\"l\", \"LL\"))\nprint(\"aaa\".replace(\"a\", \"\"))",
    "print(\"hello.py\".endswith(\".py\"), \"hello.py\".startswith(\"he\"), \"x\".startswith(\"xy\"))",
    // float() — strings (exactly-representable values) and numbers
    "print(float(\"2.5\"), float(\"-0.25\"), float(\"100\"), float(\"3.0\"))",
    "print(float(\"1e3\"), float(\"2.5e2\"), float(\"1.25e2\"))\nprint(float(5), float(True), float(2.5))",
    // dict .get / .pop and list .pop
    "d = {\"a\": 1, \"b\": 2}\nprint(d.get(\"a\"), d.get(\"z\"), d.get(\"z\", -1))\nprint(d.pop(\"a\"), d)\nprint(d.pop(\"z\", 99))",
    "xs = [10, 20, 30, 40]\nprint(xs.pop(), xs)\nprint(xs.pop(0), xs)",
    // any() / all()
    "print(any([0, 0, 1]), any([0, 0]), all([1, 1, 1]), all([1, 0]))\nprint(any([]), all([]))",
    "print(any([x > 2 for x in [1, 2, 3]]), all([x > 0 for x in [1, 2, 3]]))",
    // default arguments
    "def greet(name, greeting=\"Hello\"):\n    return greeting + \", \" + name + \"!\"\nprint(greet(\"Sam\"))\nprint(greet(\"Sam\", \"Hi\"))",
    "def f(a, b=1, c=2):\n    return a + b + c\nprint(f(10), f(10, 20), f(10, 20, 30))",
    // min/max over an iterable or several args; bool(); round() to int
    "print(min([3, 1, 2]), max([3, 1, 2]), min(5, 2, 8), max(5, 2, 8))\nprint(min(\"banana\", \"apple\"), max([1.5, 2, 0.5]))",
    "print(bool(0), bool(1), bool(\"\"), bool(\"x\"), bool([]), bool([1]))",
    "print(round(2.5), round(3.5), round(2.4), round(-2.5), round(0.5))",
    // math module (sqrt is correctly-rounded in both WASM and CPython)
    "import math\nprint(math.sqrt(16), math.sqrt(2))\nprint(math.floor(3.7), math.ceil(3.2), math.trunc(-3.7))\nprint(math.fabs(-5.5))",
    "import math\nprint(round(math.pi, 5), round(math.e, 5), int(math.sqrt(144)))",
    // generator expressions as sole call arguments (materialized as lists)
    "print(sum(x * x for x in range(5)))\nprint(any(x > 3 for x in [1, 2, 3, 4]), all(x > 0 for x in [1, 2, 3]))",
    "print(max(len(w) for w in [\"a\", \"bbb\", \"cc\"]))\nprint(sorted(x % 3 for x in range(6)))",
    "words = [\"hi\", \"world\"]\nprint(\" \".join(w.upper() for w in words))",
    // string count / find / isdigit / isalpha
    "print(\"abracadabra\".count(\"a\"), \"abracadabra\".count(\"bra\"), \"xyz\".count(\"q\"))",
    "print(\"hello world\".find(\"world\"), \"hello\".find(\"z\"))",
    "print(\"123\".isdigit(), \"12a\".isdigit(), \"abc\".isalpha(), \"ab1\".isalpha(), \"\".isdigit())",
    // capitalize / title / lstrip / rstrip / zfill
    "print(\"hello world\".capitalize(), \"hello world\".title())\nprint(\"the QUICK fox\".title())",
    "print(\"  hi  \".lstrip() + \"|\", \"|\" + \"  hi  \".rstrip())\nprint(\"5\".zfill(3), \"-5\".zfill(3), \"+7\".zfill(4))",
    // keyword arguments (order-independent; can skip a middle default)
    "def rect(w, h=1, label=\"r\"):\n    return label + str(w * h)\nprint(rect(5), rect(5, 2), rect(5, label=\"x\"))\nprint(rect(w=4, h=3), rect(h=3, w=4))",
    "def f(a, b, c):\n    return a * 100 + b * 10 + c\nprint(f(1, c=3, b=2), f(c=3, a=1, b=2))",
    // sets — only order-independent operations (CPython set order is hash-based)
    "print(len(set([3, 1, 2, 1, 3])))\ns = set([1, 2, 3])\nprint(2 in s, 5 in s)",
    "print(set([1, 2]) == set([2, 1]), set([1]) == set([1, 2]))\nprint(sorted(set([3, 1, 2, 1])))",
    "seen = set()\nfor x in [1, 2, 2, 3, 1]:\n    seen.add(x)\nprint(len(seen), sorted(seen))",
    "print(sorted({x % 3 for x in range(10)}))\nprint(sorted(set(\"mississippi\")))",
    "print(len({1, 1, 2, 3, 3}), sorted({c for c in \"hello\"}))",
    // set operations: union | / intersection & / difference - / sym-diff ^
    "a = set([1, 2, 3, 4])\nb = set([3, 4, 5, 6])\nprint(sorted(a | b), sorted(a & b))\nprint(sorted(a - b), sorted(b - a), sorted(a ^ b))",
    "s = {1, 2}\ns |= {2, 3, 4}\nprint(sorted(s))\nprint(sorted({1, 2, 3} - {2} | {5}))",
    "print(sorted({1, 2, 3, 4} & {2, 4, 6}), sorted({1, 2} | {3} & {3, 4}))",
    // list methods: sort, reverse, insert, extend, count, index
    "xs = [3, 1, 2]\nxs.sort()\nprint(xs)\nxs.reverse()\nprint(xs)\nxs.insert(1, 99)\nprint(xs)\nxs.extend([7, 8])\nprint(xs)",
    "print([1, 2, 2, 3, 2].count(2), [\"a\", \"b\", \"c\"].index(\"b\"))\nprint((1, 2, 3).count(2), (5, 6, 7).index(7))",
    "words = [\"banana\", \"apple\", \"cherry\"]\nwords.sort()\nprint(words)\nnums = [5, 3, 8, 1]\nnums.sort()\nprint(nums, nums.index(8))",
    "print(\"banana\".count(\"a\"), \"banana\".find(\"na\"), \"banana\".index(\"ana\"))",
    // set methods (accept iterables) + subset/superset
    "a = {1, 2, 3}\nb = {2, 3, 4}\nprint(sorted(a.union(b)), sorted(a.intersection(b)))\nprint(sorted(a.difference(b)), sorted(a.symmetric_difference(b)))",
    "print({1, 2, 3}.issubset({1, 2, 3, 4}), {1, 2, 3}.issuperset({1, 2}), {1, 2}.issubset({1, 2, 3}))\nprint(sorted({1, 2}.union([3, 4, 2])))",
    // dict methods
    "d = {\"a\": 1}\nd.update({\"b\": 2, \"a\": 10})\nprint(d)\nd.setdefault(\"c\", 3)\nd.setdefault(\"a\", 99)\nprint(d, d.setdefault(\"a\"))\nd.clear()\nprint(d, len(d))",
    // str.format(): auto/indexed fields, specs, literal braces
    "print(\"{} + {} = {}\".format(2, 3, 5))\nprint(\"{0} {1} {0}\".format(\"a\", \"b\"))",
    "print(\"{:.2f}\".format(3.14159), \"[{:>5}]\".format(42))\nprint(\"{{x}} {} {}\".format(7, 8))",
    "print(\"name={}, score={:.1f}\".format(\"Sam\", 9.5))\nprint(\"{} items\".format(len([1, 2, 3])))",
    // f-string format specs: precision, width, alignment, zero-pad
    "print(f\"{3.14159:.2f}\", f\"{3.14159:.4f}\")\nprint(f\"pi is about {3.14159:.3f}\")",
    "print(f\"[{42:5}]\", f\"[{42:<5}]\", f\"[{42:^5}]\", f\"[{7:03}]\")",
    "print(f\"[{'hi':>6}]\", f\"[{'hi':<6}]\", f\"[{'hi':^6}]\")",
    "n = 5\nprint(f\"{n} squared is {n * n:d}\")\nprint(f\"{255:d} {0:03d}\")",
    "print(format(7, \"03\"), format(3.5, \".1f\"), format(\"ab\", \">5\"))",
    // str() / repr() of collections (no floats inside — str(float) unsupported)
    "print(str([1, 2, 3]), str((1, 2)), str((9,)))\nprint(str({\"a\": 1, \"b\": 2}))",
    "print(str([1, \"two\", True, None]), str([[1, 2], [3]]))",
    "print(\"nums: \" + str([1, 2, 3]))\nprint(f\"list is {[1, 2, 3]} and dict {{'k': 1}}\")",
    "d = {\"x\": [1, 2], \"y\": 3}\nprint(str(d))\nprint(repr(\"hi\"), repr([1, \"a\"]))",
    "print(str(sorted(set([3, 1, 2, 1]))))",
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

/// `(program, stdin)` pairs exercising `input()` / `int(input())`.
const STDIN_CORPUS: &[(&str, &str)] = &[
    ("print(\"Hello, \" + input() + \"!\")", "World\n"),
    ("n = int(input())\nprint(n * n)", "9\n"),
    ("print(input()[::-1])", "stressed\n"),
    (
        "total = 0\nfor i in range(int(input())):\n    total += int(input())\nprint(total)",
        "4\n1\n2\n3\n4\n",
    ),
    (
        "names = [input() for i in range(int(input()))]\nprint(sorted(names))",
        "3\ncharlie\nalice\nbob\n",
    ),
    ("print(int(input()) + int(input()))", "40\n2\n"),
    // the classic "two ints on one line", now possible via split()
    ("a, b = input().split()\nprint(int(a) + int(b))", "20 22\n"),
    (
        "print(sum([int(x) for x in input().split()]))",
        "5 10 15 20\n",
    ),
];

#[test]
fn differential_stdin_against_cpython() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Some(python) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    for (src, stdin) in STDIN_CORPUS {
        let mut child = Command::new(python)
            .args(["-c", src])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn python");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        let out = child.wait_with_output().expect("run python");
        assert!(
            out.status.success(),
            "CPython rejected:\n{src}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = String::from_utf8(out.stdout)
            .expect("python output is UTF-8")
            .replace("\r\n", "\n");
        let (got, result) = execute_io(src, stdin);
        assert_eq!(result.expect("trapped"), 0);
        assert_eq!(got, expected, "differs from CPython for:\n{src}");
    }
}
