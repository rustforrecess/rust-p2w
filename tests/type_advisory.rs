//! Phase A's executable contract, against the oracle programs themselves
//! (tests/oracle/ — the type-system spec, as programs).
//!
//! * Every `ok/` program: compiles AND produces ZERO advisory findings —
//!   the false-positive gate. A checker that annoys a beginner writing
//!   correct code has failed at its actual job.
//! * Every `must-reject/` program: is either already a compile error, or
//!   produces at least one advisory finding. (Phase C is where advisories
//!   graduate to errors and `tests/oracle.rs`'s floor test moves.)
//! * `open-question/` programs are deliberately unasserted: their answers
//!   are pedagogy decisions scheduled for phase C, and the checker keeps
//!   them `Dyn` — silent — until then.

use std::fs;

fn cases(dir: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = fs::read_dir(format!("tests/oracle/{dir}"))
        .expect("oracle dir")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "py").then(|| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read_to_string(&p).expect("read"),
                )
            })
        })
        .collect();
    out.sort();
    assert!(!out.is_empty());
    out
}

#[test]
fn ok_programs_compile_with_zero_findings() {
    for (name, src) in cases("ok") {
        assert!(
            rust_p2w::compile_to_wat(&src).is_ok(),
            "{name} must compile"
        );
        let f = rust_p2w::type_findings(&src);
        assert!(
            f.is_empty(),
            "FALSE POSITIVE in {name}: {:?}",
            f.iter().map(|x| (x.code, &x.message)).collect::<Vec<_>>()
        );
    }
}

#[test]
fn must_reject_programs_are_caught_or_flagged() {
    for (name, src) in cases("must-reject") {
        let compiles = rust_p2w::compile_to_wat(&src).is_ok();
        let findings = rust_p2w::type_findings(&src);
        assert!(
            !compiles || !findings.is_empty(),
            "{name}: compiles clean AND no advisory finding — the gap the \
             checker exists to close"
        );
    }
}

#[test]
fn every_finding_cites_its_ledger_when_a_name_is_involved() {
    // The design's point, held as a contract: the classic mistake's message
    // must name the CAUSE line, not just the symptom line.
    let f = rust_p2w::type_findings("age = \"12\"\nnext_year = age + 1\n");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].line, 2, "symptom line");
    assert!(
        f[0].message.contains("line 1"),
        "cause line: {}",
        f[0].message
    );
}
