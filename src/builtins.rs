//! The builtin/capability registry — the single source of truth for the names
//! the language exposes as callable builtins, their domain category, and their
//! argument labels.
//!
//! Several places need this list and used to drift: `lint` (typo "did you mean"),
//! `codegen` (the builtin lowering arms), and the IDE's blocks toolbox (the
//! data-driven builtin categories, see `acornstem-ide/BLOCKS_ROADMAP.md`). The
//! registry centralizes the *names + shapes*; `codegen` still owns each builtin's
//! actual lowering (arity-specific), but draws its known-name set from here.

/// One callable builtin: its name, domain `category` (for grouping in the blocks
/// toolbox), and `params` (argument labels — representative; variadic builtins
/// list their common arguments). Categories: `io`, `convert`, `math`,
/// `sequence`, `web`, `activity`.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    pub category: &'static str,
    pub params: &'static [&'static str],
    /// Whether the call yields a value (→ a value-call block) vs. a statement
    /// effect (→ a statement block). Drives the data-driven blocks toolbox.
    pub returns_value: bool,
}

/// Every builtin the language exposes. Adding one here adds it to the typo
/// "did you mean" set and (once wired) the blocks toolbox automatically.
/// `b(..)` = statement (no value); `v(..)` = returns a value.
pub const BUILTINS: &[Builtin] = &[
    // --- io ---
    b("print", "io", &["value"]),
    v("input", "io", &["prompt"]),
    // --- convert / constructors ---
    v("int", "convert", &["x"]),
    v("str", "convert", &["x"]),
    v("float", "convert", &["x"]),
    v("bool", "convert", &["x"]),
    v("list", "convert", &["items"]),
    v("dict", "convert", &[]),
    v("set", "convert", &["items"]),
    v("tuple", "convert", &["items"]),
    // --- math ---
    v("abs", "math", &["x"]),
    v("min", "math", &["a", "b"]),
    v("max", "math", &["a", "b"]),
    v("sum", "math", &["items"]),
    v("round", "math", &["x", "ndigits"]),
    // --- sequence / inspection ---
    v("len", "sequence", &["x"]),
    v("range", "sequence", &["start", "stop", "step"]),
    v("sorted", "sequence", &["items"]),
    v("enumerate", "sequence", &["items"]),
    v("zip", "sequence", &["a", "b"]),
    v("any", "sequence", &["items"]),
    v("all", "sequence", &["items"]),
    v("repr", "sequence", &["x"]),
    v("type", "sequence", &["x"]),
    // --- web (interactive pages; see docs/INTERACTIVE_WEB.md) ---
    b("on_click", "web", &["handler"]),
    b("on", "web", &["selector", "event", "handler"]),
    b("on_key", "web", &["key", "handler"]),
    b("every", "web", &["ms", "handler"]),
    b("set_attr", "web", &["selector", "name", "value"]),
    b("set_text", "web", &["selector", "text"]),
    v("get_value", "web", &["selector"]),
    b("play_sound", "web", &["name"]),
    b("beep", "web", &[]),
    b("flash", "web", &[]),
    // --- activity (host capabilities; see acornstem/ACTIVITY_INTERFACE.md) ---
    v("seed", "activity", &[]),
    b("report", "activity", &["score", "trace"]),
    b("evidence", "activity", &["key", "value"]),
    b("set_field", "activity", &["key", "value"]),
    v("get_field", "activity", &["key"]),
];

/// Statement builtin (no value). Terse so the table reads as data.
const fn b(name: &'static str, category: &'static str, params: &'static [&'static str]) -> Builtin {
    Builtin {
        name,
        category,
        params,
        returns_value: false,
    }
}

/// Value-returning builtin.
const fn v(name: &'static str, category: &'static str, params: &'static [&'static str]) -> Builtin {
    Builtin {
        name,
        category,
        params,
        returns_value: true,
    }
}

/// Just the builtin names — the set `lint` checks calls against.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|b| b.name)
}

/// The registry as a JSON array, for the IDE to inject so the blocks toolbox can
/// build data-driven builtin categories from it (no serde dependency). Shape:
/// `[{"name","category","params":[…],"returns":bool}, …]`.
pub fn builtins_json() -> String {
    let mut s = String::from("[");
    for (i, b) in BUILTINS.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":\"");
        s.push_str(b.name);
        s.push_str("\",\"category\":\"");
        s.push_str(b.category);
        s.push_str("\",\"params\":[");
        for (j, p) in b.params.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push('"');
            s.push_str(p);
            s.push('"');
        }
        s.push_str("],\"returns\":");
        s.push_str(if b.returns_value { "true" } else { "false" });
        s.push('}');
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_covers_the_legacy_lint_set_plus_capabilities() {
        let have: HashSet<&str> = names().collect();
        // The names lint historically knew — must not regress (dropping one would
        // turn a valid call into a false "did you mean").
        for n in [
            "print",
            "range",
            "len",
            "int",
            "str",
            "float",
            "bool",
            "abs",
            "min",
            "max",
            "sum",
            "sorted",
            "input",
            "round",
            "any",
            "all",
            "enumerate",
            "zip",
            "repr",
            "list",
            "dict",
            "set",
            "tuple",
            "type",
            "seed",
            "report",
            "set_field",
            "get_field",
        ] {
            assert!(have.contains(n), "registry is missing legacy builtin `{n}`");
        }
        // And it now also knows the interactive-web builtins (a prior lint gap).
        for n in ["on_click", "on", "on_key", "every", "set_attr", "get_value"] {
            assert!(have.contains(n), "registry is missing web builtin `{n}`");
        }
        // No duplicate names.
        assert_eq!(have.len(), BUILTINS.len(), "duplicate builtin name");
        // Every entry has a non-empty category.
        assert!(BUILTINS.iter().all(|b| !b.category.is_empty()));
    }

    fn find(name: &str) -> Builtin {
        *BUILTINS.iter().find(|b| b.name == name).unwrap()
    }

    #[test]
    fn returns_value_shapes_calls() {
        // Effects are statements; queries/constructors return values.
        assert!(!find("set_attr").returns_value);
        assert!(!find("on").returns_value);
        assert!(!find("print").returns_value);
        assert!(find("get_value").returns_value);
        assert!(find("len").returns_value);
        assert!(find("seed").returns_value);
    }

    #[test]
    fn builtins_json_is_well_formed() {
        let j = builtins_json();
        assert!(j.starts_with('[') && j.ends_with(']'));
        assert!(j.contains(r#"{"name":"set_attr","category":"web","params":["selector","name","value"],"returns":false}"#), "{j}");
        assert!(
            j.contains(
                r#"{"name":"get_value","category":"web","params":["selector"],"returns":true}"#
            ),
            "{j}"
        );
    }
}
