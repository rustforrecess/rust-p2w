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

/// Parse `s` exactly as CPython's `float(s)` would, or `None` where CPython
/// raises ValueError. Surrounding whitespace is ignored; `inf`, `infinity`
/// and `nan` are accepted in any case, with a sign; a single underscore may
/// sit between two digits (`1_000.5`). The digits go through Rust's
/// correctly rounded parser, so the result is bit-identical to CPython's.
pub fn py_parse_float(s: &str) -> Option<f64> {
    let s = s.trim();
    if !s.contains('_') {
        return s.parse::<f64>().ok();
    }
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'_' {
            let digit = |j: Option<usize>| j.and_then(|j| b.get(j)).is_some_and(u8::is_ascii_digit);
            if !digit(i.checked_sub(1)) || !digit(Some(i + 1)) {
                return None;
            }
        }
    }
    s.replace('_', "").parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::{py_float_repr, py_parse_float};

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
            #[allow(clippy::approx_constant)] // formatting pi's digits IS the test
            (3.141592653589793, "3.141592653589793"),
            (-1e-5, "-1e-05"),
        ];
        for (v, want) in cases {
            assert_eq!(py_float_repr(v), want, "repr({v:?})");
        }
    }

    #[test]
    fn parses_what_cpython_float_accepts() {
        for (s, want) in [
            ("0.3", 0.3),
            ("1.15", 1.15),
            ("  1.5  ", 1.5),
            ("1_000.5", 1000.5),
            ("1e5", 1e5),
            (".5", 0.5),
            ("5.", 5.0),
            ("+2", 2.0),
            ("-INF", f64::NEG_INFINITY),
            ("Infinity", f64::INFINITY),
            ("1.7976931348623157e308", f64::MAX),
            ("0.000001", 0.000001),
        ] {
            assert_eq!(py_parse_float(s), Some(want), "float({s:?})");
        }
        assert!(py_parse_float("nan").unwrap().is_nan());
        for s in [
            "abc", "", "1.5.2", "0x10", "1__0", "_1", "1_", "1 000", "1_.5", "1e",
        ] {
            assert_eq!(py_parse_float(s), None, "float({s:?})");
        }
    }
}
