//! Every runtime diagnostic the language can produce — keyed, in ONE place.
//!
//! Three surfaces print these messages and must agree: the WASM-GC backend
//! (`codegen` raise helpers), the native runtime (`runtime/src/lib.rs`), and
//! the Stepper's interpreter (`debug`). Before this table each surface carried
//! its own copy of the text and they agreed only by discipline — several had
//! already drifted (BACKEND_DIVERGENCE.md). Message text lives here and
//! nowhere else; a surface that cannot interpolate consumes a degraded form
//! deliberately, not accidentally.
//!
//! `{slot}` marks a runtime value interpolated by the surface: `codegen`
//! splits the template and emits printing code for each slot, `debug`
//! format-fills, the native runtime (Phase C) prints the pieces it can.
//! The slot NAME is documentation and a stability contract — renderers
//! (translations, reading levels, a future host-side weaver) key on it.
//!
//! `key` is the stable identity of the diagnostic. It never changes once
//! shipped, even if the English text improves: keys are what translations,
//! xAPI evidence, and the before/after error corpus attach to.
//!
//! What each message should SAY is specified in `TYPE_ERROR_MESSAGES.md` —
//! written for a twelve-year-old, which is the default reading level.
//!
//! This file is `no_std`-clean (consts only) so the native runtime can share
//! it by path-include, the same way `js_string_host.rs` is shared with the
//! test hosts.

/// One diagnostic: a stable key and the canonical message template.
pub struct Msg {
    /// Stable identity — never changes once shipped.
    pub key: &'static str,
    /// Canonical English text; `{slot}` marks a runtime value.
    pub text: &'static str,
}

const fn m(key: &'static str, text: &'static str) -> Msg {
    Msg { key, text }
}

// --- NameError ---------------------------------------------------------------

pub const NAME_USE_BEFORE_ASSIGN: Msg = m(
    "name.use-before-assign",
    "NameError: a variable was used before it was given a value",
);

// --- TypeError ---------------------------------------------------------------

pub const TYPE_EXPECTED_NUMBER: Msg = m(
    "type.expected-number",
    "TypeError: expected a number, got '{type}'",
);

pub const TYPE_NO_LEN: Msg = m(
    "type.no-len",
    "TypeError: object of type '{type}' has no len()",
);

pub const TYPE_NOT_SUBSCRIPTABLE: Msg = m(
    "type.not-subscriptable",
    "TypeError: '{type}' object is not subscriptable",
);

pub const TYPE_NO_ITEM_ASSIGN: Msg = m(
    "type.no-item-assign",
    "TypeError: '{type}' object does not support item assignment",
);

pub const TYPE_ARG_NOT_ITERABLE: Msg = m(
    "type.arg-not-iterable",
    "TypeError: argument of type '{type}' is not iterable",
);

pub const TYPE_IN_STRING_LEFT: Msg = m(
    "type.in-string-left-operand",
    "TypeError: 'in <string>' requires string as left operand, not '{type}'",
);

pub const TYPE_STR_UNSUPPORTED: Msg = m(
    "type.str-unsupported",
    "TypeError: str() of '{type}' values isn't supported yet",
);

pub const TYPE_UNHASHABLE: Msg = m(
    "type.unhashable",
    "TypeError: unhashable type: '{type}' (a set can't contain it — use a tuple)",
);

pub const TYPE_FOR_NOT_ITERABLE: Msg = m(
    "type.for-not-iterable",
    "TypeError: a '{type}' is one single value, so a for loop has nothing to go through. A loop needs a list, some text, a dict or a set — or use range(n) to count.",
);

pub const TYPE_METHOD_AS_VALUE: Msg = m(
    "type.method-as-value",
    "TypeError: method '{name}' can't be used as a value yet (call it with parentheses)",
);

pub const TYPE_SET_OP: Msg = m(
    "type.set-op",
    "TypeError: unsupported operand type for a set operation (both sides must be sets)",
);

pub const TYPE_METHOD_ARITY: Msg = m(
    "type.method-arity",
    "TypeError: method called with the wrong number of arguments",
);

pub const TYPE_POW_FRACTIONAL: Msg = m(
    "type.pow-fractional",
    "TypeError: ** can raise to a whole number, or to 0.5 — a square root, which the machine does in one step. Other fractional powers need a maths library this program does not have; try math.sqrt() or a whole-number power.",
);

// --- AttributeError ----------------------------------------------------------

pub const ATTR_MISSING: Msg = m(
    "attr.missing",
    "AttributeError: '{type}' object has no attribute '{name}'",
);

pub const ATTR_NO_APPEND: Msg = m(
    "attr.no-append",
    "AttributeError: '{type}' object has no attribute 'append'",
);

// --- IndexError / KeyError ---------------------------------------------------

pub const INDEX_OUT_OF_RANGE: Msg = m("index.out-of-range", "IndexError: {seq} index out of range");

pub const INDEX_CHOICE_EMPTY: Msg = m(
    "index.choice-empty",
    "IndexError: cannot choose from an empty sequence",
);

pub const KEY_MISSING: Msg = m("key.missing", "KeyError: {key}");

// --- ValueError --------------------------------------------------------------

pub const VALUE_INT_PARSE: Msg = m(
    "value.int-parse",
    "ValueError: invalid literal for int() with base 10: '{text}'",
);

pub const VALUE_FLOAT_PARSE: Msg = m(
    "value.float-parse",
    "ValueError: could not convert string to float: '{text}'",
);

pub const VALUE_NOT_IN_SEQUENCE: Msg = m(
    "value.not-in-sequence",
    "ValueError: value is not in the sequence",
);

pub const VALUE_EMPTY_SEQUENCE: Msg = m(
    "value.empty-sequence",
    "ValueError: arg is an empty sequence",
);

pub const VALUE_UNPACK_COUNT: Msg = m(
    "value.unpack-count",
    "ValueError: wrong number of values to unpack",
);

pub const VALUE_SLICE_STEP_ZERO: Msg = m(
    "value.slice-step-zero",
    "ValueError: slice step cannot be zero",
);

pub const VALUE_RANDINT_ORDER: Msg = m(
    "value.randint-order",
    "ValueError: randint(a, b) needs a <= b — the low end first",
);

// --- ZeroDivisionError / OverflowError ---------------------------------------

pub const ZERO_DIVISION: Msg = m("arith.zero-division", "ZeroDivisionError: division by zero");

pub const INT_OVERFLOW: Msg = m(
    "arith.int-overflow",
    "OverflowError: this calculation went outside the range of whole numbers we can store (-2147483648 to 2147483647)",
);
