//! The `wasm:js-string` builtins and string-literal imports, as a wasmtime
//! host — the polyfill the spec REQUIRES to be possible.
//!
//! The browser gets these natively (compile options
//! `{ builtins: ['js-string'], importedStringConstants: "'" }`); every
//! wasmtime host — the exec/differential test rig AND `p2w run`'s fuel
//! harness — links THIS file, so the two cannot drift. Strings live host-side
//! as Rust `String`s wrapped in `ExternRef`.

use wasmtime::{AsContextMut, ExternRef, Global, GlobalType, Mutability, Rooted, Val};
use wasmtime::{Caller, Linker, Module, Store};

/// Read the host String out of a js-string externref (trap-worthy if the ref
/// is null or not a string — mirrors the builtins' spec'd type errors).
pub fn js_str(store: &mut impl AsContextMut, r: &Option<Rooted<ExternRef>>) -> String {
    let r = r.as_ref().expect("js-string builtin: null externref");
    r.data(&mut *store)
        .expect("externref data")
        .expect("externref host data")
        .downcast_ref::<String>()
        .expect("externref is not a js-string")
        .clone()
}

fn js_new(store: &mut impl AsContextMut, s: String) -> Rooted<ExternRef> {
    ExternRef::new(&mut *store, s).expect("alloc externref")
}

