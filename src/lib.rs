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

/// Whether `source` could create a reference cycle — and so leak under plain
/// reference counting. A `false` result is a guarantee that the program is
/// cycle-free (RC frees everything); unparseable source is conservatively `true`.
/// This is the seam for a `--no-mutation` fast path and for surfacing a
/// "leak-free" signal in the editor. See `MEMORY_MANAGEMENT.md`.
pub fn may_form_cycle(source: &str) -> bool {
    match lexer::lex(source).and_then(|t| parser::parse(&t)) {
        Ok(stmts) => lint::may_form_cycle(&stmts),
        Err(_) => true,
    }
}

/// Names bound to a set somewhere in `source` — the seam the IDE uses to decide
/// when `&`/`|`/`-`/`^` should *display* as set-theory glyphs (∩ ∪ ∖ ∆), since
/// those are also int/bitwise operators (see `acornstem-ide/SET_NOTATION_SPEC.md`,
/// Part 2). Uses error-recovering parse so a half-typed program still classifies;
/// returns empty when nothing lexes.
pub fn set_typed_names(source: &str) -> Vec<String> {
    match lexer::lex(source) {
        Ok(toks) => {
            let (stmts, _) = parser::parse_recovering(&toks);
            lint::set_typed_names(&stmts)
        }
        Err(_) => Vec::new(),
    }
}

/// Byte spans `[start, end)` of the `& | - ^` operators that are *set* operations
/// (both operands are sets), so the IDE can render exactly those as set-theory
/// glyphs (∩ ∪ ∖ ∆) while leaving int bitwise / subtraction as ASCII. Unlike a
/// token heuristic this is precedence- and parenthesis-correct (`(A | B) & C`,
/// `set(...)` results, nested set ops). Offsets index into `source`. See
/// `acornstem-ide/SET_NOTATION_SPEC.md` (Part 2). Empty when `source` doesn't lex.
pub fn set_operator_spans(source: &str) -> Vec<(usize, usize)> {
    match lexer::lex(source) {
        Ok(toks) => {
            let (stmts, _) = parser::parse_recovering(&toks);
            lint::set_operator_spans(&stmts)
        }
        Err(_) => Vec::new(),
    }
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
    fn errors_carry_a_byte_span_for_underlining() {
        // Lexer error: the span points exactly at the bad character.
        let src = "print(3 $ 4)";
        let err = try_compile(src).unwrap_err();
        let (s, e) = err.span.expect("lexer error should carry a span");
        assert_eq!(&src[s..e], "$");

        // Parser error: the span covers the unexpected token.
        let src = "x = 1 2\n";
        let err = try_compile(src).unwrap_err();
        let (s, e) = err.span.expect("parser error should carry a span");
        assert!(s < e && e <= src.len(), "valid range: {s}..{e}");
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
    fn interactive_web_seam_compiles() {
        // Layer 1 of the interactive-web backend: a zero-arg handler passed to
        // on_click (function-as-id), plus the effect builtins. See
        // docs/INTERACTIVE_WEB.md.
        let src = "def boom():\n    flash()\n    beep()\non_click(boom)\n";
        let wat = compile_to_wat(src).unwrap();
        assert!(wat.contains(r#"(import "env" "on_click""#), "{wat}");
        assert!(wat.contains(r#"(import "env" "flash""#), "{wat}");
        assert!(wat.contains(r#"(import "env" "beep""#), "{wat}");
        assert!(
            wat.contains(r#"(export "__dispatch")"#),
            "dispatch export: {wat}"
        );
        // `boom` is the only zero-arg def -> dispatch id 0, passed boxed.
        assert!(
            wat.contains("(call $box (i32.const 0))"),
            "handler-as-id: {wat}"
        );
        // And it's all valid WASM.
        assert_valid_wasm(src);
        // A normal program emits none of the DOM imports (host stays minimal).
        assert!(!compile_to_wat("print(1)\n").unwrap().contains("on_click"));
    }

    #[test]
    fn interactive_web_string_ops_compile() {
        // Layer 3: string-argument capabilities marshal each string via
        // $marshal_str, then call the op. See docs/INTERACTIVE_WEB.md.
        let src = "def grow():\n    set_attr(\"#box\", \"fill\", \"gold\")\n    set_text(\"#msg\", \"hi\")\n    play_sound(\"beep\")\non(\"#box\", \"click\", grow)\n";
        let wat = compile_to_wat(src).unwrap();
        for imp in [
            r#"(import "env" "s_byte""#,
            r#"(import "env" "dom_set_attr""#,
            r#"(import "env" "dom_set_text""#,
            r#"(import "env" "play_sound""#,
            r#"(import "env" "dom_on""#,
        ] {
            assert!(wat.contains(imp), "missing {imp}: {wat}");
        }
        assert!(wat.contains("call $marshal_str"), "marshalling: {wat}");
        assert!(wat.contains("(func $marshal_str"), "marshal helper: {wat}");
        assert_valid_wasm(src);

        // get_value reads a value back (reverse marshalling).
        let gv = "name = get_value(\"#name\")\nset_text(\"#msg\", name)\n";
        let wat = compile_to_wat(gv).unwrap();
        assert!(wat.contains(r#"(import "env" "gv_fetch""#), "{wat}");
        assert!(wat.contains("(func $get_value"), "get_value helper: {wat}");
        assert_valid_wasm(gv);

        // every(ms, handler): the animation/game loop (numeric args + dispatch).
        let ev = "def step():\n    set_attr(\"#b\", \"cx\", \"5\")\nevery(30, step)\n";
        let wat = compile_to_wat(ev).unwrap();
        assert!(wat.contains(r#"(import "env" "every""#), "{wat}");
        assert!(wat.contains(r#"(export "__dispatch")"#), "{wat}");
        assert_valid_wasm(ev);
        // String ops are gated separately: a flash/beep-only program emits none
        // of the string-marshalling machinery (host stays minimal).
        let noarg = compile_to_wat("def b():\n    flash()\non_click(b)\n").unwrap();
        assert!(!noarg.contains("dom_set_attr"));
        assert!(!noarg.contains("$marshal_str"));
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
