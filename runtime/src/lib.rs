//! Bare-metal runtime for the rust-p2w native (Pico 2 W) backend.
//!
//! This implements the `p2w_*` ABI the LLVM emitter (`rust-p2w/src/llvm.rs`)
//! calls — see `PICO_BACKEND.md`. The emitter is representation-agnostic; **this
//! crate owns the value representation and the allocator.** It's `no_std` for the
//! device (`thumbv8m.main-none-eabihf`) and compiled to a static library that
//! links against the emitted IR in the toolchain phase. The logic is pure, so it
//! host-tests under plain `cargo test`.
//!
//! ## Value representation — tagged `i32` (2-bit tag)
//! A Python value is one 32-bit word (pointer-width on the RP2350; matches
//! MicroPython/V8-SMI — see the value-model note):
//! - `0b01` → **small int**, payload `v >> 2` (signed, 30-bit; bignum promotion
//!   on overflow is a TODO).
//! - `0b00` → **heap pointer** (objects are ≥4-byte aligned, so the low bits are
//!   free); the object header carries a type tag (str/list/dict). *(Heap types
//!   are the next slice; this one is ints/bools/None.)*
//! - `0b10` → **immediate singleton**: `None`, `False`, `True`.
//!
//! Scope of this slice: int/bool/None values, arithmetic (`+ - * // %`, neg),
//! comparisons, truthiness, `not`, and `print`. Strings/lists/dicts + the bump
//! allocator + float (which `/` and `**` need) come next.

#![cfg_attr(not(test), no_std)]

/// An encoded Python value.
pub type Value = i32;

const TAG_MASK: i32 = 0b11;
/// Heap-pointer tag — used once the heap value types (str/list/dict) land.
#[allow(dead_code)]
const TAG_PTR: i32 = 0b00;
const TAG_INT: i32 = 0b01;
const TAG_SPECIAL: i32 = 0b10;

/// Immediate singletons (kind in the upper bits, `TAG_SPECIAL` in the low two).
const V_NONE: Value = TAG_SPECIAL; // kind 0
const V_FALSE: Value = (1 << 2) | TAG_SPECIAL; // kind 1
const V_TRUE: Value = (2 << 2) | TAG_SPECIAL; // kind 2

/// 30-bit small-int range (bignum promotion beyond this is a TODO).
const INT_MIN: i64 = -(1 << 29);
const INT_MAX: i64 = (1 << 29) - 1;

fn is_int(v: Value) -> bool {
    v & TAG_MASK == TAG_INT
}

fn as_int(v: Value) -> i64 {
    (v >> 2) as i64
}

/// Encode an integer as a small-int value.
fn make_int(n: i64) -> Value {
    debug_assert!(
        (INT_MIN..=INT_MAX).contains(&n),
        "small-int overflow; bignum promotion is a TODO"
    );
    ((n as i32) << 2) | TAG_INT
}

fn make_bool(b: bool) -> Value {
    if b { V_TRUE } else { V_FALSE }
}

/// Numeric view of int/bool (Python treats `True`/`False` as `1`/`0`), or None
/// for non-numeric values.
fn num(v: Value) -> Option<i64> {
    if is_int(v) {
        Some(as_int(v))
    } else if v == V_TRUE {
        Some(1)
    } else if v == V_FALSE {
        Some(0)
    } else {
        None
    }
}

