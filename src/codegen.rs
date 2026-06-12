//! WAT code generation — boxed WASM-GC value model.
//!
//! Every runtime value is a `(ref null eq)` (the universal type), following
//! the reference p2w compiler's boxed design:
//!
//! - small ints are `i31ref`; ints outside the 31-bit range spill to an
//!   `$INT` struct (the `$box`/`$unbox` helpers pick at runtime)
//! - `True`/`False` are the `$TRUE`/`$FALSE` singleton structs, so they
//!   print as `True`/`False` while still counting as 1/0 in arithmetic
//! - `print` dispatches on the runtime type via `$print_value`
//!
//! Compiler-internal loop counters and bound snapshots stay raw `i32`
//! locals — the dynamic model applies to *Python* values, not bookkeeping.
//! Conditions compile through `$truthy` (or a direct i32 comparison when the
//! expression is statically boolean-shaped).
//!
//! Output conventions mirror p2w's runnable module shape so the same browser
//! harness can execute it: the body is an exported `_start` returning an i32
//! exit code (0), and output goes through host imports `env.write_char(i32)`
//! / `env.write_i32(i32)`.
//!
//! Structure: `Gen` holds module-wide state (which runtime helpers are used);
//! `FuncCx` holds per-function state (locals, labels, loop stack) — `_start`
//! is the only function today, but `def` lands as one `FuncCx` per function.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::emit::{Body, Func, Module};
use crate::error::CompileError;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, CompileError>;

/// The universal boxed value type.
const VAL: &str = "(ref null eq)";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    /// Definitely a number/bool (literals and arithmetic results).
    Num,
    /// Definitely a string (literals and concatenations of them).
    Str,
    /// Unknown until runtime (variables, `==`, `and`/`or`).
    Value,
}

type Vars = HashMap<String, Ty>;

/// Largest/smallest ints that fit an i31ref.
const I31_MAX: i64 = (1 << 30) - 1;
const I31_MIN: i64 = -(1 << 30);

