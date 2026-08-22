//! `p2w` — the headless harness: source in, structured JSON out.
//!
//! This is the machine-facing front door to the compiler. It exists so that
//! something *other than a human at an IDE* can ask the questions the IDE
//! asks: does this compile, what is wrong with it, what is worth teaching
//! about it, and which concepts does it exercise.
//!
//! Two consumers, one shape:
//!
//! * **Curriculum CI** — compile-check every code example in every lesson on
//!   commit. Needs no AI, and is the reason this is worth building on its own:
//!   a broken snippet in a lesson is currently discovered by a child.
//! * **An agent's inner loop** — a copilot writing p2w gets errors, lints and
//!   concept tags back in milliseconds, and iterates against them. See
//!   `PRIOR-ART-AGENTIC.md` for what that pattern is and is not.
//!
//! ## Why the diagnostic shape looks borrowed
//!
//! It is. Fields follow `rustc`'s JSON diagnostics and LSP's `Diagnostic` —
//! `line`, `span`, `message`, `severity`, `code` — because agents and editors
//! already parse those. Inventing a format costs tooling and buys nothing.
//! The parts with no precedent (`concepts`, `scaffold`) ride as extra fields
//! rather than as a new dialect.
//!
//! ## Why the JSON is hand-written
//!
//! `rust-p2w` has exactly one runtime dependency (`ryu`). Adding `serde` for a
//! binary's output would make it two, and permanently. The precedent is
//! `sequent`'s `json.rs`: dependency-free emission with the contract pinned by
//! tests. The mess lives here in the binary, not in the library.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            std::process::exit(2);
        }
    };

    match cmd {
        "check" => {
            // `--profile mojo` adds the Mojo-bridge findings to the report;
            // everything else in `rest` is the file argument.
            let mut mojo = false;
            let mut file_args: Vec<String> = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--profile" => match it.next().map(String::as_str) {
                        Some("mojo") => mojo = true,
                        other => {
                            eprintln!(
                                "p2w: unknown profile {:?} — the only profile is `mojo`",
                                other.unwrap_or("")
                            );
                            std::process::exit(2);
                        }
                    },
                    other => file_args.push(other.to_string()),
                }
            }
            let source = match read_source(&file_args) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("p2w: {e}");
                    std::process::exit(2);
                }
            };
            let report = check(&source, mojo);
            println!("{}", report.json);
            // Exit code is the CI contract: 0 clean, 1 has errors. Lints alone
            // do not fail a build — they are teaching, not gates.
            std::process::exit(if report.ok { 0 } else { 1 });
        }
        "concepts" => {
            let source = match read_source(rest) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("p2w: {e}");
                    std::process::exit(2);
                }
            };
            println!("{}", concepts_json(&source));
        }
        // `run` is one CLI with a conditional subcommand rather than a second
        // binary: executing needs wasmtime, and a default build should not.
        // When it is absent the command still EXISTS and says how to get it —
        // an unknown-command error would look like a typo.
        #[cfg(feature = "run")]
        "run" => {
            let mut fuel = rust_p2w::harness::DEFAULT_FUEL;
            let mut stdin_text = String::new();
            let mut path: Option<String> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--fuel" => match it.next().and_then(|v| v.parse().ok()) {
                        Some(v) => fuel = v,
                        None => {
                            eprintln!("p2w: --fuel needs a whole number");
                            std::process::exit(2);
                        }
                    },
                    "--stdin" => match it.next() {
                        Some(v) => stdin_text = v.clone(),
                        None => {
                            eprintln!("p2w: --stdin needs a value");
                            std::process::exit(2);
                        }
                    },
                    other => path = Some(other.to_string()),
                }
            }
            let source = match rust_p2w::harness::read_source(path.as_deref()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("p2w: {e}");
                    std::process::exit(2);
                }
            };
            let r = rust_p2w::harness::run(&source, fuel, &stdin_text);
            println!("{}", r.json);
            std::process::exit(r.exit);
        }
        #[cfg(not(feature = "run"))]
        "run" => {
            eprintln!(
                "p2w: `run` executes a program, which needs wasmtime, so it is not in this \
                 build.\n     Rebuild with:  cargo build --features run"
            );
            std::process::exit(2);
        }
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("p2w: unknown command `{other}`");
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!(
        "\
p2w — headless compiler harness

USAGE:
    p2w check [FILE]        compile-check; JSON to stdout
                            exit 0 = compiles, 1 = errors, 2 = bad invocation
                            --profile mojo  adds the Mojo-bridge findings
    p2w concepts [FILE]     concept evidence only, as JSON
    p2w run [FILE]          execute it under a fuel budget; JSON to stdout
                            --fuel N, --stdin TEXT  (needs --features run)

With no FILE, reads stdin. Lints never affect the exit code."
    );
}

