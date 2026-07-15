//! Step 5e: the WIT/PXC component converter's front half
//! (acornstem/LESSON_PLAYER.md, "The convertibility contract").
//!
//! Takes a program containing a stamped component instance and produces the
//! three generated inputs the native chain (`tools/componentize.sh`) builds
//! into a real Component-Model component:
//!
//! - **`python`** — the instance's def group, extracted VERBATIM by source
//!   lines (glass-box: the kid's own text is what compiles). The
//!   component-clean lint is the mechanical precondition, so the extract is
//!   self-contained by construction — a clean group can only reference
//!   itself and builtins.
//! - **`wit`** — the world: exports from the recorded API surface + param
//!   annotations; imports = exactly the host capabilities the group uses
//!   (plus `p2w-putc`, the runtime's own output seam).
//! - **`shim_c`** — the canonical-ABI shim: `cabi_realloc` (bump), import
//!   wrappers (p2w string Value → ptr/len; readers rebuild returned strings
//!   from the retptr), export wrappers (canonical params → p2w values,
//!   call per the def's BORROW MASK — borrowed strings released post-call —
//!   and release the result).
//!
//! Type mapping (as-built): `int -> s32` (the linear-memory runtime's int
//! width today — the spec's `s64` arrives when the value model widens),
//! `float -> f64`, `str -> string`. An API def with any other param shape
//! stays INTERNAL (compiled, callable from exports, just not exported);
//! a def group that exports nothing is an error, not a silent empty world.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::lint;

/// A host capability the converter knows how to carry across the component
/// boundary in v1: its builtin name, WIT declaration, and the C shim body
/// that unmarshals p2w Values (all args cross the LLVM seam BOXED — see the
/// `p2w_host_*` lowering) into canonical-ABI scalars.
struct Cap {
    name: &'static str,
    wit: &'static str,
    /// (canonical import C signature, wrapper body) — see `shim_c` assembly.
    c_import: &'static str,
    c_wrapper: &'static str,
}

