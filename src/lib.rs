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
mod builtins;
mod codegen;
mod debug;
mod emit;
mod error;
mod evidence;
mod lexer;
mod lint;
mod llvm;
mod parser;
mod reuse;

pub use ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
pub use blockly::{BlocksOutcome, to_blockly_json, to_blocks};
pub use builtins::{BUILTINS, Builtin, builtins_json};
pub use debug::{Status, Stepper, Value, Vm};
pub use error::CompileError;
pub use evidence::{Concept, concept_evidence, concept_vocab};

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

        // on_key(keyname, handler): keyboard for games (forward string + dispatch).
        let key = "def left():\n    set_attr(\"#b\", \"cx\", \"1\")\non_key(\"ArrowLeft\", left)\n";
        let wat = compile_to_wat(key).unwrap();
        assert!(wat.contains(r#"(import "env" "key_on""#), "{wat}");
        assert!(
            wat.contains("call $marshal_str"),
            "key name marshalled: {wat}"
        );
        assert_valid_wasm(key);
        // String ops are gated separately: a flash/beep-only program emits none
        // of the string-marshalling machinery (host stays minimal).
        let noarg = compile_to_wat("def b():\n    flash()\non_click(b)\n").unwrap();
        assert!(!noarg.contains("dom_set_attr"));
        assert!(!noarg.contains("$marshal_str"));
    }

    #[test]
    fn emit_html_marshals_to_the_host() {
        // Rich output: emit_html(s) marshals the string and calls env.emit_html.
        let src = "emit_html(\"<b>hi</b>\")\n";
        let wat = compile_to_wat(src).unwrap();
        assert!(wat.contains(r#"(import "env" "emit_html""#), "{wat}");
        assert!(wat.contains("call $marshal_str"), "marshalling: {wat}");
        assert!(wat.contains("call $emit_html"), "{wat}");
        assert_valid_wasm(src);
        // A program that doesn't use it emits no emit_html import.
        assert!(!compile_to_wat("print(1)\n").unwrap().contains("emit_html"));
    }

    #[test]
    fn repr_html_method_returns_a_string() {
        // Layer 2 prerequisite: a class can define a method that returns a
        // string, and that string can feed emit_html. (The `_repr_html_`
        // protocol baseline — see docs/RICH_OUTPUT.md.)
        let src = "class Model:\n    def _repr_html_(self):\n        return \"<b>model</b>\"\n\nm = Model()\nemit_html(m._repr_html_())\n";
        let wat = compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
        assert!(wat.contains(r#"(import "env" "emit_html""#), "{wat}");
        assert_valid_wasm(src);
    }

    #[test]
    fn show_dispatches_repr_html_or_text() {
        // show(obj) emits the $show helper, which renders _repr_html_() as HTML
        // (emit_html) when present and falls back to printing as text.
        let src = "class Model:\n    def _repr_html_(self):\n        return \"<b>m</b>\"\n\nshow(Model())\nshow(42)\n";
        let wat = compile_to_wat(src).unwrap_or_else(|e| panic!("compile failed: {e}"));
        assert!(wat.contains("(func $show "), "{wat}");
        assert!(
            wat.contains(r#"(import "env" "emit_html""#),
            "show implies emit_html"
        );
        assert!(
            wat.contains("call $class_lookup_method"),
            "runtime method lookup"
        );
        assert!(wat.contains("call $print_value"), "text fallback: {wat}");
        assert_valid_wasm(src);
        // No show -> no $show helper.
        assert!(
            !compile_to_wat("print(1)\n")
                .unwrap()
                .contains("(func $show ")
        );
    }

    #[test]
    fn chain_map_consumes_the_dying_source() {
        // Drop-reuse (step 3): both chain stages consume their dying source's
        // buffer — each emits a runtime unique() guard.
        let ir = compile_to_llvm_ir(
            "a: list[int] = [1, 2, 3]\nb = [x + 1 for x in a]\nc = [y * 2 for y in b]\nprint(c)\n",
        )
        .unwrap();
        assert_eq!(ir.matches("call i1 @p2w_unique").count(), 2, "{ir}");
        // Source still read later -> no death token -> no reuse guard.
        let ir2 = compile_to_llvm_ir(
            "a: list[int] = [1, 2, 3]\nb = [x + 1 for x in a]\nprint(a)\nprint(b)\n",
        )
        .unwrap();
        assert_eq!(ir2.matches("call i1 @p2w_unique").count(), 0, "{ir2}");
        // A borrowed param's buffer is never stolen, even when it dies in the
        // callee (rc==1 is the CALLER's count) — no guard emitted.
        let ir3 = compile_to_llvm_ir(
            "def dbl(xs: list[int]) -> int:\n    b = [x * 2 for x in xs]\n    return b[0]\nys: list[int] = [3, 4]\nprint(dbl(ys))\nprint(ys)\n",
        )
        .unwrap();
        assert_eq!(ir3.matches("call i1 @p2w_unique").count(), 0, "{ir3}");
        // A non-whitelisted element (str(x) isn't int-typed) never adopts the
        // packed buffer.
        let ir4 = compile_to_llvm_ir("a: list[int] = [1, 2]\nb = [str(x) for x in a]\nprint(b)\n")
            .unwrap();
        assert_eq!(ir4.matches("call i1 @p2w_unique").count(), 0, "{ir4}");
    }

    #[test]
    fn literal_reassignment_reuses_in_place() {
        // Assign-site drop-reuse: reassigning with a literal emits the runtime
        // can-reuse guard (tag + unique + exact length).
        let ir = compile_to_llvm_ir("xs = [1, 2]\nxs = [3, 4]\nprint(xs)\n").unwrap();
        assert_eq!(ir.matches("call i1 @p2w_can_reuse_list").count(), 1, "{ir}");
        // Annotated slots use the packed predicate.
        let ir2 = compile_to_llvm_ir("ys: list[int] = [1, 2]\nys = [3, 4]\nprint(ys)\n").unwrap();
        assert_eq!(
            ir2.matches("call i1 @p2w_can_reuse_iarray").count(),
            1,
            "{ir2}"
        );
        // Elements reading the container must NOT reuse (swap, not smear).
        let ir3 = compile_to_llvm_ir("xs = [1, 2]\nxs = [xs[1], xs[0]]\nprint(xs)\n").unwrap();
        assert_eq!(ir3.matches("call i1 @p2w_can_reuse").count(), 0, "{ir3}");
        // A first assignment (no old value) has nothing to reuse.
        let ir4 = compile_to_llvm_ir("xs = [1, 2]\nprint(xs)\n").unwrap();
        assert_eq!(ir4.matches("call i1 @p2w_can_reuse").count(), 0, "{ir4}");
    }

    #[test]
    fn self_concat_consumes_the_old_value() {
        // `s = s + "x"` on a Boxed slot lowers to p2w_add_assign (the old value
        // consumed as a reuse token: in-place growth when unique).
        let ir = compile_to_llvm_ir("s = \"\"\ns = s + \"x\"\nprint(s)\n").unwrap();
        assert_eq!(ir.matches("call i32 @p2w_add_assign").count(), 1, "{ir}");
        // A different target isn't the pattern. (Match the CALL form — the
        // declare line exists in every module's runtime ABI header.)
        let ir2 = compile_to_llvm_ir("s = \"a\"\nt = s + \"x\"\nprint(t)\n").unwrap();
        assert_eq!(ir2.matches("call i32 @p2w_add_assign").count(), 0, "{ir2}");
        // Typed int slots take the native add, never the dynamic path.
        let ir3 = compile_to_llvm_ir("n: int = 1\nn = n + 1\nprint(n)\n").unwrap();
        assert_eq!(ir3.matches("call i32 @p2w_add_assign").count(), 0, "{ir3}");
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
