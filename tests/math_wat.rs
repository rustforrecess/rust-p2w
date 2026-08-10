//! The vendored libm WAT (src/math_wat.rs) against the libm crate itself.
//!
//! The whole point of extracting from libm — rather than adopting any of the
//! existing wasm math libraries — is that the browser module and the native
//! runtime compute from the SAME source and therefore agree to the last bit.
//! These tests hold that claim to `f64::to_bits` equality, not tolerance:
//! the float formatter is shortest-round-trip exact, so a one-ULP difference
//! between backends would print different digits and show up in the
//! differential harness as a phantom language divergence.

use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

fn instantiate() -> (Store<()>, Instance) {
    // Compose exactly what the emitter will append, plus exports so the test
    // can reach the entry points.
    let wat = format!(
        "(module\n{}\n{}\n{}\n{}\n\
         (export \"pow\" (func $mw_m_pow))\n\
         (export \"exp\" (func $mw_m_exp))\n\
         (export \"log\" (func $mw_m_log))\n\
         (export \"log2\" (func $mw_m_log2))\n\
         (export \"log10\" (func $mw_m_log10)))",
        rust_p2w::MATH_MEMORY,
        rust_p2w::MATH_GLOBALS,
        rust_p2w::MATH_FUNCS,
        rust_p2w::MATH_DATA,
    );
    let wasm = wat::parse_str(&wat).expect("vendored WAT is invalid");
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    (store, instance)
}

/// Inputs that walk the interesting regions: exact cases, subnormal-adjacent,
/// huge, tiny, negative, the curriculum's own idioms (cube root, sigmoid
/// arguments, entropy probabilities).
#[allow(clippy::approx_constant)] // ln(2), e, pi as inputs: hard mantissas, not constants
const XS: &[f64] = &[
    1e-300,
    1e-10,
    0.001,
    0.1,
    0.5,
    0.6931471805599453,
    1.0,
    1.5,
    2.0,
    2.718281828459045,
    3.14159265358979,
    10.0,
    42.0,
    1e5,
    1e10,
    1e300,
];

#[test]
fn exp_log_log2_log10_are_bit_identical_to_libm() {
    let (mut store, instance) = instantiate();
    #[allow(clippy::type_complexity)] // (name, oracle) pairs — the shape is the point
    let cases: &[(&str, fn(f64) -> f64)] = &[
        ("exp", libm::exp),
        ("log", libm::log),
        ("log2", libm::log2),
        ("log10", libm::log10),
    ];
    for (name, oracle) in cases {
        let f: TypedFunc<f64, f64> = instance.get_typed_func(&mut store, name).unwrap();
        for &x in XS {
            let got = f.call(&mut store, x).unwrap();
            let want = oracle(x);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "{name}({x:e}): wasm {got:e} != libm {want:e}"
            );
        }
        // Negative input: log family returns NaN, exp returns a real value.
        let got = f.call(&mut store, -2.5).unwrap();
        let want = oracle(-2.5);
        assert_eq!(got.is_nan(), want.is_nan(), "{name}(-2.5) NaN-ness");
        if !want.is_nan() {
            assert_eq!(got.to_bits(), want.to_bits(), "{name}(-2.5)");
        }
    }
}

#[test]
fn pow_is_bit_identical_to_libm() {
    let (mut store, instance) = instantiate();
    let f: TypedFunc<(f64, f64), f64> = instance.get_typed_func(&mut store, "pow").unwrap();
    // The exponents students actually reach for, plus adversarial ones.
    let ys = [
        0.3333333333333333,
        0.5,
        1.5,
        2.0,
        -1.0,
        -0.5,
        10.0,
        100.0,
        0.0,
        2.5,
        -2.5,
        1e-3,
    ];
    for &x in XS {
        for &y in &ys {
            let got = f.call(&mut store, (x, y)).unwrap();
            let want = libm::pow(x, y);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "pow({x:e}, {y}): wasm {got:e} != libm {want:e}"
            );
        }
    }
    // Sign/zero/inf edge cases pow defines specially.
    for (a, b) in [
        (-8.0, 3.0),
        (-8.0, 0.3333333333333333),
        (0.0, -1.0),
        (f64::INFINITY, -2.0),
        (-1.0, f64::INFINITY),
    ] {
        let got = f.call(&mut store, (a, b)).unwrap();
        let want = libm::pow(a, b);
        assert!(
            got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
            "pow({a}, {b}): wasm {got:e} != libm {want:e}"
        );
    }
}

/// The regeneration gate: if tools/mathwat's pinned libm and the dev-dep
/// oracle ever drift apart, every assertion above would be comparing two
/// different libms and could pass while the BACKENDS disagree. Pin them
/// together here so the drift is loud.
#[test]
fn the_three_libm_pins_agree() {
    let mathwat = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/mathwat/Cargo.toml"),
    )
    .unwrap();
    let runtime = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/Cargo.toml"),
    )
    .unwrap();
    let own = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .unwrap();
    for (name, text) in [
        ("mathwat", &mathwat),
        ("runtime", &runtime),
        ("rust-p2w", &own),
    ] {
        assert!(
            text.contains("libm = \"=0.2.16\""),
            "{name}: libm pin drifted from =0.2.16 — regenerate math_wat.rs and \
             update all three manifests together"
        );
    }
}