fn read_source(rest: &[String]) -> Result<String, String> {
    match rest.first() {
        Some(path) if path != "-" => {
            std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
        }
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("stdin: {e}"))?;
            Ok(s)
        }
    }
}

struct Report {
    ok: bool,
    json: String,
}

/// The whole check: does it compile, what is wrong, what is worth teaching,
/// what concepts does it touch. `mojo` adds the Mojo-bridge profile section.
fn check(source: &str, mojo: bool) -> Report {
    let compile = rust_p2w::try_compile(source);
    let ok = compile.is_ok();

    let mut out = String::from("{\n  \"ok\": ");
    out.push_str(if ok { "true" } else { "false" });

    // ---- errors ---------------------------------------------------------
    // Built as items then joined, so an empty result is `[]` and not `[\n  ]`
    // — consumers diffing JSON deserve a stable empty form.
    let mut items: Vec<String> = Vec::new();
    if let Err(e) = &compile {
        let mut it = String::from("{");
        push_field(&mut it, "severity", &json_str("error"), true);
        push_field(&mut it, "code", &json_str(error_code(e.kind)), false);
        push_field(&mut it, "headline", &json_str(e.kind.headline()), false);
        push_field(&mut it, "message", &json_str(&e.message), false);
        match e.line {
            Some(l) => push_field(&mut it, "line", &l.to_string(), false),
            None => push_field(&mut it, "line", "null", false),
        }
        match e.span {
            Some((a, b)) => push_field(&mut it, "span", &format!("[{a}, {b}]"), false),
            None => push_field(&mut it, "span", "null", false),
        }
        it.push_str("\n    }");
        items.push(it);
    }
    out.push_str(",\n  \"errors\": ");
    out.push_str(&join_array(&items));

    // ---- lints ----------------------------------------------------------
    // Teaching output, not gates. Each carries its fading hint ladder when the
    // lint has a concept behind it, so a consumer can offer help without
    // knowing anything about our pedagogy.
    let mut items: Vec<String> = Vec::new();
    for l in rust_p2w::lints(source) {
        let mut it = String::from("{");
        push_field(&mut it, "severity", &json_str("warning"), true);
        push_field(&mut it, "code", &json_str(&lint_code(l.kind)), false);
        push_field(&mut it, "message", &json_str(&l.message), false);
        push_field(&mut it, "line", &l.line.to_string(), false);
        let (a, b) = l.span;
        push_field(&mut it, "span", &format!("[{a}, {b}]"), false);
        match rust_p2w::scaffold(l.kind) {
            Some(s) => {
                let ladder = format!(
                    "{{\"question\": {}, \"hint\": {}, \"fix\": {}}}",
                    json_str(s.question),
                    json_str(s.hint),
                    json_str(s.fix)
                );
                push_field(&mut it, "scaffold", &ladder, false);
            }
            None => push_field(&mut it, "scaffold", "null", false),
        }
        it.push_str("\n    }");
        items.push(it);
    }
    out.push_str(",\n  \"lints\": ");
    out.push_str(&join_array(&items));

    // ---- capabilities ---------------------------------------------------
    // What the program can actually TOUCH, read out of the module's import
    // list. The subset grants no ambient authority — no filesystem, clock,
    // network or randomness — so this list is the complete statement of a
    // program's reach, and CI or a teacher can check it without reading WAT.
    //
    // Empty when compilation failed: a program that does not build has no
    // manifest, and guessing one would be worse than saying nothing.
    let caps: Vec<String> = match &compile {
        Ok(wat) => rust_p2w::capabilities(wat)
            .iter()
            .map(|c| json_str(c))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.push_str(",\n  \"capabilities\": ");
    out.push_str(&join_array(&caps));

    // ---- concepts -------------------------------------------------------
    // The field with no precedent elsewhere: what this program is *reaching
    // for*, not just whether it is well-formed.
    out.push_str(",\n  \"concepts\": ");
    out.push_str(&concept_array(source));

    // ---- Mojo-bridge profile (opt-in) ------------------------------------
    // Findings for constructs outside the Python∩Mojo intersection. `ready`
    // means: still valid Python (always true of p2w), and believed valid
    // Mojo 1.0 as well, given tools/mojo/p2w_prelude.mojo. docs/MOJO_BRIDGE.md
    // records what "believed" rests on and how it gets verified.
    if mojo {
        let findings = rust_p2w::mojo_profile(source);
        let mut items: Vec<String> = Vec::new();
        for (line, (a, b), message) in &findings {
            let mut it = String::from("{");
            push_field(&mut it, "message", &json_str(message), true);
            push_field(&mut it, "line", &line.to_string(), false);
            push_field(&mut it, "span", &format!("[{a}, {b}]"), false);
            it.push_str("\n    }");
            items.push(it);
        }
        out.push_str(",\n  \"mojo_profile\": {\n    \"ready\": ");
        out.push_str(if ok && findings.is_empty() {
            "true"
        } else {
            "false"
        });
        out.push_str(",\n    \"findings\": ");
        out.push_str(&join_array(&items).replace('\n', "\n  "));
        out.push_str("\n  }");
    }

    out.push_str("\n}");
    Report { ok, json: out }
}

fn concepts_json(source: &str) -> String {
    format!("{{\n  \"concepts\": {}\n}}", concept_array(source))
}

fn concept_array(source: &str) -> String {
    let cs = rust_p2w::concept_evidence(source);
    if cs.is_empty() {
        return "[]".to_string();
    }
    let mut s = String::from("[");
    for (i, c) in cs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "\n    {{\"name\": {}, \"count\": {}}}",
            json_str(c.name),
            c.count
        ));
    }
    s.push_str("\n  ]");
    s
}

