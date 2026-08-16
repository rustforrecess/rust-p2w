//! Stage-2 contract test (docs/REPRESENTATION_REWORK.md): the wasmtime
//! polyfill of `wasm:js-string` behaves the way the emitter will rely on.
//! Hand-written WAT stands in for stage-3 output: literals via the `"'"`
//! import module, and every builtin the lowering plan uses.

mod common;

use wasmtime::{Config, Engine, Linker, Module, Store};

const WAT: &str = r#"(module
  (type $u16s (array (mut i16)))
  (import "'" "hello " (global $lit_hello (ref extern)))
  (import "'" "world" (global $lit_world (ref extern)))
  (import "wasm:js-string" "length" (func $length (param externref) (result i32)))
  (import "wasm:js-string" "charCodeAt" (func $charCodeAt (param externref) (param i32) (result i32)))
  (import "wasm:js-string" "concat" (func $concat (param externref) (param externref) (result (ref extern))))
  (import "wasm:js-string" "equals" (func $equals (param externref) (param externref) (result i32)))
  (import "wasm:js-string" "compare" (func $compare (param externref) (param externref) (result i32)))
  (import "wasm:js-string" "substring" (func $substring (param externref) (param i32) (param i32) (result (ref extern))))
  (import "wasm:js-string" "fromCharCodeArray" (func $fromCharCodeArray (param (ref null $u16s)) (param i32) (param i32) (result (ref extern))))
  (import "wasm:js-string" "intoCharCodeArray" (func $intoCharCodeArray (param externref) (param (ref null $u16s)) (param i32) (result i32)))

  (func (export "greeting_len") (result i32)
    (call $length (call $concat (global.get $lit_hello) (global.get $lit_world))))
  (func (export "third_char") (result i32)
    (call $charCodeAt (global.get $lit_world) (i32.const 2)))
  (func (export "lits_equal") (result i32)
    (call $equals (global.get $lit_hello) (global.get $lit_hello)))
  (func (export "lits_differ") (result i32)
    (call $equals (global.get $lit_hello) (global.get $lit_world)))
  (func (export "hello_before_world") (result i32)
    (call $compare (global.get $lit_hello) (global.get $lit_world)))
  (func (export "sub_is_ell") (result i32)
    ;; substring("hello ", 1, 4) == "ell" -> compare against a round-trip
    ;; through the char-code array (into + from must invert each other).
    (local $buf (ref $u16s))
    (local $s (ref extern))
    (local.set $s (call $substring (global.get $lit_hello) (i32.const 1) (i32.const 4)))
    (local.set $buf (array.new_default $u16s (i32.const 8)))
    (drop (call $intoCharCodeArray (local.get $s) (local.get $buf) (i32.const 0)))
    (call $equals
      (call $fromCharCodeArray (local.get $buf) (i32.const 0) (i32.const 3))
      (local.get $s)))
)"#;

#[test]
fn js_string_polyfill_honors_the_contract() {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    let engine = Engine::new(&config).expect("engine");
    let module = Module::new(&engine, wat::parse_str(WAT).expect("wat")).expect("module");
    let mut store: Store<()> = Store::new(&engine, ());
    let mut linker: Linker<()> = Linker::new(&engine);
    common::add_js_string_builtins(&mut linker);
    common::define_string_literals(&mut linker, &mut store, &module);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");

    let call = |store: &mut Store<()>, name: &str| -> i32 {
        instance
            .get_typed_func::<(), i32>(&mut *store, name)
            .unwrap()
            .call(&mut *store, ())
            .unwrap()
    };
    assert_eq!(call(&mut store, "greeting_len"), 11); // "hello world"
    assert_eq!(call(&mut store, "third_char"), 'r' as i32);
    assert_eq!(call(&mut store, "lits_equal"), 1);
    assert_eq!(call(&mut store, "lits_differ"), 0);
    assert_eq!(call(&mut store, "hello_before_world"), -1);
    assert_eq!(call(&mut store, "sub_is_ell"), 1);
}
