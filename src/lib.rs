//! rust-p2w — a Rust reimplementation of the p2w Python-subset -> WebAssembly
//! (WAT) compiler, for the AcornSTEM K-12 IDE.
//!
//! Pipeline: source -> [lexer] -> tokens -> [parser] -> spanned AST ->
//! [codegen] -> WAT (via the emit module's module/function builders).
//!
//! Derived from MIT-licensed p2w (semantics / WAT conventions) and informed by
//! the design of ruff_python_parser (front-end architecture). See the NOTICE
//! file for full attribution.

mod ast;
mod blockly;
mod codegen;
mod debug;
mod emit;
mod error;
mod lexer;
mod lint;
mod llvm;
mod parser;

pub use ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
pub use blockly::{BlocksOutcome, to_blockly_json, to_blocks};
pub use debug::{Status, Stepper, Value, Vm};
pub use error::CompileError;

/// Compile Python (the supported subset) to WebAssembly text (WAT).
///
/// Returns a friendly, line-numbered error string on failure — suitable to
/// show a K-12 student directly.
pub fn compile_to_wat(source: &str) -> Result<String, String> {
    try_compile(source).map_err(|e| e.to_string())
}

/// Like [`compile_to_wat`], but returns the structured error (line +
/// message) so callers can highlight the offending line (e.g. the IDE).
pub fn try_compile(source: &str) -> Result<String, CompileError> {
    let tokens = lexer::lex(source)?;
    let stmts = parser::parse(&tokens)?;
    codegen::generate(&stmts)
}

/// Compile Python to textual LLVM IR — Phase 0 of the native Pico 2 W backend
/// (the integer subset; see `PICO_BACKEND.md`). Text only: turning this into an
/// RP2350 binary is a later, toolchain-gated phase.
pub fn compile_to_llvm_ir(source: &str) -> Result<String, String> {
    let tokens = lexer::lex(source).map_err(|e| e.to_string())?;
    let stmts = parser::parse(&tokens).map_err(|e| e.to_string())?;
    llvm::emit_llvm_ir(&stmts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_hello() {
        let wat = compile_to_wat("print(\"hello world\")").unwrap();
        assert!(wat.contains("(export \"_start\")"));
        assert!(wat.contains("call $write_char"));
    }

    #[test]
    fn end_to_end_math_and_strings() {
        let src = "print(\"answer:\", 6 * 7)\nprint(100 - 1)";
        let wat = compile_to_wat(src).unwrap();
        assert!(wat.contains("call $write_i32"));
        assert!(wat.contains("(call $py_mul (ref.i31 (i32.const 6)) (ref.i31 (i32.const 7)))"));
    }

    #[test]
    fn errors_are_friendly() {
        let err = compile_to_wat("print(3 $ 4)").unwrap_err();
        assert!(err.contains("unexpected character"));
        assert!(err.starts_with("line 1:"));
    }

    #[test]
    fn structured_errors_carry_the_line() {
        let err = try_compile("x = 1\nprint(\"ok\")\nprint(zzz)\n").unwrap_err();
        assert_eq!(err.line, Some(3));
        assert!(err.message.contains("zzz"));
    }

    #[test]
    fn full_program_with_loop_and_if() {
        // A realistic K-12 program should compile to a runnable module.
        let src = "\
total = 0
for i in range(1, 6):
    if i % 2 == 0:
        total = total + i
print(\"sum of evens:\", total)
";
        let wat = compile_to_wat(src).unwrap();
        assert!(wat.contains("(export \"_start\")"));
        assert!(wat.contains("loop $l0"));
        assert!(wat.contains("i32.rem_s"));
    }

    /// Validate that emitted WAT is well-formed by parsing it to WASM with the
    /// same crate the IDE runner uses. Catches folded-form mistakes that
    /// string-contains assertions would miss.
    fn assert_valid_wasm(src: &str) {
        let wat = compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
        wat::parse_str(&wat).unwrap_or_else(|e| panic!("emitted WAT is invalid: {e}\n---\n{wat}"));
    }

    #[test]
    fn emitted_wat_parses_print() {
        assert_valid_wasm("print(\"hi\", 6 * 7)\nx = 5\nprint(x)\n");
    }

    #[test]
    fn emitted_wat_parses_if_elif_else() {
        assert_valid_wasm(
            "x = 2\nif x < 1:\n    print(1)\nelif x < 3:\n    print(2)\nelse:\n    print(3)\n",
        );
    }

    #[test]
    fn emitted_wat_parses_nested_for() {
        assert_valid_wasm("for i in range(3):\n    for j in range(3):\n        print(i * j)\n");
    }

    #[test]
    fn emitted_wat_parses_while_break_continue() {
        assert_valid_wasm(
            "i = 0\nwhile i < 10:\n    i = i + 1\n    if i % 2 == 0:\n        continue\n    if i > 7:\n        break\n    print(i)\n",
        );
    }

    #[test]
    fn emitted_wat_parses_fizzbuzz_core() {
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
        assert_valid_wasm(src);
    }
}
