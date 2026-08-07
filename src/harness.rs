//! Executing a program under a **deterministic** budget — the other half of the
//! harness. `p2w check` says whether a program compiles; this says whether it
//! does the right thing, and what it cost.
//!
//! Behind the `run` feature, because executing needs wasmtime and compiling
//! does not: a default build of the library and the compiler keeps exactly one
//! runtime dependency.
//!
//! ## Why fuel and not a timeout
//!
//! Fuel is wasmtime's instruction budget: each instruction costs a unit and the
//! program traps when the allowance runs out. **Same program, same input, same
//! number, on every machine.** A wall-clock timeout would answer differently on
//! a fast laptop than on a school Chromebook, reintroducing exactly the
//! nondeterminism the subset removes by having no ambient clock and no ambient
//! randomness. A number that changes per machine cannot go in a rubric.
//!
//! Two distinct uses:
//!
//! * **A bound** — a runaway loop traps instead of hanging, which is what makes
//!   it safe to execute a student's, or a generated, program in CI at all.
//! * **A measure** — two correct solutions with different costs, so efficiency
//!   becomes a number a student watches move rather than a word a teacher uses.
//!
//! ## What fuel is NOT
//!
//! **It counts instructions, not time.** Fair between two programs on the same
//! runtime and a good regression signal, but no predictor of wall-clock on the
//! board, where memory traffic dominates. A teaching metric, not a benchmark.
//!
//! ## This is also the REFERENCE HOST
//!
//! The `env.*` imports wired below are the contract `codegen` emits against.
//! Keeping one implementation next to the compiler is deliberate: host copies
//! that drift are how a flush bug faked an entire harness run.

use std::io::Read;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};

/// Enough for any classroom program; small enough that a runaway loop stops
/// while a person is still looking at the screen.
pub const DEFAULT_FUEL: u64 = 100_000_000;

/// Read a program from a path, or from stdin when absent or `-`.
pub fn read_source(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(p) if p != "-" => std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}")),
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("stdin: {e}"))?;
            Ok(s)
        }
    }
}

/// An unrecoverable harness problem (not a problem with the student's program).
fn fatal(msg: &str) -> ! {
    eprintln!("p2w run: {msg}");
    std::process::exit(2);
}

pub struct Report {
    pub json: String,
    pub exit: i32,
}

/// Host state: what the program printed, plus the stdin it may read.
struct Io {
    out: Vec<u8>,
    input: Vec<u8>,
    pos: usize,
}