// --- the p2w_* ABI ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn p2w_int(n: i32) -> Value {
    make_int(n as i64)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_bool(b: i32) -> Value {
    make_bool(b != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_none() -> Value {
    V_NONE
}

/// Numeric binary op, trapping on non-numeric operands (heap types arrive with
/// the next slice).
fn numeric<F: Fn(i64, i64) -> i64>(a: Value, b: Value, f: F) -> Value {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_int(f(x, y)),
        _ => trap("unsupported operand type for a numeric op (heap types are TODO)"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_add(a: Value, b: Value) -> Value {
    numeric(a, b, |x, y| x + y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_sub(a: Value, b: Value) -> Value {
    numeric(a, b, |x, y| x - y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_mul(a: Value, b: Value) -> Value {
    numeric(a, b, |x, y| x * y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_floordiv(a: Value, b: Value) -> Value {
    match (num(a), num(b)) {
        (Some(_), Some(0)) => trap("integer division or modulo by zero"),
        (Some(x), Some(y)) => make_int(x.div_euclid(y)),
        _ => trap("unsupported operand type for //"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_mod(a: Value, b: Value) -> Value {
    match (num(a), num(b)) {
        (Some(_), Some(0)) => trap("integer division or modulo by zero"),
        (Some(x), Some(y)) => make_int(x.rem_euclid(y)),
        _ => trap("unsupported operand type for %"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_neg(a: Value) -> Value {
    match num(a) {
        Some(x) => make_int(-x),
        None => trap("bad operand type for unary -"),
    }
}

fn compare<F: Fn(i64, i64) -> bool>(a: Value, b: Value, f: F) -> Value {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_bool(f(x, y)),
        _ => trap("unsupported operand type for a comparison"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_lt(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x < y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_le(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x <= y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_gt(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x > y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_ge(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x >= y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_eq(a: Value, b: Value) -> Value {
    make_bool(value_eq(a, b))
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_ne(a: Value, b: Value) -> Value {
    make_bool(!value_eq(a, b))
}

fn value_eq(a: Value, b: Value) -> bool {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x == y, // int/bool compare numerically (True == 1)
        _ => a == b,                  // None == None; identical specials
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_truthy(v: Value) -> bool {
    if let Some(n) = num(v) {
        n != 0
    } else {
        // None is falsy; heap types (nonempty) handled with the next slice.
        v != V_NONE
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_not(v: Value) -> Value {
    make_bool(!p2w_truthy(v))
}

// --- printing --------------------------------------------------------------

/// Write a value's `str()` form via the byte sink `out` (no trailing newline).
/// Generic so the formatting logic is testable without the device's putc.
fn write_value(v: Value, out: &mut impl FnMut(u8)) {
    if is_int(v) {
        write_int(as_int(v), out);
    } else if v == V_TRUE {
        write_bytes(b"True", out);
    } else if v == V_FALSE {
        write_bytes(b"False", out);
    } else if v == V_NONE {
        write_bytes(b"None", out);
    } else {
        write_bytes(b"<object>", out); // heap types: next slice
    }
}

fn write_bytes(b: &[u8], out: &mut impl FnMut(u8)) {
    for &c in b {
        out(c);
    }
}

/// Decimal formatting of an integer with no allocation (`no_std`-friendly).
fn write_int(n: i64, out: &mut impl FnMut(u8)) {
    if n < 0 {
        out(b'-');
    }
    // Work in the non-positive domain so i64::MIN doesn't overflow on negate.
    let mut neg = if n < 0 { n } else { n.wrapping_neg() };
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (-(neg % 10)) as u8;
        neg /= 10;
        if neg == 0 {
            break;
        }
    }
    write_bytes(&buf[i..], out);
}

unsafe extern "C" {
    /// The platform byte sink: USB-CDC on the device. (Provided at final link;
    /// host tests supply a stub — see the tests module.)
    fn p2w_putc(c: u8);
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_print(v: Value) {
    write_value(v, &mut |c| unsafe { p2w_putc(c) });
    unsafe { p2w_putc(b'\n') };
}

/// Halt on an unrecoverable runtime error. On the device this will report over
/// USB-CDC and stop; for now it panics (host) / loops (device).
fn trap(_msg: &str) -> ! {
    #[cfg(test)]
    panic!("p2w runtime trap: {_msg}");
    #[cfg(not(test))]
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Satisfy the linker for the test binary (p2w_print references p2w_putc).
    #[unsafe(no_mangle)]
    extern "C" fn p2w_putc(_c: u8) {}

    fn shown(v: Value) -> String {
        let mut s = Vec::new();
        write_value(v, &mut |c| s.push(c));
        String::from_utf8(s).unwrap()
    }

    #[test]
    fn ints_round_trip_and_print() {
        assert_eq!(as_int(p2w_int(42)), 42);
        assert_eq!(as_int(p2w_int(-7)), -7);
        assert_eq!(shown(p2w_int(0)), "0");
        assert_eq!(shown(p2w_int(12345)), "12345");
        assert_eq!(shown(p2w_int(-9876)), "-9876");
    }

    #[test]
    fn arithmetic_matches_python_for_ints() {
        assert_eq!(as_int(p2w_add(p2w_int(2), p2w_int(3))), 5);
        assert_eq!(as_int(p2w_mul(p2w_int(6), p2w_int(7))), 42);
        assert_eq!(as_int(p2w_sub(p2w_int(1), p2w_int(4))), -3);
        // floor division and modulo follow the divisor's sign (Python).
        assert_eq!(as_int(p2w_floordiv(p2w_int(7), p2w_int(2))), 3);
        assert_eq!(as_int(p2w_floordiv(p2w_int(-7), p2w_int(2))), -4);
        assert_eq!(as_int(p2w_mod(p2w_int(-7), p2w_int(3))), 2);
        assert_eq!(as_int(p2w_neg(p2w_int(5))), -5);
    }

    #[test]
    fn bools_none_and_truthiness() {
        assert_eq!(shown(p2w_bool(1)), "True");
        assert_eq!(shown(p2w_bool(0)), "False");
        assert_eq!(shown(p2w_none()), "None");
        assert!(p2w_truthy(p2w_int(3)));
        assert!(!p2w_truthy(p2w_int(0)));
        assert!(p2w_truthy(p2w_bool(1)));
        assert!(!p2w_truthy(p2w_none()));
        // bool acts as int in arithmetic (True + True == 2).
        assert_eq!(as_int(p2w_add(p2w_bool(1), p2w_bool(1))), 2);
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(p2w_lt(p2w_int(1), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_ge(p2w_int(2), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_eq(p2w_int(5), p2w_int(5)), V_TRUE);
        assert_eq!(p2w_ne(p2w_int(5), p2w_int(6)), V_TRUE);
        assert_eq!(p2w_eq(p2w_bool(1), p2w_int(1)), V_TRUE); // True == 1
        assert_eq!(p2w_eq(p2w_none(), p2w_none()), V_TRUE);
        assert_eq!(p2w_eq(p2w_none(), p2w_int(0)), V_FALSE);
        assert_eq!(p2w_not(p2w_int(0)), V_TRUE);
    }

    // Note: division-by-zero / type errors call `trap`, which on the device
    // halts and in tests panics — but it panics *through* an `extern "C"` fn,
    // which aborts rather than unwinds, so it can't be asserted with
    // `#[should_panic]`. The detection logic itself is straightforward; the
    // happy-path arithmetic tests above cover the encode/decode round trip.
}