pub fn generate(stmts: &[Stmt]) -> Result<String> {
    let mut g = Gen::default();

    // Pass 1: collect function signatures so calls (including mutual
    // recursion) resolve regardless of definition order.
    for s in stmts {
        if let StmtKind::Def { name, params, .. } = &s.kind {
            if name == "print" {
                return Err(CompileError::at(s.line, "can't redefine print"));
            }
            if g.funcs.insert(name.clone(), params.len()).is_some() {
                return Err(CompileError::at(
                    s.line,
                    format!("function '{name}' is defined twice"),
                ));
            }
        }
    }

    // Pass 2a: top-level statements become _start; top-level variables are
    // module globals so functions can read them.
    let mut cx = FuncCx {
        is_top: true,
        ..Default::default()
    };
    let mut body = Body::new();
    for s in stmts {
        if !matches!(s.kind, StmtKind::Def { .. }) {
            g.stmt(&mut cx, s, &mut body)?;
        }
    }
    body.push("(i32.const 0)");

    // Pass 2b: each def becomes its own function (after 2a, so every module
    // global is known).
    let mut user_funcs = Vec::new();
    for s in stmts {
        if let StmtKind::Def { name, params, body } = &s.kind {
            user_funcs.push(g.gen_def(name, params, body)?);
        }
    }

    let mut module = Module::default();
    module.types.push("(type $INT (struct (field i32)))".into());
    module.types.push("(type $BOOL (struct (field i8)))".into());
    module
        .types
        .push("(type $FLOAT (struct (field f64)))".into());
    module.types.push("(type $NONE_T (struct))".into());
    // NOTE: WASM-GC type canonicalization is structural — keep these
    // shapes distinct or ref.test misfires (see $BOOL's i8 field; $NONE_T
    // must stay the only fieldless struct).
    module.types.push("(type $STR (array (mut i8)))".into());
    module
        .types
        .push("(type $ITEMS (array (mut (ref null eq))))".into());
    // A list is a Vec: logical length + a capacity-sized item array.
    module
        .types
        .push("(type $LIST (struct (field (mut i32)) (field (mut (ref null $ITEMS)))))".into());
    // A dict is an insertion-ordered association: parallel key/value arrays
    // with linear-scan lookup (classroom-sized; order matches Python).
    module.types.push(
        "(type $DICT (struct (field (mut i32)) (field (mut (ref null $ITEMS))) (field (mut (ref null $ITEMS)))))".into(),
    );
    module
        .imports
        .push(r#"(import "env" "write_char" (func $write_char (param i32)))"#.into());
    module
        .imports
        .push(r#"(import "env" "write_i32" (func $write_i32 (param i32)))"#.into());
    module
        .imports
        .push(r#"(import "env" "write_f64" (func $write_f64 (param f64)))"#.into());
    module
        .globals
        .push("(global $TRUE (ref $BOOL) (struct.new $BOOL (i32.const 1)))".into());
    module
        .globals
        .push("(global $FALSE (ref $BOOL) (struct.new $BOOL (i32.const 0)))".into());
    module
        .globals
        .push("(global $NONE (ref $NONE_T) (struct.new $NONE_T))".into());
    for name in &g.globals {
        module.globals.push(format!(
            "(global $g_{name} (mut (ref null eq)) (ref.null eq))"
        ));
    }

    module.funcs.push(Func {
        signature: r#"(func $_start (export "_start") (result i32)"#.into(),
        locals: cx
            .locals
            .iter()
            .map(|(name, ty)| format!("(local ${name} {ty})"))
            .collect(),
        body,
    });
    for f in user_funcs {
        module.funcs.push(f);
    }
    for f in runtime_helpers() {
        module.funcs.push(f);
    }
    if g.uses_floordiv {
        module.funcs.push(py_floordiv_helper());
        module.funcs.push(floordiv_helper());
    }
    if g.uses_floormod {
        module.funcs.push(py_mod_helper());
        module.funcs.push(floormod_helper());
    }
    Ok(module.render())
}

/// The always-present boxed-value runtime: box/unbox/bool/truthy/print.
fn runtime_helpers() -> Vec<Func> {
    let mut fs = Vec::new();

    // $box: i32 -> value (i31 when it fits, $INT struct otherwise).
    let mut b = Body::new();
    b.push("(if (result (ref null eq))");
    b.push_in(
        2,
        "(i32.eq (i32.shr_s (i32.shl (local.get $v) (i32.const 1)) (i32.const 1)) (local.get $v))",
    );
    b.push_in(1, "(then (ref.i31 (local.get $v)))");
    b.push_in(1, "(else (struct.new $INT (local.get $v))))");
    fs.push(Func {
        signature: "(func $box (param $v i32) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $unbox: value -> i32 (i31, $BOOL as 0/1, or $INT; traps on null).
    let mut b = Body::new();
    b.push("(if (result i32) (ref.test (ref i31) (local.get $r))");
    b.push_in(1, "(then (i31.get_s (ref.cast (ref i31) (local.get $r))))");
    b.push_in(1, "(else");
    b.push_in(2, "(if (result i32) (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(
        3,
        "(then (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r))))",
    );
    b.push_in(
        3,
        "(else (struct.get $INT 0 (ref.cast (ref $INT) (local.get $r)))))))",
    );
    fs.push(Func {
        signature: "(func $unbox (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $bool: i32 (0/1) -> the singleton $TRUE/$FALSE.
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (local.get $v)");
    b.push_in(1, "(then (global.get $TRUE))");
    b.push_in(1, "(else (global.get $FALSE)))");
    fs.push(Func {
        signature: "(func $bool (param $v i32) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $unbox_f64: any numeric value as f64 (ints/bools convert exactly).
    let mut b = Body::new();
    b.push("(if (result f64) (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r))))",
    );
    b.push_in(
        1,
        "(else (f64.convert_i32_s (call $unbox (local.get $r)))))",
    );
    fs.push(Func {
        signature: "(func $unbox_f64 (param $r (ref null eq)) (result f64)".into(),
        locals: vec![],
        body: b,
    });

    // $either_float: arithmetic promotes to float when either side is one.
    let mut b = Body::new();
    b.push(
        "(i32.or (ref.test (ref $FLOAT) (local.get $a)) (ref.test (ref $FLOAT) (local.get $b)))",
    );
    fs.push(Func {
        signature:
            "(func $either_float (param $a (ref null eq)) (param $b (ref null eq)) (result i32)"
                .into(),
        locals: vec![],
        body: b,
    });

    // $truthy: value -> i32 0/1 (non-empty string / nonzero number is true).
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i32.ne (array.len (ref.cast (ref $STR) (local.get $r))) (i32.const 0))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (f64.ne (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r))) (f64.const 0))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $NONE_T) (local.get $r))");
    b.push_in(1, "(then (return (i32.const 0)))"); // None is falsy
    b.push(")");
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i32.ne (struct.get $LIST 0 (ref.cast (ref $LIST) (local.get $r))) (i32.const 0))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i32.ne (struct.get $DICT 0 (ref.cast (ref $DICT) (local.get $r))) (i32.const 0))))",
    );
    b.push(")");
    b.push("(i32.ne (call $unbox (local.get $r)) (i32.const 0))");
    fs.push(Func {
        signature: "(func $truthy (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $py_len: sequence length (lists, dicts, strings).
    let mut b = Body::new();
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get $LIST 0 (ref.cast (ref $LIST) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get $DICT 0 (ref.cast (ref $DICT) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.len (ref.cast (ref $STR) (local.get $r)))))",
    );
    b.push(")");
    b.push("unreachable"); // len() of a non-sequence
    fs.push(Func {
        signature: "(func $py_len (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $py_index: subscript read with Python negative-index normalization;
    // out of range traps (Python raises IndexError). Strings yield a
    // one-character string.
    let mut b = Body::new();
    b.push("(local.set $n (call $py_len (local.get $r)))");
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(
        1,
        "(then (local.set $i (i32.add (local.get $i) (local.get $n))))",
    );
    b.push(")");
    b.push("(if (i32.or (i32.lt_s (local.get $i) (i32.const 0)) (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(1, "(then unreachable)");
    b.push(")");
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $LIST 1 (ref.cast (ref $LIST) (local.get $r))) (local.get $i))))",
    );
    b.push(")");
    // Positional access on a dict yields its i-th KEY — this is what makes
    // `for k in d:` iterate keys in insertion order.
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $DICT 1 (ref.cast (ref $DICT) (local.get $r))) (local.get $i))))",
    );
    b.push(")");
    b.push("(local.set $c (array.new_default $STR (i32.const 1)))");
    b.push("(array.set $STR (local.get $c) (i32.const 0) (array.get_u $STR (ref.cast (ref $STR) (local.get $r)) (local.get $i)))");
    b.push("(local.get $c)");
    fs.push(Func {
        signature: "(func $py_index (param $r (ref null eq)) (param $i i32) (result (ref null eq))"
            .into(),
        locals: vec!["(local $n i32)".into(), "(local $c (ref null $STR))".into()],
        body: b,
    });

    // $py_set_index: `xs[i] = v` (lists only; same index rules as reads).
    let mut b = Body::new();
    b.push("(local.set $n (call $py_len (local.get $r)))");
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(
        1,
        "(then (local.set $i (i32.add (local.get $i) (local.get $n))))",
    );
    b.push(")");
    b.push("(if (i32.or (i32.lt_s (local.get $i) (i32.const 0)) (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(1, "(then unreachable)");
    b.push(")");
    b.push("(array.set $ITEMS (struct.get $LIST 1 (ref.cast (ref $LIST) (local.get $r))) (local.get $i) (local.get $v))");
    fs.push(Func {
        signature:
            "(func $py_set_index (param $r (ref null eq)) (param $i i32) (param $v (ref null eq))"
                .into(),
        locals: vec!["(local $n i32)".into()],
        body: b,
    });

    // $list_append: amortized growth (double-ish, min 8); returns None like
    // Python's append.
    let mut b = Body::new();
    b.push("(local.set $lst (ref.cast (ref $LIST) (local.get $l)))");
    b.push("(local.set $items (struct.get $LIST 1 (local.get $lst)))");
    b.push("(local.set $len (struct.get $LIST 0 (local.get $lst)))");
    b.push("(if (i32.ge_s (local.get $len) (array.len (local.get $items)))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(local.set $new (array.new_default $ITEMS (i32.shl (i32.add (array.len (local.get $items)) (i32.const 4)) (i32.const 1))))",
    );
    b.push_in(
        2,
        "(array.copy $ITEMS $ITEMS (local.get $new) (i32.const 0) (local.get $items) (i32.const 0) (local.get $len))",
    );
    b.push_in(2, "(struct.set $LIST 1 (local.get $lst) (local.get $new))");
    b.push_in(2, "(local.set $items (local.get $new))");
    b.push_in(1, ")");
    b.push(")");
    b.push("(array.set $ITEMS (local.get $items) (local.get $len) (local.get $v))");
    b.push("(struct.set $LIST 0 (local.get $lst) (i32.add (local.get $len) (i32.const 1)))");
    b.push("(global.get $NONE)");
    fs.push(Func {
        signature:
            "(func $list_append (param $l (ref null eq)) (param $v (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $lst (ref null $LIST))".into(),
            "(local $items (ref null $ITEMS))".into(),
            "(local $new (ref null $ITEMS))".into(),
            "(local $len i32)".into(),
        ],
        body: b,
    });

    // $dict_find: index of a key (by py_eq), or -1.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $DICT 0 (local.get $d)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (call $py_eq (array.get $ITEMS (struct.get $DICT 1 (local.get $d)) (local.get $i)) (local.get $k))",
    );
    b.push_in(3, "(then (return (local.get $i)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const -1)");
    fs.push(Func {
        signature:
            "(func $dict_find (param $d (ref null $DICT)) (param $k (ref null eq)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $dict_get: value for a key; missing key traps (Python's KeyError).
    let mut b = Body::new();
    b.push("(local.set $dict (ref.cast (ref $DICT) (local.get $d)))");
    b.push("(local.set $i (call $dict_find (local.get $dict) (local.get $k)))");
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(1, "(then unreachable)");
    b.push(")");
    b.push("(array.get $ITEMS (struct.get $DICT 2 (local.get $dict)) (local.get $i))");
    fs.push(Func {
        signature:
            "(func $dict_get (param $d (ref null eq)) (param $k (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $dict (ref null $DICT))".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // $dict_set: update an existing key or append a new entry (growing both
    // parallel arrays together).
    let mut b = Body::new();
    b.push("(local.set $dict (ref.cast (ref $DICT) (local.get $d)))");
    b.push("(local.set $i (call $dict_find (local.get $dict) (local.get $k)))");
    b.push("(if (i32.ge_s (local.get $i) (i32.const 0))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(array.set $ITEMS (struct.get $DICT 2 (local.get $dict)) (local.get $i) (local.get $v))",
    );
    b.push_in(2, "(return)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.set $len (struct.get $DICT 0 (local.get $dict)))");
    b.push("(local.set $keys (struct.get $DICT 1 (local.get $dict)))");
    b.push("(local.set $vals (struct.get $DICT 2 (local.get $dict)))");
    b.push("(if (i32.ge_s (local.get $len) (array.len (local.get $keys)))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(local.set $cap (i32.shl (i32.add (array.len (local.get $keys)) (i32.const 4)) (i32.const 1)))",
    );
    b.push_in(
        2,
        "(local.set $nk (array.new_default $ITEMS (local.get $cap)))",
    );
    b.push_in(
        2,
        "(local.set $nv (array.new_default $ITEMS (local.get $cap)))",
    );
    b.push_in(
        2,
        "(array.copy $ITEMS $ITEMS (local.get $nk) (i32.const 0) (local.get $keys) (i32.const 0) (local.get $len))",
    );
    b.push_in(
        2,
        "(array.copy $ITEMS $ITEMS (local.get $nv) (i32.const 0) (local.get $vals) (i32.const 0) (local.get $len))",
    );
    b.push_in(2, "(struct.set $DICT 1 (local.get $dict) (local.get $nk))");
    b.push_in(2, "(struct.set $DICT 2 (local.get $dict) (local.get $nv))");
    b.push_in(2, "(local.set $keys (local.get $nk))");
    b.push_in(2, "(local.set $vals (local.get $nv))");
    b.push_in(1, ")");
    b.push(")");
    b.push("(array.set $ITEMS (local.get $keys) (local.get $len) (local.get $k))");
    b.push("(array.set $ITEMS (local.get $vals) (local.get $len) (local.get $v))");
    b.push("(struct.set $DICT 0 (local.get $dict) (i32.add (local.get $len) (i32.const 1)))");
    fs.push(Func {
        signature:
            "(func $dict_set (param $d (ref null eq)) (param $k (ref null eq)) (param $v (ref null eq))"
                .into(),
        locals: vec![
            "(local $dict (ref null $DICT))".into(),
            "(local $i i32)".into(),
            "(local $len i32)".into(),
            "(local $cap i32)".into(),
            "(local $keys (ref null $ITEMS))".into(),
            "(local $vals (ref null $ITEMS))".into(),
            "(local $nk (ref null $ITEMS))".into(),
            "(local $nv (ref null $ITEMS))".into(),
        ],
        body: b,
    });

    // $py_subscript / $py_set_subscript: general `obj[key]` — dicts take the
    // key as a value; lists/strings unbox it as a position.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $dict_get (local.get $r) (local.get $k))))",
    );
    b.push(")");
    b.push("(call $py_index (local.get $r) (call $unbox (local.get $k)))");
    fs.push(Func {
        signature:
            "(func $py_subscript (param $r (ref null eq)) (param $k (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });
    let mut b = Body::new();
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $dict_set (local.get $r) (local.get $k) (local.get $v))))",
    );
    b.push(")");
    b.push("(call $py_set_index (local.get $r) (call $unbox (local.get $k)) (local.get $v))");
    fs.push(Func {
        signature:
            "(func $py_set_subscript (param $r (ref null eq)) (param $k (ref null eq)) (param $v (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });

    // $dict_eq: same keys and values, order-insensitive (Python).
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $DICT 0 (local.get $a)))");
    b.push("(if (i32.ne (local.get $n) (struct.get $DICT 0 (local.get $b)))");
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $j (call $dict_find (local.get $b) (array.get $ITEMS (struct.get $DICT 1 (local.get $a)) (local.get $i))))",
    );
    b.push_in(2, "(if (i32.lt_s (local.get $j) (i32.const 0))");
    b.push_in(3, "(then (return (i32.const 0)))");
    b.push_in(2, ")");
    b.push_in(2, "(if (i32.eqz (call $py_eq");
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $DICT 2 (local.get $a)) (local.get $i))",
    );
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $DICT 2 (local.get $b)) (local.get $j))))",
    );
    b.push_in(3, "(then (return (i32.const 0)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 1)");
    fs.push(Func {
        signature:
            "(func $dict_eq (param $a (ref null $DICT)) (param $b (ref null $DICT)) (result i32)"
                .into(),
        locals: vec![
            "(local $i i32)".into(),
            "(local $j i32)".into(),
            "(local $n i32)".into(),
        ],
        body: b,
    });

    // $print_dict: `{'k': v, ...}` with repr for both keys and values.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 123))"); // {
    b.push("(local.set $n (struct.get $DICT 0 (local.get $d)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(if (i32.gt_s (local.get $i) (i32.const 0))");
    b.push_in(
        3,
        "(then (call $write_char (i32.const 44)) (call $write_char (i32.const 32)))",
    );
    b.push_in(2, ")");
    b.push_in(
        2,
        "(call $print_repr (array.get $ITEMS (struct.get $DICT 1 (local.get $d)) (local.get $i)))",
    );
    b.push_in(2, "(call $write_char (i32.const 58))"); // :
    b.push_in(2, "(call $write_char (i32.const 32))");
    b.push_in(
        2,
        "(call $print_repr (array.get $ITEMS (struct.get $DICT 2 (local.get $d)) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(call $write_char (i32.const 125))"); // }
    fs.push(Func {
        signature: "(func $print_dict (param $d (ref null $DICT))".into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $print_repr: element form used inside list printing — strings get
    // quotes (Python repr), everything else prints as itself.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(2, "(call $write_char (i32.const 39))"); // '
    b.push_in(2, "(call $print_str (ref.cast (ref $STR) (local.get $r)))");
    b.push_in(2, "(call $write_char (i32.const 39))");
    b.push_in(2, "(return)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(call $print_value (local.get $r))");
    fs.push(Func {
        signature: "(func $print_repr (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $print_list: `[e1, e2, ...]` with repr elements (nested lists recurse
    // through $print_value -> $print_list).
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 91))"); // [
    b.push("(local.set $n (struct.get $LIST 0 (local.get $l)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(if (i32.gt_s (local.get $i) (i32.const 0))");
    b.push_in(
        3,
        "(then (call $write_char (i32.const 44)) (call $write_char (i32.const 32)))",
    );
    b.push_in(2, ")");
    b.push_in(
        2,
        "(call $print_repr (array.get $ITEMS (struct.get $LIST 1 (local.get $l)) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(call $write_char (i32.const 93))"); // ]
    fs.push(Func {
        signature: "(func $print_list (param $l (ref null $LIST))".into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $list_eq: element-wise equality (recurses through $py_eq).
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $LIST 0 (local.get $a)))");
    b.push("(if (i32.ne (local.get $n) (struct.get $LIST 0 (local.get $b)))");
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(if (i32.eqz (call $py_eq");
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $LIST 1 (local.get $a)) (local.get $i))",
    );
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $LIST 1 (local.get $b)) (local.get $i))))",
    );
    b.push_in(3, "(then (return (i32.const 0)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 1)");
    fs.push(Func {
        signature:
            "(func $list_eq (param $a (ref null $LIST)) (param $b (ref null $LIST)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $print_str: write a string's bytes through write_char.
    let mut b = Body::new();
    b.push("(local.set $n (array.len (local.get $s)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_u (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(call $write_char (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    fs.push(Func {
        signature: "(func $print_str (param $s (ref null $STR))".into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $print_value: runtime type dispatch — strings as bytes, floats via the
    // host (which formats Python-style), bools as True/False, ints as digits.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_str (ref.cast (ref $STR) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $write_f64 (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r))))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $NONE_T) (local.get $r))");
    b.push_in(1, "(then");
    for c in "None".bytes() {
        b.push_in(2, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(1, "(return))");
    b.push(")");
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_list (ref.cast (ref $LIST) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_dict (ref.cast (ref $DICT) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(if (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r)))",
    );
    b.push_in(3, "(then");
    for c in "True".bytes() {
        b.push_in(4, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(3, ")");
    b.push_in(3, "(else");
    for c in "False".bytes() {
        b.push_in(4, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(3, ")))");
    b.push_in(1, "(else (call $write_i32 (call $unbox (local.get $r)))))");
    fs.push(Func {
        signature: "(func $print_value (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $list_concat: `[1] + [2]` makes a new list.
    let mut b = Body::new();
    b.push("(local.set $na (struct.get $LIST 0 (local.get $a)))");
    b.push("(local.set $nb (struct.get $LIST 0 (local.get $b)))");
    b.push(
        "(local.set $items (array.new_default $ITEMS (i32.add (local.get $na) (local.get $nb))))",
    );
    b.push("(array.copy $ITEMS $ITEMS (local.get $items) (i32.const 0) (struct.get $LIST 1 (local.get $a)) (i32.const 0) (local.get $na))");
    b.push("(array.copy $ITEMS $ITEMS (local.get $items) (local.get $na) (struct.get $LIST 1 (local.get $b)) (i32.const 0) (local.get $nb))");
    b.push("(struct.new $LIST (i32.add (local.get $na) (local.get $nb)) (local.get $items))");
    fs.push(Func {
        signature:
            "(func $list_concat (param $a (ref null $LIST)) (param $b (ref null $LIST)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $na i32)".into(),
            "(local $nb i32)".into(),
            "(local $items (ref null $ITEMS))".into(),
        ],
        body: b,
    });

    // $py_add: Python `+` — list/string concatenation when both sides
    // match, numeric addition otherwise.
    let mut b = Body::new();
    b.push(
        "(if (i32.and (ref.test (ref $LIST) (local.get $a)) (ref.test (ref $LIST) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $list_concat (ref.cast (ref $LIST) (local.get $a)) (ref.cast (ref $LIST) (local.get $b)))))",
    );
    b.push(")");
    b.push("(if (result (ref null eq))");
    b.push_in(
        2,
        "(i32.and (ref.test (ref $STR) (local.get $a)) (ref.test (ref $STR) (local.get $b)))",
    );
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $sa (ref.cast (ref $STR) (local.get $a)))");
    b.push_in(2, "(local.set $sb (ref.cast (ref $STR) (local.get $b)))");
    b.push_in(
        2,
        "(local.set $out (array.new_default $STR (i32.add (array.len (local.get $sa)) (array.len (local.get $sb)))))",
    );
    b.push_in(
        2,
        "(array.copy $STR $STR (local.get $out) (i32.const 0) (local.get $sa) (i32.const 0) (array.len (local.get $sa)))",
    );
    b.push_in(
        2,
        "(array.copy $STR $STR (local.get $out) (array.len (local.get $sa)) (local.get $sb) (i32.const 0) (array.len (local.get $sb)))",
    );
    b.push_in(2, "(local.get $out)");
    b.push_in(1, ")");
    b.push_in(1, "(else");
    b.push_in(
        2,
        "(if (result (ref null eq)) (call $either_float (local.get $a) (local.get $b))",
    );
    b.push_in(
        3,
        "(then (struct.new $FLOAT (f64.add (call $unbox_f64 (local.get $a)) (call $unbox_f64 (local.get $b)))))",
    );
    b.push_in(
        3,
        "(else (call $box (i32.add (call $unbox (local.get $a)) (call $unbox (local.get $b))))))))",
    );
    fs.push(Func {
        signature:
            "(func $py_add (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $sa (ref null $STR))".into(),
            "(local $sb (ref null $STR))".into(),
            "(local $out (ref null $STR))".into(),
        ],
        body: b,
    });

    // $py_sub / $py_mul: float promotion, else i32.
    for (name, f_instr, i_instr) in [
        ("$py_sub", "f64.sub", "i32.sub"),
        ("$py_mul", "f64.mul", "i32.mul"),
    ] {
        let mut b = Body::new();
        b.push("(if (result (ref null eq)) (call $either_float (local.get $a) (local.get $b))");
        b.push_in(
            1,
            format!("(then (struct.new $FLOAT ({f_instr} (call $unbox_f64 (local.get $a)) (call $unbox_f64 (local.get $b)))))"),
        );
        b.push_in(
            1,
            format!("(else (call $box ({i_instr} (call $unbox (local.get $a)) (call $unbox (local.get $b))))))"),
        );
        fs.push(Func {
            signature: format!(
                "(func {name} (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
            ),
            locals: vec![],
            body: b,
        });
    }

    // $py_div: Python `/` — always float; division by zero traps loudly
    // (Python raises ZeroDivisionError; silent inf would be a wrong answer).
    let mut b = Body::new();
    b.push("(local.set $fb (call $unbox_f64 (local.get $b)))");
    b.push("(if (f64.eq (local.get $fb) (f64.const 0))");
    b.push_in(1, "(then unreachable)");
    b.push(")");
    b.push("(struct.new $FLOAT (f64.div (call $unbox_f64 (local.get $a)) (local.get $fb)))");
    fs.push(Func {
        signature:
            "(func $py_div (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec!["(local $fb f64)".into()],
        body: b,
    });

    // $py_neg: unary minus across int/float.
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (struct.new $FLOAT (f64.neg (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r))))))",
    );
    b.push_in(
        1,
        "(else (call $box (i32.sub (i32.const 0) (call $unbox (local.get $r))))))",
    );
    fs.push(Func {
        signature: "(func $py_neg (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $str_eq: byte-wise string equality.
    let mut b = Body::new();
    b.push("(local.set $n (array.len (local.get $a)))");
    b.push("(if (i32.ne (local.get $n) (array.len (local.get $b)))");
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_u (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (i32.ne (array.get_u $STR (local.get $a) (local.get $i)) (array.get_u $STR (local.get $b) (local.get $i)))",
    );
    b.push_in(3, "(then (return (i32.const 0)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 1)");
    fs.push(Func {
        signature:
            "(func $str_eq (param $a (ref null $STR)) (param $b (ref null $STR)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $py_eq: Python `==` — None only equals None; strings by value,
    // string-vs-number is False; numbers (ints, bools as 1/0, floats)
    // compared as f64 (exact for i32).
    let mut b = Body::new();
    b.push("(if (i32.or (ref.test (ref $NONE_T) (local.get $a)) (ref.test (ref $NONE_T) (local.get $b)))");
    b.push_in(
        1,
        "(then (return (i32.and (ref.test (ref $NONE_T) (local.get $a)) (ref.test (ref $NONE_T) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.and (ref.test (ref $LIST) (local.get $a)) (ref.test (ref $LIST) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $list_eq (ref.cast (ref $LIST) (local.get $a)) (ref.cast (ref $LIST) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.or (ref.test (ref $LIST) (local.get $a)) (ref.test (ref $LIST) (local.get $b)))",
    );
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push(
        "(if (i32.and (ref.test (ref $DICT) (local.get $a)) (ref.test (ref $DICT) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $dict_eq (ref.cast (ref $DICT) (local.get $a)) (ref.cast (ref $DICT) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.or (ref.test (ref $DICT) (local.get $a)) (ref.test (ref $DICT) (local.get $b)))",
    );
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push(
        "(if (i32.and (ref.test (ref $STR) (local.get $a)) (ref.test (ref $STR) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $str_eq (ref.cast (ref $STR) (local.get $a)) (ref.cast (ref $STR) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.or (ref.test (ref $STR) (local.get $a)) (ref.test (ref $STR) (local.get $b)))",
    );
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push("(f64.eq (call $unbox_f64 (local.get $a)) (call $unbox_f64 (local.get $b)))");
    fs.push(Func {
        signature: "(func $py_eq (param $a (ref null eq)) (param $b (ref null eq)) (result i32)"
            .into(),
        locals: vec![],
        body: b,
    });

    fs
}

/// `//` dispatch: floats floor-divide as f64 (zero divisor traps); ints use
/// the `$i32_floordiv` helper.
fn py_floordiv_helper() -> Func {
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (call $either_float (local.get $a) (local.get $b))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $fb (call $unbox_f64 (local.get $b)))");
    b.push_in(
        2,
        "(if (f64.eq (local.get $fb) (f64.const 0)) (then unreachable))",
    );
    b.push_in(
        2,
        "(struct.new $FLOAT (f64.floor (f64.div (call $unbox_f64 (local.get $a)) (local.get $fb))))",
    );
    b.push_in(1, ")");
    b.push_in(
        1,
        "(else (call $box (call $i32_floordiv (call $unbox (local.get $a)) (call $unbox (local.get $b))))))",
    );
    Func {
        signature:
            "(func $py_floordiv (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec!["(local $fb f64)".into()],
        body: b,
    }
}

/// `%` dispatch: float modulo follows Python (`a - floor(a/b)*b`, sign of the
/// divisor; zero divisor traps); ints use the `$i32_floormod` helper.
fn py_mod_helper() -> Func {
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (call $either_float (local.get $a) (local.get $b))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $fa (call $unbox_f64 (local.get $a)))");
    b.push_in(2, "(local.set $fb (call $unbox_f64 (local.get $b)))");
    b.push_in(
        2,
        "(if (f64.eq (local.get $fb) (f64.const 0)) (then unreachable))",
    );
    b.push_in(
        2,
        "(struct.new $FLOAT (f64.sub (local.get $fa) (f64.mul (f64.floor (f64.div (local.get $fa) (local.get $fb))) (local.get $fb))))",
    );
    b.push_in(1, ")");
    b.push_in(
        1,
        "(else (call $box (call $i32_floormod (call $unbox (local.get $a)) (call $unbox (local.get $b))))))",
    );
    Func {
        signature:
            "(func $py_mod (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec!["(local $fa f64)".into(), "(local $fb f64)".into()],
        body: b,
    }
}

/// Python floor division: truncating `i32.div_s` adjusted by -1 when the
/// signs differ and the division isn't exact (`-7 // 2` is -4, not -3).
fn floordiv_helper() -> Func {
    let mut b = Body::new();
    b.push("(local.set $q (i32.div_s (local.get $a) (local.get $b)))");
    b.push("(if (i32.and");
    b.push_in(
        2,
        "(i32.ne (i32.rem_s (local.get $a) (local.get $b)) (i32.const 0))",
    );
    b.push_in(
        2,
        "(i32.ne (i32.lt_s (local.get $a) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))",
    );
    b.push_in(
        1,
        "(then (local.set $q (i32.sub (local.get $q) (i32.const 1)))))",
    );
    b.push("(local.get $q)");
    Func {
        signature: "(func $i32_floordiv (param $a i32) (param $b i32) (result i32)".into(),
        locals: vec!["(local $q i32)".into()],
        body: b,
    }
}

/// Python modulo: the result takes the sign of the divisor (`-7 % 2` is 1).
fn floormod_helper() -> Func {
    let mut b = Body::new();
    b.push("(local.set $r (i32.rem_s (local.get $a) (local.get $b)))");
    b.push("(if (i32.and");
    b.push_in(2, "(i32.ne (local.get $r) (i32.const 0))");
    b.push_in(
        2,
        "(i32.ne (i32.lt_s (local.get $r) (i32.const 0)) (i32.lt_s (local.get $b) (i32.const 0))))",
    );
    b.push_in(
        1,
        "(then (local.set $r (i32.add (local.get $r) (local.get $b)))))",
    );
    b.push("(local.get $r)");
    Func {
        signature: "(func $i32_floormod (param $a i32) (param $b i32) (result i32)".into(),
        locals: vec!["(local $r i32)".into()],
        body: b,
    }
}

/// Module-wide codegen state.
#[derive(Default)]
struct Gen {
    uses_floordiv: bool,
    uses_floormod: bool,
    /// User functions: name -> arity (collected before any body compiles).
    funcs: HashMap<String, usize>,
    /// Top-level Python variables, in definition order — WASM globals named
    /// `$g_<name>` so function bodies can read them.
    globals: Vec<String>,
}

impl Gen {
    fn ensure_global(&mut self, name: &str) {
        if !self.globals.iter().any(|g| g == name) {
            self.globals.push(name.to_string());
        }
    }

    fn is_global(&self, name: &str) -> bool {
        self.globals.iter().any(|g| g == name)
    }
}

/// Per-function codegen state.
#[derive(Default)]
struct FuncCx {
    /// True for _start: assignments target module globals, `return` is an
    /// error.
    is_top: bool,
    vars: Vars,
    /// `(name, wat_type)` — Python variables are boxed values; compiler
    /// bookkeeping (loop counters, bound snapshots) stays raw i32.
    locals: Vec<(String, String)>,
    label: usize,
    scratch: usize,
    /// Enclosing loops as `(break_label, continue_label)`, innermost last.
    /// In a `for`, continue targets the inner `$c` block so the counter
    /// increment still runs; in a `while`, it targets the loop head (re-test).
    loops: Vec<(String, String)>,
}

impl FuncCx {
    fn fresh(&mut self) -> usize {
        let n = self.label;
        self.label += 1;
        n
    }

    /// A fresh compiler-internal local (`.`-prefixed, so it can't collide
    /// with a Python variable name).
    fn scratch_local(&mut self, ty: &str) -> String {
        let name = format!(".t{}", self.scratch);
        self.scratch += 1;
        self.locals.push((name.clone(), ty.to_string()));
        name
    }

    fn ensure_local(&mut self, name: &str) {
        if !self.vars.contains_key(name) {
            self.vars.insert(name.to_string(), Ty::Value);
            self.locals.push((name.to_string(), VAL.to_string()));
        }
    }
}

impl Gen {
    fn stmts(&mut self, cx: &mut FuncCx, stmts: &[Stmt], out: &mut Body) -> Result<()> {
        for s in stmts {
            self.stmt(cx, s, out)?;
        }
        Ok(())
    }

    fn stmt(&mut self, cx: &mut FuncCx, s: &Stmt, out: &mut Body) -> Result<()> {
        match &s.kind {
            StmtKind::Assign(name, expr) => {
                self.type_of(cx, expr)?; // surface literal-misuse errors
                let value = self.value_expr(cx, expr)?;
                if cx.is_top {
                    self.ensure_global(name);
                    out.push(format!("(global.set $g_{name} {value})"));
                } else {
                    // Function locals are pre-registered by gen_def.
                    out.push(format!("(local.set ${name} {value})"));
                }
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => self.gen_print(cx, args, out),
                ExprKind::Call(..) | ExprKind::MethodCall(..) => {
                    // A bare call: evaluate for its effects, drop the result.
                    let v = self.value_expr(cx, e)?;
                    out.push(format!("(drop {v})"));
                    Ok(())
                }
                _ => Err(CompileError::at(
                    s.line,
                    "a bare value on its own line has no effect; did you mean print(...)?",
                )),
            },
            StmtKind::Def { .. } => Err(CompileError::at(
                s.line,
                "functions can only be defined at the top level (not inside \
                 another function, loop, or if)",
            )),
            StmtKind::Return(value) => {
                if cx.is_top {
                    return Err(CompileError::at(
                        s.line,
                        "'return' can only be used inside a function",
                    ));
                }
                let v = match value {
                    Some(e) => {
                        self.type_of(cx, e)?;
                        self.value_expr(cx, e)?
                    }
                    None => "(global.get $NONE)".to_string(),
                };
                out.push(format!("(return {v})"));
                Ok(())
            }
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => self.gen_if(cx, cond, body, elifs, else_body, out),
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => self.gen_for(cx, var, start, end, step, body, s.line, out),
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => self.gen_foreach(cx, var, iterable, body, out),
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                self.type_of(cx, value)?;
                let t = self.value_expr(cx, target)?;
                let k = self.value_expr(cx, index)?;
                let v = self.value_expr(cx, value)?;
                out.push(format!("(call $py_set_subscript {t} {k} {v})"));
                Ok(())
            }
            StmtKind::While { cond, body } => self.gen_while(cx, cond, body, out),
            StmtKind::Break => match cx.loops.last() {
                Some((brk, _)) => {
                    out.push(format!("(br {brk})"));
                    Ok(())
                }
                None => Err(CompileError::at(
                    s.line,
                    "'break' can only be used inside a loop",
                )),
            },
            StmtKind::Continue => match cx.loops.last() {
                Some((_, cont)) => {
                    out.push(format!("(br {cont})"));
                    Ok(())
                }
                None => Err(CompileError::at(
                    s.line,
                    "'continue' can only be used inside a loop",
                )),
            },
        }
    }

    fn gen_print(&mut self, cx: &mut FuncCx, args: &[Expr], out: &mut Body) -> Result<()> {
        for (idx, arg) in args.iter().enumerate() {
            if idx > 0 {
                emit_char(out, b' ');
            }
            self.type_of(cx, arg)?; // surface literal-misuse errors
            if let ExprKind::Str(s) = &arg.kind {
                // Literal fast path: no allocation, identical output bytes.
                for byte in s.bytes() {
                    emit_char(out, byte);
                }
            } else {
                let v = self.value_expr(cx, arg)?;
                out.push(format!("(call $print_value {v})"));
            }
        }
        emit_char(out, b'\n');
        Ok(())
    }

    fn gen_if(
        &mut self,
        cx: &mut FuncCx,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
        out: &mut Body,
    ) -> Result<()> {
        self.type_of(cx, cond)?; // any value is a condition (strings: non-empty)
        let c = self.cond_i32(cx, cond)?;
        let mut then_b = Body::new();
        self.stmts(cx, body, &mut then_b)?;
        let else_b = self.else_chain(cx, elifs, else_body)?;

        out.push(format!("(if {c}"));
        out.push_in(1, "(then");
        out.append(then_b, 2);
        out.push_in(1, ")");
        if let Some(e) = else_b {
            out.push_in(1, "(else");
            out.append(e, 2);
            out.push_in(1, ")");
        }
        out.push(")");
        Ok(())
    }

    /// The else-side of an if: an elif chain lowers to a nested if inside the
    /// else. Returns None when there is no else at all.
    fn else_chain(
        &mut self,
        cx: &mut FuncCx,
        elifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<Option<Body>> {
        if let Some(((cond, body), rest)) = elifs.split_first() {
            self.type_of(cx, cond)?;
            let c = self.cond_i32(cx, cond)?;
            let mut then_b = Body::new();
            self.stmts(cx, body, &mut then_b)?;
            let inner = self.else_chain(cx, rest, else_body)?;

            let mut b = Body::new();
            b.push(format!("(if {c}"));
            b.push_in(1, "(then");
            b.append(then_b, 2);
            b.push_in(1, ")");
            if let Some(e) = inner {
                b.push_in(1, "(else");
                b.append(e, 2);
                b.push_in(1, ")");
            }
            b.push(")");
            Ok(Some(b))
        } else if let Some(body) = else_body {
            let mut b = Body::new();
            self.stmts(cx, body, &mut b)?;
            Ok(Some(b))
        } else {
            Ok(None)
        }
    }

    fn gen_while(
        &mut self,
        cx: &mut FuncCx,
        cond: &Expr,
        body: &[Stmt],
        out: &mut Body,
    ) -> Result<()> {
        self.type_of(cx, cond)?;
        let c = self.cond_i32(cx, cond)?;
        let n = cx.fresh();

        cx.loops.push((format!("$b{n}"), format!("$l{n}")));
        let mut body_b = Body::new();
        let r = self.stmts(cx, body, &mut body_b);
        cx.loops.pop();
        r?;

        out.push(format!("(block $b{n}"));
        out.push_in(1, format!("(loop $l{n}"));
        out.push_in(2, format!("(br_if $b{n} (i32.eqz {c}))"));
        out.append(body_b, 2);
        out.push_in(2, format!("(br $l{n})"));
        out.push_in(1, ")");
        out.push(")");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_for(
        &mut self,
        cx: &mut FuncCx,
        var: &str,
        start: &Expr,
        end: &Expr,
        step: &Expr,
        body: &[Stmt],
        line: usize,
        out: &mut Body,
    ) -> Result<()> {
        self.require_value(cx, start, "a range start")?;
        self.require_value(cx, end, "a range end")?;
        if [start, end, step].iter().any(|b| const_float(b).is_some()) {
            return Err(CompileError::at(
                line,
                "range() needs whole numbers, not decimals",
            ));
        }

        // A runtime step would need a sign-aware termination check; until that
        // lands, only constant steps are accepted (so the direction is known).
        let step_v = match const_int(step) {
            Some(0) => return Err(CompileError::at(line, "range() step can't be zero")),
            Some(v) => {
                i32::try_from(v).map_err(|_| CompileError::at(line, "range() step is too big"))?
            }
            None => {
                return Err(CompileError::at(
                    line,
                    "the range() step must be a plain number for now",
                ))
            }
        };
        let done_cmp = if step_v > 0 { "i32.ge_s" } else { "i32.le_s" };

        // Counter and bounds are compiler bookkeeping: raw i32, not boxed.
        let start_wat = self.i32_expr(cx, start)?;
        // Python evaluates range() bounds once, before the loop — snapshot a
        // non-constant end so the body mutating its variables can't change
        // the iteration count.
        let end_wat = self.i32_expr(cx, end)?;
        let end_operand = if const_int(end).is_some() {
            end_wat
        } else {
            let snap = cx.scratch_local("i32");
            out.push(format!("(local.set ${snap} {end_wat})"));
            format!("(local.get ${snap})")
        };

        // Iterate a hidden counter and assign it (boxed) to the loop variable
        // at the top of each pass, so reassigning the variable in the body
        // doesn't change the iteration count (matching Python). The variable
        // is a function-level local, visible after the loop.
        let n = cx.fresh();
        let ctr = format!(".f{n}");
        cx.locals.push((ctr.clone(), "i32".to_string()));
        let set_var = if cx.is_top {
            self.ensure_global(var);
            format!("(global.set $g_{var} (call $box (local.get ${ctr})))")
        } else {
            format!("(local.set ${var} (call $box (local.get ${ctr})))")
        };

        cx.loops.push((format!("$b{n}"), format!("$c{n}")));
        let mut body_b = Body::new();
        let r = self.stmts(cx, body, &mut body_b);
        cx.loops.pop();
        r?;

        out.push(format!("(local.set ${ctr} {start_wat})"));
        out.push(format!("(block $b{n}"));
        out.push_in(1, format!("(loop $l{n}"));
        out.push_in(
            2,
            format!("(br_if $b{n} ({done_cmp} (local.get ${ctr}) {end_operand}))"),
        );
        out.push_in(2, set_var);
        out.push_in(2, format!("(block $c{n}"));
        out.append(body_b, 3);
        out.push_in(2, ")");
        out.push_in(
            2,
            format!("(local.set ${ctr} (i32.add (local.get ${ctr}) (i32.const {step_v})))"),
        );
        out.push_in(2, format!("(br $l{n})"));
        out.push_in(1, ")");
        out.push(")");
        Ok(())
    }

    /// Compile one `def` into its own WASM function. Python scoping rule:
    /// names assigned anywhere in the body (plus parameters) are locals;
    /// everything else resolves to module globals.
    fn gen_def(&mut self, name: &str, params: &[String], body: &[Stmt]) -> Result<Func> {
        let mut cx = FuncCx::default();
        for p in params {
            cx.vars.insert(p.clone(), Ty::Value);
        }
        let mut assigned = std::collections::HashSet::new();
        collect_assigned(body, &mut assigned);
        let mut local_names: Vec<&String> = assigned.iter().collect();
        local_names.sort(); // deterministic output
        for a in local_names {
            if !cx.vars.contains_key(a) {
                cx.ensure_local(a);
            }
        }

        let mut b = Body::new();
        self.stmts(&mut cx, body, &mut b)?;
        // Falling off the end returns None, like Python.
        b.push("(global.get $NONE)");

        let param_decls: String = params
            .iter()
            .map(|p| format!(" (param ${p} (ref null eq))"))
            .collect();
        Ok(Func {
            signature: format!("(func $f_{name}{param_decls} (result (ref null eq))"),
            locals: cx
                .locals
                .iter()
                .map(|(n, ty)| format!("(local ${n} {ty})"))
                .collect(),
            body: b,
        })
    }

    /// `for var in <sequence>:` — snapshot the iterable, index through it
    /// with $py_index. Length is re-read each pass (Python's list iterator
    /// does the same, so appending inside the loop extends it).
    fn gen_foreach(
        &mut self,
        cx: &mut FuncCx,
        var: &str,
        iterable: &Expr,
        body: &[Stmt],
        out: &mut Body,
    ) -> Result<()> {
        self.type_of(cx, iterable)?;
        let it_wat = self.value_expr(cx, iterable)?;
        let n = cx.fresh();
        let it = cx.scratch_local(VAL);
        let idx = format!(".f{n}");
        cx.locals.push((idx.clone(), "i32".to_string()));
        let set_var = if cx.is_top {
            self.ensure_global(var);
            format!("(global.set $g_{var} (call $py_index (local.get ${it}) (local.get ${idx})))")
        } else {
            format!("(local.set ${var} (call $py_index (local.get ${it}) (local.get ${idx})))")
        };

        cx.loops.push((format!("$b{n}"), format!("$c{n}")));
        let mut body_b = Body::new();
        let r = self.stmts(cx, body, &mut body_b);
        cx.loops.pop();
        r?;

        out.push(format!("(local.set ${it} {it_wat})"));
        out.push(format!("(local.set ${idx} (i32.const 0))"));
        out.push(format!("(block $b{n}"));
        out.push_in(1, format!("(loop $l{n}"));
        out.push_in(
            2,
            format!("(br_if $b{n} (i32.ge_s (local.get ${idx}) (call $py_len (local.get ${it}))))"),
        );
        out.push_in(2, set_var);
        out.push_in(2, format!("(block $c{n}"));
        out.append(body_b, 3);
        out.push_in(2, ")");
        out.push_in(
            2,
            format!("(local.set ${idx} (i32.add (local.get ${idx}) (i32.const 1)))"),
        );
        out.push_in(2, format!("(br $l{n})"));
        out.push_in(1, ")");
        out.push(")");
        Ok(())
    }

    /// Generate WAT producing the boxed `(ref null eq)` value of `e`.
    fn value_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        // Fold integer constants — this is also where literals are
        // range-checked instead of silently wrapping.
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v32) => {
                    if (I31_MIN..=I31_MAX).contains(&v) {
                        Ok(format!("(ref.i31 (i32.const {v32}))"))
                    } else {
                        Ok(format!("(struct.new $INT (i32.const {v32}))"))
                    }
                }
                Err(_) => Err(CompileError::at(
                    e.line,
                    format!(
                        "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                    ),
                )),
            };
        }
        // Float constants (and negated ones) fold to a $FLOAT literal.
        if let Some(f) = const_float(e) {
            return Ok(format!("(struct.new $FLOAT (f64.const {f}))"));
        }
        match &e.kind {
            // All numeric literals (and negated ones) were folded above.
            ExprKind::Int(_) => unreachable!("integer literals are folded above"),
            ExprKind::Float(_) => unreachable!("float literals are folded above"),
            ExprKind::Bool(true) => Ok("(global.get $TRUE)".into()),
            ExprKind::Bool(false) => Ok("(global.get $FALSE)".into()),
            ExprKind::NoneLit => Ok("(global.get $NONE)".into()),
            ExprKind::Name(n) => {
                if cx.vars.contains_key(n) {
                    Ok(format!("(local.get ${n})"))
                } else if self.is_global(n) {
                    Ok(format!("(global.get $g_{n})"))
                } else if self.funcs.contains_key(n) {
                    Err(CompileError::at(
                        e.line,
                        format!("'{n}' is a function — call it with {n}(...)"),
                    ))
                } else {
                    Err(CompileError::at(e.line, format!("unknown name '{n}'")))
                }
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.value_expr(cx, inner)?;
                Ok(format!("(call $py_neg {v})"))
            }
            ExprKind::Unary(UnOp::Not, inner) => {
                let c = self.cond_i32(cx, inner)?;
                Ok(format!("(call $bool (i32.eqz {c}))"))
            }
            ExprKind::Bin(BinOp::And, a, b) => {
                // Python value semantics with short-circuit: `a and b` is `a`
                // if a is falsy, else `b` (b unevaluated when a is falsy).
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let t = cx.scratch_local(VAL);
                Ok(format!(
                    "(if (result (ref null eq)) (call $truthy (local.tee ${t} {lhs})) (then {rhs}) (else (local.get ${t})))"
                ))
            }
            ExprKind::Bin(BinOp::Or, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let t = cx.scratch_local(VAL);
                Ok(format!(
                    "(if (result (ref null eq)) (call $truthy (local.tee ${t} {lhs})) (then (local.get ${t})) (else {rhs}))"
                ))
            }
            ExprKind::Bin(BinOp::Add, a, b) => {
                // `+` is concatenation when both sides are strings — runtime
                // dispatch via $py_add.
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_add {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Eq, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $bool (call $py_eq {lhs} {rhs}))"))
            }
            ExprKind::Bin(BinOp::Ne, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $bool (i32.eqz (call $py_eq {lhs} {rhs})))"))
            }
            ExprKind::Bin(BinOp::Div, a, b) => {
                // Python `/` is true division: always a float.
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_div {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Sub, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_sub {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Mul, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_mul {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::FloorDiv, a, b) => {
                self.uses_floordiv = true;
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_floordiv {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Mod, a, b) => {
                self.uses_floormod = true;
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_mod {lhs} {rhs})"))
            }
            ExprKind::Bin(op, a, b) => {
                // Comparisons run as f64 (exact for every i32).
                let lhs = self.f64_expr(cx, a)?;
                let rhs = self.f64_expr(cx, b)?;
                let cmp = |instr: &str| format!("(call $bool ({instr} {lhs} {rhs}))");
                Ok(match op {
                    BinOp::Lt => cmp("f64.lt"),
                    BinOp::Le => cmp("f64.le"),
                    BinOp::Gt => cmp("f64.gt"),
                    BinOp::Ge => cmp("f64.ge"),
                    _ => unreachable!("handled above"),
                })
            }
            ExprKind::Str(s) => {
                let bytes: Vec<String> = s.bytes().map(|b| format!("(i32.const {b})")).collect();
                Ok(format!(
                    "(array.new_fixed $STR {} {})",
                    bytes.len(),
                    bytes.join(" ")
                ))
            }
            ExprKind::List(elems) => {
                let mut items = String::new();
                for el in elems {
                    items.push(' ');
                    items.push_str(&self.value_expr(cx, el)?);
                }
                let n = elems.len();
                Ok(format!(
                    "(struct.new $LIST (i32.const {n}) (array.new_fixed $ITEMS {n}{items}))"
                ))
            }
            ExprKind::Dict(entries) => {
                // Known divergence: duplicate literal keys keep the FIRST
                // entry here (Python keeps the last) — rare enough to defer.
                let mut keys = String::new();
                let mut vals = String::new();
                for (k, v) in entries {
                    keys.push(' ');
                    keys.push_str(&self.value_expr(cx, k)?);
                    vals.push(' ');
                    vals.push_str(&self.value_expr(cx, v)?);
                }
                let n = entries.len();
                Ok(format!(
                    "(struct.new $DICT (i32.const {n}) (array.new_fixed $ITEMS {n}{keys}) (array.new_fixed $ITEMS {n}{vals}))"
                ))
            }
            ExprKind::Index(obj, key) => {
                let o = self.value_expr(cx, obj)?;
                let k = self.value_expr(cx, key)?;
                Ok(format!("(call $py_subscript {o} {k})"))
            }
            ExprKind::MethodCall(recv, method, args) => {
                if method == "append" && args.len() == 1 {
                    let r = self.value_expr(cx, recv)?;
                    let v = self.value_expr(cx, &args[0])?;
                    Ok(format!("(call $list_append {r} {v})"))
                } else {
                    Err(CompileError::at(
                        e.line,
                        format!(
                            ".{method}({}) isn't supported yet — lists support .append(value)",
                            if args.is_empty() { "" } else { "..." }
                        ),
                    ))
                }
            }
            ExprKind::Call(n, args) => {
                if n == "print" {
                    return Err(CompileError::at(
                        e.line,
                        "print(...) can't be used inside an expression",
                    ));
                }
                // User functions first (a `def len` shadows the builtin,
                // like Python); then the len() builtin.
                if !self.funcs.contains_key(n.as_str()) && n == "len" {
                    if args.len() != 1 {
                        return Err(CompileError::at(e.line, "len() takes exactly one argument"));
                    }
                    let v = self.value_expr(cx, &args[0])?;
                    return Ok(format!("(call $box (call $py_len {v}))"));
                }
                let Some(&arity) = self.funcs.get(n) else {
                    return Err(CompileError::at(e.line, format!("unknown function '{n}'")));
                };
                if args.len() != arity {
                    return Err(CompileError::at(
                        e.line,
                        format!(
                            "{n}() takes {arity} argument{} but {} {} given",
                            if arity == 1 { "" } else { "s" },
                            args.len(),
                            if args.len() == 1 { "was" } else { "were" }
                        ),
                    ));
                }
                let mut wat = format!("(call $f_{n}");
                for a in args {
                    wat.push(' ');
                    wat.push_str(&self.value_expr(cx, a)?);
                }
                wat.push(')');
                Ok(wat)
            }
        }
    }

    /// Generate WAT producing the raw i32 of `e` — a constant directly,
    /// anything else via `$unbox`.
    fn i32_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        if let Some(v) = const_int(e) {
            return match i32::try_from(v) {
                Ok(v32) => Ok(format!("(i32.const {v32})")),
                Err(_) => Err(CompileError::at(
                    e.line,
                    format!(
                        "the number {v} is too big — whole numbers from -2147483648 to 2147483647 are supported for now"
                    ),
                )),
            };
        }
        Ok(format!("(call $unbox {})", self.value_expr(cx, e)?))
    }

    /// Generate WAT producing the f64 of `e` — numeric constants directly,
    /// anything else via `$unbox_f64`.
    fn f64_expr(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        if let Some(v) = const_int(e) {
            return Ok(format!("(f64.const {v})"));
        }
        if let Some(f) = const_float(e) {
            return Ok(format!("(f64.const {f})"));
        }
        Ok(format!("(call $unbox_f64 {})", self.value_expr(cx, e)?))
    }

    /// Generate WAT producing an i32 condition (0 = false). Comparisons and
    /// `not` skip the boxed-bool round-trip.
    fn cond_i32(&mut self, cx: &mut FuncCx, e: &Expr) -> Result<String> {
        match &e.kind {
            ExprKind::Bool(v) => Ok(format!("(i32.const {})", *v as i32)),
            ExprKind::Unary(UnOp::Not, inner) => {
                let c = self.cond_i32(cx, inner)?;
                Ok(format!("(i32.eqz {c})"))
            }
            ExprKind::Bin(BinOp::Eq, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_eq {lhs} {rhs})"))
            }
            ExprKind::Bin(BinOp::Ne, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(i32.eqz (call $py_eq {lhs} {rhs}))"))
            }
            ExprKind::Bin(op, a, b) if cmp_instr(*op).is_some() => {
                let lhs = self.f64_expr(cx, a)?;
                let rhs = self.f64_expr(cx, b)?;
                Ok(format!("({} {lhs} {rhs})", cmp_instr(*op).unwrap()))
            }
            _ => Ok(format!("(call $truthy {})", self.value_expr(cx, e)?)),
        }
    }
}

fn cmp_instr(op: BinOp) -> Option<&'static str> {
    Some(match op {
        BinOp::Lt => "f64.lt",
        BinOp::Le => "f64.le",
        BinOp::Gt => "f64.gt",
        BinOp::Ge => "f64.ge",
        _ => return None,
    })
}

fn emit_char(out: &mut Body, byte: u8) {
    out.push(format!("(call $write_char (i32.const {byte}))"));
}

/// Constant value of an integer literal (handling unary minus), if it is one.
fn const_int(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Unary(UnOp::Neg, inner) => const_int(inner).map(|v| -v),
        _ => None,
    }
}

/// Constant value of a float literal (handling unary minus), if it is one.
fn const_float(e: &Expr) -> Option<f64> {
    match &e.kind {
        ExprKind::Float(f) => Some(*f),
        ExprKind::Unary(UnOp::Neg, inner) => const_float(inner).map(|v| -v),
        _ => None,
    }
}

/// Names assigned anywhere in a statement list (assignment targets and
/// for-loop variables) — Python's "assigned anywhere in the body = local".
fn collect_assigned(stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign(name, _) => {
                out.insert(name.clone());
            }
            StmtKind::For { var, body, .. } | StmtKind::ForEach { var, body, .. } => {
                out.insert(var.clone());
                collect_assigned(body, out);
            }
            StmtKind::SetIndex { .. } => {}
            StmtKind::While { body, .. } => collect_assigned(body, out),
            StmtKind::If {
                body,
                elifs,
                else_body,
                ..
            } => {
                collect_assigned(body, out);
                for (_, b) in elifs {
                    collect_assigned(b, out);
                }
                if let Some(b) = else_body {
                    collect_assigned(b, out);
                }
            }
            _ => {}
        }
    }
}

