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

use crate::ast::{BinOp, CompClause, Expr, ExprKind, Stmt, StmtKind, UnOp};
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

    // Pass 1: collect function and class signatures so calls/construction
    // (including mutual recursion) resolve regardless of definition order.
    for s in stmts {
        if let StmtKind::Def {
            name,
            params,
            defaults,
            ..
        } = &s.kind
        {
            if name == "print" {
                return Err(CompileError::at(s.line, "can't redefine print"));
            }
            if g.funcs.insert(name.clone(), params.len()).is_some() {
                return Err(CompileError::at(
                    s.line,
                    format!("function '{name}' is defined twice"),
                ));
            }
            g.func_defaults.insert(name.clone(), defaults.clone());
            g.func_params.insert(name.clone(), params.clone());
        }
    }
    for s in stmts {
        if let StmtKind::ClassDef { name, base, .. } = &s.kind {
            if g.funcs.contains_key(name) || g.classes.contains_key(name) {
                return Err(CompileError::at(
                    s.line,
                    format!("'{name}' is defined twice"),
                ));
            }
            if let Some(b) = base {
                if !g.classes.contains_key(b) {
                    return Err(CompileError::at(
                        s.line,
                        format!("unknown base class '{b}' (define it before '{name}')"),
                    ));
                }
            }
            g.classes.insert(name.clone(), base.clone());
        }
    }
    for s in stmts {
        if let StmtKind::Import(names) = &s.kind {
            for m in names {
                if m != "math" {
                    return Err(CompileError::at(
                        s.line,
                        format!("module '{m}' isn't available (only 'math' for now)"),
                    ));
                }
                g.imported.insert(m.clone());
            }
        }
    }

    // Pass 2a: top-level statements become _start. Classes are built first
    // (their method tables must exist before any user code constructs an
    // instance), then top-level variables (which become module globals).
    let mut cx = FuncCx {
        is_top: true,
        ..Default::default()
    };
    let mut body = Body::new();
    if !g.classes.is_empty() {
        cx.locals
            .push((".cd".to_string(), "(ref null $DICT)".to_string()));
        for s in stmts {
            if let StmtKind::ClassDef {
                name,
                base,
                methods,
                class_vars,
            } = &s.kind
            {
                g.gen_class_init(&mut cx, name, base, methods, class_vars, &mut body)?;
            }
        }
    }
    for s in stmts {
        if !matches!(s.kind, StmtKind::Def { .. } | StmtKind::ClassDef { .. }) {
            g.stmt(&mut cx, s, &mut body)?;
        }
    }
    body.push("(i32.const 0)");

    // Pass 2b: each def and each method becomes its own function (after 2a,
    // so every module global is known).
    let mut user_funcs = Vec::new();
    let mut method_names = Vec::new();
    for s in stmts {
        match &s.kind {
            StmtKind::Def {
                name, params, body, ..
            } => {
                user_funcs.push(g.gen_def(name, params, body)?);
            }
            StmtKind::ClassDef { name, methods, .. } => {
                for m in methods {
                    user_funcs.push(g.gen_method(name, m, s.line)?);
                    method_names.push(format!("$m_{name}_{}", m.name));
                }
            }
            _ => {}
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
    // A tuple is immutable: non-mut fields make it a structurally distinct type
    // from $LIST, so ref.test never confuses the two.
    module
        .types
        .push("(type $TUPLE (struct (field i32) (field (ref null $ITEMS))))".into());
    // A set: count + element array. The trailing i8 marker makes it
    // structurally distinct from $LIST (count + items). Insertion-ordered (a
    // documented divergence from CPython's hash order).
    module.types.push(
        "(type $SET (struct (field (mut i32)) (field (mut (ref null $ITEMS))) (field (mut i8))))"
            .into(),
    );
    // A dict is an insertion-ordered association: parallel key/value arrays
    // with linear-scan lookup (classroom-sized; order matches Python).
    module.types.push(
        "(type $DICT (struct (field (mut i32)) (field (mut (ref null $ITEMS))) (field (mut (ref null $ITEMS)))))".into(),
    );
    // Classes (boxed object model, ported from reference p2w — see
    // CLASSES_DESIGN.md). Methods dispatch via $MFUNC function references;
    // instance attrs and class method tables reuse $DICT.
    module.types.push(
        "(type $MFUNC (func (param (ref null eq)) (param (ref null eq)) (result (ref null eq))))"
            .into(),
    );
    module
        .types
        .push("(type $METHOD (struct (field (ref $MFUNC))))".into());
    // $CLASS self-references via $base, so it lives in a singleton rec group.
    module.types.push(
        "(rec (type $CLASS (struct (field (ref $STR)) (field (ref null $DICT)) (field (ref null $CLASS)))))".into(),
    );
    module
        .types
        .push("(type $OBJECT (struct (field (ref $CLASS)) (field (ref null $DICT))))".into());
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
    for name in g.classes.keys() {
        module.globals.push(format!(
            "(global $g_class_{name} (mut (ref null $CLASS)) (ref.null $CLASS))"
        ));
    }
    module.elem_declares = method_names;

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
    for f in class_helpers() {
        module.funcs.push(f);
    }
    for f in raise_helpers() {
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
    if g.uses_input {
        module
            .imports
            .push(r#"(import "env" "read_char" (func $read_char (result i32)))"#.into());
        module.funcs.push(read_line_helper());
    }
    Ok(module.render())
}

/// Emit `write_char` calls spelling out `text` (for runtime messages).
fn push_text(b: &mut Body, depth: usize, text: &str) {
    for c in text.bytes() {
        b.push_in(depth, format!("(call $write_char (i32.const {c}))"));
    }
}

/// The error-raising runtime: each $raise_* prints a Python-style message
/// (on its own line) through write_char, then traps. Bodies end in
/// `unreachable`, so call sites in value position add their own trailing
/// `unreachable` to satisfy the validator.
fn raise_helpers() -> Vec<Func> {
    let mut fs = Vec::new();

    // $type_name: print the Python type name of a value.
    let mut b = Body::new();
    b.push("(if (ref.is_null (local.get $r))");
    b.push_in(1, "(then");
    push_text(&mut b, 2, "unassigned");
    b.push_in(1, "(return))");
    b.push(")");
    for (test, name) in [
        ("(ref.test (ref i31) (local.get $r))", "int"),
        ("(ref.test (ref $INT) (local.get $r))", "int"),
        ("(ref.test (ref $BOOL) (local.get $r))", "bool"),
        ("(ref.test (ref $FLOAT) (local.get $r))", "float"),
        ("(ref.test (ref $STR) (local.get $r))", "str"),
        ("(ref.test (ref $LIST) (local.get $r))", "list"),
        ("(ref.test (ref $TUPLE) (local.get $r))", "tuple"),
        ("(ref.test (ref $SET) (local.get $r))", "set"),
        ("(ref.test (ref $DICT) (local.get $r))", "dict"),
        ("(ref.test (ref $NONE_T) (local.get $r))", "NoneType"),
    ] {
        b.push(format!("(if {test}"));
        b.push_in(1, "(then");
        push_text(&mut b, 2, name);
        b.push_in(1, "(return))");
        b.push(")");
    }
    // Instances report their class name (e.g. AttributeError messages).
    b.push("(if (ref.test (ref $OBJECT) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(call $print_str (struct.get $CLASS 0 (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $r)))))",
    );
    b.push_in(1, "(return))");
    b.push(")");
    push_text(&mut b, 0, "object");
    fs.push(Func {
        signature: "(func $type_name (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_type_num: numeric operation on a non-number. A null here means
    // a function local read before assignment — that's a NameError.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    b.push("(if (ref.is_null (local.get $r))");
    b.push_in(1, "(then");
    push_text(
        &mut b,
        2,
        "NameError: a variable was used before it was given a value",
    );
    b.push_in(2, "(call $write_char (i32.const 10))");
    b.push_in(2, "unreachable");
    b.push_in(1, ")");
    b.push(")");
    push_text(&mut b, 0, "TypeError: expected a number, got '");
    b.push("(call $type_name (local.get $r))");
    push_text(&mut b, 0, "'");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_type_num (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_index: subscript position out of range.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "IndexError: ");
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(1, "(then");
    push_text(&mut b, 2, "string");
    b.push_in(1, ")");
    b.push_in(1, "(else");
    push_text(&mut b, 2, "list");
    b.push_in(1, ")");
    b.push(")");
    push_text(&mut b, 0, " index out of range");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_index (param $r (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_key: dict lookup miss; the key prints in repr form.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "KeyError: ");
    b.push("(call $print_repr (local.get $k))");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_key (param $k (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_zero_div
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "ZeroDivisionError: division by zero");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_zero_div".into(),
        locals: vec![],
        body: b,
    });

    // Type-name-bearing raisers that differ only in their message shape.
    for (fname, before, after) in [
        (
            "$raise_no_len",
            "TypeError: object of type '",
            "' has no len()",
        ),
        (
            "$raise_not_sub",
            "TypeError: '",
            "' object is not subscriptable",
        ),
        (
            "$raise_no_item_assign",
            "TypeError: '",
            "' object does not support item assignment",
        ),
        (
            "$raise_no_append",
            "AttributeError: '",
            "' object has no attribute 'append'",
        ),
        (
            "$raise_not_iter",
            "TypeError: argument of type '",
            "' is not iterable",
        ),
        (
            "$raise_in_str",
            "TypeError: 'in <string>' requires string as left operand, not '",
            "'",
        ),
        (
            "$raise_no_str",
            "TypeError: str() of '",
            "' values isn't supported yet",
        ),
    ] {
        let mut b = Body::new();
        b.push("(call $write_char (i32.const 10))");
        push_text(&mut b, 0, before);
        b.push("(call $type_name (local.get $r))");
        push_text(&mut b, 0, after);
        b.push("(call $write_char (i32.const 10))");
        b.push("unreachable");
        fs.push(Func {
            signature: format!("(func {fname} (param $r (ref null eq))"),
            locals: vec![],
            body: b,
        });
    }

    // $raise_no_attr: attribute miss (read or method) — names the attribute.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "AttributeError: '");
    b.push("(call $type_name (local.get $obj))");
    push_text(&mut b, 0, "' object has no attribute '");
    b.push("(call $print_str (ref.cast (ref null $STR) (local.get $name)))");
    push_text(&mut b, 0, "'");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_no_attr (param $obj (ref null eq)) (param $name (ref null eq))"
            .into(),
        locals: vec![],
        body: b,
    });

    // $raise_method_value: a method was read as a value (e.g. `f = d.speak`).
    // Bound methods aren't first-class in this subset.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "TypeError: method '");
    b.push("(call $print_str (ref.cast (ref null $STR) (local.get $name)))");
    push_text(
        &mut b,
        0,
        "' can't be used as a value yet (call it with parentheses)",
    );
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature:
            "(func $raise_method_value (param $obj (ref null eq)) (param $name (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });

    // $raise_int_parse: int() of a string that isn't a valid integer.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(
        &mut b,
        0,
        "ValueError: invalid literal for int() with base 10: '",
    );
    b.push("(call $print_str (local.get $s))");
    push_text(&mut b, 0, "'");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_int_parse (param $s (ref null $STR))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_setop: a set operator (|, &, ^, set - set) on a non-set.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(
        &mut b,
        0,
        "TypeError: unsupported operand type for a set operation (both sides must be sets)",
    );
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_setop".into(),
        locals: vec![],
        body: b,
    });

    // $raise_empty: min()/max() of an empty sequence.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "ValueError: arg is an empty sequence");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_empty".into(),
        locals: vec![],
        body: b,
    });

    // $raise_float_parse: float() of a string that isn't a valid number.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(
        &mut b,
        0,
        "ValueError: could not convert string to float: '",
    );
    b.push("(call $print_str (local.get $s))");
    push_text(&mut b, 0, "'");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_float_parse (param $s (ref null $STR))".into(),
        locals: vec![],
        body: b,
    });

    // $raise_unpack: tuple-unpacking length mismatch.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "ValueError: wrong number of values to unpack");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_unpack".into(),
        locals: vec![],
        body: b,
    });

    // $raise_slice_step: slice step of zero.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(&mut b, 0, "ValueError: slice step cannot be zero");
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_slice_step".into(),
        locals: vec![],
        body: b,
    });

    // $raise_arity: a method got the wrong number of arguments.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 10))");
    push_text(
        &mut b,
        0,
        "TypeError: method called with the wrong number of arguments",
    );
    b.push("(call $write_char (i32.const 10))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $raise_arity".into(),
        locals: vec![],
        body: b,
    });

    fs
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

    // $unbox: value -> i32 (i31, $BOOL as 0/1, or $INT). Anything else
    // raises a friendly TypeError (or NameError for an unassigned local).
    let mut b = Body::new();
    b.push("(if (ref.test (ref i31) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i31.get_s (ref.cast (ref i31) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $INT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get $INT 0 (ref.cast (ref $INT) (local.get $r)))))",
    );
    b.push(")");
    b.push("(call $raise_type_num (local.get $r))");
    b.push("unreachable");
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
    b.push("(if (ref.test (ref $TUPLE) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i32.ne (struct.get $TUPLE 0 (ref.cast (ref $TUPLE) (local.get $r))) (i32.const 0))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $SET) (local.get $r))");
    b.push_in(
        1,
        "(then (return (i32.ne (struct.get $SET 0 (ref.cast (ref $SET) (local.get $r))) (i32.const 0))))",
    );
    b.push(")");
    b.push("(i32.ne (call $unbox (local.get $r)) (i32.const 0))");
    fs.push(Func {
        signature: "(func $truthy (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $py_len: a custom __len__ first, then sequence length (lists, dicts,
    // strings). __len__ returns a Python int, unboxed back to i32.
    let mut b = Body::new();
    b.push(format!(
        "(if (call $obj_has (local.get $r) {n}) (then (return (call $unbox (call $obj_call0 (local.get $r) {n})))))",
        n = str_lit("__len__")
    ));
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
    b.push("(if (ref.test (ref $TUPLE) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get $TUPLE 0 (ref.cast (ref $TUPLE) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $SET) (local.get $r))");
    b.push_in(
        1,
        "(then (return (struct.get $SET 0 (ref.cast (ref $SET) (local.get $r)))))",
    );
    b.push(")");
    b.push("(call $raise_no_len (local.get $r))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $py_len (param $r (ref null eq)) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $py_index: subscript read with Python negative-index normalization;
    // out of range raises IndexError. Strings yield a one-character string.
    let mut b = Body::new();
    b.push("(if (i32.eqz (i32.or (i32.or (i32.or (i32.or (ref.test (ref $LIST) (local.get $r)) (ref.test (ref $STR) (local.get $r))) (ref.test (ref $DICT) (local.get $r))) (ref.test (ref $TUPLE) (local.get $r))) (ref.test (ref $SET) (local.get $r))))");
    b.push_in(1, "(then (call $raise_not_sub (local.get $r)))");
    b.push(")");
    b.push("(local.set $n (call $py_len (local.get $r)))");
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(
        1,
        "(then (local.set $i (i32.add (local.get $i) (local.get $n))))",
    );
    b.push(")");
    b.push("(if (i32.or (i32.lt_s (local.get $i) (i32.const 0)) (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(1, "(then (call $raise_index (local.get $r)))");
    b.push(")");
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $LIST 1 (ref.cast (ref $LIST) (local.get $r))) (local.get $i))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $TUPLE) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $TUPLE 1 (ref.cast (ref $TUPLE) (local.get $r))) (local.get $i))))",
    );
    b.push(")");
    // Positional access on a set yields its i-th element (insertion order) —
    // used by `for x in s` iteration. Direct `s[i]` is blocked in py_subscript.
    b.push("(if (ref.test (ref $SET) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $SET 1 (ref.cast (ref $SET) (local.get $r))) (local.get $i))))",
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
    b.push("(if (i32.eqz (ref.test (ref $LIST) (local.get $r)))");
    b.push_in(1, "(then (call $raise_no_item_assign (local.get $r)))");
    b.push(")");
    b.push("(local.set $n (call $py_len (local.get $r)))");
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(
        1,
        "(then (local.set $i (i32.add (local.get $i) (local.get $n))))",
    );
    b.push(")");
    b.push("(if (i32.or (i32.lt_s (local.get $i) (i32.const 0)) (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(1, "(then (call $raise_index (local.get $r)))");
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
    b.push("(if (i32.eqz (ref.test (ref $LIST) (local.get $l)))");
    b.push_in(1, "(then (call $raise_no_append (local.get $l)))");
    b.push(")");
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

    // $set_find: index of an element (by py_eq), or -1.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $SET 0 (local.get $s)))");
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (call $py_eq (array.get $ITEMS (struct.get $SET 1 (local.get $s)) (local.get $i)) (local.get $v)) (then (return (local.get $i))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push("(i32.const -1)");
    fs.push(Func {
        signature:
            "(func $set_find (param $s (ref null $SET)) (param $v (ref null eq)) (result i32)"
                .into(),
        locals: vec!["(local $n i32)".into(), "(local $i i32)".into()],
        body: b,
    });

    // $set_insert: add an element if absent (amortized growth, like list).
    let mut b = Body::new();
    b.push("(if (i32.ge_s (call $set_find (local.get $s) (local.get $v)) (i32.const 0)) (then (return)))");
    b.push("(local.set $items (struct.get $SET 1 (local.get $s)))");
    b.push("(local.set $len (struct.get $SET 0 (local.get $s)))");
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
    b.push_in(2, "(struct.set $SET 1 (local.get $s) (local.get $new))");
    b.push_in(2, "(local.set $items (local.get $new))");
    b.push_in(1, "))");
    b.push("(array.set $ITEMS (local.get $items) (local.get $len) (local.get $v))");
    b.push("(struct.set $SET 0 (local.get $s) (i32.add (local.get $len) (i32.const 1)))");
    fs.push(Func {
        signature: "(func $set_insert (param $s (ref null $SET)) (param $v (ref null eq))".into(),
        locals: vec![
            "(local $items (ref null $ITEMS))".into(),
            "(local $new (ref null $ITEMS))".into(),
            "(local $len i32)".into(),
        ],
        body: b,
    });

    // $set_remove_at: drop element at idx (shift tail down, count--).
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $SET 0 (local.get $s)))");
    b.push(
        "(array.copy $ITEMS $ITEMS (struct.get $SET 1 (local.get $s)) (local.get $idx) (struct.get $SET 1 (local.get $s)) (i32.add (local.get $idx) (i32.const 1)) (i32.sub (i32.sub (local.get $n) (local.get $idx)) (i32.const 1)))",
    );
    b.push("(struct.set $SET 0 (local.get $s) (i32.sub (local.get $n) (i32.const 1)))");
    fs.push(Func {
        signature: "(func $set_remove_at (param $s (ref null $SET)) (param $idx i32)".into(),
        locals: vec!["(local $n i32)".into()],
        body: b,
    });

    // $py_set: build a set from an iterable (set() / set literal / set comp).
    let mut b = Body::new();
    b.push(
        "(local.set $s (struct.new $SET (i32.const 0) (array.new_fixed $ITEMS 0) (i32.const 0)))",
    );
    b.push("(if (ref.is_null (local.get $it)) (then (return (local.get $s))))");
    b.push("(local.set $n (call $py_len (local.get $it)))");
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(call $set_insert (local.get $s) (call $py_index (local.get $it) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push("(local.get $s)");
    fs.push(Func {
        signature: "(func $py_set (param $it (ref null eq)) (result (ref null eq))".into(),
        locals: vec![
            "(local $s (ref null $SET))".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // $set_eq: same size and every element of `a` is in `b`.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $SET 0 (local.get $a)))");
    b.push("(if (i32.ne (local.get $n) (struct.get $SET 0 (local.get $b))) (then (return (i32.const 0))))");
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (i32.lt_s (call $set_find (local.get $b) (array.get $ITEMS (struct.get $SET 1 (local.get $a)) (local.get $i))) (i32.const 0)) (then (return (i32.const 0))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push("(i32.const 1)");
    fs.push(Func {
        signature:
            "(func $set_eq (param $a (ref null $SET)) (param $b (ref null $SET)) (result i32)"
                .into(),
        locals: vec!["(local $n i32)".into(), "(local $i i32)".into()],
        body: b,
    });

    // $py_setop: union (0) / intersection (1) / symmetric difference (2) /
    // difference (3). Both operands must be sets.
    let mut b = Body::new();
    b.push("(if (i32.eqz (i32.and (ref.test (ref $SET) (local.get $a)) (ref.test (ref $SET) (local.get $b)))) (then (call $raise_setop) (unreachable)))");
    b.push("(local.set $sa (ref.cast (ref $SET) (local.get $a)))");
    b.push("(local.set $sb (ref.cast (ref $SET) (local.get $b)))");
    b.push(
        "(local.set $out (struct.new $SET (i32.const 0) (array.new_fixed $ITEMS 0) (i32.const 0)))",
    );
    // Union: every element of a, then of b.
    b.push("(if (i32.eq (local.get $mode) (i32.const 0)) (then");
    b.push_in(1, "(local.set $i (i32.const 0))");
    b.push_in(1, "(block $ud (loop $ul");
    b.push_in(
        2,
        "(br_if $ud (i32.ge_s (local.get $i) (struct.get $SET 0 (local.get $sa))))",
    );
    b.push_in(2, "(call $set_insert (local.get $out) (array.get $ITEMS (struct.get $SET 1 (local.get $sa)) (local.get $i)))");
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (i32.const 1))) (br $ul)))",
    );
    b.push_in(1, "(local.set $i (i32.const 0))");
    b.push_in(1, "(block $ud2 (loop $ul2");
    b.push_in(
        2,
        "(br_if $ud2 (i32.ge_s (local.get $i) (struct.get $SET 0 (local.get $sb))))",
    );
    b.push_in(2, "(call $set_insert (local.get $out) (array.get $ITEMS (struct.get $SET 1 (local.get $sb)) (local.get $i)))");
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (i32.const 1))) (br $ul2)))",
    );
    b.push("))");
    // Intersection (1) / difference (3): elements of a that are / aren't in b.
    b.push("(if (i32.or (i32.eq (local.get $mode) (i32.const 1)) (i32.eq (local.get $mode) (i32.const 3))) (then");
    b.push_in(1, "(local.set $i (i32.const 0))");
    b.push_in(1, "(block $id (loop $il");
    b.push_in(
        2,
        "(br_if $id (i32.ge_s (local.get $i) (struct.get $SET 0 (local.get $sa))))",
    );
    b.push_in(
        2,
        "(local.set $e (array.get $ITEMS (struct.get $SET 1 (local.get $sa)) (local.get $i)))",
    );
    b.push_in(2, "(local.set $found (i32.ge_s (call $set_find (local.get $sb) (local.get $e)) (i32.const 0)))");
    // keep when (intersection & found) or (difference & !found)
    b.push_in(2, "(if (i32.or (i32.and (i32.eq (local.get $mode) (i32.const 1)) (local.get $found)) (i32.and (i32.eq (local.get $mode) (i32.const 3)) (i32.eqz (local.get $found)))) (then (call $set_insert (local.get $out) (local.get $e))))");
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (i32.const 1))) (br $il)))",
    );
    b.push("))");
    // Symmetric difference (2): (a - b) then (b - a).
    b.push("(if (i32.eq (local.get $mode) (i32.const 2)) (then");
    b.push_in(1, "(local.set $i (i32.const 0))");
    b.push_in(1, "(block $xd (loop $xl");
    b.push_in(
        2,
        "(br_if $xd (i32.ge_s (local.get $i) (struct.get $SET 0 (local.get $sa))))",
    );
    b.push_in(
        2,
        "(local.set $e (array.get $ITEMS (struct.get $SET 1 (local.get $sa)) (local.get $i)))",
    );
    b.push_in(2, "(if (i32.lt_s (call $set_find (local.get $sb) (local.get $e)) (i32.const 0)) (then (call $set_insert (local.get $out) (local.get $e))))");
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (i32.const 1))) (br $xl)))",
    );
    b.push_in(1, "(local.set $i (i32.const 0))");
    b.push_in(1, "(block $xd2 (loop $xl2");
    b.push_in(
        2,
        "(br_if $xd2 (i32.ge_s (local.get $i) (struct.get $SET 0 (local.get $sb))))",
    );
    b.push_in(
        2,
        "(local.set $e (array.get $ITEMS (struct.get $SET 1 (local.get $sb)) (local.get $i)))",
    );
    b.push_in(2, "(if (i32.lt_s (call $set_find (local.get $sa) (local.get $e)) (i32.const 0)) (then (call $set_insert (local.get $out) (local.get $e))))");
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (i32.const 1))) (br $xl2)))",
    );
    b.push("))");
    b.push("(local.get $out)");
    fs.push(Func {
        signature:
            "(func $py_setop (param $a (ref null eq)) (param $b (ref null eq)) (param $mode i32) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $sa (ref null $SET))".into(),
            "(local $sb (ref null $SET))".into(),
            "(local $out (ref null $SET))".into(),
            "(local $i i32)".into(),
            "(local $e (ref null eq))".into(),
            "(local $found i32)".into(),
        ],
        body: b,
    });

    // $print_set: `{e1, e2}` (insertion order); empty prints `set()`.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $SET 0 (local.get $s)))");
    b.push("(if (i32.eqz (local.get $n))");
    b.push_in(1, "(then");
    for c in "set()".bytes() {
        b.push_in(2, format!("(call $write_char (i32.const {c}))"));
    }
    b.push_in(2, "(return)))");
    b.push("(call $write_char (i32.const 123))"); // {
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (i32.gt_s (local.get $i) (i32.const 0)) (then (call $write_char (i32.const 44)) (call $write_char (i32.const 32))))",
    );
    b.push_in(
        2,
        "(call $print_repr (array.get $ITEMS (struct.get $SET 1 (local.get $s)) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push("(call $write_char (i32.const 125))"); // }
    fs.push(Func {
        signature: "(func $print_set (param $s (ref null $SET))".into(),
        locals: vec!["(local $n i32)".into(), "(local $i i32)".into()],
        body: b,
    });

    // $set_add / $set_discard / $set_remove: the methods (fall back to dispatch
    // for non-set receivers). add returns None; remove raises KeyError if
    // absent; discard is a no-op if absent.
    for (fname, mode) in [("$set_add", 0), ("$set_discard", 1), ("$set_remove", 2)] {
        let mut b = Body::new();
        b.push("(if (i32.eqz (ref.test (ref $SET) (local.get $r)))");
        b.push_in(
            1,
            "(then (return (call $call_method (local.get $r) (local.get $name) (local.get $args)))))",
        );
        b.push("(local.set $ss (ref.cast (ref $SET) (local.get $r)))");
        if mode == 0 {
            b.push("(call $set_insert (local.get $ss) (local.get $v))");
            b.push("(global.get $NONE)");
        } else {
            b.push("(local.set $idx (call $set_find (local.get $ss) (local.get $v)))");
            b.push("(if (i32.ge_s (local.get $idx) (i32.const 0))");
            b.push_in(
                1,
                "(then (call $set_remove_at (local.get $ss) (local.get $idx)))",
            );
            if mode == 2 {
                b.push_in(1, "(else (call $raise_key (local.get $v)) (unreachable))");
            }
            b.push(")");
            b.push("(global.get $NONE)");
        }
        fs.push(Func {
            signature: format!(
                "(func {fname} (param $r (ref null eq)) (param $v (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
            ),
            locals: vec![
                "(local $ss (ref null $SET))".into(),
                "(local $idx i32)".into(),
            ],
            body: b,
        });
    }

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
    b.push_in(1, "(then (call $raise_key (local.get $k)))");
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

    // $py_subscript / $py_set_subscript: general `obj[key]` — a custom
    // __getitem__ first, then dicts take the key as a value; lists/strings
    // unbox it as a position.
    let mut b = Body::new();
    b.push(format!(
        "(if (call $obj_has (local.get $r) {n}) (then (return (call $obj_call1 (local.get $r) (local.get $k) {n}))))",
        n = str_lit("__getitem__")
    ));
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $dict_get (local.get $r) (local.get $k))))",
    );
    b.push(")");
    // Sets aren't subscriptable (even though iteration uses $py_index).
    b.push("(if (ref.test (ref $SET) (local.get $r)) (then (call $raise_not_sub (local.get $r)) (unreachable)))");
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

    // $slice_adjust: clamp one explicit slice bound into range, Python-style
    // (negative indices count from the end; out-of-range clamps, never errors).
    let mut b = Body::new();
    b.push("(if (i32.lt_s (local.get $i) (i32.const 0))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (i32.lt_s (local.get $i) (local.get $lower)) (then (local.set $i (local.get $lower)))))",
    );
    b.push_in(1, "(else");
    b.push_in(
        2,
        "(if (i32.gt_s (local.get $i) (local.get $upper)) (then (local.set $i (local.get $upper))))))",
    );
    b.push("(local.get $i)");
    fs.push(Func {
        signature:
            "(func $slice_adjust (param $i i32) (param $n i32) (param $lower i32) (param $upper i32) (result i32)"
                .into(),
        locals: vec![],
        body: b,
    });

    // $py_slice: `seq[start:stop:step]` for lists and strings. Bounds arrive
    // boxed, with $NONE for an omitted one; this mirrors CPython's
    // PySlice_AdjustIndices (clamping defaults that depend on the step's sign).
    let mut b = Body::new();
    b.push("(if (i32.eqz (i32.or (ref.test (ref $LIST) (local.get $r)) (ref.test (ref $STR) (local.get $r))))");
    b.push_in(1, "(then (call $raise_not_sub (local.get $r))))");
    b.push("(local.set $n (call $py_len (local.get $r)))");
    // step: default 1, zero is a ValueError.
    b.push("(if (ref.test (ref $NONE_T) (local.get $tv))");
    b.push_in(1, "(then (local.set $step (i32.const 1)))");
    b.push_in(1, "(else (local.set $step (call $unbox (local.get $tv)))))");
    b.push("(if (i32.eqz (local.get $step)) (then (call $raise_slice_step)))");
    // clamp bounds depend on the step's sign.
    b.push("(if (i32.lt_s (local.get $step) (i32.const 0))");
    b.push_in(
        1,
        "(then (local.set $lower (i32.const -1)) (local.set $upper (i32.sub (local.get $n) (i32.const 1))))",
    );
    b.push_in(
        1,
        "(else (local.set $lower (i32.const 0)) (local.set $upper (local.get $n))))",
    );
    // start: omitted -> step-dependent default; explicit -> clamp.
    b.push("(if (ref.test (ref $NONE_T) (local.get $sv))");
    b.push_in(
        1,
        "(then (if (i32.lt_s (local.get $step) (i32.const 0)) (then (local.set $start (local.get $upper))) (else (local.set $start (local.get $lower)))))",
    );
    b.push_in(
        1,
        "(else (local.set $start (call $slice_adjust (call $unbox (local.get $sv)) (local.get $n) (local.get $lower) (local.get $upper)))))",
    );
    // stop: omitted -> step-dependent default; explicit -> clamp.
    b.push("(if (ref.test (ref $NONE_T) (local.get $ev))");
    b.push_in(
        1,
        "(then (if (i32.lt_s (local.get $step) (i32.const 0)) (then (local.set $stop (local.get $lower))) (else (local.set $stop (local.get $upper)))))",
    );
    b.push_in(
        1,
        "(else (local.set $stop (call $slice_adjust (call $unbox (local.get $ev)) (local.get $n) (local.get $lower) (local.get $upper)))))",
    );
    // count = number of produced elements.
    b.push("(if (i32.gt_s (local.get $step) (i32.const 0))");
    b.push_in(
        1,
        "(then (if (i32.gt_s (local.get $stop) (local.get $start)) (then (local.set $count (i32.add (i32.div_s (i32.sub (i32.sub (local.get $stop) (local.get $start)) (i32.const 1)) (local.get $step)) (i32.const 1)))) (else (local.set $count (i32.const 0)))))",
    );
    b.push_in(
        1,
        "(else (if (i32.gt_s (local.get $start) (local.get $stop)) (then (local.set $count (i32.add (i32.div_s (i32.sub (i32.sub (local.get $start) (local.get $stop)) (i32.const 1)) (i32.sub (i32.const 0) (local.get $step))) (i32.const 1)))) (else (local.set $count (i32.const 0))))))",
    );
    // materialize: lists copy elements, strings copy bytes.
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(local.set $src (struct.get $LIST 1 (ref.cast (ref $LIST) (local.get $r))))",
    );
    b.push_in(
        2,
        "(local.set $items (array.new_default $ITEMS (local.get $count)))",
    );
    b.push_in(2, "(local.set $i (local.get $start))");
    b.push_in(2, "(local.set $j (i32.const 0))");
    b.push_in(2, "(block $ld (loop $ln");
    b.push_in(
        3,
        "(br_if $ld (i32.ge_s (local.get $j) (local.get $count)))",
    );
    b.push_in(
        3,
        "(array.set $ITEMS (local.get $items) (local.get $j) (array.get $ITEMS (local.get $src) (local.get $i)))",
    );
    b.push_in(
        3,
        "(local.set $i (i32.add (local.get $i) (local.get $step)))",
    );
    b.push_in(3, "(local.set $j (i32.add (local.get $j) (i32.const 1)))");
    b.push_in(3, "(br $ln)))");
    b.push_in(
        2,
        "(return (struct.new $LIST (local.get $count) (local.get $items)))))",
    );
    // string path
    b.push("(local.set $ssrc (ref.cast (ref $STR) (local.get $r)))");
    b.push("(local.set $str (array.new_default $STR (local.get $count)))");
    b.push("(local.set $i (local.get $start))");
    b.push("(local.set $j (i32.const 0))");
    b.push("(block $sd (loop $sn");
    b.push_in(
        1,
        "(br_if $sd (i32.ge_s (local.get $j) (local.get $count)))",
    );
    b.push_in(
        1,
        "(array.set $STR (local.get $str) (local.get $j) (array.get_u $STR (local.get $ssrc) (local.get $i)))",
    );
    b.push_in(
        1,
        "(local.set $i (i32.add (local.get $i) (local.get $step)))",
    );
    b.push_in(1, "(local.set $j (i32.add (local.get $j) (i32.const 1)))");
    b.push_in(1, "(br $sn)))");
    b.push("(local.get $str)");
    fs.push(Func {
        signature:
            "(func $py_slice (param $r (ref null eq)) (param $sv (ref null eq)) (param $ev (ref null eq)) (param $tv (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $n i32)".into(),
            "(local $step i32)".into(),
            "(local $start i32)".into(),
            "(local $stop i32)".into(),
            "(local $lower i32)".into(),
            "(local $upper i32)".into(),
            "(local $count i32)".into(),
            "(local $i i32)".into(),
            "(local $j i32)".into(),
            "(local $src (ref null $ITEMS))".into(),
            "(local $items (ref null $ITEMS))".into(),
            "(local $ssrc (ref null $STR))".into(),
            "(local $str (ref null $STR))".into(),
        ],
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

    // $list_contains: element-wise membership via py_eq.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $LIST 0 (local.get $l)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (call $py_eq (array.get $ITEMS (struct.get $LIST 1 (local.get $l)) (local.get $i)) (local.get $item))",
    );
    b.push_in(3, "(then (return (i32.const 1)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 0)");
    fs.push(Func {
        signature:
            "(func $list_contains (param $l (ref null $LIST)) (param $item (ref null eq)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $tuple_contains: element-wise membership via py_eq (like list_contains).
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $TUPLE 0 (local.get $l)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(if (call $py_eq (array.get $ITEMS (struct.get $TUPLE 1 (local.get $l)) (local.get $i)) (local.get $item))",
    );
    b.push_in(3, "(then (return (i32.const 1)))");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 0)");
    fs.push(Func {
        signature:
            "(func $tuple_contains (param $l (ref null $TUPLE)) (param $item (ref null eq)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $str_contains: naive substring search (empty needle matches).
    let mut b = Body::new();
    b.push("(local.set $hl (array.len (local.get $h)))");
    b.push("(local.set $nl (array.len (local.get $needle)))");
    b.push("(if (i32.eqz (local.get $nl)) (then (return (i32.const 1))))");
    b.push("(block $no");
    b.push_in(1, "(loop $outer");
    b.push_in(
        2,
        "(br_if $no (i32.gt_s (i32.add (local.get $i) (local.get $nl)) (local.get $hl)))",
    );
    b.push_in(2, "(local.set $j (i32.const 0))");
    b.push_in(2, "(block $fail");
    b.push_in(3, "(loop $inner");
    b.push_in(
        4,
        "(if (i32.ge_s (local.get $j) (local.get $nl)) (then (return (i32.const 1))))",
    );
    b.push_in(4, "(br_if $fail (i32.ne");
    b.push_in(
        5,
        "(array.get_u $STR (local.get $h) (i32.add (local.get $i) (local.get $j)))",
    );
    b.push_in(5, "(array.get_u $STR (local.get $needle) (local.get $j))))");
    b.push_in(4, "(local.set $j (i32.add (local.get $j) (i32.const 1)))");
    b.push_in(4, "(br $inner)");
    b.push_in(3, ")");
    b.push_in(2, ")");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $outer)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.const 0)");
    fs.push(Func {
        signature:
            "(func $str_contains (param $h (ref null $STR)) (param $needle (ref null $STR)) (result i32)"
                .into(),
        locals: vec![
            "(local $hl i32)".into(),
            "(local $nl i32)".into(),
            "(local $i i32)".into(),
            "(local $j i32)".into(),
        ],
        body: b,
    });

    // $py_in: membership — list elements, dict keys, substrings.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $LIST) (local.get $c))");
    b.push_in(
        1,
        "(then (return (call $list_contains (ref.cast (ref $LIST) (local.get $c)) (local.get $item))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $TUPLE) (local.get $c))");
    b.push_in(
        1,
        "(then (return (call $tuple_contains (ref.cast (ref $TUPLE) (local.get $c)) (local.get $item))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $SET) (local.get $c))");
    b.push_in(
        1,
        "(then (return (i32.ge_s (call $set_find (ref.cast (ref $SET) (local.get $c)) (local.get $item)) (i32.const 0)))))",
    );
    b.push("(if (ref.test (ref $DICT) (local.get $c))");
    b.push_in(
        1,
        "(then (return (i32.ge_s (call $dict_find (ref.cast (ref $DICT) (local.get $c)) (local.get $item)) (i32.const 0))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $STR) (local.get $c))");
    b.push_in(1, "(then");
    b.push_in(2, "(if (i32.eqz (ref.test (ref $STR) (local.get $item)))");
    b.push_in(3, "(then (call $raise_in_str (local.get $item)))");
    b.push_in(2, ")");
    b.push_in(
        2,
        "(return (call $str_contains (ref.cast (ref $STR) (local.get $c)) (ref.cast (ref $STR) (local.get $item))))",
    );
    b.push_in(1, ")");
    b.push(")");
    b.push("(call $raise_not_iter (local.get $c))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $py_in (param $item (ref null eq)) (param $c (ref null eq)) (result i32)"
            .into(),
        locals: vec![],
        body: b,
    });

    // $i32_to_str: decimal digits as a $STR. Works on the unsigned
    // magnitude so INT_MIN (whose negation overflows i32) is correct.
    let mut b = Body::new();
    b.push("(if (i32.eqz (local.get $v))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $s (array.new_default $STR (i32.const 1)))");
    b.push_in(
        2,
        "(array.set $STR (local.get $s) (i32.const 0) (i32.const 48))",
    );
    b.push_in(2, "(return (local.get $s))");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.set $neg (i32.lt_s (local.get $v) (i32.const 0)))");
    b.push("(local.set $mag (select (i32.sub (i32.const 0) (local.get $v)) (local.get $v) (local.get $neg)))");
    b.push("(local.set $tmp (local.get $mag))");
    b.push("(block $counted");
    b.push_in(1, "(loop $count");
    b.push_in(2, "(br_if $counted (i32.eqz (local.get $tmp)))");
    b.push_in(
        2,
        "(local.set $len (i32.add (local.get $len) (i32.const 1)))",
    );
    b.push_in(
        2,
        "(local.set $tmp (i32.div_u (local.get $tmp) (i32.const 10)))",
    );
    b.push_in(2, "(br $count)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.set $s (array.new_default $STR (i32.add (local.get $len) (local.get $neg))))");
    b.push("(local.set $i (i32.sub (i32.add (local.get $len) (local.get $neg)) (i32.const 1)))");
    b.push("(block $done");
    b.push_in(1, "(loop $fill");
    b.push_in(
        2,
        "(array.set $STR (local.get $s) (local.get $i) (i32.add (i32.const 48) (i32.rem_u (local.get $mag) (i32.const 10))))",
    );
    b.push_in(
        2,
        "(local.set $mag (i32.div_u (local.get $mag) (i32.const 10)))",
    );
    b.push_in(2, "(local.set $i (i32.sub (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br_if $done (i32.eqz (local.get $mag)))");
    b.push_in(2, "(br $fill)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(if (local.get $neg)");
    b.push_in(
        1,
        "(then (array.set $STR (local.get $s) (i32.const 0) (i32.const 45)))",
    );
    b.push(")");
    b.push("(local.get $s)");
    fs.push(Func {
        signature: "(func $i32_to_str (param $v i32) (result (ref null $STR))".into(),
        locals: vec![
            "(local $neg i32)".into(),
            "(local $mag i32)".into(),
            "(local $tmp i32)".into(),
            "(local $len i32)".into(),
            "(local $i i32)".into(),
            "(local $s (ref null $STR))".into(),
        ],
        body: b,
    });

    // $to_str: str(x) — strings pass through; ints/bools/None convert.
    // Floats and containers raise a friendly not-yet error.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(1, "(then (return (local.get $r)))");
    b.push(")");
    b.push("(if (ref.test (ref $BOOL) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(return (if (result (ref null eq)) (struct.get_u $BOOL 0 (ref.cast (ref $BOOL) (local.get $r)))",
    );
    b.push_in(
        3,
        "(then (array.new_fixed $STR 4 (i32.const 84) (i32.const 114) (i32.const 117) (i32.const 101)))",
    );
    b.push_in(
        3,
        "(else (array.new_fixed $STR 5 (i32.const 70) (i32.const 97) (i32.const 108) (i32.const 115) (i32.const 101)))))",
    );
    b.push_in(1, ")");
    b.push(")");
    b.push("(if (ref.test (ref $NONE_T) (local.get $r))");
    b.push_in(
        1,
        "(then (return (array.new_fixed $STR 4 (i32.const 78) (i32.const 111) (i32.const 110) (i32.const 101))))",
    );
    b.push(")");
    b.push("(if (i32.or (ref.test (ref i31) (local.get $r)) (ref.test (ref $INT) (local.get $r)))");
    b.push_in(
        1,
        "(then (return (call $i32_to_str (call $unbox (local.get $r)))))",
    );
    b.push(")");
    // Collections render to their repr (elements in repr form). Floats inside
    // still hit $raise_no_str — str(float) is unsupported.
    b.push("(if (ref.test (ref $LIST) (local.get $r)) (then (return (call $list_to_str (ref.cast (ref $LIST) (local.get $r))))))");
    b.push("(if (ref.test (ref $TUPLE) (local.get $r)) (then (return (call $tuple_to_str (ref.cast (ref $TUPLE) (local.get $r))))))");
    b.push("(if (ref.test (ref $DICT) (local.get $r)) (then (return (call $dict_to_str (ref.cast (ref $DICT) (local.get $r))))))");
    b.push("(if (ref.test (ref $SET) (local.get $r)) (then (return (call $set_to_str (ref.cast (ref $SET) (local.get $r))))))");
    b.push("(if (ref.test (ref $OBJECT) (local.get $r)) (then (return (call $object_to_str (local.get $r) (i32.const 1)))))");
    b.push("(call $raise_no_str (local.get $r))");
    b.push("unreachable");
    fs.push(Func {
        signature: "(func $to_str (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $repr_str: the form used for collection elements — strings get quotes,
    // everything else uses $to_str.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        format!(
            "(then (return (call $py_add (call $py_add {q} (local.get $r)) {q})))",
            q = str_lit("'")
        ),
    );
    b.push(")");
    b.push("(if (ref.test (ref $OBJECT) (local.get $r)) (then (return (call $object_to_str (local.get $r) (i32.const 0)))))");
    b.push("(call $to_str (local.get $r))");
    fs.push(Func {
        signature: "(func $repr_str (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $list_to_str / $tuple_to_str / $set_to_str: `[..]` / `(..)` / `{..}` with
    // repr-form elements joined by ", ". Built with $py_add (quadratic, fine at
    // classroom sizes).
    for (fname, ty, open, close) in [
        ("$list_to_str", "$LIST", "[", "]"),
        ("$tuple_to_str", "$TUPLE", "(", ")"),
        ("$set_to_str", "$SET", "{", "}"),
    ] {
        let mut b = Body::new();
        b.push(format!("(local.set $n (struct.get {ty} 0 (local.get $s)))"));
        if ty == "$SET" {
            b.push(format!(
                "(if (i32.eqz (local.get $n)) (then (return {})))",
                str_lit("set()")
            ));
        }
        b.push(format!("(local.set $res {})", str_lit(open)));
        b.push("(block $done (loop $next");
        b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
        b.push_in(
            2,
            format!(
                "(if (i32.gt_s (local.get $i) (i32.const 0)) (then (local.set $res (call $py_add (local.get $res) {}))))",
                str_lit(", ")
            ),
        );
        b.push_in(
            2,
            format!("(local.set $res (call $py_add (local.get $res) (call $repr_str (array.get $ITEMS (struct.get {ty} 1 (local.get $s)) (local.get $i)))))"),
        );
        b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
        b.push_in(2, "(br $next)))");
        // A 1-tuple keeps its trailing comma.
        if ty == "$TUPLE" {
            b.push(format!(
                "(if (i32.eq (local.get $n) (i32.const 1)) (then (local.set $res (call $py_add (local.get $res) {}))))",
                str_lit(",")
            ));
        }
        b.push(format!(
            "(call $py_add (local.get $res) {})",
            str_lit(close)
        ));
        fs.push(Func {
            signature: format!("(func {fname} (param $s (ref null {ty})) (result (ref null eq))"),
            locals: vec![
                "(local $n i32)".into(),
                "(local $i i32)".into(),
                "(local $res (ref null eq))".into(),
            ],
            body: b,
        });
    }

    // $dict_to_str: `{k: v, ...}` (both repr).
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $DICT 0 (local.get $d)))");
    b.push(format!("(local.set $res {})", str_lit("{")));
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        format!(
            "(if (i32.gt_s (local.get $i) (i32.const 0)) (then (local.set $res (call $py_add (local.get $res) {}))))",
            str_lit(", ")
        ),
    );
    b.push_in(
        2,
        "(local.set $res (call $py_add (local.get $res) (call $repr_str (array.get $ITEMS (struct.get $DICT 1 (local.get $d)) (local.get $i)))))",
    );
    b.push_in(
        2,
        format!(
            "(local.set $res (call $py_add (local.get $res) {}))",
            str_lit(": ")
        ),
    );
    b.push_in(
        2,
        "(local.set $res (call $py_add (local.get $res) (call $repr_str (array.get $ITEMS (struct.get $DICT 2 (local.get $d)) (local.get $i)))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push(format!("(call $py_add (local.get $res) {})", str_lit("}")));
    fs.push(Func {
        signature: "(func $dict_to_str (param $d (ref null $DICT)) (result (ref null eq))".into(),
        locals: vec![
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $res (ref null eq))".into(),
        ],
        body: b,
    });

    // $object_to_str: an instance as a string — __str__ (or __repr__) result if
    // defined, else `<Name object>`. `prefer_str` mirrors $object_display.
    let mut b = Body::new();
    b.push("(local.set $cls (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))))");
    b.push("(if (local.get $prefer_str)");
    b.push_in(
        1,
        format!(
            "(then (local.set $m (call $class_lookup_method (local.get $cls) {}))))",
            str_lit("__str__")
        ),
    );
    b.push("(if (i32.eqz (ref.test (ref $METHOD) (local.get $m)))");
    b.push_in(
        1,
        format!(
            "(then (local.set $m (call $class_lookup_method (local.get $cls) {}))))",
            str_lit("__repr__")
        ),
    );
    b.push("(if (ref.test (ref $METHOD) (local.get $m))");
    b.push_in(
        1,
        "(then (return (call_ref $MFUNC (local.get $obj) (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)) (struct.get $METHOD 0 (ref.cast (ref $METHOD) (local.get $m)))))))",
    );
    b.push(format!(
        "(call $py_add (call $py_add {lt} (struct.get $CLASS 0 (local.get $cls))) {obj})",
        lt = str_lit("<"),
        obj = str_lit(" object>")
    ));
    fs.push(Func {
        signature:
            "(func $object_to_str (param $obj (ref null eq)) (param $prefer_str i32) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $cls (ref null $CLASS))".into(),
            "(local $m (ref null eq))".into(),
        ],
        body: b,
    });

    // $f64_to_fixed: format a float with `prec` decimal places (rounded ties to
    // even via f64.nearest), e.g. (3.14159, 2) -> "3.14". Assembled from pieces
    // with $py_add. (Values whose scaled magnitude exceeds i32 saturate.)
    let mut b = Body::new();
    b.push("(local.set $neg (f64.lt (local.get $x) (f64.const 0)))");
    b.push("(if (local.get $neg) (then (local.set $x (f64.neg (local.get $x)))))");
    // scale = 10^prec (f64 and i32)
    b.push("(local.set $scale (f64.const 1))");
    b.push("(local.set $sci (i32.const 1))");
    b.push("(local.set $k (i32.const 0))");
    b.push("(block $sd (loop $sl");
    b.push_in(2, "(br_if $sd (i32.ge_s (local.get $k) (local.get $prec)))");
    b.push_in(
        2,
        "(local.set $scale (f64.mul (local.get $scale) (f64.const 10)))",
    );
    b.push_in(
        2,
        "(local.set $sci (i32.mul (local.get $sci) (i32.const 10)))",
    );
    b.push_in(2, "(local.set $k (i32.add (local.get $k) (i32.const 1)))");
    b.push_in(2, "(br $sl)))");
    b.push("(local.set $scaled (i32.trunc_sat_f64_s (f64.nearest (f64.mul (local.get $x) (local.get $scale)))))");
    b.push("(local.set $ip (i32.div_s (local.get $scaled) (local.get $sci)))");
    b.push("(local.set $fp (i32.rem_s (local.get $scaled) (local.get $sci)))");
    b.push("(local.set $res (call $i32_to_str (local.get $ip)))");
    b.push("(if (i32.gt_s (local.get $prec) (i32.const 0))");
    b.push_in(1, "(then");
    // fracarr: prec digits, zero-padded, filled right-to-left
    b.push_in(
        2,
        "(local.set $frac (array.new_default $STR (local.get $prec)))",
    );
    b.push_in(
        2,
        "(local.set $k (i32.sub (local.get $prec) (i32.const 1)))",
    );
    b.push_in(2, "(block $fd (loop $fl");
    b.push_in(3, "(br_if $fd (i32.lt_s (local.get $k) (i32.const 0)))");
    b.push_in(
        3,
        "(array.set $STR (local.get $frac) (local.get $k) (i32.add (i32.const 48) (i32.rem_u (local.get $fp) (i32.const 10))))",
    );
    b.push_in(
        3,
        "(local.set $fp (i32.div_u (local.get $fp) (i32.const 10)))",
    );
    b.push_in(3, "(local.set $k (i32.sub (local.get $k) (i32.const 1)))");
    b.push_in(3, "(br $fl)))");
    b.push_in(
        2,
        "(local.set $res (call $py_add (call $py_add (local.get $res) (array.new_fixed $STR 1 (i32.const 46))) (local.get $frac)))",
    );
    b.push_in(1, "))");
    b.push("(if (local.get $neg) (then (local.set $res (call $py_add (array.new_fixed $STR 1 (i32.const 45)) (local.get $res)))))");
    b.push("(local.get $res)");
    fs.push(Func {
        signature: "(func $f64_to_fixed (param $x f64) (param $prec i32) (result (ref null eq))"
            .into(),
        locals: vec![
            "(local $neg i32)".into(),
            "(local $scale f64)".into(),
            "(local $sci i32)".into(),
            "(local $scaled i32)".into(),
            "(local $ip i32)".into(),
            "(local $fp i32)".into(),
            "(local $k i32)".into(),
            "(local $res (ref null eq))".into(),
            "(local $frac (ref null $STR))".into(),
        ],
        body: b,
    });

    // $str_pad: pad a string to `width` with `fill`, aligned (0=left/1=right/
    // 2=center). Returns the string unchanged if already wide enough.
    let mut b = Body::new();
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(if (i32.ge_s (local.get $n) (local.get $width)) (then (return (local.get $ss))))");
    b.push("(local.set $pad (i32.sub (local.get $width) (local.get $n)))");
    b.push("(local.set $lp (if (result i32) (i32.eq (local.get $align) (i32.const 1)) (then (local.get $pad)) (else (if (result i32) (i32.eq (local.get $align) (i32.const 2)) (then (i32.div_s (local.get $pad) (i32.const 2))) (else (i32.const 0))))))");
    b.push("(local.set $out (array.new_default $STR (local.get $width)))");
    b.push("(array.fill $STR (local.get $out) (i32.const 0) (local.get $fill) (local.get $width))");
    b.push("(array.copy $STR $STR (local.get $out) (local.get $lp) (local.get $ss) (i32.const 0) (local.get $n))");
    b.push("(local.get $out)");
    fs.push(Func {
        signature:
            "(func $str_pad (param $s (ref null eq)) (param $width i32) (param $fill i32) (param $align i32) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $pad i32)".into(),
            "(local $lp i32)".into(),
            "(local $out (ref null $STR))".into(),
        ],
        body: b,
    });

    // $py_abs: float-aware absolute value.
    let mut b = Body::new();
    b.push("(if (result (ref null eq)) (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (struct.new $FLOAT (f64.abs (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r))))))",
    );
    b.push_in(1, "(else");
    b.push_in(2, "(local.set $v (call $unbox (local.get $r)))");
    b.push_in(
        2,
        "(call $box (select (i32.sub (i32.const 0) (local.get $v)) (local.get $v) (i32.lt_s (local.get $v) (i32.const 0))))",
    );
    b.push_in(1, "))");
    fs.push(Func {
        signature: "(func $py_abs (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec!["(local $v i32)".into()],
        body: b,
    });

    // min/max return the winning ORIGINAL value (min(1, 2.0) is 1, an int).
    // min/max reduce over an iterable (callers wrap multiple positional args in
    // a list). Comparison via $sort_lt (lexicographic for strings, else
    // numeric). Empty input is a ValueError. The winner keeps its original
    // value/type (so min(1, 2.0) is the int 1).
    for (name, take_when) in [
        // min: take element when element < acc.
        (
            "$py_min",
            "(call $sort_lt (local.get $el) (local.get $acc))",
        ),
        // max: take element when acc < element.
        (
            "$py_max",
            "(call $sort_lt (local.get $acc) (local.get $el))",
        ),
    ] {
        let mut b = Body::new();
        b.push("(local.set $n (call $py_len (local.get $seq)))");
        b.push("(if (i32.eqz (local.get $n)) (then (call $raise_empty) (unreachable)))");
        b.push("(local.set $acc (call $py_index (local.get $seq) (i32.const 0)))");
        b.push("(local.set $i (i32.const 1))");
        b.push("(block $d (loop $l");
        b.push_in(2, "(br_if $d (i32.ge_s (local.get $i) (local.get $n)))");
        b.push_in(
            2,
            "(local.set $el (call $py_index (local.get $seq) (local.get $i)))",
        );
        b.push_in(
            2,
            format!("(if {take_when} (then (local.set $acc (local.get $el))))"),
        );
        b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
        b.push_in(2, "(br $l)))");
        b.push("(local.get $acc)");
        fs.push(Func {
            signature: format!("(func {name} (param $seq (ref null eq)) (result (ref null eq))"),
            locals: vec![
                "(local $n i32)".into(),
                "(local $i i32)".into(),
                "(local $acc (ref null eq))".into(),
                "(local $el (ref null eq))".into(),
            ],
            body: b,
        });
    }

    // $py_bool: truthiness as a bool value.
    let mut b = Body::new();
    b.push("(call $bool (call $truthy (local.get $r)))");
    fs.push(Func {
        signature: "(func $py_bool (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $py_round1: round to the nearest integer, ties to even (f64.nearest is
    // exactly Python's round()). $py_round2: round to n decimal places (float).
    let mut b = Body::new();
    b.push("(call $box (i32.trunc_sat_f64_s (f64.nearest (call $unbox_f64 (local.get $r)))))");
    fs.push(Func {
        signature: "(func $py_round1 (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });
    let mut b = Body::new();
    b.push("(local.set $x (call $unbox_f64 (local.get $r)))");
    b.push("(local.set $scale (f64.const 1))");
    b.push("(if (i32.ge_s (local.get $n) (i32.const 0))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $i (i32.const 0))");
    b.push_in(2, "(block $d (loop $l (br_if $d (i32.ge_s (local.get $i) (local.get $n))) (local.set $scale (f64.mul (local.get $scale) (f64.const 10))) (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l))))");
    b.push_in(1, "(else");
    b.push_in(2, "(local.set $i (i32.const 0))");
    b.push_in(2, "(block $d2 (loop $l2 (br_if $d2 (i32.ge_s (local.get $i) (i32.sub (i32.const 0) (local.get $n)))) (local.set $scale (f64.mul (local.get $scale) (f64.const 0.1))) (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l2))))");
    b.push(")");
    b.push("(struct.new $FLOAT (f64.div (f64.nearest (f64.mul (local.get $x) (local.get $scale))) (local.get $scale)))");
    fs.push(Func {
        signature: "(func $py_round2 (param $r (ref null eq)) (param $n i32) (result (ref null eq))".into(),
        locals: vec![
            "(local $x f64)".into(),
            "(local $scale f64)".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // int(x): floats truncate toward zero; a string is parsed (so
    // int(input()) works); ints/bools pass through unbox.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $FLOAT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $box (i32.trunc_sat_f64_s (struct.get $FLOAT 0 (ref.cast (ref $FLOAT) (local.get $r)))))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $str_to_int (ref.cast (ref $STR) (local.get $r)))))",
    );
    b.push(")");
    b.push("(call $box (call $unbox (local.get $r)))");
    fs.push(Func {
        signature: "(func $py_int (param $r (ref null eq)) (result (ref null eq))".into(),
        locals: vec![],
        body: b,
    });

    // $str_to_int: parse a decimal int from a string (surrounding spaces and an
    // optional sign allowed); anything else is a ValueError. Used by int() and
    // int(input()). Wraps on i32 overflow (the compiler's int range).
    let mut b = Body::new();
    b.push("(local.set $n (array.len (local.get $s)))");
    // skip leading spaces/tabs
    b.push("(block $sl (loop $sln");
    b.push_in(2, "(br_if $sl (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(
        2,
        "(br_if $sl (i32.and (i32.ne (local.get $c) (i32.const 32)) (i32.ne (local.get $c) (i32.const 9))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $sln)))");
    // optional sign
    b.push("(local.set $sign (i32.const 1))");
    b.push("(if (i32.lt_s (local.get $i) (local.get $n))");
    b.push_in(1, "(then");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(2, "(if (i32.eq (local.get $c) (i32.const 45))");
    b.push_in(
        3,
        "(then (local.set $sign (i32.const -1)) (local.set $i (i32.add (local.get $i) (i32.const 1)))))",
    );
    b.push_in(2, "(if (i32.eq (local.get $c) (i32.const 43))");
    b.push_in(
        3,
        "(then (local.set $i (i32.add (local.get $i) (i32.const 1)))))",
    );
    b.push_in(1, "))");
    // digits
    b.push("(local.set $start (local.get $i))");
    b.push("(block $dl (loop $dln");
    b.push_in(2, "(br_if $dl (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(
        2,
        "(br_if $dl (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))",
    );
    b.push_in(
        2,
        "(local.set $acc (i32.add (i32.mul (local.get $acc) (i32.const 10)) (i32.sub (local.get $c) (i32.const 48))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $dln)))");
    // at least one digit required
    b.push("(if (i32.eq (local.get $i) (local.get $start)) (then (call $raise_int_parse (local.get $s))))");
    // skip trailing spaces/tabs
    b.push("(block $tl (loop $tln");
    b.push_in(2, "(br_if $tl (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(
        2,
        "(br_if $tl (i32.and (i32.ne (local.get $c) (i32.const 32)) (i32.ne (local.get $c) (i32.const 9))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $tln)))");
    // any leftover char is junk
    b.push("(if (i32.lt_s (local.get $i) (local.get $n)) (then (call $raise_int_parse (local.get $s))))");
    b.push("(call $box (i32.mul (local.get $sign) (local.get $acc)))");
    fs.push(Func {
        signature: "(func $str_to_int (param $s (ref null $STR)) (result (ref null eq))".into(),
        locals: vec![
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $c i32)".into(),
            "(local $sign i32)".into(),
            "(local $start i32)".into(),
            "(local $acc i32)".into(),
        ],
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
    // Instances inside containers use repr() — __repr__ only, never __str__.
    b.push("(if (ref.test (ref $OBJECT) (local.get $r))");
    b.push_in(
        1,
        "(then (call $object_display (local.get $r) (i32.const 0)) (return))",
    );
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

    // $print_tuple: `(e1, e2)` with repr elements; a 1-tuple keeps the trailing
    // comma (`(x,)`) like Python.
    let mut b = Body::new();
    b.push("(call $write_char (i32.const 40))"); // (
    b.push("(local.set $n (struct.get $TUPLE 0 (local.get $l)))");
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
        "(call $print_repr (array.get $ITEMS (struct.get $TUPLE 1 (local.get $l)) (local.get $i)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    // Singleton tuple: trailing comma.
    b.push("(if (i32.eq (local.get $n) (i32.const 1)) (then (call $write_char (i32.const 44))))");
    b.push("(call $write_char (i32.const 41))"); // )
    fs.push(Func {
        signature: "(func $print_tuple (param $l (ref null $TUPLE))".into(),
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

    // $tuple_eq: element-wise equality (recurses through $py_eq), like list_eq.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $TUPLE 0 (local.get $a)))");
    b.push("(if (i32.ne (local.get $n) (struct.get $TUPLE 0 (local.get $b)))");
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(if (i32.eqz (call $py_eq");
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $TUPLE 1 (local.get $a)) (local.get $i))",
    );
    b.push_in(
        4,
        "(array.get $ITEMS (struct.get $TUPLE 1 (local.get $b)) (local.get $i))))",
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
            "(func $tuple_eq (param $a (ref null $TUPLE)) (param $b (ref null $TUPLE)) (result i32)"
                .into(),
        locals: vec!["(local $i i32)".into(), "(local $n i32)".into()],
        body: b,
    });

    // $py_pow: `a ** b` with an integer exponent (a float/fractional exponent
    // isn't supported — unbox traps). Non-negative exponent on an int base
    // stays int (wrapping i32); a float base or negative exponent goes through
    // f64. `0 ** negative` is a ZeroDivisionError.
    let mut b = Body::new();
    b.push("(local.set $e (call $unbox (local.get $b)))");
    b.push("(if (i32.lt_s (local.get $e) (i32.const 0))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $base (call $unbox_f64 (local.get $a)))");
    b.push_in(
        2,
        "(if (f64.eq (local.get $base) (f64.const 0)) (then (call $raise_zero_div)))",
    );
    b.push_in(2, "(local.set $facc (f64.const 1))");
    b.push_in(2, "(local.set $i (i32.const 0))");
    b.push_in(2, "(block $d (loop $l");
    b.push_in(
        3,
        "(br_if $d (i32.ge_s (local.get $i) (i32.sub (i32.const 0) (local.get $e))))",
    );
    b.push_in(
        3,
        "(local.set $facc (f64.mul (local.get $facc) (local.get $base)))",
    );
    b.push_in(3, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(3, "(br $l)))");
    b.push_in(
        2,
        "(return (struct.new $FLOAT (f64.div (f64.const 1) (local.get $facc))))))",
    );
    // non-negative exponent, float base
    b.push("(if (ref.test (ref $FLOAT) (local.get $a))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $base (call $unbox_f64 (local.get $a)))");
    b.push_in(2, "(local.set $facc (f64.const 1))");
    b.push_in(2, "(local.set $i (i32.const 0))");
    b.push_in(2, "(block $d2 (loop $l2");
    b.push_in(3, "(br_if $d2 (i32.ge_s (local.get $i) (local.get $e)))");
    b.push_in(
        3,
        "(local.set $facc (f64.mul (local.get $facc) (local.get $base)))",
    );
    b.push_in(3, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(3, "(br $l2)))");
    b.push_in(2, "(return (struct.new $FLOAT (local.get $facc)))))");
    // non-negative exponent, integer base
    b.push("(local.set $acc (i32.const 1))");
    b.push("(local.set $i (i32.const 0))");
    b.push("(block $d3 (loop $l3");
    b.push_in(1, "(br_if $d3 (i32.ge_s (local.get $i) (local.get $e)))");
    b.push_in(
        1,
        "(local.set $acc (i32.mul (local.get $acc) (call $unbox (local.get $a))))",
    );
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(br $l3)))");
    b.push("(call $box (local.get $acc))");
    fs.push(Func {
        signature:
            "(func $py_pow (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $e i32)".into(),
            "(local $i i32)".into(),
            "(local $acc i32)".into(),
            "(local $facc f64)".into(),
            "(local $base f64)".into(),
        ],
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
    b.push("(if (ref.test (ref $TUPLE) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_tuple (ref.cast (ref $TUPLE) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $SET) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_set (ref.cast (ref $SET) (local.get $r)))))",
    );
    b.push(")");
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $print_dict (ref.cast (ref $DICT) (local.get $r)))))",
    );
    b.push(")");
    // $OBJECT: print via str()/__repr__ (slice 3), default `<Name object>`.
    b.push("(if (ref.test (ref $OBJECT) (local.get $r))");
    b.push_in(
        1,
        "(then (call $object_display (local.get $r) (i32.const 1)) (return))",
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

    // $py_add: Python `+` — a left operand's __add__ first, then list/string
    // concatenation when both sides match, numeric addition otherwise.
    let mut b = Body::new();
    b.push(format!(
        "(if (call $obj_has (local.get $a) {n}) (then (return (call $obj_call1 (local.get $a) (local.get $b) {n}))))",
        n = str_lit("__add__")
    ));
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

    // $py_sub / $py_mul: a left operand's dunder first, then float promotion,
    // else i32.
    for (name, f_instr, i_instr, dunder) in [
        ("$py_sub", "f64.sub", "i32.sub", "__sub__"),
        ("$py_mul", "f64.mul", "i32.mul", "__mul__"),
    ] {
        let mut b = Body::new();
        // `set - set` is set difference (mode 3); other `-` is numeric.
        if name == "$py_sub" {
            b.push("(if (i32.and (ref.test (ref $SET) (local.get $a)) (ref.test (ref $SET) (local.get $b))) (then (return (call $py_setop (local.get $a) (local.get $b) (i32.const 3)))))");
        }
        b.push(format!(
            "(if (call $obj_has (local.get $a) {n}) (then (return (call $obj_call1 (local.get $a) (local.get $b) {n}))))",
            n = str_lit(dunder)
        ));
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

    // $py_lt / $py_le / $py_gt / $py_ge: ordered comparison (raw i32 0/1). A
    // left operand's dunder first (Vector/Fraction etc.); otherwise compare as
    // f64 (exact for every i32). String comparison is rejected at compile time.
    for (name, f_instr, dunder) in [
        ("$py_lt", "f64.lt", "__lt__"),
        ("$py_le", "f64.le", "__le__"),
        ("$py_gt", "f64.gt", "__gt__"),
        ("$py_ge", "f64.ge", "__ge__"),
    ] {
        let mut b = Body::new();
        b.push(format!(
            "(if (call $obj_has (local.get $a) {n}) (then (return (call $truthy (call $obj_call1 (local.get $a) (local.get $b) {n})))))",
            n = str_lit(dunder)
        ));
        b.push(format!(
            "({f_instr} (call $unbox_f64 (local.get $a)) (call $unbox_f64 (local.get $b)))"
        ));
        fs.push(Func {
            signature: format!(
                "(func {name} (param $a (ref null eq)) (param $b (ref null eq)) (result i32)"
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
    b.push_in(1, "(then (call $raise_zero_div))");
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

    // $py_eq: Python `==` — a custom __eq__ (left, then reflected right);
    // otherwise objects compare by identity. None only equals None; strings
    // by value, string-vs-number is False; numbers (ints, bools as 1/0,
    // floats) compared as f64 (exact for i32).
    let mut b = Body::new();
    b.push(format!(
        "(if (call $obj_has (local.get $a) {n}) (then (return (call $truthy (call $obj_call1 (local.get $a) (local.get $b) {n})))))",
        n = str_lit("__eq__")
    ));
    b.push(format!(
        "(if (call $obj_has (local.get $b) {n}) (then (return (call $truthy (call $obj_call1 (local.get $b) (local.get $a) {n})))))",
        n = str_lit("__eq__")
    ));
    b.push("(if (i32.or (ref.test (ref $OBJECT) (local.get $a)) (ref.test (ref $OBJECT) (local.get $b)))");
    b.push_in(1, "(then (return (ref.eq (local.get $a) (local.get $b))))");
    b.push(")");
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
        "(if (i32.and (ref.test (ref $TUPLE) (local.get $a)) (ref.test (ref $TUPLE) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $tuple_eq (ref.cast (ref $TUPLE) (local.get $a)) (ref.cast (ref $TUPLE) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.or (ref.test (ref $TUPLE) (local.get $a)) (ref.test (ref $TUPLE) (local.get $b)))",
    );
    b.push_in(1, "(then (return (i32.const 0)))");
    b.push(")");
    b.push(
        "(if (i32.and (ref.test (ref $SET) (local.get $a)) (ref.test (ref $SET) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $set_eq (ref.cast (ref $SET) (local.get $a)) (ref.cast (ref $SET) (local.get $b)))))",
    );
    b.push(")");
    b.push(
        "(if (i32.or (ref.test (ref $SET) (local.get $a)) (ref.test (ref $SET) (local.get $b)))",
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

    // $range_list: materialize `range(start, end, step)` as a list (the value
    // form; the for-statement and comprehensions use a counted loop instead).
    let mut b = Body::new();
    b.push("(local.set $lst (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))");
    // A zero step would loop forever; yield an empty range instead of hanging.
    b.push("(if (i32.eqz (local.get $step)) (then (return (local.get $lst))))");
    b.push("(local.set $i (local.get $start))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(
        2,
        "(br_if $done (if (result i32) (i32.gt_s (local.get $step) (i32.const 0)) (then (i32.ge_s (local.get $i) (local.get $end))) (else (i32.le_s (local.get $i) (local.get $end)))))",
    );
    b.push_in(
        2,
        "(drop (call $list_append (local.get $lst) (call $box (local.get $i))))",
    );
    b.push_in(
        2,
        "(local.set $i (i32.add (local.get $i) (local.get $step)))",
    );
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.get $lst)");
    fs.push(Func {
        signature:
            "(func $range_list (param $start i32) (param $end i32) (param $step i32) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $lst (ref null eq))".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // $enumerate: list of (index, element) tuples, index counting from $start.
    let mut b = Body::new();
    b.push("(local.set $lst (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))");
    b.push("(local.set $n (call $py_len (local.get $seq)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $tup (array.new_default $ITEMS (i32.const 2)))",
    );
    b.push_in(
        2,
        "(array.set $ITEMS (local.get $tup) (i32.const 0) (call $box (i32.add (local.get $start) (local.get $i))))",
    );
    b.push_in(
        2,
        "(array.set $ITEMS (local.get $tup) (i32.const 1) (call $py_index (local.get $seq) (local.get $i)))",
    );
    b.push_in(
        2,
        "(drop (call $list_append (local.get $lst) (struct.new $TUPLE (i32.const 2) (local.get $tup))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.get $lst)");
    fs.push(Func {
        signature:
            "(func $enumerate (param $seq (ref null eq)) (param $start i32) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $lst (ref null eq))".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $tup (ref null $ITEMS))".into(),
        ],
        body: b,
    });

    // $zip2: list of (a[i], b[i]) tuples up to the shorter length.
    let mut b = Body::new();
    b.push("(local.set $lst (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))");
    b.push("(local.set $na (call $py_len (local.get $a)))");
    b.push("(local.set $nb (call $py_len (local.get $b)))");
    b.push("(local.set $n (select (local.get $na) (local.get $nb) (i32.lt_s (local.get $na) (local.get $nb))))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $tup (array.new_default $ITEMS (i32.const 2)))",
    );
    b.push_in(
        2,
        "(array.set $ITEMS (local.get $tup) (i32.const 0) (call $py_index (local.get $a) (local.get $i)))",
    );
    b.push_in(
        2,
        "(array.set $ITEMS (local.get $tup) (i32.const 1) (call $py_index (local.get $b) (local.get $i)))",
    );
    b.push_in(
        2,
        "(drop (call $list_append (local.get $lst) (struct.new $TUPLE (i32.const 2) (local.get $tup))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.get $lst)");
    fs.push(Func {
        signature:
            "(func $zip2 (param $a (ref null eq)) (param $b (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $lst (ref null eq))".into(),
            "(local $na i32)".into(),
            "(local $nb i32)".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $tup (ref null $ITEMS))".into(),
        ],
        body: b,
    });

    // $dict_view: dict.keys()/.values()/.items() (which = 0/1/2) as a list. A
    // non-dict receiver falls back to ordinary method dispatch, so a user class
    // may define methods with these names.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $DICT) (local.get $d)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $d) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $dd (ref.cast (ref $DICT) (local.get $d)))");
    b.push("(local.set $n (struct.get $DICT 0 (local.get $dd)))");
    b.push("(local.set $items (array.new_default $ITEMS (local.get $n)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(if (i32.eq (local.get $which) (i32.const 0))");
    b.push_in(
        3,
        "(then (array.set $ITEMS (local.get $items) (local.get $i) (array.get $ITEMS (struct.get $DICT 1 (local.get $dd)) (local.get $i)))))",
    );
    b.push_in(2, "(if (i32.eq (local.get $which) (i32.const 1))");
    b.push_in(
        3,
        "(then (array.set $ITEMS (local.get $items) (local.get $i) (array.get $ITEMS (struct.get $DICT 2 (local.get $dd)) (local.get $i)))))",
    );
    b.push_in(2, "(if (i32.eq (local.get $which) (i32.const 2))");
    b.push_in(3, "(then");
    b.push_in(
        4,
        "(local.set $tup (array.new_default $ITEMS (i32.const 2)))",
    );
    b.push_in(
        4,
        "(array.set $ITEMS (local.get $tup) (i32.const 0) (array.get $ITEMS (struct.get $DICT 1 (local.get $dd)) (local.get $i)))",
    );
    b.push_in(
        4,
        "(array.set $ITEMS (local.get $tup) (i32.const 1) (array.get $ITEMS (struct.get $DICT 2 (local.get $dd)) (local.get $i)))",
    );
    b.push_in(
        4,
        "(array.set $ITEMS (local.get $items) (local.get $i) (struct.new $TUPLE (i32.const 2) (local.get $tup)))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(struct.new $LIST (local.get $n) (local.get $items))");
    fs.push(Func {
        signature:
            "(func $dict_view (param $d (ref null eq)) (param $which i32) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $dd (ref null $DICT))".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $items (ref null $ITEMS))".into(),
            "(local $tup (ref null $ITEMS))".into(),
        ],
        body: b,
    });

    // $py_sum: numeric sum of an iterable, starting from 0 (via $py_add).
    let mut b = Body::new();
    b.push("(local.set $acc (call $box (i32.const 0)))");
    b.push("(local.set $n (call $py_len (local.get $seq)))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $acc (call $py_add (local.get $acc) (call $py_index (local.get $seq) (local.get $i))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.get $acc)");
    fs.push(Func {
        signature: "(func $py_sum (param $seq (ref null eq)) (result (ref null eq))".into(),
        locals: vec![
            "(local $acc (ref null eq))".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // $py_any / $py_all: truthiness reductions over an iterable. `any` returns
    // TRUE on the first truthy element (else FALSE); `all` returns FALSE on the
    // first falsy element (else TRUE).
    for (fname, any, hit, miss) in [
        ("$py_any", true, "$TRUE", "$FALSE"),
        ("$py_all", false, "$FALSE", "$TRUE"),
    ] {
        let truthy = "(call $truthy (call $py_index (local.get $seq) (local.get $i)))";
        let cond = if any {
            truthy.to_string()
        } else {
            format!("(i32.eqz {truthy})")
        };
        let mut b = Body::new();
        b.push("(local.set $n (call $py_len (local.get $seq)))");
        b.push("(block $done (loop $next");
        b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
        b.push_in(2, format!("(if {cond} (then (return (global.get {hit}))))"));
        b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
        b.push_in(2, "(br $next)))");
        b.push(format!("(global.get {miss})"));
        fs.push(Func {
            signature: format!("(func {fname} (param $seq (ref null eq)) (result (ref null eq))"),
            locals: vec!["(local $n i32)".into(), "(local $i i32)".into()],
            body: b,
        });
    }

    // $str_lt: lexicographic byte comparison, `a < b` (shorter is smaller when
    // it's a prefix). Supports sorting strings.
    let mut b = Body::new();
    b.push("(local.set $la (array.len (local.get $a)))");
    b.push("(local.set $lb (array.len (local.get $b)))");
    b.push("(local.set $m (select (local.get $la) (local.get $lb) (i32.lt_s (local.get $la) (local.get $lb))))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $m)))");
    b.push_in(
        2,
        "(local.set $ca (array.get_u $STR (local.get $a) (local.get $i)))",
    );
    b.push_in(
        2,
        "(local.set $cb (array.get_u $STR (local.get $b) (local.get $i)))",
    );
    b.push_in(
        2,
        "(if (i32.lt_u (local.get $ca) (local.get $cb)) (then (return (i32.const 1))))",
    );
    b.push_in(
        2,
        "(if (i32.gt_u (local.get $ca) (local.get $cb)) (then (return (i32.const 0))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(i32.lt_s (local.get $la) (local.get $lb))");
    fs.push(Func {
        signature:
            "(func $str_lt (param $a (ref null $STR)) (param $b (ref null $STR)) (result i32)"
                .into(),
        locals: vec![
            "(local $la i32)".into(),
            "(local $lb i32)".into(),
            "(local $m i32)".into(),
            "(local $i i32)".into(),
            "(local $ca i32)".into(),
            "(local $cb i32)".into(),
        ],
        body: b,
    });

    // $sort_lt: `a < b` for sorting — lexicographic for two strings, numeric
    // otherwise.
    let mut b = Body::new();
    b.push(
        "(if (i32.and (ref.test (ref $STR) (local.get $a)) (ref.test (ref $STR) (local.get $b)))",
    );
    b.push_in(
        1,
        "(then (return (call $str_lt (ref.cast (ref $STR) (local.get $a)) (ref.cast (ref $STR) (local.get $b))))))",
    );
    b.push("(f64.lt (call $unbox_f64 (local.get $a)) (call $unbox_f64 (local.get $b)))");
    fs.push(Func {
        signature: "(func $sort_lt (param $a (ref null eq)) (param $b (ref null eq)) (result i32)"
            .into(),
        locals: vec![],
        body: b,
    });

    // $py_sorted: a new sorted list (insertion sort; classroom-sized inputs).
    let mut b = Body::new();
    b.push("(local.set $n (call $py_len (local.get $seq)))");
    b.push("(local.set $items (array.new_default $ITEMS (local.get $n)))");
    b.push("(local.set $i (i32.const 0))");
    b.push("(block $cd (loop $cl");
    b.push_in(1, "(br_if $cd (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        1,
        "(array.set $ITEMS (local.get $items) (local.get $i) (call $py_index (local.get $seq) (local.get $i)))",
    );
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(br $cl)))");
    // insertion sort
    b.push("(local.set $i (i32.const 1))");
    b.push("(block $od (loop $ol");
    b.push_in(1, "(br_if $od (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        1,
        "(local.set $key (array.get $ITEMS (local.get $items) (local.get $i)))",
    );
    b.push_in(1, "(local.set $j (i32.sub (local.get $i) (i32.const 1)))");
    b.push_in(1, "(block $id (loop $il");
    b.push_in(2, "(br_if $id (i32.lt_s (local.get $j) (i32.const 0)))");
    b.push_in(
        2,
        "(br_if $id (i32.eqz (call $sort_lt (local.get $key) (array.get $ITEMS (local.get $items) (local.get $j)))))",
    );
    b.push_in(
        2,
        "(array.set $ITEMS (local.get $items) (i32.add (local.get $j) (i32.const 1)) (array.get $ITEMS (local.get $items) (local.get $j)))",
    );
    b.push_in(2, "(local.set $j (i32.sub (local.get $j) (i32.const 1)))");
    b.push_in(2, "(br $il)))");
    b.push_in(
        1,
        "(array.set $ITEMS (local.get $items) (i32.add (local.get $j) (i32.const 1)) (local.get $key))",
    );
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(br $ol)))");
    b.push("(struct.new $LIST (local.get $n) (local.get $items))");
    fs.push(Func {
        signature: "(func $py_sorted (param $seq (ref null eq)) (result (ref null eq))".into(),
        locals: vec![
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $j i32)".into(),
            "(local $items (ref null $ITEMS))".into(),
            "(local $key (ref null eq))".into(),
        ],
        body: b,
    });

    // $is_space: ASCII whitespace test (space, tab, newline, carriage return).
    let mut b = Body::new();
    b.push("(i32.or (i32.or (i32.eq (local.get $c) (i32.const 32)) (i32.eq (local.get $c) (i32.const 9))) (i32.or (i32.eq (local.get $c) (i32.const 10)) (i32.eq (local.get $c) (i32.const 13))))");
    fs.push(Func {
        signature: "(func $is_space (param $c i32) (result i32)".into(),
        locals: vec![],
        body: b,
    });

    // $str_sub: a fresh $STR copy of s[start .. start+len].
    let mut b = Body::new();
    b.push("(local.set $out (array.new_default $STR (local.get $len)))");
    b.push("(array.copy $STR $STR (local.get $out) (i32.const 0) (local.get $s) (local.get $start) (local.get $len))");
    b.push("(local.get $out)");
    fs.push(Func {
        signature:
            "(func $str_sub (param $s (ref null $STR)) (param $start i32) (param $len i32) (result (ref null $STR))"
                .into(),
        locals: vec!["(local $out (ref null $STR))".into()],
        body: b,
    });

    // $str_match_at: do the `sl` bytes of `sep` occur in `h` starting at `at`?
    let mut b = Body::new();
    b.push("(block $done (loop $next");
    b.push_in(2, "(br_if $done (i32.ge_s (local.get $j) (local.get $sl)))");
    b.push_in(
        2,
        "(if (i32.ne (array.get_u $STR (local.get $h) (i32.add (local.get $at) (local.get $j))) (array.get_u $STR (local.get $sep) (local.get $j))) (then (return (i32.const 0))))",
    );
    b.push_in(2, "(local.set $j (i32.add (local.get $j) (i32.const 1)))");
    b.push_in(2, "(br $next)))");
    b.push("(i32.const 1)");
    fs.push(Func {
        signature:
            "(func $str_match_at (param $h (ref null $STR)) (param $at i32) (param $sep (ref null $STR)) (param $sl i32) (result i32)"
                .into(),
        locals: vec!["(local $j i32)".into()],
        body: b,
    });

    // $str_upper / $str_lower: ASCII case shift. Non-strings fall back to method
    // dispatch (so a class may define these names).
    for (name, lo, hi, delta) in [("$str_upper", 97, 122, -32), ("$str_lower", 65, 90, 32)] {
        let mut b = Body::new();
        b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
        b.push_in(
            1,
            "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
        );
        b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
        b.push("(local.set $n (array.len (local.get $ss)))");
        b.push("(local.set $out (array.new_default $STR (local.get $n)))");
        b.push("(block $done (loop $next");
        b.push_in(2, "(br_if $done (i32.ge_s (local.get $i) (local.get $n)))");
        b.push_in(
            2,
            "(local.set $c (array.get_u $STR (local.get $ss) (local.get $i)))",
        );
        b.push_in(
            2,
            format!("(if (i32.and (i32.ge_u (local.get $c) (i32.const {lo})) (i32.le_u (local.get $c) (i32.const {hi}))) (then (local.set $c (i32.add (local.get $c) (i32.const {delta})))))"),
        );
        b.push_in(
            2,
            "(array.set $STR (local.get $out) (local.get $i) (local.get $c))",
        );
        b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
        b.push_in(2, "(br $next)))");
        b.push("(local.get $out)");
        fs.push(Func {
            signature: format!(
                "(func {name} (param $s (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
            ),
            locals: vec![
                "(local $ss (ref null $STR))".into(),
                "(local $n i32)".into(),
                "(local $i i32)".into(),
                "(local $c i32)".into(),
                "(local $out (ref null $STR))".into(),
            ],
            body: b,
        });
    }

    // $str_strip: drop leading/trailing ASCII whitespace.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(local.set $start (i32.const 0))");
    b.push("(block $ld (loop $ln");
    b.push_in(
        2,
        "(br_if $ld (i32.ge_s (local.get $start) (local.get $n)))",
    );
    b.push_in(
        2,
        "(br_if $ld (i32.eqz (call $is_space (array.get_u $STR (local.get $ss) (local.get $start)))))",
    );
    b.push_in(
        2,
        "(local.set $start (i32.add (local.get $start) (i32.const 1)))",
    );
    b.push_in(2, "(br $ln)))");
    b.push("(local.set $end (local.get $n))");
    b.push("(block $td (loop $tn");
    b.push_in(
        2,
        "(br_if $td (i32.le_s (local.get $end) (local.get $start)))",
    );
    b.push_in(
        2,
        "(br_if $td (i32.eqz (call $is_space (array.get_u $STR (local.get $ss) (i32.sub (local.get $end) (i32.const 1))))))",
    );
    b.push_in(
        2,
        "(local.set $end (i32.sub (local.get $end) (i32.const 1)))",
    );
    b.push_in(2, "(br $tn)))");
    b.push("(call $str_sub (local.get $ss) (local.get $start) (i32.sub (local.get $end) (local.get $start)))");
    fs.push(Func {
        signature:
            "(func $str_strip (param $s (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $start i32)".into(),
            "(local $end i32)".into(),
        ],
        body: b,
    });

    // $str_split: `sep` None -> split on whitespace runs (no empty tokens);
    // otherwise split on each occurrence of the separator string (Python
    // semantics, empty tokens kept).
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(local.set $lst (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))");
    b.push("(if (ref.test (ref $NONE_T) (local.get $sep))");
    b.push_in(1, "(then");
    // whitespace split
    b.push_in(2, "(block $wd (loop $wn");
    b.push_in(3, "(block $sk (loop $skn");
    b.push_in(4, "(br_if $sk (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        4,
        "(br_if $sk (i32.eqz (call $is_space (array.get_u $STR (local.get $ss) (local.get $i)))))",
    );
    b.push_in(4, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(4, "(br $skn)))");
    b.push_in(3, "(br_if $wd (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(3, "(local.set $start (local.get $i))");
    b.push_in(3, "(block $tk (loop $tkn");
    b.push_in(4, "(br_if $tk (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        4,
        "(br_if $tk (call $is_space (array.get_u $STR (local.get $ss) (local.get $i))))",
    );
    b.push_in(4, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(4, "(br $tkn)))");
    b.push_in(
        3,
        "(drop (call $list_append (local.get $lst) (call $str_sub (local.get $ss) (local.get $start) (i32.sub (local.get $i) (local.get $start)))))",
    );
    b.push_in(3, "(br $wn)))");
    b.push_in(1, ")");
    b.push_in(1, "(else");
    // separator split
    b.push_in(
        2,
        "(local.set $sepc (ref.cast (ref $STR) (local.get $sep)))",
    );
    b.push_in(2, "(local.set $sl (array.len (local.get $sepc)))");
    b.push_in(2, "(if (i32.eqz (local.get $sl))");
    b.push_in(
        3,
        "(then (drop (call $list_append (local.get $lst) (local.get $ss))) (return (local.get $lst))))",
    );
    b.push_in(2, "(local.set $start (i32.const 0))");
    b.push_in(2, "(block $pd (loop $pn");
    b.push_in(
        3,
        "(br_if $pd (i32.gt_s (i32.add (local.get $i) (local.get $sl)) (local.get $n)))",
    );
    b.push_in(
        3,
        "(if (call $str_match_at (local.get $ss) (local.get $i) (local.get $sepc) (local.get $sl))",
    );
    b.push_in(4, "(then");
    b.push_in(
        5,
        "(drop (call $list_append (local.get $lst) (call $str_sub (local.get $ss) (local.get $start) (i32.sub (local.get $i) (local.get $start)))))",
    );
    b.push_in(5, "(local.set $i (i32.add (local.get $i) (local.get $sl)))");
    b.push_in(5, "(local.set $start (local.get $i)))");
    b.push_in(
        4,
        "(else (local.set $i (i32.add (local.get $i) (i32.const 1)))))",
    );
    b.push_in(3, "(br $pn)))");
    b.push_in(
        2,
        "(drop (call $list_append (local.get $lst) (call $str_sub (local.get $ss) (local.get $start) (i32.sub (local.get $n) (local.get $start)))))",
    );
    b.push_in(1, ")");
    b.push(")");
    b.push("(local.get $lst)");
    fs.push(Func {
        signature:
            "(func $str_split (param $s (ref null eq)) (param $sep (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $start i32)".into(),
            "(local $lst (ref null eq))".into(),
            "(local $sepc (ref null $STR))".into(),
            "(local $sl i32)".into(),
        ],
        body: b,
    });

    // $str_join: join the (string) elements of an iterable with `sep`.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $sep)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $sep) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $sepc (ref.cast (ref $STR) (local.get $sep)))");
    b.push("(local.set $sl (array.len (local.get $sepc)))");
    b.push("(local.set $cnt (call $py_len (local.get $it)))");
    // total length = sum(len(elem)) + sl*(cnt-1)
    b.push("(block $ld (loop $ln");
    b.push_in(2, "(br_if $ld (i32.ge_s (local.get $i) (local.get $cnt)))");
    b.push_in(
        2,
        "(local.set $total (i32.add (local.get $total) (array.len (ref.cast (ref $STR) (call $py_index (local.get $it) (local.get $i))))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $ln)))");
    b.push(
        "(if (i32.gt_s (local.get $cnt) (i32.const 0)) (then (local.set $total (i32.add (local.get $total) (i32.mul (local.get $sl) (i32.sub (local.get $cnt) (i32.const 1)))))))",
    );
    // build
    b.push("(local.set $out (array.new_default $STR (local.get $total)))");
    b.push("(local.set $i (i32.const 0))");
    b.push("(local.set $pos (i32.const 0))");
    b.push("(block $bd (loop $bn");
    b.push_in(2, "(br_if $bd (i32.ge_s (local.get $i) (local.get $cnt)))");
    b.push_in(2, "(if (i32.gt_s (local.get $i) (i32.const 0))");
    b.push_in(3, "(then");
    b.push_in(
        4,
        "(array.copy $STR $STR (local.get $out) (local.get $pos) (local.get $sepc) (i32.const 0) (local.get $sl))",
    );
    b.push_in(
        4,
        "(local.set $pos (i32.add (local.get $pos) (local.get $sl)))))",
    );
    b.push_in(
        2,
        "(local.set $elem (ref.cast (ref $STR) (call $py_index (local.get $it) (local.get $i))))",
    );
    b.push_in(2, "(local.set $el (array.len (local.get $elem)))");
    b.push_in(
        2,
        "(array.copy $STR $STR (local.get $out) (local.get $pos) (local.get $elem) (i32.const 0) (local.get $el))",
    );
    b.push_in(
        2,
        "(local.set $pos (i32.add (local.get $pos) (local.get $el)))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $bn)))");
    b.push("(local.get $out)");
    fs.push(Func {
        signature:
            "(func $str_join (param $sep (ref null eq)) (param $it (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $sepc (ref null $STR))".into(),
            "(local $sl i32)".into(),
            "(local $cnt i32)".into(),
            "(local $i i32)".into(),
            "(local $total i32)".into(),
            "(local $out (ref null $STR))".into(),
            "(local $pos i32)".into(),
            "(local $elem (ref null $STR))".into(),
            "(local $el i32)".into(),
        ],
        body: b,
    });

    // $str_replace: replace every non-overlapping occurrence of `old` with
    // `new` (count, then build to the exact length). Empty `old` returns the
    // original (lenient).
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $oldc (ref.cast (ref $STR) (local.get $old)))");
    b.push("(local.set $newc (ref.cast (ref $STR) (local.get $new)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(local.set $ol (array.len (local.get $oldc)))");
    b.push("(local.set $nl (array.len (local.get $newc)))");
    b.push("(if (i32.eqz (local.get $ol)) (then (return (local.get $ss))))");
    b.push("(block $cd (loop $cl");
    b.push_in(
        2,
        "(br_if $cd (i32.gt_s (i32.add (local.get $i) (local.get $ol)) (local.get $n)))",
    );
    b.push_in(
        2,
        "(if (call $str_match_at (local.get $ss) (local.get $i) (local.get $oldc) (local.get $ol))",
    );
    b.push_in(
        3,
        "(then (local.set $cnt (i32.add (local.get $cnt) (i32.const 1))) (local.set $i (i32.add (local.get $i) (local.get $ol))))",
    );
    b.push_in(
        3,
        "(else (local.set $i (i32.add (local.get $i) (i32.const 1)))))",
    );
    b.push_in(2, "(br $cl)))");
    b.push(
        "(local.set $total (i32.add (local.get $n) (i32.mul (local.get $cnt) (i32.sub (local.get $nl) (local.get $ol)))))",
    );
    b.push("(local.set $out (array.new_default $STR (local.get $total)))");
    b.push("(local.set $i (i32.const 0))");
    b.push("(local.set $pos (i32.const 0))");
    b.push("(block $bd (loop $bl");
    b.push_in(2, "(br_if $bd (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(2, "(local.set $mt (i32.const 0))");
    b.push_in(
        2,
        "(if (i32.le_s (i32.add (local.get $i) (local.get $ol)) (local.get $n))",
    );
    b.push_in(
        3,
        "(then (local.set $mt (call $str_match_at (local.get $ss) (local.get $i) (local.get $oldc) (local.get $ol)))))",
    );
    b.push_in(2, "(if (local.get $mt)");
    b.push_in(3, "(then");
    b.push_in(
        4,
        "(array.copy $STR $STR (local.get $out) (local.get $pos) (local.get $newc) (i32.const 0) (local.get $nl))",
    );
    b.push_in(
        4,
        "(local.set $pos (i32.add (local.get $pos) (local.get $nl)))",
    );
    b.push_in(
        4,
        "(local.set $i (i32.add (local.get $i) (local.get $ol))))",
    );
    b.push_in(3, "(else");
    b.push_in(
        4,
        "(array.set $STR (local.get $out) (local.get $pos) (array.get_u $STR (local.get $ss) (local.get $i)))",
    );
    b.push_in(
        4,
        "(local.set $pos (i32.add (local.get $pos) (i32.const 1)))",
    );
    b.push_in(4, "(local.set $i (i32.add (local.get $i) (i32.const 1)))))");
    b.push_in(2, "(br $bl)))");
    b.push("(local.get $out)");
    fs.push(Func {
        signature:
            "(func $str_replace (param $s (ref null eq)) (param $old (ref null eq)) (param $new (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $oldc (ref null $STR))".into(),
            "(local $newc (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $ol i32)".into(),
            "(local $nl i32)".into(),
            "(local $i i32)".into(),
            "(local $cnt i32)".into(),
            "(local $total i32)".into(),
            "(local $out (ref null $STR))".into(),
            "(local $pos i32)".into(),
            "(local $mt i32)".into(),
        ],
        body: b,
    });

    // $str_starts / $str_ends: prefix / suffix test, returning a bool value.
    for (fname, at_start) in [("$str_starts", true), ("$str_ends", false)] {
        let mut b = Body::new();
        b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
        b.push_in(
            1,
            "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
        );
        b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
        b.push("(local.set $pc (ref.cast (ref $STR) (local.get $p)))");
        b.push("(local.set $n (array.len (local.get $ss)))");
        b.push("(local.set $pl (array.len (local.get $pc)))");
        b.push(
            "(if (i32.gt_s (local.get $pl) (local.get $n)) (then (return (global.get $FALSE))))",
        );
        let at = if at_start {
            "(i32.const 0)".to_string()
        } else {
            "(i32.sub (local.get $n) (local.get $pl))".to_string()
        };
        b.push(format!(
            "(call $bool (call $str_match_at (local.get $ss) {at} (local.get $pc) (local.get $pl)))"
        ));
        fs.push(Func {
            signature: format!(
                "(func {fname} (param $s (ref null eq)) (param $p (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
            ),
            locals: vec![
                "(local $ss (ref null $STR))".into(),
                "(local $pc (ref null $STR))".into(),
                "(local $n i32)".into(),
                "(local $pl i32)".into(),
            ],
            body: b,
        });
    }

    // $str_count: count non-overlapping occurrences of `sub`. $str_find:
    // index of the first occurrence, or -1. Both fall back to method dispatch
    // for a non-string receiver.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $subc (ref.cast (ref $STR) (local.get $sub)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(local.set $sl (array.len (local.get $subc)))");
    b.push("(if (i32.eqz (local.get $sl)) (then (return (call $box (i32.add (local.get $n) (i32.const 1))))))");
    b.push("(block $cd (loop $cl");
    b.push_in(
        2,
        "(br_if $cd (i32.gt_s (i32.add (local.get $i) (local.get $sl)) (local.get $n)))",
    );
    b.push_in(
        2,
        "(if (call $str_match_at (local.get $ss) (local.get $i) (local.get $subc) (local.get $sl))",
    );
    b.push_in(
        3,
        "(then (local.set $cnt (i32.add (local.get $cnt) (i32.const 1))) (local.set $i (i32.add (local.get $i) (local.get $sl))))",
    );
    b.push_in(
        3,
        "(else (local.set $i (i32.add (local.get $i) (i32.const 1)))))",
    );
    b.push_in(2, "(br $cl)))");
    b.push("(call $box (local.get $cnt))");
    fs.push(Func {
        signature:
            "(func $str_count (param $s (ref null eq)) (param $sub (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $subc (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $sl i32)".into(),
            "(local $i i32)".into(),
            "(local $cnt i32)".into(),
        ],
        body: b,
    });
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
    b.push("(local.set $subc (ref.cast (ref $STR) (local.get $sub)))");
    b.push("(local.set $n (array.len (local.get $ss)))");
    b.push("(local.set $sl (array.len (local.get $subc)))");
    b.push("(block $fd (loop $fl");
    b.push_in(
        2,
        "(br_if $fd (i32.gt_s (i32.add (local.get $i) (local.get $sl)) (local.get $n)))",
    );
    b.push_in(
        2,
        "(if (call $str_match_at (local.get $ss) (local.get $i) (local.get $subc) (local.get $sl)) (then (return (call $box (local.get $i)))))",
    );
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $fl)))");
    b.push("(call $box (i32.const -1))");
    fs.push(Func {
        signature:
            "(func $str_find (param $s (ref null eq)) (param $sub (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $ss (ref null $STR))".into(),
            "(local $subc (ref null $STR))".into(),
            "(local $n i32)".into(),
            "(local $sl i32)".into(),
            "(local $i i32)".into(),
        ],
        body: b,
    });

    // $str_isdigit / $str_isalpha: non-empty and every char in the class.
    for (fname, lo1, hi1, lo2, hi2) in [
        ("$str_isdigit", 48, 57, 48, 57),
        ("$str_isalpha", 65, 90, 97, 122),
    ] {
        let mut b = Body::new();
        b.push("(if (i32.eqz (ref.test (ref $STR) (local.get $s)))");
        b.push_in(
            1,
            "(then (return (call $call_method (local.get $s) (local.get $name) (local.get $args)))))",
        );
        b.push("(local.set $ss (ref.cast (ref $STR) (local.get $s)))");
        b.push("(local.set $n (array.len (local.get $ss)))");
        b.push("(if (i32.eqz (local.get $n)) (then (return (global.get $FALSE))))");
        b.push("(block $d (loop $l");
        b.push_in(2, "(br_if $d (i32.ge_s (local.get $i) (local.get $n)))");
        b.push_in(
            2,
            "(local.set $c (array.get_u $STR (local.get $ss) (local.get $i)))",
        );
        b.push_in(
            2,
            format!("(if (i32.eqz (i32.or (i32.and (i32.ge_u (local.get $c) (i32.const {lo1})) (i32.le_u (local.get $c) (i32.const {hi1}))) (i32.and (i32.ge_u (local.get $c) (i32.const {lo2})) (i32.le_u (local.get $c) (i32.const {hi2}))))) (then (return (global.get $FALSE))))"),
        );
        b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
        b.push_in(2, "(br $l)))");
        b.push("(global.get $TRUE)");
        fs.push(Func {
            signature: format!(
                "(func {fname} (param $s (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
            ),
            locals: vec![
                "(local $ss (ref null $STR))".into(),
                "(local $n i32)".into(),
                "(local $i i32)".into(),
                "(local $c i32)".into(),
            ],
            body: b,
        });
    }

    // $dict_remove_at / $list_remove_at: drop the entry at `idx`, shifting the
    // tail down (array.copy handles the overlap) and decrementing the count.
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $DICT 0 (local.get $d)))");
    b.push(
        "(array.copy $ITEMS $ITEMS (struct.get $DICT 1 (local.get $d)) (local.get $idx) (struct.get $DICT 1 (local.get $d)) (i32.add (local.get $idx) (i32.const 1)) (i32.sub (i32.sub (local.get $n) (local.get $idx)) (i32.const 1)))",
    );
    b.push(
        "(array.copy $ITEMS $ITEMS (struct.get $DICT 2 (local.get $d)) (local.get $idx) (struct.get $DICT 2 (local.get $d)) (i32.add (local.get $idx) (i32.const 1)) (i32.sub (i32.sub (local.get $n) (local.get $idx)) (i32.const 1)))",
    );
    b.push("(struct.set $DICT 0 (local.get $d) (i32.sub (local.get $n) (i32.const 1)))");
    fs.push(Func {
        signature: "(func $dict_remove_at (param $d (ref null $DICT)) (param $idx i32)".into(),
        locals: vec!["(local $n i32)".into()],
        body: b,
    });
    let mut b = Body::new();
    b.push("(local.set $n (struct.get $LIST 0 (local.get $l)))");
    b.push(
        "(array.copy $ITEMS $ITEMS (struct.get $LIST 1 (local.get $l)) (local.get $idx) (struct.get $LIST 1 (local.get $l)) (i32.add (local.get $idx) (i32.const 1)) (i32.sub (i32.sub (local.get $n) (local.get $idx)) (i32.const 1)))",
    );
    b.push("(struct.set $LIST 0 (local.get $l) (i32.sub (local.get $n) (i32.const 1)))");
    fs.push(Func {
        signature: "(func $list_remove_at (param $l (ref null $LIST)) (param $idx i32)".into(),
        locals: vec!["(local $n i32)".into()],
        body: b,
    });

    // $dict_get: dict.get(key[, default]) -> value or default (never raises).
    // Non-dict receiver falls back to method dispatch.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $DICT) (local.get $d)))");
    b.push_in(
        1,
        "(then (return (call $call_method (local.get $d) (local.get $name) (local.get $args)))))",
    );
    b.push("(local.set $dd (ref.cast (ref $DICT) (local.get $d)))");
    b.push("(local.set $idx (call $dict_find (local.get $dd) (local.get $key)))");
    b.push(
        "(if (i32.ge_s (local.get $idx) (i32.const 0)) (then (return (array.get $ITEMS (struct.get $DICT 2 (local.get $dd)) (local.get $idx)))))",
    );
    b.push("(local.get $default)");
    fs.push(Func {
        signature:
            "(func $dict_get_default (param $d (ref null eq)) (param $key (ref null eq)) (param $default (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $dd (ref null $DICT))".into(),
            "(local $idx i32)".into(),
        ],
        body: b,
    });

    // $py_pop: dict.pop(key[, default]) and list.pop([index]). `nargs` is the
    // Python argument count. Non-dict/non-list falls back to method dispatch.
    let mut b = Body::new();
    b.push("(if (ref.test (ref $DICT) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $dd (ref.cast (ref $DICT) (local.get $r)))");
    b.push_in(
        2,
        "(if (i32.lt_s (local.get $nargs) (i32.const 1)) (then (return (call $call_method (local.get $r) (local.get $name) (local.get $args)))))",
    );
    b.push_in(
        2,
        "(local.set $idx (call $dict_find (local.get $dd) (local.get $arg)))",
    );
    b.push_in(2, "(if (i32.ge_s (local.get $idx) (i32.const 0))");
    b.push_in(3, "(then");
    b.push_in(
        4,
        "(local.set $val (array.get $ITEMS (struct.get $DICT 2 (local.get $dd)) (local.get $idx)))",
    );
    b.push_in(4, "(call $dict_remove_at (local.get $dd) (local.get $idx))");
    b.push_in(4, "(return (local.get $val))))");
    b.push_in(
        2,
        "(if (i32.ge_s (local.get $nargs) (i32.const 2)) (then (return (local.get $default))))",
    );
    b.push_in(2, "(call $raise_key (local.get $arg))");
    b.push_in(2, "(unreachable)))");
    b.push("(if (ref.test (ref $LIST) (local.get $r))");
    b.push_in(1, "(then");
    b.push_in(2, "(local.set $ll (ref.cast (ref $LIST) (local.get $r)))");
    b.push_in(2, "(local.set $n (struct.get $LIST 0 (local.get $ll)))");
    b.push_in(
        2,
        "(local.set $li (if (result i32) (i32.ge_s (local.get $nargs) (i32.const 1)) (then (call $unbox (local.get $arg))) (else (i32.sub (local.get $n) (i32.const 1)))))",
    );
    b.push_in(
        2,
        "(if (i32.lt_s (local.get $li) (i32.const 0)) (then (local.set $li (i32.add (local.get $li) (local.get $n)))))",
    );
    b.push_in(
        2,
        "(if (i32.or (i32.lt_s (local.get $li) (i32.const 0)) (i32.ge_s (local.get $li) (local.get $n))) (then (call $raise_index (local.get $r)) (unreachable)))",
    );
    b.push_in(
        2,
        "(local.set $val (array.get $ITEMS (struct.get $LIST 1 (local.get $ll)) (local.get $li)))",
    );
    b.push_in(2, "(call $list_remove_at (local.get $ll) (local.get $li))");
    b.push_in(2, "(return (local.get $val))))");
    b.push("(call $call_method (local.get $r) (local.get $name) (local.get $args))");
    fs.push(Func {
        signature:
            "(func $py_pop (param $r (ref null eq)) (param $arg (ref null eq)) (param $default (ref null eq)) (param $nargs i32) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $dd (ref null $DICT))".into(),
            "(local $ll (ref null $LIST))".into(),
            "(local $idx i32)".into(),
            "(local $li i32)".into(),
            "(local $n i32)".into(),
            "(local $val (ref null eq))".into(),
        ],
        body: b,
    });

    // $str_to_float / $py_float: float(x) — parse a string (sign, digits, an
    // optional fraction, an optional exponent) or convert a number to f64.
    let mut b = Body::new();
    b.push("(local.set $n (array.len (local.get $s)))");
    b.push("(local.set $sign (f64.const 1))");
    b.push("(local.set $acc (f64.const 0))");
    // leading spaces
    b.push("(block $l1 (loop $l1n (br_if $l1 (i32.ge_s (local.get $i) (local.get $n))) (br_if $l1 (i32.eqz (call $is_space (array.get_u $STR (local.get $s) (local.get $i))))) (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l1n)))");
    // sign
    b.push("(if (i32.lt_s (local.get $i) (local.get $n)) (then");
    b.push_in(
        1,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(1, "(if (i32.eq (local.get $c) (i32.const 45)) (then (local.set $sign (f64.const -1)) (local.set $i (i32.add (local.get $i) (i32.const 1)))))");
    b.push_in(1, "(if (i32.eq (local.get $c) (i32.const 43)) (then (local.set $i (i32.add (local.get $i) (i32.const 1)))))");
    b.push("))");
    // integer part
    b.push("(block $l2 (loop $l2n");
    b.push_in(1, "(br_if $l2 (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        1,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(1, "(br_if $l2 (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))");
    b.push_in(1, "(local.set $acc (f64.add (f64.mul (local.get $acc) (f64.const 10)) (f64.convert_i32_s (i32.sub (local.get $c) (i32.const 48)))))");
    b.push_in(1, "(local.set $seen (i32.const 1))");
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(br $l2n)))");
    // fraction (peek without reading past the end)
    b.push("(local.set $c (if (result i32) (i32.lt_s (local.get $i) (local.get $n)) (then (array.get_u $STR (local.get $s) (local.get $i))) (else (i32.const 0))))");
    b.push("(if (i32.eq (local.get $c) (i32.const 46)) (then");
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(local.set $scale (f64.const 0.1))");
    b.push_in(1, "(block $l3 (loop $l3n");
    b.push_in(2, "(br_if $l3 (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(2, "(br_if $l3 (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))");
    b.push_in(2, "(local.set $acc (f64.add (local.get $acc) (f64.mul (f64.convert_i32_s (i32.sub (local.get $c) (i32.const 48))) (local.get $scale))))");
    b.push_in(
        2,
        "(local.set $scale (f64.mul (local.get $scale) (f64.const 0.1)))",
    );
    b.push_in(2, "(local.set $seen (i32.const 1))");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $l3n)))");
    b.push("))");
    b.push("(if (i32.eqz (local.get $seen)) (then (call $raise_float_parse (local.get $s)) (unreachable)))");
    // exponent (peek without reading past the end)
    b.push("(local.set $esign (i32.const 1))");
    b.push("(local.set $c (if (result i32) (i32.lt_s (local.get $i) (local.get $n)) (then (array.get_u $STR (local.get $s) (local.get $i))) (else (i32.const 0))))");
    b.push("(if (i32.or (i32.eq (local.get $c) (i32.const 101)) (i32.eq (local.get $c) (i32.const 69))) (then");
    b.push_in(1, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(1, "(if (i32.lt_s (local.get $i) (local.get $n)) (then");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(2, "(if (i32.eq (local.get $c) (i32.const 45)) (then (local.set $esign (i32.const -1)) (local.set $i (i32.add (local.get $i) (i32.const 1)))))");
    b.push_in(2, "(if (i32.eq (local.get $c) (i32.const 43)) (then (local.set $i (i32.add (local.get $i) (i32.const 1)))))");
    b.push_in(1, "))");
    b.push_in(1, "(local.set $edig (i32.const 0))");
    b.push_in(1, "(block $l4 (loop $l4n");
    b.push_in(2, "(br_if $l4 (i32.ge_s (local.get $i) (local.get $n)))");
    b.push_in(
        2,
        "(local.set $c (array.get_u $STR (local.get $s) (local.get $i)))",
    );
    b.push_in(2, "(br_if $l4 (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))");
    b.push_in(2, "(local.set $exp (i32.add (i32.mul (local.get $exp) (i32.const 10)) (i32.sub (local.get $c) (i32.const 48))))");
    b.push_in(2, "(local.set $edig (i32.const 1))");
    b.push_in(2, "(local.set $i (i32.add (local.get $i) (i32.const 1)))");
    b.push_in(2, "(br $l4n)))");
    b.push_in(1, "(if (i32.eqz (local.get $edig)) (then (call $raise_float_parse (local.get $s)) (unreachable)))");
    b.push_in(1, "(block $l5 (loop $l5n");
    b.push_in(2, "(br_if $l5 (i32.le_s (local.get $exp) (i32.const 0)))");
    b.push_in(2, "(if (i32.gt_s (local.get $esign) (i32.const 0)) (then (local.set $acc (f64.mul (local.get $acc) (f64.const 10)))) (else (local.set $acc (f64.mul (local.get $acc) (f64.const 0.1)))))");
    b.push_in(
        2,
        "(local.set $exp (i32.sub (local.get $exp) (i32.const 1)))",
    );
    b.push_in(2, "(br $l5n)))");
    b.push("))");
    // trailing spaces
    b.push("(block $l6 (loop $l6n (br_if $l6 (i32.ge_s (local.get $i) (local.get $n))) (br_if $l6 (i32.eqz (call $is_space (array.get_u $STR (local.get $s) (local.get $i))))) (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l6n)))");
    b.push("(if (i32.lt_s (local.get $i) (local.get $n)) (then (call $raise_float_parse (local.get $s)) (unreachable)))");
    b.push("(struct.new $FLOAT (f64.mul (local.get $sign) (local.get $acc)))");
    fs.push(Func {
        signature: "(func $str_to_float (param $s (ref null $STR)) (result (ref null eq))".into(),
        locals: vec![
            "(local $n i32)".into(),
            "(local $i i32)".into(),
            "(local $c i32)".into(),
            "(local $sign f64)".into(),
            "(local $acc f64)".into(),
            "(local $scale f64)".into(),
            "(local $seen i32)".into(),
            "(local $exp i32)".into(),
            "(local $esign i32)".into(),
            "(local $edig i32)".into(),
        ],
        body: b,
    });
    let mut b = Body::new();
    b.push("(if (ref.test (ref $STR) (local.get $r))");
    b.push_in(
        1,
        "(then (return (call $str_to_float (ref.cast (ref $STR) (local.get $r))))))",
    );
    b.push("(struct.new $FLOAT (call $unbox_f64 (local.get $r)))");
    fs.push(Func {
        signature: "(func $py_float (param $r (ref null eq)) (result (ref null eq))".into(),
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
        "(if (f64.eq (local.get $fb) (f64.const 0)) (then (call $raise_zero_div)))",
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
        "(if (f64.eq (local.get $fb) (f64.const 0)) (then (call $raise_zero_div)))",
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

/// The class runtime: method resolution up the inheritance chain, dynamic
/// method dispatch via `call_ref`, instance construction, and attribute
/// get/set (reusing `$DICT`). Always emitted (cheap; unused if no classes).
fn class_helpers() -> Vec<Func> {
    let mut fs = Vec::new();

    // $class_lookup_method: walk class -> base, return the $METHOD (eqref) for
    // `name`, or null. (Base chain handles single inheritance.)
    let mut b = Body::new();
    b.push("(local.set $c (local.get $class))");
    b.push("(block $done");
    b.push_in(1, "(loop $walk");
    b.push_in(2, "(br_if $done (ref.is_null (local.get $c)))");
    b.push_in(
        2,
        "(local.set $methods (struct.get $CLASS 1 (local.get $c)))",
    );
    b.push_in(
        2,
        "(local.set $idx (call $dict_find (local.get $methods) (local.get $name)))",
    );
    b.push_in(2, "(if (i32.ge_s (local.get $idx) (i32.const 0))");
    b.push_in(
        3,
        "(then (return (array.get $ITEMS (struct.get $DICT 2 (local.get $methods)) (local.get $idx)))))",
    );
    b.push_in(2, "(local.set $c (struct.get $CLASS 2 (local.get $c)))");
    b.push_in(2, "(br $walk)");
    b.push_in(1, ")");
    b.push(")");
    b.push("(ref.null eq)");
    fs.push(Func {
        signature:
            "(func $class_lookup_method (param $class (ref null $CLASS)) (param $name (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $c (ref null $CLASS))".into(),
            "(local $idx i32)".into(),
            "(local $methods (ref null $DICT))".into(),
        ],
        body: b,
    });

    // $dispatch_from: look up `name` starting at a given class (not the
    // object's own), prepend self, call via call_ref. This is how
    // `super().m(...)` dispatches — from the enclosing class's base. Missing
    // method -> AttributeError.
    let mut b = Body::new();
    b.push("(local.set $m (call $class_lookup_method (local.get $class) (local.get $name)))");
    b.push("(if (i32.eqz (ref.test (ref $METHOD) (local.get $m)))");
    b.push_in(
        1,
        "(then (call $raise_no_attr (local.get $obj) (local.get $name))))",
    );
    b.push(
        "(call_ref $MFUNC (local.get $obj) (local.get $args) (struct.get $METHOD 0 (ref.cast (ref $METHOD) (local.get $m))))",
    );
    fs.push(Func {
        signature:
            "(func $dispatch_from (param $obj (ref null eq)) (param $class (ref null $CLASS)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec!["(local $m (ref null eq))".into()],
        body: b,
    });

    // $call_method: dynamic dispatch — resolve from the object's own class.
    // Missing method (non-object receiver included) -> AttributeError.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $OBJECT) (local.get $obj)))");
    b.push_in(
        1,
        "(then (call $raise_no_attr (local.get $obj) (local.get $name))))",
    );
    b.push(
        "(call $dispatch_from (local.get $obj) (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name) (local.get $args))",
    );
    fs.push(Func {
        signature:
            "(func $call_method (param $obj (ref null eq)) (param $name (ref null eq)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });

    // $instantiate: Cls(args) -> new instance with empty attrs, then __init__
    // (if defined) with self prepended.
    let mut b = Body::new();
    b.push(
        "(local.set $obj (struct.new $OBJECT (ref.cast (ref $CLASS) (local.get $class)) (struct.new $DICT (i32.const 0) (array.new_fixed $ITEMS 0) (array.new_fixed $ITEMS 0))))",
    );
    b.push(format!(
        "(local.set $init (call $class_lookup_method (local.get $class) {}))",
        str_lit("__init__")
    ));
    b.push("(if (ref.test (ref $METHOD) (local.get $init))");
    b.push_in(
        1,
        "(then (drop (call_ref $MFUNC (local.get $obj) (local.get $args) (struct.get $METHOD 0 (ref.cast (ref $METHOD) (local.get $init)))))))",
    );
    b.push("(local.get $obj)");
    fs.push(Func {
        signature:
            "(func $instantiate (param $class (ref null $CLASS)) (param $args (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $obj (ref null eq))".into(),
            "(local $init (ref null eq))".into(),
        ],
        body: b,
    });

    // $obj_getattr: attribute read — the instance dict first, then the class
    // chain (class variables). A class entry that is a $METHOD would be a bound
    // method, which v1 can't yield as a value -> clean error.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $OBJECT) (local.get $obj)))");
    b.push_in(
        1,
        "(then (call $raise_no_attr (local.get $obj) (local.get $name))))",
    );
    b.push("(local.set $attrs (struct.get $OBJECT 1 (ref.cast (ref $OBJECT) (local.get $obj))))");
    b.push("(local.set $idx (call $dict_find (local.get $attrs) (local.get $name)))");
    b.push("(if (i32.ge_s (local.get $idx) (i32.const 0))");
    b.push_in(
        1,
        "(then (return (array.get $ITEMS (struct.get $DICT 2 (local.get $attrs)) (local.get $idx)))))",
    );
    // Fall back to the class namespace (class variables shared by instances).
    b.push(
        "(local.set $m (call $class_lookup_method (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name)))",
    );
    b.push("(if (ref.is_null (local.get $m))");
    b.push_in(
        1,
        "(then (call $raise_no_attr (local.get $obj) (local.get $name))))",
    );
    b.push("(if (ref.test (ref $METHOD) (local.get $m))");
    b.push_in(
        1,
        "(then (call $raise_method_value (local.get $obj) (local.get $name))))",
    );
    b.push("(local.get $m)");
    fs.push(Func {
        signature:
            "(func $obj_getattr (param $obj (ref null eq)) (param $name (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![
            "(local $idx i32)".into(),
            "(local $attrs (ref null $DICT))".into(),
            "(local $m (ref null eq))".into(),
        ],
        body: b,
    });

    // $object_display: print an instance via its string form ($object_to_str
    // builds it — __str__/__repr__ result or the `<Name object>` default).
    let mut b = Body::new();
    b.push(
        "(call $print_str (ref.cast (ref null $STR) (call $object_to_str (local.get $obj) (local.get $prefer_str))))",
    );
    fs.push(Func {
        signature: "(func $object_display (param $obj (ref null eq)) (param $prefer_str i32)"
            .into(),
        locals: vec![],
        body: b,
    });

    // $obj_has: 1 if `obj` is an instance whose class chain defines `name`.
    // Non-objects answer 0 — so operator helpers can probe for a dunder
    // (`__add__`, `__eq__`, …) without first knowing the operand's type.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $OBJECT) (local.get $obj)))");
    b.push_in(1, "(then (return (i32.const 0))))");
    b.push(
        "(ref.test (ref $METHOD) (call $class_lookup_method (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name)))",
    );
    fs.push(Func {
        signature:
            "(func $obj_has (param $obj (ref null eq)) (param $name (ref null eq)) (result i32)"
                .into(),
        locals: vec![],
        body: b,
    });

    // $obj_call1 / $obj_call0: dispatch a method on `obj`'s own class with one
    // / zero arguments (operator dunders). Callers gate with $obj_has, so the
    // method is known to exist.
    let mut b = Body::new();
    b.push(
        "(call $dispatch_from (local.get $obj) (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name) (struct.new $LIST (i32.const 1) (array.new_fixed $ITEMS 1 (local.get $arg))))",
    );
    fs.push(Func {
        signature:
            "(func $obj_call1 (param $obj (ref null eq)) (param $arg (ref null eq)) (param $name (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });
    let mut b = Body::new();
    b.push(
        "(call $dispatch_from (local.get $obj) (struct.get $OBJECT 0 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name) (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))",
    );
    fs.push(Func {
        signature:
            "(func $obj_call0 (param $obj (ref null eq)) (param $name (ref null eq)) (result (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });

    // $obj_setattr: instance attribute write.
    let mut b = Body::new();
    b.push("(if (i32.eqz (ref.test (ref $OBJECT) (local.get $obj)))");
    b.push_in(
        1,
        "(then (call $raise_no_attr (local.get $obj) (local.get $name))))",
    );
    b.push(
        "(call $dict_set (struct.get $OBJECT 1 (ref.cast (ref $OBJECT) (local.get $obj))) (local.get $name) (local.get $val))",
    );
    fs.push(Func {
        signature:
            "(func $obj_setattr (param $obj (ref null eq)) (param $name (ref null eq)) (param $val (ref null eq))"
                .into(),
        locals: vec![],
        body: b,
    });

    fs
}

/// `$read_line`: read bytes from the `env.read_char` host import until a
/// newline or EOF (-1), returning a `$STR` with the newline stripped (a stray
/// `\r` is dropped, so `\r\n` input works). Emitted only when `input()` is used.
fn read_line_helper() -> Func {
    let mut b = Body::new();
    b.push("(local.set $cap (i32.const 16))");
    b.push("(local.set $buf (array.new_default $STR (local.get $cap)))");
    b.push("(local.set $len (i32.const 0))");
    b.push("(block $done");
    b.push_in(1, "(loop $next");
    b.push_in(2, "(local.set $c (call $read_char))");
    b.push_in(2, "(br_if $done (i32.lt_s (local.get $c) (i32.const 0)))"); // EOF
    b.push_in(2, "(br_if $done (i32.eq (local.get $c) (i32.const 10)))"); // \n
    b.push_in(2, "(if (i32.ne (local.get $c) (i32.const 13))"); // skip \r
    b.push_in(3, "(then");
    // grow if needed (double capacity, copy)
    b.push_in(4, "(if (i32.ge_s (local.get $len) (local.get $cap))");
    b.push_in(5, "(then");
    b.push_in(
        6,
        "(local.set $cap (i32.mul (local.get $cap) (i32.const 2)))",
    );
    b.push_in(
        6,
        "(local.set $new (array.new_default $STR (local.get $cap)))",
    );
    b.push_in(
        6,
        "(array.copy $STR $STR (local.get $new) (i32.const 0) (local.get $buf) (i32.const 0) (local.get $len))",
    );
    b.push_in(6, "(local.set $buf (local.get $new))))");
    b.push_in(
        4,
        "(array.set $STR (local.get $buf) (local.get $len) (local.get $c))",
    );
    b.push_in(
        4,
        "(local.set $len (i32.add (local.get $len) (i32.const 1)))))",
    );
    b.push_in(2, "(br $next)");
    b.push_in(1, ")");
    b.push(")");
    // Trim to exactly $len.
    b.push("(local.set $out (array.new_default $STR (local.get $len)))");
    b.push("(array.copy $STR $STR (local.get $out) (i32.const 0) (local.get $buf) (i32.const 0) (local.get $len))");
    b.push("(local.get $out)");
    Func {
        signature: "(func $read_line (result (ref null eq))".into(),
        locals: vec![
            "(local $cap i32)".into(),
            "(local $len i32)".into(),
            "(local $c i32)".into(),
            "(local $buf (ref null $STR))".into(),
            "(local $new (ref null $STR))".into(),
            "(local $out (ref null $STR))".into(),
        ],
        body: b,
    }
}

/// Python floor division: truncating `i32.div_s` adjusted by -1 when the
/// signs differ and the division isn't exact (`-7 // 2` is -4, not -3).
fn floordiv_helper() -> Func {
    let mut b = Body::new();
    b.push("(if (i32.eqz (local.get $b)) (then (call $raise_zero_div)))");
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
    b.push("(if (i32.eqz (local.get $b)) (then (call $raise_zero_div)))");
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
    /// Set when `input()` is used, so the `env.read_char` import and the
    /// `$read_line` helper are only emitted (and only required of the host)
    /// when a program actually reads input.
    uses_input: bool,
    /// User functions: name -> total parameter count (collected before any
    /// body compiles).
    funcs: HashMap<String, usize>,
    /// Default-value expressions for the trailing parameters of each function
    /// (evaluated at the call site to fill omitted arguments).
    func_defaults: HashMap<String, Vec<Expr>>,
    /// Parameter names of each function, for binding keyword arguments.
    func_params: HashMap<String, Vec<String>>,
    /// User classes: name -> base class name (None if no base). Collected in
    /// pass 1 so `Cls(args)` construction is distinguished from a function call.
    classes: HashMap<String, Option<String>>,
    /// Top-level Python variables, in definition order — WASM globals named
    /// `$g_<name>` so function bodies can read them.
    globals: Vec<String>,
    /// Imported module names (only `math` is supported), so `math.sqrt(...)`
    /// and `math.pi` resolve to built-in operations.
    imported: std::collections::HashSet<String>,
}

impl Gen {
    /// Whether `e` is a bare reference to an imported module (not shadowed by a
    /// variable), e.g. the `math` in `math.sqrt(x)`.
    fn is_module_ref(&self, cx: &FuncCx, e: &Expr) -> bool {
        if let ExprKind::Name(m) = &e.kind {
            self.imported.contains(m) && !cx.vars.contains_key(m) && !self.is_global(m)
        } else {
            false
        }
    }

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
    /// Inside a method: the enclosing class name and the `self` parameter
    /// name, so `super().m(...)` can resolve from the class's base with the
    /// real `self`. `None` at top level and in plain functions.
    current_class: Option<String>,
    self_name: Option<String>,
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
            // A comprehension variable is removed from `vars` (scoping) but its
            // local declaration stays, so guard against re-declaring it.
            if !self.locals.iter().any(|(n, _)| n == name) {
                self.locals.push((name.to_string(), VAL.to_string()));
            }
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
            StmtKind::ClassDef { .. } => Err(CompileError::at(
                s.line,
                "classes can only be defined at the top level (not inside \
                 another function, loop, or if)",
            )),
            StmtKind::SetAttr { obj, attr, value } => {
                self.type_of(cx, value)?;
                let o = self.value_expr(cx, obj)?;
                let v = self.value_expr(cx, value)?;
                out.push(format!("(call $obj_setattr {o} {} {v})", str_lit(attr)));
                Ok(())
            }
            // Imports are recorded in pass 1; nothing to emit here.
            StmtKind::Import(_) => Ok(()),
            StmtKind::UnpackAssign { targets, value } => {
                self.type_of(cx, value)?;
                let v = self.value_expr(cx, value)?;
                let tmp = cx.scratch_local(VAL);
                out.push(format!("(local.set ${tmp} {v})"));
                let n = targets.len();
                // Length must match exactly (Python's unpack semantics).
                out.push(format!(
                    "(if (i32.ne (call $py_len (local.get ${tmp})) (i32.const {n})) (then (call $raise_unpack)))"
                ));
                for (i, target) in targets.iter().enumerate() {
                    let elem = format!("(call $py_index (local.get ${tmp}) (i32.const {i}))");
                    self.assign_target(cx, target, &elem, out)?;
                }
                Ok(())
            }
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

    /// `math.<fn>(args)`. sqrt/fabs return a float; floor/ceil/trunc return an
    /// int (Python's behavior). WASM has native f64 ops for all of these.
    fn gen_math_call(
        &mut self,
        cx: &mut FuncCx,
        method: &str,
        args: &[Expr],
        line: usize,
    ) -> Result<String> {
        if args.len() != 1 {
            return Err(CompileError::at(
                line,
                format!("math.{method}() takes one argument"),
            ));
        }
        let x = self.value_expr(cx, &args[0])?;
        let float = |op: &str| format!("(struct.new $FLOAT ({op} (call $unbox_f64 {x})))");
        let to_int =
            |op: &str| format!("(call $box (i32.trunc_sat_f64_s ({op} (call $unbox_f64 {x}))))");
        match method {
            "sqrt" => Ok(float("f64.sqrt")),
            "fabs" => Ok(float("f64.abs")),
            "floor" => Ok(to_int("f64.floor")),
            "ceil" => Ok(to_int("f64.ceil")),
            "trunc" => Ok(to_int("f64.trunc")),
            _ => Err(CompileError::at(
                line,
                format!("math has no function '{method}'"),
            )),
        }
    }

    /// Build an `$LIST` from argument expressions (the method-call args
    /// container; same shape as a list literal).
    fn list_of(&mut self, cx: &mut FuncCx, args: &[Expr]) -> Result<String> {
        let mut items = String::new();
        for a in args {
            items.push(' ');
            items.push_str(&self.value_expr(cx, a)?);
        }
        let n = args.len();
        Ok(format!(
            "(struct.new $LIST (i32.const {n}) (array.new_fixed $ITEMS {n}{items}))"
        ))
    }

    /// Compile one method into a uniform-signature `$MFUNC` function. The
    /// instance comes in as the first WASM param (named after the Python
    /// `self`-parameter); the remaining declared params are unpacked from the
    /// `$.args` list in the prologue. Dispatched via `call_ref`.
    fn gen_method(&mut self, class: &str, m: &crate::ast::Method, line: usize) -> Result<Func> {
        if m.params.is_empty() {
            return Err(CompileError::at(
                line,
                format!("method '{}' needs at least a 'self' parameter", m.name),
            ));
        }
        let self_name = &m.params[0];
        let rest = &m.params[1..];

        let mut cx = FuncCx {
            current_class: Some(class.to_string()),
            self_name: Some(self_name.clone()),
            ..Default::default()
        };
        for p in &m.params {
            cx.vars.insert(p.clone(), Ty::Value);
        }
        // Non-self params are locals unpacked from the args list.
        for p in rest {
            cx.locals.push((p.clone(), VAL.to_string()));
        }
        let mut assigned = std::collections::HashSet::new();
        collect_assigned(&m.body, &mut assigned);
        let mut local_names: Vec<&String> = assigned.iter().collect();
        local_names.sort();
        for a in local_names {
            if !cx.vars.contains_key(a) {
                cx.ensure_local(a);
            }
        }

        let mut b = Body::new();
        // Arity check: the args list must hold exactly the non-self params.
        b.push(format!(
            "(if (i32.ne (call $py_len (local.get $.args)) (i32.const {})) (then (call $raise_arity)))",
            rest.len()
        ));
        for (i, p) in rest.iter().enumerate() {
            b.push(format!(
                "(local.set ${p} (call $py_index (local.get $.args) (i32.const {i})))"
            ));
        }
        self.stmts(&mut cx, &m.body, &mut b)?;
        b.push("(global.get $NONE)");

        Ok(Func {
            signature: format!(
                "(func $m_{class}_{} (type $MFUNC) (param ${self_name} (ref null eq)) (param $.args (ref null eq)) (result (ref null eq))",
                m.name
            ),
            locals: cx
                .locals
                .iter()
                .map(|(n, ty)| format!("(local ${n} {ty})"))
                .collect(),
            body: b,
        })
    }

    /// Emit the runtime construction of one class's `$CLASS` global into the
    /// `_start` prologue: build the class namespace (`$DICT` holding methods as
    /// `$METHOD` and class variables as their values) then `global.set`. Runs
    /// in source order, so a base class (built earlier) is available when a
    /// subclass references it. Class-variable initializers are evaluated here,
    /// in the top-level scope, before the program's other top-level statements.
    fn gen_class_init(
        &mut self,
        cx: &mut FuncCx,
        name: &str,
        base: &Option<String>,
        methods: &[crate::ast::Method],
        class_vars: &[(String, Expr)],
        out: &mut Body,
    ) -> Result<()> {
        out.push(
            "(local.set $.cd (struct.new $DICT (i32.const 0) (array.new_fixed $ITEMS 0) (array.new_fixed $ITEMS 0)))",
        );
        for m in methods {
            out.push(format!(
                "(call $dict_set (local.get $.cd) {} (struct.new $METHOD (ref.func $m_{name}_{})))",
                str_lit(&m.name),
                m.name
            ));
        }
        for (var, expr) in class_vars {
            self.type_of(cx, expr)?;
            let value = self.value_expr(cx, expr)?;
            out.push(format!(
                "(call $dict_set (local.get $.cd) {} {value})",
                str_lit(var)
            ));
        }
        let base_ref = match base {
            Some(b) => format!("(global.get $g_class_{b})"),
            None => "(ref.null $CLASS)".to_string(),
        };
        out.push(format!(
            "(global.set $g_class_{name} (struct.new $CLASS {} (local.get $.cd) {base_ref}))",
            str_lit(name)
        ));
        Ok(())
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

    /// Emit an assignment of an already-built value expression to one target
    /// (a `Name`, `Index`, or `Attr`). Used by tuple unpacking, where each
    /// target receives one element of the right-hand side.
    fn assign_target(
        &mut self,
        cx: &mut FuncCx,
        target: &Expr,
        value_wat: &str,
        out: &mut Body,
    ) -> Result<()> {
        match &target.kind {
            ExprKind::Name(name) => {
                if cx.is_top {
                    self.ensure_global(name);
                    out.push(format!("(global.set $g_{name} {value_wat})"));
                } else {
                    out.push(format!("(local.set ${name} {value_wat})"));
                }
            }
            ExprKind::Index(t, idx) => {
                let tw = self.value_expr(cx, t)?;
                let kw = self.value_expr(cx, idx)?;
                out.push(format!("(call $py_set_subscript {tw} {kw} {value_wat})"));
            }
            ExprKind::Attr(obj, attr) => {
                let ow = self.value_expr(cx, obj)?;
                out.push(format!(
                    "(call $obj_setattr {ow} {} {value_wat})",
                    str_lit(attr)
                ));
            }
            _ => return Err(CompileError::at(target.line, "invalid unpacking target")),
        }
        Ok(())
    }

    /// Recursively emit a comprehension's clauses into a `Body`. The innermost
    /// level appends to (list) or inserts into (dict) the accumulator `acc`;
    /// `key` is `Some` for dict comprehensions, `elem` is the element/value.
    ///
    /// A `for` target is a function-scoped local restored afterward so a fresh
    /// name doesn't leak (matching Python 3 for new names; a name that shadows
    /// an outer *local* still shares that local — a documented divergence).
    fn comp_loop(
        &mut self,
        cx: &mut FuncCx,
        clauses: &[CompClause],
        acc: &str,
        key: Option<&Expr>,
        elem: &Expr,
    ) -> Result<Body> {
        let Some((clause, rest)) = clauses.split_first() else {
            let mut b = Body::new();
            match key {
                Some(k) => {
                    let kw = self.value_expr(cx, k)?;
                    let vw = self.value_expr(cx, elem)?;
                    b.push(format!("(call $dict_set (local.get ${acc}) {kw} {vw})"));
                }
                None => {
                    let ew = self.value_expr(cx, elem)?;
                    b.push(format!(
                        "(drop (call $list_append (local.get ${acc}) {ew}))"
                    ));
                }
            }
            return Ok(b);
        };
        match clause {
            CompClause::If(cond) => {
                let c = self.cond_i32(cx, cond)?;
                let inner = self.comp_loop(cx, rest, acc, key, elem)?;
                let mut b = Body::new();
                b.push(format!("(if {c}"));
                b.push_in(1, "(then");
                b.append(inner, 2);
                b.push_in(1, "))");
                Ok(b)
            }
            CompClause::For { vars, iter } => {
                // Target names are function-scoped; save and restore so they
                // don't leak past the comprehension.
                let prev: Vec<(String, Option<Ty>)> = vars
                    .iter()
                    .map(|v| (v.clone(), cx.vars.get(v).copied()))
                    .collect();
                for v in vars {
                    cx.ensure_local(v);
                }
                let result = self.comp_for_clause(cx, vars, iter, rest, acc, key, elem);
                for (v, t) in prev {
                    match t {
                        Some(t) => {
                            cx.vars.insert(v, t);
                        }
                        None => {
                            cx.vars.remove(&v);
                        }
                    }
                }
                result
            }
        }
    }

    /// A `for` clause: bind one hidden loop variable, then (for a tuple target)
    /// unpack it into the named targets before the rest of the comprehension.
    #[allow(clippy::too_many_arguments)]
    fn comp_for_clause(
        &mut self,
        cx: &mut FuncCx,
        vars: &[String],
        iter: &Expr,
        rest: &[CompClause],
        acc: &str,
        key: Option<&Expr>,
        elem: &Expr,
    ) -> Result<Body> {
        let single = vars.len() == 1;
        let loopvar = if single {
            vars[0].clone()
        } else {
            cx.scratch_local(VAL)
        };
        let mut inner = self.comp_loop(cx, rest, acc, key, elem)?;
        if !single {
            let mut unpacked = Body::new();
            unpacked.push(format!(
                "(if (i32.ne (call $py_len (local.get ${loopvar})) (i32.const {})) (then (call $raise_unpack)))",
                vars.len()
            ));
            for (i, v) in vars.iter().enumerate() {
                unpacked.push(format!(
                    "(local.set ${v} (call $py_index (local.get ${loopvar}) (i32.const {i})))"
                ));
            }
            unpacked.append(inner, 0);
            inner = unpacked;
        }
        // `range(...)` over a single target is a counted i32 loop.
        if single {
            if let ExprKind::Call(name, args) = &iter.kind {
                if name == "range"
                    && (1..=3).contains(&args.len())
                    && !self.funcs.contains_key("range")
                {
                    return self.comp_range_for(cx, &loopvar, args, inner, iter.line);
                }
            }
        }
        self.comp_for(cx, &loopvar, iter, inner)
    }

    /// Build a sequence-iterating comprehension loop around `inner`, binding
    /// `var` to each element via $py_index.
    fn comp_for(&mut self, cx: &mut FuncCx, var: &str, iter: &Expr, inner: Body) -> Result<Body> {
        self.type_of(cx, iter)?;
        let it_wat = self.value_expr(cx, iter)?;
        let it = cx.scratch_local(VAL);
        let idx = cx.scratch_local("i32");
        let n = cx.fresh();
        let mut b = Body::new();
        b.push(format!("(local.set ${it} {it_wat})"));
        b.push(format!("(local.set ${idx} (i32.const 0))"));
        b.push(format!("(block $b{n}"));
        b.push_in(1, format!("(loop $l{n}"));
        b.push_in(
            2,
            format!("(br_if $b{n} (i32.ge_s (local.get ${idx}) (call $py_len (local.get ${it}))))"),
        );
        b.push_in(
            2,
            format!("(local.set ${var} (call $py_index (local.get ${it}) (local.get ${idx})))"),
        );
        b.append(inner, 2);
        b.push_in(
            2,
            format!("(local.set ${idx} (i32.add (local.get ${idx}) (i32.const 1)))"),
        );
        b.push_in(2, format!("(br $l{n})"));
        b.push_in(1, ")");
        b.push(")");
        Ok(b)
    }

    /// Build a counted `range(...)` comprehension loop around `inner`, binding
    /// `var` to the boxed counter (the for-statement's range fast path).
    fn comp_range_for(
        &mut self,
        cx: &mut FuncCx,
        var: &str,
        args: &[Expr],
        inner: Body,
        line: usize,
    ) -> Result<Body> {
        let (start_wat, end_expr, step_v): (String, &Expr, i32) = match args.len() {
            1 => ("(i32.const 0)".to_string(), &args[0], 1),
            2 => (self.i32_expr(cx, &args[0])?, &args[1], 1),
            _ => {
                let sv = const_int(&args[2]).ok_or_else(|| {
                    CompileError::at(line, "the range() step must be a plain number")
                })?;
                if sv == 0 {
                    return Err(CompileError::at(line, "range() step can't be zero"));
                }
                let sv = i32::try_from(sv)
                    .map_err(|_| CompileError::at(line, "range() step is too big"))?;
                (self.i32_expr(cx, &args[0])?, &args[1], sv)
            }
        };
        if const_float(end_expr).is_some() {
            return Err(CompileError::at(
                line,
                "range() needs whole numbers, not decimals",
            ));
        }
        let end_wat = self.i32_expr(cx, end_expr)?;
        let ctr = cx.scratch_local("i32");
        let endloc = cx.scratch_local("i32");
        let n = cx.fresh();
        let done_cmp = if step_v > 0 { "i32.ge_s" } else { "i32.le_s" };
        let mut b = Body::new();
        b.push(format!("(local.set ${endloc} {end_wat})"));
        b.push(format!("(local.set ${ctr} {start_wat})"));
        b.push(format!("(block $b{n}"));
        b.push_in(1, format!("(loop $l{n}"));
        b.push_in(
            2,
            format!("(br_if $b{n} ({done_cmp} (local.get ${ctr}) (local.get ${endloc})))"),
        );
        b.push_in(
            2,
            format!("(local.set ${var} (call $box (local.get ${ctr})))"),
        );
        b.append(inner, 2);
        b.push_in(
            2,
            format!("(local.set ${ctr} (i32.add (local.get ${ctr}) (i32.const {step_v})))"),
        );
        b.push_in(2, format!("(br $l{n})"));
        b.push_in(1, ")");
        b.push(")");
        Ok(b)
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
            ExprKind::Bin(BinOp::In, a, b) => {
                let item = self.value_expr(cx, a)?;
                let cont = self.value_expr(cx, b)?;
                Ok(format!("(call $bool (call $py_in {item} {cont}))"))
            }
            ExprKind::Bin(BinOp::NotIn, a, b) => {
                let item = self.value_expr(cx, a)?;
                let cont = self.value_expr(cx, b)?;
                Ok(format!(
                    "(call $bool (i32.eqz (call $py_in {item} {cont})))"
                ))
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
            ExprKind::Bin(BinOp::Pow, a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call $py_pow {lhs} {rhs})"))
            }
            // Set operators: union / intersection / symmetric difference
            // (modes 0/1/2). Both operands must be sets.
            ExprKind::Bin(op @ (BinOp::BitOr | BinOp::BitXor | BinOp::BitAnd), a, b) => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let mode = match op {
                    BinOp::BitOr => 0,
                    BinOp::BitAnd => 1,
                    _ => 2,
                };
                Ok(format!("(call $py_setop {lhs} {rhs} (i32.const {mode}))"))
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
                // Boxed operands so a custom dunder can run; the helper falls
                // to an f64 compare (exact for every i32) for plain numbers.
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                let helper = cmp_helper(*op).expect("handled above");
                Ok(format!("(call $bool (call {helper} {lhs} {rhs}))"))
            }
            ExprKind::Str(s) => Ok(str_lit(s)),
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
            ExprKind::Tuple(elems) => {
                let mut items = String::new();
                for el in elems {
                    items.push(' ');
                    items.push_str(&self.value_expr(cx, el)?);
                }
                let n = elems.len();
                if n == 0 {
                    Ok("(struct.new $TUPLE (i32.const 0) (array.new_fixed $ITEMS 0))".to_string())
                } else {
                    Ok(format!(
                        "(struct.new $TUPLE (i32.const {n}) (array.new_fixed $ITEMS {n}{items}))"
                    ))
                }
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
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                let o = self.value_expr(cx, obj)?;
                let none = "(global.get $NONE)".to_string();
                let s = match start {
                    Some(e) => self.value_expr(cx, e)?,
                    None => none.clone(),
                };
                let st = match stop {
                    Some(e) => self.value_expr(cx, e)?,
                    None => none.clone(),
                };
                let sp = match step {
                    Some(e) => self.value_expr(cx, e)?,
                    None => none,
                };
                Ok(format!("(call $py_slice {o} {s} {st} {sp})"))
            }
            ExprKind::Attr(obj, name) => {
                // `math.pi` / `math.e` / `math.tau` — module constants.
                if self.is_module_ref(cx, obj) {
                    return match name.as_str() {
                        "pi" => Ok("(struct.new $FLOAT (f64.const 3.141592653589793))".into()),
                        "e" => Ok("(struct.new $FLOAT (f64.const 2.718281828459045))".into()),
                        "tau" => Ok("(struct.new $FLOAT (f64.const 6.283185307179586))".into()),
                        _ => Err(CompileError::at(
                            e.line,
                            format!("math has no attribute '{name}'"),
                        )),
                    };
                }
                let o = self.value_expr(cx, obj)?;
                Ok(format!("(call $obj_getattr {o} {})", str_lit(name)))
            }
            ExprKind::ListComp { element, clauses } => {
                let acc = cx.scratch_local(VAL);
                let inner = self.comp_loop(cx, clauses, &acc, None, element)?;
                let mut body = Body::new();
                body.push(format!(
                    "(local.set ${acc} (struct.new $LIST (i32.const 0) (array.new_fixed $ITEMS 0)))"
                ));
                body.append(inner, 0);
                let mut s = String::new();
                body.render(0, &mut s);
                Ok(format!(
                    "(block (result (ref null eq))\n{s}(local.get ${acc}))"
                ))
            }
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => {
                let acc = cx.scratch_local(VAL);
                let inner = self.comp_loop(cx, clauses, &acc, Some(key), value)?;
                let mut body = Body::new();
                body.push(format!(
                    "(local.set ${acc} (struct.new $DICT (i32.const 0) (array.new_fixed $ITEMS 0) (array.new_fixed $ITEMS 0)))"
                ));
                body.append(inner, 0);
                let mut s = String::new();
                body.render(0, &mut s);
                Ok(format!(
                    "(block (result (ref null eq))\n{s}(local.get ${acc}))"
                ))
            }
            ExprKind::MethodCall(recv, method, args) => {
                if args.iter().any(|a| matches!(a.kind, ExprKind::Kwarg(..))) {
                    return Err(CompileError::at(
                        e.line,
                        "keyword arguments aren't supported in method calls yet",
                    ));
                }
                // `super().m(args)` dispatches from the enclosing class's base
                // with the current `self` (resolved at compile time — v1 has
                // no first-class super).
                if let ExprKind::Call(callee, super_args) = &recv.kind {
                    if callee == "super" {
                        if !super_args.is_empty() {
                            return Err(CompileError::at(
                                e.line,
                                "super() takes no arguments in this subset",
                            ));
                        }
                        let class = cx.current_class.as_ref().ok_or_else(|| {
                            CompileError::at(e.line, "super() can only be used inside a method")
                        })?;
                        let base = self.classes.get(class).cloned().flatten().ok_or_else(|| {
                            CompileError::at(
                                e.line,
                                format!("super() needs a base class, but '{class}' has none"),
                            )
                        })?;
                        let self_name = cx.self_name.clone().expect("method has self");
                        let args_list = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $dispatch_from (local.get ${self_name}) (global.get $g_class_{base}) {} {args_list})",
                            str_lit(method)
                        ));
                    }
                }
                // `math.fn(...)` — a module function, not a value method.
                if self.is_module_ref(cx, recv) {
                    return self.gen_math_call(cx, method, args, e.line);
                }
                // String methods. Each helper falls back to method dispatch for
                // a non-string receiver, so a class may reuse these names.
                match method.as_str() {
                    "upper" | "lower" | "strip" if args.is_empty() => {
                        let r = self.value_expr(cx, recv)?;
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $str_{method} {r} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    "split" if args.len() <= 1 => {
                        let r = self.value_expr(cx, recv)?;
                        let sep = if args.len() == 1 {
                            self.value_expr(cx, &args[0])?
                        } else {
                            "(global.get $NONE)".to_string()
                        };
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $str_split {r} {sep} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    "join" if args.len() == 1 => {
                        let r = self.value_expr(cx, recv)?;
                        let it = self.value_expr(cx, &args[0])?;
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $str_join {r} {it} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    "count" | "find" if args.len() == 1 => {
                        let r = self.value_expr(cx, recv)?;
                        let sub = self.value_expr(cx, &args[0])?;
                        let argl = self.list_of(cx, args)?;
                        let helper = if method == "count" {
                            "$str_count"
                        } else {
                            "$str_find"
                        };
                        return Ok(format!(
                            "(call {helper} {r} {sub} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    // set methods: add / discard / remove.
                    "add" | "discard" | "remove" if args.len() == 1 => {
                        let r = self.value_expr(cx, recv)?;
                        let v = self.value_expr(cx, &args[0])?;
                        let argl = self.list_of(cx, args)?;
                        let helper = match method.as_str() {
                            "add" => "$set_add",
                            "discard" => "$set_discard",
                            _ => "$set_remove",
                        };
                        return Ok(format!(
                            "(call {helper} {r} {v} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    "isdigit" | "isalpha" if args.is_empty() => {
                        let r = self.value_expr(cx, recv)?;
                        let argl = self.list_of(cx, args)?;
                        let helper = if method == "isdigit" {
                            "$str_isdigit"
                        } else {
                            "$str_isalpha"
                        };
                        return Ok(format!("(call {helper} {r} {} {argl})", str_lit(method)));
                    }
                    "replace" if args.len() == 2 => {
                        let r = self.value_expr(cx, recv)?;
                        let old = self.value_expr(cx, &args[0])?;
                        let new = self.value_expr(cx, &args[1])?;
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $str_replace {r} {old} {new} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    "startswith" | "endswith" if args.len() == 1 => {
                        let r = self.value_expr(cx, recv)?;
                        let p = self.value_expr(cx, &args[0])?;
                        let argl = self.list_of(cx, args)?;
                        let helper = if method == "startswith" {
                            "$str_starts"
                        } else {
                            "$str_ends"
                        };
                        return Ok(format!(
                            "(call {helper} {r} {p} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    // dict.get(key[, default]) — never raises.
                    "get" if (1..=2).contains(&args.len()) => {
                        let r = self.value_expr(cx, recv)?;
                        let key = self.value_expr(cx, &args[0])?;
                        let default = if args.len() == 2 {
                            self.value_expr(cx, &args[1])?
                        } else {
                            "(global.get $NONE)".to_string()
                        };
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $dict_get_default {r} {key} {default} {} {argl})",
                            str_lit(method)
                        ));
                    }
                    // dict.pop(key[, default]) and list.pop([index]).
                    "pop" if args.len() <= 2 => {
                        let r = self.value_expr(cx, recv)?;
                        let arg = if !args.is_empty() {
                            self.value_expr(cx, &args[0])?
                        } else {
                            "(global.get $NONE)".to_string()
                        };
                        let default = if args.len() == 2 {
                            self.value_expr(cx, &args[1])?
                        } else {
                            "(global.get $NONE)".to_string()
                        };
                        let argl = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $py_pop {r} {arg} {default} (i32.const {}) {} {argl})",
                            args.len(),
                            str_lit(method)
                        ));
                    }
                    _ => {}
                }
                // dict.keys()/.values()/.items() — $dict_view falls back to
                // method dispatch for a non-dict, so a class may reuse the names.
                if args.is_empty() {
                    if let Some(which) = match method.as_str() {
                        "keys" => Some(0),
                        "values" => Some(1),
                        "items" => Some(2),
                        _ => None,
                    } {
                        let r = self.value_expr(cx, recv)?;
                        let empty = self.list_of(cx, args)?;
                        return Ok(format!(
                            "(call $dict_view {r} (i32.const {which}) {} {empty})",
                            str_lit(method)
                        ));
                    }
                }
                // `.append(v)` is the list fast path; every other method is a
                // dynamic object dispatch.
                if method == "append" && args.len() == 1 {
                    let r = self.value_expr(cx, recv)?;
                    let v = self.value_expr(cx, &args[0])?;
                    Ok(format!("(call $list_append {r} {v})"))
                } else {
                    let r = self.value_expr(cx, recv)?;
                    let args_list = self.list_of(cx, args)?;
                    Ok(format!(
                        "(call $call_method {r} {} {args_list})",
                        str_lit(method)
                    ))
                }
            }
            ExprKind::Kwarg(..) => Err(CompileError::at(
                e.line,
                "keyword arguments can only appear in a function call",
            )),
            ExprKind::Call(n, args) => {
                if n == "print" {
                    return Err(CompileError::at(
                        e.line,
                        "print(...) can't be used inside an expression",
                    ));
                }
                if n == "super" {
                    return Err(CompileError::at(
                        e.line,
                        "super() is only supported as super().method(...) in this subset",
                    ));
                }
                // Keyword arguments are bound only for user functions (below).
                if args.iter().any(|a| matches!(a.kind, ExprKind::Kwarg(..)))
                    && !self.funcs.contains_key(n.as_str())
                {
                    return Err(CompileError::at(
                        e.line,
                        format!("keyword arguments aren't supported for '{n}'"),
                    ));
                }
                // Class construction: `Cls(args)` builds an instance and runs
                // __init__ (which validates its own arity at runtime).
                if self.classes.contains_key(n.as_str()) {
                    let args_list = self.list_of(cx, args)?;
                    return Ok(format!(
                        "(call $instantiate (global.get $g_class_{n}) {args_list})"
                    ));
                }
                // Builtins with custom arity/codegen (user defs shadow them).
                if !self.funcs.contains_key(n.as_str()) {
                    match n.as_str() {
                        // `range(...)` as a value materializes a list (the for
                        // statement and comprehensions use a counted loop).
                        "range" => {
                            let one = "(i32.const 1)".to_string();
                            let zero = "(i32.const 0)".to_string();
                            let (start, end, step) = match args.len() {
                                1 => (zero, self.i32_expr(cx, &args[0])?, one),
                                2 => (
                                    self.i32_expr(cx, &args[0])?,
                                    self.i32_expr(cx, &args[1])?,
                                    one,
                                ),
                                3 => (
                                    self.i32_expr(cx, &args[0])?,
                                    self.i32_expr(cx, &args[1])?,
                                    self.i32_expr(cx, &args[2])?,
                                ),
                                k => {
                                    return Err(CompileError::at(
                                        e.line,
                                        format!("range() takes 1 to 3 arguments, got {k}"),
                                    ))
                                }
                            };
                            return Ok(format!("(call $range_list {start} {end} {step})"));
                        }
                        "enumerate" if (1..=2).contains(&args.len()) => {
                            let seq = self.value_expr(cx, &args[0])?;
                            let start = if args.len() == 2 {
                                self.i32_expr(cx, &args[1])?
                            } else {
                                "(i32.const 0)".to_string()
                            };
                            return Ok(format!("(call $enumerate {seq} {start})"));
                        }
                        "zip" if args.len() == 2 => {
                            let a = self.value_expr(cx, &args[0])?;
                            let b = self.value_expr(cx, &args[1])?;
                            return Ok(format!("(call $zip2 {a} {b})"));
                        }
                        "sum" if args.len() == 1 => {
                            let seq = self.value_expr(cx, &args[0])?;
                            return Ok(format!("(call $py_sum {seq})"));
                        }
                        "sorted" if args.len() == 1 => {
                            let seq = self.value_expr(cx, &args[0])?;
                            return Ok(format!("(call $py_sorted {seq})"));
                        }
                        "any" if args.len() == 1 => {
                            let seq = self.value_expr(cx, &args[0])?;
                            return Ok(format!("(call $py_any {seq})"));
                        }
                        "all" if args.len() == 1 => {
                            let seq = self.value_expr(cx, &args[0])?;
                            return Ok(format!("(call $py_all {seq})"));
                        }
                        // set() / set(iterable) (also the desugar target of set
                        // literals and set comprehensions).
                        "set" if args.len() <= 1 => {
                            let it = if args.len() == 1 {
                                self.value_expr(cx, &args[0])?
                            } else {
                                "(ref.null eq)".to_string()
                            };
                            return Ok(format!("(call $py_set {it})"));
                        }
                        // format(value, "spec") — spec must be a string literal
                        // (always is, from an f-string `{x:spec}`).
                        "format" if args.len() == 2 => {
                            let v = self.value_expr(cx, &args[0])?;
                            let ExprKind::Str(spec) = &args[1].kind else {
                                return Err(CompileError::at(
                                    e.line,
                                    "the format spec must be a string literal",
                                ));
                            };
                            let (is_float, prec, width, fill, align) =
                                parse_format_spec(spec).map_err(|m| CompileError::at(e.line, m))?;
                            if is_float {
                                let base = format!(
                                    "(call $f64_to_fixed (call $unbox_f64 {v}) (i32.const {prec}))"
                                );
                                if width == 0 {
                                    return Ok(base);
                                }
                                return Ok(format!(
                                    "(call $str_pad {base} (i32.const {width}) (i32.const {fill}) (i32.const {align}))"
                                ));
                            }
                            if width == 0 {
                                return Ok(format!("(call $to_str {v})"));
                            }
                            if align == 3 {
                                // Auto-align: numbers right, everything else
                                // left — decided at runtime from the value type.
                                let t = cx.scratch_local(VAL);
                                let base = format!("(call $to_str (local.tee ${t} {v}))");
                                let numeric = format!(
                                    "(i32.or (i32.or (ref.test (ref i31) (local.get ${t})) (ref.test (ref $INT) (local.get ${t}))) (i32.or (ref.test (ref $BOOL) (local.get ${t})) (ref.test (ref $FLOAT) (local.get ${t}))))"
                                );
                                return Ok(format!(
                                    "(call $str_pad {base} (i32.const {width}) (i32.const {fill}) (if (result i32) {numeric} (then (i32.const 1)) (else (i32.const 0))))"
                                ));
                            }
                            let base = format!("(call $to_str {v})");
                            return Ok(format!(
                                "(call $str_pad {base} (i32.const {width}) (i32.const {fill}) (i32.const {align}))"
                            ));
                        }
                        // min/max: one iterable, or several positional args.
                        "min" | "max" if !args.is_empty() => {
                            let seq = if args.len() == 1 {
                                self.value_expr(cx, &args[0])?
                            } else {
                                self.list_of(cx, args)?
                            };
                            let h = if n == "min" { "$py_min" } else { "$py_max" };
                            return Ok(format!("(call {h} {seq})"));
                        }
                        "round" if (1..=2).contains(&args.len()) => {
                            let x = self.value_expr(cx, &args[0])?;
                            if args.len() == 2 {
                                let nd = self.i32_expr(cx, &args[1])?;
                                return Ok(format!("(call $py_round2 {x} {nd})"));
                            }
                            return Ok(format!("(call $py_round1 {x})"));
                        }
                        // input([prompt]) -> a line from stdin (newline
                        // stripped); a prompt is printed first.
                        "input" if args.len() <= 1 => {
                            self.uses_input = true;
                            if args.len() == 1 {
                                let p = self.value_expr(cx, &args[0])?;
                                return Ok(format!(
                                    "(block (result (ref null eq)) (call $print_str (ref.cast (ref null $STR) (call $to_str {p}))) (call $read_line))"
                                ));
                            }
                            return Ok("(call $read_line)".to_string());
                        }
                        _ => {}
                    }
                }
                // User functions first (a `def len` shadows the builtin,
                // like Python); then the fixed-arity builtins.
                if !self.funcs.contains_key(n.as_str()) {
                    let builtin = match n.as_str() {
                        "len" => Some(("$py_len", 1, true)), // returns raw i32
                        "str" => Some(("$to_str", 1, false)),
                        "repr" => Some(("$repr_str", 1, false)),
                        "float" => Some(("$py_float", 1, false)),
                        "abs" => Some(("$py_abs", 1, false)),
                        "int" => Some(("$py_int", 1, false)),
                        "bool" => Some(("$py_bool", 1, false)),
                        _ => None,
                    };
                    if let Some((helper, arity, boxed_i32)) = builtin {
                        if args.len() != arity {
                            return Err(CompileError::at(
                                e.line,
                                format!(
                                    "{n}() takes exactly {arity} argument{}",
                                    if arity == 1 { "" } else { "s" }
                                ),
                            ));
                        }
                        let mut wat = format!("(call {helper}");
                        for a in args {
                            wat.push(' ');
                            wat.push_str(&self.value_expr(cx, a)?);
                        }
                        wat.push(')');
                        return Ok(if boxed_i32 {
                            format!("(call $box {wat})")
                        } else {
                            wat
                        });
                    }
                }
                let Some(&total) = self.funcs.get(n) else {
                    return Err(CompileError::at(e.line, format!("unknown function '{n}'")));
                };
                // Clone params/defaults so we can evaluate at the call site
                // without holding a borrow on self.
                let params = self.func_params.get(n).cloned().unwrap_or_default();
                let defaults = self.func_defaults.get(n).cloned().unwrap_or_default();
                let required = total - defaults.len();

                // Bind each parameter slot from positional args, then keyword
                // args, then defaults (in parameter order for the WASM call).
                let mut slots: Vec<Option<Expr>> = vec![None; total];
                let mut npos = 0usize;
                for a in args {
                    if let ExprKind::Kwarg(k, v) = &a.kind {
                        let Some(j) = params.iter().position(|p| p == k) else {
                            return Err(CompileError::at(
                                e.line,
                                format!("{n}() got an unexpected keyword argument '{k}'"),
                            ));
                        };
                        if slots[j].is_some() {
                            return Err(CompileError::at(
                                e.line,
                                format!("{n}() got multiple values for argument '{k}'"),
                            ));
                        }
                        slots[j] = Some((**v).clone());
                    } else {
                        if npos >= total {
                            return Err(CompileError::at(
                                e.line,
                                format!("{n}() got too many positional arguments"),
                            ));
                        }
                        slots[npos] = Some(a.clone());
                        npos += 1;
                    }
                }

                let mut wat = format!("(call $f_{n}");
                for (j, slot) in slots.into_iter().enumerate() {
                    let bound = match slot {
                        Some(ex) => ex,
                        None if j >= required => defaults[j - required].clone(),
                        None => {
                            return Err(CompileError::at(
                                e.line,
                                format!(
                                    "{n}() is missing a required argument '{}'",
                                    params.get(j).map(|s| s.as_str()).unwrap_or("?")
                                ),
                            ))
                        }
                    };
                    wat.push(' ');
                    wat.push_str(&self.value_expr(cx, &bound)?);
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
            ExprKind::Bin(BinOp::In, a, b) => {
                let item = self.value_expr(cx, a)?;
                let cont = self.value_expr(cx, b)?;
                Ok(format!("(call $py_in {item} {cont})"))
            }
            ExprKind::Bin(BinOp::NotIn, a, b) => {
                let item = self.value_expr(cx, a)?;
                let cont = self.value_expr(cx, b)?;
                Ok(format!("(i32.eqz (call $py_in {item} {cont}))"))
            }
            ExprKind::Bin(op, a, b) if cmp_helper(*op).is_some() => {
                let lhs = self.value_expr(cx, a)?;
                let rhs = self.value_expr(cx, b)?;
                Ok(format!("(call {} {lhs} {rhs})", cmp_helper(*op).unwrap()))
            }
            _ => Ok(format!("(call $truthy {})", self.value_expr(cx, e)?)),
        }
    }
}

/// The runtime helper for an ordered comparison (object-aware), or `None`
/// for operators handled elsewhere.
fn cmp_helper(op: BinOp) -> Option<&'static str> {
    Some(match op {
        BinOp::Lt => "$py_lt",
        BinOp::Le => "$py_le",
        BinOp::Gt => "$py_gt",
        BinOp::Ge => "$py_ge",
        _ => return None,
    })
}

fn emit_char(out: &mut Body, byte: u8) {
    out.push(format!("(call $write_char (i32.const {byte}))"));
}

/// Parse a (subset of the) Python format mini-language into
/// `(is_float, precision, width, fill_char_code, align)` where align is
/// 0=left, 1=right, 2=center. Supports `[[fill]align][0][width][.prec][type]`
/// with type in `f`/`d`/`s` (or none).
fn parse_format_spec(spec: &str) -> std::result::Result<(bool, i32, i32, i32, i32), String> {
    let cs: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let is_align = |c: char| matches!(c, '<' | '>' | '^');
    let mut fill = ' ';
    let mut align: Option<char> = None;
    if cs.len() >= 2 && is_align(cs[1]) {
        fill = cs[0];
        align = Some(cs[1]);
        i = 2;
    } else if !cs.is_empty() && is_align(cs[0]) {
        align = Some(cs[0]);
        i = 1;
    }
    // `0` zero-pad (right-aligns numbers, fills with '0').
    if i < cs.len() && cs[i] == '0' {
        if align.is_none() {
            align = Some('>');
            fill = '0';
        }
        i += 1;
    }
    let mut width = 0i32;
    while i < cs.len() && cs[i].is_ascii_digit() {
        width = width * 10 + (cs[i] as i32 - '0' as i32);
        i += 1;
    }
    let mut prec: Option<i32> = None;
    if i < cs.len() && cs[i] == '.' {
        i += 1;
        let start = i;
        let mut p = 0i32;
        while i < cs.len() && cs[i].is_ascii_digit() {
            p = p * 10 + (cs[i] as i32 - '0' as i32);
            i += 1;
        }
        if i == start {
            return Err("format precision needs a number after '.'".into());
        }
        prec = Some(p);
    }
    let mut ty = '\0';
    if i < cs.len() {
        ty = cs[i];
        i += 1;
    }
    if i != cs.len() {
        return Err(format!("unsupported format spec '{spec}'"));
    }
    if !matches!(ty, '\0' | 'f' | 'd' | 's') {
        return Err(format!("unsupported format type '{ty}' in '{spec}'"));
    }
    let is_float = ty == 'f' || (prec.is_some() && ty != 'd' && ty != 's');
    let prec_v = prec.unwrap_or(if ty == 'f' { 6 } else { 0 });
    let align_code = match align {
        Some('<') => 0,
        Some('^') => 2,
        Some('>') => 1,
        // Default: floats/`d` right-align; `s` left; a bare width with no type
        // is resolved at runtime by the value's type (code 3 = auto).
        _ if ty == 'd' || is_float => 1,
        _ if ty == 's' => 0,
        _ => 3,
    };
    Ok((is_float, prec_v, width, fill as i32, align_code))
}

/// WAT for a `$STR` literal from Rust bytes (used for attribute/method-name
/// keys as well as string literals).
fn str_lit(s: &str) -> String {
    let bytes: Vec<String> = s.bytes().map(|b| format!("(i32.const {b})")).collect();
    format!("(array.new_fixed $STR {} {})", bytes.len(), bytes.join(" "))
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
            StmtKind::UnpackAssign { targets, .. } => {
                for t in targets {
                    if let ExprKind::Name(n) = &t.kind {
                        out.insert(n.clone());
                    }
                }
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
                    // Equality, membership, and/or, and set operators accept any
                    // mix (the result isn't a plain number).
                    BinOp::Eq
                    | BinOp::Ne
                    | BinOp::And
                    | BinOp::Or
                    | BinOp::In
                    | BinOp::NotIn
                    | BinOp::BitOr
                    | BinOp::BitAnd
                    | BinOp::BitXor => Ok(Ty::Value),
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
            ExprKind::Kwarg(_, v) => {
                self.type_of(cx, v)?;
                Ok(Ty::Value)
            }
            ExprKind::List(elems) | ExprKind::Tuple(elems) => {
                for el in elems {
                    self.type_of(cx, el)?;
                }
                Ok(Ty::Value)
            }
            ExprKind::Attr(obj, _) => {
                // A module attribute (`math.pi`) isn't a value-attr read.
                if !self.is_module_ref(cx, obj) {
                    self.type_of(cx, obj)?;
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
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => {
                self.type_of(cx, obj)?;
                for b in [start, stop, step].into_iter().flatten() {
                    self.type_of(cx, b)?;
                }
                Ok(Ty::Value)
            }
            // Comprehensions bind their own variables, which aren't in scope
            // here — checking happens during codegen where they're bound.
            ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => Ok(Ty::Value),
            ExprKind::MethodCall(recv, _, args) => {
                // A module receiver (`math`) isn't a value — don't type it.
                if !self.is_module_ref(cx, recv) {
                    self.type_of(cx, recv)?;
                }
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
        // Comparison conditions skip the boxed-bool round-trip; the operands
        // stay boxed so a custom dunder could run.
        assert!(wat.contains("(if (call $py_lt (global.get $g_x) (ref.i31 (i32.const 5)))"));
        assert!(wat.contains("(then"));
        assert!(wat.contains("(else"));
    }

    #[test]
    fn elif_chain_nests() {
        let src =
            "x = 2\nif x < 1:\n    print(1)\nelif x < 3:\n    print(2)\nelse:\n    print(3)\n";
        let wat = compile(src).unwrap();
        // Two conditions compile to two direct comparisons in _start.
        assert_eq!(wat.matches("(if (call $py_lt").count(), 2);
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
            "(br_if $b0 (i32.eqz (call $py_gt (global.get $g_i) (ref.i31 (i32.const 0)))))"
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
        assert!(err.message.contains("too many positional arguments"));
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