pub fn run(source: &str, fuel: u64, stdin_text: &str) -> Report {
    let wat = match crate::try_compile(source) {
        Ok(w) => w,
        Err(e) => {
            return Report {
                json: format!(
                    "{{\n  \"ok\": false,\n  \"reason\": \"compile-error\",\n  \
                     \"message\": {},\n  \"line\": {},\n  \"output\": \"\",\n  \
                     \"fuel_used\": null\n}}",
                    crate::json_escape(&e.message),
                    e.line.map_or("null".into(), |l| l.to_string()),
                ),
                exit: 1,
            };
        }
    };

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    // The whole point: a deterministic budget rather than a wall clock.
    config.consume_fuel(true);
    let engine = match Engine::new(&config) {
        Ok(e) => e,
        Err(e) => fatal(&format!("engine: {e}")),
    };

    let wasm = match wat::parse_str(&wat) {
        Ok(w) => w,
        Err(e) => fatal(&format!("internal: emitted invalid WAT: {e}")),
    };
    let module = match Module::new(&engine, &wasm[..]) {
        Ok(m) => m,
        Err(e) => fatal(&format!("internal: module rejected: {e}")),
    };

    let mut store = Store::new(
        &engine,
        Io {
            out: Vec::new(),
            input: stdin_text.as_bytes().to_vec(),
            pos: 0,
        },
    );
    if let Err(e) = store.set_fuel(fuel) {
        fatal(&format!("fuel: {e}"));
    }

    let mut linker: Linker<Io> = Linker::new(&engine);
    linker
        .func_wrap("env", "write_char", |mut c: Caller<'_, Io>, ch: i32| {
            c.data_mut().out.push(ch as u8);
        })
        .unwrap();
    linker
        .func_wrap("env", "write_i32", |mut c: Caller<'_, Io>, v: i32| {
            c.data_mut().out.extend_from_slice(v.to_string().as_bytes());
        })
        .unwrap();
    linker
        .func_wrap("env", "write_f64", |mut c: Caller<'_, Io>, v: f64| {
            let s = crate::py_float_repr(v);
            c.data_mut().out.extend_from_slice(s.as_bytes());
        })
        .unwrap();
    linker
        .func_wrap("env", "read_char", |mut c: Caller<'_, Io>| -> i32 {
            let d = c.data_mut();
            if d.pos < d.input.len() {
                let b = d.input[d.pos];
                d.pos += 1;
                b as i32
            } else {
                -1
            }
        })
        .unwrap();
    // A fixed seed: `run` is for checking answers, and a per-attempt value would
    // make the same program give different results on two invocations.
    linker
        .func_wrap("env", "seed", |_: Caller<'_, Io>| -> i32 { 42 })
        .unwrap();

    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => {
            // A missing import means the program used a capability this runner
            // does not provide — worth naming, not crashing over.
            return Report {
                json: format!(
                    "{{\n  \"ok\": false,\n  \"reason\": \"missing-capability\",\n  \
                     \"message\": {},\n  \"output\": \"\",\n  \"fuel_used\": null\n}}",
                    crate::json_escape(&e.to_string())
                ),
                exit: 1,
            };
        }
    };
    let start = match instance.get_typed_func::<(), i32>(&mut store, "_start") {
        Ok(f) => f,
        Err(e) => fatal(&format!("internal: no _start: {e}")),
    };

    let result = start.call(&mut store, ());
    // Remaining fuel is what is left of the grant; used is the difference.
    let left = store.get_fuel().unwrap_or(0);
    let used = fuel.saturating_sub(left);
    let exhausted = matches!(store.get_fuel(), Ok(0)) && result.is_err();
    let out = String::from_utf8_lossy(&store.into_data().out).into_owned();

    let (ok, reason, message) = match &result {
        Ok(_) => (true, "ok", String::new()),
        Err(e) if exhausted => (
            false,
            "out-of-fuel",
            format!("used the whole budget of {fuel} — is there a loop that never ends?"),
        ),
        Err(e) => {
            // A p2w trap prints its message and then executes `unreachable`, so
            // the explanation is the tail of stdout, not the wasmtime error.
            let last = out.lines().last().unwrap_or("").trim().to_string();
            let msg = if last.is_empty() { e.to_string() } else { last };
            (false, "trap", msg)
        }
    };

    Report {
        json: format!(
            "{{\n  \"ok\": {ok},\n  \"reason\": {},\n  \"message\": {},\n  \
             \"output\": {},\n  \"fuel_used\": {used},\n  \"fuel_limit\": {fuel}\n}}",
            crate::json_escape(reason),
            crate::json_escape(&message),
            crate::json_escape(&out),
        ),
        exit: i32::from(!ok),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_program_reports_its_output_and_a_cost() {
        let r = run("print(1 + 1)\n", DEFAULT_FUEL, "");
        assert_eq!(r.exit, 0, "{}", r.json);
        assert!(r.json.contains("\"output\": \"2\\n\""), "{}", r.json);
        assert!(r.json.contains("\"reason\": \"ok\""), "{}", r.json);
        // The cost must be a real measurement, not zero.
        assert!(
            !r.json.contains("\"fuel_used\": 0,"),
            "fuel should have been consumed: {}",
            r.json
        );
    }

    #[test]
    fn more_work_costs_more_fuel() {
        // The property the whole feature rests on: cost tracks work done, so a
        // student can watch the number move.
        let small = run("for i in range(10):\n    x = i\n", DEFAULT_FUEL, "");
        let big = run("for i in range(1000):\n    x = i\n", DEFAULT_FUEL, "");
        let f = |j: &str| -> u64 {
            let k = "\"fuel_used\": ";
            let i = j.find(k).unwrap() + k.len();
            j[i..].split(',').next().unwrap().trim().parse().unwrap()
        };
        assert!(
            f(&big.json) > f(&small.json) * 10,
            "1000 iterations should cost far more than 10:\n{}\n{}",
            small.json,
            big.json
        );
    }

    #[test]
    fn the_same_program_costs_the_same_every_time() {
        // Determinism is the reason for fuel over a wall clock. If this ever
        // fails, the number cannot be put in a rubric.
        let a = run("for i in range(100):\n    x = i * 2\n", DEFAULT_FUEL, "");
        let b = run("for i in range(100):\n    x = i * 2\n", DEFAULT_FUEL, "");
        assert_eq!(a.json, b.json);
    }

    #[test]
    fn a_loop_that_never_ends_runs_out_instead_of_hanging() {
        let r = run("while True:\n    x = 1\n", 1_000_000, "");
        assert_eq!(r.exit, 1);
        assert!(r.json.contains("\"reason\": \"out-of-fuel\""), "{}", r.json);
        assert!(r.json.contains("never ends"), "{}", r.json);
    }

    #[test]
    fn a_trap_reports_the_message_the_student_would_see() {
        let r = run("print([1, 2][9])\n", DEFAULT_FUEL, "");
        assert_eq!(r.exit, 1);
        assert!(r.json.contains("\"reason\": \"trap\""), "{}", r.json);
        assert!(r.json.contains("IndexError"), "{}", r.json);
    }

    #[test]
    fn a_program_that_does_not_compile_says_so_without_running() {
        let r = run("for i in range(3)\n    print(i)\n", DEFAULT_FUEL, "");
        assert_eq!(r.exit, 1);
        assert!(
            r.json.contains("\"reason\": \"compile-error\""),
            "{}",
            r.json
        );
        assert!(r.json.contains("\"fuel_used\": null"), "{}", r.json);
    }

    #[test]
    fn input_is_fed_from_the_flag() {
        let r = run("x = input()\nprint('hi ' + x)\n", DEFAULT_FUEL, "ada\n");
        assert_eq!(r.exit, 0, "{}", r.json);
        assert!(r.json.contains("hi ada"), "{}", r.json);
    }
}
