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
}

/// Every builtin the language exposes. Adding one here adds it to the typo
/// "did you mean" set and (once wired) the blocks toolbox automatically.
pub const BUILTINS: &[Builtin] = &[
    // --- io ---
    b("print", "io", &["value"]),
    b("input", "io", &["prompt"]),
    // --- convert / constructors ---
    b("int", "convert", &["x"]),
    b("str", "convert", &["x"]),
    b("float", "convert", &["x"]),
    b("bool", "convert", &["x"]),
    b("list", "convert", &["items"]),
    b("dict", "convert", &[]),
    b("set", "convert", &["items"]),
    b("tuple", "convert", &["items"]),
    // --- math ---
    b("abs", "math", &["x"]),
    b("min", "math", &["a", "b"]),
    b("max", "math", &["a", "b"]),
    b("sum", "math", &["items"]),
    b("round", "math", &["x", "ndigits"]),
    // --- sequence / inspection ---
    b("len", "sequence", &["x"]),
    b("range", "sequence", &["start", "stop", "step"]),
    b("sorted", "sequence", &["items"]),
    b("enumerate", "sequence", &["items"]),
    b("zip", "sequence", &["a", "b"]),
    b("any", "sequence", &["items"]),
    b("all", "sequence", &["items"]),
    b("repr", "sequence", &["x"]),
    b("type", "sequence", &["x"]),
    // --- web (interactive pages; see docs/INTERACTIVE_WEB.md) ---
    b("on_click", "web", &["handler"]),
    b("on", "web", &["selector", "event", "handler"]),
    b("on_key", "web", &["key", "handler"]),
    b("every", "web", &["ms", "handler"]),
    b("set_attr", "web", &["selector", "name", "value"]),
    b("set_text", "web", &["selector", "text"]),
    b("get_value", "web", &["selector"]),
    b("play_sound", "web", &["name"]),
    b("beep", "web", &[]),
    b("flash", "web", &[]),
    // --- activity (host capabilities; see acornstem/ACTIVITY_INTERFACE.md) ---
    b("seed", "activity", &[]),
    b("report", "activity", &["score", "trace"]),
    b("evidence", "activity", &["key", "value"]),
    b("set_field", "activity", &["key", "value"]),
    b("get_field", "activity", &["key"]),
];

/// Terse const-constructor so the table above reads as data.
const fn b(name: &'static str, category: &'static str, params: &'static [&'static str]) -> Builtin {
    Builtin {
        name,
        category,
        params,
    }
}

/// Just the builtin names — the set `lint` checks calls against.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|b| b.name)
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
}