impl Gen {
    fn require_value(&self, cx: &FuncCx, e: &Expr, what: &str) -> Result<()> {
        match self.type_of(cx, e)? {
            Ty::Num | Ty::Value => Ok(()),
            Ty::Str => Err(CompileError::at(
                e.line,
                format!("{what} needs to be a number, not text"),
            )),
        }
    }

    /// Static type of an expression. This is a friendliness pass — it catches
    /// *definite* misuse (`5 - "a"`) at compile time; expressions involving
    /// variables or calls are `Value` (unknown) and dynamic misuse traps at
    /// run time until real runtime type errors land.
    fn type_of(&self, cx: &FuncCx, e: &Expr) -> Result<Ty> {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) => Ok(Ty::Num),
            ExprKind::NoneLit => Ok(Ty::Value),
            ExprKind::Str(_) => Ok(Ty::Str),
            ExprKind::Unary(op, inner) => {
                let t = self.type_of(cx, inner)?;
                match op {
                    UnOp::Not => Ok(Ty::Num), // `not "x"` is a bool
                    UnOp::Neg => match t {
                        Ty::Str => Err(CompileError::at(
                            e.line,
                            "operator needs a number, not text",
                        )),
                        _ => Ok(Ty::Num),
                    },
                }
            }
            ExprKind::Bin(op, a, b) => {
                let (ta, tb) = (self.type_of(cx, a)?, self.type_of(cx, b)?);
                match op {
                    // Equality and and/or accept any mix of types.
                    BinOp::Eq | BinOp::Ne | BinOp::And | BinOp::Or => Ok(Ty::Value),
                    // `+` concatenates strings or adds numbers — never across
                    // (only flagged when both sides are statically known).
                    BinOp::Add => match (ta, tb) {
                        (Ty::Str, Ty::Str) => Ok(Ty::Str),
                        (Ty::Str, Ty::Num) | (Ty::Num, Ty::Str) => Err(CompileError::at(
                            e.line,
                            "can't add text and a number together",
                        )),
                        (Ty::Num, Ty::Num) => Ok(Ty::Num),
                        _ => Ok(Ty::Value),
                    },
                    _ => {
                        if ta == Ty::Str || tb == Ty::Str {
                            Err(CompileError::at(
                                e.line,
                                "this operator needs numbers on both sides",
                            ))
                        } else {
                            Ok(Ty::Num)
                        }
                    }
                }
            }
            ExprKind::Name(n) => {
                if cx.vars.contains_key(n) || self.is_global(n) {
                    Ok(Ty::Value)
                } else if self.funcs.contains_key(n) {
                    Err(CompileError::at(
                        e.line,
                        format!("'{n}' is a function — call it with {n}(...)"),
                    ))
                } else {
                    Err(CompileError::at(
                        e.line,
                        format!("unknown name '{n}' (define it with `{n} = ...` first)"),
                    ))
                }
            }
            ExprKind::Call(_, args) => {
                for a in args {
                    self.type_of(cx, a)?;
                }
                Ok(Ty::Value)
            }
            ExprKind::List(elems) => {
                for el in elems {
                    self.type_of(cx, el)?;
                }
                Ok(Ty::Value)
            }
            ExprKind::Dict(entries) => {
                for (k, v) in entries {
                    self.type_of(cx, k)?;
                    self.type_of(cx, v)?;
                }
                Ok(Ty::Value)
            }
            ExprKind::Index(obj, idx) => {
                self.type_of(cx, obj)?;
                self.type_of(cx, idx)?;
                Ok(Ty::Value)
            }
            ExprKind::MethodCall(recv, _, args) => {
                self.type_of(cx, recv)?;
                for a in args {
                    self.type_of(cx, a)?;
                }
                Ok(Ty::Value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::lex, parser::parse};

    fn compile(src: &str) -> Result<String> {
        generate(&parse(&lex(src).unwrap()).unwrap())
    }

    #[test]
    fn print_int_arithmetic() {
        let wat = compile("print(2 + 3 * 4)").unwrap();
        assert!(wat.contains("(export \"_start\")"));
        // All arithmetic dispatches at runtime: `+` might concatenate,
        // `*` might promote to float.
        assert!(wat.contains("(call $py_mul (ref.i31 (i32.const 3)) (ref.i31 (i32.const 4)))"));
        assert!(wat.contains("(call $py_add (ref.i31 (i32.const 2))"));
        assert!(wat.contains("(call $print_value"));
    }

    #[test]
    fn strings_are_gc_arrays() {
        let wat = compile("x = \"hi\"\nprint(x)").unwrap();
        assert!(wat.contains("(type $STR (array (mut i8)))"));
        assert!(wat.contains(
            "(global.set $g_x (array.new_fixed $STR 2 (i32.const 104) (i32.const 105)))"
        ));
        assert!(wat.contains("(call $print_value (global.get $g_x))"));
    }

    #[test]
    fn string_equality_uses_py_eq() {
        let wat = compile("print(\"a\" == \"b\")").unwrap();
        assert!(wat.contains("(call $py_eq"));
        // In a condition, the boxed-bool round-trip is skipped.
        let wat = compile("if \"a\" == \"b\":\n    print(1)\n").unwrap();
        assert!(wat.contains("(if (call $py_eq"));
    }

    #[test]
    fn literal_type_misuse_is_a_compile_error() {
        assert!(compile("print(1 + \"a\")")
            .unwrap_err()
            .message
            .contains("can't add text and a number"));
        assert!(compile("print(\"a\" - 1)")
            .unwrap_err()
            .message
            .contains("numbers on both sides"));
        assert!(compile("print(\"a\" < \"b\")").is_err()); // lexicographic: later
        assert!(compile("for i in range(\"x\"):\n    print(i)\n").is_err());
    }

    #[test]
    fn variable_then_print() {
        let wat = compile("x = 5\nprint(x)").unwrap();
        assert!(wat.contains("(global $g_x (mut (ref null eq)) (ref.null eq))"));
        assert!(wat.contains("(global.set $g_x (ref.i31 (i32.const 5)))"));
        assert!(wat.contains("(call $print_value (global.get $g_x))"));
    }

    #[test]
    fn booleans_are_singletons_not_ints() {
        let wat = compile("x = True\nprint(x, False)").unwrap();
        assert!(wat.contains("(global.set $g_x (global.get $TRUE))"));
        assert!(wat.contains("(call $print_value (global.get $FALSE))"));
        // The runtime knows how to spell them.
        assert!(wat.contains("(type $BOOL (struct (field i8)))"));
    }

    #[test]
    fn big_literals_spill_to_int_struct() {
        let wat = compile("print(2147483647)").unwrap();
        assert!(wat.contains("(struct.new $INT (i32.const 2147483647))"));
        let wat = compile("print(5)").unwrap();
        assert!(wat.contains("(ref.i31 (i32.const 5))"));
    }

    #[test]
    fn start_returns_exit_code() {
        let wat = compile("print(1)").unwrap();
        assert!(wat.contains("(func $_start (export \"_start\") (result i32)"));
        assert!(wat.contains("(i32.const 0)\n  )"));
    }

    #[test]
    fn if_else_emits_branches() {
        let wat = compile("x = 3\nif x < 5:\n    print(1)\nelse:\n    print(2)\n").unwrap();
        // Comparison conditions skip the boxed-bool round-trip.
        assert!(wat.contains("(if (f64.lt (call $unbox_f64 (global.get $g_x)) (f64.const 5))"));
        assert!(wat.contains("(then"));
        assert!(wat.contains("(else"));
    }

    #[test]
    fn elif_chain_nests() {
        let src =
            "x = 2\nif x < 1:\n    print(1)\nelif x < 3:\n    print(2)\nelse:\n    print(3)\n";
        let wat = compile(src).unwrap();
        // Two conditions compile to two direct comparisons in _start.
        assert_eq!(wat.matches("(if (f64.lt").count(), 2);
    }

    #[test]
    fn for_loop_uses_raw_i32_counter() {
        let wat = compile("for i in range(3):\n    print(i)\n").unwrap();
        assert!(wat.contains("(global $g_i (mut (ref null eq)) (ref.null eq))"));
        assert!(wat.contains("(local $.f0 i32)"));
        assert!(wat.contains("(local.set $.f0 (i32.const 0))"));
        assert!(wat.contains("(br_if $b0 (i32.ge_s (local.get $.f0) (i32.const 3)))"));
        // The Python-visible loop variable gets the boxed counter.
        assert!(wat.contains("(global.set $g_i (call $box (local.get $.f0)))"));
        assert!(wat.contains("(local.set $.f0 (i32.add (local.get $.f0) (i32.const 1)))"));
    }

    #[test]
    fn for_loop_snapshots_nonconstant_end() {
        let wat = compile("n = 3\nfor i in range(0, n):\n    n = n + 1\n").unwrap();
        // The end bound is unboxed once into an i32 scratch local.
        assert!(wat.contains("(local $.t0 i32)"));
        assert!(wat.contains("(local.set $.t0 (call $unbox (global.get $g_n)))"));
        assert!(wat.contains("(br_if $b0 (i32.ge_s (local.get $.f0) (local.get $.t0)))"));
    }

    #[test]
    fn nested_loops_get_unique_labels() {
        let src = "for i in range(2):\n    for j in range(2):\n        print(j)\n";
        let wat = compile(src).unwrap();
        assert!(wat.contains("$l0"));
        assert!(wat.contains("$l1"));
    }

    #[test]
    fn use_before_assignment_errors() {
        assert!(compile("print(x)").is_err());
    }

    #[test]
    fn codegen_errors_carry_lines() {
        let err = compile("x = 1\nprint(y)\n").unwrap_err();
        assert_eq!(err.line, Some(2));
        let err = compile("x = 1\n\nbreak\n").unwrap_err();
        assert_eq!(err.line, Some(3));
    }

    #[test]
    fn negative_step_counts_down() {
        let wat = compile("for i in range(5, 0, -1):\n    print(i)\n").unwrap();
        assert!(wat.contains("(i32.le_s (local.get $.f0) (i32.const 0))"));
        assert!(wat.contains("(i32.const -1)"));
    }

    #[test]
    fn zero_step_is_rejected() {
        assert!(compile("for i in range(0, 5, 0):\n    print(i)\n").is_err());
    }

    #[test]
    fn non_constant_step_is_rejected() {
        let err = compile("s = 1\nfor i in range(0, 5, s):\n    print(i)\n").unwrap_err();
        assert!(err.message.contains("step"));
    }

    #[test]
    fn while_emits_loop_with_negated_test() {
        let wat = compile("i = 3\nwhile i > 0:\n    i = i - 1\n").unwrap();
        assert!(wat.contains(
            "(br_if $b0 (i32.eqz (f64.gt (call $unbox_f64 (global.get $g_i)) (f64.const 0))))"
        ));
        assert!(wat.contains("(br $l0)"));
    }

    #[test]
    fn break_and_continue_target_the_right_labels() {
        // In a for-loop, continue must reach the increment (the $c block),
        // and break must exit the whole loop (the $b block).
        let wat =
            compile("for i in range(3):\n    if i == 1:\n        continue\n    break\n").unwrap();
        assert!(wat.contains("(br $c0)"));
        assert!(wat.contains("(br $b0)"));
        // In a while, continue re-tests the condition (the loop head).
        let wat = compile("i = 0\nwhile i < 3:\n    i = i + 1\n    continue\n").unwrap();
        assert!(wat.contains("(br $l0)"));
    }

    #[test]
    fn break_continue_outside_loop_are_rejected() {
        assert!(compile("break\n")
            .unwrap_err()
            .message
            .contains("inside a loop"));
        assert!(compile("continue\n")
            .unwrap_err()
            .message
            .contains("inside a loop"));
        // ...including in an if that isn't inside a loop.
        assert!(compile("if 1:\n    break\n").is_err());
    }

    #[test]
    fn and_or_short_circuit_shape() {
        let wat = compile("print(2 and 1)").unwrap();
        assert!(wat.contains(
            "(if (result (ref null eq)) (call $truthy (local.tee $.t0 (ref.i31 (i32.const 2)))) (then (ref.i31 (i32.const 1))) (else (local.get $.t0)))"
        ));
        let wat = compile("print(4 or 2)").unwrap();
        assert!(wat.contains("(then (local.get $.t0)) (else (ref.i31 (i32.const 2)))"));
    }

    #[test]
    fn floordiv_and_mod_call_helpers() {
        let wat = compile("print(-7 // 2)\nprint(-7 % 2)").unwrap();
        assert!(
            wat.contains("(call $py_floordiv (ref.i31 (i32.const -7)) (ref.i31 (i32.const 2)))")
        );
        assert!(wat.contains("(call $py_mod (ref.i31 (i32.const -7)) (ref.i31 (i32.const 2)))"));
        // The dispatchers and their int helpers are both emitted.
        assert!(wat.contains("(func $py_floordiv"));
        assert!(wat.contains("(func $i32_floordiv"));
        assert!(wat.contains("(func $i32_floormod"));
    }

    #[test]
    fn helpers_omitted_when_unused() {
        let wat = compile("print(1 + 2)").unwrap();
        assert!(!wat.contains("$i32_floordiv"));
        assert!(!wat.contains("$i32_floormod"));
    }

    #[test]
    fn true_division_is_float() {
        let wat = compile("print(7 / 2)").unwrap();
        assert!(wat.contains("(call $py_div (ref.i31 (i32.const 7)) (ref.i31 (i32.const 2)))"));
        assert!(wat.contains("(type $FLOAT (struct (field f64)))"));
    }

    #[test]
    fn float_literals_fold_to_float_structs() {
        let wat = compile("x = 3.5\nprint(-2.5)").unwrap();
        assert!(wat.contains("(global.set $g_x (struct.new $FLOAT (f64.const 3.5)))"));
        assert!(wat.contains("(struct.new $FLOAT (f64.const -2.5))"));
    }

    #[test]
    fn range_rejects_decimals() {
        let err = compile("for i in range(2.5):\n    print(i)\n").unwrap_err();
        assert!(err.message.contains("whole numbers"));
    }

    #[test]
    fn out_of_range_literal_is_rejected() {
        assert!(compile("print(3000000000)").is_err());
        assert!(compile("print(-2147483649)").is_err());
        // The i32 boundary values themselves are fine.
        assert!(compile("print(2147483647)").is_ok());
        assert!(compile("print(-2147483648)").is_ok());
    }

    #[test]
    fn def_compiles_to_a_function() {
        let wat = compile("def add(a, b):\n    return a + b\nprint(add(2, 3))\n").unwrap();
        assert!(wat.contains(
            "(func $f_add (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
        ));
        assert!(wat.contains("(call $f_add (ref.i31 (i32.const 2)) (ref.i31 (i32.const 3)))"));
        // Falling off the end returns None.
        assert!(wat.contains("(global.get $NONE)"));
    }

    #[test]
    fn function_locals_shadow_globals() {
        let wat = compile("x = 1\ndef f():\n    x = 2\n    return x\nprint(f(), x)\n").unwrap();
        // Global x exists; inside f, x is a local.
        assert!(wat.contains("(global $g_x (mut (ref null eq)) (ref.null eq))"));
        assert!(wat.contains("(local $x (ref null eq))"));
        assert!(wat.contains("(local.set $x (ref.i31 (i32.const 2)))"));
    }

    #[test]
    fn def_error_cases() {
        // Arity mismatch is a compile error.
        let err = compile("def f(a):\n    return a\nprint(f(1, 2))\n").unwrap_err();
        assert!(err.message.contains("takes 1 argument"));
        // return at top level.
        assert!(compile("return 1\n")
            .unwrap_err()
            .message
            .contains("inside a function"));
        // Nested def.
        let err = compile("def f():\n    def g():\n        return 1\n    return 2\n").unwrap_err();
        assert!(err.message.contains("top level"));
        // Unknown function.
        assert!(compile("print(nope(1))\n")
            .unwrap_err()
            .message
            .contains("unknown function"));
        // Duplicate definition.
        assert!(compile("def f():\n    return 1\ndef f():\n    return 2\n")
            .unwrap_err()
            .message
            .contains("defined twice"));
        // Function used without calling.
        let err = compile("def f():\n    return 1\nprint(f + 1)\n").unwrap_err();
        assert!(err.message.contains("call it"));
    }

    #[test]
    fn chained_comparison_around_call_is_rejected() {
        // The guard fires in the parser (the middle operand would be cloned).
        let err = parse(&lex("def f():\n    return 2\nif 1 < f() < 3:\n    print(1)\n").unwrap())
            .unwrap_err();
        assert!(err
            .message
            .contains("chained comparisons around a function call"));
    }

    #[test]
    fn none_is_a_value() {
        let wat = compile("x = None\nprint(x == None)\n").unwrap();
        assert!(wat.contains("(global.set $g_x (global.get $NONE))"));
        assert!(wat.contains("(type $NONE_T (struct))"));
    }
}
