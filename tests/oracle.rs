//! Runs the type-system oracle corpus in `tests/oracle/`.
//!
//! The corpus is a specification by example for a type checker that does not
//! exist yet. See `tests/oracle/README.md` for what each directory means and
//! how to add a case.
//!
//! What this file enforces *today* is deliberately modest — there is nothing
//! to check types with. What it does is make the gap visible and stop it
//! widening: the accepted programs must keep compiling, the errors that are
//! already caught must stay caught, and the day inference lands, the tests
//! here fail in a way that says exactly which cases moved.

use std::path::{Path, PathBuf};

fn dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/oracle")
        .join(name)
}

/// Every `.py` in a corpus directory, sorted, as (name, source).
fn cases(name: &str) -> Vec<(String, String)> {
    let d = dir(name);
    let mut out: Vec<(String, String)> = std::fs::read_dir(&d)
        .unwrap_or_else(|e| panic!("corpus directory {}: {e}", d.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "py"))
        .map(|p| {
            let src =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            (p.file_name().unwrap().to_string_lossy().into_owned(), src)
        })
        .collect();
    out.sort();
    // A path typo would otherwise make every assertion below vacuously true.
    assert!(!out.is_empty(), "no cases found in {}", d.display());
    out
}

/// Cases in `must-reject/` that the compiler ALREADY refuses, without any type
/// checker — mostly arity and operator checks that fall out of codegen.
///
/// Listing them explicitly means each one is a regression guard, and it keeps
/// the "still to do" set honest: anything not named here is a real gap.
const ALREADY_REJECTED: &[&str] = &[
    "calling-a-number.py",
    // Caught by the type checker (phase C, rule 3).
    "indexing-a-number.py",
    // Caught by the type checker (phase C, rule 1): cause-line message.
    "string-plus-int.py",
    "subtracting-strings.py",
    "too-few-arguments.py",
];

#[test]
fn accepted_programs_compile() {
    let mut broken = Vec::new();
    for (name, src) in cases("ok") {
        if let Err(e) = rust_p2w::try_compile(&src) {
            broken.push(format!("  {name}: {}", e.message));
        }
    }
    assert!(
        broken.is_empty(),
        "these must compile — the corpus says so:\n{}",
        broken.join("\n")
    );
}

#[test]
fn already_caught_errors_stay_caught() {
    for name in ALREADY_REJECTED {
        let src = std::fs::read_to_string(dir("must-reject").join(name))
            .unwrap_or_else(|e| panic!("{name} is listed in ALREADY_REJECTED but: {e}"));
        assert!(
            rust_p2w::try_compile(&src).is_err(),
            "{name} used to be rejected and now compiles — a check was lost"
        );
    }
}

/// The gap, written down.
///
/// **This test is SUPPOSED to fail when a type checker lands.** That is the
/// point: the failure names the cases that started being caught, and moving
/// them into `ALREADY_REJECTED` is a deliberate act rather than something
/// noticed months later.
#[test]
fn the_remaining_gap_is_exactly_what_we_think_it_is() {
    let mut now_caught = Vec::new();
    for (name, src) in cases("must-reject") {
        if ALREADY_REJECTED.contains(&name.as_str()) {
            continue;
        }
        if let Err(e) = rust_p2w::try_compile(&src) {
            now_caught.push(format!("  {name}: {}", e.message));
        }
    }
    assert!(
        now_caught.is_empty(),
        "GOOD NEWS, probably: these are now rejected. Check the message reads \
         well for a 12-year-old, then move them into ALREADY_REJECTED:\n{}",
        now_caught.join("\n")
    );
}

#[test]
fn open_questions_state_their_question() {
    for (name, src) in cases("open-question") {
        assert!(
            src.contains("# DECIDE:"),
            "{name} is an open question but never says what has to be decided"
        );
    }
}

#[test]
fn every_case_explains_itself() {
    // The comment is the specification; the code is only the example. A case
    // without one is a puzzle for whoever inherits this.
    for d in ["ok", "must-reject", "open-question"] {
        for (name, src) in cases(d) {
            assert!(
                src.starts_with('#'),
                "{d}/{name} has no leading comment saying why it is here"
            );
        }
    }
}