const CAPS: &[Cap] = &[
    Cap {
        name: "set_text",
        wit: "set-text: func(selector: string, text: string);",
        c_import: "extern void imp_set_text(int sp, int sl, int tp, int tl);",
        c_wrapper: "void p2w_host_set_text(int sel, int txt) {\n  imp_set_text(p2w_str_ptr(sel), p2w_str_len(sel), p2w_str_ptr(txt), p2w_str_len(txt));\n}",
    },
    Cap {
        name: "set_attr",
        wit: "set-attr: func(selector: string, name: string, value: string);",
        c_import: "extern void imp_set_attr(int sp, int sl, int np, int nl, int vp, int vl);",
        c_wrapper: "void p2w_host_set_attr(int sel, int name, int val) {\n  imp_set_attr(p2w_str_ptr(sel), p2w_str_len(sel), p2w_str_ptr(name), p2w_str_len(name), p2w_str_ptr(val), p2w_str_len(val));\n}",
    },
    Cap {
        name: "set_position",
        wit: "set-position: func(selector: string, x: s32, y: s32);",
        c_import: "extern void imp_set_position(int sp, int sl, int x, int y);",
        c_wrapper: "void p2w_host_set_position(int sel, int x, int y) {\n  imp_set_position(p2w_str_ptr(sel), p2w_str_len(sel), p2w_unbox_int(x), p2w_unbox_int(y));\n}",
    },
    Cap {
        name: "set_field",
        wit: "set-field: func(key: string, value: string);",
        c_import: "extern void imp_set_field(int kp, int kl, int vp, int vl);",
        c_wrapper: "void p2w_host_set_field(int key, int val) {\n  imp_set_field(p2w_str_ptr(key), p2w_str_len(key), p2w_str_ptr(val), p2w_str_len(val));\n}",
    },
    Cap {
        name: "evidence",
        wit: "evidence: func(key: string, value: string);",
        c_import: "extern void imp_evidence(int kp, int kl, int vp, int vl);",
        c_wrapper: "void p2w_host_evidence(int key, int val) {\n  imp_evidence(p2w_str_ptr(key), p2w_str_len(key), p2w_str_ptr(val), p2w_str_len(val));\n}",
    },
    Cap {
        name: "add_element",
        wit: "add-element: func(parent: string, tag: string, id: string);",
        c_import: "extern void imp_add_element(int pp, int pl, int tp, int tl, int ip, int il);",
        c_wrapper: "void p2w_host_add_element(int parent, int tag, int id) {\n  imp_add_element(p2w_str_ptr(parent), p2w_str_len(parent), p2w_str_ptr(tag), p2w_str_len(tag), p2w_str_ptr(id), p2w_str_len(id));\n}",
    },
    // Readers: a canonical string RETURN flows host -> guest through a
    // retptr — jco lands the bytes in guest memory via our cabi_realloc and
    // writes [ptr, len] at the retarea; the wrapper copies them into an
    // OWNED p2w string (rc 1 — the LLVM caller releases it downstream).
    Cap {
        name: "get_field",
        wit: "get-field: func(key: string) -> string;",
        c_import: "extern void imp_get_field(int kp, int kl, int* ret);",
        c_wrapper: "static int gf_ret[2];\nint p2w_host_get_field(int key) {\n  imp_get_field(p2w_str_ptr(key), p2w_str_len(key), gf_ret);\n  return p2w_str((const unsigned char*)gf_ret[0], gf_ret[1]);\n}",
    },
    Cap {
        name: "get_value",
        wit: "get-value: func(selector: string) -> string;",
        c_import: "extern void imp_get_value(int sp, int sl, int* ret);",
        c_wrapper: "static int gv_ret[2];\nint p2w_host_get_value(int sel) {\n  imp_get_value(p2w_str_ptr(sel), p2w_str_len(sel), gv_ret);\n  return p2w_str((const unsigned char*)gv_ret[0], gv_ret[1]);\n}",
    },
    Cap {
        name: "pointer_x",
        wit: "pointer-x: func() -> s32;",
        c_import: "extern int imp_pointer_x(void);",
        c_wrapper: "int p2w_host_pointer_x(void) { return p2w_int(imp_pointer_x()); }",
    },
    Cap {
        name: "pointer_y",
        wit: "pointer-y: func() -> s32;",
        c_import: "extern int imp_pointer_y(void);",
        c_wrapper: "int p2w_host_pointer_y(void) { return p2w_int(imp_pointer_y()); }",
    },
];

/// Host builtins a component def may NOT use yet — callbacks need an event
/// bridge design (how does a host deliver events INTO a component?), and the
/// rest are app/activity-level, not component-level. The message names the
/// cap so the fix is obvious. Everything else non-cap (print, len, str, …)
/// compiles normally.
const UNSUPPORTED_CAPS: &[&str] = &[
    "on",
    "on_click",
    "on_key",
    "every",
    "on_frame",
    "play_sound",
    "beep",
    "flash",
    "seed",
    "report",
    "emit_html",
    "show",
    "input",
];

