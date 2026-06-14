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

/// Compile and execute `src`; returns everything written via the host
/// imports plus `_start`'s result (Err = trapped).
fn execute(src: &str) -> (String, Result<i32, wasmtime::Error>) {
    let wat = rust_p2w::compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
    let wasm = wat::parse_str(&wat).unwrap_or_else(|e| panic!("invalid WAT: {e}\n---\n{wat}"));

    let module = Module::new(engine(), &wasm[..]).expect("module");
    // Store data is the output byte buffer; write_char sends UTF-8 bytes.
    let mut store: Store<Vec<u8>> = Store::new(engine(), Vec::new());
    let mut linker: Linker<Vec<u8>> = Linker::new(engine());
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
    let result = start.call(&mut store, ());
    let out = String::from_utf8(store.into_data()).expect("output is UTF-8");
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
    assert_raises(
        "print(str(2.5))",
        "TypeError: str() of 'float' values isn't supported yet",
    );
    assert_raises(
        "print(str([1]))",
        "TypeError: str() of 'list' values isn't supported yet",
    );
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
    assert!(format!("{err}").contains("super()"), "{err}");
}

#[test]
fn super_without_a_base_is_an_error() {
    let err = rust_p2w::compile_to_wat("class A:\n    def m(self):\n        return super().m()")
        .unwrap_err();
    assert!(format!("{err}").contains("base class"), "{err}");
}

#[test]
fn class_redefinition_is_an_error() {
    let err = rust_p2w::compile_to_wat("class A:\n    def m(self):\n        return 1\nclass A:\n    def m(self):\n        return 2").unwrap_err();
    assert!(format!("{err}").contains("defined twice"), "{err}");
}

#[test]
fn unknown_base_class_is_an_error() {
    let err =
        rust_p2w::compile_to_wat("class Dog(Animal):\n    def speak(self):\n        return 1")
            .unwrap_err();
    assert!(format!("{err}").contains("unknown base class"), "{err}");
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