/// Stable machine codes. Deliberately not the human headline — consumers
/// match on these, humans read `message`.
fn error_code(k: rust_p2w::ErrorKind) -> &'static str {
    match k {
        rust_p2w::ErrorKind::Syntax => "syntax",
        rust_p2w::ErrorKind::Name => "name",
        rust_p2w::ErrorKind::Type => "type",
    }
}

fn lint_code(k: rust_p2w::LintKind) -> String {
    // Derived from the debug name so a new LintKind cannot silently ship
    // without a code: `MutableDefault` -> `mutable_default`.
    let name = format!("{k:?}");
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// `[]` when empty, otherwise one item per line. A stable empty form matters:
/// consumers diff this output, and `[\n  ]` versus `[]` is a spurious change.
fn join_array(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let mut s = String::from("[\n    ");
    s.push_str(&items.join(",\n    "));
    s.push_str("\n  ]");
    s
}

fn push_field(out: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    out.push_str("\n      \"");
    out.push_str(key);
    out.push_str("\": ");
    out.push_str(value);
}

/// Minimal RFC 8259 string escaping — the only part of JSON that is easy to
/// get subtly wrong, so it is the part with tests.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_survives_hostile_strings() {
        assert_eq!(json_str(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(json_str("a\\b"), r#""a\\b""#);
        assert_eq!(json_str("line\nbreak"), r#""line\nbreak""#);
        assert_eq!(json_str("tab\there"), r#""tab\there""#);
        // Control characters are ESCAPED, not dropped.
        assert_eq!(json_str("\u{7}"), r#""\u0007""#);
    }

    #[test]
    fn clean_program_reports_ok_and_no_errors() {
        let r = check("print('hello')\n", false);
        assert!(r.ok, "should compile: {}", r.json);
        assert!(r.json.contains("\"ok\": true"));
        assert!(r.json.contains("\"errors\": []"));
    }

    #[test]
    fn broken_program_reports_an_error_with_a_code() {
        let r = check("for i in range(3)\n    print(i)\n", false);
        assert!(!r.ok, "should not compile: {}", r.json);
        assert!(r.json.contains("\"ok\": false"));
        assert!(r.json.contains("\"severity\": \"error\""));
        assert!(r.json.contains("\"code\": \"syntax\""));
    }

    #[test]
    fn the_mojo_profile_is_opt_in_and_judges_the_intersection() {
        // Without the flag: no section at all (the report shape is stable).
        let r = check("print('hello')\n", false);
        assert!(!r.json.contains("mojo_profile"), "{}", r.json);

        // A typed procedural program IS the intersection: ready.
        let r = check(
            "def double(n: int) -> int:\n    return n * 2\nprint(double(21))\n",
            true,
        );
        assert!(r.json.contains("\"ready\": true"), "{}", r.json);

        // Each construct outside the intersection is a finding, not an error:
        // the program stays valid p2w/Python either way.
        for (src, expect) in [
            (
                "class Dog:\n    def __init__(self):\n        self.n = 1\nd = Dog()\n",
                "classes",
            ),
            ("name = 'Ada'\nprint(f'hi {name}')\n", "f-strings"),
            ("xs = [1, 'two']\nprint(xs)\n", "ONE"),
            ("import random\nprint(random.randint(1, 6))\n", "random"),
            ("x = 5\nx = 'now text'\nprint(x)\n", "type"),
            ("print(len('abc'))\n", "len()"),
        ] {
            let r = check(src, true);
            assert!(r.ok, "still compiles as p2w: {src}");
            assert!(r.json.contains("\"ready\": false"), "{src}: {}", r.json);
            assert!(
                r.json.contains(expect),
                "{src} should mention {expect}: {}",
                r.json
            );
        }
    }

    #[test]
    fn concepts_are_reported_for_a_loop() {
        let r = check("for i in range(3):\n    print(i)\n", false);
        assert!(r.json.contains("\"concepts\""), "{}", r.json);
        assert!(
            r.json.contains("\"loop\""),
            "loop concept expected: {}",
            r.json
        );
    }

    #[test]
    fn lint_codes_are_snake_case_from_the_kind() {
        assert_eq!(
            lint_code(rust_p2w::LintKind::MutableDefault),
            "mutable_default"
        );
        assert_eq!(lint_code(rust_p2w::LintKind::Typo), "typo");
        assert_eq!(lint_code(rust_p2w::LintKind::UnusedLocal), "unused_local");
    }

    #[test]
    fn the_report_states_what_the_program_can_reach() {
        let r = check("print(1)\n", false);
        assert!(r.json.contains("\"capabilities\""), "{}", r.json);
        assert!(r.json.contains("\"write_char\""), "{}", r.json);
        // A program that only prints must not claim the input capability.
        assert!(!r.json.contains("\"read_char\""), "{}", r.json);
    }

    #[test]
    fn a_broken_program_claims_no_capabilities() {
        // No module, no manifest. Guessing would be worse than silence.
        let r = check("for i in range(3)\n    print(i)\n", false);
        assert!(!r.ok);
        assert!(r.json.contains("\"capabilities\": []"), "{}", r.json);
    }

    #[test]
    fn a_lint_carries_its_fix_ladder() {
        // Mutable default is the canonical scaffolded lint.
        let r = check(
            "def f(x, acc=[]):\n    acc.append(x)\n    return acc\n",
            false,
        );
        if r.json.contains("mutable_default") {
            assert!(
                r.json.contains("\"scaffold\"") && r.json.contains("\"question\""),
                "scaffolded lint should carry its ladder: {}",
                r.json
            );
        }
    }
}
