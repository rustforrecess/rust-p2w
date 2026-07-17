//! CPython-exact `repr(float)` — the shortest string that round-trips, with
//! CPython's fixed-vs-scientific thresholds. `ryu` yields the shortest
//! round-tripping digits with round-half-to-even tie-breaking (matching
//! CPython's dtoa — Rust's own `{}` rounds half-up and so disagrees on exact
//! 16-digit ties); we extract the significant digits and the decimal-point
//! position from ryu's output and re-lay them out the way CPython does
//! (scientific when `decpt <= -4 || decpt > 16`, a trailing `.0` on whole
//! numbers, a two-digit padded exponent).

/// Format `f` exactly as CPython's `repr`/`str` would.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let neg = f.is_sign_negative(); // catches -0.0, like CPython
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(f.abs());

    // ryu emits either positional ("1234567.891", "0.0001") or scientific
    // ("1e16", "1.5e20", "1e-10"); split off any exponent, then parse the
    // mantissa's digits and decimal-point position, folding the exponent in.
    let (mant, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().unwrap()),
        None => (s, 0),
    };
    let (int_str, frac_str) = match mant.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mant, ""),
    };
    let all: Vec<u8> = int_str.bytes().chain(frac_str.bytes()).collect();
    let point = int_str.len() as i32;

    let first = all.iter().position(|&c| c != b'0');
    let Some(first) = first else {
        // Value is zero.
        return if neg { "-0.0".into() } else { "0.0".into() };
    };
    let last = all.iter().rposition(|&c| c != b'0').unwrap();
    let digits = &all[first..=last]; // significant digits, no leading/trailing zeros
    // Decimal-point position relative to the first significant digit (CPython's
    // dtoa convention: digits[0] sits in the 10^(decpt-1) place), with ryu's
    // exponent folded in.
    let decpt = point - first as i32 + exp;
    let n = digits.len() as i32;

    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let push_digits = |out: &mut String, ds: &[u8]| {
        out.extend(ds.iter().map(|&b| b as char));
    };

    if decpt <= -4 || decpt > 16 {
        // Scientific: d[0](.d[1..])e±XX, exponent = decpt - 1, ≥2 digits.
        out.push(digits[0] as char);
        if digits.len() > 1 {
            out.push('.');
            push_digits(&mut out, &digits[1..]);
        }
        out.push('e');
        let exp = decpt - 1;
        out.push(if exp < 0 { '-' } else { '+' });
        let e = exp.unsigned_abs();
        if e < 10 {
            out.push('0');
        }
        out.push_str(&e.to_string());
    } else if decpt <= 0 {
        // 0.00…digits
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        push_digits(&mut out, digits);
    } else if decpt >= n {
        // digits followed by zeros, then a trailing ".0"
        push_digits(&mut out, digits);
        for _ in 0..(decpt - n) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        // digits with a point inside
        let k = decpt as usize;
        push_digits(&mut out, &digits[..k]);
        out.push('.');
        push_digits(&mut out, &digits[k..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::py_float_repr;

    #[test]
    fn matches_cpython_on_representative_values() {
        // (input, CPython repr) — the values that motivated this, plus edges.
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (3.0, "3.0"),
            (1.5, "1.5"),
            (-2.25, "-2.25"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1234567.891, "1234567.891"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e-10, "1e-10"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1e17, "1e+17"),
            (1.5e20, "1.5e+20"),
            (1e100, "1e+100"),
            (123.456, "123.456"),
            (9999999.9999999, "9999999.9999999"),
            (2.5, "2.5"),
            (3.141592653589793, "3.141592653589793"),
            (-1e-5, "-1e-05"),
        ];
        for (v, want) in cases {
            assert_eq!(py_float_repr(v), want, "repr({v:?})");
        }
    }
}