/// Define every `wasm:js-string` builtin the emitter uses on `linker`.
pub fn add_js_string_builtins<T: 'static>(linker: &mut Linker<T>) {
    let m = "wasm:js-string";
    linker
        .func_wrap(
            m,
            "test",
            |mut c: Caller<'_, T>, r: Option<Rooted<ExternRef>>| -> i32 {
                // 1 iff the externref wraps a host string. Null and externalized
                // wasm values (i31s, structs — they arrive as non-string data)
                // answer 0, matching JS semantics where those surface as
                // numbers/objects.
                match r {
                    None => 0,
                    Some(r) => r
                        .data(&mut c)
                        .ok()
                        .flatten()
                        .map(|d| d.downcast_ref::<String>().is_some())
                        .unwrap_or(false) as i32,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "length",
            |mut c: Caller<'_, T>, s: Option<Rooted<ExternRef>>| -> i32 {
                js_str(&mut c, &s).encode_utf16().count() as i32
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "charCodeAt",
            |mut c: Caller<'_, T>, s: Option<Rooted<ExternRef>>, i: i32| -> i32 {
                js_str(&mut c, &s)
                    .encode_utf16()
                    .nth(i as usize)
                    .expect("charCodeAt out of bounds") as i32
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "concat",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> Rooted<ExternRef> {
                let s = js_str(&mut c, &a) + &js_str(&mut c, &b);
                js_new(&mut c, s)
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "equals",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> i32 {
                // Spec: equals accepts null (null == null is true).
                match (&a, &b) {
                    (None, None) => 1,
                    (Some(_), Some(_)) => (js_str(&mut c, &a) == js_str(&mut c, &b)) as i32,
                    _ => 0,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "compare",
            |mut c: Caller<'_, T>,
             a: Option<Rooted<ExternRef>>,
             b: Option<Rooted<ExternRef>>|
             -> i32 {
                // UTF-16 code-unit order, the JS `<` on strings.
                let (a, b) = (js_str(&mut c, &a), js_str(&mut c, &b));
                let (au, bu): (Vec<u16>, Vec<u16>) =
                    (a.encode_utf16().collect(), b.encode_utf16().collect());
                match au.cmp(&bu) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "substring",
            |mut c: Caller<'_, T>,
             s: Option<Rooted<ExternRef>>,
             start: i32,
             end: i32|
             -> Rooted<ExternRef> {
                let units: Vec<u16> = js_str(&mut c, &s).encode_utf16().collect();
                let n = units.len() as i32;
                let (start, end) = (start.clamp(0, n) as usize, end.clamp(0, n) as usize);
                let out = if start >= end {
                    String::new()
                } else {
                    String::from_utf16_lossy(&units[start..end])
                };
                js_new(&mut c, out)
            },
        )
        .unwrap();
    linker
        .func_wrap(
            m,
            "fromCharCode",
            |mut c: Caller<'_, T>, u: i32| -> Rooted<ExternRef> {
                let s = String::from_utf16_lossy(&[u as u16]);
                js_new(&mut c, s)
            },
        )
        .unwrap();
    // The two array builtins take the CONCRETE (array (mut i16)) type — the
    // generic Func::wrap ArrayRef signature doesn't unify with it, so their
    // types are built explicitly against the engine.
    let u16s = wasmtime::ArrayType::new(
        linker.engine(),
        wasmtime::FieldType::new(Mutability::Var, wasmtime::StorageType::I16),
    );
    let u16s_ref = wasmtime::ValType::Ref(wasmtime::RefType::new(
        true,
        wasmtime::HeapType::ConcreteArray(u16s),
    ));
    let extern_nonnull =
        wasmtime::ValType::Ref(wasmtime::RefType::new(false, wasmtime::HeapType::Extern));
    let from_ty = wasmtime::FuncType::new(
        linker.engine(),
        [
            u16s_ref.clone(),
            wasmtime::ValType::I32,
            wasmtime::ValType::I32,
        ],
        [extern_nonnull],
    );
    linker
        .func_new(m, "fromCharCodeArray", from_ty, |mut c, params, results| {
            let arr = params[0].unwrap_anyref().expect("null array");
            let arr = arr.unwrap_array(&mut c).expect("not an array");
            let (start, end) = (params[1].unwrap_i32(), params[2].unwrap_i32());
            let mut units = Vec::with_capacity((end - start).max(0) as usize);
            for i in start..end {
                let v = arr.get(&mut c, i as u32).expect("array get");
                units.push(v.unwrap_i32() as u16);
            }
            let s = String::from_utf16_lossy(&units);
            results[0] = Val::ExternRef(Some(ExternRef::new(&mut c, s).expect("alloc")));
            Ok(())
        })
        .unwrap();
    let into_ty = wasmtime::FuncType::new(
        linker.engine(),
        [
            wasmtime::ValType::EXTERNREF,
            u16s_ref,
            wasmtime::ValType::I32,
        ],
        [wasmtime::ValType::I32],
    );
    linker
        .func_new(m, "intoCharCodeArray", into_ty, |mut c, params, results| {
            let s = params[0].unwrap_externref().cloned();
            let text = js_str(&mut c, &s);
            let arr = params[1].unwrap_anyref().expect("null array");
            let arr = arr.unwrap_array(&mut c).expect("not an array");
            let start = params[2].unwrap_i32();
            let mut n = 0;
            for (i, u) in text.encode_utf16().enumerate() {
                arr.set(&mut c, start as u32 + i as u32, Val::I32(u as i32))
                    .expect("array set");
                n += 1;
            }
            results[0] = Val::I32(n);
            Ok(())
        })
        .unwrap();
}

/// Provide every `(import "'" "<literal>" (global (ref extern)))` the module
/// declares: the import NAME is the string value — the spec's
/// `importedStringConstants` mechanism, polyfilled.
pub fn define_string_literals<T>(linker: &mut Linker<T>, store: &mut Store<T>, module: &Module) {
    for imp in module.imports() {
        if imp.module() != "'" {
            continue;
        }
        let text = imp.name().to_string();
        let r = ExternRef::new(&mut *store, text).expect("literal externref");
        let ty = GlobalType::new(
            wasmtime::ValType::Ref(wasmtime::RefType::new(false, wasmtime::HeapType::Extern)),
            Mutability::Const,
        );
        let g = Global::new(&mut *store, ty, Val::ExternRef(Some(r))).expect("literal global");
        linker
            .define(&mut *store, "'", imp.name(), g)
            .expect("define literal");
    }
}