/// One exported function of the component: the recorded API name (`set`),
/// the def it binds to (`grid_set`), and the WIT-typed signature.
#[derive(Debug)]
pub struct WitExport {
    pub api_name: String,
    pub def_name: String,
    /// `(param name, wit type)` — wit type is one of `s32`/`f64`/`string`.
    pub params: Vec<(String, &'static str)>,
    /// Per-param borrow mask under the emitted calling convention: `true` =
    /// the def BORROWS it (a non-escaping heap param), so the shim — the
    /// caller — must release the string it built AFTER the call; `false` =
    /// the def consumes it. Positional with `params`.
    pub param_borrowed: Vec<bool>,
    /// `Some("s32"|"f64")` for an annotated scalar return, else `None`.
    pub result: Option<&'static str>,
}

/// The converter's output: everything the native chain needs, plus the
/// surface lists for display.
#[derive(Debug)]
pub struct ComponentExtract {
    pub python: String,
    pub wit: String,
    pub shim_c: String,
    pub exports: Vec<WitExport>,
    /// Host capability builtin names the group uses (WIT imports).
    pub imports: Vec<&'static str>,
    /// API defs kept internal (present but not exportable in v1), with why.
    pub skipped: Vec<(String, String)>,
}

/// Convert one stamped instance. `instance` is the stamped id (`grid`,
/// `grid2`, …); `api` is the registry row's recorded API surface.
pub fn to_component(
    source: &str,
    instance: &str,
    api: &[String],
) -> Result<ComponentExtract, String> {
    let toks = crate::lexer::lex(source).map_err(|e| e.to_string())?;
    let stmts = crate::parser::parse(&toks).map_err(|e| e.to_string())?;

    let group = lint::component_group(&stmts, instance);
    if group.is_empty() {
        return Err(format!(
            "no defs named `{instance}_…` found — is `{instance}` the stamped instance id?"
        ));
    }

    // The component-clean lint is the conversion PRECONDITION, mechanically:
    // a clean group references only itself and builtins, so the extract
    // below is self-contained by construction.
    let unclean = lint::component_clean_warnings(&stmts, &group, instance);
    if !unclean.is_empty() {
        let mut msg = String::from("not component-clean — fix these first:\n");
        for (line, m) in &unclean {
            msg.push_str(&format!("  line {line}: {m}\n"));
        }
        return Err(msg);
    }

    // Caps: collect what the group uses; refuse what v1 can't carry.
    let (imports, blocked) = scan_caps(&stmts, &group);
    if let Some((line, name)) = blocked {
        return Err(format!(
            "line {line}: `{name}` can't cross the component boundary yet — \
             v1 converts components that only use: {}",
            CAPS.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
        ));
    }

    let python = extract_group_source(source, &stmts, &group);

    // Export surface: annotated scalar/string API defs. Others stay internal.
    let mut exports = Vec::new();
    let mut skipped = Vec::new();
    for a in api {
        let def_name = format!("{instance}_{a}");
        let Some((params, param_types, return_type)) = find_def(&stmts, &def_name) else {
            skipped.push((a.clone(), "no such def in this instance".to_string()));
            continue;
        };
        let mask = crate::llvm::param_borrow_mask(&stmts, &def_name);
        match export_sig(a, &def_name, params, param_types, return_type, mask) {
            Ok(x) => exports.push(x),
            Err(why) => skipped.push((a.clone(), why)),
        }
    }
    if exports.is_empty() {
        return Err(format!(
            "none of the API defs are exportable — annotate their parameters \
             (int / float / str) to give them WIT types. Skipped: {}",
            skipped
                .iter()
                .map(|(n, w)| format!("{n} ({w})"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let wit = wit_world(instance, &exports, &imports);
    let shim_c = shim_c(&exports, &imports);
    Ok(ComponentExtract {
        python,
        wit,
        shim_c,
        exports,
        imports,
        skipped,
    })
}

/// Walk the group defs' bodies collecting used capability names; the first
/// unsupported cap (with its line) blocks conversion.
fn scan_caps(stmts: &[Stmt], group: &[String]) -> (Vec<&'static str>, Option<(usize, String)>) {
    let mut used: Vec<&'static str> = Vec::new();
    let mut blocked: Option<(usize, String)> = None;
    for s in stmts {
        if let StmtKind::Def { name, body, .. } = &s.kind {
            if group.contains(name) {
                scan_body(body, &mut used, &mut blocked);
            }
        }
    }
    (used, blocked)
}

fn scan_body(stmts: &[Stmt], used: &mut Vec<&'static str>, blocked: &mut Option<(usize, String)>) {
    for s in stmts {
        lint::stmt_exprs(s, &mut |e| scan_expr(e, used, blocked));
        lint::for_each_child_block(s, |b, _| scan_body(b, used, blocked));
    }
}

fn scan_expr(e: &Expr, used: &mut Vec<&'static str>, blocked: &mut Option<(usize, String)>) {
    if let ExprKind::Call(f, _) = &e.kind {
        if let Some(cap) = CAPS.iter().find(|c| c.name == f) {
            if !used.contains(&cap.name) {
                used.push(cap.name);
            }
        } else if UNSUPPORTED_CAPS.contains(&f.as_str()) && blocked.is_none() {
            *blocked = Some((e.line, f.clone()));
        }
    }
    lint::each_child_expr(e, &mut |c| scan_expr(c, used, blocked));
}

/// Slice the group defs out of `source` by line span: each top-level group
/// def runs from its own line to the line before the next top-level
/// statement (or EOF), trailing blanks trimmed. Verbatim — the kid's text.
fn extract_group_source(source: &str, stmts: &[Stmt], group: &[String]) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::new();
    for (i, s) in stmts.iter().enumerate() {
        let StmtKind::Def { name, .. } = &s.kind else {
            continue;
        };
        if !group.contains(name) {
            continue;
        }
        let start = s.line.saturating_sub(1); // 1-based -> 0-based
        let end = stmts
            .get(i + 1)
            .map_or(lines.len(), |n| n.line.saturating_sub(1));
        let chunk = lines[start..end.min(lines.len())].join("\n");
        out.push_str(chunk.trim_end());
        out.push_str("\n\n");
    }
    out.trim_end().to_string() + "\n"
}

fn find_def<'a>(
    stmts: &'a [Stmt],
    def_name: &str,
) -> Option<(&'a [String], &'a [Option<Expr>], &'a Option<Expr>)> {
    stmts.iter().find_map(|s| match &s.kind {
        StmtKind::Def {
            name,
            params,
            param_types,
            return_type,
            ..
        } if name == def_name => Some((params.as_slice(), param_types.as_slice(), return_type)),
        _ => None,
    })
}

/// The scalar WIT type of an annotation, or `None` for anything v1 can't
/// carry across the canonical ABI (bool needs an i1 story, lists need
/// canonical lowering into p2w values).
fn wit_ty(ann: &Option<Expr>) -> Option<&'static str> {
    match ann {
        Some(e) => match &e.kind {
            ExprKind::Name(n) if n == "int" => Some("s32"),
            ExprKind::Name(n) if n == "float" => Some("f64"),
            ExprKind::Name(n) if n == "str" => Some("string"),
            _ => None,
        },
        None => None,
    }
}

fn export_sig(
    api_name: &str,
    def_name: &str,
    params: &[String],
    param_types: &[Option<Expr>],
    return_type: &Option<Expr>,
    param_borrowed: Vec<bool>,
) -> Result<WitExport, String> {
    let mut sig = Vec::new();
    for (i, p) in params.iter().enumerate() {
        let ann: Option<Expr> = param_types.get(i).cloned().flatten();
        match wit_ty(&ann) {
            Some(t) => sig.push((p.clone(), t)),
            None => {
                return Err(format!(
                    "parameter `{p}` needs an int / float / str annotation"
                ));
            }
        }
    }
    let result = match return_type {
        None => None,
        Some(_) => match wit_ty(return_type) {
            Some("string") => {
                return Err("str returns need canonical lowering (later slice)".to_string());
            }
            Some(t) => Some(t),
            None => return Err("the return annotation must be int or float".to_string()),
        },
    };
    Ok(WitExport {
        api_name: api_name.to_string(),
        def_name: def_name.to_string(),
        params: sig,
        param_borrowed,
        result,
    })
}

fn kebab(name: &str) -> String {
    name.replace('_', "-")
}

/// WIT keywords and built-in type names — using one as an identifier needs
/// the explicit-identifier `%` prefix (`option: string` is a parse error,
/// `%option: string` is fine). Kids' param names hit these constantly
/// (`option`, `list`, `type`, `result`, …). Over-listing is harmless: `%` on
/// a non-keyword is still a legal identifier.
const WIT_KEYWORDS: &[&str] = &[
    "use",
    "type",
    "resource",
    "func",
    "record",
    "enum",
    "flags",
    "variant",
    "static",
    "interface",
    "world",
    "import",
    "export",
    "package",
    "include",
    "constructor",
    "as",
    "from",
    "with",
    "own",
    "borrow",
    "async",
    "option",
    "result",
    "list",
    "tuple",
    "string",
    "bool",
    "char",
    "future",
    "stream",
    "u8",
    "u16",
    "u32",
    "u64",
    "s8",
    "s16",
    "s32",
    "s64",
    "f32",
    "f64",
];

/// A name as a WIT identifier: kebab-cased, `%`-escaped if it collides with
/// a WIT keyword.
fn wit_ident(name: &str) -> String {
    let k = kebab(name);
    if WIT_KEYWORDS.contains(&k.as_str()) {
        format!("%{k}")
    } else {
        k
    }
}

/// The WIT world text. One `host` import interface carrying exactly the used
/// caps plus `p2w-putc` (the runtime's unconditional output seam), exports
/// from the API surface, and `live` — the RC oracle rides along so hosts can
/// assert live==0 like every other backend.
fn wit_world(instance: &str, exports: &[WitExport], imports: &[&'static str]) -> String {
    let mut w = String::from("package acorn:component;\n\ninterface host {\n");
    w.push_str("  p2w-putc: func(byte: s32);\n");
    for cap in CAPS {
        if imports.contains(&cap.name) {
            w.push_str("  ");
            w.push_str(cap.wit);
            w.push('\n');
        }
    }
    w.push_str("}\n\nworld ");
    w.push_str(&kebab(instance));
    w.push_str(" {\n  import host;\n");
    for x in exports {
        w.push_str("  export ");
        w.push_str(&wit_ident(&x.api_name));
        w.push_str(": func(");
        let ps: Vec<String> = x
            .params
            .iter()
            .map(|(n, t)| format!("{}: {t}", wit_ident(n)))
            .collect();
        w.push_str(&ps.join(", "));
        w.push(')');
        if let Some(r) = x.result {
            w.push_str(&format!(" -> {r}"));
        }
        w.push_str(";\n");
    }
    w.push_str("  export live: func() -> s32;\n");
    // Teardown: frees the literal caches main's exit epilogue would have
    // freed (a resident component never runs main), resetting the slots so
    // the component stays usable. After dispose, live must be 0.
    w.push_str("  export dispose: func();\n}\n");
    w
}

/// The generated canonical-ABI shim. Layout: runtime externs, `cabi_realloc`
/// (bump — reset at each export entry; canonical params are consumed into
/// p2w values immediately), a stubbed `p2w_getc` (components have no stdin),
/// import wrappers, export wrappers.
fn shim_c(exports: &[WitExport], imports: &[&'static str]) -> String {
    let mut c = String::from(
        "/* generated by rust-p2w's component converter (LESSON_PLAYER.md step 5e) */\n\
         extern int p2w_int(int n);\n\
         extern int p2w_str(const unsigned char* p, int len);\n\
         extern int p2w_str_ptr(int v);\n\
         extern int p2w_str_len(int v);\n\
         extern int p2w_unbox_int(int v);\n\
         extern void p2w_release(int v);\n\
         extern int p2w_live(void);\n\
         int p2w_getc(void) { return -1; } /* components have no stdin */\n\n\
         /* canonical-ABI guest allocator: a bump over a static page, reset per\n\
            export call (lowered params are consumed into p2w values at entry) */\n\
         static unsigned char cabi_buf[65536];\n\
         static unsigned long cabi_off = 0;\n\
         __attribute__((export_name(\"cabi_realloc\")))\n\
         void* cabi_realloc(void* old, unsigned long old_size, unsigned long align, unsigned long size) {\n\
           (void)old; (void)old_size;\n\
           cabi_off = (cabi_off + align - 1) & ~(align - 1);\n\
           void* p = cabi_buf + cabi_off;\n\
           cabi_off += size;\n\
           return p;\n\
         }\n\n",
    );

    // p2w-putc always: the runtime's print path imports it.
    c.push_str(
        "__attribute__((import_module(\"acorn:component/host\"), import_name(\"p2w-putc\")))\n\
         extern void imp_putc(int byte);\n\
         void p2w_putc(int ch) { imp_putc(ch); }\n\n",
    );
    for cap in CAPS {
        if imports.contains(&cap.name) {
            c.push_str(&format!(
                "__attribute__((import_module(\"acorn:component/host\"), import_name(\"{}\")))\n",
                kebab(cap.name)
            ));
            c.push_str(cap.c_import);
            c.push('\n');
            c.push_str(cap.c_wrapper);
            c.push_str("\n\n");
        }
    }

    for x in exports {
        // The compiled def's C-visible signature: annotated int params are
        // UNBOXED i32 (Repr::Int), float are double, str are boxed Values.
        let ext_params: Vec<&str> = x
            .params
            .iter()
            .map(|(_, t)| match *t {
                "f64" => "double",
                _ => "int", // s32 (unboxed) and string (boxed Value) are both i32
            })
            .collect();
        let ret_c = match x.result {
            Some("f64") => "double",
            Some(_) => "int",
            None => "int", // un-annotated defs return a boxed Value (released below)
        };
        c.push_str(&format!(
            "extern {ret_c} {}({});\n",
            x.def_name,
            ext_params.join(", ")
        ));
        // The canonical export wrapper. String params bind to temps because
        // ownership is per-param (the def's borrow mask): an OWNED param is
        // consumed by the call, a BORROWED one stays ours — release it after
        // the call or it leaks once per call (the poll_bump lesson).
        let mut sig = Vec::new();
        let mut pre = String::new();
        let mut args = Vec::new();
        let mut post = String::new();
        for (i, (_, t)) in x.params.iter().enumerate() {
            match *t {
                "string" => {
                    sig.push(format!("unsigned char* p{i}, int p{i}_len"));
                    pre.push_str(&format!("  int s{i} = p2w_str(p{i}, p{i}_len);\n"));
                    args.push(format!("s{i}"));
                    if x.param_borrowed.get(i).copied().unwrap_or(false) {
                        post.push_str(&format!("  p2w_release(s{i});\n"));
                    }
                }
                "f64" => {
                    sig.push(format!("double p{i}"));
                    args.push(format!("p{i}"));
                }
                _ => {
                    sig.push(format!("int p{i}"));
                    args.push(format!("p{i}"));
                }
            }
        }
        c.push_str(&format!(
            "__attribute__((export_name(\"{}\")))\n",
            kebab(&x.api_name)
        ));
        let call = format!("{}({})", x.def_name, args.join(", "));
        match x.result {
            Some(r) => {
                let rc = if r == "f64" { "double" } else { "int" };
                c.push_str(&format!(
                    "{rc} x_{}({}) {{\n  cabi_off = 0;\n{pre}  {rc} r = {call};\n{post}  return r;\n}}\n\n",
                    x.api_name,
                    sig.join(", "),
                ));
            }
            None => {
                c.push_str(&format!(
                    "void x_{}({}) {{\n  cabi_off = 0;\n{pre}  p2w_release({call});\n{post}}}\n\n",
                    x.api_name,
                    sig.join(", "),
                ));
            }
        }
    }

    c.push_str("__attribute__((export_name(\"live\")))\nint x_live(void) { return p2w_live(); }\n");
    c.push_str(
        "extern void p2w_dispose(void);\n\
         __attribute__((export_name(\"dispose\")))\nvoid x_dispose(void) { p2w_dispose(); }\n",
    );
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: &str = "def grid_set(row: int, col: int, value: str):\n    set_text(\"#grid_\" + str(row) + \"_\" + str(col), value)\n\ndef grid_show(data):\n    for r in range(len(data)):\n        for c in range(len(data[r])):\n            grid_set(r, c, str(data[r][c]))\n";

    fn api(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn grid_converts_with_set_exported_and_show_internal() {
        let x = to_component(GRID, "grid", &api(&["set", "show"])).expect("convert");
        // set is exported with WIT types from the annotations…
        assert_eq!(x.exports.len(), 1);
        assert_eq!(x.exports[0].api_name, "set");
        assert!(
            x.wit
                .contains("export set: func(row: s32, col: s32, value: string);"),
            "{}",
            x.wit
        );
        // …show stays internal (unannotated list param), named in `skipped`.
        assert_eq!(x.skipped.len(), 1);
        assert!(x.skipped[0].1.contains("annotation"), "{:?}", x.skipped);
        // Imports = exactly the used caps (+ putc, always).
        assert_eq!(x.imports, vec!["set_text"]);
        assert!(x.wit.contains("p2w-putc"), "{}", x.wit);
        assert!(
            x.wit.contains("set-text: func(selector: string"),
            "{}",
            x.wit
        );
        assert!(
            !x.wit.contains("set-attr"),
            "unused cap leaked in: {}",
            x.wit
        );
        // The extract is the verbatim def group and still compiles standalone.
        assert!(
            x.python.starts_with("def grid_set(row: int"),
            "{}",
            x.python
        );
        assert!(x.python.contains("def grid_show(data):"), "{}", x.python);
        crate::compile_to_wat(&x.python).expect("extract compiles standalone");
        // The shim wires the canonical shapes.
        assert!(x.shim_c.contains("export_name(\"set\")"), "{}", x.shim_c);
        assert!(
            x.shim_c.contains("int s2 = p2w_str(p2, p2_len);"),
            "{}",
            x.shim_c
        );
        assert!(
            x.shim_c.contains("p2w_release(grid_set(p0, p1, s2));"),
            "{}",
            x.shim_c
        );
        assert!(x.shim_c.contains("cabi_realloc"), "{}", x.shim_c);
        assert!(
            x.shim_c.contains("import_name(\"set-text\")"),
            "{}",
            x.shim_c
        );
        // The world is the instance, kebab-cased, with the RC oracle and the
        // resident-teardown export.
        assert!(x.wit.contains("world grid {"), "{}", x.wit);
        assert!(x.wit.contains("export live: func() -> s32;"), "{}", x.wit);
        assert!(x.wit.contains("export dispose: func();"), "{}", x.wit);
        assert!(
            x.shim_c.contains("export_name(\"dispose\")"),
            "{}",
            x.shim_c
        );
    }

    #[test]
    fn dispose_releases_and_resets_every_cache_slot() {
        // The emitted module carries p2w_dispose: for each literal-cache slot,
        // release THEN reset to 0 — so a resident component can prove live==0
        // at teardown and still keep working (next call re-interns).
        let toks = crate::lexer::lex(GRID).unwrap();
        let stmts = crate::parser::parse(&toks).unwrap();
        let ir = crate::llvm::emit_llvm_ir(&stmts).expect("emit");
        let dispose = ir
            .split("define void @p2w_dispose()")
            .nth(1)
            .expect("dispose emitted");
        let dispose = &dispose[..dispose.find('}').unwrap()];
        // GRID interns two literals ("#grid_" and "_") — both released + zeroed.
        assert_eq!(
            dispose.matches("call void @p2w_release").count(),
            2,
            "{dispose}"
        );
        assert_eq!(
            dispose.matches("store i32 0, ptr @").count(),
            2,
            "{dispose}"
        );
    }

    #[test]
    fn caps_are_in_lockstep_with_the_llvm_lowering() {
        // component::CAPS (WIT + shim) and llvm::HOST_CAPS (the lowering)
        // must agree exactly, or a cap converts on one side only.
        let here: Vec<&str> = CAPS.iter().map(|c| c.name).collect();
        let llvm: Vec<&str> = crate::llvm::HOST_CAPS.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(here, llvm);
    }

    #[test]
    fn host_caps_lower_to_shim_symbols_in_the_ir() {
        let toks = crate::lexer::lex(GRID).unwrap();
        let stmts = crate::parser::parse(&toks).unwrap();
        let ir = crate::llvm::emit_llvm_ir(&stmts).expect("emit");
        // The call is a void call on the shim-resolved symbol…
        assert!(ir.contains("call void @p2w_host_set_text(i32"), "{ir}");
        // …declared in the runtime ABI header.
        assert!(
            ir.contains("declare void @p2w_host_set_text(i32, i32)"),
            "{ir}"
        );
    }

    #[test]
    fn unclean_groups_are_refused_with_the_lint_text() {
        let src = "def grid_set(v: str):\n    set_text(\"#msg\", v)\n";
        let err = to_component(src, "grid", &api(&["set"])).unwrap_err();
        assert!(err.contains("not component-clean"), "{err}");
        assert!(err.contains("#msg"), "{err}");
    }

    #[test]
    fn unsupported_caps_are_named() {
        let src = "def grid_set(v: str):\n    set_text(\"#grid_a\", v)\n    on_frame(grid_set)\n";
        let err = to_component(src, "grid", &api(&["set"])).unwrap_err();
        assert!(err.contains("`on_frame`"), "{err}");
        assert!(
            err.contains("can't cross the component boundary yet"),
            "{err}"
        );
    }

    #[test]
    fn all_api_defs_unexportable_is_an_error_naming_the_gaps() {
        let src = "def grid_show(data):\n    print(data)\n";
        let err = to_component(src, "grid", &api(&["show"])).unwrap_err();
        assert!(err.contains("annotate"), "{err}");
        assert!(err.contains("show"), "{err}");
    }

    #[test]
    fn reader_caps_convert_with_canonical_string_returns() {
        // The Poll shape: a def that READS host state (get_field) and writes
        // it back — the canonical-string-return machinery end to end.
        let src = "def poll_bump(option: str):\n    n = get_field(\"poll_\" + option)\n    if n == \"\":\n        n = \"0\"\n    set_field(\"poll_\" + option, str(int(n) + 1))\n    set_text(\"#poll_count_\" + option, str(int(n) + 1))\n";
        let x = to_component(src, "poll", &api(&["bump"])).expect("convert");
        assert!(
            x.wit.contains("get-field: func(key: string) -> string;"),
            "{}",
            x.wit
        );
        // The reader wrapper goes retptr -> owned p2w string…
        assert!(
            x.shim_c
                .contains("imp_get_field(p2w_str_ptr(key), p2w_str_len(key), gf_ret)"),
            "{}",
            x.shim_c
        );
        assert!(
            x.shim_c
                .contains("return p2w_str((const unsigned char*)gf_ret[0], gf_ret[1])"),
            "{}",
            x.shim_c
        );
        // …and the IR calls it as a VALUE (not void).
        let toks = crate::lexer::lex(src).unwrap();
        let stmts = crate::parser::parse(&toks).unwrap();
        let ir = crate::llvm::emit_llvm_ir(&stmts).expect("emit");
        assert!(ir.contains("call i32 @p2w_host_get_field(i32"), "{ir}");
        // poll_bump's `option` is BORROW-masked (it never escapes), so the
        // shim — the caller — must release the string it built, or one
        // string leaks per call (found by the live oracle under jco).
        assert_eq!(
            x.exports[0].param_borrowed,
            vec![true],
            "{:?}",
            x.exports[0]
        );
        assert!(
            x.shim_c.contains("p2w_release(poll_bump(s0));")
                || x.shim_c.contains("p2w_release(s0);"),
            "borrowed param must be released post-call:\n{}",
            x.shim_c
        );
        // pointer caps ride the same table: no-arg s32 readers.
        let draw = "def draw_dot():\n    add_element(\"#draw\", \"circle\", \"draw_d1\")\n    set_attr(\"#draw_d1\", \"cx\", str(pointer_x()))\n";
        let d = to_component(draw, "draw", &api(&["dot"])).expect("convert draw");
        assert!(d.wit.contains("pointer-x: func() -> s32;"), "{}", d.wit);
        assert!(
            d.wit
                .contains("add-element: func(parent: string, tag: string, id: string);"),
            "{}",
            d.wit
        );
    }

    #[test]
    fn scalar_returns_export_and_string_returns_wait() {
        let src = "def calc_area(w: int, h: int) -> int:\n    return w * h\n";
        let x = to_component(src, "calc", &api(&["area"])).expect("convert");
        assert!(
            x.wit.contains("export area: func(w: s32, h: s32) -> s32;"),
            "{}",
            x.wit
        );
        // A value-returning export returns the unboxed result — no release.
        assert!(
            x.shim_c.contains("int r = calc_area(p0, p1);"),
            "{}",
            x.shim_c
        );

        let bad = "def calc_name(w: int) -> str:\n    return str(w)\n";
        let err = to_component(bad, "calc", &api(&["name"])).unwrap_err();
        assert!(err.contains("later slice"), "{err}");
    }
}
